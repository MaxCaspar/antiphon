use std::path::PathBuf;

use antiphon::agent::AgentSpec;
use antiphon::conversation::{
    self, ConversationConfig, ConversationControl, ConversationEvent, RoutingMode,
};
use assert_cmd::Command;
use predicates::prelude::*;
use tokio::sync::{mpsc, watch};

fn fixture(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p.to_string_lossy().to_string()
}

#[test]
fn debug_happy_path_with_mock_agents() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    cmd.args([
        "--debug",
        "--turns",
        "2",
        "--agent-a",
        &fixture("mock_claude.sh"),
        "--agent-b",
        &fixture("mock_codex.sh"),
        "--",
        "start the exchange",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Claude says hello."))
    .stdout(predicate::str::contains("Codex replies hi."))
    .stdout(predicate::str::contains("== Done =="));
}

#[test]
fn debug_quiet_suppresses_output() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    cmd.args([
        "--debug",
        "--quiet",
        "--turns",
        "2",
        "--agent-a",
        &fixture("mock_claude.sh"),
        "--agent-b",
        &fixture("mock_codex.sh"),
        "--",
        "start the exchange",
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());
}

#[test]
fn subprocess_failure_path() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    cmd.args([
        "--debug",
        "--output",
        "json",
        "--turns",
        "1",
        "--agent-a",
        &fixture("mock_fail.sh"),
        "--",
        "start",
    ])
    .assert()
    .failure()
    .stdout(predicate::str::contains("\"code\": \"non_zero_exit\""));
}

#[tokio::test]
async fn codex_reasoning_events_emit_thinking_without_duplicates() {
    let cfg = ConversationConfig {
        agents: [
            AgentSpec {
                cmd: fixture("mock_codex_reasoning_stream.sh"),
            },
            AgentSpec {
                cmd: fixture("mock_codex.sh"),
            },
        ],
        turns: 1,
        initial_prompt: "start".to_string(),
        routing_mode: RoutingMode::PromptOnlyToAgentA,
        debug: true,
        audit: None,
        agent_system_prompts: [String::new(), String::new()],
    };

    let (events_tx, mut events_rx) = mpsc::channel(128);
    let (_control_tx, control_rx) = watch::channel(ConversationControl::Run);
    let transcript = conversation::run(cfg, events_tx, control_rx)
        .await
        .expect("conversation run should succeed");

    let mut event_trace: Vec<(&'static str, String)> = Vec::new();
    while let Some(event) = events_rx.recv().await {
        match event {
            ConversationEvent::Thinking { text, .. } => event_trace.push(("thinking", text)),
            ConversationEvent::Token { text, .. } => event_trace.push(("token", text)),
            ConversationEvent::TurnStart { .. }
            | ConversationEvent::TurnDone { .. }
            | ConversationEvent::Done
            | ConversationEvent::ToolEvent { .. }
            | ConversationEvent::Error { .. } => {}
        }
    }

    assert_eq!(
        event_trace,
        vec![
            ("thinking", "think-a".to_string()),
            ("thinking", "think-b".to_string()),
            ("thinking", "think-c".to_string()),
            ("token", "assistant token".to_string()),
        ]
    );
    let thinking: Vec<&str> = event_trace
        .iter()
        .filter_map(|(kind, text)| (*kind == "thinking").then_some(text.as_str()))
        .collect();
    assert!(!thinking.iter().any(|t| t.contains("ignore-me")));
    assert_eq!(
        thinking.iter().filter(|t| **t == "think-a").count(),
        1,
        "each input reasoning line should produce at most one thinking event"
    );
    assert_eq!(
        thinking.iter().filter(|t| **t == "think-b").count(),
        1,
        "each input reasoning line should produce at most one thinking event"
    );
    assert_eq!(
        thinking.iter().filter(|t| **t == "think-c").count(),
        1,
        "each input reasoning line should produce at most one thinking event"
    );
    assert!(
        transcript.render().contains("assistant token"),
        "assistant token text should still be present in transcript output"
    );
}

#[tokio::test]
async fn codex_multiple_agent_messages_are_separated_by_blank_line() {
    let cfg = ConversationConfig {
        agents: [
            AgentSpec {
                cmd: fixture("mock_codex_two_messages.sh"),
            },
            AgentSpec {
                cmd: fixture("mock_codex.sh"),
            },
        ],
        turns: 1,
        initial_prompt: "start".to_string(),
        routing_mode: RoutingMode::PromptOnlyToAgentA,
        debug: true,
        audit: None,
        agent_system_prompts: [String::new(), String::new()],
    };

    let (events_tx, _events_rx) = mpsc::channel(128);
    let (_control_tx, control_rx) = watch::channel(ConversationControl::Run);
    let transcript = conversation::run(cfg, events_tx, control_rx)
        .await
        .expect("conversation run should succeed");

    assert!(
        transcript
            .render()
            .contains("Aria: First message.\n\nSecond message."),
        "discrete codex agent messages in one turn should be separated by a blank line"
    );
}

#[tokio::test]
async fn codex_tool_events_emit_once_per_stream_line_after_single_pass_decode() {
    let cfg = ConversationConfig {
        agents: [
            AgentSpec {
                cmd: fixture("mock_codex_tool_stream.sh"),
            },
            AgentSpec {
                cmd: fixture("mock_codex.sh"),
            },
        ],
        turns: 1,
        initial_prompt: "start".to_string(),
        routing_mode: RoutingMode::PromptOnlyToAgentA,
        debug: true,
        audit: None,
        agent_system_prompts: [String::new(), String::new()],
    };

    let (events_tx, mut events_rx) = mpsc::channel(128);
    let (_control_tx, control_rx) = watch::channel(ConversationControl::Run);
    let transcript = conversation::run(cfg, events_tx, control_rx)
        .await
        .expect("conversation run should succeed");

    let mut use_count = 0usize;
    let mut result_count = 0usize;
    while let Some(event) = events_rx.recv().await {
        if let ConversationEvent::ToolEvent { kind, text, .. } = event {
            if text == "bash: echo hi" {
                match kind {
                    antiphon::agent::ToolStreamEventKind::Use => use_count += 1,
                    antiphon::agent::ToolStreamEventKind::Result => result_count += 1,
                    antiphon::agent::ToolStreamEventKind::Error => {}
                }
            }
        }
    }

    assert_eq!(
        use_count, 1,
        "tool use event should be emitted exactly once"
    );
    assert_eq!(
        result_count, 1,
        "tool result event should be emitted exactly once"
    );
    assert!(
        transcript.render().contains("tool stream done"),
        "agent message token should still be preserved in transcript output"
    );
}

#[tokio::test]
async fn codex_interleaved_reasoning_and_tool_events_preserve_order_without_duplicates() {
    let cfg = ConversationConfig {
        agents: [
            AgentSpec {
                cmd: fixture("mock_codex_interleaved_stream.sh"),
            },
            AgentSpec {
                cmd: fixture("mock_codex.sh"),
            },
        ],
        turns: 1,
        initial_prompt: "start".to_string(),
        routing_mode: RoutingMode::PromptOnlyToAgentA,
        debug: true,
        audit: None,
        agent_system_prompts: [String::new(), String::new()],
    };

    let (events_tx, mut events_rx) = mpsc::channel(128);
    let (_control_tx, control_rx) = watch::channel(ConversationControl::Run);
    let transcript = conversation::run(cfg, events_tx, control_rx)
        .await
        .expect("conversation run should succeed");

    let mut event_trace: Vec<(&'static str, String)> = Vec::new();
    while let Some(event) = events_rx.recv().await {
        match event {
            ConversationEvent::Thinking { text, .. } => event_trace.push(("thinking", text)),
            ConversationEvent::ToolEvent { kind, text, .. } => match kind {
                antiphon::agent::ToolStreamEventKind::Use => event_trace.push(("tool_use", text)),
                antiphon::agent::ToolStreamEventKind::Result => {
                    event_trace.push(("tool_result", text))
                }
                antiphon::agent::ToolStreamEventKind::Error => {
                    event_trace.push(("tool_error", text))
                }
            },
            ConversationEvent::Token { text, .. } => event_trace.push(("token", text)),
            ConversationEvent::TurnStart { .. }
            | ConversationEvent::TurnDone { .. }
            | ConversationEvent::Done
            | ConversationEvent::Error { .. } => {}
        }
    }

    assert_eq!(
        event_trace,
        vec![
            ("thinking", "think-a".to_string()),
            ("tool_use", "bash: echo hi".to_string()),
            ("thinking", "think-b".to_string()),
            ("tool_result", "bash: echo hi".to_string()),
            ("thinking", "think-c".to_string()),
            ("token", "interleaved stream done".to_string()),
        ]
    );
    assert_eq!(
        event_trace
            .iter()
            .filter(|(kind, text)| *kind == "thinking" && text == "think-a")
            .count(),
        1,
        "reasoning delta should emit exactly once"
    );
    assert_eq!(
        event_trace
            .iter()
            .filter(|(kind, text)| *kind == "thinking" && text == "think-b")
            .count(),
        1,
        "reasoning summary should emit exactly once"
    );
    assert_eq!(
        event_trace
            .iter()
            .filter(|(kind, text)| *kind == "thinking" && text == "think-c")
            .count(),
        1,
        "reasoning delta should emit exactly once"
    );
    assert_eq!(
        event_trace
            .iter()
            .filter(|(kind, text)| *kind == "tool_use" && text == "bash: echo hi")
            .count(),
        1,
        "tool use should emit exactly once"
    );
    assert_eq!(
        event_trace
            .iter()
            .filter(|(kind, text)| *kind == "tool_result" && text == "bash: echo hi")
            .count(),
        1,
        "tool result should emit exactly once"
    );
    assert!(
        transcript.render().contains("interleaved stream done"),
        "agent message token should still be preserved in transcript output"
    );
}
