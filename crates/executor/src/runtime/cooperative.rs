//! Worker-local CPU loops yield so other consumers and cancellation can progress.
use super::RunContext;
use crate::Error;
use std::task::Poll;

pub(crate) struct Cooperative {
    context: RunContext,
    remaining: usize,
}
impl Cooperative {
    pub(crate) fn new(context: &RunContext) -> Self {
        Self {
            context: context.clone(),
            remaining: 1024,
        }
    }
    pub(crate) async fn checkpoint(&mut self) -> Result<(), Error> {
        self.checkpoint_rows(1).await
    }

    /// A checkpoint after `rows` units of work at once (a columnar loop
    /// over `rows` rows): it yields as often as `rows` single checkpoints.
    pub(crate) async fn checkpoint_rows(&mut self, rows: usize) -> Result<(), Error> {
        if self.context.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.remaining = self.remaining.saturating_sub(rows);
        if self.remaining == 0 {
            self.remaining = 1024;
            let mut yielded = false;
            futures::future::poll_fn(|cx| {
                if self.context.is_cancelled() {
                    return Poll::Ready(Err(Error::Cancelled));
                }
                if yielded {
                    return Poll::Ready(Ok(()));
                }
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            })
            .await?;
        }
        Ok(())
    }
}
