use std::env;
use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub cmd: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgentSession {
    pub id: Option<String>,
    /// Cached result of `codex exec --help` flag probe; avoids re-spawning each turn.
    codex_supports_reasoning_effort: Option<bool>,
    /// Set true after the first successful login check; skips re-checking each turn.
    codex_api_login_verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    ClaudeStreamJson,
    CodexJson,
    PlainStdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStreamEventKind {
    Use,
    Result,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStreamEvent {
    pub kind: ToolStreamEventKind,
    pub tool_type: String,
    pub label: String,
    pub tool_call_id: Option<String>,
}

impl AgentSpec {
    fn command_name_lower(&self) -> String {
        Path::new(&self.cmd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(self.cmd.as_str())
            .to_ascii_lowercase()
    }

    pub fn is_codex_api_alias(&self) -> bool {
        self.command_name_lower() == "codex-api"
    }

    pub fn executable_cmd(&self) -> String {
        if self.is_codex_api_alias() {
            env::var("CODEX_API_CMD").unwrap_or_else(|_| "codex".to_string())
        } else {
            self.cmd.clone()
        }
    }

    fn codex_api_home(&self) -> String {
        env::var("CODEX_API_CODEX_HOME").unwrap_or_else(|_| ".codex-api-home".to_string())
    }

    fn codex_api_model(&self) -> String {
        self.read_model_candidate(&["OPENAI_MODEL", "CODEX_API_MODEL"]) // prefer OpenAI-style env, fallback to legacy
            .unwrap_or_else(|| "gpt-5.4".to_string())
    }

    fn read_model_candidate(&self, keys: &[&str]) -> Option<String> {
        keys.iter()
            .filter_map(|key| env::var(key).ok())
            .map(|value| value.trim().to_string())
            .find(|value| self.is_valid_model_name(value))
    }

    fn is_valid_model_name(&self, value: &str) -> bool {
        if value.is_empty() {
            return false;
        }

        if value.starts_with("gpt-")
            || value.starts_with("o")
            || value.starts_with("codex-")
            || value.starts_with("chatgpt-")
        {
            return true;
        }

        false
    }

    fn codex_api_reasoning_effort(&self) -> Option<String> {
        const ALLOWED: &[&str] = &["none", "low", "medium", "high", "xhigh"];
        env::var("OPENAI_REASONING_EFFORT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .and_then(|value| {
                if ALLOWED
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&value))
                {
                    Some(value.to_lowercase())
                } else {
                    None
                }
            })
    }

    fn codex_api_verbosity(&self) -> String {
        const ALLOWED: &[&str] = &["low", "medium", "high"];
        ["OPENAI_VERBOSITY", "CODEX_API_VERBOSITY"]
            .iter()
            .filter_map(|key| env::var(key).ok())
            .map(|value| value.trim().to_string())
            .find_map(|value| {
                ALLOWED
                    .iter()
                    .find(|allowed| allowed.eq_ignore_ascii_case(&value))
                    .map(|_| value.to_lowercase())
            })
            .unwrap_or_else(|| "high".to_string())
    }

    pub fn mode(&self) -> AgentMode {
        let file_name = self.command_name_lower();

        if file_name.contains("claude") {
            AgentMode::ClaudeStreamJson
        } else if file_name.contains("codex") {
            AgentMode::CodexJson
        } else {
            AgentMode::PlainStdout
        }
    }
}

pub async fn stream_reply<F, Fut>(
    agent: &AgentSpec,
    transcript: &str,
    debug: bool,
    session: &mut AgentSession,
    mut on_raw_line: impl FnMut(&str, Option<String>, Vec<ToolStreamEvent>),
    mut on_token: F,
) -> Result<String, AppError>
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mode = agent.mode();
    let exec_cmd = agent.executable_cmd();
    let mut cmd = Command::new(&exec_cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);

    if debug {
        cmd.stderr(std::process::Stdio::inherit());
    } else {
        cmd.stderr(std::process::Stdio::null());
    }

    match mode {
        AgentMode::ClaudeStreamJson => {
            cmd.arg("--allow-dangerously-skip-permissions")
                .arg("--dangerously-skip-permissions");
            if let Some(id) = &session.id {
                cmd.arg("--resume").arg(id);
            }
            cmd.arg("--print")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg(transcript);
        }
        AgentMode::CodexJson => {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
            if agent.is_codex_api_alias() {
                let api_key = env::var("OPENAI_API_KEY")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        AppError::InvalidInput(
                            "codex-api requires OPENAI_API_KEY in environment (.env)".to_string(),
                        )
                    })?;
                let codex_home = agent.codex_api_home();
                std::fs::create_dir_all(&codex_home)?;
                if !session.codex_api_login_verified {
                    ensure_codex_api_login(&exec_cmd, &codex_home, &api_key).await?;
                    session.codex_api_login_verified = true;
                }

                cmd.env("CODEX_HOME", &codex_home);
                cmd.env("OPENAI_API_KEY", &api_key);
                cmd.arg("-c").arg("preferred_auth_method=\"apikey\"");
                cmd.arg("-c").arg("model_provider=\"openai\"");
                cmd.arg("-c").arg(format!(
                    "model_verbosity=\"{}\"",
                    agent.codex_api_verbosity()
                ));
                cmd.arg("--model").arg(agent.codex_api_model());
                if let Some(effort) = agent.codex_api_reasoning_effort() {
                    let supports = match session.codex_supports_reasoning_effort {
                        Some(cached) => cached,
                        None => {
                            let result = codex_exec_supports_flag(
                                &exec_cmd,
                                &codex_home,
                                "--reasoning-effort",
                            )
                            .await;
                            session.codex_supports_reasoning_effort = Some(result);
                            result
                        }
                    };
                    if supports {
                        cmd.arg("--reasoning-effort").arg(effort);
                    }
                }
            }
            if let Some(id) = &session.id {
                cmd.arg("exec")
                    .arg("resume")
                    .arg("--json")
                    .arg("--skip-git-repo-check")
                    .arg(id)
                    .arg(transcript);
            } else {
                cmd.arg("exec")
                    .arg("--json")
                    .arg("--skip-git-repo-check")
                    .arg(transcript);
            }
        }
        AgentMode::PlainStdout => {
            cmd.arg(transcript);
        }
    }

    let mut child = cmd.spawn().map_err(|source| AppError::Spawn {
        cmd: exec_cmd.clone(),
        source,
    })?;

    let stdout = child.stdout.take().ok_or_else(|| AppError::Parse {
        agent: agent.cmd.clone(),
        message: "child stdout unavailable".to_string(),
    })?;

    let mut reader = BufReader::new(stdout).lines();
    let mut output = String::new();
    let mut saw_claude_delta = false;
    let mut saw_codex_delta = false;
    let mut saw_codex_final_message = false;

    while let Some(line) = reader.next_line().await? {
        let parsed = match mode {
            AgentMode::ClaudeStreamJson | AgentMode::CodexJson => parse_json_line(&line),
            AgentMode::PlainStdout => None,
        };

        let thinking = parse_thinking_value_for_mode(mode, parsed.as_ref());
        let tool_events = parse_tool_events_for_mode(mode, parsed.as_ref());
        on_raw_line(&line, thinking, tool_events);

        match mode {
            AgentMode::ClaudeStreamJson => {
                if session.id.is_none() {
                    session.id = parse_claude_session_id_value(parsed.as_ref());
                }
            }
            AgentMode::CodexJson => {
                if session.id.is_none() {
                    session.id = parse_codex_thread_id_value(parsed.as_ref());
                }
            }
            AgentMode::PlainStdout => {}
        }

        let token = match mode {
            AgentMode::ClaudeStreamJson => {
                if let Some(sep) = parse_claude_text_block_separator_value(parsed.as_ref()) {
                    Some(sep)
                } else if let Some(delta) = parse_claude_delta_value(parsed.as_ref()) {
                    saw_claude_delta = true;
                    Some(delta)
                } else if let Some(final_text) = parse_claude_final_value(parsed.as_ref()) {
                    if saw_claude_delta {
                        None
                    } else {
                        Some(final_text)
                    }
                } else {
                    fallback_text_line(&line)
                }
            }
            AgentMode::CodexJson => {
                if let Some(delta) = parse_codex_delta_value(parsed.as_ref()) {
                    saw_codex_delta = true;
                    Some(delta)
                } else if let Some(final_text) = parse_codex_value(parsed.as_ref()) {
                    if saw_codex_delta {
                        None
                    } else if saw_codex_final_message {
                        Some(format!("\n\n{final_text}"))
                    } else {
                        saw_codex_final_message = true;
                        Some(final_text)
                    }
                } else {
                    fallback_text_line(&line)
                }
            }
            AgentMode::PlainStdout => Some(format!("{line}\n")),
        };

        if let Some(token) = token {
            output.push_str(&token);
            on_token(&token).await;
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(AppError::NonZeroExit {
            cmd: agent.cmd.clone(),
            status: status.code().unwrap_or(-1),
        });
    }

    if output.trim().is_empty() {
        return Err(AppError::Parse {
            agent: agent.cmd.clone(),
            message: "no parsable output".to_string(),
        });
    }

    Ok(output)
}

async fn ensure_codex_api_login(
    exec_cmd: &str,
    codex_home: &str,
    api_key: &str,
) -> Result<(), AppError> {
    let status_out = Command::new(exec_cmd)
        .env("CODEX_HOME", codex_home)
        .arg("login")
        .arg("status")
        .output()
        .await
        .map_err(|source| AppError::Spawn {
            cmd: format!("{exec_cmd} login status"),
            source,
        })?;

    let status_stdout = String::from_utf8_lossy(&status_out.stdout);
    if status_out.status.success() && status_stdout.contains("Logged in using API key") {
        return Ok(());
    }

    let mut login_cmd = Command::new(exec_cmd);
    login_cmd
        .env("CODEX_HOME", codex_home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .arg("login")
        .arg("--with-api-key");

    let mut child = login_cmd.spawn().map_err(|source| AppError::Spawn {
        cmd: format!("{exec_cmd} login --with-api-key"),
        source,
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(format!("{api_key}\n").as_bytes()).await?;
    }
    let login_status = child.wait().await?;
    if !login_status.success() {
        return Err(AppError::InvalidInput(
            "codex-api login failed using OPENAI_API_KEY; verify the key and account permissions"
                .to_string(),
        ));
    }

    Ok(())
}

async fn codex_exec_supports_flag(exec_cmd: &str, codex_home: &str, flag: &str) -> bool {
    let out = match Command::new(exec_cmd)
        .env("CODEX_HOME", codex_home)
        .arg("exec")
        .arg("--help")
        .output()
        .await
    {
        Ok(out) => out,
        Err(_) => return false,
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stdout.contains(flag) || stderr.contains(flag)
}

pub fn invocation_summary(agent: &AgentSpec, session: &AgentSession) -> String {
    let exec_cmd = agent.executable_cmd();
    match agent.mode() {
        AgentMode::ClaudeStreamJson => {
            if let Some(id) = &session.id {
                format!(
                    "{} --allow-dangerously-skip-permissions --dangerously-skip-permissions --resume {} --print --output-format stream-json --verbose <prompt>",
                    exec_cmd, id
                )
            } else {
                format!(
                    "{} --allow-dangerously-skip-permissions --dangerously-skip-permissions --print --output-format stream-json --verbose <prompt>",
                    exec_cmd
                )
            }
        }
        AgentMode::CodexJson => {
            let model = if agent.is_codex_api_alias() {
                format!(
                    " -c preferred_auth_method=\"apikey\" -c model_provider=\"openai\" -c model_verbosity=\"{}\" --model {}",
                    agent.codex_api_verbosity(),
                    agent.codex_api_model()
                )
            } else {
                String::new()
            };
            if let Some(id) = &session.id {
                format!(
                    "{} --dangerously-bypass-approvals-and-sandbox{} exec resume --json --skip-git-repo-check {} <prompt>",
                    exec_cmd, model, id
                )
            } else {
                format!(
                    "{} --dangerously-bypass-approvals-and-sandbox{} exec --json --skip-git-repo-check <prompt>",
                    exec_cmd, model
                )
            }
        }
        AgentMode::PlainStdout => format!("{} <prompt>", exec_cmd),
    }
}

pub fn parse_claude_session_id(line: &str) -> Option<String> {
    parse_claude_session_id_value(parse_json_line(line).as_ref())
}

fn parse_claude_session_id_value(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if v.get("type").and_then(Value::as_str) == Some("system")
        && v.get("subtype").and_then(Value::as_str) == Some("init")
    {
        return v
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    None
}

pub fn parse_codex_thread_id(line: &str) -> Option<String> {
    parse_codex_thread_id_value(parse_json_line(line).as_ref())
}

fn parse_codex_thread_id_value(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if v.get("type").and_then(Value::as_str) == Some("thread.started") {
        return v
            .get("thread_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    None
}

pub fn parse_claude_stream_line(line: &str) -> Option<String> {
    parse_claude_delta_line(line).or_else(|| parse_claude_final_line(line))
}

pub fn parse_thinking_line_for_agent(agent: &AgentSpec, line: &str) -> Option<String> {
    match agent.mode() {
        AgentMode::ClaudeStreamJson => parse_claude_thinking_line(line),
        AgentMode::CodexJson => parse_codex_thinking_line(line),
        AgentMode::PlainStdout => None,
    }
}

pub fn parse_tool_events_line_for_agent(agent: &AgentSpec, line: &str) -> Vec<ToolStreamEvent> {
    match agent.mode() {
        AgentMode::ClaudeStreamJson => parse_claude_tool_event_line(line),
        AgentMode::CodexJson => parse_codex_tool_event_line(line),
        AgentMode::PlainStdout => Vec::new(),
    }
}

/// Returns `"\n"` when a new (non-first) text content block starts in the Claude stream,
/// separating distinct text segments that are interleaved with tool calls.
pub fn parse_claude_text_block_separator(line: &str) -> Option<String> {
    parse_claude_text_block_separator_value(parse_json_line(line).as_ref())
}

fn parse_claude_text_block_separator_value(v: Option<&Value>) -> Option<String> {
    let v = v?;

    // Handle both direct events and stream_event wrappers.
    let event = if v.get("type").and_then(Value::as_str) == Some("stream_event") {
        v.get("event")?
    } else {
        &v
    };

    if event.get("type").and_then(Value::as_str) != Some("content_block_start") {
        return None;
    }
    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
    if index == 0 {
        return None;
    }
    if event
        .get("content_block")
        .and_then(|cb| cb.get("type"))
        .and_then(Value::as_str)
        == Some("text")
    {
        Some("\n".to_owned())
    } else {
        None
    }
}

pub fn parse_claude_delta_line(line: &str) -> Option<String> {
    parse_claude_delta_value(parse_json_line(line).as_ref())
}

fn parse_claude_delta_value(v: Option<&Value>) -> Option<String> {
    let v = v?;

    if v.get("type").and_then(Value::as_str) == Some("stream_event") {
        if let Some(delta) = v.get("delta") {
            if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                return delta
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
        }

        if let Some(event) = v.get("event") {
            if event.get("type").and_then(Value::as_str) == Some("content_block_delta")
                && event
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str)
                    == Some("text_delta")
            {
                return event
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
        }
    }

    if v.get("type").and_then(Value::as_str) == Some("content_block_delta")
        && v.get("delta")
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            == Some("text_delta")
    {
        return v
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    None
}

pub fn parse_claude_final_line(line: &str) -> Option<String> {
    parse_claude_final_value(parse_json_line(line).as_ref())
}

fn parse_claude_final_value(v: Option<&Value>) -> Option<String> {
    let v = v?;

    if v.get("type").and_then(Value::as_str) == Some("assistant") {
        let content = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)?;
        let mut out = String::new();
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = item.get("text").and_then(Value::as_str)
            {
                out.push_str(text);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }

    None
}

pub fn parse_claude_thinking_line(line: &str) -> Option<String> {
    parse_claude_thinking_value(parse_json_line(line).as_ref())
}

fn parse_claude_thinking_value(v: Option<&Value>) -> Option<String> {
    let v = v?;

    if v.get("type").and_then(Value::as_str) == Some("assistant") {
        let content = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)?;
        let mut out = String::new();
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("thinking")
                && let Some(thinking) = item.get("thinking").and_then(Value::as_str)
            {
                out.push_str(thinking);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }

    if v.get("type").and_then(Value::as_str) == Some("stream_event") {
        if let Some(delta) = v.get("delta")
            && delta.get("type").and_then(Value::as_str) == Some("thinking_delta")
        {
            return delta
                .get("thinking")
                .or_else(|| delta.get("text"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }

        if let Some(event) = v.get("event")
            && event.get("type").and_then(Value::as_str) == Some("content_block_delta")
            && event
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
                == Some("thinking_delta")
        {
            return event
                .get("delta")
                .and_then(|d| d.get("thinking").or_else(|| d.get("text")))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
    }

    if v.get("type").and_then(Value::as_str) == Some("content_block_delta")
        && v.get("delta")
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            == Some("thinking_delta")
    {
        return v
            .get("delta")
            .and_then(|d| d.get("thinking").or_else(|| d.get("text")))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    None
}

pub fn parse_codex_line(line: &str) -> Option<String> {
    parse_codex_value(parse_json_line(line).as_ref())
}

fn parse_codex_value(v: Option<&Value>) -> Option<String> {
    let v = v?;
    let event_type = v.get("type").and_then(Value::as_str)?;
    if event_type != "item.completed" {
        return None;
    }

    let item = v.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str)?;
    if item_type != "agent_message" {
        return None;
    }

    item.get("text")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub fn parse_codex_delta_line(line: &str) -> Option<String> {
    parse_codex_delta_value(parse_json_line(line).as_ref())
}

fn parse_codex_delta_value(v: Option<&Value>) -> Option<String> {
    let v = v?;
    let event_type = v.get("type").and_then(Value::as_str)?;

    if event_type == "item.delta" {
        if let Some(item_type) = v
            .get("item")
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str)
            && is_codex_reasoning_item_type(item_type)
        {
            return None;
        }
        if let Some(text) = v
            .get("item")
            .and_then(|i| i.get("delta"))
            .and_then(extract_textish)
        {
            return Some(text);
        }
        if let Some(text) = v.get("item").and_then(extract_textish) {
            return Some(text);
        }
    }

    if event_type.ends_with(".delta") {
        if is_codex_reasoning_delta_event_type(event_type) || event_type.contains("trace") {
            return None;
        }
        if let Some(text) = v.get("delta").and_then(extract_textish) {
            return Some(text);
        }
        if let Some(text) = v
            .get("item")
            .and_then(|i| i.get("delta"))
            .and_then(extract_textish)
        {
            return Some(text);
        }
        if let Some(text) = v.get("text").and_then(Value::as_str).map(ToOwned::to_owned) {
            return Some(text);
        }
    }

    None
}

fn extract_textish(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_owned());
    }
    if let Some(s) = v.get("text").and_then(Value::as_str) {
        return Some(s.to_owned());
    }
    if let Some(s) = v.get("value").and_then(Value::as_str) {
        return Some(s.to_owned());
    }
    None
}

fn fallback_text_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with('[') {
        None
    } else {
        Some(format!("{line}\n"))
    }
}

pub fn parse_codex_thinking_line(line: &str) -> Option<String> {
    parse_codex_thinking_value(parse_json_line(line).as_ref())
}

fn parse_codex_thinking_value(v: Option<&Value>) -> Option<String> {
    let v = v?;
    let event_type = v.get("type").and_then(Value::as_str)?;

    // Codex emits reasoning in item payloads across several event families
    // (for example item.completed and response.output_item.*).
    if let Some(item) = v.get("item") {
        if let Some(item_type) = item.get("type").and_then(Value::as_str) {
            if is_codex_reasoning_item_type(item_type) {
                return item
                    .get("delta")
                    .and_then(extract_reasoning_textish)
                    .or_else(|| item.get("summary").and_then(extract_reasoning_textish))
                    .or_else(|| extract_reasoning_textish(item));
            }
        }
    }

    if !is_codex_reasoning_delta_event_type(event_type) {
        return None;
    }

    v.get("delta")
        .and_then(extract_reasoning_textish)
        .or_else(|| v.get("text").and_then(Value::as_str).map(ToOwned::to_owned))
        .or_else(|| v.get("summary").and_then(extract_reasoning_textish))
        .or_else(|| {
            v.get("item")
                .and_then(|i| i.get("delta"))
                .and_then(extract_reasoning_textish)
        })
}

fn parse_codex_tool_event_line(line: &str) -> Vec<ToolStreamEvent> {
    parse_codex_tool_event_value(parse_json_line(line).as_ref())
}

fn parse_codex_tool_event_value(v: Option<&Value>) -> Vec<ToolStreamEvent> {
    let Some(v) = v else {
        return Vec::new();
    };
    let Some(event_type) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };

    if event_type.ends_with(".delta") || event_type == "item.delta" {
        return Vec::new();
    }

    if let Some(items) = v.get("output").and_then(Value::as_array) {
        let mut out = Vec::new();
        for item in items {
            if let Some(ev) = parse_codex_tool_event_item(item, event_type) {
                out.push(ev);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    if let Some(items) = v.get("items").and_then(Value::as_array) {
        let mut out = Vec::new();
        for item in items {
            if let Some(ev) = parse_codex_tool_event_item(item, event_type) {
                out.push(ev);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    if let Some(items) = v
        .get("response")
        .and_then(|response| response.get("output"))
        .and_then(Value::as_array)
    {
        let mut out = Vec::new();
        for item in items {
            if let Some(ev) = parse_codex_tool_event_item(item, event_type) {
                out.push(ev);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    v.get("item")
        .and_then(|item| parse_codex_tool_event_item(item, event_type))
        .into_iter()
        .collect()
}

fn parse_codex_tool_event_item(item: &Value, event_type: &str) -> Option<ToolStreamEvent> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)?
        .to_ascii_lowercase();

    if item_type == "command_execution" {
        let command = item
            .get("command")
            .and_then(Value::as_str)
            .map(command_preview)
            .filter(|s| !s.is_empty());
        let status = item_status(item);
        if event_type == "item.started" || status == Some("in_progress") {
            return Some(ToolStreamEvent {
                kind: ToolStreamEventKind::Use,
                tool_type: item_type,
                label: format_bash_label(command.as_deref(), None),
                tool_call_id: extract_tool_reference(item),
            });
        }

        if !is_command_execution_terminal(event_type, status) {
            return None;
        }

        let exit_code = item.get("exit_code").and_then(Value::as_i64);
        let kind = if status == Some("failed")
            || status == Some("error")
            || status == Some("cancelled")
            || event_type.contains("error")
            || exit_code.is_some_and(|code| code != 0)
        {
            ToolStreamEventKind::Error
        } else {
            ToolStreamEventKind::Result
        };
        let label = format_bash_label(command.as_deref(), exit_code.filter(|code| *code != 0));
        return Some(ToolStreamEvent {
            kind,
            tool_type: item_type,
            label,
            tool_call_id: extract_tool_reference(item),
        });
    }

    if item_type == "file_change" {
        let status = item_status(item);
        let label = format_file_change_label(item);
        if event_type == "item.started" || status == Some("in_progress") {
            return Some(ToolStreamEvent {
                kind: ToolStreamEventKind::Use,
                tool_type: item_type,
                label,
                tool_call_id: extract_tool_reference(item),
            });
        }

        if !is_file_change_terminal(event_type, status) {
            return None;
        }

        let kind = if status == Some("failed")
            || status == Some("error")
            || status == Some("cancelled")
            || event_type.contains("error")
        {
            ToolStreamEventKind::Error
        } else {
            ToolStreamEventKind::Result
        };
        return Some(ToolStreamEvent {
            kind,
            tool_type: item_type,
            label,
            tool_call_id: extract_tool_reference(item),
        });
    }

    if is_tool_result_item_type(&item_type) {
        let label = format_tool_label(item, false)
            .or_else(|| extract_tool_reference(item))
            .unwrap_or_else(|| "tool".to_string());
        let kind = if item.get("is_error").and_then(Value::as_bool) == Some(true)
            || event_type.contains("error")
        {
            ToolStreamEventKind::Error
        } else {
            ToolStreamEventKind::Result
        };
        Some(ToolStreamEvent {
            kind,
            tool_type: item_type,
            label,
            tool_call_id: extract_tool_reference(item),
        })
    } else if is_tool_use_item_type(&item_type) {
        Some(ToolStreamEvent {
            kind: ToolStreamEventKind::Use,
            tool_type: item_type,
            label: format_tool_label(item, true).unwrap_or_else(|| "tool".to_string()),
            tool_call_id: extract_tool_reference(item),
        })
    } else {
        None
    }
}

fn item_status(item: &Value) -> Option<&str> {
    item.get("status").and_then(Value::as_str)
}

fn is_command_execution_terminal(event_type: &str, status: Option<&str>) -> bool {
    if event_type == "item.completed" {
        return true;
    }
    matches!(
        status,
        Some("completed") | Some("failed") | Some("error") | Some("cancelled")
    )
}

fn is_file_change_terminal(event_type: &str, status: Option<&str>) -> bool {
    if event_type == "item.completed" {
        return true;
    }
    matches!(
        status,
        Some("completed") | Some("failed") | Some("error") | Some("cancelled")
    )
}

fn format_file_change_label(item: &Value) -> String {
    let Some(changes) = item.get("changes").and_then(Value::as_array) else {
        return "file change".to_string();
    };
    if changes.is_empty() {
        return "file change".to_string();
    }

    let first = &changes[0];
    let first_path = first
        .get("path")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .map(compact_preview)
        .unwrap_or_else(|| "file".to_string());
    let first_kind = first
        .get("kind")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "update".to_string());

    if changes.len() == 1 {
        return format!("{first_kind} {first_path}");
    }

    format!("{first_kind} {first_path} (+{} more)", changes.len() - 1)
}

fn parse_claude_tool_event_line(line: &str) -> Vec<ToolStreamEvent> {
    parse_claude_tool_event_value(parse_json_line(line).as_ref())
}

fn parse_claude_tool_event_value(v: Option<&Value>) -> Vec<ToolStreamEvent> {
    let Some(v) = v else {
        return Vec::new();
    };

    if v.get("type").and_then(Value::as_str) == Some("assistant") {
        let Some(content) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("tool_use") {
                out.push(ToolStreamEvent {
                    kind: ToolStreamEventKind::Use,
                    tool_type: "tool_use".to_string(),
                    label: item
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| "tool".to_string()),
                    tool_call_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                let label = item
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| "tool".to_string());
                let kind = if item.get("is_error").and_then(Value::as_bool) == Some(true) {
                    ToolStreamEventKind::Error
                } else {
                    ToolStreamEventKind::Result
                };
                out.push(ToolStreamEvent {
                    kind,
                    tool_type: "tool_result".to_string(),
                    label,
                    tool_call_id: item
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    let event = if v.get("type").and_then(Value::as_str) == Some("stream_event") {
        if let Some(event) = v.get("event") {
            event
        } else {
            return Vec::new();
        }
    } else {
        &v
    };

    if event.get("type").and_then(Value::as_str) != Some("content_block_start") {
        return Vec::new();
    }

    let Some(content_block) = event.get("content_block") else {
        return Vec::new();
    };
    let Some(block_type) = content_block.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    if block_type == "tool_use" {
        return vec![ToolStreamEvent {
            kind: ToolStreamEventKind::Use,
            tool_type: "tool_use".to_string(),
            label: content_block
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "tool".to_string()),
            tool_call_id: content_block
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }];
    }

    if block_type == "tool_result" {
        let label = content_block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "tool".to_string());
        let kind = if content_block.get("is_error").and_then(Value::as_bool) == Some(true) {
            ToolStreamEventKind::Error
        } else {
            ToolStreamEventKind::Result
        };
        return vec![ToolStreamEvent {
            kind,
            tool_type: "tool_result".to_string(),
            label,
            tool_call_id: content_block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }];
    }

    Vec::new()
}

fn parse_json_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

fn parse_thinking_value_for_mode(mode: AgentMode, parsed: Option<&Value>) -> Option<String> {
    match mode {
        AgentMode::ClaudeStreamJson => parse_claude_thinking_value(parsed),
        AgentMode::CodexJson => parse_codex_thinking_value(parsed),
        AgentMode::PlainStdout => None,
    }
}

fn parse_tool_events_for_mode(mode: AgentMode, parsed: Option<&Value>) -> Vec<ToolStreamEvent> {
    match mode {
        AgentMode::ClaudeStreamJson => parse_claude_tool_event_value(parsed),
        AgentMode::CodexJson => parse_codex_tool_event_value(parsed),
        AgentMode::PlainStdout => Vec::new(),
    }
}

fn is_codex_reasoning_item_type(item_type: &str) -> bool {
    item_type.contains("reasoning") || item_type.contains("thinking")
}

fn is_tool_use_item_type(item_type: &str) -> bool {
    (item_type.contains("function_call") && !item_type.contains("output"))
        || item_type.contains("tool_call")
        || item_type == "tool_use"
}

fn is_tool_result_item_type(item_type: &str) -> bool {
    item_type.contains("function_call_output")
        || item_type.contains("tool_result")
        || item_type.contains("tool_output")
}

fn extract_tool_name(item: &Value) -> Option<String> {
    item.get("name")
        .and_then(Value::as_str)
        .or_else(|| item.get("tool_name").and_then(Value::as_str))
        .or_else(|| {
            item.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            item.get("call")
                .and_then(|c| c.get("name"))
                .and_then(Value::as_str)
        })
        .map(ToOwned::to_owned)
}

fn format_tool_label(item: &Value, include_detail: bool) -> Option<String> {
    let tool_name = extract_tool_name(item)?;
    if !include_detail {
        return Some(tool_name);
    }
    if let Some(detail) = extract_tool_detail(item, &tool_name) {
        if detail.is_empty() {
            return Some(tool_name);
        }
        return Some(format!("{tool_name}: {detail}"));
    }
    Some(tool_name)
}

fn extract_tool_detail(item: &Value, tool_name: &str) -> Option<String> {
    let lower = tool_name.to_ascii_lowercase();

    if lower.contains("exec_command")
        || lower.contains("command")
        || lower.contains("shell")
        || lower == "bash"
    {
        return extract_argument_string(item, &["cmd", "command"])
            .map(|text| command_preview(&text));
    }

    if lower.contains("search_query") {
        return extract_argument_string(item, &["q", "query", "search_query"]).map(compact_preview);
    }
    if lower.contains("image_query") {
        return extract_argument_string(item, &["q", "query", "image_query"]).map(compact_preview);
    }
    if lower.ends_with(".open") || lower == "open" {
        return extract_argument_string(item, &["ref_id", "url", "uri"]).map(compact_preview);
    }
    if lower.ends_with(".find") || lower == "find" {
        return extract_argument_string(item, &["pattern", "q", "query"]).map(compact_preview);
    }
    if lower.ends_with(".click") || lower == "click" {
        return extract_argument_string(item, &["id", "ref_id"]).map(compact_preview);
    }
    if lower.starts_with("web.") {
        return extract_argument_string(item, &["q", "query", "location", "ticker", "team"])
            .map(compact_preview);
    }

    extract_argument_string(item, &["path", "file", "file_path", "ref_id"]).map(compact_preview)
}

fn extract_argument_string(item: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = item.get(key)
            && let Some(text) = value_to_inline_text(value)
        {
            return Some(text);
        }
    }

    for container_key in ["input", "arguments", "params"] {
        let Some(container) = item.get(container_key) else {
            continue;
        };
        if let Some(text) = extract_from_argument_container(container, keys) {
            return Some(text);
        }
    }

    None
}

fn extract_from_argument_container(container: &Value, keys: &[&str]) -> Option<String> {
    if let Some(obj) = container.as_object() {
        for key in keys {
            if let Some(value) = obj.get(*key)
                && let Some(text) = value_to_inline_text(value)
            {
                return Some(text);
            }
        }
    }

    if let Some(raw) = container.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && let Some(text) = extract_from_argument_container(&parsed, keys)
    {
        return Some(text);
    }

    if let Some(array) = container.as_array() {
        for entry in array {
            if let Some(text) = extract_from_argument_container(entry, keys) {
                return Some(text);
            }
        }
    }

    None
}

fn value_to_inline_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(entries) => entries.iter().find_map(value_to_inline_text),
        Value::Object(map) => {
            if let Some(v) = map.get("q").and_then(Value::as_str) {
                return Some(v.to_string());
            }
            if let Some(v) = map.get("query").and_then(Value::as_str) {
                return Some(v.to_string());
            }
            None
        }
        _ => None,
    }
}

fn command_preview(command: &str) -> String {
    let mut text = command.trim().to_string();
    text = text.replace('\n', " ");
    if let Some(stripped) = text.strip_prefix("/bin/bash -lc ") {
        text = stripped.to_string();
    } else if let Some(stripped) = text.strip_prefix("bash -lc ") {
        text = stripped.to_string();
    }

    if (text.starts_with('\'') && text.ends_with('\''))
        || (text.starts_with('"') && text.ends_with('"'))
    {
        text = text[1..text.len().saturating_sub(1)].to_string();
    }
    if let Some((_, tail)) = text.rsplit_once("&&") {
        text = tail.trim().to_string();
    }

    compact_preview(text)
}

fn compact_preview(text: String) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let max_chars = 96usize;
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let clipped: String = compact.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{clipped}...")
}

fn format_bash_label(command: Option<&str>, exit_code: Option<i64>) -> String {
    let base = match command {
        Some(command) if !command.is_empty() => format!("bash: {command}"),
        _ => "bash".to_string(),
    };
    match exit_code {
        Some(code) => format!("{base} (exit {code})"),
        None => base,
    }
}

fn extract_tool_reference(item: &Value) -> Option<String> {
    item.get("tool_use_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("call_id").and_then(Value::as_str))
        .or_else(|| item.get("id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn is_codex_reasoning_delta_event_type(event_type: &str) -> bool {
    (event_type.contains("reasoning") || event_type.contains("thinking"))
        && event_type.contains("delta")
}

fn extract_reasoning_textish(v: &Value) -> Option<String> {
    if let Some(text) = extract_textish(v) {
        return Some(text);
    }

    if let Some(content) = v.get("content").and_then(Value::as_array) {
        let joined = content
            .iter()
            .filter_map(extract_reasoning_textish)
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }

    if let Some(summary) = v.get("summary").and_then(Value::as_array) {
        let joined = summary
            .iter()
            .filter_map(extract_reasoning_textish)
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }

    if let Some(array) = v.as_array() {
        let joined = array
            .iter()
            .filter_map(extract_reasoning_textish)
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_sample() {
        let line = r#"{"type":"stream_event","delta":{"type":"text_delta","text":"hello"}}"#;
        assert_eq!(parse_claude_stream_line(line).as_deref(), Some("hello"));
    }

    #[test]
    fn parse_claude_nested_event_sample() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello2"}}}"#;
        assert_eq!(parse_claude_stream_line(line).as_deref(), Some("hello2"));
    }

    #[test]
    fn parse_claude_assistant_message_sample() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"x"},{"type":"text","text":"Hello!"}]}}"#;
        assert_eq!(parse_claude_stream_line(line).as_deref(), Some("Hello!"));
    }

    #[test]
    fn parse_claude_assistant_thinking_sample() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"reasoning trace"},{"type":"text","text":"Hello!"}]}}"#;
        assert_eq!(
            parse_claude_thinking_line(line).as_deref(),
            Some("reasoning trace")
        );
    }

    #[test]
    fn parse_claude_delta_only_sample() {
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"H"}}"#;
        assert_eq!(parse_claude_delta_line(line).as_deref(), Some("H"));
        assert_eq!(parse_claude_final_line(line), None);
    }

    #[test]
    fn parse_codex_sample() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"world"}}"#;
        assert_eq!(parse_codex_line(line).as_deref(), Some("world"));
    }

    #[test]
    fn parse_codex_delta_sample() {
        let line = r#"{"type":"response.output_text.delta","delta":{"text":"par"}}"#;
        assert_eq!(parse_codex_delta_line(line).as_deref(), Some("par"));
    }

    #[test]
    fn parse_codex_reasoning_delta_does_not_emit_token_sample() {
        let line = r#"{"type":"response.reasoning.delta","delta":{"text":"internal step"}}"#;
        assert_eq!(parse_codex_delta_line(line), None);
    }

    #[test]
    fn parse_codex_trace_delta_does_not_emit_token_sample() {
        let line = r#"{"type":"response.trace.delta","delta":{"text":"trace data"}}"#;
        assert_eq!(parse_codex_delta_line(line), None);
    }

    #[test]
    fn parse_codex_item_delta_reasoning_does_not_emit_token_sample() {
        let line = r#"{"type":"item.delta","item":{"type":"reasoning","delta":{"text":"step"}}}"#;
        assert_eq!(parse_codex_delta_line(line), None);
    }

    #[test]
    fn parse_codex_item_completed_reasoning_sample() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Preparing response**"}}"#;
        assert_eq!(
            parse_codex_thinking_line(line).as_deref(),
            Some("**Preparing response**")
        );
    }

    #[test]
    fn parse_codex_item_delta_reasoning_sample() {
        let line = r#"{"type":"item.delta","item":{"type":"reasoning","delta":{"text":"step"}}}"#;
        assert_eq!(parse_codex_thinking_line(line).as_deref(), Some("step"));
    }

    #[test]
    fn parse_codex_item_completed_non_reasoning_sample() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}"#;
        assert_eq!(parse_codex_thinking_line(line), None);
    }

    #[test]
    fn parse_codex_response_reasoning_delta_sample() {
        let line = r#"{"type":"response.reasoning.delta","delta":{"text":"internal step"}}"#;
        assert_eq!(
            parse_codex_thinking_line(line).as_deref(),
            Some("internal step")
        );
    }

    #[test]
    fn parse_codex_response_reasoning_summary_text_delta_sample() {
        let line = r#"{"type":"response.reasoning_summary_text.delta","delta":"chain-step"}"#;
        assert_eq!(
            parse_codex_thinking_line(line).as_deref(),
            Some("chain-step")
        );
    }

    #[test]
    fn parse_codex_output_item_added_reasoning_summary_sample() {
        let line = r#"{"type":"response.output_item.added","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"plan part A"}]}}"#;
        assert_eq!(
            parse_codex_thinking_line(line).as_deref(),
            Some("plan part A")
        );
    }

    #[test]
    fn parse_codex_reasoning_delta_with_item_missing_type_still_parses() {
        let line = r#"{"type":"response.reasoning.delta","item":{"id":"item_1"},"delta":{"text":"fallback step"}}"#;
        assert_eq!(
            parse_codex_thinking_line(line).as_deref(),
            Some("fallback step")
        );
    }

    #[test]
    fn parse_codex_response_reasoning_non_delta_sample() {
        let line = r#"{"type":"response.reasoning.completed","delta":{"text":"done"}}"#;
        assert_eq!(parse_codex_thinking_line(line), None);
    }

    #[test]
    fn parse_codex_unrelated_reasoning_event_sample() {
        let line = r#"{"type":"reasoning.telemetry","text":"heartbeat"}"#;
        assert_eq!(parse_codex_thinking_line(line), None);
    }

    #[test]
    fn parse_claude_session_id_sample() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        assert_eq!(parse_claude_session_id(line).as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_codex_tool_use_event_sample() {
        let line = r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_55","name":"shell"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        let parsed = &parsed[0];
        assert_eq!(parsed.kind, ToolStreamEventKind::Use);
        assert_eq!(parsed.tool_type, "function_call");
        assert_eq!(parsed.label, "shell");
        assert_eq!(parsed.tool_call_id.as_deref(), Some("call_55"));
    }

    #[test]
    fn parse_codex_tool_result_event_sample() {
        let line = r#"{"type":"response.output_item.done","item":{"type":"function_call_output","call_id":"call_123"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        let parsed = &parsed[0];
        assert_eq!(parsed.kind, ToolStreamEventKind::Result);
        assert_eq!(parsed.tool_type, "function_call_output");
        assert_eq!(parsed.label, "call_123");
        assert_eq!(parsed.tool_call_id.as_deref(), Some("call_123"));
    }

    #[test]
    fn parse_codex_multi_tool_entries_emit_all_events() {
        let line = r#"{"type":"response.output","output":[{"type":"function_call","name":"shell"},{"type":"function_call_output","call_id":"call_1"},{"type":"tool_call","name":"fetch"}]}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].label, "shell");
        assert_eq!(parsed[1].kind, ToolStreamEventKind::Result);
        assert_eq!(parsed[1].label, "call_1");
        assert_eq!(parsed[2].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[2].label, "fetch");
    }

    #[test]
    fn parse_codex_multi_tool_entries_from_items_emit_all_events() {
        let line = r#"{"type":"response.output","items":[{"type":"function_call","name":"read_file"},{"type":"function_call_output","call_id":"call_22"}]}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].label, "read_file");
        assert_eq!(parsed[1].kind, ToolStreamEventKind::Result);
        assert_eq!(parsed[1].label, "call_22");
    }

    #[test]
    fn parse_codex_multi_tool_entries_from_response_output_emit_all_events() {
        let line = r#"{"type":"response.completed","response":{"output":[{"type":"tool_call","name":"search"},{"type":"tool_result","tool_use_id":"tool_9","is_error":true}]}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].label, "search");
        assert_eq!(parsed[1].kind, ToolStreamEventKind::Error);
        assert_eq!(parsed[1].label, "tool_9");
    }

    #[test]
    fn parse_codex_item_started_command_execution_emits_use_with_command_preview() {
        let line = r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/bash -lc 'cd /home/max/antiphon && rg --files'","status":"in_progress"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].tool_type, "command_execution");
        assert_eq!(parsed[0].label, "bash: rg --files");
    }

    #[test]
    fn parse_codex_item_completed_command_execution_failure_emits_error() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/bash -lc 'cd /repo && rg nope'","exit_code":1,"status":"failed"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Error);
        assert_eq!(parsed[0].tool_type, "command_execution");
        assert_eq!(parsed[0].label, "bash: rg nope (exit 1)");
    }

    #[test]
    fn parse_codex_item_completed_command_execution_success_emits_result() {
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"/bin/bash -lc 'cd /repo && cargo test -q'","exit_code":0,"status":"completed"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Result);
        assert_eq!(parsed[0].label, "bash: cargo test -q");
    }

    #[test]
    fn parse_codex_non_terminal_command_execution_event_is_ignored() {
        let line = r#"{"type":"response.output_item.added","item":{"id":"item_3","type":"command_execution","command":"bash -lc 'echo hi'"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_codex_item_completed_file_change_emits_result() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/repo/src/ui.rs","kind":"update"}],"status":"completed"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Result);
        assert_eq!(parsed[0].tool_type, "file_change");
        assert_eq!(parsed[0].label, "update /repo/src/ui.rs");
    }

    #[test]
    fn parse_codex_non_terminal_file_change_event_is_ignored() {
        let line = r#"{"type":"response.output_item.added","item":{"id":"item_3","type":"file_change","changes":[{"path":"src/ui.rs","kind":"update"}]}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_codex_command_execution_missing_command_uses_plain_bash_label() {
        let line = r#"{"type":"item.started","item":{"id":"item_4","type":"command_execution","status":"in_progress"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].label, "bash");
    }

    #[test]
    fn parse_codex_function_call_includes_query_detail() {
        let line = r#"{"type":"response.output_item.added","item":{"type":"function_call","name":"web.search_query","arguments":"{\"search_query\":[{\"q\":\"rust crossterm colors\"}]}"}}"#;
        let parsed = parse_codex_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].label, "web.search_query: rust crossterm colors");
    }

    #[test]
    fn parse_claude_tool_use_event_sample() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"src/main.rs"}}}}"#;
        let parsed = parse_claude_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        let parsed = &parsed[0];
        assert_eq!(parsed.kind, ToolStreamEventKind::Use);
        assert_eq!(parsed.label, "Read");
        assert_eq!(parsed.tool_call_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn parse_claude_tool_result_error_event_sample() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true}]}}"#;
        let parsed = parse_claude_tool_event_line(line);
        assert_eq!(parsed.len(), 1);
        let parsed = &parsed[0];
        assert_eq!(parsed.kind, ToolStreamEventKind::Error);
        assert_eq!(parsed.label, "toolu_1");
        assert_eq!(parsed.tool_call_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn parse_claude_multi_tool_entries_emit_all_events() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read"},{"type":"tool_result","tool_use_id":"toolu_1"},{"type":"tool_use","id":"toolu_2","name":"Bash"}]}}"#;
        let parsed = parse_claude_tool_event_line(line);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[0].label, "Read");
        assert_eq!(parsed[1].kind, ToolStreamEventKind::Result);
        assert_eq!(parsed[1].label, "toolu_1");
        assert_eq!(parsed[2].kind, ToolStreamEventKind::Use);
        assert_eq!(parsed[2].label, "Bash");
    }

    #[test]
    fn parse_codex_thread_id_sample() {
        let line = r#"{"type":"thread.started","thread_id":"thread-1"}"#;
        assert_eq!(parse_codex_thread_id(line).as_deref(), Some("thread-1"));
    }

    #[test]
    fn value_dispatch_matches_line_parsers_for_codex() {
        let line = r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc 'echo hi'","status":"in_progress"}}"#;
        let parsed = parse_json_line(line);

        assert_eq!(
            parse_thinking_value_for_mode(AgentMode::CodexJson, parsed.as_ref()),
            parse_codex_thinking_line(line)
        );
        assert_eq!(
            parse_tool_events_for_mode(AgentMode::CodexJson, parsed.as_ref()),
            parse_codex_tool_event_line(line)
        );
    }

    #[test]
    fn value_dispatch_matches_line_parsers_for_claude() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"plan"},{"type":"tool_use","id":"toolu_1","name":"Read"}]}}"#;
        let parsed = parse_json_line(line);

        assert_eq!(
            parse_thinking_value_for_mode(AgentMode::ClaudeStreamJson, parsed.as_ref()),
            parse_claude_thinking_line(line)
        );
        assert_eq!(
            parse_tool_events_for_mode(AgentMode::ClaudeStreamJson, parsed.as_ref()),
            parse_claude_tool_event_line(line)
        );
    }

    #[test]
    fn value_dispatch_handles_invalid_json() {
        let line = "not-json";
        let parsed = parse_json_line(line);
        assert!(parsed.is_none());
        assert_eq!(
            parse_thinking_value_for_mode(AgentMode::CodexJson, parsed.as_ref()),
            None
        );
        assert!(parse_tool_events_for_mode(AgentMode::CodexJson, parsed.as_ref()).is_empty());
    }

    #[test]
    fn codex_api_alias_uses_codex_mode() {
        let spec = AgentSpec {
            cmd: "codex-api".to_string(),
        };
        assert_eq!(spec.mode(), AgentMode::CodexJson);
        assert!(spec.is_codex_api_alias());
    }

    #[test]
    fn codex_api_invocation_summary_includes_high_verbosity_default() {
        let spec = AgentSpec {
            cmd: "codex-api".to_string(),
        };
        let summary = invocation_summary(&spec, &AgentSession::default());
        assert!(summary.contains("-c model_verbosity=\"high\""));
        assert!(summary.contains("--model gpt-5.4"));
    }
}
