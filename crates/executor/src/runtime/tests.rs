use super::*;
use crate::plan::PhysicalOperator;
use futures::{executor::block_on, stream, StreamExt};
use planner_types::stages::StorageMedium;
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
/// A query run at time 100 with a byte budget of `max_bytes`.
fn query_context(max_bytes: usize) -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 100,
        },
        Limits { max_bytes },
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

const ONE_EVALUATION: Lifetime = Lifetime::OneEvaluation(StorageMedium::Memory);

// At query time each consumer of a store read (a leaf) reads it on its own,
// so one consumer never waits for another.
#[test]
fn query_time_leaves_are_read_per_consumer() {
    let (source, starts, polls) = source(false);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    let context = query_context(1024);
    let mut readers = dag.execute(&[0, 0], context.clone()).unwrap();
    let second = readers.pop().unwrap();
    let mut first = readers.pop().unwrap();
    assert_eq!(starts.get(), 2);
    assert_eq!(*block_on(first.next()).unwrap().unwrap(), 0);
    drop(second);
    for expected in 1..4 {
        assert_eq!(*block_on(first.next()).unwrap().unwrap(), expected);
    }
    assert!(block_on(first.next()).is_none());
    assert_eq!(polls.get(), 4);
    drop(first);
    assert_eq!(context.retained_bytes(), 0);
}

// A materialized node runs once per run, to the end, and every consumer
// reads its finished output, charged to the run until all have read it.
#[test]
fn materialized_node_runs_once_and_is_held() {
    let (source, starts, polls) = source(false);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![0], Identity, ONE_EVALUATION).unwrap();
    dag.add(2, vec![1], Identity, Lifetime::None).unwrap();
    dag.add(3, vec![1], Identity, Lifetime::None).unwrap();
    for _ in 0..2 {
        let run = query_context(1024);
        let outputs = dag.execute(&[2, 3], run.clone()).unwrap();
        for output in outputs {
            assert_eq!(
                values(block_on(output.collect::<Vec<_>>())),
                vec![Ok(0), Ok(1), Ok(2), Ok(3)]
            );
        }
        // The held output, and the first root's batches collected while the
        // second root has not read it yet.
        assert_eq!(run.peak_bytes(), 2 * 4 * 8);
        assert_eq!(run.retained_bytes(), 0);
    }
    assert_eq!(starts.get(), 2);
    assert_eq!(polls.get(), 8);
}

// Failure reaches every consumer, and a held node that fails partway is not
// read in part: each consumer gets only the error, so none reports success
// after its first batch. Cancellation stops further producer work.
#[test]
fn broadcast_error_and_cancel() {
    let (source, _, polls) = source(true);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![0], Identity, ONE_EVALUATION).unwrap();
    let mut outputs = dag.execute(&[1, 1], query_context(1024)).unwrap();
    let a = outputs.pop().unwrap();
    let b = outputs.pop().unwrap();
    let (a, b) = block_on(async { futures::join!(a.collect::<Vec<_>>(), b.collect::<Vec<_>>()) });
    for values in [a, b] {
        assert_eq!(values.len(), 1);
        assert!(matches!(values[0], Err(Error::AtNode { node: 0, .. })));
    }
    assert_eq!(polls.get(), 2);
    let run = query_context(1024);
    let mut output = dag.execute(&[0], run.clone()).unwrap().remove(0);
    run.cancel();
    assert!(matches!(
        block_on(output.next()),
        Some(Err(Error::Cancelled))
    ));
    assert!(block_on(output.next()).is_none());
    assert_eq!(polls.get(), 2);
}

// Retaining a consumer output retains its budget lease after it is read.
#[test]
fn retained_outputs_count_against_budget() {
    let (source, _, _) = source(false);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    let run = query_context(8);
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

// Invalid DAGs fail before even starting a source.
#[test]
fn invalid_dags_do_not_start_sources() {
    let (source, starts, _) = source(false);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![2], Identity, Lifetime::None).unwrap();
    dag.add(2, vec![1], Identity, Lifetime::None).unwrap();
    assert!(dag.execute(&[0, 1], query_context(1024)).is_err());
    assert_eq!(starts.get(), 0);
    let mut missing = PhysicalDAG::default();
    missing.add(1, vec![9], Identity, Lifetime::None).unwrap();
    assert!(missing.validate(&[1]).is_err());
    let mut arity = PhysicalDAG::default();
    arity.add(1, vec![], Identity, Lifetime::None).unwrap();
    assert!(arity.validate(&[1]).is_err());
}

// An always-ready source must yield so cancellation can be polled on this worker.
#[test]
fn ready_sources_cooperate_with_cancellation() {
    let (mut source, _, polls) = source(false);
    source.end = 10_000;
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    let context = query_context(1024);
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
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    for id in 1..129 {
        dag.add(id, vec![id - 1], Identity, Lifetime::None).unwrap();
    }
    assert!(dag.validate(&(0..129).collect::<Vec<_>>()).is_err());
}

// Reads its left input to the end before its right, as `SeriesBinary` does.
struct LeftThenRight;
impl PhysicalOperator<u64, ()> for LeftThenRight {
    fn name(&self) -> &str {
        "LeftThenRight"
    }
    fn input_schemas(&self) -> Vec<()> {
        vec![(), ()]
    }
    fn output_schema(&self) {}
    fn output_bytes(&self, _: &u64) -> usize {
        8
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<Input<'a, u64>>,
        _: RunContext,
    ) -> Result<OutputStream<'a, u64>, Error> {
        Ok(stream::iter(inputs)
            .flatten()
            .map(|value| value.map(|v| *v))
            .boxed_local())
    }
}

// Reads its input one minute before the run's evaluation time, like a
// `TimeShift` with an `@` anchor.
struct MinuteEarlier;
impl PhysicalOperator<u64, ()> for MinuteEarlier {
    fn name(&self) -> &str {
        "MinuteEarlier"
    }
    fn input_context(&self, context: &RunContext) -> Result<RunContext, Error> {
        Ok(context.at_evaluation_time(context.evaluation_time_ms().unwrap() - 60_000))
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

/// Runs `run` on its own thread and fails the test if it does not return in
/// time, so a deadlocked run fails instead of hanging the test binary.
fn within_timeout<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(run()).unwrap());
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the run did not finish")
}

/// More batches than any queue bound the runtime ever had (8 by default).
const MANY_BATCHES: u64 = 20;
/// Node 0 emits `MANY_BATCHES` batches; nodes 1 and 2 each read it.
fn two_readers_of_one_leaf(dag: &mut PhysicalDAG<'static, u64, ()>) -> Rc<Cell<usize>> {
    let (mut source, starts, _) = source(false);
    source.end = MANY_BATCHES;
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![0], Identity, Lifetime::None).unwrap();
    dag.add(2, vec![0], Identity, Lifetime::None).unwrap();
    starts
}
fn ingestion_context(max_bytes: usize) -> RunContext {
    RunContext::new(
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 10,
        },
        Limits { max_bytes },
    )
    .unwrap()
}
fn values(results: Vec<Result<SharedValue<u64>, Error>>) -> Vec<Result<u64, Error>> {
    results.into_iter().map(|r| r.map(|v| *v)).collect()
}
fn all_batches() -> Vec<Result<u64, Error>> {
    (0..MANY_BATCHES).map(Ok).collect()
}

// Two queries read one stored view (y16): draining their roots one after
// another gives the same answers as polling them together.
#[test]
fn roots_reading_one_leaf_drain_in_order() {
    within_timeout(|| {
        let mut dag = PhysicalDAG::default();
        two_readers_of_one_leaf(&mut dag);
        let outputs = dag.execute(&[1, 2], query_context(1024)).unwrap();
        let joined: Vec<_> = block_on(futures::future::join_all(
            outputs.into_iter().map(|output| output.collect::<Vec<_>>()),
        ))
        .into_iter()
        .map(values)
        .collect();
        assert_eq!(joined, vec![all_batches(), all_batches()]);
        let run = query_context(1024);
        let outputs = dag.execute(&[1, 2], run.clone()).unwrap();
        let drained: Vec<_> = outputs
            .into_iter()
            .map(|output| values(block_on(output.collect::<Vec<_>>())))
            .collect();
        assert_eq!(drained, joined);
        assert_eq!(run.retained_bytes(), 0);
    });
}

// One query whose two branches read one stored view (b17, g10), combined by
// an operator that reads its left branch to the end first.
#[test]
fn one_root_with_two_branches_over_one_leaf() {
    within_timeout(|| {
        let mut dag = PhysicalDAG::default();
        two_readers_of_one_leaf(&mut dag);
        dag.add(3, vec![1, 2], LeftThenRight, Lifetime::None)
            .unwrap();
        let output = dag.execute(&[3], query_context(1024)).unwrap().remove(0);
        let both: Vec<_> = all_batches().into_iter().chain(all_batches()).collect();
        assert_eq!(values(block_on(output.collect::<Vec<_>>())), both);
    });
}

// At ingestion time one read of a source feeds several chain builds: the
// runtime polls every root itself and each batch reaches every consumer.
#[test]
fn ingestion_fan_out_reads_the_source_once() {
    within_timeout(|| {
        let mut dag = PhysicalDAG::default();
        let starts = two_readers_of_one_leaf(&mut dag);
        dag.add(3, vec![0], Identity, Lifetime::None).unwrap();
        dag.add(4, vec![1, 2], LeftThenRight, Lifetime::None)
            .unwrap();
        let run = ingestion_context(1024);
        let outputs = block_on(dag.execute_ingestion(&[3, 4], run.clone()).unwrap()).unwrap();
        let outputs: Vec<Vec<Result<u64, Error>>> = outputs
            .into_iter()
            .map(|batches| batches.into_iter().map(|v| Ok(*v)).collect())
            .collect();
        let both: Vec<_> = all_batches().into_iter().chain(all_batches()).collect();
        assert_eq!(outputs, vec![all_batches(), both]);
        assert_eq!(starts.get(), 1);
        assert_eq!(run.retained_bytes(), 0);
    });
}

// Each entry point runs one scope and names the other for the other scope.
#[test]
fn execute_rejects_ingestion_scope_and_vice_versa() {
    let mut dag = PhysicalDAG::default();
    two_readers_of_one_leaf(&mut dag);
    let Err(error) = dag.execute(&[1], ingestion_context(1024)) else {
        panic!("`execute` ran an ingestion scope");
    };
    assert_eq!(
        error,
        Error::Invalid(
            "`execute` takes a query scope; run an ingestion scope with `execute_ingestion`".into()
        )
    );
    let Err(error) = dag.execute_ingestion(&[1], query_context(1024)) else {
        panic!("`execute_ingestion` ran a query scope");
    };
    assert_eq!(
        error,
        Error::Invalid(
            "`execute_ingestion` takes an ingestion scope; run a query scope with `execute`".into()
        )
    );
}

// A computed node that is not materialized has exactly one consumer at
// query time; two mean a planner bug, reported before any operator starts.
#[test]
fn query_time_fan_out_of_a_not_materialized_node_is_an_error() {
    let (source, starts, _) = source(false);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![0], Identity, Lifetime::None).unwrap();
    dag.add(2, vec![1], Identity, Lifetime::None).unwrap();
    dag.add(3, vec![1], Identity, Lifetime::None).unwrap();
    let Err(error) = dag.execute(&[2, 3], query_context(1024)) else {
        panic!("a not-materialized node with two consumers ran");
    };
    assert_eq!(
        error,
        Error::Invalid(
            "node 1 (Identity) at evaluation time 100 has 2 consumers but Lifetime::None; \
             a shared query-time output must be OneEvaluation (planner bug)"
                .into()
        )
    );
    assert_eq!(starts.get(), 0);
    // Read at two evaluation times, it runs once per time, each with one consumer.
    dag.add(4, vec![1], MinuteEarlier, Lifetime::None).unwrap();
    let outputs = dag.execute(&[2, 4], query_context(1024)).unwrap();
    for output in outputs {
        assert_eq!(values(block_on(output.collect::<Vec<_>>())).len(), 4);
    }
}

// A one-evaluation output over the run's byte budget fails with
// `MemoryLimit`; it is not spilled.
#[test]
fn one_evaluation_output_over_budget_is_a_memory_limit() {
    within_timeout(|| {
        let mut dag = PhysicalDAG::default();
        let (mut source, _, _) = source(false);
        source.end = MANY_BATCHES;
        dag.add(0, vec![], source, Lifetime::None).unwrap();
        dag.add(1, vec![0], Identity, ONE_EVALUATION).unwrap();
        let run = query_context(5 * 8);
        let output = dag.execute(&[1], run.clone()).unwrap().remove(0);
        let read = values(block_on(output.collect::<Vec<_>>()));
        assert_eq!(read.last(), Some(&Err(Error::MemoryLimit)));
        assert_eq!(run.retained_bytes(), 0);
    });
}

// Ignores its input: drops it in `start` and emits nothing.
struct DropsInput;
impl PhysicalOperator<u64, ()> for DropsInput {
    fn name(&self) -> &str {
        "DropsInput"
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
        _: Vec<Input<'a, u64>>,
        _: RunContext,
    ) -> Result<OutputStream<'a, u64>, Error> {
        Ok(stream::empty().boxed_local())
    }
}

// A consumer that drops its input in `start`, before the node's other
// consumer subscribes, leaves the node running for that one.
#[test]
fn an_input_dropped_at_start_leaves_the_node_to_later_consumers() {
    let (source, _, _) = source(false);
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![0], Identity, ONE_EVALUATION).unwrap();
    dag.add(2, vec![1], DropsInput, Lifetime::None).unwrap();
    dag.add(3, vec![1], Identity, Lifetime::None).unwrap();
    let run = query_context(1024);
    let outputs = dag.execute(&[2, 3], run.clone()).unwrap();
    let drained: Vec<_> = outputs
        .into_iter()
        .map(|output| values(block_on(output.collect::<Vec<_>>())))
        .collect();
    assert_eq!(drained, vec![vec![], vec![Ok(0), Ok(1), Ok(2), Ok(3)]]);
    assert_eq!(run.retained_bytes(), 0);
}

// A cancellation seen by one root releases what a held node keeps for a
// root nobody polls, and every root then ends with `Cancelled`.
#[test]
fn cancellation_releases_held_batches_of_unpolled_roots() {
    let (mut source, _, _) = source(false);
    source.end = MANY_BATCHES;
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], source, Lifetime::None).unwrap();
    dag.add(1, vec![0], Identity, ONE_EVALUATION).unwrap();
    dag.add(2, vec![1], Identity, Lifetime::None).unwrap();
    dag.add(3, vec![1], Identity, Lifetime::None).unwrap();
    let run = query_context(1024);
    let mut outputs = dag.execute(&[2, 3], run.clone()).unwrap();
    let first = block_on(outputs[0].next()).unwrap().unwrap();
    assert_eq!(*first, 0);
    drop(first);
    assert!(run.retained_bytes() > 0);
    run.cancel();
    assert!(matches!(
        block_on(outputs[0].next()),
        Some(Err(Error::Cancelled))
    ));
    assert_eq!(run.retained_bytes(), 0);
    assert!(matches!(
        block_on(outputs[1].next()),
        Some(Err(Error::Cancelled))
    ));
}
