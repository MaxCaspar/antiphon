use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::agent::{self, AgentSession, AgentSpec, ToolStreamEventKind};
use crate::audit::AuditSet;
use crate::error::{AppError, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingMode {
    PromptOnlyToAgentA,
    PromptToAAndB,
}

impl RoutingMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PromptOnlyToAgentA => "prompt_only_to_agent_a",
            Self::PromptToAAndB => "prompt_to_a_and_b",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::PromptOnlyToAgentA => "prompt-only->A",
            Self::PromptToAAndB => "prompt-to-A-and-B",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::PromptOnlyToAgentA => Self::PromptToAAndB,
            Self::PromptToAAndB => Self::PromptOnlyToAgentA,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConversationEvent {
    TurnStart {
        agent_idx: usize,
    },
    Token {
        agent_idx: usize,
        text: String,
    },
    Thinking {
        agent_idx: usize,
        text: String,
    },
    ToolEvent {
        agent_idx: usize,
        kind: ToolStreamEventKind,
        tool_type: String,
        text: String,
        tool_call_id: Option<String>,
    },
    TurnDone {
        agent_idx: usize,
    },
    Done,
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationControl {
    Run,
    Pause,
    Stop,
}

#[derive(Debug, Clone)]
pub struct Transcript {
    initial_prompt: String,
    entries: Vec<(usize, String)>,
}

impl Transcript {
    pub fn new(initial_prompt: String) -> Self {
        Self {
            initial_prompt,
            entries: Vec::new(),
        }
    }

    pub fn push_reply(&mut self, agent_idx: usize, reply: String) {
        let trimmed = reply.trim();
        if !trimmed.is_empty() {
            self.entries.push((agent_idx, trimmed.to_owned()));
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.initial_prompt);
        out.push_str("\n\n");
        for (idx, msg) in &self.entries {
            let name = agent_name(*idx);
            out.push_str(name);
            out.push_str(": ");
            out.push_str(msg);
            out.push_str("\n\n");
        }
        out
    }

    pub fn prompt_for_agent_turn(
        &self,
        turn: usize,
        mode: RoutingMode,
        agent_system_prompts: &[String; 2],
    ) -> String {
        let agent_idx = turn % 2;
        let base = match mode {
            RoutingMode::PromptOnlyToAgentA => self.prompt_relay_latest_reply(turn),
            RoutingMode::PromptToAAndB => self.prompt_to_a_and_b(turn),
        };
        inject_system_prompt(&agent_system_prompts[agent_idx], base)
    }

    fn prompt_relay_latest_reply(&self, turn: usize) -> String {
        let speaker_idx = turn % 2;
        let listener_idx = (speaker_idx + 1) % 2;
        let instruction = "Reply directly to the other assistant in second person (\"you\"). \
Do not narrate or evaluate the other assistant in third person and do not describe the exchange itself.";

        if turn == 0 {
            return self.initial_prompt.clone();
        }

        if let Some((idx, msg)) = self.entries.last() {
            if *idx != listener_idx {
                // Defensive fallback: in normal flow the latest message is from the listener.
                return format!("{instruction}\n\n{msg}");
            }
            // Manual relay behavior: pass through only the other agent's latest text.
            return msg.clone();
        }

        // Fallback if no prior non-empty reply was recorded.
        format!("{instruction}\n\nUser prompt:\n{}", self.initial_prompt)
    }

    fn prompt_to_a_and_b(&self, turn: usize) -> String {
        if turn == 0 {
            // "Prompt to A" path: pass the user prompt cleanly.
            return self.initial_prompt.clone();
        }

        let speaker_idx = turn % 2;
        let listener_idx = (speaker_idx + 1) % 2;

        if let Some((idx, msg)) = self.entries.last() {
            // On B's first turn, include the initial prompt as explicit context.
            if turn == 1 && speaker_idx == 1 && *idx == listener_idx {
                return format!(
                    "You are in a conversation with another assistant.\n\
Initial user prompt:\n{}\n\n\
The other assistant's latest reply:\n{}\n\n\
Respond directly to the other assistant in second person (\"you\").",
                    self.initial_prompt, msg
                );
            }

            return msg.clone();
        }

        self.initial_prompt.clone()
    }
}

fn inject_system_prompt(system: &str, prompt: String) -> String {
    if system.is_empty() {
        return prompt;
    }
    format!("[System instructions for this agent]\n{system}\n\n[Conversation prompt]\n{prompt}")
}

#[derive(Debug, Clone)]
pub struct ConversationConfig {
    pub agents: [AgentSpec; 2],
    pub turns: usize,
    pub initial_prompt: String,
    pub routing_mode: RoutingMode,
    pub debug: bool,
    pub audit: Option<Arc<AuditSet>>,
    pub agent_system_prompts: [String; 2],
}

pub async fn run(
    cfg: ConversationConfig,
    events_tx: mpsc::Sender<ConversationEvent>,
    mut control_rx: watch::Receiver<ConversationControl>,
) -> Result<Transcript, AppError> {
    if let Some(audit) = &cfg.audit {
        audit.log_run(json!({
            "event":"run.start",
            "turns": cfg.turns,
            "routing_mode": cfg.routing_mode.as_str(),
            "agent_a": cfg.agents[0].cmd,
            "agent_b": cfg.agents[1].cmd
        }));
        audit.live_line(&format!(
            "[run.start] turns={} routing_mode={} agent_a={} agent_b={}",
            cfg.turns,
            cfg.routing_mode.as_str(),
            cfg.agents[0].cmd,
            cfg.agents[1].cmd
        ));
    }

    let mut transcript = Transcript::new(cfg.initial_prompt);
    let mut sessions = [AgentSession::default(), AgentSession::default()];

    for turn in 0..cfg.turns {
        wait_if_paused(&mut control_rx).await?;
        if *control_rx.borrow() == ConversationControl::Stop {
            send_event(
                &events_tx,
                ConversationEvent::Error {
                    code: ErrorCode::Cancelled,
                    message: "conversation stopped".to_string(),
                },
            )
            .await;
            return Err(AppError::Cancelled);
        }

        let agent_idx = turn % 2;
        send_event(&events_tx, ConversationEvent::TurnStart { agent_idx }).await;

        let prompt =
            transcript.prompt_for_agent_turn(turn, cfg.routing_mode, &cfg.agent_system_prompts);
        let turn_start = Instant::now();
        if let Some(audit) = &cfg.audit {
            audit.log_run(json!({
                "event":"turn.start",
                "turn": turn + 1,
                "agent_idx": agent_idx,
                "agent_name": agent_name(agent_idx),
                "agent_cmd": cfg.agents[agent_idx].cmd,
                "prompt": prompt
            }));
            audit.live_line(&format!(
                "\n[turn.start] turn={} agent={} cmd={}",
                turn + 1,
                agent_name(agent_idx),
                cfg.agents[agent_idx].cmd
            ));
            audit.live_line(&format!("prompt: {}", prompt.replace('\n', "\\n")));
            audit.live_agent_line(
                agent_idx,
                &format!(
                    "\n[turn.start] turn={} agent={} cmd={}",
                    turn + 1,
                    agent_name(agent_idx),
                    cfg.agents[agent_idx].cmd
                ),
            );
        }
        let mut per_turn = String::new();
        let mut token_coalescer = TokenCoalescer::new(Instant::now());
        let audit_for_token = cfg.audit.clone();
        let audit_for_raw = cfg.audit.clone();
        let turn_no = turn + 1;
        let agent_name_now = agent_name(agent_idx);
        let cmd_summary = agent::invocation_summary(&cfg.agents[agent_idx], &sessions[agent_idx]);
        if let Some(audit) = &cfg.audit {
            audit.live_line(&format!("[exec][{}] {}", agent_name_now, cmd_summary));
            audit.live_agent_line(agent_idx, &format!("[exec] {}", cmd_summary));
        }

        let streamed = agent::stream_reply(
            &cfg.agents[agent_idx],
            &prompt,
            cfg.debug,
            &mut sessions[agent_idx],
            |line, thinking, tool_events| {
                if let Some(audit) = &audit_for_raw {
                    audit.live_line(&format!("[raw][{}] {}", agent_name_now, line));
                    audit.live_agent_line(agent_idx, &format!("[raw] {}", line));
                }
                if let Some(thinking) = thinking {
                    let _ = events_tx.try_send(ConversationEvent::Thinking {
                        agent_idx,
                        text: thinking,
                    });
                }
                for tool_event in tool_events {
                    let _ = events_tx.try_send(ConversationEvent::ToolEvent {
                        agent_idx,
                        kind: tool_event.kind,
                        tool_type: tool_event.tool_type,
                        text: tool_event.label,
                        tool_call_id: tool_event.tool_call_id,
                    });
                }
            },
            |token| {
                per_turn.push_str(token);
                let maybe_chunk = token_coalescer.push(token, Instant::now());
                let tx = events_tx.clone();
                let audit = audit_for_token.clone();
                async move {
                    if let Some(chunk) = maybe_chunk {
                        emit_token_chunk(&tx, audit, turn_no, agent_idx, agent_name_now, chunk)
                            .await;
                    }
                }
            },
        )
        .await;

        if let Some(chunk) = token_coalescer.flush(Instant::now()) {
            emit_token_chunk(
                &events_tx,
                cfg.audit.clone(),
                turn_no,
                agent_idx,
                agent_name_now,
                chunk,
            )
            .await;
        }

        match streamed {
            Ok(reply) => {
                if per_turn.is_empty() {
                    per_turn = reply;
                }
                transcript.push_reply(agent_idx, per_turn);
                if let Some(audit) = &cfg.audit {
                    audit.log_run(json!({
                        "event":"turn.done",
                        "turn": turn + 1,
                        "agent_idx": agent_idx,
                        "agent_name": agent_name(agent_idx),
                        "duration_ms": turn_start.elapsed().as_millis(),
                        "reply": transcript.entries.last().map(|(_,m)|m.clone()).unwrap_or_default()
                    }));
                    audit.log_agent(agent_idx, json!({
                        "event":"turn.done",
                        "turn": turn + 1,
                        "duration_ms": turn_start.elapsed().as_millis(),
                        "reply": transcript.entries.last().map(|(_,m)|m.clone()).unwrap_or_default()
                    }));
                    audit.live_line(&format!(
                        "[turn.done] turn={} agent={} duration_ms={}",
                        turn + 1,
                        agent_name(agent_idx),
                        turn_start.elapsed().as_millis()
                    ));
                    audit.live_agent_line(
                        agent_idx,
                        &format!(
                            "[turn.done] turn={} duration_ms={}",
                            turn + 1,
                            turn_start.elapsed().as_millis()
                        ),
                    );
                }
                send_event(&events_tx, ConversationEvent::TurnDone { agent_idx }).await;
            }
            Err(err) => {
                if let Some(audit) = &cfg.audit {
                    audit.log_run(json!({
                        "event":"turn.error",
                        "turn": turn + 1,
                        "agent_idx": agent_idx,
                        "agent_name": agent_name(agent_idx),
                        "duration_ms": turn_start.elapsed().as_millis(),
                        "code": err.code().as_str(),
                        "message": err.to_string()
                    }));
                    audit.log_agent(
                        agent_idx,
                        json!({
                            "event":"turn.error",
                            "turn": turn + 1,
                            "duration_ms": turn_start.elapsed().as_millis(),
                            "code": err.code().as_str(),
                            "message": err.to_string()
                        }),
                    );
                    audit.live_line(&format!(
                        "[turn.error] turn={} agent={} code={} message={}",
                        turn + 1,
                        agent_name(agent_idx),
                        err.code().as_str(),
                        err
                    ));
                    audit.live_agent_line(
                        agent_idx,
                        &format!(
                            "[turn.error] turn={} code={} message={}",
                            turn + 1,
                            err.code().as_str(),
                            err
                        ),
                    );
                }
                send_event(&events_tx, ConversationEvent::TurnDone { agent_idx }).await;
                send_event(
                    &events_tx,
                    ConversationEvent::Error {
                        code: err.code(),
                        message: err.to_string(),
                    },
                )
                .await;
                return Err(err);
            }
        }
    }

    if let Some(audit) = &cfg.audit {
        audit.log_run(json!({"event":"run.done"}));
        audit.live_line("[run.done]");
    }
    send_event(&events_tx, ConversationEvent::Done).await;
    Ok(transcript)
}

const TOKEN_COALESCE_MAX_BYTES: usize = 256;
const TOKEN_COALESCE_MAX_AGE: Duration = Duration::from_millis(8);

#[derive(Debug, Default)]
struct TokenCoalescer {
    buffer: String,
    since: Option<Instant>,
}

impl TokenCoalescer {
    fn new(now: Instant) -> Self {
        Self {
            buffer: String::new(),
            since: Some(now),
        }
    }

    fn push(&mut self, token: &str, now: Instant) -> Option<String> {
        if self.buffer.is_empty() {
            self.since = Some(now);
        }
        self.buffer.push_str(token);

        let age_limit_reached = self
            .since
            .map(|since| now.saturating_duration_since(since) >= TOKEN_COALESCE_MAX_AGE)
            .unwrap_or(false);
        if self.buffer.len() >= TOKEN_COALESCE_MAX_BYTES || age_limit_reached {
            return self.flush(now);
        }

        None
    }

    fn flush(&mut self, now: Instant) -> Option<String> {
        if self.buffer.is_empty() {
            self.since = Some(now);
            return None;
        }
        self.since = Some(now);
        Some(std::mem::take(&mut self.buffer))
    }
}

async fn emit_token_chunk(
    tx: &mpsc::Sender<ConversationEvent>,
    audit: Option<Arc<AuditSet>>,
    turn_no: usize,
    agent_idx: usize,
    agent_name_now: &'static str,
    chunk: String,
) {
    if let Some(audit) = audit {
        audit.log_run(json!({
            "event":"turn.token",
            "turn": turn_no,
            "agent_idx": agent_idx,
            "agent_name": agent_name_now,
            "text": chunk.clone()
        }));
        audit.log_agent(
            agent_idx,
            json!({
                "event":"token",
                "turn": turn_no,
                "agent_name": agent_name_now,
                "text": chunk.clone()
            }),
        );
        audit.live_line(&format!("[token][{}] {}", agent_name_now, chunk));
        audit.live_agent_line(agent_idx, &format!("[token] {}", chunk));
    }
    let _ = tx
        .send(ConversationEvent::Token {
            agent_idx,
            text: chunk,
        })
        .await;
}

async fn wait_if_paused(
    control_rx: &mut watch::Receiver<ConversationControl>,
) -> Result<(), AppError> {
    loop {
        let state = *control_rx.borrow();
        match state {
            ConversationControl::Run => return Ok(()),
            ConversationControl::Stop => return Err(AppError::Cancelled),
            ConversationControl::Pause => {
                if control_rx.changed().await.is_err() {
                    return Err(AppError::Cancelled);
                }
            }
        }
    }
}

async fn send_event(tx: &mpsc::Sender<ConversationEvent>, event: ConversationEvent) {
    let _ = tx.send(event).await;
}

pub const fn agent_name(idx: usize) -> &'static str {
    if idx == 0 { "Aria" } else { "Basil" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_assembly() {
        let mut t = Transcript::new("hello".to_string());
        t.push_reply(0, " a ".to_string());
        t.push_reply(1, "b".to_string());
        t.push_reply(0, "   ".to_string());
        let rendered = t.render();
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("Aria: a"));
        assert!(rendered.contains("Basil: b"));
        assert!(!rendered.contains("Aria:    "));
    }

    #[test]
    fn prompts_use_relay_style_after_first_turn() {
        let mut t = Transcript::new("seed prompt".to_string());
        let sp = &[String::new(), String::new()];
        let first = t.prompt_for_agent_turn(0, RoutingMode::PromptOnlyToAgentA, sp);
        t.push_reply(0, "a".to_string());
        let second = t.prompt_for_agent_turn(1, RoutingMode::PromptOnlyToAgentA, sp);
        t.push_reply(1, "b".to_string());
        let third = t.prompt_for_agent_turn(2, RoutingMode::PromptOnlyToAgentA, sp);
        assert_eq!(first, "seed prompt");
        assert_eq!(second, "a");
        assert_eq!(third, "b");
    }

    #[test]
    fn prompts_include_initial_context_for_b_in_prompt_to_a_and_b_mode() {
        let mut t = Transcript::new("seed prompt".to_string());
        let sp = &[String::new(), String::new()];
        let first = t.prompt_for_agent_turn(0, RoutingMode::PromptToAAndB, sp);
        t.push_reply(0, "a".to_string());
        let second = t.prompt_for_agent_turn(1, RoutingMode::PromptToAAndB, sp);
        t.push_reply(1, "b".to_string());
        let third = t.prompt_for_agent_turn(2, RoutingMode::PromptToAAndB, sp);

        assert_eq!(first, "seed prompt");
        assert!(second.contains("Initial user prompt:\nseed prompt"));
        assert!(second.contains("The other assistant's latest reply:\na"));
        assert_eq!(third, "b");
    }

    #[tokio::test]
    async fn pause_resume_state_transitions() {
        let (tx, mut rx) = watch::channel(ConversationControl::Pause);
        let handle = tokio::spawn(async move { wait_if_paused(&mut rx).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        tx.send(ConversationControl::Run).expect("send run");
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[test]
    fn token_coalescer_flushes_when_size_limit_reached() {
        let now = Instant::now();
        let mut coalescer = TokenCoalescer::new(now);
        let seed = "x".repeat(TOKEN_COALESCE_MAX_BYTES - 1);
        assert!(coalescer.push(&seed, now).is_none());

        let flushed = coalescer
            .push("y", now + Duration::from_millis(1))
            .expect("size flush");
        assert_eq!(flushed.len(), TOKEN_COALESCE_MAX_BYTES);
        assert!(coalescer.flush(now + Duration::from_millis(2)).is_none());
    }

    #[test]
    fn token_coalescer_flushes_when_age_limit_reached() {
        let now = Instant::now();
        let mut coalescer = TokenCoalescer::new(now);
        assert!(coalescer.push("hello", now).is_none());

        let flushed = coalescer
            .push(" world", now + TOKEN_COALESCE_MAX_AGE)
            .expect("age flush");
        assert_eq!(flushed, "hello world");
    }

    #[test]
    fn token_coalescer_preserves_token_order_across_flushes() {
        let now = Instant::now();
        let mut coalescer = TokenCoalescer::new(now);
        let mut out = Vec::new();
        if let Some(chunk) = coalescer.push("A", now) {
            out.push(chunk);
        }
        if let Some(chunk) = coalescer.push("B", now + TOKEN_COALESCE_MAX_AGE) {
            out.push(chunk);
        }
        if let Some(chunk) =
            coalescer.push("C", now + TOKEN_COALESCE_MAX_AGE + Duration::from_millis(1))
        {
            out.push(chunk);
        }
        if let Some(chunk) =
            coalescer.flush(now + TOKEN_COALESCE_MAX_AGE + Duration::from_millis(2))
        {
            out.push(chunk);
        }
        assert_eq!(out.join(""), "ABC");
    }
}
