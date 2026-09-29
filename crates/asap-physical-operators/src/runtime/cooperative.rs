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
        if self.context.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.remaining -= 1;
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
