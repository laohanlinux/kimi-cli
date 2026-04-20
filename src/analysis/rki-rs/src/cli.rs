//! Command-line argument parser.
//!
//! `Cli` defines the `rki` binary interface with clap derive macros.

use clap::Parser;

const AFTER_HELP: &str = r"Environment (subset):
  KIMI_SUPPORTS_VISION       0/false/no or 1/true/yes — multimodal input gate (see user_input::resolve_supports_vision).
  Programmatic API: `KimiSoul::run` accepts `turn_input::TurnInput` / `&str` / `String`; wire `TurnBegin.user_input.parts` carries non-text media when present.
  Stdin JSON (one line): lines starting with `{` or `[` are parsed by `turn_input::parse_cli_turn_line` (non-empty `parts` array, `text` string, or `role:user` message JSON).
  Config `[models.vision_by_model]`: per-model booleans override `model_supports_vision_hint` for `default_model` when `ignore_vision_model_hint` is false.
  KIMI_IGNORE_VISION_MODEL_HINT  1/true — skip echo/mock vision hint; use supports_vision only.
  RKI_UI_SHUTDOWN_WAIT_SECS   If >0, bound interactive UI task join on exit (§1.2 L35); 0 = wait forever.
  RKI_ACP_TOKEN               When set with `--acp`, require `Authorization: Bearer <token>` on POST /turn and GET /events (health and GET /turn hint stay open).
  RKI_ACP_MAX_REQUEST_BYTES   Cap for a single ACP HTTP read (headers+body), default 262144; clamped 4096..16777216.
  KIMI_EXPERIMENTAL_SUBAGENT_WIRE_PERSISTENCE  1/true — persist forwarded subagent wire rows to parent session (§6.5).

Config file ([models]): supports_vision, ignore_vision_model_hint (see config_registry ModelsSection).";

#[derive(Parser, Debug)]
#[command(name = "rki")]
#[command(about = "Rust Kimi CLI Agent")]
#[command(after_help = AFTER_HELP)]
pub struct Cli {
    #[arg(long, help = "Resume the latest session")]
    pub resume: bool,
    #[arg(long, help = "Resume a specific session by ID")]
    pub session: Option<String>,
    #[arg(long, help = "Auto-approve all destructive operations")]
    pub yolo: bool,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Comma-separated list of actions to auto-approve"
    )]
    pub auto_approve: Option<Vec<String>>,
    #[arg(long, help = "Model name to use")]
    pub model: Option<String>,
    #[arg(long, help = "Working directory")]
    pub work_dir: Option<String>,
    #[arg(long, help = "Enter plan mode on startup (read-only research)")]
    pub plan: bool,
    #[arg(long, help = "Enter Ralph automated iteration mode")]
    pub ralph: bool,
    #[arg(long, help = "Non-interactive print mode (one-shot output)")]
    pub print: bool,
    #[arg(long, help = "Archive the resumed session on exit")]
    pub archive: bool,
    #[arg(long, help = "List sessions and exit")]
    pub list_sessions: bool,
    /// Print unified session timeline (context/wire/notification/task/state) as JSON lines and exit.
    #[arg(
        long = "show-unified-events",
        value_name = "SESSION_ID",
        conflicts_with = "list_sessions"
    )]
    pub show_unified_events: Option<String>,
    /// Max rows for `--show-unified-events` (default 500, max 10000).
    #[arg(long = "unified-events-limit", default_value_t = 500)]
    pub unified_events_limit: usize,
    /// Only events with `created_at` strictly after this timestamp (SQLite `datetime` format, e.g. `2026-01-01 00:00:00`).
    #[arg(long = "unified-events-after", value_name = "CREATED_AT")]
    pub unified_events_after: Option<String>,
    /// New session forked from this parent session id (copies context; see `--fork-context-up-to-id`).
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with = "session",
        conflicts_with = "resume"
    )]
    pub fork_from: Option<String>,
    /// Start ACP (Agent Communication Protocol) SSE server on the given port for IDE integrations.
    #[arg(long, value_name = "PORT", conflicts_with = "print")]
    pub acp: Option<u16>,
    /// With `--fork-from`, copy only `context_entries` rows with id ≤ this value (omit for full copy).
    #[arg(long, value_name = "ROW_ID", requires = "fork_from")]
    pub fork_context_up_to_id: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_default_args() {
        let cli = Cli::try_parse_from(["rki"]).unwrap();
        assert!(!cli.resume);
        assert!(!cli.yolo);
        assert!(cli.auto_approve.is_none());
        assert!(cli.model.is_none());
        assert!(cli.work_dir.is_none());
    }

    #[test]
    fn test_cli_all_flags() {
        let cli = Cli::try_parse_from([
            "rki",
            "--resume",
            "--yolo",
            "--auto-approve",
            "read_file,write_file",
            "--model",
            "gpt-4",
            "--work-dir",
            "/tmp",
        ])
        .unwrap();
        assert!(cli.resume);
        assert!(cli.yolo);
        assert_eq!(
            cli.auto_approve,
            Some(vec!["read_file".to_string(), "write_file".to_string()])
        );
        assert_eq!(cli.model, Some("gpt-4".to_string()));
        assert_eq!(cli.work_dir, Some("/tmp".to_string()));
    }

    #[test]
    fn test_cli_model_only() {
        let cli = Cli::try_parse_from(["rki", "--model", "claude"]).unwrap();
        assert!(!cli.resume);
        assert_eq!(cli.model, Some("claude".to_string()));
    }

    #[test]
    fn test_cli_resume_only() {
        let cli = Cli::try_parse_from(["rki", "--resume"]).unwrap();
        assert!(cli.resume);
        assert!(!cli.yolo);
    }

    #[test]
    fn test_cli_plan_flag() {
        let cli = Cli::try_parse_from(["rki", "--plan"]).unwrap();
        assert!(cli.plan);
        assert!(!cli.ralph);
    }

    #[test]
    fn test_cli_ralph_flag() {
        let cli = Cli::try_parse_from(["rki", "--ralph"]).unwrap();
        assert!(cli.ralph);
        assert!(!cli.plan);
    }

    #[test]
    fn test_cli_plan_and_ralph_mutually_exclusive_in_usage() {
        // Both can be parsed; runtime will handle precedence
        let cli = Cli::try_parse_from(["rki", "--plan", "--ralph"]).unwrap();
        assert!(cli.plan);
        assert!(cli.ralph);
    }

    #[test]
    fn test_cli_print_flag() {
        let cli = Cli::try_parse_from(["rki", "--print"]).unwrap();
        assert!(cli.print);
        assert!(!cli.plan);
    }

    #[test]
    fn test_cli_session_flag() {
        let cli = Cli::try_parse_from(["rki", "--session", "abc-123"]).unwrap();
        assert_eq!(cli.session, Some("abc-123".to_string()));
        assert!(!cli.resume);
    }

    #[test]
    fn test_cli_archive_flag() {
        let cli = Cli::try_parse_from(["rki", "--archive"]).unwrap();
        assert!(cli.archive);
    }

    #[test]
    fn test_cli_list_sessions_flag() {
        let cli = Cli::try_parse_from(["rki", "--list-sessions"]).unwrap();
        assert!(cli.list_sessions);
    }

    #[test]
    fn test_cli_fork_from_flags() {
        let cli = Cli::try_parse_from([
            "rki",
            "--fork-from",
            "parent-uuid",
            "--fork-context-up-to-id",
            "42",
        ])
        .unwrap();
        assert_eq!(cli.fork_from.as_deref(), Some("parent-uuid"));
        assert_eq!(cli.fork_context_up_to_id, Some(42));
    }

    #[test]
    fn test_cli_fork_from_conflicts_with_session() {
        let err = Cli::try_parse_from(["rki", "--fork-from", "p", "--session", "s"]).unwrap_err();
        assert!(
            err.to_string().contains("cannot be used with") || err.to_string().contains("conflict")
        );
    }

    #[test]
    fn test_cli_show_unified_events_flags() {
        let cli = Cli::try_parse_from([
            "rki",
            "--show-unified-events",
            "sess-1",
            "--unified-events-limit",
            "50",
            "--unified-events-after",
            "2026-01-01 00:00:00",
        ])
        .unwrap();
        assert_eq!(cli.show_unified_events.as_deref(), Some("sess-1"));
        assert_eq!(cli.unified_events_limit, 50);
        assert_eq!(
            cli.unified_events_after.as_deref(),
            Some("2026-01-01 00:00:00")
        );
    }
}
