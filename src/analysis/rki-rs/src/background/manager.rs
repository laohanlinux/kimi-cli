use crate::background::executor::{AgentExecutor, TaskExecutor};
use crate::background::types::{TaskEvent, TaskKind, TaskRef, TaskSpec, TaskStatus};
use crate::feature_flags::ExperimentalFeature;
use crate::runtime::Runtime;
use crate::store::Store;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast};

struct Inner {
    tasks: Mutex<HashMap<String, TaskRef>>,
    pids: Mutex<HashMap<String, u32>>,
    aborts: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// Running bash subprocess tasks (§8.3 per-executor cap).
    running_bash: Mutex<usize>,
    /// Running in-process agent tasks (§8.3 per-executor cap).
    running_agent: Mutex<usize>,
    max_running_bash: usize,
    max_running_agent: usize,
    /// When set, `bash + agent` running may not exceed this (legacy `with_max_running` global pool).
    max_total_concurrent: Option<usize>,
    store: Store,
    session_id: String,
    runtime: std::sync::Mutex<Option<Runtime>>,
    /// Bash auto-retries are queued here so worker tasks never `await submit()` (keeps futures `Send`).
    retry_tx: tokio::sync::mpsc::UnboundedSender<TaskSpec>,
    /// §8.3: per-task broadcast so `subscribe_task_results` yields `TaskEvent` as execution progresses.
    task_event_txs: Mutex<HashMap<String, broadcast::Sender<TaskEvent>>>,
}

impl Inner {
    async fn emit_task_event(&self, task_id: &str, ev: TaskEvent) {
        if let Some(tx) = self.task_event_txs.lock().await.get(task_id).cloned() {
            let _ = tx.send(ev);
        }
    }

    async fn finish_task_event_channel(&self, task_id: &str) {
        self.task_event_txs.lock().await.remove(task_id);
    }

    async fn inc_running_for_bash(&self) {
        *self.running_bash.lock().await += 1;
    }

    async fn inc_running_for_agent(&self) {
        *self.running_agent.lock().await += 1;
    }

    async fn dec_running_for_bash(&self) {
        let mut g = self.running_bash.lock().await;
        *g = g.saturating_sub(1);
    }

    async fn dec_running_for_agent(&self) {
        let mut g = self.running_agent.lock().await;
        *g = g.saturating_sub(1);
    }
}

async fn publish_terminal_bg_notification_inner(
    inner: &Arc<Inner>,
    task_id: &str,
    terminal_reason: &str,
) {
    let task = { inner.tasks.lock().await.get(task_id).cloned() };
    let Some(task) = task else {
        return;
    };
    let rt = inner.runtime.lock().unwrap().clone();
    let Some(runtime) = rt else {
        return;
    };
    let ev = crate::notification::task_terminal::build_background_task_notification(
        &task,
        terminal_reason,
    );
    let _ = runtime.notifications.publish(ev).await;
}

#[derive(Clone)]
pub struct BackgroundTaskManager {
    inner: Arc<Inner>,
}

impl BackgroundTaskManager {
    pub fn new(session_id: String, store: Store) -> Self {
        Self::with_max_running(session_id, store, 4)
    }

    /// Legacy global pool: at most `max_running` tasks total, each executor may use up to the same budget.
    pub fn with_max_running(session_id: String, store: Store, max_running: usize) -> Self {
        let n = max_running.max(1);
        Self::with_limits(session_id, store, n, n, Some(n))
    }

    /// §8.3: separate caps for bash vs agent with **no** global ceiling (both can run up to their limits).
    pub fn with_executor_caps(
        session_id: String,
        store: Store,
        max_concurrent_bash: usize,
        max_concurrent_agent: usize,
    ) -> Self {
        Self::with_limits(
            session_id,
            store,
            max_concurrent_bash.max(1),
            max_concurrent_agent.max(1),
            None,
        )
    }

    fn with_limits(
        session_id: String,
        store: Store,
        max_bash: usize,
        max_agent: usize,
        max_total: Option<usize>,
    ) -> Self {
        let (retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel::<TaskSpec>();
        let inner = Arc::new(Inner {
            tasks: Mutex::new(HashMap::new()),
            pids: Mutex::new(HashMap::new()),
            aborts: Mutex::new(HashMap::new()),
            running_bash: Mutex::new(0),
            running_agent: Mutex::new(0),
            max_running_bash: max_bash,
            max_running_agent: max_agent,
            max_total_concurrent: max_total,
            store,
            session_id,
            runtime: std::sync::Mutex::new(None),
            retry_tx,
            task_event_txs: Mutex::new(HashMap::new()),
        });
        // Start a lightweight poller to check for tasks whose dependencies
        // have completed (§8.3 deviation). Only spawn if inside a Tokio runtime.
        if let Ok(_handle) = tokio::runtime::Handle::try_current() {
            let inner_clone = inner.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
                loop {
                    interval.tick().await;
                    let mgr = BackgroundTaskManager {
                        inner: inner_clone.clone(),
                    };
                    let _ = mgr.start_pending().await;
                }
            });
            let inner_retry = inner.clone();
            tokio::spawn(async move {
                while let Some(spec) = retry_rx.recv().await {
                    let mgr = BackgroundTaskManager {
                        inner: inner_retry.clone(),
                    };
                    let _ = mgr.submit(spec).await;
                }
            });
        }
        Self { inner }
    }

    pub fn set_runtime(&self, runtime: Runtime) {
        *self.inner.runtime.lock().unwrap() = Some(runtime);
    }

    /// Nominal concurrent capacity: global cap when configured, else sum of per-executor caps (§8.3).
    pub fn max_concurrent_tasks(&self) -> usize {
        self.inner
            .max_total_concurrent
            .unwrap_or(self.inner.max_running_bash + self.inner.max_running_agent)
    }

    pub fn max_concurrent_bash(&self) -> usize {
        self.inner.max_running_bash
    }

    pub fn max_concurrent_agent(&self) -> usize {
        self.inner.max_running_agent
    }

    pub async fn recover(&self) -> anyhow::Result<()> {
        let rows = self.inner.store.list_tasks(&self.inner.session_id)?;
        {
            let mut tasks = self.inner.tasks.lock().await;
            for (id, _kind, spec, status, _output) in rows {
                let spec: TaskSpec = serde_json::from_str(&spec).unwrap_or(TaskSpec {
                    id: id.clone(),
                    kind: TaskKind::Bash {
                        command: String::new(),
                    },
                    created_at: chrono::Utc::now(),
                    dependencies: vec![],
                    max_retries: 0,
                    timeout_s: None,
                });
                let status = match status.as_str() {
                    "pending" => TaskStatus::Pending,
                    "running" => TaskStatus::Running,
                    "completed" => TaskStatus::Completed { exit_code: None },
                    "failed" => TaskStatus::Failed {
                        reason: "Recovered failed task".to_string(),
                    },
                    "cancelled" => TaskStatus::Cancelled,
                    _ => TaskStatus::Lost,
                };
                tasks.insert(
                    id.clone(),
                    TaskRef {
                        id,
                        spec,
                        status,
                        timed_out: false,
                    },
                );
            }
        }
        self.publish_stored_terminal_notifications(None).await;
        Ok(())
    }

    /// Parity with Python `BackgroundManager.publish_terminal_notifications`: persist terminal task
    /// notifications after restart. Dedupe keys prevent duplicates when the event was already stored.
    pub async fn publish_stored_terminal_notifications(&self, limit: Option<usize>) {
        use crate::notification::task_terminal::{
            build_background_task_notification, terminal_reason_for_task,
        };
        let tasks = self.list().await;
        let mut new_count = 0usize;
        for task in tasks {
            let Some(reason) = terminal_reason_for_task(&task) else {
                continue;
            };
            let rt = self.inner.runtime.lock().unwrap().clone();
            let Some(runtime) = rt else {
                continue;
            };
            let ev = build_background_task_notification(&task, reason);
            match runtime.notifications.publish(ev).await {
                Ok(Some(_)) => {
                    new_count += 1;
                    if let Some(lim) = limit {
                        if new_count >= lim {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// §8.3: submit and return a live `TaskEvent` receiver before scheduling (avoids missing fast tasks).
    pub async fn submit_with_receiver(
        &self,
        spec: TaskSpec,
    ) -> anyhow::Result<(String, broadcast::Receiver<TaskEvent>)> {
        let id = spec.id.clone();
        let (ev_tx, rx) = broadcast::channel::<TaskEvent>(128);
        let task_ref = TaskRef {
            id: id.clone(),
            spec: spec.clone(),
            status: TaskStatus::Pending,
            timed_out: false,
        };
        self.inner.tasks.lock().await.insert(id.clone(), task_ref);
        self.inner
            .task_event_txs
            .lock()
            .await
            .insert(id.clone(), ev_tx);
        let spec_json = serde_json::to_string(&spec)?;
        if let Err(e) =
            self.inner
                .store
                .create_task(&id, &self.inner.session_id, "task", &spec_json, "pending")
        {
            self.inner.tasks.lock().await.remove(&id);
            self.inner.task_event_txs.lock().await.remove(&id);
            return Err(e.into());
        }
        self.start_pending().await?;
        Ok((id, rx))
    }

    pub async fn submit(&self, spec: TaskSpec) -> anyhow::Result<String> {
        Ok(self.submit_with_receiver(spec).await?.0)
    }

    /// §8.3: subscribe to live task events (`Started`, `Output`, terminal state). Returns `None` if `task_id` is unknown.
    pub async fn subscribe_task_results(
        &self,
        task_id: &str,
    ) -> Option<broadcast::Receiver<TaskEvent>> {
        self.inner
            .task_event_txs
            .lock()
            .await
            .get(task_id)
            .map(|tx| tx.subscribe())
    }

    pub async fn status(&self, id: &str) -> Option<TaskStatus> {
        self.inner
            .tasks
            .lock()
            .await
            .get(id)
            .map(|t| t.status.clone())
    }

    pub async fn list(&self) -> Vec<TaskRef> {
        self.inner.tasks.lock().await.values().cloned().collect()
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        if let Some(pid) = self.inner.pids.lock().await.remove(id) {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        if let Some(handle) = self.inner.aborts.lock().await.remove(id) {
            handle.abort();
        }
        self.inner.store.update_task_status(id, "cancelled", None)?;
        self.inner.emit_task_event(id, TaskEvent::Cancelled).await;
        self.inner.finish_task_event_channel(id).await;
        let had_in_memory = {
            let mut tasks = self.inner.tasks.lock().await;
            if let Some(t) = tasks.get_mut(id) {
                t.status = TaskStatus::Cancelled;
                true
            } else {
                false
            }
        };
        if had_in_memory {
            publish_terminal_bg_notification_inner(&self.inner, id, "killed").await;
        }
        Ok(())
    }

    pub async fn output(&self, id: &str) -> anyhow::Result<String> {
        if let Ok(Some((_, _, _, out))) = self.inner.store.get_task(id) {
            Ok(out.unwrap_or_default())
        } else {
            Ok(String::new())
        }
    }

    async fn start_pending(&self) -> anyhow::Result<()> {
        // Snapshot work without holding executor counters across `tokio::spawn` (keeps this future `Send`).
        let to_start: Vec<(String, TaskKind)> = {
            let bash_r = *self.inner.running_bash.lock().await;
            let agent_r = *self.inner.running_agent.lock().await;
            let total = bash_r + agent_r;

            let mut bash_slots = self.inner.max_running_bash.saturating_sub(bash_r);
            let mut agent_slots = self.inner.max_running_agent.saturating_sub(agent_r);
            let mut total_slots = self
                .inner
                .max_total_concurrent
                .map(|c| c.saturating_sub(total))
                .unwrap_or(usize::MAX);

            if bash_slots == 0 && agent_slots == 0 {
                return Ok(());
            }
            if total_slots == 0 {
                return Ok(());
            }

            let mut out = Vec::new();
            {
                let tasks = self.inner.tasks.lock().await;
                for (_, task_ref) in tasks.iter() {
                    if matches!(task_ref.status, TaskStatus::Pending) {
                        let deps_satisfied = task_ref.spec.dependencies.iter().all(|dep_id| {
                            match tasks.get(dep_id) {
                                Some(dep) => matches!(dep.status, TaskStatus::Completed { .. }),
                                None => false,
                            }
                        });
                        if deps_satisfied {
                            let can = match &task_ref.spec.kind {
                                TaskKind::Bash { .. } => bash_slots > 0 && total_slots > 0,
                                TaskKind::Agent { .. } => agent_slots > 0 && total_slots > 0,
                            };
                            if can {
                                out.push((task_ref.id.clone(), task_ref.spec.kind.clone()));
                                match &task_ref.spec.kind {
                                    TaskKind::Bash { .. } => {
                                        bash_slots -= 1;
                                        total_slots -= 1;
                                    }
                                    TaskKind::Agent { .. } => {
                                        agent_slots -= 1;
                                        total_slots -= 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            out
        };

        for (id, kind) in to_start {
            {
                let mut tasks = self.inner.tasks.lock().await;
                if let Some(t) = tasks.get_mut(&id) {
                    if !matches!(t.status, TaskStatus::Pending) {
                        continue;
                    }
                    t.status = TaskStatus::Running;
                } else {
                    continue;
                }
            }
            match &kind {
                TaskKind::Bash { .. } => self.inner.inc_running_for_bash().await,
                TaskKind::Agent { .. } => self.inner.inc_running_for_agent().await,
            }
            let inner = self.inner.clone();

            match &kind {
                TaskKind::Bash { command } => {
                    let command = command.clone();
                    let id_spawn = id.clone();
                    let distributed_retry = self
                        .inner
                        .runtime
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|r| r.features.is_enabled(ExperimentalFeature::DistributedQueue))
                        .unwrap_or(false);
                    let handle = tokio::spawn(async move {
                        let child = match Command::new("sh")
                            .arg("-c")
                            .arg(&command)
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .spawn()
                        {
                            Ok(c) => c,
                            Err(e) => {
                                let _ = inner.store.update_task_status(
                                    &id_spawn,
                                    "failed",
                                    Some(&e.to_string()),
                                );
                                let mut tasks = inner.tasks.lock().await;
                                if let Some(t) = tasks.get_mut(&id_spawn) {
                                    t.status = TaskStatus::Failed {
                                        reason: e.to_string(),
                                    };
                                }
                                inner
                                    .emit_task_event(
                                        &id_spawn,
                                        TaskEvent::Failed {
                                            reason: e.to_string(),
                                        },
                                    )
                                    .await;
                                inner.finish_task_event_channel(&id_spawn).await;
                                publish_terminal_bg_notification_inner(&inner, &id_spawn, "failed")
                                    .await;
                                inner.dec_running_for_bash().await;
                                return;
                            }
                        };
                        inner.emit_task_event(&id_spawn, TaskEvent::Started).await;
                        if let Some(pid) = child.id() {
                            inner.pids.lock().await.insert(id_spawn.clone(), pid);
                        }

                        // Heartbeat loop: update heartbeat every 5 seconds while running
                        let hb_id = id_spawn.clone();
                        let hb_inner = inner.clone();
                        let hb_handle = tokio::spawn(async move {
                            let mut interval =
                                tokio::time::interval(std::time::Duration::from_secs(5));
                            loop {
                                interval.tick().await;
                                let _ = hb_inner.store.heartbeat_task(&hb_id);
                            }
                        });

                        let timeout_secs = {
                            let tasks = inner.tasks.lock().await;
                            tasks.get(&id_spawn).and_then(|t| t.spec.timeout_s)
                        };

                        let kill_pid = child.id();
                        let (result, hit_timeout) = if let Some(secs) = timeout_secs {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(secs),
                                child.wait_with_output(),
                            )
                            .await
                            {
                                Ok(r) => (r, false),
                                Err(_) => {
                                    #[cfg(unix)]
                                    if let Some(pid) = kill_pid {
                                        unsafe {
                                            libc::kill(pid as i32, libc::SIGTERM);
                                        }
                                    }
                                    (
                                        Err(std::io::Error::new(
                                            std::io::ErrorKind::TimedOut,
                                            "timed out",
                                        )),
                                        true,
                                    )
                                }
                            }
                        } else {
                            (child.wait_with_output().await, false)
                        };

                        hb_handle.abort();
                        let _ = inner.pids.lock().await.remove(&id_spawn);

                        if hit_timeout {
                            let secs = timeout_secs.unwrap_or(0);
                            let reason = format!("Command timed out after {secs}s");
                            let text = result
                                .as_ref()
                                .ok()
                                .map(|o| {
                                    format!(
                                        "{}{}",
                                        String::from_utf8_lossy(&o.stdout),
                                        String::from_utf8_lossy(&o.stderr)
                                    )
                                })
                                .unwrap_or_default();
                            let _ =
                                inner
                                    .store
                                    .update_task_status(&id_spawn, "failed", Some(&text));
                            {
                                let mut tasks = inner.tasks.lock().await;
                                if let Some(t) = tasks.get_mut(&id_spawn) {
                                    t.status = TaskStatus::Failed {
                                        reason: reason.clone(),
                                    };
                                    t.timed_out = true;
                                }
                            }
                            inner
                                .emit_task_event(
                                    &id_spawn,
                                    TaskEvent::Output { text: text.clone() },
                                )
                                .await;
                            inner
                                .emit_task_event(
                                    &id_spawn,
                                    TaskEvent::Failed {
                                        reason: reason.clone(),
                                    },
                                )
                                .await;
                            inner.finish_task_event_channel(&id_spawn).await;
                            publish_terminal_bg_notification_inner(&inner, &id_spawn, "timed_out")
                                .await;
                        } else {
                            match result {
                                Ok(output) => {
                                    let text = format!(
                                        "{}{}",
                                        String::from_utf8_lossy(&output.stdout),
                                        String::from_utf8_lossy(&output.stderr)
                                    );
                                    let success = output.status.success();
                                    let status_str = if success { "completed" } else { "failed" };
                                    let retry_next = if !success && distributed_retry {
                                        let tasks = inner.tasks.lock().await;
                                        tasks.get(&id_spawn).and_then(|t| {
                                            if t.spec.max_retries > 0 {
                                                let mut ns = t.spec.clone();
                                                ns.id = uuid::Uuid::new_v4().to_string();
                                                ns.max_retries = t.spec.max_retries - 1;
                                                Some(ns)
                                            } else {
                                                None
                                            }
                                        })
                                    } else {
                                        None
                                    };

                                    let _ = inner.store.update_task_status(
                                        &id_spawn,
                                        status_str,
                                        Some(&text),
                                    );
                                    {
                                        let mut tasks = inner.tasks.lock().await;
                                        if let Some(t) = tasks.get_mut(&id_spawn) {
                                            t.status = TaskStatus::Completed {
                                                exit_code: output.status.code(),
                                            };
                                        }
                                    }

                                    inner
                                        .emit_task_event(
                                            &id_spawn,
                                            TaskEvent::Output { text: text.clone() },
                                        )
                                        .await;
                                    inner
                                        .emit_task_event(
                                            &id_spawn,
                                            TaskEvent::Completed {
                                                exit_code: output.status.code(),
                                            },
                                        )
                                        .await;
                                    inner.finish_task_event_channel(&id_spawn).await;
                                    let term = if success { "completed" } else { "failed" };
                                    publish_terminal_bg_notification_inner(&inner, &id_spawn, term)
                                        .await;

                                    if let Some(ns) = retry_next {
                                        let _ = inner.retry_tx.send(ns);
                                    }
                                }
                                Err(e) => {
                                    let _ = inner.store.update_task_status(
                                        &id_spawn,
                                        "failed",
                                        Some(&e.to_string()),
                                    );
                                    let mut tasks = inner.tasks.lock().await;
                                    if let Some(t) = tasks.get_mut(&id_spawn) {
                                        t.status = TaskStatus::Failed {
                                            reason: e.to_string(),
                                        };
                                    }
                                    inner
                                        .emit_task_event(
                                            &id_spawn,
                                            TaskEvent::Failed {
                                                reason: e.to_string(),
                                            },
                                        )
                                        .await;
                                    inner.finish_task_event_channel(&id_spawn).await;
                                    publish_terminal_bg_notification_inner(
                                        &inner, &id_spawn, "failed",
                                    )
                                    .await;
                                }
                            }
                        }
                        inner.dec_running_for_bash().await;
                    });
                    self.inner
                        .aborts
                        .lock()
                        .await
                        .insert(id, handle.abort_handle());
                }
                TaskKind::Agent { .. } => {
                    let id_spawn = id.clone();
                    let inner_clone = self.inner.clone();
                    let handle = tokio::spawn(async move {
                        let runtime_opt = inner_clone.runtime.lock().unwrap().clone();
                        let runtime = match runtime_opt {
                            Some(r) => r,
                            None => {
                                let _ = inner_clone.store.update_task_status(
                                    &id_spawn,
                                    "failed",
                                    Some("Runtime not set"),
                                );
                                let mut tasks = inner_clone.tasks.lock().await;
                                if let Some(t) = tasks.get_mut(&id_spawn) {
                                    t.status = TaskStatus::Failed {
                                        reason: "Runtime not set".to_string(),
                                    };
                                }
                                inner_clone
                                    .emit_task_event(
                                        &id_spawn,
                                        TaskEvent::Failed {
                                            reason: "Runtime not set".to_string(),
                                        },
                                    )
                                    .await;
                                inner_clone.finish_task_event_channel(&id_spawn).await;
                                publish_terminal_bg_notification_inner(
                                    &inner_clone,
                                    &id_spawn,
                                    "failed",
                                )
                                .await;
                                inner_clone.dec_running_for_agent().await;
                                return;
                            }
                        };
                        let executor = AgentExecutor::new(runtime);
                        let spec_clone = {
                            let tasks = inner_clone.tasks.lock().await;
                            tasks.get(&id_spawn).map(|t| t.spec.clone())
                        };
                        if let Some(spec) = spec_clone {
                            let timeout_s = spec.timeout_s;
                            let (events, timed_out_agent) = if let Some(secs) = timeout_s {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(secs),
                                    executor.execute(&spec),
                                )
                                .await
                                {
                                    Ok(evs) => (evs, false),
                                    Err(_) => (
                                        vec![TaskEvent::Failed {
                                            reason: format!("Agent task timed out after {secs}s"),
                                        }],
                                        true,
                                    ),
                                }
                            } else {
                                (executor.execute(&spec).await, false)
                            };
                            let mut output_parts = Vec::new();
                            let mut final_status = TaskStatus::Completed { exit_code: Some(0) };
                            for ev in &events {
                                inner_clone.emit_task_event(&id_spawn, ev.clone()).await;
                                match ev {
                                    crate::background::types::TaskEvent::Output { text } => {
                                        output_parts.push(text.clone());
                                    }
                                    crate::background::types::TaskEvent::Failed { reason } => {
                                        final_status = TaskStatus::Failed {
                                            reason: reason.clone(),
                                        };
                                    }
                                    crate::background::types::TaskEvent::Completed {
                                        exit_code,
                                    } => {
                                        final_status = TaskStatus::Completed {
                                            exit_code: *exit_code,
                                        };
                                    }
                                    _ => {}
                                }
                            }
                            let output_text = output_parts.join("\n");
                            let status_str = match &final_status {
                                TaskStatus::Completed { .. } => "completed",
                                TaskStatus::Failed { .. } => "failed",
                                _ => "completed",
                            };
                            let _ = inner_clone.store.update_task_status(
                                &id_spawn,
                                status_str,
                                Some(&output_text),
                            );
                            let mut tasks = inner_clone.tasks.lock().await;
                            if let Some(t) = tasks.get_mut(&id_spawn) {
                                t.status = final_status.clone();
                                if timed_out_agent {
                                    t.timed_out = true;
                                }
                            }
                            drop(tasks);
                            let reason = if timed_out_agent {
                                "timed_out"
                            } else {
                                match &final_status {
                                    TaskStatus::Failed { .. } => "failed",
                                    _ => "completed",
                                }
                            };
                            publish_terminal_bg_notification_inner(&inner_clone, &id_spawn, reason)
                                .await;
                            inner_clone.finish_task_event_channel(&id_spawn).await;
                        }
                        inner_clone.dec_running_for_agent().await;
                    });
                    self.inner
                        .aborts
                        .lock()
                        .await
                        .insert(id, handle.abort_handle());
                }
            }
        }
        Ok(())
    }
}

/// §8.3: `futures::Stream` over [`TaskEvent`]s from [`BackgroundTaskManager::submit_with_receiver`]
/// or [`BackgroundTaskManager::subscribe_task_results`] (skips `Lagged` receive gaps).
pub fn task_events_stream(
    rx: broadcast::Receiver<TaskEvent>,
) -> impl futures::stream::Stream<Item = TaskEvent> + Send {
    use futures::stream;
    use tokio::sync::broadcast::error::RecvError;
    stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((ev, rx)),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalRuntime;
    use crate::config::Config;
    use crate::feature_flags::{ExperimentalFeature, FeatureFlags};
    use crate::runtime::Runtime;
    use crate::session::Session;
    use crate::store::Store;
    use crate::wire::RootWireHub;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::RecvError;

    fn test_store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[tokio::test]
    async fn test_recover_restores_pending_task() {
        let store = test_store();
        let session_id = "test-session-1".to_string();

        // Create first manager and submit a task
        let mgr1 = BackgroundTaskManager::new(session_id.clone(), store.clone());
        let spec = TaskSpec {
            id: "task-1".to_string(),
            kind: TaskKind::Bash {
                command: "echo hello".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        mgr1.submit(spec).await.unwrap();

        // Create second manager (simulating process restart) and recover
        let mgr2 = BackgroundTaskManager::new(session_id.clone(), store.clone());
        mgr2.recover().await.unwrap();

        // Verify task was restored
        let status = mgr2.status("task-1").await;
        assert!(status.is_some());
        assert!(matches!(status.unwrap(), TaskStatus::Pending));
    }

    #[tokio::test]
    async fn test_recover_restores_multiple_tasks() {
        let store = test_store();
        let session_id = "test-session-2".to_string();

        let mgr1 = BackgroundTaskManager::new(session_id.clone(), store.clone());
        for i in 0..3 {
            let spec = TaskSpec {
                id: format!("task-{}", i),
                kind: TaskKind::Bash {
                    command: format!("echo {}", i),
                },
                created_at: chrono::Utc::now(),
                dependencies: vec![],
                max_retries: 0,
                timeout_s: None,
            };
            mgr1.submit(spec).await.unwrap();
        }

        // Simulate restart
        let mgr2 = BackgroundTaskManager::new(session_id.clone(), store.clone());
        mgr2.recover().await.unwrap();

        let tasks = mgr2.list().await;
        assert_eq!(tasks.len(), 3);
        for t in &tasks {
            assert!(matches!(t.status, TaskStatus::Pending));
        }
    }

    #[tokio::test]
    async fn test_recover_different_session_isolated() {
        let store = test_store();
        let session_a = "session-a".to_string();
        let session_b = "session-b".to_string();

        let mgr_a = BackgroundTaskManager::new(session_a.clone(), store.clone());
        let spec = TaskSpec {
            id: "task-a".to_string(),
            kind: TaskKind::Bash {
                command: "echo a".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        mgr_a.submit(spec).await.unwrap();

        // Recover session_b should not see session_a's tasks
        let mgr_b = BackgroundTaskManager::new(session_b.clone(), store.clone());
        mgr_b.recover().await.unwrap();

        let tasks = mgr_b.list().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_recover_task_with_agent_kind() {
        let store = test_store();
        let session_id = "test-session-agent".to_string();

        let mgr1 = BackgroundTaskManager::new(session_id.clone(), store.clone());
        let spec = TaskSpec {
            id: "agent-task-1".to_string(),
            kind: TaskKind::Agent {
                description: "test agent".to_string(),
                prompt: "do something".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        mgr1.submit(spec).await.unwrap();

        let mgr2 = BackgroundTaskManager::new(session_id.clone(), store.clone());
        mgr2.recover().await.unwrap();

        let tasks = mgr2.list().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "agent-task-1");
        assert!(matches!(tasks[0].spec.kind, TaskKind::Agent { .. }));
    }

    #[tokio::test]
    async fn test_heartbeat_updates_store() {
        let store = test_store();
        let session_id = "test-session-hb".to_string();

        let mgr = BackgroundTaskManager::new(session_id.clone(), store.clone());
        let spec = TaskSpec {
            id: "hb-task-1".to_string(),
            kind: TaskKind::Bash {
                command: "sleep 0.1".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        mgr.submit(spec).await.unwrap();

        // Wait for task to start and heartbeat
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Check that heartbeat_at was set by querying raw SQLite
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT heartbeat_at FROM tasks WHERE id = ?1")
            .unwrap();
        let mut rows = stmt.query(rusqlite::params!["hb-task-1"]).unwrap();
        if let Some(row) = rows.next().unwrap() {
            let hb: Option<String> = row.get(0).unwrap();
            assert!(hb.is_some(), "heartbeat_at should be set");
        } else {
            panic!("Task not found");
        }
    }

    #[tokio::test]
    async fn test_task_dependencies_block_until_complete() {
        let store = test_store();
        let session_id = "test-session-deps".to_string();

        let mgr = BackgroundTaskManager::new(session_id.clone(), store.clone());

        // Submit a dependent task before its dependency
        let dependent = TaskSpec {
            id: "dependent-task".to_string(),
            kind: TaskKind::Bash {
                command: "echo dependent".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec!["parent-task".to_string()],
            max_retries: 0,
            timeout_s: None,
        };
        mgr.submit(dependent).await.unwrap();

        // Dependent should stay pending because parent is not done
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let status = mgr.status("dependent-task").await.unwrap();
        assert!(
            matches!(status, TaskStatus::Pending),
            "Dependent task should be pending"
        );

        // Submit parent task
        let parent = TaskSpec {
            id: "parent-task".to_string(),
            kind: TaskKind::Bash {
                command: "echo parent".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        mgr.submit(parent).await.unwrap();

        // Wait for parent to complete
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Now parent should be completed
        let parent_status = mgr.status("parent-task").await.unwrap();
        assert!(
            matches!(parent_status, TaskStatus::Completed { .. }),
            "Parent should be completed"
        );

        // Dependent should now run (since parent is done and we call start_pending on submit)
        // But it might still be pending because start_pending only runs on submit
        // and we removed the recursive start_pending call. Let's verify:
        let dep_status = mgr.status("dependent-task").await.unwrap();
        // Since dependent was submitted first, it was checked but parent wasn't there.
        // When parent was submitted, start_pending ran again and should have started dependent.
        assert!(
            matches!(dep_status, TaskStatus::Completed { .. }),
            "Dependent should run after parent completes, got {:?}",
            dep_status
        );
    }

    #[tokio::test]
    async fn test_task_with_missing_dependency_stays_pending() {
        let store = test_store();
        let session_id = "test-session-missing".to_string();

        let mgr = BackgroundTaskManager::new(session_id.clone(), store.clone());

        let orphan = TaskSpec {
            id: "orphan-task".to_string(),
            kind: TaskKind::Bash {
                command: "echo orphan".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec!["nonexistent".to_string()],
            max_retries: 0,
            timeout_s: None,
        };
        mgr.submit(orphan).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = mgr.status("orphan-task").await.unwrap();
        assert!(
            matches!(status, TaskStatus::Pending),
            "Orphan task should stay pending"
        );
    }

    #[tokio::test]
    async fn test_bash_resubmits_on_failure_when_distributed_queue_and_max_retries() {
        let store = test_store();
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let session = Session::create(&store, std::env::current_dir().unwrap()).unwrap();
        let sid = session.id.clone();
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::DistributedQueue);
        let runtime = Runtime::with_features(
            Config::default(),
            session,
            approval,
            hub,
            store.clone(),
            features,
        );
        let mgr = runtime.bg_manager.clone();
        let spec = TaskSpec {
            id: "bash-fail-retry".to_string(),
            kind: TaskKind::Bash {
                command: "exit 9".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 1,
            timeout_s: None,
        };
        mgr.submit(spec).await.unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut n = 0usize;
        while tokio::time::Instant::now() < deadline {
            n = store.list_tasks(&sid).unwrap().len();
            if n >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        assert!(
            n >= 2,
            "expected retry task row in store (original + resubmit), got {}",
            n
        );
    }

    #[tokio::test]
    async fn test_list_empty_manager() {
        let store = test_store();
        let mgr = BackgroundTaskManager::new("empty-session".to_string(), store);
        let tasks = mgr.list().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_status_unknown_task() {
        let store = test_store();
        let mgr = BackgroundTaskManager::new("unknown-session".to_string(), store);
        assert!(mgr.status("no-such-task").await.is_none());
    }

    #[tokio::test]
    async fn test_bash_wall_clock_timeout_sets_timed_out() {
        let store = test_store();
        let sid = "session-bash-timeout".to_string();
        let mgr = BackgroundTaskManager::new(sid, store);
        let spec = TaskSpec {
            id: "slow-bash".to_string(),
            kind: TaskKind::Bash {
                command: "sleep 30".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: Some(1),
        };
        mgr.submit(spec).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        loop {
            if let Some(t) = mgr.list().await.into_iter().find(|t| t.id == "slow-bash") {
                if t.timed_out && matches!(t.status, TaskStatus::Failed { .. }) {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for bash wall-clock timeout"
            );
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
    }

    async fn recv_task_ev(rx: &mut broadcast::Receiver<TaskEvent>) -> TaskEvent {
        loop {
            match rx.recv().await {
                Ok(ev) => return ev,
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => panic!("task event receiver closed"),
            }
        }
    }

    #[tokio::test]
    async fn test_executor_caps_serialize_parallel_bash() {
        let store = test_store();
        let sid = "cap-bash-session".to_string();
        let mgr = BackgroundTaskManager::with_executor_caps(sid, store, 1, 4);
        assert_eq!(mgr.max_concurrent_bash(), 1);
        assert_eq!(mgr.max_concurrent_agent(), 4);
        assert_eq!(mgr.max_concurrent_tasks(), 5);

        let s1 = TaskSpec {
            id: "slow-bash".to_string(),
            kind: TaskKind::Bash {
                command: "sleep 0.35".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        let s2 = TaskSpec {
            id: "fast-bash".to_string(),
            kind: TaskKind::Bash {
                command: "echo queued".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        mgr.submit(s1).await.unwrap();
        mgr.submit(s2).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let st2 = mgr.status("fast-bash").await.expect("task exists");
        assert!(
            matches!(st2, TaskStatus::Pending),
            "second bash should wait for slot, got {:?}",
            st2
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let st2 = mgr.status("fast-bash").await.unwrap();
        assert!(
            matches!(st2, TaskStatus::Completed { .. }),
            "expected completion after first bash, got {:?}",
            st2
        );
    }

    #[tokio::test]
    async fn test_task_events_stream_collects_bash_lifecycle() {
        use futures::stream::StreamExt;
        let store = test_store();
        let mgr = BackgroundTaskManager::new("sess-stream-fn".into(), store);
        let spec = TaskSpec {
            id: "ev-stream-1".to_string(),
            kind: TaskKind::Bash {
                command: "echo evstream".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        let (_id, rx) = mgr.submit_with_receiver(spec).await.unwrap();
        let collected: Vec<_> = super::task_events_stream(rx).collect().await;
        assert!(collected.iter().any(|e| matches!(e, TaskEvent::Started)));
        assert!(collected.iter().any(|e| matches!(
            e,
            TaskEvent::Output { text } if text.contains("evstream")
        )));
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, TaskEvent::Completed { .. }))
        );
    }

    #[tokio::test]
    async fn test_submit_with_receiver_streams_bash_events() {
        let store = test_store();
        let session_id = "test-session-stream".to_string();
        let mgr = BackgroundTaskManager::new(session_id, store);
        let tid = "stream-task-1".to_string();
        let spec = TaskSpec {
            id: tid.clone(),
            kind: TaskKind::Bash {
                command: "echo streamline".to_string(),
            },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        let (_id, mut rx) = mgr.submit_with_receiver(spec).await.unwrap();
        assert!(matches!(recv_task_ev(&mut rx).await, TaskEvent::Started));
        let out = recv_task_ev(&mut rx).await;
        assert!(
            matches!(out, TaskEvent::Output { ref text } if text.contains("streamline")),
            "got {:?}",
            out
        );
        assert!(matches!(
            recv_task_ev(&mut rx).await,
            TaskEvent::Completed { .. }
        ));
    }
}
