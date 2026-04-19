# Kimi Code CLI — Exhaustive Behavioral Specifications & Architectural Improvements

> **Scope**: This document analyzes the current kimi-cli source and produces exhaustive behavioral specifications together with principled improvements that **deviate** from the original in architecture, data flows, tool contracts, and system designs.

---

## Table of Contents

1. [Current Architecture — Behavioral Specification](#1-current-architecture--behavioral-specification)
2. [Current Data Flows — Behavioral Specification](#2-current-data-flows--behavioral-specification)
3. [Current Tool Contracts — Behavioral Specification](#3-current-tool-contracts--behavioral-specification)
4. [Current System Designs — Behavioral Specification](#4-current-system-designs--behavioral-specification)
5. [Proposed Architectural Deviations](#5-proposed-architectural-deviations)
6. [Proposed Data Flow Deviations](#6-proposed-data-flow-deviations)
7. [Proposed Tool Contract Deviations](#7-proposed-tool-contract-deviations)
8. [Proposed System Design Deviations](#8-proposed-system-design-deviations)
9. [Migration Path & Risk Assessment](#9-migration-path--risk-assessment)

---

## 1. Current Architecture — Behavioral Specification

### 1.1 Layer Topology

The system is organized into **five conceptual layers**:

| Layer | Modules | Responsibility |
|-------|---------|--------------|
| Presentation | `ui/shell/`, `ui/print/`, `ui/acp/`, `wire/server` | Render Wire messages to human or machine consumers |
| Application | `cli/__init__.py`, `app.py` | Bootstrap, config resolution, session lifecycle, UI dispatch |
| Domain (Soul) | `soul/kimisoul.py`, `soul/agent.py`, `soul/context.py` | Agent loop, conversation state, compaction, checkpointing |
| Tooling | `soul/toolset.py`, `tools/`, `mcp.py`, `plugin/` | Tool loading, execution, MCP bridging, plugin isolation |
| Infrastructure | `wire/`, `approval_runtime/`, `background/`, `notifications/`, `session.py` | Persistence, broadcast, task isolation, auth, notifications |

### 1.2 Component Lifecycle (Exact Behavioral Sequence)

**Phase A — CLI Bootstrap**
1. `python -m kimi_cli` → `__main__.py` → `cli/__init__.py:kimi()`
2. Typer resolves flags (`--model`, `--yolo`, `--print`, `--session`, `--work-dir`)
3. `load_config()` loads TOML/JSON from `~/.kimi/config.toml`
4. `Session.create()` or `Session.continue_()` establishes session directory:
   - New: `~/.kimi/sessions/<work_dir_hash>/<uuid>/`
   - Resume: discovers latest non-archived session or exact match by ID
5. `KimiCLI.create(session, config, model_name, yolo, plan_mode, ...)` is awaited

**Phase B — Runtime & Agent Construction**
6. `create_llm()` instantiates a `kosong.ChatProvider` wrapped in `LLM`
7. `Runtime.create()`:
   - `list_directory(work_dir)` — recursive listing up to depth 2, max 100 entries
   - `load_agents_md(work_dir)` — discovers `AGENTS.md` files root→leaf, concatenates
   - `Environment.detect()` — detects OS, shell, Python version, git status
   - `discover_skills_from_roots()` — scans `~/.kimi/skills/`, `.kimi/skills/`, package defaults
   - Builds `BuiltinSystemPromptArgs` with all collected metadata
   - Creates `ApprovalState` (merged from CLI `--yolo` and persisted `state.json`)
   - Creates `BackgroundTaskManager`, `NotificationManager`, `SubagentStore`, `RootWireHub`, `ApprovalRuntime`
8. `load_agent()`:
   - Parses YAML agent spec with `extend` inheritance chain
   - Renders system prompt via Jinja2 with `${var}` delimiters using `builtin_args`
   - Registers builtin subagent types in `LaborMarket`
   - `KimiToolset.load_tools()` — imports by path string, injects deps via constructor introspection
   - `load_mcp_tools()` — deferred background connection to MCP servers
9. `Context.restore()` reads `context.jsonl` line-by-line, reconstructs history, token count, checkpoint counter
10. System prompt sync: if context already has `_system_prompt`, it **overrides** the agent's loaded prompt
11. `KimiSoul(agent, context)` instantiation

**Phase C — UI Launch & Turn Execution**
12. UI-specific entry: `run_shell()`, `run_print()`, `run_acp()`, `run_wire_stdio()`
13. `KimiCLI.run()` sets up approval bridging between `ApprovalRuntime` and per-turn `Wire`
14. `run_soul()` creates fresh `Wire`, starts `ui_loop_fn(wire)` and `soul.run(user_input)` concurrently
15. `KimiSoul.run()`:
    - Sets `_current_approval_source` ContextVar to `foreground_turn` + turn UUID
    - Runs `UserPromptSubmit` hooks; if any block → emits `TurnBegin → TextPart("Blocked by hook") → TurnEnd`
    - Parses slash commands; if matched → dispatches handler
    - If `max_ralph_iterations != 0` → enters `FlowRunner.ralph_loop()`
    - Otherwise → `self._turn(user_message)`

**Phase D — Agent Loop (`_turn` → `_agent_loop` → `_step`)**
16. `_turn()` validates message against LLM capabilities (e.g., rejects image input for text-only models)
17. `_checkpoint()` writes `{"role":"_checkpoint","id":N}` to `context.jsonl`
18. Appends user message to `Context`
19. `_agent_loop()`:
    - Starts deferred MCP loading task if not already started
    - Step loop: max `loop_control.max_steps_per_turn` (default 100)
    - Emits `StepBegin(n)`
    - Auto-compaction check: if `token_count >= max_context_size * 0.85` or `token_count + 50K >= max_context_size`
    - `compact_context()` preserves last 2 user/assistant exchanges, summarizes rest via separate LLM call
    - `_checkpoint()`
    - `_step()`

**Phase E — `_step()` Execution**
20. Notification delivery (root role only): claims up to 4 pending notifications, appends as user messages
21. Dynamic injection: collects from `PlanModeInjectionProvider`, `YoloModeInjectionProvider`, appends as system reminder messages
22. History normalization: merges adjacent user messages into single message with concatenated content
23. LLM call: `kosong.step(chat_provider, system_prompt, toolset, history, on_message_part=wire_send, on_tool_result=wire_send)`
    - Retry via `tenacity`: `APIConnectionError`, `APITimeoutError`, `APIEmptyResponseError`, 429, 5xx
    - OAuth 401: attempts token refresh once, retries once
24. Emits `StatusUpdate(token_count, context_size, plan_mode, mcp_status)`
25. `await result.tool_results()` — kosong executes all tool calls concurrently via `KimiToolset.handle()`
26. Plan mode check: if changed during tool execution, emits corrected `StatusUpdate`
27. `_grow_context()`: appends assistant message, updates token count, appends tool result messages
28. Rejection handling: if any tool rejected without user feedback and not subagent → `stop_reason="tool_rejected"`
29. D-Mail check: if `DenwaRenji` has pending D-Mail → raises `BackToTheFuture(checkpoint_id, messages)`
30. Steer consumption: if steers queued → continues to next step
31. If no tool calls → `stop_reason="no_tool_calls"`, turn ends

**Phase F — Turn Conclusion**
32. `Stop` hook triggered (max 1 re-trigger to prevent infinite loops)
33. Emits `TurnEnd()`
34. Auto-sets session title after first real turn if not already set (scans wire file for first `TurnBegin`)
35. `run_soul()` cleanup: cancels notification pump, shuts down Wire, joins UI loop, Wire recorder flushes to `wire.jsonl`

### 1.3 Key Structural Invariants

- **Wire is SPMC**: One soul publisher, multiple UI subscribers. A fresh Wire is created per turn.
- **RootWireHub is session-scoped**: Persists across turns for out-of-turn approvals from background agents/subagents.
- **Context is append-only JSONL**: Never mutated in-place; compaction creates a new file via rotation.
- **ApprovalRuntime is the single source of truth**: All approval state lives here; Wire only projects it.
- **Runtime is a dependency container**: Nearly all cross-cutting state is held in `Runtime` and injected into tools.
- **ContextVars for implicit propagation**: `current_wire`, `current_session_id`, `current_approval_source`, `current_tool_call` are all ContextVars.

---

## 2. Current Data Flows — Behavioral Specification

### 2.1 User Input → LLM → Response (Complete Trace)

```
[User keystrokes]
    │
    ▼
[Shell UI: prompt_toolkit reads input]
    │
    ▼
[Shell.run_soul_command builds UserInput]
    │
    ▼
[run_soul() creates Wire, starts ui_task + soul_task]
    │
    ├──► [ui_task: Shell UI subscribes to WireUISide, renders via rich.Live]
    │
    └──► [soul_task: KimiSoul.run()]
             │
             ▼
         [wire_send(TurnBegin)] ───────► Wire ───────► UI renders turn header
             │
             ▼
         [parse slash commands?]
             │ Yes ──► execute handler (may call soul._turn recursively)
             │ No
             ▼
         [_turn(user_message)]
             │
             ▼
         [_checkpoint() ──► context.jsonl]
             │
             ▼
         [_context.append_message(user_message) ──► context.jsonl]
             │
             ▼
         [_agent_loop()]
             │
             ▼
         [_step()]
             │
             ├──► [notification delivery] ──► append to effective_history
             ├──► [dynamic injection] ──► append to effective_history
             ├──► [history normalization] ──► merge adjacent user messages
             │
             ▼
         [kosong.step(..., on_message_part=wire_send)]
             │
             ├──► [LLM streams TextPart] ──► wire_send ──► Wire ──► UI updates _current_content_block
             ├──► [LLM streams ThinkPart] ──► wire_send ──► UI accumulates thinking block
             ├──► [LLM emits ToolCall] ──► wire_send ──► UI creates _tool_call_block
             │
             ▼
         [result.tool_results()]
             │
             ├──► For each ToolCall:
             │       KimiToolset.handle(tool_call)
             │       ├──► PreToolUse hooks
             │       ├──► Approval.request() ──► ApprovalRuntime ──► RootWireHub ──► UI modal
             │       ├──► tool.call(arguments) ──► async execution
             │       ├──► PostToolUse hooks (fire-and-forget)
             │       └──► returns ToolResult
             │
             ▼
         [_grow_context()]
             │
             ├──► append assistant Message ──► context.jsonl
             ├──► update_token_count ──► context.jsonl (_usage record)
             └──► append tool result Messages ──► context.jsonl
             │
             ▼
         [wire_send(ToolResult)] ──► Wire ──► UI updates _tool_call_block with result
             │
             ▼
         [D-Mail check?]
             │ Yes ──► raise BackToTheFuture ──► revert context ──► restart from checkpoint
             │ No
             ▼
         [steers queued?]
             │ Yes ──► continue to next step
             │ No + no tool calls ──► return stop_reason
             │
             ▼
         [wire_send(TurnEnd)] ──► Wire ──► UI returns to prompt
```

### 2.2 Wire Protocol — Exact Message Taxonomy & Behavior

**Control Events** (soul → UI, no response expected):
- `TurnBegin(user_input: UserInput)` — emitted once per turn start
- `TurnEnd()` — emitted once per turn end
- `StepBegin(n: int)` — emitted at start of each step
- `StepInterrupted(reason: str)` — emitted when step is cancelled or errors
- `SteerInput(content: str)` — emitted when user sends steer during active turn

**Compaction Events**:
- `CompactionBegin()` / `CompactionEnd()` — bracket compaction operation

**Status Events**:
- `StatusUpdate(token_count, context_size, plan_mode, mcp_status)` — emitted after LLM response and after plan mode changes
- `MCPLoadingBegin()` / `MCPLoadingEnd()` — bracket MCP background loading
- `MCPStatusSnapshot(servers: list[ServerStatus])` — current MCP server states

**Content Events** (mergeable in Wire):
- `TextPart(text: str)` — streaming text delta
- `ThinkPart(text: str)` — streaming thinking delta
- `ImageURLPart(url: str)` — image reference
- `AudioURLPart(url: str)` — audio reference
- `VideoURLPart(url: str)` — video reference

**Tool Events**:
- `ToolCall(id: str, function: FunctionCall)` — tool call start
- `ToolCallPart(id: str, ...)` — partial tool call (streaming function args)
- `ToolResult(tool_call_id: str, output: str | list[ContentPart], is_error: bool)` — tool execution result

**Request/Response Events** (have internal `asyncio.Future`):
- `ApprovalRequest(id, tool_call_id, sender, action, description, display)` → expects `ApprovalResponse(id, approved, feedback)`
- `ToolCallRequest(id, tool_call)` → expects response with result
- `QuestionRequest(id, questions)` → expects `QuestionResponse(id, answers)`
- `HookRequest(id, hook_name, payload)` → expects `HookResponse(id, action, reason)`

**Other Events**:
- `Notification(category, type, severity, payload)` — system notification
- `SubagentEvent(parent_tool_call_id, agent_id, subagent_type, event)` — subagent event wrapper
- `PlanDisplay(content)` — plan mode display content
- `BtwBegin()` / `BtwEnd()` — bracket "by the way" side questions
- `HookTriggered()` / `HookResolved()` — hook lifecycle

### 2.3 Wire Merge Behavior

The `WireSoulSide` maintains two `BroadcastQueue`s:
1. `_raw_queue` — every message as-is
2. `_merged_queue` — mergeable messages coalesced

**Merge rules** (implemented in `MergeableMixin.merge_in_place`):
- `TextPart` + `TextPart` → single `TextPart` with concatenated text
- `ThinkPart` + `ThinkPart` → single `ThinkPart` with concatenated text
- Non-mergeable messages flush the buffer
- This prevents UI re-rendering on every single token

### 2.4 Approval Flow — Exact Sequence

```
[Tool.__call__()]
    │
    ▼
[Approval.request(sender, action, description, display)]
    │
    ├──► YOLO mode? ──► return approved immediately
    ├──► auto_approve_actions contains action? ──► return approved immediately
    │
    └──► Otherwise:
             │
             ▼
         [ApprovalRuntime.create_request(...)]
             │
             ├──► stores ApprovalRequestRecord in _requests dict
             ├──► broadcasts ApprovalRequest onto RootWireHub
             │
             ▼
         [ApprovalRuntime.wait_for_response(request_id)]
             │
             ├──► creates asyncio.Future, stores in _waiters
             ├──► awaits future (5-minute timeout)
             │
             ▼
         [UI receives ApprovalRequest via RootWireHub]
             │
             ├──► Shell: queues request, activates ApprovalPromptDelegate modal
             ├──► Wire Server: sends JSON-RPC request to client
             │
             ▼
         [User responds]
             │
             ▼
         [UI calls ApprovalRuntime.resolve(request_id, response, feedback)]
             │
             ├──► updates ApprovalRequestRecord
             ├──► sets future result
             ├──► broadcasts ApprovalResponse onto RootWireHub
             │
             ▼
         [Approval.request() returns ApprovalResult]
```

### 2.5 Session Persistence — Exact File Formats

**context.jsonl** (append-only, line-delimited JSON):
```jsonl
{"role":"_system_prompt","content":"You are Kimi..."}
{"role":"_checkpoint","id":0}
{"role":"user","content":"hello"}
{"role":"assistant","content":"Hi there"}
{"role":"_usage","token_count":42}
```

**wire.jsonl** (timestamped envelope format):
```jsonl
{"type":"metadata","protocol_version":"1.9"}
{"timestamp":1234567890.0,"message":{"type":"TurnBegin","payload":{"user_input":{"text":"hello"}}}}
```

**state.json** (atomically written Pydantic model):
```json
{
  "approval": {"yolo": false, "auto_approve_actions": ["read_file"]},
  "plan_mode": false,
  "plan_session_id": null,
  "custom_title": null,
  "archived": false,
  "todos": []
}
```

### 2.6 Subagent Data Flow

```
[Parent soul executing Agent tool]
    │
    ▼
[ForegroundSubagentRunner.run()]
    │
    ├──► Runtime.copy_for_subagent() ──► shares config, session, approval, labor_market, etc.
    ├──► Creates new DenwaRenji, subagent_id, role="subagent"
    │
    ▼
[run_soul_checked()]
    │
    ├──► Creates nested Wire
    ├──► UI loop forwards events to parent wire:
    │       ApprovalRequest/Response/ToolCallRequest/QuestionRequest ──► direct to parent
    │       Everything else ──► wrapped in SubagentEvent ──► parent wire
    │
    ▼
[Subagent soul runs its own _agent_loop]
    │
    └──► All wire events bubble up through SubagentEvent wrapper to parent UI
```

---

## 3. Current Tool Contracts — Behavioral Specification

### 3.1 Tool Loading Contract

**Input**: Agent spec declares tools as import path strings:
```yaml
tools:
  - "kimi_cli.tools.shell:Shell"
  - "kimi_cli.tools.file:ReadFile"
```

**Process**:
1. `importlib.import_module(module_name)`
2. `getattr(module, class_name)`
3. Inspect `__init__` signature up to first keyword-only parameter
4. Resolve each positional parameter's annotation from dependency registry
5. Instantiate: `tool_cls(*injected_args)`
6. Register in `_tool_dict: dict[str, ToolType]`

**Dependency Registry** (built in `load_agent()`):
```python
{
    KimiToolset: toolset,
    Runtime: runtime,
    Config: runtime.config,
    BuiltinSystemPromptArgs: runtime.builtin_args,
    Session: runtime.session,
    DenwaRenji: runtime.denwa_renji,
    Approval: runtime.approval,
    LaborMarket: runtime.labor_market,
    Environment: runtime.environment,
}
```

**Error behaviors**:
- Module not found → `ImportError` propagated up
- Class not found → `AttributeError` propagated up
- Dependency not in registry → `ValueError` with message `Tool dependency not found: {annotation}`
- Name conflict with existing tool → plugin tool skipped with warning; builtin wins

### 3.2 Tool Call Contract (Per-Call)

**Preconditions**:
- Tool is registered in `KimiToolset._tool_dict`
- LLM has emitted a `ToolCall` with matching `name`
- `current_tool_call` ContextVar is set by `KimiToolset.handle()`

**Execution sequence**:
1. Parse JSON arguments with `json.loads(..., strict=False)`
2. Fire `PreToolUse` hooks; if any return `action="block"` → return `ToolRejectedError`
3. For `CallableTool2`: Pydantic-validate dict into `Params` model
4. `await tool.call(arguments)` → delegates to `tool.__call__(validated_params)`
5. Fire-and-forget `PostToolUse` hooks on success
6. Fire-and-forget `PostToolUseFailure` hooks on exception
7. Return `ToolResult`

**Output normalization** (`tool_result_to_message` in `soul/message.py`):
- Error: prepends `<system>ERROR: {message}</system>`, appends output parts
- Success: appends optional message (as system text) and output parts
- Empty output → injects `<system>Tool output is empty.</system>`
- Only non-text parts → inserts synthetic text part for API compatibility

### 3.3 Builtin Tool Specifications

| Tool | Name | Parameters | Approval? | Max Limits | Special Behavior |
|------|------|------------|-----------|------------|----------------|
| Shell | `shell` | command, timeout, run_in_background, description | Yes (destructive) | FG: 5min, BG: 24hr | Closes stdin immediately; non-interactive env injected |
| ReadFile | `read_file` | path, line_offset, n_lines | No | 1000 lines, 100KB | Negative offset = tail mode; returns cat-n format |
| ReadMediaFile | `read_media_file` | path | No | 100MB | Skips if model lacks vision; returns data URL |
| Glob | `glob` | pattern, directory, include_dirs | No | 1000 matches | Rejects `**` prefix patterns |
| Grep | `grep` | pattern, path, glob, output_mode, head_limit, offset | No | 20s timeout | Downloads rg binary on-demand; EAGAIN retry with `-j 1` |
| WriteFile | `write_file` | path, content, mode | Yes (diff shown) | None | Overwrite or append; auto-approves plan file writes in plan mode |
| StrReplaceFile | `str_replace_file` | path, edit(s) | Yes (diff shown) | None | Single or batch edits; auto-approves plan file writes |
| SearchWeb | `search_web` | query, limit, include_content | No | None | Raises `SkipThisTool` if search service unconfigured |
| FetchURL | `fetch_url` | url | No | None | Primary: moonshot service; fallback: direct HTTP + trafilatura |
| Think | `think` | thought | No | None | No-op; logs thought and returns success |
| SetTodoList | `set_todo_list` | todos | No | None | Persists to session state (root) or subagent state.json |
| Agent | `agent` | description, prompt, subagent_type, model, resume, run_in_background, timeout | No | None | Blocks nested subagents; validates model alias |
| TaskList | `task_list` | active_only, limit | No | None | Lists background tasks |
| TaskOutput | `task_output` | task_id, block, timeout | No | None | Gets output; can block until completion |
| TaskStop | `task_stop` | task_id, reason | Yes | None | Kills background task; blocked in plan mode |
| AskUserQuestion | `ask_user_question` | questions | No | None | Auto-dismisses in yolo mode; sends QuestionRequest over wire |
| SendDMail | `send_dmail` | checkpoint_id, messages | No | None | Raises `BackToTheFuture` in parent soul |
| EnterPlanMode | `enter_plan_mode` | — | No | None | Requests user confirmation; late-bound callbacks |
| ExitPlanMode | `exit_plan_mode` | options | No | None | Presents plan for approval/rejection/revision |

### 3.4 MCP Tool Contract

**Loading**:
- Config from `~/.kimi/mcp.json`
- `fastmcp.Client` per server
- Background task `_connect()` iterates pending servers, connects, lists tools, wraps as `MCPTool`

**Execution**:
1. Approval with action name `mcp:{tool_name}`
2. `client.call_tool(name, arguments, timeout=..., raise_on_error=False)`
3. `convert_mcp_tool_result()`:
   - Maps MCP content blocks to kosong `ContentPart` types
   - Truncates text to `MCP_MAX_OUTPUT_CHARS` (100K)
   - Drops oversized media parts
   - Unsupported types → `[Unsupported content: ...]` placeholder
   - `is_error=True` → `ToolError`

### 3.5 Plugin Tool Contract

**Install-time**:
- Parse `plugin.json` → `PluginSpec`
- Apply `inject` map: resolve host credentials via `OAuthManager`, write to plugin config
- Atomic staging + swap into `~/.kimi/plugins/<name>/`

**Runtime**:
- `PluginTool` executes declared command in subprocess
- Parameters passed via stdin as JSON
- Host credentials injected as environment variables (fresh each call for OAuth rotation)
- Name conflicts → plugin tool skipped with warning

---

## 4. Current System Designs — Behavioral Specification

### 4.1 Configuration System

**Resolution precedence** (highest to lowest):
1. CLI flags (`--model`, `--yolo`)
2. Environment variables (`KIMI_BASE_URL`, `KIMI_API_KEY`, etc.)
3. Inline config string (`--config`)
4. Config file (`~/.kimi/config.toml` or `.json`)
5. Default config

**Validation**:
- `model_validator(mode="after")` ensures `default_model` exists in `models` dict
- Every model references a valid provider
- `SecretStr` for API keys; serialized only on explicit dump

**OAuth indirection**:
- `LLMProvider` stores `OAuthRef` (key + storage backend)
- Tokens in `~/.kimi/credentials/<key>.json` with `0o600` permissions
- Keyring storage deprecated, migrated to file on load

### 4.2 LLM Abstraction

**Provider types**: `kimi`, `openai_legacy`, `openai_responses`, `anthropic`, `gemini`, `vertexai`, `_echo`, `_scripted_echo`, `_chaos`

**Capability model**:
- Config-declared capabilities
- Model name heuristics (e.g., "thinking" in name)
- Hardcoded aliases for `kimi-for-coding`
- `Literal["image_in", "video_in", "thinking", "always_thinking"]`

**Thinking control**:
- `.with_thinking("high")` or `"off"` based on capability + preference
- "always_thinking" models force thinking on

**Session affinity**:
- `prompt_cache_key` for Kimi
- `metadata.user_id` for Anthropic

### 4.3 Background Task System

**Task kinds**: `bash`, `agent`
**Max running**: 4 (combined)
**Root-only**: Subagents get copied manager with `owner_role="subagent"` that raises on creation

**Bash task isolation**:
- `subprocess.Popen` with `start_new_session=True` (Unix) or `CREATE_NEW_PROCESS_GROUP` (Windows)
- Separate `__background-task-worker` CLI entrypoint
- Heartbeats to `runtime.json`
- Stdout/stderr to `output.log`
- Control via `control.json` (SIGTERM → 5s grace → SIGKILL)

**Agent tasks**:
- In-process `asyncio.create_task(BackgroundAgentRunner.run())`
- Share parent runtime
- Write to subagent store output files

**Recovery**:
- `recover()` scans tasks on startup
- Stale bash (no heartbeat > threshold) → `lost` or `killed`
- Stale agent (asyncio task gone) → `lost`
- `reconcile()` publishes terminal notifications

**Approval integration**:
- Background agent tasks subscribe to `ApprovalRuntime` events
- Status transitions: `running` → `awaiting_approval` → `running`

### 4.4 Notification System

**Event model**:
- `NotificationEvent`: category (`task`/`agent`/`system`), type, severity, payload, targets, `dedupe_key`
- `NotificationDelivery`: per-sink state machine `pending` → `claimed` → `acked`

**Manager behavior**:
- `publish()` writes atomically; dedupes by `dedupe_key`
- `claim_for_sink()` moves pending to claimed with timestamp
- `recover()` reclaims stale claims (> `claim_stale_after_ms`)
- `deliver_pending()` with error isolation

**Sinks**: `llm`, `wire`, `shell`
**Store**: Directory-based, one subdir per notification (`event.json` + `delivery.json`)

### 4.5 Authentication / Security

**OAuth device flow**:
- Device authorization against `https://auth.kimi.com`
- Cross-process token refresh via `fcntl.flock` (Unix) / `msvcrt.locking` (Windows)
- Triple-check after lock acquisition to avoid redundant refresh
- Background refresh with sleep/wake detection
- 401/403 → delete tokens, clear runtime API key

**Platform abstraction**:
- `kimi-code`, `moonshot-cn`, `moonshot-ai` platforms
- Managed providers use `managed:kimi-code` keys
- `refresh_managed_models()` fetches model lists atomically

### 4.6 Compaction Design

**Trigger**: `token_count >= max_context_size * 0.85` OR `token_count + 50K >= max_context_size`

**SimpleCompaction behavior**:
1. Preserve last 2 user/assistant message pairs (4 messages total)
2. Send all earlier messages to LLM for summarization
3. Replace compacted messages with single summary message
4. Write new context file atomically (rotate old to backup)
5. Emit `CompactionBegin/CompactionEnd`

**Token estimation**: `chars // 4` (conservative heuristic)

### 4.7 Checkpoint / D-Mail Design

**Checkpoint**:
- Monotonically increasing integer ID
- Written as `{"role":"_checkpoint","id":N}` to context.jsonl
- `_checkpoint()` called before user message append and before each step

**Revert**:
- `revert_to(checkpoint_id)` rotates file to numbered backup
- Re-reads original file up to target checkpoint
- Rewrites only kept lines to new file
- Rebuilds `_history`, `_token_count`, `_next_checkpoint_id`

**D-Mail (DenwaRenji)**:
- Tool or background agent can "send message to past checkpoint"
- Stores pending D-Mail with target checkpoint ID and messages
- Parent soul detects pending D-Mail after `_grow_context()`
- Raises `BackToTheFuture(checkpoint_id, messages)`
- Caught in `_agent_loop()`, triggers `revert_to()` + injects messages + continues

### 4.8 Hook System Design

**Event types**: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure`, `PreCompact`, `PostCompact`, `Notification`

**Hook kinds**:
- **Server-side**: Shell commands from config; executed via subprocess
- **Client-side**: Wire subscriptions registered by UI clients

**Execution semantics**:
- Hooks run in parallel (`asyncio.gather`)
- Aggregate decision: `block` if ANY hook returns `block`
- Timeouts and errors default to `allow` (fail-open)

---

## 5. Proposed Architectural Deviations

### 5.1 From Monolithic Runtime to Capability-Based Services

**Current problem**: `Runtime` is a god-object holding 15+ fields. It is passed everywhere and copied for subagents. This creates tight coupling and makes unit testing require massive fixture construction.

**Deviation**: Decompose `Runtime` into a **capability registry** pattern.

```python
# Proposed
class CapabilityRegistry:
    """Lazy, typed capability resolution."""
    _capabilities: dict[type, Callable[[], Any]]
    
    def get[T](self, cap_type: type[T]) -> T:
        ...
    
    def fork(self, overrides: dict[type, Callable[[], Any]]) -> "CapabilityRegistry":
        """Subagent gets forked registry with selective overrides."""
        ...
```

**Behavioral changes**:
- Tools declare dependencies as `Annotated[SomeService, Capability]` instead of constructor parameters
- `CapabilityRegistry` resolves on first use, enabling lazy initialization
- Subagent forking becomes explicit: only capabilities listed in the agent spec's `capabilities` field are inherited
- Test fixtures become minimal: register only the capabilities the test subject needs

**Impact**: Breaks all tool constructors. Requires migration from positional DI to annotated/declared DI.

### 5.2 From Per-Turn Wire to Persistent Session Stream

**Current problem**: A fresh `Wire` is created per turn. This means:
- UI state must be reconstructed every turn
- Subagent events crossing turn boundaries must go through `RootWireHub`
- Replay requires reading `wire.jsonl` from disk
- There is no unified "session event log" in memory

**Deviation**: Replace per-turn `Wire` with a **persistent session-scoped event log** (`SessionStream`).

```python
# Proposed
class SessionStream:
    """Append-only session event bus with persistent log and subscriber management."""
    _log: list[StreamEvent]
    _subscribers: WeakSet[StreamSubscriber]
    _cursor: int  # monotonic write cursor
    
    async def publish(self, event: StreamEvent) -> None:
        self._log.append(event)
        await asyncio.gather(*[s.on_event(event) for s in self._subscribers])
    
    def subscribe(self, from_cursor: int = -1) -> StreamSubscription:
        """Returns async iterator from cursor. -1 means from end (live)."""
        ...
```

**Behavioral changes**:
- UIs subscribe once at session start and stay subscribed across turns
- No more `RootWireHub` vs `Wire` distinction — one unified stream
- Subagent events are just events in the same stream with `source_id` metadata
- Replay is instant: `subscription = stream.subscribe(from_cursor=0)`
- Turn boundaries become `TurnBeginEvent` / `TurnEndEvent` in the stream, not stream lifecycle boundaries
- Compaction events are visible in replay

**Impact**: Eliminates `run_soul()` wire creation. Changes all UI loop contracts. Requires new backpressure strategy.

### 5.3 From ContextVar Propagation to Explicit Context Token

**Current problem**: `current_wire`, `current_session_id`, `current_approval_source`, `current_tool_call` are ContextVars. This makes data flow implicit and debugging difficult. It also breaks if code crosses task boundaries unexpectedly.

**Deviation**: Replace all ContextVars with an **explicit `ContextToken`** passed through every call boundary.

```python
# Proposed
@dataclass(frozen=True)
class ContextToken:
    session_id: str
    turn_id: str
    step_id: str
    tool_call_id: str | None
    approval_source: ApprovalSource
    stream: SessionStream  # reference to current stream
    
    def child(self, **overrides) -> "ContextToken":
        return replace(self, **overrides)
```

**Behavioral changes**:
- Every async function in the soul receives `ctx: ContextToken` as first parameter
- `wire_send()` becomes `ctx.stream.publish(event)`
- Tool calls receive `ctx` and pass it to nested operations
- Approval requests include `ctx` for source tracking and cancellation scoping
- Stack traces are explicit: you can see the token chain

**Impact**: Massive signature change across soul, tools, and UI. But eliminates an entire class of "where did this wire message come from?" bugs.

### 5.4 From Agent Loop inside Soul to External Orchestrator

**Current problem**: `KimiSoul` contains both the agent state (context, system prompt) AND the loop logic (`_agent_loop`, `_step`, compaction, D-Mail). This violates single responsibility.

**Deviation**: Extract the loop into a **stateless `TurnOrchestrator`** protocol.

```python
# Proposed
class TurnOrchestrator(Protocol):
    async def execute_turn(
        self,
        agent: AgentState,
        user_input: UserInput,
        ctx: ContextToken,
    ) -> TurnResult:
        ...

class ReActOrchestrator:
    """Default implementation: step loop with compaction, D-Mail, steers."""
    _compaction_policy: CompactionPolicy
    _step_limit: int
    _retry_policy: RetryPolicy
    
    async def execute_turn(self, agent, user_input, ctx) -> TurnResult:
        # All loop logic here, stateless
        ...

class PlanModeOrchestrator:
    """Read-only research mode: single step, no tools, no compaction."""
    ...

class RalphOrchestrator:
    """Automated iteration: runs ReActOrchestrator in a loop with decision gate."""
    ...
```

**Behavioral changes**:
- `KimiSoul` becomes a thin wrapper holding `AgentState` and delegating to `TurnOrchestrator`
- Orchestrators are composable: `RalphOrchestrator` wraps `ReActOrchestrator`
- Plan mode is just a different orchestrator, not a boolean flag
- Testability: orchestrators can be unit-tested with mock `AgentState`
- Slash commands can switch orchestrators mid-session

**Impact**: Major refactoring of `kimisoul.py`. But enables experimentation with new loop strategies (e.g., tree-of-thought, reflection loops) without touching soul state.

### 5.5 From File-Based Context to Differential Context Tree

**Current problem**: `Context` is a flat JSONL file. Compaction requires rewriting the entire file. Checkpoints require file rotation. History is a linear list.

**Deviation**: Replace with a **differential context tree** backed by an immutable log.

```python
# Proposed
class ContextNode:
    """Immutable node in context tree."""
    id: str
    parent_id: str | None
    messages: tuple[Message, ...]
    token_count: int
    checkpoint: bool
    compacted: bool
    
class ContextTree:
    """Persistent tree with lazy branch evaluation."""
    _nodes: dict[str, ContextNode]
    _head: str  # current node id
    
    def append(self, messages: list[Message]) -> ContextNode:
        """Creates new node, returns it. Old nodes unchanged."""
        ...
    
    def branch(self, from_node_id: str) -> "ContextBranchView":
        """For speculative execution (subagents, planning)."""
        ...
    
    def compact(self, strategy: CompactionStrategy) -> ContextNode:
        """Creates compacted successor node. Original preserved for undo."""
        ...
    
    def linearize(self, from_node_id: str | None = None) -> list[Message]:
        """Flattens path from root to head (or given node) for LLM consumption."""
        ...
```

**Behavioral changes**:
- Compaction creates a new branch; original history is preserved for undo/debugging
- D-Mail becomes a branch operation: revert to checkpoint node, create new branch with injected messages
- Subagents can speculate on branches without affecting parent context
- Token counting is cached per node; linearization is lazy
- Persistence writes nodes as delta records, not full file rewrites

**Impact**: Complete rewrite of `context.py`. But enables powerful features: context diff, branch comparison, speculative subagents.

---

## 6. Proposed Data Flow Deviations

### 6.1 From Streaming Callbacks to Pull-Based Generation

**Current problem**: `kosong.step()` takes `on_message_part=wire_send` callback. This inverts control: the LLM framework pushes tokens to the soul, which pushes to the wire. This creates callback hell and makes backpressure difficult.

**Deviation**: Invert to **pull-based generation** using async iterators.

```python
# Proposed
class LLMGeneration:
    """Pull-based LLM response stream."""
    async def chunks(self) -> AsyncIterator[ContentChunk]:
        ...
    
    async def tool_calls(self) -> list[ToolCall]:
        """Available after chunks() is exhausted."""
        ...
    
    async def usage(self) -> TokenUsage:
        """Available after chunks() is exhausted."""
        ...

class TurnOrchestrator:
    async def execute_turn(self, agent, user_input, ctx):
        generation = await self.llm.generate(agent, user_input)
        
        async for chunk in generation.chunks():
            await ctx.stream.publish(TextPartEvent(chunk.text))
            # Backpressure: if stream subscribers are slow, this naturally slows down
        
        tool_calls = await generation.tool_calls()
        if tool_calls:
            results = await self.execute_tools(tool_calls, ctx)
            ...
```

**Behavioral changes**:
- No more callbacks. The orchestrator is in full control of pacing.
- Backpressure is natural: slow UI slows down token consumption from LLM.
- Token counting can be done incrementally without final callback.
- Testing: mock `LLMGeneration` with list iterator.

**Impact**: Requires changes to kosong interface or a wrapper layer. Changes all streaming paths in the soul.

### 6.2 From Fire-and-Forget Hooks to Structured Side Effects

**Current problem**: `PostToolUse` and `PostToolUseFailure` hooks are fire-and-forget `asyncio.create_task()` calls. The soul does not wait for them, and their results are ignored. This makes hooks unreliable for critical side effects (e.g., audit logging, metric collection).

**Deviation**: Make hooks **structured, awaited side effects** with explicit ordering.

```python
# Proposed
class SideEffectEngine:
    """Ordered, awaited side effects with error boundaries."""
    _stages: list[list[SideEffect]]
    
    async def run(self, event: str, payload: dict, ctx: ContextToken) -> SideEffectResult:
        for stage in self._stages:
            results = await asyncio.gather(
                *[fx.execute(payload, ctx) for fx in stage],
                return_exceptions=True
            )
            for fx, result in zip(stage, results):
                if isinstance(result, Exception):
                    if fx.critical:
                        raise SideEffectError(fx.name, result)
                    logger.warning(f"Non-critical side effect {fx.name} failed: {result}")
```

**Behavioral changes**:
- Hooks are organized into ordered stages (e.g., `pre_validate`, `pre_execute`, `post_execute`, `audit`)
- Each stage completes before next begins
- Side effects declare if they are `critical` (failure stops turn) or `best_effort` (failure logged)
- `PreToolUse` becomes a `pre_execute` stage side effect
- `PostToolUse` becomes a `post_execute` stage side effect
- `Notification` generation becomes an `audit` stage side effect

**Impact**: Replaces `HookEngine`. Changes hook config schema. Makes hook failures visible.

### 6.3 From ApprovalRuntime Broadcast to Request-Response Channel

**Current problem**: Approval requests are broadcast on `RootWireHub`. Any subscriber can resolve them. There is no ownership model. This makes multi-UI scenarios racy.

**Deviation**: Model approvals as **named request-response channels** with explicit ownership.

```python
# Proposed
class ApprovalChannel:
    """Named channel for a specific approval sink."""
    name: str
    priority: int
    
    async def request(self, req: ApprovalRequest) -> ApprovalResponse:
        ...

class ApprovalRouter:
    """Routes approval requests to highest-priority available channel."""
    _channels: dict[str, ApprovalChannel]
    
    async def request(self, req: ApprovalRequest) -> ApprovalResponse:
        available = [c for c in self._channels.values() if c.is_available()]
        if not available:
            raise NoApprovalChannelAvailable()
        channel = max(available, key=lambda c: c.priority)
        return await channel.request(req)
```

**Behavioral changes**:
- Shell UI registers `ApprovalChannel(name="shell", priority=100)`
- Wire server registers `ApprovalChannel(name="wire", priority=50)`
- Background agents register `ApprovalChannel(name="background", priority=10)`
- When shell is active, it gets approvals. When running headless, wire gets them.
- Channels declare `is_available()` — shell returns False when not in interactive mode
- Request timeouts are per-channel, not global

**Impact**: Replaces `ApprovalRuntime` broadcast model. Requires channel registration protocol.

### 6.4 From ToolResult Wrapping to Native Content Parts

**Current problem**: Tool results go through multiple wrapping layers:
1. Tool returns `ToolReturnValue`
2. `KimiToolset.handle()` returns `HandleResult` (async task)
3. kosong executes and returns `ToolResult`
4. `tool_result_to_message()` converts to `Message(role="tool")`
5. `Context.append_message()` writes to JSONL
6. On restore, `Message` is reconstructed from JSONL

This creates impedance mismatch: tool outputs can be `str | list[ContentPart]`, but context stores everything as `Message` with `content` field.

**Deviation**: Unify on **native content parts throughout**.

```python
# Proposed
class ToolEvent(StreamEvent):
    tool_call_id: str
    tool_name: str
    status: "started" | "completed" | "failed"
    content_parts: list[ContentPart]
    metadata: dict  # execution time, exit code, etc.

class ContextEntry:
    """Any entry in context: message, tool event, system prompt, checkpoint."""
    type: Literal["message", "tool_event", "system_prompt", "checkpoint", "compaction"]
    payload: Message | ToolEvent | SystemPromptEvent | CheckpointEvent | CompactionEvent
```

**Behavioral changes**:
- Tool results are `ToolEvent` entries in context, not `Message(role="tool")`
- LLM history construction filters and transforms entries: `ToolEvent` → `Message(role="tool")` at the boundary
- Media parts (images, videos) stay as typed objects in context, not serialized to strings
- Context replay can show rich tool output without re-execution
- Token counting is entry-type aware

**Impact**: Changes context schema. Changes tool result contract. Changes LLM boundary layer.

### 6.5 From Subagent Wire Forwarding to Event Sourcing

**Current problem**: Subagent events are wrapped in `SubagentEvent` and forwarded to parent wire. The parent UI must unwrap and render. This creates tight coupling between parent and subagent event types.

**Deviation**: Use **event sourcing with provenance metadata**.

```python
# Proposed
class StreamEvent:
    event_id: str
    timestamp: datetime
    source: EventSource  # root soul, subagent, background task, external
    payload: TextPartEvent | ToolEvent | ApprovalRequestEvent | ...

class EventSource:
    type: Literal["root", "subagent", "background_task", "external"]
    agent_id: str | None
    task_id: str | None
    parent_event_id: str | None  # for subagent tool calls
```

**Behavioral changes**:
- All events in the session stream carry `EventSource` metadata
- Subagent events are NOT wrapped; they are native events with `source.type="subagent"`
- UIs filter by source: "show only root events", "show events from subagent X"
- Parent tool call block references subagent events by `parent_event_id`
- No special `SubagentEvent` type needed

**Impact**: Eliminates `SubagentEvent`. Changes all event consumers to check `source` field.

---

## 7. Proposed Tool Contract Deviations

### 7.1 From Import-Path Tool Loading to Plugin Registry with Manifests

**Current problem**: Tools are loaded by import path string (`"kimi_cli.tools.shell:Shell"`). This requires Python import machinery, prevents dynamic tool discovery, and ties tools to Python classes.

**Deviation**: Move to a **declarative tool registry** with manifest files.

```yaml
# ~/.kimi/tools/shell/manifest.yaml
name: shell
version: 1.0.0
description: Execute shell commands
entry:
  type: python_class
  module: kimi_cli.tools.shell
  class: Shell
  # OR:
  # type: subprocess
  # command: ["kimi-tool-shell"]
  # OR:
  # type: wasm
  # module: shell.wasm
parameters:
  command:
    type: string
    description: The command to execute
    required: true
  timeout:
    type: integer
    default: 300
approval:
  required: true
  action: run_command
  diff: false
sandbox:
  network: false
  filesystem: read_write
  max_memory_mb: 512
```

**Behavioral changes**:
- Tools are discovered by scanning `~/.kimi/tools/` and `.kimi/tools/` directories
- Each tool has a `manifest.yaml` declaring its interface, sandbox policy, and approval rules
- Built-in tools ship as manifests inside the package
- MCP tools generate manifests dynamically from MCP tool schemas
- Plugin tools use `entry.type: subprocess` with stdin/stdout JSON protocol
- Tools can declare sandbox constraints (network, filesystem, memory)

**Impact**: New tool discovery mechanism. New manifest schema. Backward compatibility layer for existing import-path specs.

### 7.2 From Class-Based Tools to Stateless Function Tools

**Current problem**: All tools are classes with `__init__` (for dependency injection) and `__call__`. This creates stateful tool instances that are hard to serialize and reason about.

**Deviation**: Tools are **pure async functions** with dependency injection via parameter annotations.

```python
# Proposed
from kimi_cli.toolkit import tool, ToolContext

@tool(
    name="shell",
    description="Execute shell commands",
    approval_action="run_command",
)
async def shell_tool(
    command: str,
    timeout: int = 300,
    ctx: ToolContext,  # injected by toolkit
) -> ToolOutput:
    env = ctx.environment
    approval = ctx.approval
    
    await approval.request(action="run_command", description=command)
    
    result = await env.execute(command, timeout=timeout)
    return ToolOutput(
        text=result.stdout,
        exit_code=result.exit_code,
    )
```

**Behavioral changes**:
- No classes. No `__init__`. No constructor introspection.
- `ToolContext` is injected as a special parameter (like `fastapi.Request`)
- Parameters are declared as function signatures; schema generated from type hints
- Tools are registered by decorating functions, not by listing import paths
- Agent specs reference tools by name, not by import path
- Tools can be stateless and trivially serializable

**Impact**: Complete rewrite of all builtin tools. But drastically simplifies tool authoring.

### 7.3 From String/ContentPart Output to Structured Tool Output

**Current problem**: Tool output is `str | list[ContentPart]`. This is ambiguous: is the string an error message? Is it user-facing or model-facing? The `ToolReturnValue` has `output`, `message`, `display`, `extras` fields but they are loosely typed.

**Deviation**: Enforce **structured tool output** with typed result variants.

```python
# Proposed
class ToolOutput(BaseModel):
    """Structured output from any tool."""
    result: ToolResult  # success | error | partial | skipped
    artifacts: list[Artifact]  # files, images, references produced
    metrics: ToolMetrics  # timing, memory, tokens consumed
    
class ToolResult(BaseModel):
    type: Literal["success", "error", "partial", "skipped"]
    content: list[ContentBlock]  # typed content blocks
    summary: str  # one-line summary for compact display
    
class ContentBlock(BaseModel):
    type: Literal["text", "code", "image", "diff", "table", "traceback"]
    ...
```

**Behavioral changes**:
- `diff` output from `WriteFile`/`StrReplaceFile` is a `ContentBlock(type="diff", ...)`
- Shell output with exit code != 0 is `ToolResult(type="error", content=[...], summary="Command failed with exit code 1")`
- Code output from tools is `ContentBlock(type="code", language="python", code="...")`
- Tables from search results are `ContentBlock(type="table", headers=[...], rows=[...])`
- UI renders each block with appropriate renderer
- LLM boundary converts structured blocks to text with markdown formatting

**Impact**: Changes all tool return types. Changes `tool_result_to_message`. Changes UI rendering.

### 7.4 From Global Approval to Capability-Based Authorization

**Current problem**: Approval is action-based (`run_command`, `write_file`, `mcp:{name}`). It does not consider the tool's actual capabilities or the user's trust level for specific capabilities.

**Deviation**: Move to **capability-based authorization**.

```yaml
# In tool manifest
capabilities:
  - filesystem:write
  - process:exec
  - network:outbound

# In user config
trust_profile:
  default: prompt  # prompt, auto, block
  overrides:
    - capability: filesystem:write
      path: "~/Projects/**"
      decision: auto
    - capability: process:exec
      command_pattern: "^git "
      decision: auto
    - capability: network:outbound
      host_pattern: "^api\.github\.com$"
      decision: auto
```

**Behavioral changes**:
- Tools declare capabilities they require, not action names
- User config defines trust profiles based on capability + constraints (path, command pattern, host)
- Approval system matches tool call against trust profile; if no match → prompt
- Granular auto-approve: "auto-approve all git commands" instead of "auto-approve all shell commands"
- Subagents can be restricted to subsets of capabilities
- Audit log records which capabilities were exercised

**Impact**: New approval schema. New config fields. Replaces `auto_approve_actions`.

### 7.5 From MCP Tool Wrapping to Native MCP Integration

**Current problem**: MCP tools are wrapped in `MCPTool` class with manual result conversion, truncation, and error handling. This adds a translation layer that loses MCP-native features (e.g., progress notifications, resource subscriptions, sampling).

**Deviation**: First-class **MCP session integration**.

```python
# Proposed
class MCPSession:
    """Native MCP client session with full protocol support."""
    async def list_tools(self) -> list[MCPToolRef]
    async def call_tool(self, name, arguments) -> MCPResult
    async def subscribe_resource(self, uri) -> AsyncIterator[ResourceUpdate]
    async def request_sampling(self, messages) -> SamplingResult
    
    # Events published to session stream
    async def events(self) -> AsyncIterator[MCPEvent]
```

**Behavioral changes**:
- MCP servers maintain persistent sessions, not per-call connections
- MCP progress notifications are forwarded as `StatusUpdate` events
- MCP resource subscriptions create background streaming into context
- MCP sampling requests are routed to the LLM and responses fed back
- MCP tools are NOT wrapped; they are invoked through `MCPSession` directly
- MCP tool schemas are used natively without title stripping or dereferencing

**Impact**: Replaces `MCPTool` and `convert_mcp_tool_result`. Requires `fastmcp` session management.

---

## 8. Proposed System Design Deviations

### 8.1 From Pydantic Config to Typed Config with Validation Rules

**Current problem**: Config is a single large Pydantic model. Cross-field validation is done in `model_validator`. This makes it hard to add modular config sections or plugin-specific configs.

**Deviation**: **Plugin-extensible config with validation rules**.

```python
# Proposed
class ConfigRegistry:
    _schemas: dict[str, ConfigSchema]
    _validators: list[ConfigValidator]
    
    def register(self, section: str, schema: ConfigSchema) -> None:
        ...
    
    def validate(self, raw: dict) -> ValidatedConfig:
        for section, schema in self._schemas.items():
            schema.validate(raw.get(section, {}))
        for validator in self._validators:
            validator(raw)
        return ValidatedConfig(raw)

# Usage
config_registry.register("loop_control", LoopControlSchema)
config_registry.register("mcp", MCPConfigSchema)
config_registry.register("hooks", HooksSchema)
# Plugins can register their own sections
```

**Behavioral changes**:
- Config sections are independently validated
- Plugins register their own config schemas at load time
- Cross-section validators run after per-section validation
- Config migration is schema-version aware
- Secrets use dedicated `SecretStorage` abstraction, not `SecretStr`

**Impact**: New config loading path. Backward compatible with existing TOML.

### 8.2 From OAuth Manager to Pluggable Identity Layer

**Current problem**: OAuth is hardcoded for `auth.kimi.com` device flow. Token refresh uses platform-specific file locking. This does not support other auth methods (API keys, SSO, mutual TLS).

**Deviation**: **Pluggable identity layer** with credential providers.

```python
# Proposed
class IdentityProvider(Protocol):
    name: str
    async def authenticate(self) -> Credential
    async def refresh(self, credential: Credential) -> Credential
    async def revoke(self, credential: Credential) -> None

class CredentialStore(Protocol):
    async def get(self, key: str) -> Credential | None
    async def set(self, key: str, credential: Credential) -> None
    async def delete(self, key: str) -> None

# Implementations
class FileCredentialStore: ...
class KeyringCredentialStore: ...
class EnvCredentialStore: ...  # reads from env vars, never persists

class KimiOAuthProvider(IdentityProvider): ...
class ApiKeyProvider(IdentityProvider): ...
class SamlProvider(IdentityProvider): ...
```

**Behavioral changes**:
- Provider config references identity provider by name, not OAuth-specific fields
- `CredentialStore` is pluggable; `EnvCredentialStore` enables CI/CD use cases
- Cross-process refresh uses atomic file operations, not flock (portable, works on network filesystems)
- Token refresh is triggered by `IdentityProvider`, not centralized OAuthManager
- Audit log records all auth events

**Impact**: Replaces `OAuthManager` and `auth/` module. New credential storage abstraction.

### 8.3 From Background Task Manager to Distributed Task Queue

**Current problem**: Background tasks are local to the CLI process. If the CLI exits, bash tasks continue as orphans but agent tasks die. There is no queue semantics, retries, or task dependencies.

**Deviation**: **Local-first distributed task queue**.

```python
# Proposed
class TaskQueue:
    """Durable task queue with execution guarantees."""
    async def submit(self, spec: TaskSpec) -> TaskRef
    async def status(self, ref: TaskRef) -> TaskStatus
    async def cancel(self, ref: TaskRef) -> None
    async def results(self, ref: TaskRef) -> AsyncIterator[TaskEvent]
    
class TaskExecutor(Protocol):
    async def can_execute(self, spec: TaskSpec) -> bool
    async def execute(self, spec: TaskSpec, queue: TaskQueue) -> None

# Executors
class BashExecutor: ...  # subprocess isolation
class AgentExecutor: ...  # in-process asyncio
class RemoteExecutor: ...  # SSH / container execution
```

**Behavioral changes**:
- Tasks are submitted to a durable queue (SQLite or filesystem-backed)
- Queue survives process restarts; tasks resume on next CLI startup
- Executors declare `can_execute()`; queue routes to first available
- Agent tasks are checkpointed: if CLI restarts, agent resumes from last checkpoint
- Task dependencies: `spec.dependencies = [ref1, ref2]` — task starts only after deps complete
- Task results are streamable: `results()` yields events as they happen
- Max concurrency enforced per-executor, not globally

**Impact**: Replaces `BackgroundTaskManager`. New task persistence model.

### 8.4 From Directory-Based Notification Store to Stream-Based Notifications

**Current problem**: Notifications are stored as one directory per notification with `event.json` + `delivery.json`. This is I/O heavy and does not scale.

**Deviation**: **Stream-based notification log** with consumer groups.

```python
# Proposed
class NotificationLog:
    """Append-only notification stream with consumer offset tracking."""
    async def publish(self, event: NotificationEvent) -> NotificationRef
    async def subscribe(self, consumer_id: str, from_offset: int | None = None) -> AsyncIterator[NotificationEvent]
    async def ack(self, consumer_id: str, offset: int) -> None
    async def claim(self, consumer_id: str, timeout_ms: int) -> list[NotificationEvent]
```

**Behavioral changes**:
- Notifications are appended to a single JSONL stream (or SQLite WAL)
- Consumers (shell UI, LLM context, wire) track their own offsets
- Deduplication is done at publish time via bloom filter, not disk scan
- Claim/ack pattern enables exactly-once delivery to LLM context
- Stale claim recovery is automatic: unacked messages are redelivered
- No more one-directory-per-notification I/O explosion

**Impact**: Replaces `NotificationManager` store. New consumer offset persistence.

### 8.5 From Simple Compaction to Hierarchical Memory Management

**Current problem**: Compaction is a single strategy (`SimpleCompaction`) that preserves last 2 exchanges and summarizes the rest. This loses fine-grained information and does not support long-term memory.

**Deviation**: **Hierarchical memory management** with multiple storage tiers.

```python
# Proposed
class MemoryHierarchy:
    """Multi-tier conversation memory."""
    working: WorkingMemory      # Full messages, last ~20 turns
    episodic: EpisodicMemory    # Summarized episodes, last ~100 turns
    semantic: SemanticMemory    # Vector-indexed facts and code references
    
    async def recall(self, query: str, limit: int = 10) -> list[MemoryFragment]:
        """Retrieves relevant fragments from all tiers."""
        ...
    
    async def compact(self) -> None:
        """Moves old working memory to episodic, old episodic to semantic."""
        ...
```

**Behavioral changes**:
- Working memory: exact messages, high fidelity, limited size
- Episodic memory: LLM-generated episode summaries ("We refactored the auth module to use OAuth2")
- Semantic memory: vector embeddings of key facts, code snippets, decisions
- On context overflow, working memory compacts into episodic; episodic into semantic
- LLM history construction: working + relevant episodic/semantic fragments (RAG-style)
- Subagents share semantic memory but have isolated working/episodic memory
- Session resume loads all three tiers

**Impact**: Replaces `Compaction` protocol and `SimpleCompaction`. Requires embedding model and vector store.

### 8.6 From File-Based Session to Database-Backed Session with Replication

**Current problem**: Session state is scattered across `context.jsonl`, `wire.jsonl`, `state.json`, and subdirectories. Atomicity is achieved via file rotation. Cross-session queries are impossible.

**Deviation**: **SQLite-backed session store** with JSONB columns.

```python
# Proposed
class SessionStore:
    """SQLite-backed session storage with ACID semantics."""
    async def create_session(self, work_dir: str) -> Session
    async def append_event(self, session_id: str, event: StreamEvent) -> None
    async def get_history(self, session_id: str, from_cursor: int = 0) -> list[StreamEvent]
    async def search_sessions(self, query: str) -> list[SessionSummary]
    async def fork_session(self, session_id: str, at_cursor: int) -> Session
```

**Behavioral changes**:
- All session data in single SQLite file: `~/.kimi/sessions.db`
- Events are inserted in a transaction with session metadata
- Context, wire, and state are views over the event log, not separate files
- Forking a session is a metadata operation: new session references parent cursor
- Cross-session search: `SELECT DISTINCT session_id FROM events WHERE content LIKE ?`
- Backup/sync: single file to copy or sync via SQLite replication
- Subagent data is in the same database with `parent_session_id` reference

**Impact**: Replaces all file-based session persistence. Migration script required.

### 8.7 From Sync-Config-Once to Hot-Reloading Config

**Current problem**: Config is loaded once at startup. Changes require CLI restart. Session state is separate from config.

**Deviation**: **Hot-reloading config with change propagation**.

```python
# Proposed
class ConfigWatcher:
    """Watches config files and propagates changes to runtime."""
    _subscribers: dict[str, list[Callable]]
    
    async def watch(self) -> None:
        ...
    
    def subscribe(self, section: str, callback: Callable) -> None:
        ...
```

**Behavioral changes**:
- `ConfigWatcher` uses `watchdog` or `fsevents` to monitor config files
- Changes trigger validation and selective subscriber notification
- Model/provider changes: next turn uses new LLM
- Hook changes: next turn uses new hooks
- Trust profile changes: apply to pending approvals immediately
- MCP server changes: reconnect/reload without restart
- UI shows config change notifications

**Impact**: New config loading path. Requires file watching dependency.

---

## 9. Migration Path & Risk Assessment

### 9.1 Phased Migration Strategy

| Phase | Focus | Duration | Risk |
|-------|-------|----------|------|
| 1 | **ContextToken + explicit propagation** | 2 weeks | Medium — signature changes across soul and tools |
| 2 | **SessionStream (unified event bus)** | 2 weeks | High — replaces Wire and RootWireHub |
| 3 | **TurnOrchestrator extraction** | 1 week | Low — internal refactoring, no external contract changes |
| 4 | **Tool manifests + function tools** | 3 weeks | High — all tools rewritten, agent specs change |
| 5 | **Capability-based approval** | 1 week | Medium — config migration, new approval UI |
| 6 | **SQLite session store** | 2 weeks | High — data migration, backup strategy |
| 7 | **Memory hierarchy** | 3 weeks | Medium — new dependency (embeddings), subtle behavior changes |
| 8 | **Distributed task queue** | 2 weeks | Medium — replaces background system |

### 9.2 Backward Compatibility Requirements

- Agent specs using import-path tool loading must continue to work (shim layer)
- Existing sessions (`context.jsonl`, `wire.jsonl`, `state.json`) must be migratable
- Config TOML format must remain valid (new fields are additive)
- Wire protocol JSON-RPC must remain compatible (new event types are additive)
- MCP integration must remain `fastmcp`-based

### 9.3 Risk Mitigation

1. **Feature flags**: All deviations are behind `KIMI_EXPERIMENTAL_*` env vars
2. **A/B testing**: New orchestrator can be selected per-session
3. **Rollback**: SQLite store maintains export-to-JSONL function
4. **Canary**: New tool manifest system loads alongside old system; conflicts favor old
5. **Test coverage**: Each phase requires 90%+ test coverage before merge

### 9.4 Success Metrics

- **Latency**: Turn start-to-first-token latency < 50ms (currently ~100ms due to Wire creation)
- **Memory**: Session memory usage grows sub-linearly with turn count (currently linear due to in-memory history)
- **Reliability**: Zero lost approvals after SessionStream migration
- **Extensibility**: New tool can be added by creating a single file (manifest + function)
- **Testability**: Unit test for new tool requires < 20 lines of fixture setup (currently ~100+)

---

## Appendix A: Glossary of Terms

| Term | Definition |
|------|------------|
| **Soul** | The core agent loop implementation (`KimiSoul`) |
| **Wire** | SPMC event channel between soul and UI |
| **RootWireHub** | Session-scoped broadcast for out-of-turn events |
| **Context** | Append-only conversation history store |
| **D-Mail** | Checkpointed time-travel messaging via `DenwaRenji` |
| **Steer** | User message injected mid-turn into active agent loop |
| **YOLO** | Auto-approve all destructive operations |
| **Ralph** | Automated iteration mode with decision gate |
| **LaborMarket** | Subagent type registry |
| **Kosong** | LLM abstraction framework |
| **Kaos** | OS abstraction layer (local/SSH) |

## Appendix B: File Inventory

| File | Lines (approx) | Role |
|------|----------------|------|
| `src/kimi_cli/app.py` | ~300 | Application factory |
| `src/kimi_cli/cli/__init__.py` | ~400 | CLI entry point |
| `src/kimi_cli/soul/kimisoul.py` | ~800 | Agent loop |
| `src/kimi_cli/soul/agent.py` | ~400 | Runtime, Agent, LaborMarket |
| `src/kimi_cli/soul/context.py` | ~300 | Context persistence |
| `src/kimi_cli/soul/toolset.py` | ~500 | Tool loading & execution |
| `src/kimi_cli/wire/__init__.py` | ~300 | Wire channel |
| `src/kimi_cli/wire/types.py` | ~400 | Wire message types |
| `src/kimi_cli/approval_runtime/runtime.py` | ~300 | Approval state machine |
| `src/kimi_cli/session.py` | ~300 | Session lifecycle |
| `src/kimi_cli/config.py` | ~400 | Configuration models |
| `src/kimi_cli/llm.py` | ~200 | LLM factory |
| `src/kimi_cli/background/manager.py` | ~400 | Background task management |
| `src/kimi_cli/notifications/manager.py` | ~300 | Notification delivery |
| `src/kimi_cli/subagents/runner.py` | ~300 | Subagent execution |
| `src/kimi_cli/subagents/store.py` | ~200 | Subagent persistence |
| `src/kimi_cli/tools/shell/__init__.py` | ~300 | Shell tool |
| `src/kimi_cli/tools/file/*.py` | ~600 | File tools |
| `src/kimi_cli/tools/web/*.py` | ~200 | Web tools |

---

*End of Specification*
