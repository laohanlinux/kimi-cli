//! Core agent loop (`KimiSoul`) and D-Mail time-travel messaging.
//!
//! `KimiSoul::run` executes a single turn: slash-command interception,
//! orchestrator dispatch, wire event streaming, and result materialisation.

pub mod denwa_renji;

use crate::agent::Agent;
use crate::context::Context;
use crate::llm::ChatProvider;
use crate::message::Message;
use crate::orchestrator::TurnResult;
use crate::runtime::Runtime;
use crate::slash::SlashOutcome;
use crate::wire::{RootWireHub, WireEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct BackToTheFuture {
    pub checkpoint_id: u64,
    pub messages: Vec<Message>,
}

impl std::fmt::Display for BackToTheFuture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BackToTheFuture(checkpoint_id={})", self.checkpoint_id)
    }
}

impl std::error::Error for BackToTheFuture {}

pub struct KimiSoul {
    agent: Agent,
    context: Arc<Mutex<Context>>,
    llm: Arc<dyn ChatProvider>,
    runtime: Runtime,
    stop_hook_active: AtomicBool,
}

impl KimiSoul {
    pub fn new(
        agent: Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: Runtime,
    ) -> Self {
        Self {
            agent,
            context,
            llm,
            runtime,
            stop_hook_active: AtomicBool::new(false),
        }
    }

    pub async fn run<I: Into<crate::turn_input::TurnInput>>(
        &self,
        turn: I,
        hub: &RootWireHub,
    ) -> anyhow::Result<TurnResult> {
        let turn: crate::turn_input::TurnInput = turn.into();
        // §8.4: drop stale notification claims so the LLM consumer can reclaim after restarts / crashes.
        const STALE_NOTIFICATION_CLAIM_MS: i64 = 300_000;
        let _ = self
            .runtime
            .notifications
            .recover_stale_claims(STALE_NOTIFICATION_CLAIM_MS)
            .await;

        // Python `KimiSoul`: `ack_ids("llm", extract_notification_ids(context.history))` for restored sessions.
        {
            let ids = {
                let ctx = self.context.lock().await;
                crate::notification::llm::extract_notification_ids_from_history(&ctx.history())
            };
            for id in ids {
                let _ = self.runtime.notifications.ack("llm", &id).await;
            }
        }

        // Slash command interception (§1.2 step 15) — before turn validation so `/plan` etc. stay valid.
        let slash_src = turn.text_for_slash();
        if let Some(cmd) = crate::slash::SlashCommand::parse(&slash_src)
            && let Some(result) = self.runtime.slash_registry.handle(&cmd, &self.runtime)
        {
            hub.broadcast(WireEvent::TurnBegin {
                user_input: crate::wire::UserInput::text_only(slash_src.clone()),
            });
            let outcome = result?;
            let text = match outcome {
                SlashOutcome::Message(s) => s,
                SlashOutcome::EnterPlan => {
                    self.runtime.enter_plan_mode().await;
                    "Entered plan mode.".to_string()
                }
                SlashOutcome::ExitPlan => {
                    self.runtime.exit_plan_mode().await;
                    "Exited plan mode.".to_string()
                }
                SlashOutcome::EnterRalph { max_iterations } => {
                    self.runtime.enter_ralph_mode(max_iterations).await;
                    format!("Entered Ralph mode with max {max_iterations} iterations.")
                }
                SlashOutcome::ToggleYolo => {
                    let next = !self.runtime.is_yolo();
                    self.runtime.set_yolo(next);
                    if next {
                        "YOLO on: approvals bypassed for this session.".to_string()
                    } else {
                        "YOLO off: interactive approvals enabled.".to_string()
                    }
                }
            };
            hub.broadcast(WireEvent::TextPart { text: text.clone() });
            hub.broadcast(WireEvent::TurnEnd);
            return Ok(TurnResult {
                stop_reason: format!("slash:{}", cmd.name),
            });
        }

        // §1.2 L16: reject empty or image-like input for text-only models (after slash dispatch).
        let supports_vision = {
            let cfg = self.runtime.config.read().await;
            crate::user_input::resolve_supports_vision_for_model(&cfg, &cfg.default_model)
        };
        if let Err(rej) =
            crate::user_input::validate_turn_content_parts(&turn.parts, supports_vision)
        {
            let stop_reason = match rej {
                crate::user_input::UserInputRejection::Empty => "validation:empty",
                crate::user_input::UserInputRejection::VisionContentNotSupported => {
                    "validation:vision_not_supported"
                }
            };
            hub.broadcast(WireEvent::TextPart {
                text: rej.to_string(),
            });
            hub.broadcast(WireEvent::TurnEnd);
            return Ok(TurnResult {
                stop_reason: stop_reason.to_string(),
            });
        }

        hub.broadcast(WireEvent::TurnBegin {
            user_input: crate::wire::UserInput::from_turn(&turn),
        });

        let token = self
            .runtime
            .context_token_for_turn(uuid::Uuid::new_v4().to_string());
        let orchestrator = self.runtime.get_orchestrator().await;
        let mut result = orchestrator
            .execute_turn(
                &self.agent,
                self.context.clone(),
                self.llm.clone(),
                &self.runtime,
                turn,
                hub,
                token.clone(),
            )
            .await;

        // --- Stop hook (max 1 re-trigger to prevent infinite loop) ---
        if result.is_ok() && !self.stop_hook_active.load(Ordering::SeqCst) {
            let stop_payload = serde_json::json!({
                "session_id": self.runtime.session.id,
                "cwd": std::env::current_dir().unwrap_or_default().to_string_lossy(),
                "stop_hook_active": false,
            });
            match self
                .runtime
                .hooks
                .run(crate::hooks::HookStage::Stop, "stop", &stop_payload)
                .await
            {
                Ok(crate::hooks::SideEffectResult {
                    decision: crate::hooks::EffectDecision::Block { reason },
                    ..
                }) if !reason.is_empty() => {
                    self.stop_hook_active.store(true, Ordering::SeqCst);
                    let re_trigger_turn = crate::turn_input::TurnInput::text(reason);
                    let re_trigger_token = self
                        .runtime
                        .context_token_for_turn(uuid::Uuid::new_v4().to_string());
                    result = orchestrator
                        .execute_turn(
                            &self.agent,
                            self.context.clone(),
                            self.llm.clone(),
                            &self.runtime,
                            re_trigger_turn,
                            hub,
                            re_trigger_token,
                        )
                        .await;
                    self.stop_hook_active.store(false, Ordering::SeqCst);
                }
                _ => {}
            }
        }

        hub.broadcast(WireEvent::TurnEnd);
        // Auto-set session title after first real turn (§1.2 step 34)
        let _ = self.runtime.session.auto_title(&self.runtime.store);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentSpec};
    use crate::approval::ApprovalRuntime;
    use crate::config::Config;
    use crate::llm::EchoProvider;
    use crate::message::ContentPart;
    use crate::runtime::Runtime;
    use crate::session::Session;
    use crate::store::Store;
    use crate::turn_input::TurnInput;
    use crate::wire::RootWireHub;
    use std::sync::Arc;

    fn test_soul() -> KimiSoul {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let runtime = Runtime::new(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
        );
        let context = Arc::new(Mutex::new(
            futures::executor::block_on(Context::load(&runtime.store, &runtime.session.id))
                .unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "sys".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "sys".to_string(),
        };
        KimiSoul::new(agent, context, Arc::new(EchoProvider), runtime)
    }

    #[test]
    fn test_back_to_the_future_display() {
        let btf = BackToTheFuture {
            checkpoint_id: 42,
            messages: vec![],
        };
        assert_eq!(btf.to_string(), "BackToTheFuture(checkpoint_id=42)");
    }

    #[test]
    fn test_back_to_the_future_implements_error() {
        let btf = BackToTheFuture {
            checkpoint_id: 7,
            messages: vec![],
        };
        // Verify it can be used as a dyn Error
        let err: &dyn std::error::Error = &btf;
        assert!(err.to_string().contains("7"));
    }

    #[tokio::test]
    async fn test_soul_run_slash_command() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let result = soul.run("/exit", &hub).await.unwrap();
        assert_eq!(result.stop_reason, "slash:exit");
    }

    #[tokio::test]
    async fn test_soul_run_plan_slash_command() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let result = soul.run("/plan", &hub).await.unwrap();
        assert_eq!(result.stop_reason, "slash:plan");
        assert!(soul.runtime.is_plan_mode().await);
    }

    #[tokio::test]
    async fn test_soul_run_yolo_slash_command() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let initial = soul.runtime.is_yolo();
        let result = soul.run("/yolo", &hub).await.unwrap();
        assert_eq!(result.stop_reason, "slash:yolo");
        assert_eq!(soul.runtime.is_yolo(), !initial);
    }

    #[tokio::test]
    async fn test_soul_run_ralph_slash_command() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let result = soul.run("/ralph 7", &hub).await.unwrap();
        assert_eq!(result.stop_reason, "slash:ralph");
        assert!(matches!(
            soul.runtime.get_orchestrator().await.name(),
            "ralph"
        ));
    }

    #[tokio::test]
    async fn test_soul_auto_title_after_turn() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let result = soul.run("how do I refactor this code?", &hub).await;
        assert!(result.is_ok());

        // Verify title was auto-set
        let state = soul
            .runtime
            .store
            .get_state(&soul.runtime.session.id)
            .unwrap();
        if let Some(s) = state {
            let data: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert_eq!(data["title"].as_str(), Some("how do I refactor this code?"));
        }
    }

    #[tokio::test]
    async fn test_soul_rejects_empty_user_turn() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let r = soul.run("  \t  ", &hub).await.unwrap();
        assert_eq!(r.stop_reason, "validation:empty");
    }

    #[tokio::test]
    async fn test_soul_rejects_image_like_input_when_text_only() {
        let soul = test_soul();
        {
            let mut c = soul.runtime.config.write().await;
            c.supports_vision = false;
        }
        let hub = RootWireHub::new();
        let r = soul
            .run("see ![cap](http://example.com/a.png)", &hub)
            .await
            .unwrap();
        assert_eq!(r.stop_reason, "validation:vision_not_supported");
    }

    /// Default `echo` model uses vision hint off → image-like markdown rejected without toggling config.
    #[tokio::test]
    async fn test_soul_echo_model_rejects_markdown_image_by_default() {
        let soul = test_soul();
        let hub = RootWireHub::new();
        let r = soul
            .run("![](https://example.com/x.png)", &hub)
            .await
            .unwrap();
        assert_eq!(r.stop_reason, "validation:vision_not_supported");
    }

    #[tokio::test]
    async fn test_soul_accepts_multimodal_parts_when_vision_on() {
        let soul = test_soul();
        {
            let mut c = soul.runtime.config.write().await;
            c.supports_vision = true;
            c.ignore_vision_model_hint = true;
        }
        let hub = RootWireHub::new();
        let turn = TurnInput::new(vec![
            ContentPart::Text {
                text: "describe".into(),
            },
            ContentPart::ImageUrl {
                url: "https://example.com/z.png".into(),
            },
        ]);
        let r = soul.run(turn, &hub).await.unwrap();
        assert_ne!(r.stop_reason, "validation:vision_not_supported");
        assert_ne!(r.stop_reason, "validation:empty");
    }

    #[tokio::test]
    async fn test_stop_hook_re_trigger_max_once() {
        use crate::hooks::{EffectDecision, HookStage, SideEffect, SideEffectResult};
        use async_trait::async_trait;
        use serde_json::Value;

        struct StopReTrigger;
        #[async_trait]
        impl SideEffect for StopReTrigger {
            fn name(&self) -> &str { "stop_retrigger" }
            fn stage(&self) -> HookStage { HookStage::Stop }
            fn is_critical(&self) -> bool { false }
            async fn execute(&self, _event: &str, _payload: &Value) -> anyhow::Result<SideEffectResult> {
                Ok(SideEffectResult {
                    decision: EffectDecision::Block { reason: "follow-up".to_string() },
                    message: None,
                })
            }
        }

        let soul = test_soul();
        soul.runtime.hooks.register(std::sync::Arc::new(StopReTrigger));
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let result = soul.run("hello", &hub).await.unwrap();
        // EchoProvider returns no_tool_calls; stop hook re-triggers once with "follow-up"
        // The re-triggered turn also returns no_tool_calls.
        assert_eq!(result.stop_reason, "no_tool_calls");
        // Verify only one TurnBegin (the original) and one TurnEnd
        let mut events = vec![];
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let turn_begins = events.iter().filter(|e| matches!(e, WireEvent::TurnBegin { .. })).count();
        let turn_ends = events.iter().filter(|e| matches!(e, WireEvent::TurnEnd)).count();
        assert_eq!(turn_begins, 1, "Only original turn should broadcast TurnBegin");
        assert_eq!(turn_ends, 1, "Only one TurnEnd after stop hook re-trigger");
    }

    #[tokio::test]
    async fn test_stop_hook_no_re_trigger_when_already_active() {
        use crate::hooks::{EffectDecision, HookStage, SideEffect, SideEffectResult};
        use async_trait::async_trait;
        use serde_json::Value;

        struct StopReTrigger;
        #[async_trait]
        impl SideEffect for StopReTrigger {
            fn name(&self) -> &str { "stop_retrigger" }
            fn stage(&self) -> HookStage { HookStage::Stop }
            fn is_critical(&self) -> bool { false }
            async fn execute(&self, _event: &str, _payload: &Value) -> anyhow::Result<SideEffectResult> {
                Ok(SideEffectResult {
                    decision: EffectDecision::Block { reason: "follow-up".to_string() },
                    message: None,
                })
            }
        }

        let soul = test_soul();
        soul.runtime.hooks.register(std::sync::Arc::new(StopReTrigger));
        // Manually set stop_hook_active to simulate being inside a re-triggered turn
        soul.stop_hook_active.store(true, Ordering::SeqCst);
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let result = soul.run("hello", &hub).await.unwrap();
        assert_eq!(result.stop_reason, "no_tool_calls");
        // No re-trigger should happen
        let mut events = vec![];
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let turn_begins = events.iter().filter(|e| matches!(e, WireEvent::TurnBegin { .. })).count();
        assert_eq!(turn_begins, 1);
    }
}
