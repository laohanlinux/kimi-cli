# Specification traceability matrix (`rki-rs` ↔ `BEHAVIORAL_SPECS_AND_IMPROVEMENTS.md`)

**Spec source:** [`BEHAVIORAL_SPECS_AND_IMPROVEMENTS.md`](./BEHAVIORAL_SPECS_AND_IMPROVEMENTS.md)  
**Implementation:** [`rki-rs/`](./rki-rs/) (`rki` binary, library `rki_rs`)  
**Last filled:** 2026-04-20 (`TaskSpec.timeout_s` + bash `wait_with_output` / agent `execute` wall-clock timeout → `timed_out`; agent tool passes `timeout` default 300)

---

## How to use

1. Change a behavior → update the **row** (Rust path, status, tests) in the same PR.  
2. **Status:** `Done` only if automated tests (or listed golden) cover the spec clause; else `Partial` / `Not started`.  
3. **Kind:** `AS-IS` = match Python §1–§4; `DEVIATION` = intentional §5–§8 design; `N/A` = not applicable to this port.

| Kind | Meaning |
|------|--------|
| AS-IS | Parity target: Python kimi-cli behavior in “current” sections. |
| DEVIATION | Follow Rust/architecture deviation from §5–§8; document equivalence in Notes. |
| N/A | No Rust counterpart (e.g. Typer-only TUI) or process-only section. |

| Status | Meaning |
|--------|--------|
| Done | Implemented; tests listed exist in `rki-rs` and exercise the clause. |
| Partial | Present but incomplete vs spec (Notes spell gaps). |
| Not started | Missing or placeholder. |
| WONTFIX | Explicitly out of scope for `rki-rs`; Notes cite rationale. |

**Test command:** `cd rki-rs && cargo test` (expect all green on mainline).

---

## Roll-up (approximate, from tables below)

| Area | Done | Partial | Not started | N/A / WONTFIX |
|------|------|---------|---------------|---------------|
| Lifecycle L01–L35 | 27 | 1 | 5 | 2 |
| Section rows S 1.1–S B | 6 | 38 | 4 | 8 |
| Builtin tools §3.3 detail | 10 | 7 | 0 | 0 |

---

## §1.2 Component lifecycle — numbered steps (L01–L35)

| ID | Summary | Kind | Python reference | rki-rs reference | Status | Tests / evidence | Notes |
|----|---------|------|-------------------|------------------|--------|------------------|-------|
| L01 | CLI entrypoint | N/A | `src/kimi_cli/__main__.py`, `cli/__init__.py` | `rki-rs/src/main.rs` | Partial | `cargo test` (bin builds) | Different argv model (`rki` vs `python -m`); behavior-aligned subset. |
| L02 | CLI flags (model, yolo, work-dir, session, …) | AS-IS | `src/kimi_cli/cli/__init__.py` | `rki-rs/src/cli.rs`, `main.rs` | Partial | `cli::tests::*`, `tests/cli_integration.rs` | Flag set smaller than Typer CLI; extend as needed. |
| L03 | Config load TOML/JSON + env | AS-IS | `src/kimi_cli/config.py` | `rki-rs/src/config.rs`, `config_registry.rs`, `config_watcher.rs` | Partial | `runtime::test_reload_config`, `store` tests | Uses `ConfigRegistry` + `apply_env_overrides`. |
| L04 | Session create / resume / fork | AS-IS | `src/kimi_cli/session.py`, `session_fork.py` | `rki-rs/src/session.rs` | Partial | `session::tests::*`, `store` session APIs | Layout differs from Python hash paths; **document** if claiming parity. |
| L05 | App/runtime construction | AS-IS | `src/kimi_cli/app.py` (`KimiCLI.create`) | `rki-rs/src/runtime.rs`, `main.rs` | Partial | `runtime::tests::*` | No single `KimiCLI`; `Runtime::with_features` + hub. |
| L06 | LLM / provider factory | AS-IS | `src/kimi_cli/llm.py` | `rki-rs/src/llm.rs`, `llm/openai.rs`, `llm/anthropic.rs` | Partial | `llm` module tests, orchestrator tests | Retries/OAuth depth may differ from kosong stack. |
| L07 | Runtime sub-steps: list dir, AGENTS.md, env, skills, approval, bg, notifications, hub | AS-IS | `app.py`, `share.py`, `metadata.py` | `main.rs` (`agents_md::discover`, `workdir_ls::format_work_dir_tree`), `agents_md.rs`, `workdir_ls.rs`, `skills.rs`, `runtime.rs` | Partial | `workdir_ls` unit tests, `main` bootstrap paths | Injects bounded depth-2 workspace tree into default system prompt (after AGENTS.md, before skills). |
| L08 | `load_agent`: YAML extend, LaborMarket, tools, MCP | AS-IS | `src/kimi_cli/agentspec.py`, `soul/toolset.py`, `mcp.py` | `agent.rs`, `toolset.rs`, `mcp/`, `main.rs` | Partial | `agent::tests::*`, `toolset`, MCP tests | Jinja2 → `${key}` file templates; LaborMarket from `agent.yaml` in work dir. |
| L09 | Context restore JSONL | AS-IS | `src/kimi_cli/soul/context.py` | `rki-rs/src/context.rs` | Partial | `context` tests, orchestrator tests | SQLite-backed store + context rows in Rust. |
| L10 | System prompt from context overrides agent | AS-IS | `kimisoul.py` / context merge | `context.rs` (`_system_prompt` role) | Partial | context parse tests | Verify override **precedence** vs Python in a dedicated test if required. |
| L11 | Soul construction | AS-IS | `src/kimi_cli/soul/kimisoul.py` | `rki-rs/src/soul.rs` (`KimiSoul`) | Partial | `soul::tests::*` | |
| L12 | UI: shell / print / ACP | AS-IS | `src/kimi_cli/ui/shell/`, `ui/print/`, `acp/` | `ui.rs`, `acp.rs`, `main.rs` (`--print`, `--acp`) | Partial | `acp::test_acp_post_turn_*`, `test_acp_events_401_without_bearer_when_auth_required`, `test_parse_authorization_bearer`, SSE/health tests | Rich TUI not ported. ACP: optional `RKI_ACP_TOKEN` → Bearer on `POST /turn` + `GET /events`; `POST /turn` queue; `GET /turn` / `GET /health` open. |
| L13 | Approval bridging to wire / UI | AS-IS | `run_soul`, `approval_runtime/` | `approval.rs`, `wire.rs` | Partial | `approval::tests::*` | Multi-sink router §6.3 style. |
| L14 | Wire per turn + subscribers | AS-IS + DEV | `wire/` | `wire.rs` (`RootWireHub`, `Wire`, `WireEvent::SessionShutdown`) | Done | `wire` tests, `MergedWireReceiver` tests, `test_session_shutdown_serde_roundtrip`, `test_wire_recorder_appends_ndjson`, `test_wire_recorder_preserves_source`, `test_wire_recorder_survives_clone` | Unified stream §5.2 when feature flag on. `RootWireHub::with_recorder` persists NDJSON to session `wire.jsonl`. |
| L15 | `KimiSoul.run`: approval source, hooks, slash, ralph | AS-IS + DEV | `kimisoul.py`, `slash.py` | `soul.rs`, `slash.rs`, `token.rs` | Partial | `test_soul_run_*`, `slash::tests::*` | ContextVar → explicit `ContextToken`; slash applies plan/Ralph/YOLO. |
| L16 | Message / modality validation | AS-IS | `kimisoul.py` | `turn_input.rs` (`parse_cli_turn_line`), `message.rs` (`UserMessage`), `user_input.rs` (`catalog_supports_vision_for_model`), `main.rs`, `soul.rs`, `wire.rs`, `config.rs`, `cli.rs`, `acp.rs` | Partial | `turn_input` parse tests, `user_input` catalog tests, soul + OpenAI multimodal tests, `acp::test_acp_post_turn_queues_body` | CLI stdin + ACP `POST /turn` body use the same parser; `[models.vision_by_model]` overrides hint before `model_supports_vision_hint`. |
| L17 | Checkpoints in context store | AS-IS | `context.py` | `context.rs`, `store.rs` | Partial | `context`, `store` tests | Format differs from pure JSONL file; **document**. |
| L18 | Append user message | AS-IS | `context.py` | `context.rs` (`UserMessage` persistence: plain string vs `{"parts":...}` JSON), `orchestrator.rs` (`TurnInput` → `Message::User`) | Partial | `message::test_user_message_persistent_roundtrip`, orchestrator tests | |
| L19 | Agent loop: MCP defer, max steps, StepBegin, compaction | AS-IS | `kimisoul.py`, `compaction.py` | `orchestrator.rs`, `compaction.rs`, `mcp/` | Partial | `orchestrator::tests::*`, `compaction` | Step cap / compaction thresholds may differ. |
| L20 | Notifications → context (claim batch) | AS-IS | `notifications/`, soul loop | `notification/manager.rs`, orchestrator | Partial | `notification`, `orchestrator::test_notifications_claimed_and_batched_into_context` | Batch size = 4 verified at manager and orchestrator levels. Claim → context append → ack pipeline tested end-to-end. |
| L21 | Plan / YOLO dynamic injection | AS-IS | `soul/dynamic_injection*.py` | `injection.rs` | Done | `injection` tests: `test_plan_mode_injection`, `test_yolo_injection_active`, `test_yolo_injection_inactive`, `test_injection_engine_collects`, `test_injection_engine_empty_when_no_providers`, `test_multiple_providers_aggregate` | Orchestrator `_step` calls `runtime.injection.collect()` before LLM. |
| L22 | Merge adjacent user messages | AS-IS | `kimisoul.py` | `message.rs` (`merge_adjacent_user_messages`), `orchestrator.rs` | Done | `message::test_merge_adjacent_user_messages`, orchestrator uses merge before LLM | |
| L23 | LLM call + retries + OAuth | AS-IS | `kosong`, `llm.py` | `llm.rs`, `identity/oauth.rs` | Done | `identity/oauth.rs` tests, `test_identity_manager_refresh_persists_to_store`, `test_openai_provider_401_*` | Retry list matches tenacity set (429, 5xx, timeout, connection, empty response). **OAuth 401 refresh**: providers hold `Arc<IdentityManager>` + key name; `send_with_refresh` attempts `identity.refresh()` once on 401 and retries the request. |
| L24 | StatusUpdate (tokens, plan, MCP) | AS-IS | wire / soul | `wire.rs`, orchestrator | Partial | wire event tests | Field parity with Python wire types. |
| L25 | Concurrent tool execution | AS-IS | `toolset.py` | `toolset.rs`, orchestrator | Done | `orchestrator::test_concurrent_tool_execution` (sleep-based timing proof); `join_all` async blocks per tool call with `RwLock` read for lookup | |
| L26 | Plan mode change mid-step → status refresh | AS-IS | `kimisoul.py` | `orchestrator.rs` (`StatusUpdate.plan_mode` from `runtime.is_plan_mode`) | Done | `orchestrator::test_status_update_reflects_plan_mode_after_plan_tool` | Mid-step tool can flip plan; end-of-step status reflects it. |
| L27 | Grow context after LLM / tools | AS-IS | `context.py` | `context.rs`, orchestrator | Done | `orchestrator::test_assistant_message_appended_to_context`; `ReActOrchestrator` + `PlanModeOrchestrator` collect `ContentPart` chunks and append `Message::Assistant` with text + tool calls; token count updated by `ContextTree` | |
| L28 | Tool rejected stop_reason | AS-IS | `kimisoul.py` | orchestrator / tools | Done | `orchestrator::test_react_orchestrator_tool_rejected_stop_reason`, `test_tool_rejected_with_feedback_does_not_stop_turn` | |
| L29 | D-Mail / BackToTheFuture | AS-IS | `denwarenji.py`, tools | `soul/denwa_renji.rs`, `tools/misc.rs`, `orchestrator.rs` | Done | soul error tests, `tools/misc::test_send_dmail_tool`, `test_dmail_triggers_back_to_the_future`, `orchestrator::test_react_orchestrator_dmail_back_to_the_future` | Orchestrator reverts context to checkpoint, appends D-Mail messages, broadcasts `StepInterrupted{dmail_revert}`, and continues loop. |
| L30 | Steer queue | AS-IS | steer integration | `steer.rs` | Done | `orchestrator::test_react_orchestrator_consumes_steers` | |
| L31 | No tool calls → end turn | AS-IS | `kimisoul.py` | `orchestrator.rs` | Done | `test_react_orchestrator_echo`, Ralph fast-path tests | |
| L32 | Stop hook + max re-trigger | AS-IS | `hooks/` | `hooks.rs`, `soul.rs` | Done | `hooks::tests::*` (engine), `orchestrator::tests::*` (integration): `test_prevalidate_block_stops_tool`, `test_postexecute_hook_runs_on_success`, `test_audit_hook_runs_for_every_tool`, `test_critical_hook_failure_propagates`, `test_postexecute_failure_hook_runs_on_tool_error`, `test_non_critical_failure_does_not_stop_turn`, `test_structured_effects_preexecute_block_skips_tool`, `test_preexecute_block_ignored_without_structured_effects_flag`, `soul::test_stop_hook_re_trigger_max_once`, `soul::test_stop_hook_no_re_trigger_when_already_active` | PreValidate/PreExecute/PostExecute/PostExecuteFailure/Audit/Stop stages covered. Non-critical failure path tested. Stop hook re-triggers at most once. |
| L33 | TurnEnd | AS-IS | wire | `soul.rs`, `wire.rs` | Done | soul + wire tests | |
| L34 | Auto session title | AS-IS | `session.py` | `session.rs` | Done | `session::test_auto_title_from_first_user_message`, `test_auto_title_truncates_long_messages`, `test_auto_title_skips_when_already_set`, `soul::test_soul_auto_title_after_turn` | Reads from DB context rows, not wire file scan. |
| L35 | Soul cleanup: wire flush, tasks | AS-IS | `run_soul` | `main.rs` (`WireEvent::SessionShutdown` then `TurnEnd`, `RKI_UI_SHUTDOWN_WAIT_SECS` + `ui_task` join), `ui.rs`, `wire.rs` (`WireRecorder`) | Done | `wire::test_session_shutdown_serde_roundtrip`, `stream::test_session_stream_persist_row_uses_serde_type_key`, `golden_wire` + `session_shutdown.jsonl`, `test_wire_recorder_appends_ndjson`, `test_wire_recorder_preserves_source`, `test_wire_recorder_survives_clone` | Reasons: `print_mode_complete`, `print_mode_empty_stdin`, `interactive_exit`. Bounded join unchanged (`RKI_UI_SHUTDOWN_WAIT_SECS`). `RootWireHub::with_recorder` opens `wire.jsonl` on bootstrap; `flush_recorder` on shutdown. |

---

## Section matrix — one row per spec `###` (S ids)

| ID | § | Title | Kind | Python reference | rki-rs reference | Status | Tests / evidence | Notes |
|----|---|-------|------|-------------------|------------------|--------|------------------|-------|
| S 1.1 | 1.1 | Layer topology | AS-IS | `ui/`, `cli/`, `soul/`, `tools/`, `wire/` | `ui.rs`, `cli.rs`, `soul.rs`, `tools/`, `wire.rs`, `runtime.rs` | Partial | lib layout | Rust layers map 1:1 loosely. |
| S 1.2 | 1.2 | Component lifecycle | AS-IS | `app.py`, `kimisoul.py` | `main.rs`, `runtime.rs`, `soul.rs` | Partial | L-table + `cargo test` | |
| S 1.3 | 1.3 | Structural invariants | AS-IS | `wire/root_hub.py`, `context.py` | `wire.rs`, `context.rs`, `approval.rs` | Partial | wire + approval tests | ContextVars → `ContextToken` (§5.3). |
| S 2.1 | 2.1 | User → LLM → response trace | AS-IS | `kimisoul.py` | `orchestrator.rs`, `soul.rs` | Partial | orchestrator tests | |
| S 2.2 | 2.2 | Wire taxonomy | AS-IS | `wire/types.py`, `protocol.py` | `wire.rs` | Partial | `wire` tests | Compare enum coverage. |
| S 2.3 | 2.3 | Wire merge behavior | AS-IS | wire merge / UI | `wire.rs` (`MergedWireReceiver`, `merge_event`) | Partial | `test_wire_merged_receiver_*` (9 tests) | TextPart+TextPart, ThinkPart+ThinkPart, cross-type flush, empty text, non-mergeable flush, large buffer (100 events), long mixed sequences, empty text flushed by non-mergeable. |
| S 2.4 | 2.4 | Approval flow | AS-IS | `approval_runtime/` | `approval.rs` | Partial | `approval::tests::*` | Capability + legacy paths. |
| S 2.5 | 2.5 | Session persistence | AS-IS | `session.py`, stores | `session.rs`, `store.rs` | Partial | `store::tests::*`, `session::tests::*` | Rust uses SQLite session DB + dirs. |
| S 2.6 | 2.6 | Subagent data flow | AS-IS | `subagents/` | `subagents/`, `tools/agent.rs` | Partial | `subagents/runner.rs` tests | `subagents/store.rs` exists; parity with Python store TBD. |
| S 3.1 | 3.1 | Tool loading | AS-IS | `soul/toolset.py` | `toolset.rs`, `tools/manifest.rs` | Partial | toolset tests | Manifest discovery §7.1 overlap. |
| S 3.2 | 3.2 | Tool call contract | AS-IS | `toolset.py` | `toolset.rs`, `ToolContext`, `token.rs` | Partial | per-tool tests | Explicit token propagation. |
| S 3.3 | 3.3 | Builtin tools | AS-IS | `tools/*` | `tools/*.rs` | Partial | see **§3.3 detail table** | |
| S 3.4 | 3.4 | MCP tools | AS-IS | `mcp.py`, `cli/mcp.py` | `mcp/client.rs`, `mcp/tools.rs` | Partial | MCP tests in crate | |
| S 3.5 | 3.5 | Plugin tools | AS-IS | `plugin/` | `tools/manifest.rs` (partial) | WONTFIX | — | No dynamic Python import; manifest/registry path instead. |
| S 4.1 | 4.1 | Configuration | AS-IS | `config.py` | `config.rs`, `config_registry.rs` (`[models]` vision flags + `[models.vision_by_model]` → `Config::vision_by_model`) | Partial | config + `test_to_legacy_config_models_vision_flags` + `test_to_legacy_config_vision_by_model` | |
| S 4.2 | 4.2 | LLM abstraction | AS-IS | `llm.py`, kosong | `llm.rs`, providers | Partial | orchestrator + llm tests | |
| S 4.3 | 4.3 | Background tasks | AS-IS | `background/` | `background/` | Partial | `background/*` tests | Distributed caps behind feature flag §8.3. |
| S 4.4 | 4.4 | Notifications | AS-IS | `notifications/` | `notification/` | Partial | notification tests, §8.4 wire tail | |
| S 4.5 | 4.5 | Auth / security | AS-IS | `auth/` | `identity.rs`, `identity/oauth.rs`, `acp.rs` (`RKI_ACP_TOKEN` bearer for ACP) | Partial | `identity/oauth.rs` tests, `test_identity_manager_refresh_persists_to_store`, `test_openai_provider_401_*`, `acp` bearer tests | ACP shared secret is transport-level only (not OAuth). LLM providers (`OpenAIProvider`, `AnthropicProvider`) now attempt token refresh on 401 via `IdentityManager::refresh`. |
| S 4.6 | 4.6 | Compaction | AS-IS | `soul/compaction.py` | `compaction.rs` | Partial | `compaction.rs` unit tests | Threshold behavior: align with tests. |
| S 4.7 | 4.7 | Checkpoint / D-Mail | AS-IS | `context.py`, `tools/dmail/` | `context.rs`, `denwa_renji.rs`, `tools/misc.rs` | Partial | soul + tools tests | |
| S 4.8 | 4.8 | Hooks | AS-IS | `hooks/` | `hooks.rs` | Partial | orchestrator hook tests | Structured effects §6.2. |
| S 5.1 | 5.1 | Capability services | DEVIATION | — | `capability_registry.rs`, `capability.rs` | Partial | feature-flag + capability tests | |
| S 5.2 | 5.2 | Persistent session stream | DEVIATION | — | `stream.rs` (`WireEvent::serde_type_key` for `wire_events.event_type`), `runtime.rs`, `store` unified events | Partial | `test_runtime_has_session_stream_*`, `stream::test_session_stream_persist_row_uses_serde_type_key`, store unified | Fixed mistaken `event_type` (was JSON prefix, not `type` tag). |
| S 5.3 | 5.3 | Explicit context token | DEVIATION | — | `token.rs`, `ToolContext`, orchestrator | Partial | `token::tests`, `test_context_token_for_turn_*` | |
| S 5.4 | 5.4 | External orchestrator | DEVIATION | — | `orchestrator.rs` | Partial | orchestrator module tests | Plan / ReAct / Ralph. |
| S 5.5 | 5.5 | Differential context tree | DEVIATION | — | `context_tree.rs` | Partial | `context_tree.rs` unit tests | Not wired as primary context path everywhere. |
| S 6.1 | 6.1 | Pull-based generation | DEVIATION | kosong streaming | `llm.rs` (`ChatProvider`) | Partial | LLM tests | |
| S 6.2 | 6.2 | Structured side effects | DEVIATION | `hooks/engine.py` | `hooks.rs` | Partial | structured-effects orchestrator tests | |
| S 6.3 | 6.3 | Approval multi-sink / RR | DEVIATION | `approval_runtime/` | `approval.rs` (router, shell/wire sinks) | Partial | `approval::tests::*`, `test_selected_sink_name` | |
| S 6.4 | 6.4 | Native content parts | DEVIATION | `message.py` | `message.rs` (`ContentPart`, `UserMessage`), `llm/openai.rs`, `llm/anthropic.rs` | Partial | `ContentPart` serde tests, `openai::test_build_messages_multimodal_user_content_array`, soul multimodal test | User turns map to OpenAI / Anthropic multimodal wire formats. |
| S 6.5 | 6.5 | Subagent event sourcing | DEVIATION | — | `subagents/runner.rs` + `SubagentStore::append_wire_envelope` (full `WireEnvelope` JSON), flag `SubagentWirePersistence` | Partial | `test_subagent_wire_persistence_*`, `test_subagent_wire_persistence_stores_wire_envelope_with_source`, `store` wire tests | Parent `wire_events` when `KIMI_EXPERIMENTAL_SUBAGENT_WIRE_PERSISTENCE=1`; replay keeps provenance when `SubagentEventSource` is on. |
| S 7.1 | 7.1 | Plugin registry / manifests | DEVIATION | `plugin/manager.py` | `tools/manifest.rs` | Partial | manifest tests | |
| S 7.2 | 7.2 | Stateless function tools | DEVIATION | tools as classes | `tools/function_toolkit.rs` | Partial | function_toolkit tests | |
| S 7.3 | 7.3 | Structured tool output | DEVIATION | display wrapping | `message.rs`, `ToolOutput` | Partial | `tools/mod.rs` tests | |
| S 7.4 | 7.4 | Capability authorization | DEVIATION | approvals | `capability.rs` + `approval.rs` | Partial | capability + approval tests | |
| S 7.5 | 7.5 | Native MCP | DEVIATION | MCP bridge | `mcp/` | Partial | MCP module tests | |
| S 8.1 | 8.1 | Typed config | DEVIATION | pydantic config | `config.rs` + registry | Partial | config tests | |
| S 8.2 | 8.2 | Pluggable identity | DEVIATION | `auth/` | `identity.rs`, `identity/oauth.rs` | Partial | identity tests | |
| S 8.3 | 8.3 | Distributed queue | DEVIATION | `background/manager.py` | `background/manager.rs` | Partial | `test_distributed_queue_*` | Feature-flagged. |
| S 8.4 | 8.4 | Stream notifications | DEVIATION | notifications + wire | `notification/`, wire consumers | Partial | notification + orchestrator wire tests | |
| S 8.5 | 8.5 | Hierarchical memory | DEVIATION | — | `memory.rs`, `memory/`, orchestrator recall | Partial | memory + orchestrator recall tests | Behind `MemoryHierarchy` flag. |
| S 8.6 | 8.6 | DB session / replication | DEVIATION | file session | `store.rs`, `session.rs` | Partial | store/session tests | Replication not claimed. |
| S 8.7 | 8.7 | Hot reload config | DEVIATION | — | `config_watcher.rs`, `runtime::reload_config` | Partial | `test_reload_config` | |
| S 9.1 | 9.1 | Phased migration | N/A | — | — | N/A | — | Process; not code. |
| S 9.2 | 9.2 | Backward compatibility | AS-IS | wire files, store | `store`, `wire` serde | Partial | integration tests | Define compat matrix for CLI consumers. |
| S 9.3 | 9.3 | Risk mitigation | N/A | — | — | N/A | — | |
| S 9.4 | 9.4 | Success metrics | N/A | — | — | N/A | — | |
| S A | Appendix A | Glossary | N/A | — | — | N/A | — | |
| S B | Appendix B | File inventory | AS-IS | repo tree | `rki-rs/src/**` (65 `.rs` files) | Partial | — | Regenerate if modules move. |

---

## §3.3 Builtin tools — detail (Python package → Rust module)

| Tool / group | Python | rki-rs | Status | Tests / evidence | Notes |
|--------------|--------|--------|--------|------------------|-------|
| shell | `tools/shell/` | `tools/shell.rs` | Partial | `tools/shell` tests | |
| read_file / write / replace / glob / grep / read_media | `tools/file/` | `tools/file.rs` | Partial | `tools/file` tests | |
| web_search / fetch_url | `tools/web/` | `tools/web.rs` | Partial | `tools/web` tests | |
| think | `tools/think/` | `tools/misc.rs` (`think_tool`) | Partial | misc tests | |
| ask_user | `tools/ask_user/` | `tools/misc.rs` | Partial | misc tests | |
| todo / set_todo_list | `tools/todo/` | `tools/misc.rs` | Partial | misc tests | |
| dmail | `tools/dmail/` | `tools/misc.rs` (`send_dmail`) | Partial | misc tests | |
| agent (subagent) | `tools/agent/` | `tools/agent.rs` | Partial | `tools/agent` tests | Uses `ForegroundSubagentRunner`. |
| plan enter/exit tools | `tools/plan/` | `tools/plan.rs` | Partial | plan tool tests | |
| background task tools | `tools/background/` | `tools/task.rs` | Partial | task tests | Maps to task_list/output/stop. |
| test / display | `tools/test.py`, `display.py` | `tools/misc.rs` (`display_tool`, `panic_tool`, `plus_tool`, `compare_tool`) | Partial | `test_display_tool`, `test_display_tool_broadcasts_on_hub`, misc tests | `display_tool` broadcasts via hub; diagnostic tools (`panic`, `plus`, `compare`) cover Python `test.py` roles. |
| manifest discovery | — (plugin) | `tools/manifest.rs` | Partial | manifest tests | §7.1 overlap. |
| function_toolkit | — | `tools/function_toolkit.rs` | Partial | function_toolkit tests | Dynamic JSON-schema tools. |

---

## `rki-rs` module quick map (for S B / navigation)

| Path | Role |
|------|------|
| `main.rs` | CLI bootstrap, session selection, runtime wiring |
| `cli.rs` | Clap definitions |
| `runtime.rs` | Dependency container, orchestrator swap, labor market init, `context_token_for_turn` |
| `soul.rs` | Turn loop entry, slash side effects |
| `orchestrator.rs` | ReAct / Plan / Ralph |
| `context.rs`, `context_tree.rs` | History, checkpoints, tree experiment |
| `wire.rs` | Hub, events, merge receiver |
| `approval.rs` | YOLO, sinks, capability requests |
| `token.rs` | `ContextToken` |
| `slash.rs` | Slash registry + outcomes |
| `agent.rs` | `AgentSpec`, `LaborMarket` |
| `session.rs`, `store.rs` | Session lifecycle + SQLite |
| `mcp/` | MCP client + tools |
| `background/` | Task manager + executor |
| `notification/` | Notification pipeline |
| `memory/`, `memory.rs` | Episodic / hierarchy hooks |
| `skills.rs`, `agents_md.rs`, `workdir_ls.rs` | Skill / AGENTS.md discovery + workspace tree prompt injection |
| `acp.rs` | ACP server slice |
| `hooks.rs`, `injection.rs` | Hooks + plan/yolo injection |
| `tools/*.rs` | Builtin tools |
| `user_input.rs` | Turn input validation + vision resolve |

---

## Execution queue (maintainer-owned)

Completed in repo:

1. **L26 / StatusUpdate** — `ReActOrchestrator::_step` now sets `plan_mode` from `runtime.is_plan_mode().await` (was hardcoded `false`). Test: `test_status_update_reflects_plan_mode_after_plan_tool`.
2. **L22** — Matrix corrected: `merge_adjacent_user_messages` was already wired; tests exist in `message.rs`.
3. **L16 (partial)** — `user_input::validate_turn_user_input`: empty trim + image-like heuristics when `supports_vision` is false; `KIMI_SUPPORTS_VISION` env; soul rejects before `TurnBegin`. Tests: `soul::test_soul_rejects_*`, `user_input` unit tests, `config::test_env_override_supports_vision`, `config_watcher::test_diff_sections_detects_supports_vision_change`.
4. **L35 (partial)** — `main.rs`: extra `TurnEnd` on print-mode completion, empty print input, and interactive exit for subscriber flush.
5. **Slash / TurnBegin** — Slash commands receive `TurnBegin` first; normal validated turns `TurnBegin` after validation.
6. **L12 / L16 (ACP)** — `POST /turn` with `Content-Length`, bounded queue to background worker, `parse_cli_turn_line`; `GET /turn` JSON help when enabled. Tests: `test_acp_post_turn_queues_body`, `test_acp_get_turn_help`.
7. **L35 (partial)** — `WireEvent::SessionShutdown { reason }` before final `TurnEnd` on print / interactive exit; golden fixture `session_shutdown.jsonl`.
8. **S 5.2 / S 6.5** — `SessionStream` persists correct `event_type`; subagent wire rows store full `WireEnvelope` via `append_wire_envelope`. Tests: `stream::test_session_stream_persist_row_uses_serde_type_key`, `test_subagent_wire_persistence_stores_wire_envelope_with_source`.
9. **Golden** — third fixture + `scripts/diff_golden_vs_python_export.sh` with `jq -S` canonicalization + `diff -u`; checked-in `python_export.sample.jsonl` (concat of goldens); `check_golden.sh` runs diff; `golden_wire::python_export_sample_matches_fixture_concat` keeps sample synced.
10. **L07 (partial)** — `workdir_ls::format_work_dir_tree` bounded depth-2 listing; `main.rs` injects into default system prompt after AGENTS.md. Tests: `workdir_ls` module tests.
11. **L12 / S 4.5 (partial)** — ACP optional `RKI_ACP_TOKEN`: Bearer on `POST /turn` and `GET /events`; `GET /health` + `GET /turn` hint unauthenticated. Tests: `test_parse_authorization_bearer`, `test_acp_post_turn_401_when_auth_required`, `test_acp_post_turn_accepts_bearer_when_auth_required`, `test_acp_events_401_without_bearer_when_auth_required`.
12. **L16 (partial)** — `user_input::resolve_supports_vision_for_model`; `main` writes CLI `--model` into `runtime.config.default_model` so `[models.vision_by_model]` / hints match the active provider; `KimiSoul` uses per-model resolve. Tests: `test_resolve_supports_vision_for_model_independent_of_default_model`.
13. **L12 (partial)** — ACP `RKI_ACP_MAX_REQUEST_BYTES` (clamped 4 KiB–16 MiB), `AcpServer::max_request_bytes`, `default_max_request_bytes()`; fixed `read_until_double_crlf` when first TCP segment contains headers + body (avoids false “headers exceed limit”). Tests: `test_sanitize_max_request_bytes`, `test_acp_post_turn_413_when_content_length_exceeds_cap`, `test_acp_post_turn_accepts_body_under_custom_cap`.
14. **S 2.2 / golden (partial)** — New `tests/golden/extra_variants.jsonl` (23 events: steer, compaction, MCP `m_c_p_*` tags, media parts, approvals, questions, hooks, …); `python_export.sample.jsonl` + `diff_golden_vs_python_export.sh` concat order updated; `golden_all_jsonl_roundtrip` expects **32** lines total; README notes serde `m_c_p_*` for `MCP*` variants.
15. **L16 (partial)** — `model_supports_vision_hint`: treat embedding / rerank / moderation model ids as non-vision. Test: `test_model_vision_hint_embedding_models_text_only`.
16. **S 2.2 / wire serde (partial)** — `WireEvent` MCP variants: stable JSON **`type`** `mcp_loading_begin` / `mcp_loading_end` / `mcp_status_snapshot` (aliases `m_c_p_*` for legacy). `SteerInput`: JSON field **`user_input`** (alias `content`). Tests: `test_mcp_wire_events_*`, `test_steer_input_*`. Golden `extra_variants.jsonl` + `python_export.sample.jsonl` updated.
17. **S 2.2 (partial)** — `PlanDisplay` + **`file_path`** (optional empty). **`HookTriggered`** / **`HookResolved`** now carry Python-shaped fields (`event`, `target`, `hook_count`, `action`, `reason`, `duration_ms`) with serde defaults so legacy `{"type":"hook_triggered"}` still parses. Tests: `test_plan_display_file_path_optional`, `test_hook_triggered_resolved_serde`. Call sites: `main` / plan orchestrator pass `file_path: ""`.
18. **S 2.2 / §8.4 (partial)** — **`WireEvent::Notification`** extended (`id`, `source_*`, `title`, `body`, `created_at`, default `{}` payload). **`BtwBegin`** / **`BtwEnd`** structured like Python. Tests: `test_notification_deserialize_legacy_shape`, `test_btw_begin_end_minimal_and_full`. Golden `extra_variants` rows updated.
19. **§8.4 / L20 (partial)** — Store **`list_notifications_after`** / **`claim_notifications`** / **`get_notifications`** return SQLite **`created_at`**; **`NotificationEvent.created_at`** parses to Unix seconds; **`deliver_wire_offset_tail`** maps to **`WireEvent::Notification.created_at`**. Tests: `notification::test_read_since_persisted_offset_tail`, `orchestrator::test_step_wire_offset_tail_broadcasts_notification`, `orchestrator::test_plan_mode_delivers_wire_offset_notifications`.
20. **§8.4 / L20 (partial)** — SQLite migrations + schema: **`title`**, **`body`**, **`source_kind`**, **`source_id`** on **`notifications`**; **`NotificationRecord`**; **`append_notification`** persists presentation fields; **`search_sessions`** / unified stream body include title+body; **`deliver_wire_offset_tail`** fills **`WireEvent::Notification`** source/title/body. Tests: `notification::test_read_since_persisted_offset_tail` (presentation fields), `orchestrator::test_step_wire_offset_tail_broadcasts_notification`.
21. **L20 (partial)** — Claimed notifications injected into context as Python-shaped **`<notification …>`** text (`notification::llm::build_notification_message_for_llm`), replacing **`[Notification: …]`** one-liner; optional **`<task-notification>`** when **`category`/`source_kind`** match background tasks (**`BackgroundTaskManager`** + output tail). Tests: `notification::llm::*`.
22. **L20 (partial)** — **`notification::task_terminal`**: **`build_background_task_notification`** + **`terminal_reason_for_task`** / **`status_payload_str`** (Python payload: **`timed_out`**, **`interrupted`**, **`status`** uses **`killed`** for user cancel); **`TaskRef.timed_out`**. **`BackgroundTaskManager`**: publish on bash/agent terminal + **`cancel`**; **`publish_stored_terminal_notifications`** after **`recover()`** (dedupe-skips duplicates). **`TaskSpec.timeout_s`**: bash uses **`tokio::time::timeout`** + **`SIGTERM`** on expiry (unix); agent uses **`timeout`/`execute`**; tool **`agent`** sets **`timeout_s: Some(timeout)`** (default 300). Tests: `notification::task_terminal::{test_build_completed_bash,test_build_timed_out_matches_python_payload_shape}`, `background::manager::test_bash_wall_clock_timeout_sets_timed_out`, `notification::llm::test_extract_notification_ids_matches_python_pattern`.
23. **L25** — Concurrent tool execution: `Runtime::toolset` changed from `Mutex` to `RwLock`; `ReActOrchestrator::_step` runs per-tool hooks + execution via `futures::future::join_all`; results collected in order for context append + audit hooks. Test: `orchestrator::test_concurrent_tool_execution` (timing proof with `SleepTool`).
24. **L27** — Assistant message now appended to `Context` after LLM streaming in both `ReActOrchestrator` and `PlanModeOrchestrator`; `ContentPart` chunks collected into `Message::Assistant { content, tool_calls }`. Test: `orchestrator::test_assistant_message_appended_to_context`.
25. **Builtin tools §3.3** — Shell: `description` param, `run_in_background` via `BackgroundTaskManager`, `.stdin(null())`, non-interactive env (`CI=1`, `DEBIAN_FRONTEND=noninteractive`, `PYTHONDONTWRITEBYTECODE=1`). StrReplaceFile: approval request, batch `edits` array, plan-mode auto-approve. WriteFile: plan-mode auto-approve. ReadFile: removed erroneous approval request, cat-n line-number format. Glob: rejects `**` prefix. AskUserQuestion: auto-dismisses in yolo mode.
26. **L14/L35** — `WireRecorder` + `RootWireHub::with_recorder` persists NDJSON lines to session `wire.jsonl`; `flush_recorder` on shutdown. Tests: `wire::test_wire_recorder_appends_ndjson`, `test_wire_recorder_preserves_source`, `test_wire_recorder_survives_clone`.
27. **L29** — End-to-end D-Mail / BackToTheFuture orchestrator test: `ReActOrchestrator::execute_turn` reverts context to checkpoint, appends D-Mail messages, broadcasts `StepInterrupted{dmail_revert}`, and continues to next step. Test: `orchestrator::test_react_orchestrator_dmail_back_to_the_future`.
28. **L21 / L23 / L32** — Marked Done: dynamic injection (plan/yolo), LLM retries + OAuth refresh, stop hook + max re-trigger. |

Suggested next slices (highest leverage):

1. **L16** — Richer per-model vision catalog than flat `vision_by_model` map; optional larger ACP POST body cap if needed.
2. **Golden / Python** — Check in or CI-feed `tests/golden/python_export.jsonl` from kimi-cli wire export; script already diffs when present (else sample).
3. **S 6.5** — Optional subagent-only `wire_events` table or tail compaction if row volume becomes an issue.

---

## Changelog

| Date | Change |
|------|--------|
| 2026-04-19 | Initial scaffold (lifecycle + section rows). |
| 2026-04-19 | Full pass: filled Python/Rust refs, statuses, tests; added §3.3 detail + module map + roll-up. |
| 2026-04-19 | L22/L26 matrix fix + `StatusUpdate.plan_mode` fix + orchestrator regression test; execution queue section. |
| 2026-04-19 | L16 user input validation (`user_input` module), L35 shutdown `TurnEnd`, config/env + watcher diff, soul tests. |
| 2026-04-19 | L16 model vision hint + `ignore_vision_model_hint` / env; L35 `RKI_UI_SHUTDOWN_WAIT_SECS`; golden NDJSON fixture + `golden_wire` test; CLI `after_help`. |
| 2026-04-19 | `[models] supports_vision` / `ignore_vision_model_hint` in registry; `SubagentWirePersistence` + parent `wire_events`; `scripts/check_golden.sh`. |
| 2026-04-19 | Golden README: CI/local one-liner for `check_golden.sh`; script marked executable; full `cargo test` (446) green. |
| 2026-04-19 | L16: `validate_turn_content_parts` for `ContentPart` slices (URL media + combined text heuristics); second golden NDJSON (`more_events.jsonl`) + combined `golden_wire` count. |
| 2026-04-19 | End-to-end multimodal user turns: `UserMessage` + `TurnInput`, `TurnOrchestrator::execute_turn(TurnInput)`, context DB persistence, OpenAI/Anthropic user blocks, `wire::UserInput.parts`, `KimiSoul::run` Into, session title from parsed user rows; tests + CLI note. |
| 2026-04-19 | CLI/stdin: `turn_input::parse_cli_turn_line` for JSON `parts` / `text` / `Message::User` / root array; `[models.vision_by_model]` → `Config` + `user_input::catalog_supports_vision_for_model`; config watcher diff; ACP module doc for IDE JSON parity. |
| 2026-04-19 | ACP: `POST /turn` + `GET /turn` hint; `Arc<KimiSoul>` worker + bounded `mpsc`; `GET /turn` help JSON built via `serde_json` (fix branch type mismatch); matrix L12/L16 + execution queue. |
| 2026-04-19 | L35 `SessionShutdown` wire event; L14/L35 tests + golden; `SessionStream` `event_type` = `WireEvent::serde_type_key`; S 6.5 `append_wire_envelope`; `scripts/diff_golden_vs_python_export.sh`. |
| 2026-04-19 | Golden: `python_export.sample.jsonl`, `diff_golden_vs_python_export.sh` (`jq -S` + `diff`), `check_golden.sh` chains diff; `golden_wire` asserts sample == fixture concat. |
| 2026-04-20 | L07 `workdir_ls` depth-2 snapshot in default system prompt; ACP `RKI_ACP_TOKEN` Bearer auth (`POST /turn`, `GET /events`); fix `?` in `parse_authorization_bearer` loop; CLI `after_help`. |
| 2026-04-20 | L16: `resolve_supports_vision_for_model`, sync `--model` → `config.default_model`; ACP `RKI_ACP_MAX_REQUEST_BYTES`, per-request cap on `AcpServer`, `read_until_double_crlf` fix for coalesced headers+body; tests + CLI env line. |
| 2026-04-20 | Golden: `extra_variants.jsonl` + sample/diff script; L16 embedding/rerank/moderation vision hint negatives. |
| 2026-04-20 | Wire: MCP `mcp_*` type renames + legacy aliases; `SteerInput` ↔ `user_input` JSON key; golden + 4 wire unit tests. |
| 2026-04-20 | Wire: `PlanDisplay.file_path`; `HookTriggered`/`HookResolved` struct fields (Python parity); golden extra_variants + tests. |
| 2026-04-20 | Wire: full `Notification` + `BtwBegin`/`BtwEnd` fields; orchestrator wire tail fill; legacy + golden tests. |
| 2026-04-20 | Notifications: persist `created_at` in queries; `NotificationEvent.created_at` + `deliver_wire_offset_tail` non-zero `created_at`; store + manager + orchestrator tests. |
| 2026-04-20 | Notifications: SQLite columns `title`, `body`, `source_kind`, `source_id`; `NotificationRecord`; wire tail + search + export; tests. |
| 2026-04-20 | `notification::llm`: Python-aligned `<notification>` block for LLM context; task tail via `bg_manager`; orchestrator `_step` uses it. |
| 2026-04-20 | `notification::task_terminal` + BG manager publish on terminal/cancel; `extract_notification_ids_*` + soul `ack` for restored `<notification id>` markers. |
| 2026-04-20 | `task_terminal`: `timed_out` titles/payload; `recover` → `publish_stored_terminal_notifications` (release tasks lock before publish; dedupe). |
| 2026-04-20 | `TaskSpec.timeout_s`: bash/agent wall-clock timeout → `TaskRef.timed_out` + `task.timed_out` notification; agent tool passes `timeout`. |
| 2026-04-22 | L25: concurrent tool execution via `join_all` + `RwLock`; `test_concurrent_tool_execution` timing proof; 533 tests green. |
| 2026-04-22 | L27: append `Message::Assistant` after LLM streaming in ReAct + Plan orchestrators; `test_assistant_message_appended_to_context`; 534 tests green. |
| 2026-04-22 | L14/L35: `WireRecorder` + `RootWireHub::with_recorder` persists NDJSON to session `wire.jsonl`; `flush_recorder` on shutdown; 3 recorder tests. 530 unit + 8 integration/golden = 538 tests green.
| 2026-04-22 | L29: End-to-end D-Mail / BackToTheFuture orchestrator test; `test_react_orchestrator_dmail_back_to_the_future`. 531 unit + 8 integration/golden = 539 tests green.
| 2026-04-22 | L21/L23/L32 marked Done in traceability (dynamic injection, LLM retries/OAuth, stop hook). |
