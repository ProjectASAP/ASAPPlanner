use super::*;
use crate::plan::PhysicalOperator;
use futures::{executor::block_on, stream, StreamExt};
use std::cell::Cell;

struct Source {
    starts: Rc<Cell<usize>>,
    polls: Rc<Cell<usize>>,
    fail: bool,
    end: u64,
}
impl PhysicalOperator<u64, ()> for Source {
    fn name(&self) -> &str {
        "CountingSource"
    }
    fn input_schemas(&self) -> Vec<()> {
        vec![]
    }
    fn output_schema(&self) {}
    fn output_bytes(&self, _: &u64) -> usize {
        8
    }
    fn start<'a>(
        &'a self,
        _: Vec<Input<'a, u64>>,
        _: RunContext,
    ) -> Result<OutputStream<'a, u64>, Error> {
        self.starts.set(self.starts.get() + 1);
        Ok(stream::iter(0..self.end)
            .map(move |n| {
                self.polls.set(self.polls.get() + 1);
                if self.fail && n == 1 {
                    Err(Error::Operator("source failure".into()))
                } else {
                    Ok(n)
                }
            })
            .boxed_local())
    }
}
struct Identity;
impl PhysicalOperator<u64, ()> for Identity {
    fn name(&self) -> &str {
        "Identity"
    }
    fn input_schemas(&self) -> Vec<()> {
        vec![()]
    }
    fn output_schema(&self) {}
    fn output_bytes(&self, _: &u64) -> usize {
        8
    }
    fn start<'a>(
        &'a self,
        mut inputs: Vec<Input<'a, u64>>,
        _: RunContext,
    ) -> Result<OutputStream<'a, u64>, Error> {
        Ok(inputs
            .remove(0)
            .map(|value| value.map(|v| *v))
            .boxed_local())
    }
}
fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 100,
            revision: 1,
        },
        Limits {
            max_buffered_batches: 1,
            max_bytes: 1024,
        },
    )
    .unwrap()
}
fn source(fail: bool) -> (Source, Rc<Cell<usize>>, Rc<Cell<usize>>) {
    let starts = Rc::new(Cell::new(0));
    let polls = Rc::new(Cell::new(0));
    (
        Source {
            starts: starts.clone(),
            polls: polls.clone(),
            fail,
            end: 4,
        },
        starts,
        polls,
    )
}

// A shared producer runs once, and the slow reader bounds producer progress.
#[test]
fn shared_source_backpressure_and_reader_drop() {
    let (source, starts, polls) = source(false);
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    let context = context();
    let mut readers = dag.execute(&[0, 0], context.clone()).unwrap();
    let mut slow = readers.pop().unwrap();
    let mut fast = readers.pop().unwrap();
    assert_eq!(starts.get(), 1);
    let first = block_on(fast.next()).unwrap().unwrap();
    assert_eq!(*first, 0);
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(Pin::new(&mut fast).poll_next(&mut cx).is_pending());
    assert_eq!(polls.get(), 1);
    let same = block_on(slow.next()).unwrap().unwrap();
    assert!(Arc::ptr_eq(&first.value, &same.value));
    drop(same);
    drop(first);
    assert_eq!(context.retained_bytes(), 0);
    assert_eq!(*block_on(fast.next()).unwrap().unwrap(), 1);
    drop(slow);
    assert_eq!(*block_on(fast.next()).unwrap().unwrap(), 2);
    assert_eq!(*block_on(fast.next()).unwrap().unwrap(), 3);
    assert!(block_on(fast.next()).is_none());
    assert_eq!(polls.get(), 4);
    drop(fast);
    assert_eq!(context.retained_bytes(), 0);
}

// Independent branches consume a common node concurrently without duplicate work.
#[test]
fn diamond_and_run_isolation() {
    let (source, starts, polls) = source(false);
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    dag.add(1, vec![0], Identity).unwrap();
    dag.add(2, vec![0], Identity).unwrap();
    for _ in 0..2 {
        let mut outputs = dag.execute(&[1, 2], context()).unwrap();
        let a = outputs.pop().unwrap();
        let b = outputs.pop().unwrap();
        let (a, b) =
            block_on(async { futures::join!(a.collect::<Vec<_>>(), b.collect::<Vec<_>>()) });
        assert_eq!(
            a.iter().map(|v| **v.as_ref().unwrap()).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            b.iter().map(|v| **v.as_ref().unwrap()).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }
    assert_eq!(starts.get(), 2);
    assert_eq!(polls.get(), 8);
}

// Failure reaches every subscriber; cancellation stops further producer work.
#[test]
fn broadcast_error_and_cancel() {
    let (source, _, polls) = source(true);
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    let mut outputs = dag.execute(&[0, 0], context()).unwrap();
    let a = outputs.pop().unwrap();
    let b = outputs.pop().unwrap();
    let (a, b) = block_on(async { futures::join!(a.collect::<Vec<_>>(), b.collect::<Vec<_>>()) });
    for values in [a, b] {
        assert_eq!(values.len(), 2);
        assert!(matches!(values[1], Err(Error::AtNode { node: 0, .. })));
    }
    assert_eq!(polls.get(), 2);
    let run = context();
    let mut output = dag.execute(&[0], run.clone()).unwrap().remove(0);
    run.cancel();
    assert!(matches!(
        block_on(output.next()),
        Some(Err(Error::Cancelled))
    ));
    assert!(block_on(output.next()).is_none());
    assert_eq!(polls.get(), 2);
}

// Retaining a consumer output retains its budget lease after queue eviction.
#[test]
fn retained_outputs_count_against_budget() {
    let (source, _, _) = source(false);
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    let run = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 0,
        },
        Limits {
            max_buffered_batches: 1,
            max_bytes: 8,
        },
    )
    .unwrap();
    let mut input = dag.execute(&[0], run.clone()).unwrap().remove(0);
    let held = block_on(input.next()).unwrap().unwrap();
    assert_eq!(run.retained_bytes(), 8);
    assert!(matches!(
        block_on(input.next()),
        Some(Err(Error::MemoryLimit))
    ));
    drop(input);
    assert_eq!(run.retained_bytes(), 8);
    drop(held);
    assert_eq!(run.retained_bytes(), 0);
}

// Invalid graphs fail before even starting a source.
#[test]
fn invalid_graphs_do_not_start_sources() {
    let (source, starts, _) = source(false);
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    dag.add(1, vec![2], Identity).unwrap();
    dag.add(2, vec![1], Identity).unwrap();
    assert!(dag.execute(&[0, 1], context()).is_err());
    assert_eq!(starts.get(), 0);
    let mut missing = PhysicalDag::default();
    missing.add(1, vec![9], Identity).unwrap();
    assert!(missing.validate(&[1]).is_err());
    let mut arity = PhysicalDag::default();
    arity.add(1, vec![], Identity).unwrap();
    assert!(arity.validate(&[1]).is_err());
}

// An always-ready source must yield so cancellation can be polled on this worker.
#[test]
fn ready_sources_cooperate_with_cancellation() {
    let (mut source, _, polls) = source(false);
    source.end = 10_000;
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    let context = context();
    let mut input = dag.execute(&[0], context.clone()).unwrap().remove(0);
    block_on(async {
        let drain = async {
            while let Some(result) = input.next().await {
                if let Err(error) = result {
                    assert_eq!(error, Error::Cancelled);
                    return;
                }
            }
            panic!("source completed without yielding");
        };
        let cancel = async {
            context.cancel();
        };
        futures::join!(drain, cancel);
    });
    assert_eq!(polls.get(), 32);
    assert_eq!(context.retained_bytes(), 0);
}

// Cached shorter paths must not hide an over-deep path through shared nodes.
#[test]
fn depth_limit_covers_shared_paths() {
    let (source, _, _) = source(false);
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], source).unwrap();
    for id in 1..129 {
        dag.add(id, vec![id - 1], Identity).unwrap();
    }
    assert!(dag.validate(&(0..129).collect::<Vec<_>>()).is_err());
}
