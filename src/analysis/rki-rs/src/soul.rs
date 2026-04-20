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
            crate::user_input::resolve_supports_vision_for_model(&*cfg, &cfg.default_model)
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
        let result = orchestrator
            .execute_turn(
                &self.agent,
                self.context.clone(),
                self.llm.clone(),
                &self.runtime,
                turn,
                hub,
                token,
            )
            .await;

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
}
