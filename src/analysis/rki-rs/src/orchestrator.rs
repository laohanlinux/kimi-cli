//! Turn orchestrators: swappable agent-loop strategies.
//!
//! - `ReActOrchestrator` — standard reasoning + action loop
//! - `PlanModeOrchestrator` — read-only research mode
//! - `RalphOrchestrator` — automated iteration with decision gate

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::agent::Agent;
use crate::context::Context;
use crate::feature_flags::{ExperimentalFeature, FeatureFlags};
use crate::hooks::HookStage;
use crate::llm::{self, ChatProvider};
use crate::message::{ContentPart, Message, UserMessage, merge_adjacent_user_messages};
use crate::notification::NotificationManager;
use crate::runtime::Runtime;
use crate::soul::BackToTheFuture;
use crate::token::ContextToken;
use crate::tools::ToolContext;
use crate::turn_input::TurnInput;
use crate::wire::{RootWireHub, WireEvent};

#[derive(Debug)]
pub struct TurnResult {
    pub stop_reason: String,
}

/// §8.4: fan out notification tail to the wire hub for offset consumer `"wire"`.
pub(crate) async fn deliver_wire_offset_tail(
    notifications: &NotificationManager,
    hub: &RootWireHub,
) -> anyhow::Result<()> {
    let wire_tail = notifications
        .read_since_persisted_offset("wire", 50)
        .await?;
    let mut last_id: Option<String> = None;
    for notif in &wire_tail {
        hub.broadcast(WireEvent::Notification {
            id: notif.dedupe_key.clone().unwrap_or_default(),
            category: notif.category.clone(),
            kind: notif.kind.clone(),
            source_kind: notif.source_kind.clone(),
            source_id: notif.source_id.clone(),
            title: notif.title.clone(),
            body: notif.body.clone(),
            severity: notif.severity.clone(),
            created_at: notif.created_at.unwrap_or(0.0),
            payload: notif.payload.clone(),
        });
        last_id = notif.dedupe_key.clone();
    }
    if let Some(ref lid) = last_id {
        notifications.advance_consumer_offset("wire", lid).await?;
    }
    Ok(())
}

/// §7.2: when `FunctionTools` is enabled, annotate each tool JSON schema for providers that understand function-style tools.
pub(crate) fn apply_function_tool_schema_tags(
    features: &FeatureFlags,
    tools: &mut [serde_json::Value],
) {
    if !features.is_enabled(ExperimentalFeature::FunctionTools) {
        return;
    }
    for schema in tools.iter_mut() {
        if let Some(obj) = schema.as_object_mut() {
            obj.insert(
                "x-rki-tool-contract".to_string(),
                serde_json::json!("v1-function-tools"),
            );
        }
    }
}

/// Stateful loop protocol extracted from KimiSoul.
/// Orchestrators are composable and swappable mid-session.
#[async_trait]
pub trait TurnOrchestrator: Send + Sync {
    fn name(&self) -> &'static str;

    #[allow(clippy::too_many_arguments)]
    async fn execute_turn(
        &self,
        agent: &Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: &Runtime,
        turn: TurnInput,
        hub: &RootWireHub,
        token: ContextToken,
    ) -> anyhow::Result<TurnResult>;
}

/// Default ReAct orchestrator: step loop with compaction, D-Mail, tools.
pub struct ReActOrchestrator;

#[async_trait]
impl TurnOrchestrator for ReActOrchestrator {
    fn name(&self) -> &'static str {
        "react"
    }

    async fn execute_turn(
        &self,
        agent: &Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: &Runtime,
        turn: TurnInput,
        hub: &RootWireHub,
        token: ContextToken,
    ) -> anyhow::Result<TurnResult> {
        let mut ctx = context.lock().await;
        let checkpoint_id = ctx.write_checkpoint().await?;
        let user_msg = Message::User(UserMessage::from_parts(turn.parts.clone()));
        ctx.append(user_msg).await?;
        drop(ctx);

        let stop_reason =
            Self::_agent_loop(agent, context, llm, runtime, hub, checkpoint_id, token).await?;
        Ok(TurnResult { stop_reason })
    }
}

/// Outcome of a single step in the ReAct loop.
enum StepResult {
    /// Tool calls were made; continue to next step.
    Continue,
    /// No tool calls from LLM; turn ends.
    NoToolCalls,
    /// One or more tools were rejected; turn ends.
    ToolRejected,
}

impl ReActOrchestrator {
    async fn _agent_loop(
        agent: &Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: &Runtime,
        hub: &RootWireHub,
        checkpoint_id: u64,
        token: ContextToken,
    ) -> anyhow::Result<String> {
        // Deferred MCP loading (§1.2 L19): start once if servers configured.
        if !runtime.mcp_servers.is_empty()
            && !runtime
                .mcp_loading_started
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let rt = runtime.clone();
            tokio::spawn(async move {
                rt.mcp_status.write().await.clone_from(&"loading".to_string());
                let mut connected = 0usize;
                for (name, cfg) in &rt.mcp_servers {
                    let client = Arc::new(crate::mcp::MCPClient::new(
                        name.clone(),
                        std::iter::once(cfg.command.clone())
                            .chain(cfg.args.clone())
                            .collect(),
                    ));
                    match client.list_tools().await {
                        Ok(tools) => {
                            let mut ts = rt.toolset.write().await;
                            for t in tools {
                                ts.register(Box::new(crate::mcp::MCPTool::new(
                                    t.name,
                                    t.description,
                                    t.input_schema,
                                    client.clone(),
                                )));
                            }
                            connected += 1;
                        }
                        Err(e) => {
                            tracing::warn!("MCP server '{}' failed to load tools: {}", name, e);
                        }
                    }
                }
                let status = if connected == rt.mcp_servers.len() {
                    "connected"
                } else if connected > 0 {
                    "partial"
                } else {
                    "failed"
                };
                *rt.mcp_status.write().await = status.to_string();
            });
        }

        let max_steps = runtime
            .config
            .read()
            .await
            .max_steps_per_turn
            .unwrap_or(100);
        for step in 1..=max_steps {
            hub.broadcast(WireEvent::StepBegin { n: step });
            let step_token = token.child_step(format!("{}", step));
            match Self::_step(
                agent,
                context.clone(),
                llm.clone(),
                runtime,
                hub,
                checkpoint_id,
                step_token,
            )
            .await
            {
                Ok(StepResult::NoToolCalls) => {
                    return Ok("no_tool_calls".to_string());
                }
                Ok(StepResult::ToolRejected) => {
                    return Ok("tool_rejected".to_string());
                }
                Ok(StepResult::Continue) => {}
                Err(e) => {
                    if let Some(bttf) = e.downcast_ref::<BackToTheFuture>() {
                        let mut ctx = context.lock().await;
                        ctx.revert_to(bttf.checkpoint_id).await?;
                        for msg in &bttf.messages {
                            ctx.append(msg.clone()).await?;
                        }
                        drop(ctx);
                        hub.broadcast(WireEvent::StepInterrupted {
                            reason: "dmail_revert".to_string(),
                        });
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Ok("max_steps".to_string())
    }

    async fn _step(
        agent: &Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: &Runtime,
        hub: &RootWireHub,
        checkpoint_id: u64,
        token: ContextToken,
    ) -> anyhow::Result<StepResult> {
        let config = runtime.config.read().await;
        let max_context = config.max_context_size.unwrap_or(128_000);
        let threshold_percent = config.compaction_threshold_percent;
        let threshold_absolute = config.compaction_threshold_absolute;
        let min_messages = config.compaction_min_messages;
        drop(config);

        // Sync compaction policy with runtime config
        {
            let mut ctx = context.lock().await;
            ctx.set_compaction_config(min_messages);
        }

        let token_count = {
            let ctx = context.lock().await;
            ctx.token_count()
        };

        // Auto-compaction check (config-driven thresholds)
        if token_count >= (max_context as f64 * threshold_percent) as usize
            || token_count + threshold_absolute >= max_context
        {
            hub.broadcast(WireEvent::CompactionBegin);
            let mut ctx = context.lock().await;
            ctx.compact(Some(llm.clone())).await?;
            drop(ctx);
            hub.broadcast(WireEvent::CompactionEnd);
        }

        let _ = deliver_wire_offset_tail(&runtime.notifications, hub).await;

        // Notification delivery (exactly-once via claim+ack)
        let notifications = runtime.notifications.claim("llm").await;
        if !notifications.is_empty() {
            let mut ctx = context.lock().await;
            for notif in &notifications {
                let text = crate::notification::llm::build_notification_message_for_llm(
                    notif,
                    Some(&runtime.bg_manager),
                )
                .await;
                ctx.append(Message::User(UserMessage::text(text))).await?;
            }
            drop(ctx);
            // Ack after successful delivery to context
            for notif in &notifications {
                if let Some(ref id) = notif.dedupe_key {
                    runtime.notifications.ack("llm", id).await.ok();
                }
            }
        }

        // Dynamic injection: plan mode, YOLO, etc.
        let injections = runtime.injection.collect(runtime).await;
        if !injections.is_empty() {
            let mut ctx = context.lock().await;
            for msg in injections {
                ctx.append(msg).await?;
            }
            drop(ctx);
        }

        // Build LLM history after notifications + injections (§1.2 Phase D–E ordering).
        let history = {
            let ctx = context.lock().await;
            let recall_query = ctx
                .history()
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::User(u) => Some(u.flatten_for_recall()),
                    _ => None,
                })
                .unwrap_or_default();
            let h = if runtime
                .features
                .is_enabled(ExperimentalFeature::MemoryHierarchy)
            {
                ctx.history_with_recall(&recall_query, 5)
            } else {
                ctx.history()
            };
            merge_adjacent_user_messages(h)
        };
        // L10: context _system_prompt overrides agent system_prompt if present
        let system_prompt = {
            let ctx_sp = history
                .iter()
                .find_map(|m| match m {
                    Message::SystemPrompt { content } => Some(content.clone()),
                    _ => None,
                });
            Some(ctx_sp.unwrap_or_else(|| agent.system_prompt.clone()))
        };
        let mut tools = runtime.toolset.read().await.schemas();
        apply_function_tool_schema_tags(&runtime.features, &mut tools);

        let retry_config = llm::RetryConfig::default();
        let llm_clone = llm.clone();
        let mut generation = llm::with_retry(&retry_config, move || {
            let llm = llm_clone.clone();
            let system_prompt = system_prompt.clone();
            let history = history.clone();
            let tools = tools.clone();
            async move { llm.generate(system_prompt, history, tools).await }
        })
        .await?;
        let pull_backpressure = runtime
            .features
            .is_enabled(ExperimentalFeature::PullGeneration);
        let mut assistant_parts: Vec<ContentPart> = Vec::new();
        while let Some(chunk) = generation.next_chunk().await {
            assistant_parts.push(chunk.clone());
            let event = match chunk {
                ContentPart::Text { text } => WireEvent::TextPart { text },
                ContentPart::Think { text } => WireEvent::ThinkPart { text },
                ContentPart::ImageUrl { url } => WireEvent::ImageUrlPart { url },
                ContentPart::AudioUrl { url } => WireEvent::AudioUrlPart { url },
                ContentPart::VideoUrl { url } => WireEvent::VideoUrlPart { url },
            };
            hub.broadcast(event);
            if pull_backpressure {
                tokio::task::yield_now().await;
            }
        }

        let tool_calls = generation.tool_calls().await;

        // Append assistant message to context (§1.2 L27)
        let assistant_text: String = assistant_parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let assistant_msg = Message::Assistant {
            content: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls.clone())
            },
        };
        {
            let mut ctx = context.lock().await;
            ctx.append(assistant_msg).await?;
        }

        let has_tool_calls = !tool_calls.is_empty();
        let mut any_rejected = false;

        if has_tool_calls {
            /// Outcome of a single concurrent tool execution.
            struct ToolCallOutcome {
                context_entry: Message,
                rejected_without_feedback: bool,
                audit_payload: serde_json::Value,
            }

            let mut futures = Vec::new();
            for tc in &tool_calls {
                hub.broadcast(WireEvent::ToolCall {
                    id: tc.id.clone(),
                    function: tc.function.clone(),
                });

                let tc = tc.clone();
                let runtime = runtime.clone();
                let hub = hub.clone();
                let token = token.child_tool_call(tc.id.clone());

                futures.push(async move {
                    let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(serde_json::Value::Null);

                    // Pre-validate hook
                    let validate_payload = serde_json::json!({
                        "tool": tc.function.name,
                        "args": &args,
                        "tool_call_id": &tc.id,
                    });
                    let validate_result = runtime
                        .hooks
                        .run(HookStage::PreValidate, "tool_call", &validate_payload)
                        .await?;
                    if let crate::hooks::EffectDecision::Block { reason } = validate_result.decision
                    {
                        hub.broadcast(WireEvent::ToolResult {
                            tool_call_id: tc.id.clone(),
                            output: format!("Blocked by hook: {}", reason),
                            is_error: true,
                            elapsed_ms: None,
                        });
                        return Ok::<ToolCallOutcome, anyhow::Error>(ToolCallOutcome {
                            context_entry: Message::ToolEvent(crate::message::ToolEvent {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.function.name.clone(),
                                status: crate::message::ToolStatus::Failed,
                                content: vec![crate::message::ContentBlock::Text {
                                    text: format!("Blocked by hook: {}", reason),
                                }],
                                metrics: None,
                                elapsed_ms: None,
                            }),
                            rejected_without_feedback: true,
                            audit_payload: serde_json::json!({
                                "tool": tc.function.name,
                                "args": &args,
                                "tool_call_id": &tc.id,
                                "result": "error",
                            }),
                        });
                    }

                    // Pre-execute hook
                    let pre_payload = serde_json::json!({
                        "tool": tc.function.name,
                        "args": &args,
                        "tool_call_id": &tc.id,
                    });
                    let pre_exec = runtime
                        .hooks
                        .run(HookStage::PreExecute, "tool_call", &pre_payload)
                        .await?;
                    if runtime
                        .features
                        .is_enabled(ExperimentalFeature::StructuredEffects)
                        && let crate::hooks::EffectDecision::Block { reason } = &pre_exec.decision {
                            hub.broadcast(WireEvent::ToolResult {
                                tool_call_id: tc.id.clone(),
                                output: format!("Blocked by hook: {}", reason),
                                is_error: true,
                                elapsed_ms: None,
                            });
                            return Ok::<ToolCallOutcome, anyhow::Error>(ToolCallOutcome {
                                context_entry: Message::ToolEvent(crate::message::ToolEvent {
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.function.name.clone(),
                                    status: crate::message::ToolStatus::Failed,
                                    content: vec![crate::message::ContentBlock::Text {
                                        text: format!("Blocked by hook: {}", reason),
                                    }],
                                    metrics: None,
                                    elapsed_ms: None,
                                }),
                                rejected_without_feedback: true,
                                audit_payload: serde_json::json!({
                                    "tool": tc.function.name,
                                    "args": &args,
                                    "tool_call_id": &tc.id,
                                    "result": "error",
                                }),
                            });
                        }

                    let toolset = runtime.toolset.read().await;
                    let tool_ctx = ToolContext {
                        runtime: runtime.clone(),
                        hub: Some(hub.clone()),
                        token,
                    };
                    let start = std::time::Instant::now();
                    let tool_result = toolset
                        .handle(&tc.function.name, args.clone(), &tool_ctx)
                        .await;
                    let elapsed = start.elapsed().as_millis() as u64;
                    drop(toolset);

                    match tool_result {
                        Ok(output) => {
                            let display_text =
                                crate::message::content_to_string(&output.result.content);
                            hub.broadcast(WireEvent::ToolResult {
                                tool_call_id: tc.id.clone(),
                                output: display_text,
                                is_error: false,
                                elapsed_ms: Some(elapsed),
                            });

                            let post_payload = serde_json::json!({
                                "tool": tc.function.name,
                                "args": &args,
                                "tool_call_id": &tc.id,
                                "result": "success",
                            });
                            let _ = runtime
                                .hooks
                                .run(HookStage::PostExecute, "tool_call", &post_payload)
                                .await?;

                            Ok::<ToolCallOutcome, anyhow::Error>(ToolCallOutcome {
                                context_entry: Message::ToolEvent(crate::message::ToolEvent {
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.function.name.clone(),
                                    status: crate::message::ToolStatus::Completed,
                                    content: output.result.content,
                                    metrics: Some(output.metrics),
                                    elapsed_ms: Some(elapsed),
                                }),
                                rejected_without_feedback: false,
                                audit_payload: serde_json::json!({
                                    "tool": tc.function.name,
                                    "args": &args,
                                    "tool_call_id": &tc.id,
                                    "result": "success",
                                }),
                            })
                        }
                        Err(e) => {
                            let is_rejected =
                                e.downcast_ref::<crate::tools::ToolRejected>().is_some();
                            let rejected_without_feedback = if is_rejected {
                                let has_feedback = e
                                    .downcast_ref::<crate::tools::ToolRejected>()
                                    .map(|tr| tr.has_feedback)
                                    .unwrap_or(false);
                                !has_feedback
                            } else {
                                false
                            };
                            hub.broadcast(WireEvent::ToolResult {
                                tool_call_id: tc.id.clone(),
                                output: e.to_string(),
                                is_error: true,
                                elapsed_ms: Some(elapsed),
                            });

                            let post_payload = serde_json::json!({
                                "tool": tc.function.name,
                                "args": &args,
                                "tool_call_id": &tc.id,
                                "result": "error",
                                "error": e.to_string(),
                            });
                            let _ = runtime
                                .hooks
                                .run(HookStage::PostExecuteFailure, "tool_call", &post_payload)
                                .await?;

                            Ok::<ToolCallOutcome, anyhow::Error>(ToolCallOutcome {
                                context_entry: Message::ToolEvent(crate::message::ToolEvent {
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.function.name.clone(),
                                    status: crate::message::ToolStatus::Failed,
                                    content: vec![crate::message::ContentBlock::Text {
                                        text: e.to_string(),
                                    }],
                                    metrics: None,
                                    elapsed_ms: Some(elapsed),
                                }),
                                rejected_without_feedback,
                                audit_payload: serde_json::json!({
                                    "tool": tc.function.name,
                                    "args": &args,
                                    "tool_call_id": &tc.id,
                                    "result": "error",
                                }),
                            })
                        }
                    }
                });
            }

            let outcomes = futures::future::join_all(futures).await;
            for outcome in outcomes {
                let outcome = outcome?;
                let mut ctx = context.lock().await;
                ctx.append(outcome.context_entry).await?;
                drop(ctx);
                let _ = runtime
                    .hooks
                    .run(HookStage::Audit, "tool_call", &outcome.audit_payload)
                    .await?;
                if outcome.rejected_without_feedback {
                    any_rejected = true;
                }
            }
        }

        // Rejection handling: if any tool was rejected, end the turn (§1.2 L28).
        if any_rejected {
            return Ok(StepResult::ToolRejected);
        }

        // Emit status update after every step (plan_mode reflects runtime, e.g. after enter_plan_mode tool)
        let ctx = context.lock().await;
        let token_count = ctx.token_count();
        drop(ctx);
        let plan_mode = runtime.is_plan_mode().await;
        let mcp_status = runtime.mcp_status.read().await.clone();
        hub.broadcast(WireEvent::StatusUpdate {
            token_count,
            context_size: max_context,
            plan_mode,
            mcp_status,
        });

        // Steer consumption: inject queued user messages
        let steers = runtime.steer_queue.drain().await;
        if !steers.is_empty() {
            let mut ctx = context.lock().await;
            for steer in steers {
                ctx.append(Message::User(UserMessage::text(steer))).await?;
            }
            drop(ctx);
            hub.broadcast(WireEvent::SteerInput {
                content: "steer injected".to_string(),
            });
            return Ok(StepResult::Continue); // continue to next step
        }

        // D-Mail check
        if let Some((target_cp, messages)) = runtime.denwa_renji.claim().await {
            let effective_cp = if target_cp == 0 {
                checkpoint_id
            } else {
                target_cp
            };
            return Err(anyhow::Error::from(BackToTheFuture {
                checkpoint_id: effective_cp,
                messages,
            }));
        }

        if has_tool_calls {
            Ok(StepResult::Continue)
        } else {
            Ok(StepResult::NoToolCalls)
        }
    }
}

/// Plan mode orchestrator: read-only research, single step, no tools, no compaction.
pub struct PlanModeOrchestrator;

#[async_trait]
impl TurnOrchestrator for PlanModeOrchestrator {
    fn name(&self) -> &'static str {
        "plan"
    }

    async fn execute_turn(
        &self,
        agent: &Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: &Runtime,
        turn: TurnInput,
        hub: &RootWireHub,
        _token: ContextToken,
    ) -> anyhow::Result<TurnResult> {
        let mut ctx = context.lock().await;
        let _checkpoint_id = ctx.write_checkpoint().await?;
        let user_msg = Message::User(UserMessage::from_parts(turn.parts.clone()));
        ctx.append(user_msg).await?;
        let history = merge_adjacent_user_messages(ctx.history());
        // L10: context _system_prompt overrides agent system_prompt if present
        let base_prompt = history
            .iter()
            .find_map(|m| match m {
                Message::SystemPrompt { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| agent.system_prompt.clone());
        let system_prompt = Some(format!(
            "{}\n\n[PLAN MODE] You are in read-only research mode. Do not use tools. Think step by step and present a plan.",
            base_prompt
        ));
        drop(ctx);

        let _ = deliver_wire_offset_tail(&runtime.notifications, hub).await;

        hub.broadcast(WireEvent::StepBegin { n: 1 });
        hub.broadcast(WireEvent::PlanDisplay {
            content: "Plan mode active. Analyzing without tools...".to_string(),
            file_path: String::new(),
        });

        let mut generation = llm.generate(system_prompt, history, vec![]).await?;
        let pull_backpressure = runtime
            .features
            .is_enabled(ExperimentalFeature::PullGeneration);
        let mut assistant_parts: Vec<ContentPart> = Vec::new();
        while let Some(chunk) = generation.next_chunk().await {
            assistant_parts.push(chunk.clone());
            let event = match chunk {
                ContentPart::Text { text } => WireEvent::TextPart { text },
                ContentPart::Think { text } => WireEvent::ThinkPart { text },
                ContentPart::ImageUrl { url } => WireEvent::ImageUrlPart { url },
                ContentPart::AudioUrl { url } => WireEvent::AudioUrlPart { url },
                ContentPart::VideoUrl { url } => WireEvent::VideoUrlPart { url },
            };
            hub.broadcast(event);
            if pull_backpressure {
                tokio::task::yield_now().await;
            }
        }

        // Plan mode ignores tool calls — they shouldn't happen since we pass empty tools list
        let tool_calls = generation.tool_calls().await;
        if !tool_calls.is_empty() {
            hub.broadcast(WireEvent::StepInterrupted {
                reason: "plan_mode_ignores_tool_calls".to_string(),
            });
        }

        let assistant_text: String = assistant_parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let mut ctx = context.lock().await;
        ctx.append(Message::Assistant {
            content: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_calls: None,
        })
        .await?;
        ctx.write_checkpoint().await?;
        drop(ctx);

        // D-Mail check (plan mode can still receive time-travel messages)
        // We don't check denwa_renji here; plan mode is intentionally simple.

        Ok(TurnResult {
            stop_reason: "plan_mode_complete".to_string(),
        })
    }
}

/// Ralph orchestrator: automated iteration with decision gate (§5.4 deviation).
/// Wraps ReActOrchestrator in a loop. After each turn, asks the LLM whether
/// to continue or stop. Max iterations configurable.
pub struct RalphOrchestrator {
    max_iterations: usize,
    inner: ReActOrchestrator,
}

impl RalphOrchestrator {
    pub fn new(max_iterations: usize) -> Self {
        Self {
            max_iterations,
            inner: ReActOrchestrator,
        }
    }

    /// Decision gate: ask the LLM whether to continue iterating or stop.
    /// Returns true if the LLM says to stop.
    async fn should_stop(
        &self,
        llm: Arc<dyn ChatProvider>,
        stop_reason: &str,
        iteration: usize,
        hub: &RootWireHub,
    ) -> anyhow::Result<bool> {
        let decision_prompt = format!(
            "[RALPH DECISION] You are an automated iteration controller. \
The agent just completed iteration {} with stop_reason='{}'. \
Should the agent continue working or stop? \
Respond with exactly one word: STOP or CONTINUE.",
            iteration, stop_reason
        );

        let mut generation = llm.generate(Some(decision_prompt), vec![], vec![]).await?;

        let mut response = String::new();
        while let Some(chunk) = generation.next_chunk().await {
            if let ContentPart::Text { text } = chunk {
                response.push_str(&text);
            }
        }

        let trimmed = response.trim().to_uppercase();
        let stop = trimmed.contains("STOP");

        hub.broadcast(WireEvent::TextPart {
            text: format!(
                "[Ralph decision: {}]\n",
                if stop { "STOP" } else { "CONTINUE" }
            ),
        });

        Ok(stop)
    }
}

#[async_trait]
impl TurnOrchestrator for RalphOrchestrator {
    fn name(&self) -> &'static str {
        "ralph"
    }

    async fn execute_turn(
        &self,
        agent: &Agent,
        context: Arc<Mutex<Context>>,
        llm: Arc<dyn ChatProvider>,
        runtime: &Runtime,
        turn: TurnInput,
        hub: &RootWireHub,
        token: ContextToken,
    ) -> anyhow::Result<TurnResult> {
        hub.broadcast(WireEvent::TextPart {
            text: "[Ralph mode: starting automated iteration]\n".to_string(),
        });

        for iteration in 1..=self.max_iterations {
            hub.broadcast(WireEvent::TextPart {
                text: format!("\n--- Ralph iteration {} ---\n", iteration),
            });

            let turn_token = token.child_step(format!("ralph-{}", iteration));
            let turn_in = if iteration == 1 {
                turn.clone()
            } else {
                TurnInput::text("continue")
            };
            let turn_result = self
                .inner
                .execute_turn(
                    agent,
                    context.clone(),
                    llm.clone(),
                    runtime,
                    turn_in,
                    hub,
                    turn_token,
                )
                .await?;

            // Decision gate: ask LLM if we should continue
            if iteration >= self.max_iterations {
                hub.broadcast(WireEvent::TextPart {
                    text: "[Ralph: max iterations reached]\n".to_string(),
                });
                return Ok(TurnResult {
                    stop_reason: format!("ralph_max_iterations:{}", self.max_iterations),
                });
            }

            // Heuristic fast-path: if no tool calls, we're likely done
            if turn_result.stop_reason == "no_tool_calls" {
                hub.broadcast(WireEvent::TextPart {
                    text: "[Ralph: no tool calls, stopping]\n".to_string(),
                });
                return Ok(TurnResult {
                    stop_reason: "ralph_complete".to_string(),
                });
            }

            // LLM decision gate for other stop reasons
            match self
                .should_stop(llm.clone(), &turn_result.stop_reason, iteration, hub)
                .await
            {
                Ok(true) => {
                    return Ok(TurnResult {
                        stop_reason: format!("ralph_decided_stop:{}:", turn_result.stop_reason),
                    });
                }
                Ok(false) => continue,
                Err(e) => {
                    hub.broadcast(WireEvent::StepInterrupted {
                        reason: format!("ralph_decision_error: {}", e),
                    });
                    // On decision error, fall back to heuristic
                    if turn_result.stop_reason != "max_steps" {
                        return Ok(TurnResult {
                            stop_reason: format!("ralph_fallback:{}", turn_result.stop_reason),
                        });
                    }
                }
            }
        }

        Ok(TurnResult {
            stop_reason: "ralph_max_iterations".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentSpec};
    use crate::approval::ApprovalRuntime;
    use crate::config::Config;
    use crate::feature_flags::{ExperimentalFeature, FeatureFlags};
    use crate::hooks::{HookStage, SideEffect, SideEffectResult};
    use crate::llm::{ChatProvider, EchoProvider, HttpGeneration, ScriptedProvider};
    use crate::message::{FunctionCall, ToolCall};
    use crate::notification::types::NotificationEvent;
    use crate::session::Session;
    use crate::store::Store;

    #[test]
    fn test_apply_function_tool_schema_tags_only_when_flag_on() {
        let mut flags = FeatureFlags::default();
        let mut tools = vec![serde_json::json!({"type": "object", "properties": {}})];
        apply_function_tool_schema_tags(&flags, &mut tools);
        assert!(
            !tools[0]
                .as_object()
                .unwrap()
                .contains_key("x-rki-tool-contract")
        );

        flags.enable(ExperimentalFeature::FunctionTools);
        apply_function_tool_schema_tags(&flags, &mut tools);
        assert_eq!(tools[0]["x-rki-tool-contract"], "v1-function-tools");
    }

    fn test_runtime() -> Runtime {
        test_runtime_with_features(FeatureFlags::default())
    }

    fn test_runtime_with_features(features: FeatureFlags) -> Runtime {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session = Session::create(&store, std::env::current_dir().unwrap()).unwrap();
        Runtime::with_features(
            Config {
                max_steps_per_turn: Some(10),
                max_context_size: Some(128_000),
                ..Config::default()
            },
            session,
            approval,
            hub,
            store,
            features,
        )
    }

    /// Captures the last `history` passed into `generate` (for flag gating tests).
    struct HistoryCapture {
        last_history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    }

    #[async_trait]
    impl ChatProvider for HistoryCapture {
        async fn generate(
            &self,
            system_prompt: Option<String>,
            history: Vec<Message>,
            tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            *self.last_history.lock().await = history.clone();
            let echo = EchoProvider;
            echo.generate(system_prompt, history, tools).await
        }
    }

    /// Captures the `system_prompt` passed into `generate` (for L10 precedence tests).
    struct SystemPromptCapture {
        captured: Arc<tokio::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl ChatProvider for SystemPromptCapture {
        async fn generate(
            &self,
            system_prompt: Option<String>,
            history: Vec<Message>,
            tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            *self.captured.lock().await = system_prompt;
            let echo = EchoProvider;
            echo.generate(None, history, tools).await
        }
    }

    async fn seed_context_for_memory_recall_test(context: &Arc<Mutex<Context>>) {
        let mut ctx = context.lock().await;
        ctx.append(Message::User(UserMessage::text(
            "Earlier we discussed oauth hardening",
        )))
        .await
        .unwrap();
        ctx.append(Message::Assistant {
            content: Some("ack".to_string()),
            tool_calls: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_memory_recall_suppressed_without_experimental_flag() {
        let runtime = test_runtime_with_features(FeatureFlags::default());
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        seed_context_for_memory_recall_test(&context).await;

        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<Message>::new()));
        let llm: Arc<dyn ChatProvider> = Arc::new(HistoryCapture {
            last_history: captured.clone(),
        });
        let hub = RootWireHub::new();
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("any follow-up"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let hist = captured.lock().await;
        let joined: String = hist.iter().map(|m| format!("{m:?}")).collect();
        assert!(
            !joined.contains("Relevant context from memory"),
            "expected no recall injection without KIMI_EXPERIMENTAL_MEMORY_HIERARCHY, got {joined}"
        );
    }

    #[tokio::test]
    async fn test_memory_recall_injected_with_experimental_flag() {
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::MemoryHierarchy);
        let runtime = test_runtime_with_features(features);
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        seed_context_for_memory_recall_test(&context).await;

        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<Message>::new()));
        let llm: Arc<dyn ChatProvider> = Arc::new(HistoryCapture {
            last_history: captured.clone(),
        });
        let hub = RootWireHub::new();
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("any follow-up"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let hist = captured.lock().await;
        let joined: String = hist.iter().map(|m| format!("{m:?}")).collect();
        assert!(
            joined.contains("Relevant context from memory"),
            "expected recall injection with KIMI_EXPERIMENTAL_MEMORY_HIERARCHY, got {joined}"
        );
    }

    #[tokio::test]
    async fn test_system_prompt_override_from_context() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        {
            let mut ctx = context.lock().await;
            ctx.append(Message::SystemPrompt {
                content: "context-system-prompt".to_string(),
            })
            .await
            .unwrap();
        }
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "agent-default-prompt".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "agent-default-prompt".to_string(),
        };
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let llm: Arc<dyn ChatProvider> = Arc::new(SystemPromptCapture {
            captured: captured.clone(),
        });
        let hub = RootWireHub::new();
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let sp = captured.lock().await;
        assert_eq!(
            sp.as_deref(),
            Some("context-system-prompt"),
            "context _system_prompt should override agent.system_prompt"
        );
    }

    #[tokio::test]
    async fn test_react_orchestrator_echo() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        let turn_result = result.unwrap();
        assert_eq!(turn_result.stop_reason, "no_tool_calls");

        // Verify hub events (TurnBegin/TurnEnd are sent by KimiSoul, not orchestrator)
        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let has_step = events
            .iter()
            .any(|e| matches!(e, WireEvent::StepBegin { .. }));
        let has_text = events
            .iter()
            .any(|e| matches!(e, WireEvent::TextPart { .. }));
        let has_status = events
            .iter()
            .any(|e| matches!(e, WireEvent::StatusUpdate { .. }));
        assert!(has_step, "Expected StepBegin");
        assert!(has_text, "Expected TextPart");
        assert!(has_status, "Expected StatusUpdate");
    }

    #[tokio::test]
    async fn test_assistant_message_appended_to_context() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context.clone(),
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let ctx = context.lock().await;
        let history = ctx.history();
        let assistant_msgs: Vec<_> = history
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .collect();
        assert_eq!(
            assistant_msgs.len(),
            1,
            "Expected exactly one assistant message in context after turn, got {:?}",
            assistant_msgs
        );
        if let Message::Assistant { content, tool_calls } = assistant_msgs[0] {
            assert_eq!(content.as_deref(), Some("Hello from echo provider."));
            assert!(tool_calls.is_none() || tool_calls.as_ref().unwrap().is_empty());
        }
    }

    /// First LLM response triggers `enter_plan_mode`; second is echo without tools.
    struct EnterPlanModeOnceThenEcho {
        armed: std::sync::atomic::AtomicBool,
    }

    impl EnterPlanModeOnceThenEcho {
        fn new() -> Self {
            Self {
                armed: std::sync::atomic::AtomicBool::new(true),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for EnterPlanModeOnceThenEcho {
        async fn generate(
            &self,
            system_prompt: Option<String>,
            history: Vec<Message>,
            tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            use std::sync::atomic::Ordering;
            if self.armed.swap(false, Ordering::SeqCst) {
                Ok(Box::new(HttpGeneration::new(
                    vec![],
                    vec![ToolCall {
                        id: "tc-plan-1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "enter_plan_mode".to_string(),
                            arguments: "{}".to_string(),
                        },
                    }],
                    None,
                )))
            } else {
                EchoProvider.generate(system_prompt, history, tools).await
            }
        }
    }

    #[tokio::test]
    async fn test_react_orchestrator_dmail_back_to_the_future() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));

        // Seed context: user message -> assistant -> checkpoint(0) -> extra user message
        let mut ctx = context.lock().await;
        ctx.append(Message::User(crate::message::UserMessage::text("before cp")))
            .await
            .unwrap();
        ctx.append(Message::Assistant {
            content: Some("ack".to_string()),
            tool_calls: None,
        })
        .await
        .unwrap();
        let cp_id = ctx.write_checkpoint().await.unwrap();
        ctx.append(Message::User(crate::message::UserMessage::text("after cp")))
            .await
            .unwrap();
        drop(ctx);

        // Queue a D-Mail targeting the checkpoint
        runtime
            .denwa_renji
            .send(
                cp_id,
                vec![Message::User(crate::message::UserMessage::text(
                    "time travel message",
                ))],
            )
            .await;

        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context.clone(),
                llm,
                &runtime,
                TurnInput::text("trigger dmail"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok(), "D-Mail revert should not error: {:?}", result);

        let hist = context.lock().await.history();

        // D-Mail message must be present after revert
        let has_dmail = hist.iter().any(|m| {
            matches!(m, Message::User(u) if u.parts().iter().any(|p| matches!(p, crate::message::ContentPart::Text { text } if text == "time travel message")))
        });
        assert!(has_dmail, "expected D-Mail message in context after revert, got {:?}", hist);

        // Turn continued after revert → assistant response present
        let has_assistant = hist.iter().any(|m| {
            matches!(m, Message::Assistant { content: Some(text), .. } if text == "Hello from echo provider.")
        });
        assert!(has_assistant, "expected assistant response after D-Mail re-trigger, got {:?}", hist);

        // Verify StepInterrupted was broadcast
        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let step_interrupts: Vec<_> = events.iter().filter(|e| {
            matches!(
                e,
                WireEvent::StepInterrupted { reason } if reason == "dmail_revert"
            )
        }).collect();
        assert_eq!(step_interrupts.len(), 1, "expected exactly one StepInterrupted{{dmail_revert}}");

        // Two StepBegin events: original step + re-triggered step after revert
        let step_begins: Vec<_> = events.iter().filter(|e| matches!(e, WireEvent::StepBegin { .. })).collect();
        assert_eq!(step_begins.len(), 2, "expected two steps (original + after D-Mail revert)");
    }

    #[tokio::test]
    async fn test_status_update_reflects_plan_mode_after_plan_tool() {
        let runtime = test_runtime();
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::EnterPlanModeTool));
        }
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EnterPlanModeOnceThenEcho::new());
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        assert!(!runtime.is_plan_mode().await);

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("use plan tool"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let mut saw_plan_true = false;
        while let Ok(envelope) = rx.try_recv() {
            if let WireEvent::StatusUpdate {
                plan_mode: true, ..
            } = envelope.event
            {
                saw_plan_true = true;
            }
        }
        assert!(
            saw_plan_true,
            "StatusUpdate.plan_mode should be true after enter_plan_mode tool (§1.2 L26)"
        );
        assert!(runtime.is_plan_mode().await);
    }

    #[tokio::test]
    async fn test_status_update_fields_populated() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let mut saw_status = false;
        while let Ok(envelope) = rx.try_recv() {
            if let WireEvent::StatusUpdate {
                token_count,
                context_size,
                plan_mode,
                mcp_status,
            } = envelope.event
            {
                saw_status = true;
                // token_count should be non-zero after appending messages
                assert!(token_count > 0, "token_count should be > 0");
                // context_size should match max_context from config
                assert_eq!(context_size, 128_000, "context_size should match config max_context");
                // plan_mode should be false initially
                assert!(!plan_mode, "plan_mode should be false");
                // mcp_status should be populated
                assert!(!mcp_status.is_empty(), "mcp_status should not be empty");
            }
        }
        assert!(saw_status, "Expected at least one StatusUpdate with populated fields");
    }

    #[tokio::test]
    async fn test_react_orchestrator_tool_rejected_stop_reason() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));

        // Register a tool that always rejects
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::FunctionTool::new(
                "always_reject",
                "A tool that always rejects",
                serde_json::json!({"type": "object", "properties": {}}),
                |_args: serde_json::Value, _ctx: &crate::tools::ToolContext| async move {
                    Err(crate::tools::ToolRejected {
                        reason: "test rejection".to_string(),
                        has_feedback: false,
                    }.into())
                },
            )));
        }

        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };

        // LLM that always calls the rejecting tool
        struct ToolCallProvider;
        #[async_trait]
        impl ChatProvider for ToolCallProvider {
            async fn generate(
                &self,
                _system_prompt: Option<String>,
                _history: Vec<Message>,
                _tools: Vec<serde_json::Value>,
            ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
                Ok(Box::new(HttpGeneration::new(
                    vec![],
                    vec![ToolCall {
                        id: "tc-1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "always_reject".to_string(),
                            arguments: "{}".to_string(),
                        },
                    }],
                    None,
                )))
            }
        }

        let llm: Arc<dyn ChatProvider> = Arc::new(ToolCallProvider);
        let hub = RootWireHub::new();
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("call rejecting tool"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().stop_reason, "tool_rejected");
    }

    #[tokio::test]
    async fn test_tool_rejected_with_feedback_does_not_stop_turn() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));

        // Register a tool that rejects WITH feedback
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::FunctionTool::new(
                "reject_with_feedback",
                "A tool that rejects with feedback",
                serde_json::json!({"type": "object", "properties": {}}),
                |_args: serde_json::Value, _ctx: &crate::tools::ToolContext| async move {
                    Err(crate::tools::ToolRejected {
                        reason: "rejected but user was asked".to_string(),
                        has_feedback: true,
                    }.into())
                },
            )));
        }

        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };

        struct FeedbackRejectCallProvider;
        #[async_trait]
        impl ChatProvider for FeedbackRejectCallProvider {
            async fn generate(
                &self,
                _system_prompt: Option<String>,
                _history: Vec<Message>,
                _tools: Vec<serde_json::Value>,
            ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text { text: "done".to_string() }],
                    vec![ToolCall {
                        id: "tc-1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "reject_with_feedback".to_string(),
                            arguments: "{}".to_string(),
                        },
                    }],
                    None,
                )))
            }
        }

        let llm: Arc<dyn ChatProvider> = Arc::new(FeedbackRejectCallProvider);
        let hub = RootWireHub::new();
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("call rejecting tool with feedback"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        // When has_feedback=true, the turn should NOT end with tool_rejected
        let stop_reason = result.unwrap().stop_reason;
        assert_ne!(
            stop_reason, "tool_rejected",
            "Tool rejection with user feedback should not trigger tool_rejected stop_reason"
        );
    }

    /// Tool that sleeps for a fixed duration; used to prove concurrent execution.
    struct SleepTool {
        name: String,
        millis: u64,
    }

    impl SleepTool {
        fn new(name: &str, millis: u64) -> Self {
            Self {
                name: name.to_string(),
                millis,
            }
        }
    }

    #[async_trait]
    impl crate::tools::Tool for SleepTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "Sleep tool for concurrency testing"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::tools::ToolContext,
        ) -> anyhow::Result<crate::tools::ToolOutput> {
            tokio::time::sleep(std::time::Duration::from_millis(self.millis)).await;
            Ok(crate::tools::ToolOutput {
                result: crate::tools::ToolResult {
                    r#type: "success".to_string(),
                    content: vec![crate::message::ContentBlock::Text {
                        text: format!("{} done", self.name),
                    }],
                    summary: format!("{} ok", self.name),
                },
                artifacts: vec![],
                metrics: crate::message::ToolMetrics::default(),
            })
        }
    }

    /// LLM that calls two tools on first generate, then no tool calls.
    struct MultiToolCallProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl MultiToolCallProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for MultiToolCallProvider {
        async fn generate(
            &self,
            _system_prompt: Option<String>,
            _history: Vec<Message>,
            _tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text {
                        text: "calling both".to_string(),
                    }],
                    vec![
                        ToolCall {
                            id: "tc-a".to_string(),
                            kind: "function".to_string(),
                            function: FunctionCall {
                                name: "sleep_a".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                        ToolCall {
                            id: "tc-b".to_string(),
                            kind: "function".to_string(),
                            function: FunctionCall {
                                name: "sleep_b".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                    ],
                    None,
                )))
            } else {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text {
                        text: "done".to_string(),
                    }],
                    vec![],
                    None,
                )))
            }
        }
    }

    #[tokio::test]
    async fn test_concurrent_tool_execution() {
        let runtime = test_runtime();

        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(SleepTool::new("sleep_a", 60)));
            ts.register(Box::new(SleepTool::new("sleep_b", 60)));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(MultiToolCallProvider::new());
        let hub = RootWireHub::new();
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");

        let start = std::time::Instant::now();
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("call both"),
                &hub,
                token,
            )
            .await;
        let elapsed = start.elapsed().as_millis() as u64;

        assert!(result.is_ok(), "Concurrent tool execution should complete");
        // Sequential would be ~120ms; concurrent should be <110ms with margin.
        assert!(
            elapsed < 110,
            "Tools should run concurrently (elapsed {}ms >= 110ms)",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_step_wire_offset_tail_broadcasts_notification() {
        let runtime = test_runtime();
        let hub = runtime.hub.clone();
        let mut rx = hub.subscribe();
        runtime
            .notifications
            .publish(NotificationEvent {
                category: "task".to_string(),
                kind: "wire_tail_ping".to_string(),
                severity: "info".to_string(),
                payload: serde_json::json!({"n": 1}),
                title: "tail-title".to_string(),
                source_kind: "orchestrator".to_string(),
                dedupe_key: None,
                ..Default::default()
            })
            .await
            .unwrap();

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let mut saw = false;
        while let Ok(envelope) = rx.try_recv() {
            if let WireEvent::Notification {
                kind,
                created_at,
                title,
                source_kind,
                ..
            } = &envelope.event
                && kind == "wire_tail_ping" {
                    assert!(
                        *created_at > 0.0,
                        "notification created_at should come from SQLite row"
                    );
                    assert_eq!(title, "tail-title");
                    assert_eq!(source_kind, "orchestrator");
                    saw = true;
                    break;
                }
        }
        assert!(
            saw,
            "expected WireEvent::Notification from §8.4 wire consumer offset tail"
        );
    }

    #[tokio::test]
    async fn test_plan_mode_orchestrator() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        let orch = PlanModeOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("plan something"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        let turn_result = result.unwrap();
        assert_eq!(turn_result.stop_reason, "plan_mode_complete");

        // Verify hub events
        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let has_plan_display = events
            .iter()
            .any(|e| matches!(e, WireEvent::PlanDisplay { .. }));
        assert!(has_plan_display, "Expected PlanDisplay in plan mode");
    }

    #[tokio::test]
    async fn test_plan_mode_delivers_wire_offset_notifications() {
        let runtime = test_runtime();
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        runtime
            .notifications
            .publish(NotificationEvent {
                category: "system".to_string(),
                kind: "plan_wire_ping".to_string(),
                severity: "info".to_string(),
                payload: serde_json::json!({}),
                dedupe_key: None,
                ..Default::default()
            })
            .await
            .unwrap();

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let orch = PlanModeOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("plan ping"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let mut saw = false;
        while let Ok(envelope) = rx.try_recv() {
            if let WireEvent::Notification {
                kind, created_at, ..
            } = &envelope.event
                && kind == "plan_wire_ping" {
                    assert!(*created_at > 0.0);
                    saw = true;
                    break;
                }
        }
        assert!(
            saw,
            "plan mode should emit §8.4 wire offset notifications before LLM streaming"
        );
    }

    #[tokio::test]
    async fn test_runtime_orchestrator_swap() {
        let runtime = test_runtime();

        // Default should be react
        let orch = runtime.get_orchestrator().await;
        assert_eq!(orch.name(), "react");

        // Swap to plan mode
        runtime.enter_plan_mode().await;
        let orch = runtime.get_orchestrator().await;
        assert_eq!(orch.name(), "plan");
        assert!(runtime.is_plan_mode().await);

        // Swap back
        runtime.exit_plan_mode().await;
        let orch = runtime.get_orchestrator().await;
        assert_eq!(orch.name(), "react");
        assert!(!runtime.is_plan_mode().await);
    }

    #[tokio::test]
    async fn test_ralph_orchestrator_stops_on_decision() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        // Scripted provider: returns STOP when it sees the RALPH DECISION prompt
        let llm: Arc<dyn ChatProvider> = Arc::new(
            ScriptedProvider::new("Hello from echo provider.")
                .with_response("[RALPH DECISION]", "STOP"),
        );
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        let orch = RalphOrchestrator::new(5);
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        let turn_result = result.unwrap();
        assert!(
            turn_result.stop_reason.contains("ralph_decided_stop")
                || turn_result.stop_reason == "ralph_complete",
            "Expected ralph stop, got: {}",
            turn_result.stop_reason
        );

        // Verify hub events show Ralph iterations
        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let has_ralph_start = events
            .iter()
            .any(|e| matches!(e, WireEvent::TextPart { text } if text.contains("Ralph mode")));
        assert!(has_ralph_start, "Expected Ralph mode start message");
    }

    #[tokio::test]
    async fn test_ralph_orchestrator_max_iterations() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        // With max_iterations=1, the first iteration hits the max check before
        // any decision gate or fast path, guaranteeing max_iterations stop.
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();

        let orch = RalphOrchestrator::new(1);
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        let turn_result = result.unwrap();
        assert!(
            turn_result.stop_reason.contains("ralph_max_iterations"),
            "Expected max iterations stop, got: {}",
            turn_result.stop_reason
        );
    }

    #[tokio::test]
    async fn test_ralph_orchestrator_fast_path_no_tools() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        // EchoProvider returns no tool calls → fast path should trigger
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();

        let orch = RalphOrchestrator::new(5);
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        let turn_result = result.unwrap();
        assert_eq!(turn_result.stop_reason, "ralph_complete");
    }

    #[tokio::test]
    async fn test_react_orchestrator_consumes_steers() {
        let runtime = test_runtime();
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        // Queue a steer before the turn starts
        runtime.steer_queue.push("steer message".to_string()).await;

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context.clone(),
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        // The steer should be consumed and injected, causing the turn to continue
        // Since EchoProvider returns no tool calls, the steer causes one extra step
        // and then stops with no_tool_calls

        // Verify steer queue is empty
        assert!(runtime.steer_queue.is_empty().await);

        // Verify SteerInput event was emitted
        let mut has_steer = false;
        while let Ok(envelope) = rx.try_recv() {
            if matches!(envelope.event, WireEvent::SteerInput { .. }) {
                has_steer = true;
            }
        }
        assert!(has_steer, "Expected SteerInput event");
    }

    /// PreExecute side effect that blocks the `think` tool.
    struct BlockThinkPreExecute;

    #[async_trait]
    impl SideEffect for BlockThinkPreExecute {
        fn name(&self) -> &str {
            "block_think_pre_execute"
        }
        fn stage(&self) -> HookStage {
            HookStage::PreExecute
        }
        fn is_critical(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _event: &str,
            payload: &serde_json::Value,
        ) -> anyhow::Result<SideEffectResult> {
            if payload.get("tool").and_then(|t| t.as_str()) == Some("think") {
                return Ok(SideEffectResult::block("no think"));
            }
            Ok(SideEffectResult::allow())
        }
    }

    /// LLM that emits a `think` tool call on first generate, then no tool calls.
    struct ThinkCallProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ThinkCallProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for ThinkCallProvider {
        async fn generate(
            &self,
            _system_prompt: Option<String>,
            _history: Vec<Message>,
            _tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text {
                        text: "calling think".to_string(),
                    }],
                    vec![ToolCall {
                        id: "tc1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "think".to_string(),
                            arguments: r#"{"thought":"hmm"}"#.to_string(),
                        },
                    }],
                    None,
                )))
            } else {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text { text: "done".to_string() }],
                    vec![],
                    None,
                )))
            }
        }
    }

    fn collect_tool_results(events: &[WireEvent]) -> Vec<(bool, String)> {
        events
            .iter()
            .filter_map(|e| {
                if let WireEvent::ToolResult {
                    output, is_error, ..
                } = e
                {
                    Some((*is_error, output.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn test_structured_effects_preexecute_block_skips_tool() {
        let mut f = FeatureFlags::default();
        f.enable(ExperimentalFeature::StructuredEffects);
        let runtime = test_runtime_with_features(f);
        runtime
            .hooks
            .register(std::sync::Arc::new(BlockThinkPreExecute));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::think_tool()));
        }
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let tool_results = collect_tool_results(&events);
        assert!(
            tool_results
                .iter()
                .any(|(err, o)| *err && o.contains("Blocked by hook")),
            "expected blocked tool result, got {:?}",
            tool_results
        );
    }

    #[tokio::test]
    async fn test_preexecute_block_ignored_without_structured_effects_flag() {
        let runtime = test_runtime();
        runtime
            .hooks
            .register(std::sync::Arc::new(BlockThinkPreExecute));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::think_tool()));
        }
        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "You are a test agent.".to_string(),
        };
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let tool_results = collect_tool_results(&events);
        assert!(
            tool_results
                .iter()
                .any(|(err, o)| !err && !o.contains("Blocked by hook")),
            "expected successful tool run ignoring PreExecute block, got {:?}",
            tool_results
        );
    }

    /// PreValidate side effect that blocks the `think` tool.
    struct BlockThinkPreValidate;

    #[async_trait]
    impl SideEffect for BlockThinkPreValidate {
        fn name(&self) -> &str {
            "block_think_pre_validate"
        }
        fn stage(&self) -> HookStage {
            HookStage::PreValidate
        }
        fn is_critical(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _event: &str,
            payload: &serde_json::Value,
        ) -> anyhow::Result<SideEffectResult> {
            if payload.get("tool").and_then(|t| t.as_str()) == Some("think") {
                return Ok(SideEffectResult::block("no think in prevalidate"));
            }
            Ok(SideEffectResult::allow())
        }
    }

    /// Counter side effect for verifying stage invocation.
    struct CountingEffect {
        stage: HookStage,
        name: String,
        counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl SideEffect for CountingEffect {
        fn name(&self) -> &str {
            &self.name
        }
        fn stage(&self) -> HookStage {
            self.stage
        }
        fn is_critical(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _event: &str,
            _payload: &serde_json::Value,
        ) -> anyhow::Result<SideEffectResult> {
            self.counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SideEffectResult::allow())
        }
    }

    /// Critical side effect that always fails.
    struct FailingCriticalEffect;

    #[async_trait]
    impl SideEffect for FailingCriticalEffect {
        fn name(&self) -> &str {
            "failing_critical"
        }
        fn stage(&self) -> HookStage {
            HookStage::PreValidate
        }
        fn is_critical(&self) -> bool {
            true
        }

        async fn execute(
            &self,
            _event: &str,
            _payload: &serde_json::Value,
        ) -> anyhow::Result<SideEffectResult> {
            anyhow::bail!("critical failure")
        }
    }

    #[tokio::test]
    async fn test_prevalidate_block_stops_tool() {
        let runtime = test_runtime();
        runtime
            .hooks
            .register(std::sync::Arc::new(BlockThinkPreValidate));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::misc::ThinkTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["think".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;
        assert!(result.is_ok());

        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope.event);
        }
        let tool_results = collect_tool_results(&events);
        assert_eq!(tool_results.len(), 1);
        assert!(
            tool_results[0].0 && tool_results[0].1.contains("Blocked by hook"),
            "expected PreValidate block, got {:?}",
            tool_results
        );
    }

    #[tokio::test]
    async fn test_postexecute_hook_runs_on_success() {
        let runtime = test_runtime();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runtime.hooks.register(std::sync::Arc::new(CountingEffect {
            stage: HookStage::PostExecute,
            name: "post_exec_counter".into(),
            counter: counter.clone(),
        }));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::misc::ThinkTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["think".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "PostExecute should run once after successful tool"
        );
    }

    #[tokio::test]
    async fn test_audit_hook_runs_for_every_tool() {
        let runtime = test_runtime();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runtime.hooks.register(std::sync::Arc::new(CountingEffect {
            stage: HookStage::Audit,
            name: "audit_counter".into(),
            counter: counter.clone(),
        }));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::misc::ThinkTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["think".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Audit should run once per tool call"
        );
    }

    #[tokio::test]
    async fn test_critical_hook_failure_propagates() {
        let runtime = test_runtime();
        runtime
            .hooks
            .register(std::sync::Arc::new(FailingCriticalEffect));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::misc::ThinkTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["think".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_err(), "Critical hook failure should propagate");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("critical failure"),
            "expected critical failure in error, got {}",
            err
        );
    }

    /// Tool that always fails immediately (for PostExecuteFailure testing).
    struct FailingTool;

    #[async_trait]
    impl crate::tools::Tool for FailingTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::tools::ToolContext,
        ) -> anyhow::Result<crate::tools::ToolOutput> {
            anyhow::bail!("intentional failure")
        }
    }

    /// LLM that emits a `fail` tool call on first generate, then no tool calls.
    struct FailingToolCallProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FailingToolCallProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for FailingToolCallProvider {
        async fn generate(
            &self,
            _system_prompt: Option<String>,
            _history: Vec<Message>,
            _tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text { text: "calling fail".to_string() }],
                    vec![ToolCall {
                        id: "tc-fail".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "fail".to_string(),
                            arguments: r#"{}"#.to_string(),
                        },
                    }],
                    None,
                )))
            } else {
                Ok(Box::new(HttpGeneration::new(
                    vec![ContentPart::Text { text: "done".to_string() }],
                    vec![],
                    None,
                )))
            }
        }
    }

    /// Non-critical hook that always fails.
    struct FailingNonCriticalEffect;

    #[async_trait]
    impl crate::hooks::SideEffect for FailingNonCriticalEffect {
        fn name(&self) -> &str {
            "flaky"
        }
        fn stage(&self) -> crate::hooks::HookStage {
            crate::hooks::HookStage::PreValidate
        }
        fn is_critical(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _event: &str,
            _payload: &serde_json::Value,
        ) -> anyhow::Result<crate::hooks::SideEffectResult> {
            anyhow::bail!("non-critical oops")
        }
    }

    #[tokio::test]
    async fn test_postexecute_failure_hook_runs_on_tool_error() {
        let runtime = test_runtime();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runtime.hooks.register(std::sync::Arc::new(CountingEffect {
            stage: HookStage::PostExecuteFailure,
            name: "post_fail_counter".into(),
            counter: counter.clone(),
        }));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(FailingTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["fail".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(FailingToolCallProvider::new());
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context,
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "PostExecuteFailure should run once when tool fails"
        );
    }

    #[tokio::test]
    async fn test_non_critical_failure_does_not_stop_turn() {
        let runtime = test_runtime();
        runtime
            .hooks
            .register(std::sync::Arc::new(FailingNonCriticalEffect));
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::misc::ThinkTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["think".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(ThinkCallProvider::new());
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("hello"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok(), "Non-critical hook failure should not stop the turn");
    }

    #[tokio::test]
    async fn test_notifications_claimed_and_batched_into_context() {
        let runtime = test_runtime();
        let hub = runtime.hub.clone();

        // Publish 5 notifications
        for i in 0..5 {
            runtime
                .notifications
                .publish(NotificationEvent {
                    category: "test".to_string(),
                    kind: format!("notif-{}", i),
                    severity: "info".to_string(),
                    payload: serde_json::json!({}),
                    title: format!("Title {}", i),
                    source_kind: "orchestrator".to_string(),
                    dedupe_key: Some(format!("dk-{}", i)),
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec![],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(EchoProvider);
        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        orch.execute_turn(
            &agent,
            context.clone(),
            llm,
            &runtime,
            TurnInput::text("hello"),
            &hub,
            token,
        )
        .await
        .unwrap();

        // Count notification-derived messages in context history
        let ctx = context.lock().await;
        let history = ctx.history();
        let notif_msgs: Vec<_> = history
            .iter()
            .filter(|m| {
                if let Message::User(u) = m {
                    u.flatten_for_recall().contains("<notification")
                } else {
                    false
                }
            })
            .collect();
        assert_eq!(
            notif_msgs.len(),
            4,
            "Expected exactly 4 notification messages in context (batch limit), got {}",
            notif_msgs.len()
        );
        drop(ctx);

        // Verify the 5th notification is still pending
        let remaining = runtime.notifications.claim("llm").await;
        assert_eq!(
            remaining.len(),
            1,
            "Expected 1 remaining notification after batch of 4, got {}",
            remaining.len()
        );
    }

    #[tokio::test]
    async fn test_config_hot_reload() {
        let runtime = test_runtime();

        // Verify initial config
        {
            let cfg = runtime.config.read().await;
            assert_eq!(cfg.default_model, "echo");
            assert_eq!(cfg.max_steps_per_turn, Some(10));
        }

        // Create a temp config file with different values
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "gpt-4"

[loop_control]
max_steps_per_turn = 50
max_context_size = 256000
"#,
        )
        .unwrap();

        // Reload
        runtime.reload_config(&config_path).await.unwrap();

        // Verify new config
        {
            let cfg = runtime.config.read().await;
            assert_eq!(cfg.default_model, "gpt-4");
            assert_eq!(cfg.max_steps_per_turn, Some(50));
            assert_eq!(cfg.max_context_size, Some(256_000));
        }
    }

    /// LLM that emits a `think` tool call on EVERY generate (never stops naturally).
    struct AlwaysThinkCallProvider;

    #[async_trait]
    impl ChatProvider for AlwaysThinkCallProvider {
        async fn generate(
            &self,
            _system_prompt: Option<String>,
            _history: Vec<Message>,
            _tools: Vec<serde_json::Value>,
        ) -> anyhow::Result<Box<dyn llm::LLMGeneration>> {
            Ok(Box::new(HttpGeneration::new(
                vec![ContentPart::Text { text: "thinking".to_string() }],
                vec![ToolCall {
                    id: format!("tc-{}", uuid::Uuid::new_v4()),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "think".to_string(),
                        arguments: r#"{"thought":"hmm"}"#.to_string(),
                    },
                }],
                None,
            )))
        }
    }

    #[tokio::test]
    async fn test_max_steps_limits_loop() {
        let features = FeatureFlags::default();
        // Limit to 3 steps so the test doesn't take too long
        let runtime = test_runtime_with_features(features);
        {
            let mut cfg = runtime.config.write().await;
            cfg.max_steps_per_turn = Some(3);
        }
        {
            let mut ts = runtime.toolset.write().await;
            ts.register(Box::new(crate::tools::misc::ThinkTool));
        }

        let store = runtime.store.clone();
        let context = Arc::new(Mutex::new(
            Context::load(&store, &runtime.session.id).await.unwrap(),
        ));
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "test".to_string(),
                tools: vec!["think".to_string()],
                capabilities: vec![],
                ..Default::default()
            },
            system_prompt: "test".to_string(),
        };
        let llm: Arc<dyn ChatProvider> = Arc::new(AlwaysThinkCallProvider);
        let hub = RootWireHub::new();

        let orch = ReActOrchestrator;
        let token = ContextToken::new(runtime.session.id.clone(), "test-turn");
        let result = orch
            .execute_turn(
                &agent,
                context,
                llm,
                &runtime,
                TurnInput::text("loop forever"),
                &hub,
                token,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().stop_reason, "max_steps", "Should stop at max_steps limit");
    }
}
