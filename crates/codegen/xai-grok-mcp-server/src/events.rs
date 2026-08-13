use std::sync::Arc;

use tokio::sync::broadcast;

/// Transport-neutral gateway activity. Events deliberately exclude tool
/// arguments and output: those can contain workspace data or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    ClientConnected,
    ClientDisconnected,
    ToolCallStarted {
        call_id: String,
        tool_name: String,
    },
    ToolCallFinished {
        call_id: String,
        tool_name: String,
    },
    ToolCallFailed {
        call_id: String,
        tool_name: String,
        reason: String,
    },
    ApprovalRequested {
        call_id: String,
        tool_name: String,
    },
    ApprovalResolved {
        call_id: String,
        tool_name: String,
        allowed: bool,
    },
}

/// Bounded, lossy fan-out for gateway observers. A lagging observer receives
/// `broadcast::error::RecvError::Lagged`; it never delays a tool call.
#[derive(Debug, Clone)]
pub struct GatewayEventBus {
    sender: Arc<broadcast::Sender<GatewayEvent>>,
}

impl Default for GatewayEventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

impl GatewayEventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            sender: Arc::new(sender),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.sender.subscribe()
    }

    pub fn emit(&self, event: GatewayEvent) {
        // `send` only fails when nobody is listening; activity recording must
        // never turn that ordinary condition into a tool-call failure.
        let _ = self.sender.send(event);
    }
}
