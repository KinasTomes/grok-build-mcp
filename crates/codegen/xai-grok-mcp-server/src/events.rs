use std::sync::Arc;

use tokio::sync::broadcast;

/// Bounded, redacted operation data needed to render native Grok tool blocks
/// in the local observer. This is intentionally a projection rather than a
/// raw MCP argument object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObserverToolDetails {
    Edit {
        path: String,
        old_text: String,
        new_text: String,
    },
    Write {
        path: String,
        content: String,
    },
}

/// Transport-neutral gateway activity.
///
/// Observer fields are deliberately redacted and bounded. They are useful for
/// a local activity view, but are not a second copy of raw MCP arguments or
/// arbitrary tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    ClientConnected,
    ClientDisconnected,
    ToolCallStarted {
        call_id: String,
        tool_name: String,
        summary: String,
        details: Option<ObserverToolDetails>,
    },
    ToolCallFinished {
        call_id: String,
        tool_name: String,
        /// A bounded, redacted excerpt of the tool result for the local
        /// observer. It is never sent to another MCP client.
        output: Option<String>,
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

/// Bounded display text for a local observer. This is defense in depth over
/// the gateway's normal no-raw-arguments event policy: common secret-bearing
/// command-line values are replaced before a UI receives them.
pub(crate) fn redact_observer_text(text: &str, max_chars: usize) -> String {
    let bounded: String = text.chars().take(max_chars).collect();
    let mut redact_next = false;
    bounded
        .split_whitespace()
        .map(|word| {
            if std::mem::take(&mut redact_next) {
                return "[REDACTED]".to_owned();
            }
            let lower = word.to_ascii_lowercase();
            let secret_key = lower.trim_end_matches(':');
            if matches!(
                secret_key,
                "--token" | "--password" | "--api-key" | "authorization" | "bearer" | "cookie"
            ) {
                redact_next = true;
                "[REDACTED]".to_owned()
            } else if [
                "token=",
                "password=",
                "secret=",
                "authorization:",
                "api_key=",
                "apikey=",
                "credential=",
            ]
            .iter()
            .any(|needle| lower.contains(needle))
            {
                "[REDACTED]".to_owned()
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Same redaction policy as [`redact_observer_text`] while preserving lines so
/// the native file/list/diff viewers remain legible.
pub(crate) fn redact_observer_multiline(text: &str, max_chars: usize) -> String {
    let bounded: String = text.chars().take(max_chars).collect();
    bounded
        .lines()
        .map(|line| redact_observer_text(line, max_chars))
        .collect::<Vec<_>>()
        .join("\n")
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
