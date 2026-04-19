//! UI frontends: print-mode and shell-mode rendering.
//!
//! `PrintUI` renders wire events to a string for non-interactive use.

use crate::wire::{Wire, WireEvent, WireEnvelope, MergedWireReceiver};

/// Print-mode UI: non-interactive, renders wire events to stdout.
#[allow(dead_code)]
pub struct PrintUI;

impl PrintUI {
    pub async fn run(wire: &Wire, _input: &str) -> anyhow::Result<String> {
        let mut rx = MergedWireReceiver::new(wire.subscribe());
        let mut output = String::new();

        // Wait for turn to complete
        while let Some(envelope) = rx.recv().await {
            Self::render_event(&envelope, &mut output);
            if matches!(envelope.event, WireEvent::TurnEnd) {
                break;
            }
        }

        // Flush any remaining buffered text
        if let Some(envelope) = rx.flush() {
            Self::render_event(&envelope, &mut output);
        }

        Ok(output)
    }

    fn render_event(envelope: &WireEnvelope, output: &mut String) {
        use WireEvent::*;
        match &envelope.event {
            TextPart { text } => output.push_str(text),
            ThinkPart { text } => output.push_str(&format!("[thinking: {}]\n", text)),
            ToolCall { id, function } => {
                output.push_str(&format!("\n[Tool {}] {}({})\n", id, function.name, function.arguments));
            }
            ToolResult { tool_call_id, output: result, is_error, elapsed_ms } => {
                output.push_str(&format!(
                    "[Result {}] error={} elapsed={:?}\n{}\n",
                    tool_call_id, is_error, elapsed_ms, result
                ));
            }
            TurnBegin { user_input } => {
                output.push_str(&format!("## Turn: {}\n\n", user_input.text));
            }
            SessionShutdown { reason } => {
                output.push_str(&format!("\n[SessionShutdown: {}]\n", reason));
            }
            TurnEnd => output.push_str("\n---\n"),
            StepBegin { n } => output.push_str(&format!("\n[Step {}]\n", n)),
            StatusUpdate { token_count, context_size, plan_mode, mcp_status } => {
                output.push_str(&format!(
                    "[Status] tokens={}/{} plan={} mcp={}\n",
                    token_count, context_size, plan_mode, mcp_status
                ));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_print_ui_renders_text() {
        let wire = Wire::new(1024);
        let wire2 = wire.clone();

        let handle = tokio::spawn(async move {
            PrintUI::run(&wire2, "test").await.unwrap()
        });

        // Give the subscriber time to register
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        wire.send(WireEvent::TextPart { text: "Hello".to_string() });
        wire.send(WireEvent::TurnEnd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle
        ).await.unwrap().unwrap();
        assert!(output.contains("Hello"));
    }

    #[tokio::test]
    async fn test_print_ui_renders_tool_call() {
        let wire = Wire::new(1024);
        let wire2 = wire.clone();

        let handle = tokio::spawn(async move {
            PrintUI::run(&wire2, "test").await.unwrap()
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        wire.send(WireEvent::ToolCall {
            id: "tc-1".to_string(),
            function: crate::message::FunctionCall { name: "shell".to_string(), arguments: r#"{"command":"echo hi"}"#.to_string() },
        });
        wire.send(WireEvent::TurnEnd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle
        ).await.unwrap().unwrap();
        assert!(output.contains("shell"));
    }

    #[tokio::test]
    async fn test_print_ui_renders_think_part() {
        let wire = Wire::new(1024);
        let wire2 = wire.clone();

        let handle = tokio::spawn(async move {
            PrintUI::run(&wire2, "test").await.unwrap()
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        wire.send(WireEvent::ThinkPart { text: "pondering".to_string() });
        wire.send(WireEvent::TurnEnd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle
        ).await.unwrap().unwrap();
        assert!(output.contains("thinking"));
    }

    #[tokio::test]
    async fn test_print_ui_renders_turn_begin() {
        let wire = Wire::new(1024);
        let wire2 = wire.clone();

        let handle = tokio::spawn(async move {
            PrintUI::run(&wire2, "hello world").await.unwrap()
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        wire.send(WireEvent::TurnBegin {
            user_input: crate::wire::UserInput::text_only("hello world"),
        });
        wire.send(WireEvent::TurnEnd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle
        ).await.unwrap().unwrap();
        assert!(output.contains("Turn"));
    }
}
