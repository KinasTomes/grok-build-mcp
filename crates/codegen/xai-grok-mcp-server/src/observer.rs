//! Transport-independent projection of gateway activity for a local observer.
//!
//! The bridge never receives a toolset and cannot execute tools. It is only a
//! subscriber to [`GatewayEventBus`] plus a consumer of [`PendingApproval`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{ApprovalDecision, GatewayEvent, GatewayEventBus, PendingApproval};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverEventKind {
    ClientConnected,
    ClientDisconnected,
    ToolStarted,
    ToolFinished,
    ToolFailed,
    ApprovalRequested,
    ApprovalResolved,
}

/// Safe, bounded data emitted to a local observer. It deliberately has no
/// raw tool arguments, output, environment, or downstream credentials.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ObserverEvent {
    pub kind: ObserverEventKind,
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
    pub summary: Option<String>,
    pub duration_ms: Option<u128>,
    pub allowed: Option<bool>,
}

/// Local pending request as displayed by the UI. The approval sender remains
/// private to the bridge, so a frontend can never forge an approval response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ObserverApprovalRequest {
    pub call_id: String,
    pub tool_name: String,
    pub summary: String,
}

/// One local observer/approval consumer. Multiple pending approvals are kept
/// independently by call ID. Dropping or disconnecting this bridge drops their
/// one-shot senders, which makes the gateway fail closed.
pub struct LocalObserverBridge {
    updates: broadcast::Sender<ObserverEvent>,
    pending: Mutex<HashMap<String, PendingApproval>>,
    shutdown: CancellationToken,
}

impl LocalObserverBridge {
    pub fn spawn(
        event_bus: &GatewayEventBus,
        approvals: mpsc::Receiver<PendingApproval>,
    ) -> Arc<Self> {
        let (updates, _) = broadcast::channel(256);
        let bridge = Arc::new(Self {
            updates,
            pending: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        });
        tokio::spawn(Self::pump(
            Arc::clone(&bridge),
            event_bus.subscribe(),
            approvals,
        ));
        bridge
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ObserverEvent> {
        self.updates.subscribe()
    }

    pub fn resolve(&self, call_id: &str, decision: ApprovalDecision) -> bool {
        let pending = self
            .pending
            .lock()
            .expect("observer approval lock poisoned")
            .remove(call_id);
        let Some(pending) = pending else {
            return false;
        };
        pending.resolve(decision);
        true
    }

    pub fn disconnect(&self) {
        self.shutdown.cancel();
        // Dropping all one-shot senders causes every outstanding Ask to deny
        // immediately. This cannot execute a request after UI disconnect.
        self.pending
            .lock()
            .expect("observer approval lock poisoned")
            .clear();
    }

    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .expect("observer approval lock poisoned")
            .len()
    }

    async fn pump(
        bridge: Arc<Self>,
        mut gateway_events: broadcast::Receiver<GatewayEvent>,
        mut approvals: mpsc::Receiver<PendingApproval>,
    ) {
        let mut started = HashMap::<String, Instant>::new();
        loop {
            tokio::select! {
                _ = bridge.shutdown.cancelled() => break,
                approval = approvals.recv() => match approval {
                    Some(approval) => {
                        let request = ObserverApprovalRequest {
                            call_id: approval.request.call_id.clone(),
                            tool_name: approval.request.tool_name.clone(),
                            summary: redact_summary(&approval.request.summary),
                        };
                        bridge.pending.lock().expect("observer approval lock poisoned")
                            .insert(request.call_id.clone(), approval);
                        bridge.emit(ObserverEvent {
                            kind: ObserverEventKind::ApprovalRequested,
                            call_id: Some(request.call_id),
                            tool_name: Some(request.tool_name),
                            summary: Some(request.summary),
                            duration_ms: None,
                            allowed: None,
                        });
                    }
                    None => break,
                },
                event = gateway_events.recv() => match event {
                    Ok(event) => bridge.project_event(event, &mut started),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        bridge
            .pending
            .lock()
            .expect("observer approval lock poisoned")
            .clear();
    }

    fn project_event(&self, event: GatewayEvent, started: &mut HashMap<String, Instant>) {
        let update = match event {
            GatewayEvent::ClientConnected => ObserverEvent {
                kind: ObserverEventKind::ClientConnected,
                call_id: None,
                tool_name: None,
                summary: None,
                duration_ms: None,
                allowed: None,
            },
            GatewayEvent::ClientDisconnected => ObserverEvent {
                kind: ObserverEventKind::ClientDisconnected,
                call_id: None,
                tool_name: None,
                summary: None,
                duration_ms: None,
                allowed: None,
            },
            GatewayEvent::ToolCallStarted { call_id, tool_name } => {
                started.insert(call_id.clone(), Instant::now());
                ObserverEvent {
                    kind: ObserverEventKind::ToolStarted,
                    call_id: Some(call_id),
                    tool_name: Some(tool_name),
                    summary: None,
                    duration_ms: None,
                    allowed: None,
                }
            }
            GatewayEvent::ToolCallFinished { call_id, tool_name } => ObserverEvent {
                kind: ObserverEventKind::ToolFinished,
                duration_ms: started
                    .remove(&call_id)
                    .map(|instant| instant.elapsed().as_millis()),
                call_id: Some(call_id),
                tool_name: Some(tool_name),
                summary: None,
                allowed: None,
            },
            GatewayEvent::ToolCallFailed {
                call_id,
                tool_name,
                reason,
            } => ObserverEvent {
                kind: ObserverEventKind::ToolFailed,
                duration_ms: started
                    .remove(&call_id)
                    .map(|instant| instant.elapsed().as_millis()),
                call_id: Some(call_id),
                tool_name: Some(tool_name),
                summary: Some(redact_summary(&reason)),
                allowed: None,
            },
            GatewayEvent::ApprovalRequested { call_id, tool_name } => ObserverEvent {
                kind: ObserverEventKind::ApprovalRequested,
                call_id: Some(call_id),
                tool_name: Some(tool_name),
                summary: None,
                duration_ms: None,
                allowed: None,
            },
            GatewayEvent::ApprovalResolved {
                call_id,
                tool_name,
                allowed,
            } => ObserverEvent {
                kind: ObserverEventKind::ApprovalResolved,
                call_id: Some(call_id),
                tool_name: Some(tool_name),
                summary: None,
                duration_ms: None,
                allowed: Some(allowed),
            },
        };
        self.emit(update);
    }

    fn emit(&self, event: ObserverEvent) {
        let _ = self.updates.send(event);
    }
}

/// Prevent common secret-bearing values from reaching a local webview. This is
/// defense in depth: gateway events already avoid raw argument objects.
pub fn redact_summary(summary: &str) -> String {
    let bounded: String = summary.chars().take(240).collect();
    let mut redact_next = false;
    bounded
        .split_whitespace()
        .map(|word| {
            if std::mem::take(&mut redact_next) {
                return "[REDACTED]";
            }
            let lower = word.to_ascii_lowercase();
            if lower == "--token"
                || lower == "--password"
                || lower == "--api-key"
                || lower == "authorization:"
            {
                redact_next = true;
                "[REDACTED]"
            } else if lower.contains("token=")
                || lower.contains("password=")
                || lower.contains("secret=")
                || lower.contains("authorization:")
                || lower.contains("api_key=")
            {
                "[REDACTED]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalBroker, ApprovalRequest, ChannelApprovalBroker, GatewayEventBus};

    #[tokio::test]
    async fn bridge_projects_events_and_resolves_multiple_pending_approvals() {
        let events = GatewayEventBus::new(8);
        let (broker, approvals) = ChannelApprovalBroker::new(4);
        let bridge = LocalObserverBridge::spawn(&events, approvals);
        let mut updates = bridge.subscribe();
        let first_broker = Arc::clone(&broker);
        let first = tokio::spawn(async move {
            first_broker
                .request(ApprovalRequest {
                    call_id: "one".into(),
                    tool_name: "run_terminal_cmd".into(),
                    summary: "cargo test".into(),
                })
                .await
        });
        let second_broker = Arc::clone(&broker);
        let second = tokio::spawn(async move {
            second_broker
                .request(ApprovalRequest {
                    call_id: "two".into(),
                    tool_name: "run_terminal_cmd".into(),
                    summary: "cargo build".into(),
                })
                .await
        });
        for _ in 0..2 {
            let _ = updates.recv().await.unwrap();
        }
        assert_eq!(bridge.pending_count(), 2);
        assert!(bridge.resolve("one", ApprovalDecision::AllowOnce));
        assert!(bridge.resolve("two", ApprovalDecision::Deny));
        assert_eq!(first.await.unwrap(), ApprovalDecision::AllowOnce);
        assert_eq!(second.await.unwrap(), ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn disconnect_drops_pending_approval_and_fails_closed() {
        let events = GatewayEventBus::new(8);
        let (broker, approvals) = ChannelApprovalBroker::new(1);
        let bridge = LocalObserverBridge::spawn(&events, approvals);
        let broker = Arc::clone(&broker);
        let approval = tokio::spawn(async move {
            broker
                .request(ApprovalRequest {
                    call_id: "one".into(),
                    tool_name: "run_terminal_cmd".into(),
                    summary: "TOKEN=top-secret cargo test".into(),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while bridge.pending_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        bridge.disconnect();
        assert_eq!(approval.await.unwrap(), ApprovalDecision::Deny);
    }

    #[test]
    fn redacts_secret_bearing_summary() {
        assert_eq!(
            redact_summary("cargo test TOKEN=secret"),
            "cargo test [REDACTED]"
        );
        assert_eq!(
            redact_summary("curl --token top-secret https://example.invalid"),
            "curl [REDACTED] [REDACTED] https://example.invalid"
        );
    }
}
