use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

/// A single non-persistent approval decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
}

/// Context provided to a local approval consumer. This is intentionally not a
/// full serialized argument object; `summary` is bounded and only used to make
/// an interactive decision possible.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool_name: String,
    pub summary: String,
}

/// Transport-independent source of one-shot local approval decisions.
pub trait ApprovalBroker: Send + Sync {
    fn request(
        &self,
        request: ApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send + '_>>;
}

/// A development/test broker. A consumer receives each pending request and
/// resolves it once; dropping the consumer resolves all requests as deny.
pub struct ChannelApprovalBroker {
    sender: mpsc::Sender<PendingApproval>,
}

pub struct PendingApproval {
    pub request: ApprovalRequest,
    response: oneshot::Sender<ApprovalDecision>,
}

impl PendingApproval {
    pub fn resolve(self, decision: ApprovalDecision) {
        let _ = self.response.send(decision);
    }
}

impl ChannelApprovalBroker {
    pub fn new(capacity: usize) -> (Arc<Self>, mpsc::Receiver<PendingApproval>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (Arc::new(Self { sender }), receiver)
    }
}

impl ApprovalBroker for ChannelApprovalBroker {
    fn request(
        &self,
        request: ApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send + '_>> {
        Box::pin(async move {
            let (response, wait) = oneshot::channel();
            if self
                .sender
                .send(PendingApproval { request, response })
                .await
                .is_err()
            {
                return ApprovalDecision::Deny;
            }
            wait.await.unwrap_or(ApprovalDecision::Deny)
        })
    }
}

/// Development-only terminal approval consumer. It writes prompts to stderr
/// and reads stdin; callers must not enable it with MCP stdio transport.
pub struct TerminalApprovalBroker;

impl ApprovalBroker for TerminalApprovalBroker {
    fn request(
        &self,
        request: ApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send + '_>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                use std::io::{BufRead, Write};
                let mut stderr = std::io::stderr().lock();
                let _ = writeln!(
                    stderr,
                    "Approval requested: {}\n{}\n[y/N]",
                    request.tool_name, request.summary
                );
                let _ = stderr.flush();
                let mut line = String::new();
                let _ = std::io::stdin().lock().read_line(&mut line);
                if matches!(line.trim(), "y" | "Y" | "yes" | "YES") {
                    ApprovalDecision::AllowOnce
                } else {
                    ApprovalDecision::Deny
                }
            })
            .await
            .unwrap_or(ApprovalDecision::Deny)
        })
    }
}
