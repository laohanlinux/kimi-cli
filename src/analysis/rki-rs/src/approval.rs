//! Capability-based approval runtime with multi-sink routing.
//!
//! `ApprovalRuntime` is the session-level source of truth for pending
//! tool-use approvals. Requests are projected onto the wire for UI modals.

use crate::capability::{CapabilityEngine, Decision, extract_constraints, tool_to_capability};
use crate::wire::{RootWireHub, WireEvent};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

#[cfg(test)]
use crate::capability::{TrustProfile, CapabilityOverride};

#[derive(Clone)]
pub struct ApprovalRequestRecord {
    pub id: String,
    pub tool_call_id: String,
    pub sender: String,
    pub action: String,
    pub description: String,
    pub display: String,
    pub resolved: bool,
    pub approved: bool,
    pub feedback: Option<String>,
}

/// Named approval sink with explicit ownership (§6.3 deviation).
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    fn name(&self) -> &str;
    fn priority(&self) -> i32;
    fn is_available(&self) -> bool;
    /// Timeout for this approval channel in seconds.
    fn timeout_secs(&self) -> u64 { 300 }
    /// Handle an approval request. Returns Ok(approved) if handled, Err otherwise.
    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool>;
}

/// Broadcast-based sink: emits ApprovalRequest onto RootWireHub and waits for resolve.
pub struct BroadcastSink {
    hub: RootWireHub,
}

impl BroadcastSink {
    pub fn new(hub: RootWireHub) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl ApprovalSink for BroadcastSink {
    fn name(&self) -> &str { "broadcast" }
    fn priority(&self) -> i32 { 0 }
    fn is_available(&self) -> bool { true }

    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        self.hub.broadcast(WireEvent::ApprovalRequest {
            id: req.id.clone(),
            tool_call_id: req.tool_call_id.clone(),
            sender: req.sender.clone(),
            action: req.action.clone(),
            description: req.description.clone(),
            display: req.display.clone(),
        });
        let approved = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            resolve_rx,
        )
        .await??;
        Ok(approved)
    }
}

/// Shell UI sink: highest priority when interactive mode is active.
pub struct ShellSink {
    hub: RootWireHub,
    interactive: std::sync::atomic::AtomicBool,
}

impl ShellSink {
    pub fn new(hub: RootWireHub) -> Self {
        Self {
            hub,
            interactive: std::sync::atomic::AtomicBool::new(true),
        }
    }

    pub fn set_interactive(&self, active: bool) {
        self.interactive.store(active, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl ApprovalSink for ShellSink {
    fn name(&self) -> &str { "shell" }
    fn priority(&self) -> i32 { 100 }
    fn is_available(&self) -> bool {
        self.interactive.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        self.hub.broadcast(WireEvent::ApprovalRequest {
            id: req.id.clone(),
            tool_call_id: req.tool_call_id.clone(),
            sender: req.sender.clone(),
            action: req.action.clone(),
            description: req.description.clone(),
            display: req.display.clone(),
        });
        let approved = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            resolve_rx,
        )
        .await??;
        Ok(approved)
    }
}

/// Wire server sink: medium priority for headless/IDE clients.
pub struct WireSink {
    hub: RootWireHub,
    connected: std::sync::atomic::AtomicBool,
}

impl WireSink {
    pub fn new(hub: RootWireHub) -> Self {
        Self {
            hub,
            connected: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl ApprovalSink for WireSink {
    fn name(&self) -> &str { "wire" }
    fn priority(&self) -> i32 { 50 }
    fn is_available(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        self.hub.broadcast(WireEvent::ApprovalRequest {
            id: req.id.clone(),
            tool_call_id: req.tool_call_id.clone(),
            sender: req.sender.clone(),
            action: req.action.clone(),
            description: req.description.clone(),
            display: req.display.clone(),
        });
        let approved = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            resolve_rx,
        )
        .await??;
        Ok(approved)
    }
}

/// Background task sink: lowest priority, always available fallback.
pub struct BackgroundSink {
    hub: RootWireHub,
}

impl BackgroundSink {
    pub fn new(hub: RootWireHub) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl ApprovalSink for BackgroundSink {
    fn name(&self) -> &str { "background" }
    fn priority(&self) -> i32 { 10 }
    fn is_available(&self) -> bool { true }

    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        self.hub.broadcast(WireEvent::ApprovalRequest {
            id: req.id.clone(),
            tool_call_id: req.tool_call_id.clone(),
            sender: req.sender.clone(),
            action: req.action.clone(),
            description: req.description.clone(),
            display: req.display.clone(),
        });
        let approved = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            resolve_rx,
        )
        .await??;
        Ok(approved)
    }
}

#[async_trait]
impl ApprovalSink for Arc<ShellSink> {
    fn name(&self) -> &str {
        <ShellSink as ApprovalSink>::name(self.as_ref())
    }
    fn priority(&self) -> i32 {
        <ShellSink as ApprovalSink>::priority(self.as_ref())
    }
    fn is_available(&self) -> bool {
        <ShellSink as ApprovalSink>::is_available(self.as_ref())
    }
    fn timeout_secs(&self) -> u64 {
        <ShellSink as ApprovalSink>::timeout_secs(self.as_ref())
    }
    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        <ShellSink as ApprovalSink>::request(self.as_ref(), req, resolve_rx).await
    }
}

#[async_trait]
impl ApprovalSink for Arc<WireSink> {
    fn name(&self) -> &str {
        <WireSink as ApprovalSink>::name(self.as_ref())
    }
    fn priority(&self) -> i32 {
        <WireSink as ApprovalSink>::priority(self.as_ref())
    }
    fn is_available(&self) -> bool {
        <WireSink as ApprovalSink>::is_available(self.as_ref())
    }
    fn timeout_secs(&self) -> u64 {
        <WireSink as ApprovalSink>::timeout_secs(self.as_ref())
    }
    async fn request(
        &self,
        req: &ApprovalRequestRecord,
        resolve_rx: oneshot::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        <WireSink as ApprovalSink>::request(self.as_ref(), req, resolve_rx).await
    }
}

/// Router that directs approval requests to the highest-priority available sink.
pub struct ApprovalRouter {
    sinks: Vec<Box<dyn ApprovalSink>>,
}

impl ApprovalRouter {
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    pub fn register(&mut self, sink: Box<dyn ApprovalSink>) {
        self.sinks.push(sink);
        self.sinks.sort_by_key(|s| -s.priority());
    }

    /// Returns the highest-priority available sink, if any.
    pub fn select_sink(&self) -> Option<&dyn ApprovalSink> {
        for sink in &self.sinks {
            if sink.is_available() {
                return Some(sink.as_ref());
            }
        }
        None
    }
}

pub struct ApprovalRuntime {
    requests: Mutex<HashMap<String, ApprovalRequestRecord>>,
    waiters: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    hub: RootWireHub,
    yolo: AtomicBool,
    auto_approve_actions: Vec<String>,
    capability_engine: Option<CapabilityEngine>,
    router: ApprovalRouter,
    /// Shared shell sink (§6.3): toggle `set_interactive` for TUI vs headless.
    pub shell_sink: Arc<ShellSink>,
    /// Shared wire sink: set `set_connected(true)` when an IDE/wire client is active.
    pub wire_sink: Arc<WireSink>,
}

impl ApprovalRuntime {
    pub fn new(
        hub: RootWireHub,
        yolo: bool,
        auto_approve_actions: Vec<String>,
    ) -> Self {
        let shell_sink = Arc::new(ShellSink::new(hub.clone()));
        let wire_sink = Arc::new(WireSink::new(hub.clone()));
        let mut router = ApprovalRouter::new();
        router.register(Box::new(BackgroundSink::new(hub.clone())));
        router.register(Box::new(Arc::clone(&wire_sink)));
        router.register(Box::new(Arc::clone(&shell_sink)));
        Self {
            requests: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            hub,
            yolo: AtomicBool::new(yolo),
            auto_approve_actions,
            capability_engine: None,
            router,
            shell_sink,
            wire_sink,
        }
    }

    /// Headless / print mode: shell UI is not handling modals; prefer the wire sink when connected.
    pub fn set_headless_ide_mode(&self) {
        self.shell_sink.set_interactive(false);
        self.wire_sink.set_connected(true);
    }

    /// Highest-priority sink that would receive the next approval (diagnostics / tests).
    pub fn selected_sink_name(&self) -> Option<String> {
        self.router.select_sink().map(|s| s.name().to_string())
    }

    pub fn with_capability_engine(mut self, engine: CapabilityEngine) -> Self {
        self.capability_engine = Some(engine);
        self
    }

    #[allow(dead_code)]
    pub fn with_sink(mut self, sink: Box<dyn ApprovalSink>) -> Self {
        self.router.register(sink);
        self
    }

    pub fn is_yolo(&self) -> bool {
        self.yolo.load(Ordering::SeqCst)
    }

    pub fn set_yolo(&self, enabled: bool) {
        self.yolo.store(enabled, Ordering::SeqCst);
    }

    /// Legacy action-based approval request.
    pub async fn request(
        &self,
        tool_call_id: String,
        sender: String,
        action: String,
        description: String,
        display: String,
    ) -> anyhow::Result<bool> {
        if self.is_yolo() || self.auto_approve_actions.contains(&action) {
            return Ok(true);
        }
        self._request(tool_call_id, sender, action, description, display).await
    }

    /// Capability-based approval request with tool arguments.
    pub async fn request_tool(
        &self,
        tool_call_id: String,
        tool_name: &str,
        args: &Value,
        description: String,
        display: String,
    ) -> anyhow::Result<bool> {
        if self.is_yolo() {
            return Ok(true);
        }

        // Capability-based authorization
        if let Some(ref engine) = self.capability_engine
            && let Some(capability) = tool_to_capability(tool_name) {
                let constraints = extract_constraints(tool_name, args);
                match engine.check(capability, &constraints) {
                    Decision::Auto => return Ok(true),
                    Decision::Block => {
                        return Ok(false);
                    }
                    Decision::Prompt => {
                        // Fall through to interactive approval
                    }
                }
            }

        // Fallback to action-based auto-approve for backward compatibility
        let action = tool_name.to_string();
        if self.auto_approve_actions.contains(&action) {
            return Ok(true);
        }

        self._request(tool_call_id, "tool".to_string(), action, description, display)
            .await
    }

    async fn _request(
        &self,
        tool_call_id: String,
        sender: String,
        action: String,
        description: String,
        display: String,
    ) -> anyhow::Result<bool> {
        let id = uuid::Uuid::new_v4().to_string();
        let req = ApprovalRequestRecord {
            id: id.clone(),
            tool_call_id: tool_call_id.clone(),
            sender,
            action: action.clone(),
            description: description.clone(),
            display: display.clone(),
            resolved: false,
            approved: false,
            feedback: None,
        };
        self.requests.lock().await.insert(id.clone(), req.clone());
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(id.clone(), tx);
        let sink = self.router.select_sink()
            .ok_or_else(|| anyhow::anyhow!("No approval sink available"))?;
        let approved = sink.request(&req, rx).await?;
        Ok(approved)
    }

    pub async fn resolve(
        &self,
        request_id: String,
        approved: bool,
        feedback: Option<String>,
    ) -> anyhow::Result<()> {
        let mut requests = self.requests.lock().await;
        if let Some(req) = requests.get_mut(&request_id) {
            req.resolved = true;
            req.approved = approved;
            req.feedback = feedback.clone();
            drop(requests);
            let mut waiters = self.waiters.lock().await;
            if let Some(tx) = waiters.remove(&request_id) {
                let _ = tx.send(approved);
            }
            self.hub.broadcast(WireEvent::ApprovalResponse {
                id: request_id,
                approved,
                feedback,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> TrustProfile {
        TrustProfile {
            default: "prompt".to_string(),
            overrides: vec![
                CapabilityOverride {
                    capability: "process:exec".to_string(),
                    path: None,
                    command_pattern: Some(r"^git ".to_string()),
                    host_pattern: None,
                    decision: "auto".to_string(),
                },
                CapabilityOverride {
                    capability: "filesystem:write".to_string(),
                    path: Some("/tmp/**".to_string()),
                    command_pattern: None,
                    host_pattern: None,
                    decision: "auto".to_string(),
                },
                CapabilityOverride {
                    capability: "process:exec".to_string(),
                    path: None,
                    command_pattern: Some(r"^rm ".to_string()),
                    host_pattern: None,
                    decision: "block".to_string(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_capability_auto_approve() {
        let hub = RootWireHub::new();
        let engine = CapabilityEngine::new(test_profile());
        let approval = ApprovalRuntime::new(hub, false, vec![])
            .with_capability_engine(engine);

        let args = serde_json::json!({"command": "git status"});
        let result = approval
            .request_tool("tc1".to_string(), "shell", &args, "git status".to_string(), "".to_string())
            .await;
        assert!(result.unwrap(), "git commands should be auto-approved");
    }

    #[tokio::test]
    async fn test_capability_block() {
        let hub = RootWireHub::new();
        let engine = CapabilityEngine::new(test_profile());
        let approval = ApprovalRuntime::new(hub, false, vec![])
            .with_capability_engine(engine);

        let args = serde_json::json!({"command": "rm -rf /"});
        let result = approval
            .request_tool("tc2".to_string(), "shell", &args, "rm -rf /".to_string(), "".to_string())
            .await;
        assert!(!result.unwrap(), "rm commands should be blocked");
    }

    #[tokio::test]
    async fn test_capability_fallback_prompt() {
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let engine = CapabilityEngine::new(test_profile());
        let approval = ApprovalRuntime::new(hub.clone(), false, vec![])
            .with_capability_engine(engine);

        // Spawn a resolver task that auto-approves any request
        let approval_clone = std::sync::Arc::new(approval);
        let approval_resolve = approval_clone.clone();
        tokio::spawn(async move {
            while let Ok(envelope) = rx.recv().await {
                if let WireEvent::ApprovalRequest { id, .. } = envelope.event {
                    let _ = approval_resolve.resolve(id, true, None).await;
                    break;
                }
            }
        });

        // Unknown command should fall back to prompt (and our resolver approves it)
        let args = serde_json::json!({"command": "echo hello"});
        let result = approval_clone
            .request_tool("tc3".to_string(), "shell", &args, "echo hello".to_string(), "".to_string())
            .await;
        assert!(result.unwrap(), "unknown commands should fall through to prompt approval");
    }

    #[tokio::test]
    async fn test_capability_filesystem_auto() {
        let hub = RootWireHub::new();
        let engine = CapabilityEngine::new(test_profile());
        let approval = ApprovalRuntime::new(hub, false, vec![])
            .with_capability_engine(engine);

        let args = serde_json::json!({"path": "/tmp/test.txt"});
        let result = approval
            .request_tool("tc4".to_string(), "write_file", &args, "write".to_string(), "".to_string())
            .await;
        assert!(result.unwrap(), "writes to /tmp should be auto-approved");
    }

    #[test]
    fn test_set_yolo_toggle() {
        let hub = RootWireHub::new();
        let approval = ApprovalRuntime::new(hub, false, vec![]);
        assert!(!approval.is_yolo());
        approval.set_yolo(true);
        assert!(approval.is_yolo());
        approval.set_yolo(false);
        assert!(!approval.is_yolo());
    }

    #[tokio::test]
    async fn test_yolo_bypasses_capabilities() {
        let hub = RootWireHub::new();
        let engine = CapabilityEngine::new(test_profile());
        let approval = ApprovalRuntime::new(hub, true, vec![])
            .with_capability_engine(engine);

        let args = serde_json::json!({"command": "rm -rf /"});
        let result = approval
            .request_tool("tc5".to_string(), "shell", &args, "rm -rf /".to_string(), "".to_string())
            .await;
        assert!(result.unwrap(), "yolo should bypass even block decisions");
    }

    #[tokio::test]
    async fn test_approval_request_times_out_when_unresolved() {
        let hub = RootWireHub::new();
        let approval = ApprovalRuntime::new(hub, false, vec![]);
        // No capability engine, no yolo, no auto-approve → falls back to prompt
        // which broadcasts and waits for resolution. Without a resolver, it hangs.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            approval.request_tool(
                "tc-timeout".to_string(),
                "shell",
                &serde_json::json!({"command": "echo hi"}),
                "Run shell".to_string(),
                "echo hi".to_string(),
            )
        ).await;
        assert!(result.is_err(), "Expected timeout since no one resolved the approval");
    }

    #[test]
    fn test_router_selects_highest_priority_available() {
        let hub = RootWireHub::new();
        let mut router = ApprovalRouter::new();

        // Background is always available
        router.register(Box::new(BackgroundSink::new(hub.clone())));

        // Wire not connected yet
        let wire = WireSink::new(hub.clone());
        wire.set_connected(false);
        router.register(Box::new(wire));

        // Shell is interactive
        let shell = ShellSink::new(hub.clone());
        shell.set_interactive(true);
        router.register(Box::new(shell));

        let selected = router.select_sink().unwrap();
        assert_eq!(selected.name(), "shell", "Shell should be selected when interactive");
    }

    #[test]
    fn test_router_falls_back_when_shell_unavailable() {
        let hub = RootWireHub::new();
        let mut router = ApprovalRouter::new();

        // Background always available
        router.register(Box::new(BackgroundSink::new(hub.clone())));

        // Wire connected
        let wire = WireSink::new(hub.clone());
        wire.set_connected(true);
        router.register(Box::new(wire));

        // Shell NOT interactive
        let shell = ShellSink::new(hub.clone());
        shell.set_interactive(false);
        router.register(Box::new(shell));

        let selected = router.select_sink().unwrap();
        assert_eq!(selected.name(), "wire", "Wire should be selected when connected and shell unavailable");
    }

    #[test]
    fn test_router_falls_back_to_background() {
        let hub = RootWireHub::new();
        let mut router = ApprovalRouter::new();

        // Background always available
        router.register(Box::new(BackgroundSink::new(hub.clone())));

        // Wire not connected
        let wire = WireSink::new(hub.clone());
        wire.set_connected(false);
        router.register(Box::new(wire));

        // Shell NOT interactive
        let shell = ShellSink::new(hub.clone());
        shell.set_interactive(false);
        router.register(Box::new(shell));

        let selected = router.select_sink().unwrap();
        assert_eq!(selected.name(), "background", "Background should be selected when shell and wire unavailable");
    }

    #[test]
    fn test_router_empty_when_no_sinks_available() {
        let hub = RootWireHub::new();
        let mut router = ApprovalRouter::new();

        let shell = ShellSink::new(hub.clone());
        shell.set_interactive(false);
        let wire = WireSink::new(hub.clone());
        wire.set_connected(false);

        router.register(Box::new(shell));
        router.register(Box::new(wire));

        assert!(router.select_sink().is_none(), "No sinks should be available");
    }

    #[test]
    fn test_sink_priority_ordering() {
        let hub = RootWireHub::new();
        let mut router = ApprovalRouter::new();

        // Register in reverse priority order
        router.register(Box::new(BackgroundSink::new(hub.clone())));
        let wire = WireSink::new(hub.clone());
        wire.set_connected(true);
        router.register(Box::new(wire));
        router.register(Box::new(ShellSink::new(hub.clone())));

        // All three are available; highest priority should be selected
        let selected = router.select_sink().unwrap();
        assert_eq!(selected.name(), "shell");
        assert_eq!(selected.priority(), 100);
    }

    #[tokio::test]
    async fn test_headless_ide_mode_selects_wire_sink() {
        let hub = RootWireHub::new();
        let approval = ApprovalRuntime::new(hub.clone(), false, vec![]);
        assert_eq!(approval.selected_sink_name().as_deref(), Some("shell"));
        approval.set_headless_ide_mode();
        assert_eq!(approval.selected_sink_name().as_deref(), Some("wire"));
    }

    #[tokio::test]
    async fn test_approval_resolves_via_router_in_headless_mode() {
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), false, vec![]));
        approval.set_headless_ide_mode();
        assert_eq!(approval.selected_sink_name().as_deref(), Some("wire"));

        let approval_resolve = approval.clone();
        tokio::spawn(async move {
            while let Ok(envelope) = rx.recv().await {
                if let WireEvent::ApprovalRequest { id, .. } = envelope.event {
                    let _ = approval_resolve.resolve(id, true, None).await;
                    break;
                }
            }
        });

        let result = approval
            .request_tool(
                "tc-headless".to_string(),
                "shell",
                &serde_json::json!({"command": "echo hi"}),
                "echo".to_string(),
                "".to_string(),
            )
            .await;
        assert!(result.unwrap());
    }
}
