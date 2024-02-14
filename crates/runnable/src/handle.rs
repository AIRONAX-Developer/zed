use anyhow::{Context, Result};
use async_process::{ChildStderr, ChildStdout, ExitStatus};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::future::{join_all, BoxFuture, Shared};
pub use futures::stream::Aborted as RunnableTerminated;
use futures::stream::{AbortHandle, Abortable};
use futures::{AsyncBufReadExt, AsyncRead, Future, FutureExt};
use gpui::{AppContext, AsyncAppContext, Task};
use parking_lot::Mutex;
use smol::io::BufReader;
use std::sync::Arc;
use std::task::Poll;
use util::ResultExt;

use crate::ExecutionResult;

/// Represents a runnable that's already underway. That runnable can be cancelled at any time.
#[derive(Clone)]
pub struct Handle {
    pub(crate) fut:
        Shared<Task<Result<Result<ExitStatus, Arc<anyhow::Error>>, RunnableTerminated>>>,
    pub output: Option<PendingOutput>,
    cancel_token: AbortHandle,
}

#[derive(Clone, Debug)]
pub struct PendingOutput {
    output_read_tasks: [Shared<Task<()>>; 2],
    full_output: Arc<Mutex<String>>,
    output_lines_rx: Arc<Mutex<UnboundedReceiver<String>>>,
}

impl PendingOutput {
    pub(super) fn new(stdout: ChildStdout, stderr: ChildStderr, cx: &mut AsyncAppContext) -> Self {
        let (output_lines_tx, output_lines_rx) = futures::channel::mpsc::unbounded();
        let output_lines_rx = Arc::new(Mutex::new(output_lines_rx));
        let full_output = Arc::new(Mutex::new(String::new()));

        let stdout_capture = Arc::clone(&full_output);
        let stdout_tx = output_lines_tx.clone();
        let stdout_task = cx
            .background_executor()
            .spawn(async move {
                handle_output(stdout, stdout_tx, stdout_capture)
                    .await
                    .context("stdout capture")
                    .log_err();
            })
            .shared();

        let stderr_capture = Arc::clone(&full_output);
        let stderr_tx = output_lines_tx;
        let stderr_task = cx
            .background_executor()
            .spawn(async move {
                handle_output(stderr, stderr_tx, stderr_capture)
                    .await
                    .context("stderr capture")
                    .log_err();
            })
            .shared();

        Self {
            output_read_tasks: [stdout_task, stderr_task],
            full_output,
            output_lines_rx,
        }
    }

    pub fn subscribe(&self) -> Arc<Mutex<UnboundedReceiver<String>>> {
        Arc::clone(&self.output_lines_rx)
    }

    pub fn full_output(self, cx: &mut AppContext) -> Task<String> {
        cx.spawn(|_| async move {
            let _: Vec<()> = join_all(self.output_read_tasks).await;
            self.full_output.lock().clone()
        })
    }
}

impl Handle {
    pub fn new(
        fut: BoxFuture<'static, Result<ExitStatus, Arc<anyhow::Error>>>,
        output: Option<PendingOutput>,
        cx: AsyncAppContext,
    ) -> Result<Self> {
        let (cancel_token, abort_registration) = AbortHandle::new_pair();
        let fut = cx
            .spawn(move |_| Abortable::new(fut, abort_registration))
            .shared();
        Ok(Self {
            fut,
            output,
            cancel_token,
        })
    }

    /// Returns a handle that can be used to cancel this runnable.
    pub fn termination_handle(&self) -> AbortHandle {
        self.cancel_token.clone()
    }

    pub fn result<'a>(&self) -> Option<Result<ExecutionResult, RunnableTerminated>> {
        self.fut.peek().cloned().map(|res| {
            res.map(|runnable_result| ExecutionResult {
                status: runnable_result,
                output: self.output.clone(),
            })
        })
    }
}

impl Future for Handle {
    type Output = Result<ExecutionResult, RunnableTerminated>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        match self.fut.poll_unpin(cx) {
            Poll::Ready(res) => match res {
                Ok(runnable_result) => Poll::Ready(Ok(ExecutionResult {
                    status: runnable_result,
                    output: self.output.clone(),
                })),
                Err(aborted) => Poll::Ready(Err(aborted)),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

async fn handle_output<Output>(
    output: Output,
    output_tx: UnboundedSender<String>,
    capture: Arc<Mutex<String>>,
) -> anyhow::Result<()>
where
    Output: AsyncRead + Unpin + Send + 'static,
{
    let mut output = BufReader::new(output);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();

        let bytes_read = output
            .read_until(b'\n', &mut buffer)
            .await
            .context("reading output newline")?;
        if bytes_read == 0 {
            return Ok(());
        }

        let output_line = String::from_utf8_lossy(&buffer);
        capture.lock().push_str(&output_line);
        output_tx.unbounded_send(output_line.into_owned()).ok();

        // Don't starve the main thread when receiving lots of messages at once.
        smol::future::yield_now().await;
    }
}