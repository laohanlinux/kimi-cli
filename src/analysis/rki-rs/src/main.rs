use clap::Parser;
use rki_rs::{
    acp, agent, agents_md, approval, capability, cli, config, config_registry, config_watcher,
    context, feature_flags, llm, mcp, memory, runtime, session, skills, soul, store, tools,
    turn_input, wire, workdir_ls,
};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Resolve the Kimi home directory, respecting `KIMI_HOME` env var.
fn kimi_home() -> std::path::PathBuf {
    let path = std::env::var("KIMI_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".kimi"));
    // Ensure directory exists so SQLite can create the database file
    if let Err(e) = std::fs::create_dir_all(&path) {
        eprintln!(
            "Warning: failed to create kimi home directory {:?}: {}",
            path, e
        );
    }
    path
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = cli::Cli::parse();

    let work_dir = if let Some(wd) = &args.work_dir {
        std::path::PathBuf::from(wd)
    } else {
        std::env::current_dir()?
    };

    let config_path = kimi_home().join("config.toml");
    // Use ConfigRegistry as the single config loading path (§8.1)
    let registry = config_registry::parse_config_file(&config_path)?;
    let mut config = registry.to_legacy_config();
    config.apply_env_overrides();

    let db_path = kimi_home().join("sessions.db");
    let store = store::Store::open(&db_path)?;

    if let Some(ref session_id) = args.show_unified_events {
        let rows = store.list_unified_session_events_filtered(
            session_id,
            args.unified_events_after.as_deref(),
            args.unified_events_limit,
        )?;
        for row in rows {
            println!(
                "{}",
                serde_json::to_string(&row).unwrap_or_else(|_| "{}".to_string())
            );
        }
        return Ok(());
    }

    // Handle --list-sessions before any session creation
    if args.list_sessions {
        let sessions = store.list_sessions()?;
        if sessions.is_empty() {
            println!("No sessions found.");
        } else {
            println!("{:<36} {:<20} Work Dir", "Session ID", "Created");
            for (id, work_dir, created_at) in sessions {
                println!("{:<36} {:<20} {}", id, created_at, work_dir);
            }
        }
        return Ok(());
    }

    // AGENTS.md discovery (§1.2 step 7)
    let agents_md = agents_md::discover(&work_dir).await;

    let session = if let Some(ref parent_id) = args.fork_from {
        session::Session::fork_with_context_cursor(
            &store,
            parent_id,
            work_dir.clone(),
            args.fork_context_up_to_id,
        )?
    } else if let Some(ref session_id) = args.session {
        session::Session::load_by_id(&store, session_id)?
    } else if args.resume {
        session::Session::discover_latest(&store, work_dir)?
    } else {
        session::Session::create(&store, work_dir)?
    };

    let hub = wire::RootWireHub::new();
    let mut approval = approval::ApprovalRuntime::new(
        hub.clone(),
        args.yolo,
        args.auto_approve.clone().unwrap_or_default(),
    );
    if let Some(ref profile) = config.trust_profile {
        approval =
            approval.with_capability_engine(capability::CapabilityEngine::new(profile.clone()));
    }
    if args.print {
        approval.set_headless_ide_mode();
    }
    let approval = Arc::new(approval);

    let features = feature_flags::FeatureFlags::from_env();
    let runtime = runtime::Runtime::with_features(
        config.clone(),
        session.clone(),
        approval.clone(),
        hub.clone(),
        store.clone(),
        features,
    );
    let _ = runtime.bg_manager.recover().await;

    // CLI mode overrides: plan mode and Ralph mode (§1.2 bootstrap flags)
    if args.plan {
        runtime.enter_plan_mode().await;
        hub.broadcast(wire::WireEvent::PlanDisplay {
            content: "Plan mode active (via --plan).".to_string(),
            file_path: String::new(),
        });
    }
    if args.ralph {
        let max_iter = config.ralph_max_iterations.max(1);
        runtime.enter_ralph_mode(max_iter).await;
        hub.broadcast(wire::WireEvent::TextPart {
            text: format!("\n[Ralph mode ON] max_iterations={}\n", max_iter),
        });
    }

    // Config watcher with hot-reload and selective propagation (§8.7)
    let config_path_for_watcher = config_path.clone();
    let propagator =
        config_watcher::ConfigChangePropagator::new(config_path.clone(), Some(config.clone()));
    let propagator_for_watcher = propagator.clone();
    let runtime_for_watcher = runtime.clone();
    let config_path_for_sub = config_path.clone();
    propagator.subscribe(
        "model",
        Box::new(
            move |_section: &str, _old: &config::Config, _new: &config::Config| {
                let rt = runtime_for_watcher.clone();
                let p = config_path_for_sub.clone();
                tokio::spawn(async move {
                    match rt.reload_config(&p).await {
                        Ok(_) => tracing::info!("Config hot-reloaded from {:?}", p),
                        Err(e) => tracing::warn!("Failed to hot-reload config: {}", e),
                    }
                });
            },
        ),
    );
    let runtime_for_orch_watcher = runtime.clone();
    let config_path_for_orch_sub = config_path.clone();
    propagator.subscribe(
        "orchestrator",
        Box::new(
            move |_section: &str, _old: &config::Config, _new_cfg: &config::Config| {
                let rt = runtime_for_orch_watcher.clone();
                let p = config_path_for_orch_sub.clone();
                tokio::spawn(async move {
                    match rt.reload_config(&p).await {
                        Ok(_) => {
                            let orch_name = {
                                let cfg = rt.config.read().await;
                                cfg.default_orchestrator.clone()
                            };
                            let new_orch: Arc<dyn rki_rs::orchestrator::TurnOrchestrator> =
                                match orch_name.as_str() {
                                    "plan" => Arc::new(rki_rs::orchestrator::PlanModeOrchestrator),
                                    "ralph" => {
                                        let max_iter = {
                                            let cfg = rt.config.read().await;
                                            cfg.ralph_max_iterations.max(1)
                                        };
                                        Arc::new(rki_rs::orchestrator::RalphOrchestrator::new(
                                            max_iter,
                                        ))
                                    }
                                    _ => Arc::new(rki_rs::orchestrator::ReActOrchestrator),
                                };
                            rt.set_orchestrator(new_orch).await;
                            tracing::info!(
                                "Orchestrator switched to '{}' via hot-reload",
                                orch_name
                            );
                        }
                        Err(e) => tracing::warn!("Failed to hot-reload orchestrator config: {}", e),
                    }
                });
            },
        ),
    );
    propagator.subscribe(
        "mcp",
        Box::new(|_section: &str, _old: &config::Config, _new: &config::Config| {
            tracing::info!(
                "MCP section changed in config.toml; restart this process to rebuild MCP connections and tools."
            );
        }),
    );
    if runtime
        .features
        .is_enabled(rki_rs::feature_flags::ExperimentalFeature::ConfigHotReload)
    {
        let _config_watcher =
            config_watcher::ConfigWatcher::new(&config_path_for_watcher, propagator_for_watcher);
    }
    let toolset = runtime.toolset.clone();
    let mut mcp_sessions: Vec<Arc<mcp::MCPSession>> = Vec::new();
    {
        let mut ts = toolset.lock().await;
        ts.register(Box::new(tools::ShellTool));
        ts.register(Box::new(tools::ReadFileTool));
        ts.register(Box::new(tools::ReadMediaFileTool));
        ts.register(Box::new(tools::WriteFileTool));
        ts.register(Box::new(tools::StrReplaceFileTool));
        ts.register(Box::new(tools::GlobTool));
        ts.register(Box::new(tools::GrepTool));
        ts.register(Box::new(tools::SearchWebTool));
        ts.register(Box::new(tools::FetchURLTool));
        ts.register(Box::new(tools::think_tool()));
        ts.register(Box::new(tools::set_todo_list_tool()));
        ts.register(Box::new(tools::ask_user_question_tool()));
        ts.register(Box::new(tools::send_dmail_tool()));
        ts.register(Box::new(tools::AgentTool));
        ts.register(Box::new(tools::task_list_tool()));
        ts.register(Box::new(tools::task_output_tool()));
        ts.register(Box::new(tools::task_stop_tool()));
        ts.register(Box::new(tools::enter_plan_mode_tool()));
        ts.register(Box::new(tools::exit_plan_mode_tool()));
        if runtime
            .features
            .is_enabled(rki_rs::feature_flags::ExperimentalFeature::FunctionTools)
        {
            let fn_tool = tools::FunctionTool::new(
                "fn_ping",
                "§7.2 prototype: stateless function tool (experimental).",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "payload": { "type": "string", "description": "Optional text to echo back" }
                    }
                }),
                |args: serde_json::Value, _ctx: &tools::ToolContext| async move {
                    let text = args
                        .get("payload")
                        .and_then(|v| v.as_str())
                        .unwrap_or("pong")
                        .to_string();
                    Ok(tools::ToolOutput {
                        result: tools::ToolResult {
                            r#type: "success".to_string(),
                            content: vec![rki_rs::message::ContentBlock::Text { text }],
                            summary: "fn_ping ok".to_string(),
                        },
                        artifacts: vec![],
                        metrics: rki_rs::message::ToolMetrics::default(),
                    })
                },
            );
            ts.register(Box::new(fn_tool));
        }
        if runtime
            .features
            .is_enabled(rki_rs::feature_flags::ExperimentalFeature::PluginRegistry)
        {
            for (manifest, tool_dir) in tools::discover_manifests(&session.work_dir) {
                ts.register(Box::new(tools::ManifestTool::new(manifest, tool_dir)));
            }
        }

        // Load MCP tools and sessions (§7.5 native integration)
        let mcp_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".kimi")
            .join("mcp.json");
        if mcp_path.exists()
            && let Ok(content) = std::fs::read_to_string(&mcp_path)
            && let Ok(mcp_config) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(servers) = mcp_config["servers"].as_object()
        {
            for (name, server) in servers {
                if let Some(cmd) = server["command"].as_str() {
                    let args: Vec<String> = server["args"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut command = vec![cmd.to_string()];
                    command.extend(args);
                    let client = Arc::new(mcp::MCPClient::new(name.clone(), command));
                    let session = Arc::new(mcp::MCPSession::new(client.clone()));
                    mcp_sessions.push(session.clone());
                    match client.list_tools().await {
                        Ok(mcp_tools) => {
                            for tool in mcp_tools {
                                ts.register(Box::new(mcp::MCPTool::new(
                                    tool.name.clone(),
                                    tool.description.clone(),
                                    tool.input_schema,
                                    client.clone(),
                                )));
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to load MCP tools from {}: {}", name, e);
                        }
                    }
                }
            }
        }
    }

    let context = Arc::new(Mutex::new(
        context::Context::load(&store, &session.id).await?,
    ));
    if runtime
        .features
        .is_enabled(feature_flags::ExperimentalFeature::SemanticEmbeddings)
    {
        let mut ctx = context.lock().await;
        if let Some(http) = memory::HttpEmbeddingProvider::from_env() {
            ctx.attach_semantic_embeddings(Arc::new(http));
        } else {
            ctx.attach_semantic_embeddings(Arc::new(memory::HashEmbeddingProvider::new(64)));
        }
    }

    let discovered_skills = skills::discover_skills(&session.work_dir).await;
    let skills_md = skills::skills_prompt_section(&discovered_skills);

    let work_tree = workdir_ls::format_work_dir_tree(session.work_dir.as_path(), 2).await;
    let system_prompt = {
        let mut s = "You are Kimi, a helpful assistant.".to_string();
        if !agents_md.is_empty() {
            s.push_str("\n\n");
            s.push_str(&agents_md);
        }
        if !work_tree.trim().is_empty() {
            s.push_str("\n\n## Workspace tree (auto, depth 2)\n\n");
            s.push_str(&work_tree);
        }
        if let Some(ref block) = skills_md {
            s.push_str("\n\n");
            s.push_str(block);
        }
        s
    };

    let agent = agent::Agent {
        spec: agent::AgentSpec {
            name: "default".to_string(),
            system_prompt: system_prompt.clone(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        },
        system_prompt,
    };

    let model = args.model.as_deref().unwrap_or(&config.default_model);
    // §1.2 L16: vision validation uses `default_model`; align with CLI `--model` for the active provider.
    if args.model.is_some() {
        let mut cfg = runtime.config.write().await;
        cfg.default_model = model.to_string();
    }
    let llm: Arc<dyn llm::ChatProvider> = if model == "echo" {
        Arc::new(llm::EchoProvider)
    } else {
        match llm::create_provider(model, &runtime.identity, Some(session.id.clone())).await {
            Ok(p) => Arc::from(p),
            Err(e) => {
                eprintln!(
                    "Failed to create provider for '{}': {}. Falling back to echo.",
                    model, e
                );
                Arc::new(llm::EchoProvider)
            }
        }
    };

    // §7.5: sampling + progress forwarding are experimental; tools/list + tools/call always work.
    if runtime
        .features
        .is_enabled(feature_flags::ExperimentalFeature::NativeMcp)
    {
        let llm_for_mcp = llm.clone();
        let hub_for_mcp = hub.clone();
        for session in &mcp_sessions {
            let session_sampling = session.clone();
            let llm = llm_for_mcp.clone();
            tokio::spawn(async move {
                if let Err(e) = session_sampling.start_sampling_handler(llm).await {
                    tracing::warn!("MCP sampling handler failed to start: {}", e);
                }
            });
            let session_progress = session.clone();
            let hub = hub_for_mcp.clone();
            tokio::spawn(async move {
                if let Err(e) = session_progress.start_progress_forwarder(hub).await {
                    tracing::warn!("MCP progress forwarder failed to start: {}", e);
                }
            });
            let session_resources = session.clone();
            let hub = hub_for_mcp.clone();
            tokio::spawn(async move {
                if let Err(e) = session_resources.start_resource_monitor(hub).await {
                    tracing::warn!("MCP resource monitor failed to start: {}", e);
                }
            });
        }
    }

    let soul = std::sync::Arc::new(soul::KimiSoul::new(
        agent,
        context.clone(),
        llm,
        runtime.clone(),
    ));

    // Spawn hub listener for approvals and questions
    let approval_ui = approval.clone();
    let question_ui = runtime.question.clone();
    let mut hub_rx = hub.subscribe();
    let hub_task = tokio::spawn(async move {
        use std::io::Write;
        use wire::WireEvent;
        while let Ok(envelope) = hub_rx.recv().await {
            match envelope.event {
                WireEvent::ApprovalRequest {
                    id,
                    action,
                    description,
                    ..
                } => {
                    println!("\n[Approval {}] {}: {}", id, action, description);
                    print!("Approve? (y/n): ");
                    let _ = std::io::stdout().flush();
                    let mut answer = String::new();
                    if std::io::stdin().read_line(&mut answer).is_ok() {
                        let approved = answer.trim().to_lowercase().starts_with('y');
                        let _ = approval_ui.resolve(id, approved, None).await;
                    }
                }
                WireEvent::QuestionRequest { id, questions } => {
                    println!("\n[Question {}]", id);
                    let mut answers = Vec::new();
                    for q in questions {
                        println!("{}", q.question);
                        print!("Answer: ");
                        let _ = std::io::stdout().flush();
                        let mut answer = String::new();
                        if std::io::stdin().read_line(&mut answer).is_ok() {
                            answers.push(answer.trim().to_string());
                        }
                    }
                    let _ = question_ui.resolve(id, answers).await;
                }
                _ => {}
            }
        }
    });

    // UI subscribes directly to the session hub (§5.2: persistent session stream)
    let mut rx = hub.subscribe();
    let ui_task = tokio::spawn(async move {
        use std::io::Write;
        use wire::WireEvent;
        while let Ok(envelope) = rx.recv().await {
            match envelope.event {
                WireEvent::TurnBegin { user_input } => {
                    println!("\n[TurnBegin] {}", user_input.text);
                }
                WireEvent::SessionShutdown { reason } => {
                    println!("[SessionShutdown] {}", reason);
                }
                WireEvent::TurnEnd => println!("[TurnEnd]"),
                WireEvent::StepBegin { n } => println!("  [Step {}]", n),
                WireEvent::TextPart { text } => {
                    print!("{}", text);
                    let _ = std::io::stdout().flush();
                }
                WireEvent::ThinkPart { text } => println!("[thinking: {}]", text),
                WireEvent::ToolCall { id, function } => {
                    println!(
                        "\n[ToolCall {}] {}({})",
                        id, function.name, function.arguments
                    );
                }
                WireEvent::ToolResult {
                    tool_call_id,
                    output,
                    is_error,
                    elapsed_ms,
                } => {
                    println!(
                        "\n[ToolResult {}] error={} elapsed={:?}\n{}",
                        tool_call_id, is_error, elapsed_ms, output
                    );
                }
                WireEvent::StatusUpdate {
                    token_count,
                    context_size,
                    plan_mode,
                    mcp_status,
                } => {
                    println!(
                        "\n[Status] tokens={}/{} plan={} mcp={}",
                        token_count, context_size, plan_mode, mcp_status
                    );
                }
                _ if envelope.is_subagent_event() => {
                    println!("\n[Subagent {:?}] {:?}", envelope.source, envelope.event);
                }
                WireEvent::CompactionBegin => println!("[CompactionBegin]"),
                WireEvent::CompactionEnd => println!("[CompactionEnd]"),
                WireEvent::StepInterrupted { reason } => {
                    println!("[StepInterrupted: {}]", reason);
                }
                _ => {}
            }
        }
    });

    // Start ACP server if requested (§1.2 ACP server mode for IDE integrations)
    let acp_task = if let Some(port) = args.acp {
        let acp_auth_token = std::env::var("RKI_ACP_TOKEN").ok().and_then(|s| {
            let t = s.trim().to_string();
            (!t.is_empty()).then_some(t)
        });
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::channel::<String>(64);
        let soul_acp = soul.clone();
        let hub_acp = hub.clone();
        let _acp_turn_worker = tokio::spawn(async move {
            while let Some(body) = turn_rx.recv().await {
                let body = body.trim();
                if body.is_empty() {
                    continue;
                }
                match turn_input::parse_cli_turn_line(body) {
                    Ok(turn) => {
                        if let Err(e) = soul_acp.run(turn, &hub_acp).await {
                            tracing::warn!("ACP queued turn failed: {}", e);
                        }
                    }
                    Err(e) => tracing::warn!("ACP turn parse failed: {}", e),
                }
            }
        });
        let acp_server = acp::AcpServer::new(
            hub.clone(),
            port,
            Some(turn_tx),
            acp_auth_token,
            acp::default_max_request_bytes(),
        );
        Some(tokio::spawn(async move {
            if let Err(e) = acp_server.run().await {
                tracing::error!("ACP server error: {}", e);
            }
        }))
    } else {
        None
    };

    if args.print {
        // Non-interactive print mode: read one line, run soul, format output, exit
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let input = line.trim();
        if !input.is_empty() {
            let mut rx = hub.subscribe();
            let print_task = tokio::spawn(async move {
                let mut output = String::new();
                while let Ok(envelope) = rx.recv().await {
                    use wire::WireEvent;
                    match &envelope.event {
                        WireEvent::TextPart { text } => output.push_str(text),
                        WireEvent::ThinkPart { text } => {
                            output.push_str(&format!("[thinking: {}]\n", text))
                        }
                        WireEvent::ToolCall { id, function } => {
                            output.push_str(&format!(
                                "\n[Tool {}] {}({})\n",
                                id, function.name, function.arguments
                            ));
                        }
                        WireEvent::ToolResult {
                            tool_call_id,
                            output: result,
                            is_error,
                            elapsed_ms,
                        } => {
                            output.push_str(&format!(
                                "[Result {}] error={} elapsed={:?}\n{}\n",
                                tool_call_id, is_error, elapsed_ms, result
                            ));
                        }
                        WireEvent::SessionShutdown { reason } => {
                            output.push_str(&format!("\n[SessionShutdown: {}]\n", reason));
                        }
                        WireEvent::TurnEnd => break,
                        _ => {}
                    }
                }
                output
            });
            match turn_input::parse_cli_turn_line(input) {
                Ok(turn) => {
                    if let Err(e) = soul.run(turn, &hub).await {
                        eprintln!("Error: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("Invalid turn: {}", e);
                    std::process::exit(2);
                }
            }
            // §1.2 L35: shutdown signal then TurnEnd so wire subscribers can flush.
            hub.broadcast(wire::WireEvent::SessionShutdown {
                reason: "print_mode_complete".into(),
            });
            hub.broadcast(wire::WireEvent::TurnEnd);
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(120), print_task).await;
            match output {
                Ok(Ok(text)) => println!("{}", text),
                Ok(Err(e)) => eprintln!("Print task error: {}", e),
                Err(_) => eprintln!("Print mode timed out"),
            }
        } else {
            hub.broadcast(wire::WireEvent::SessionShutdown {
                reason: "print_mode_empty_stdin".into(),
            });
            hub.broadcast(wire::WireEvent::TurnEnd);
        }
        // In print mode ui_task loops forever; abort it so we can exit cleanly.
        ui_task.abort();
    } else {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        use std::io::Write;
        loop {
            print!("> ");
            stdout.flush()?;
            let mut line = String::new();
            stdin.read_line(&mut line)?;
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            if input == "/exit" || input == "/quit" {
                break;
            }
            match turn_input::parse_cli_turn_line(input) {
                Ok(turn) => {
                    if let Err(e) = soul.run(turn, &hub).await {
                        eprintln!("Error: {}", e);
                    }
                }
                Err(e) => eprintln!("Invalid turn: {}", e),
            }
        }
    }

    // §1.2 L35: process-exit wire flush for interactive / shared hub subscribers.
    if !args.print {
        hub.broadcast(wire::WireEvent::SessionShutdown {
            reason: "interactive_exit".into(),
        });
    }
    hub.broadcast(wire::WireEvent::TurnEnd);

    let ui_shutdown_secs: u64 = std::env::var("RKI_UI_SHUTDOWN_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if ui_shutdown_secs > 0 {
        match tokio::time::timeout(std::time::Duration::from_secs(ui_shutdown_secs), ui_task).await
        {
            Ok(join) => {
                let _ = join;
            }
            Err(_) => tracing::warn!(
                "ui_task shutdown wait timed out after {}s (set RKI_UI_SHUTDOWN_WAIT_SECS=0 to wait indefinitely)",
                ui_shutdown_secs
            ),
        }
    } else {
        let _ = ui_task.await;
    }
    hub_task.abort();
    if let Some(task) = acp_task {
        task.abort();
    }

    if args.archive {
        if let Err(e) = session.archive(&store) {
            eprintln!("Failed to archive session: {}", e);
        } else {
            println!("Session {} archived.", session.id);
        }
    }

    Ok(())
}
