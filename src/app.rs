use std::sync::Arc;
use std::time::Duration;

use std::io::IsTerminal;
use std::{fs, io};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::agent::AgentSpec;
use crate::audit::AuditSet;
use crate::cli::Cli;
use crate::conversation::{
    self, ConversationConfig, ConversationControl, ConversationEvent, RoutingMode,
};
use crate::error::AppError;
use crate::tmux;
use crate::ui::{self, UiAction};

const UI_SETTINGS_FILE: &str = ".antiphon.tui-settings.json";
const CONVERSATION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

const DEFAULT_PRESETS_JSON: &str = include_str!("default_presets.json");

fn default_presets() -> Vec<Preset> {
    serde_json::from_str(DEFAULT_PRESETS_JSON).unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub name: String,
    pub prompt: String,
    pub agent_a_system_prompt: String,
    pub agent_b_system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UiSettings {
    prompt: String,
    agent_a: String,
    agent_b: String,
    turns: usize,
    routing_mode: RoutingMode,
    tri_pane_layout: bool,
    #[serde(default)]
    orb_layout: bool,
    thinking_expanded: bool,
    show_tmux_panels: bool,
    #[serde(default)]
    agent_a_system_prompt: String,
    #[serde(default)]
    agent_b_system_prompt: String,
    #[serde(default)]
    presets: Vec<Preset>,
    #[serde(default)]
    active_preset_idx: Option<usize>,
}

fn settings_path() -> std::path::PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| crate::home_dir())
        .join(UI_SETTINGS_FILE)
}

fn load_ui_settings() -> Option<UiSettings> {
    let raw = fs::read_to_string(settings_path()).ok()?;
    serde_json::from_str::<UiSettings>(&raw).ok()
}

fn save_ui_settings(settings: &UiSettings, presets_modified: bool) -> io::Result<()> {
    // If the TUI didn't modify presets, re-read them from disk so manual edits survive.
    let mut to_save = settings.clone();
    if !presets_modified {
        if let Some(disk) = load_ui_settings() {
            to_save.presets = disk.presets;
            to_save.active_preset_idx = disk.active_preset_idx;
        }
    }
    let payload = serde_json::to_string_pretty(&to_save)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(settings_path(), payload)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkingTracker {
    pub agent_working: [bool; 2],
}

impl WorkingTracker {
    pub const fn new() -> Self {
        Self {
            agent_working: [false, false],
        }
    }

    pub fn apply_event(&mut self, event: &ConversationEvent) {
        match event {
            ConversationEvent::TurnStart { agent_idx } => {
                if *agent_idx < 2 {
                    self.agent_working[*agent_idx] = true;
                }
            }
            ConversationEvent::TurnDone { agent_idx } => {
                if *agent_idx < 2 {
                    self.agent_working[*agent_idx] = false;
                }
            }
            ConversationEvent::Done | ConversationEvent::Error { .. } => {
                self.agent_working = [false, false];
            }
            ConversationEvent::Token { .. }
            | ConversationEvent::Thinking { .. }
            | ConversationEvent::ToolEvent { .. } => {}
        }
    }
}

impl Default for WorkingTracker {
    fn default() -> Self {
        Self::new()
    }
}

async fn shutdown_conversation(
    convo: JoinHandle<Result<conversation::Transcript, AppError>>,
) -> Result<Option<conversation::Transcript>, AppError> {
    let mut convo = convo;
    match timeout(CONVERSATION_SHUTDOWN_TIMEOUT, &mut convo).await {
        Ok(joined) => match joined.map_err(|_| AppError::Cancelled)? {
            Ok(transcript) => Ok(Some(transcript)),
            Err(AppError::Cancelled) => Ok(None),
            Err(err) => Err(err),
        },
        Err(_) => {
            convo.abort();
            let _ = convo.await;
            eprintln!(
                "warning: conversation did not stop within {}ms; forced shutdown",
                CONVERSATION_SHUTDOWN_TIMEOUT.as_millis()
            );
            Ok(None)
        }
    }
}

pub async fn run(args: Cli) -> Result<(), AppError> {
    if args.turns == 0 {
        return Err(AppError::InvalidInput("--turns must be >= 1".to_string()));
    }

    run_crossterm_mode(args).await
}

async fn run_crossterm_mode(args: Cli) -> Result<(), AppError> {
    let debug_mode = args.debug || !is_interactive();

    let audit_base_dir = args
        .audit_log
        .clone()
        .unwrap_or_else(|| crate::home_dir().join("conversations"));

    if debug_mode {
        let audit = Arc::new(AuditSet::create(&audit_base_dir)?);
        let cfg = ConversationConfig {
            agents: [
                AgentSpec {
                    cmd: args.agent_a.clone(),
                },
                AgentSpec {
                    cmd: args.agent_b.clone(),
                },
            ],
            turns: args.turns,
            initial_prompt: args.initial_prompt.unwrap_or_default(),
            routing_mode: RoutingMode::PromptOnlyToAgentA,
            debug: true,
            audit: Some(audit),
            agent_system_prompts: [String::new(), String::new()],
        };
        let (events_tx, events_rx) = mpsc::channel(512);
        let (_control_tx, control_rx) = watch::channel(ConversationControl::Run);
        let convo = tokio::spawn(conversation::run(cfg, events_tx, control_rx));

        let debug_result = run_debug(events_rx, args.quiet).await;
        let convo_result = convo.await.map_err(|_| AppError::Cancelled)?;
        debug_result?;
        convo_result?;
        return Ok(());
    }

    let loaded = load_ui_settings();
    let mut prompt = args.initial_prompt.unwrap_or_else(|| {
        loaded
            .as_ref()
            .map(|s| s.prompt.clone())
            .unwrap_or_default()
    });
    let mut agent_cmds = [args.agent_a, args.agent_b];
    let mut turns = args.turns;
    let mut routing_mode = RoutingMode::PromptOnlyToAgentA;
    let mut tri_pane_layout = false;
    let mut orb_layout = false;
    let mut thinking_expanded = false;
    let mut show_tmux_panels = true;
    let mut agent_system_prompts = [String::new(), String::new()];
    let mut presets: Vec<Preset> = default_presets();
    let mut active_preset_idx: Option<usize> = None;
    let mut launch_requested = false;
    let mut initial_presets: Vec<Preset> = Vec::new();

    if let Some(saved) = loaded {
        // Keep CLI explicit prompt authoritative; otherwise restore last TUI state.
        if agent_cmds[0] == "claude" {
            agent_cmds[0] = saved.agent_a;
        }
        if agent_cmds[1] == "claude" {
            agent_cmds[1] = saved.agent_b;
        }
        if turns == 10 {
            turns = saved.turns.max(1);
        }
        routing_mode = saved.routing_mode;
        tri_pane_layout = saved.tri_pane_layout;
        orb_layout = saved.orb_layout;
        thinking_expanded = saved.thinking_expanded;
        show_tmux_panels = saved.show_tmux_panels;
        agent_system_prompts = [saved.agent_a_system_prompt, saved.agent_b_system_prompt];
        presets = if saved.presets.is_empty() {
            default_presets()
        } else {
            saved.presets
        };
        active_preset_idx = saved.active_preset_idx;
        initial_presets = presets.clone();
    }

    loop {
        let (events_tx, events_rx) = mpsc::channel(512);
        let (control_tx, control_rx) = watch::channel(ConversationControl::Run);
        let mut tmux_panes = None;
        let convo = if !launch_requested || prompt.trim().is_empty() {
            drop(events_tx);
            None
        } else {
            let audit = Arc::new(AuditSet::create(&audit_base_dir)?);
            if show_tmux_panels {
                match tmux::open_agent_windows(
                    &audit.conversation_id,
                    &audit.live_agent_path(0),
                    &audit.live_agent_path(1),
                ) {
                    Ok(panes) => tmux_panes = panes,
                    Err(err) => eprintln!("warning: could not open tmux agent windows: {err}"),
                }
            }
            let cfg = ConversationConfig {
                agents: [
                    AgentSpec {
                        cmd: agent_cmds[0].clone(),
                    },
                    AgentSpec {
                        cmd: agent_cmds[1].clone(),
                    },
                ],
                turns,
                initial_prompt: prompt.clone(),
                routing_mode,
                debug: false,
                audit: Some(audit.clone()),
                agent_system_prompts: agent_system_prompts.clone(),
            };
            Some(tokio::spawn(conversation::run(cfg, events_tx, control_rx)))
        };
        let ui_action = ui::run_tui(
            events_rx,
            control_tx.clone(),
            prompt.clone(),
            agent_cmds.clone(),
            turns,
            routing_mode,
            tri_pane_layout,
            orb_layout,
            thinking_expanded,
            show_tmux_panels,
            agent_system_prompts.clone(),
            presets.clone(),
            active_preset_idx,
        )
        .await;

        match ui_action {
            Ok(UiAction::Quit {
                prompt: next_prompt,
                agent_a,
                agent_b,
                turns: next_turns,
                routing_mode: next_routing_mode,
                tri_pane_layout: next_tri_pane_layout,
                orb_layout: next_orb_layout,
                thinking_expanded: next_thinking_expanded,
                show_tmux_panels: next_show_tmux_panels,
                agent_system_prompts: next_agent_system_prompts,
                presets: next_presets,
                active_preset_idx: next_active_preset_idx,
            }) => {
                let _ = save_ui_settings(
                    &UiSettings {
                        prompt: next_prompt,
                        agent_a,
                        agent_b,
                        turns: next_turns.max(1),
                        routing_mode: next_routing_mode,
                        tri_pane_layout: next_tri_pane_layout,
                        orb_layout: next_orb_layout,
                        thinking_expanded: next_thinking_expanded,
                        show_tmux_panels: next_show_tmux_panels,
                        agent_a_system_prompt: next_agent_system_prompts[0].clone(),
                        agent_b_system_prompt: next_agent_system_prompts[1].clone(),
                        presets: next_presets.clone(),
                        active_preset_idx: next_active_preset_idx,
                    },
                    next_presets != initial_presets,
                );
                let _ = control_tx.send(ConversationControl::Stop);
                if let Some(panes) = &tmux_panes {
                    tmux::close_panes(panes);
                }
                if let Some(convo) = convo {
                    if let Some(transcript) = shutdown_conversation(convo).await? {
                        if !args.quiet {
                            println!("{}", transcript.render());
                        }
                    }
                }
                return Ok(());
            }
            Ok(UiAction::Relaunch {
                prompt: next_prompt,
                agent_a,
                agent_b,
                turns: next_turns,
                routing_mode: next_routing_mode,
                tri_pane_layout: next_tri_pane_layout,
                orb_layout: next_orb_layout,
                thinking_expanded: next_thinking_expanded,
                show_tmux_panels: next_show_tmux_panels,
                agent_system_prompts: next_agent_system_prompts,
                presets: next_presets,
                active_preset_idx: next_active_preset_idx,
            }) => {
                let _ = save_ui_settings(
                    &UiSettings {
                        prompt: next_prompt.clone(),
                        agent_a: agent_a.clone(),
                        agent_b: agent_b.clone(),
                        turns: next_turns.max(1),
                        routing_mode: next_routing_mode,
                        tri_pane_layout: next_tri_pane_layout,
                        orb_layout: next_orb_layout,
                        thinking_expanded: next_thinking_expanded,
                        show_tmux_panels: next_show_tmux_panels,
                        agent_a_system_prompt: next_agent_system_prompts[0].clone(),
                        agent_b_system_prompt: next_agent_system_prompts[1].clone(),
                        presets: next_presets.clone(),
                        active_preset_idx: next_active_preset_idx,
                    },
                    next_presets != initial_presets,
                );
                let _ = control_tx.send(ConversationControl::Stop);
                if let Some(panes) = &tmux_panes {
                    tmux::close_panes(panes);
                }
                if let Some(convo) = convo {
                    let _ = shutdown_conversation(convo).await;
                }
                prompt = next_prompt;
                agent_cmds = [agent_a, agent_b];
                turns = next_turns.max(1);
                routing_mode = next_routing_mode;
                tri_pane_layout = next_tri_pane_layout;
                orb_layout = next_orb_layout;
                thinking_expanded = next_thinking_expanded;
                show_tmux_panels = next_show_tmux_panels;
                agent_system_prompts = next_agent_system_prompts;
                presets = next_presets;
                active_preset_idx = next_active_preset_idx;
                initial_presets = presets.clone();
                launch_requested = true;
            }
            Err(err) => {
                let _ = control_tx.send(ConversationControl::Stop);
                if let Some(panes) = &tmux_panes {
                    tmux::close_panes(panes);
                }
                if let Some(convo) = convo {
                    let _ = shutdown_conversation(convo).await;
                }
                return Err(err);
            }
        }
    }
}

async fn run_debug(
    mut events_rx: mpsc::Receiver<ConversationEvent>,
    quiet: bool,
) -> Result<(), AppError> {
    while let Some(event) = events_rx.recv().await {
        match event {
            ConversationEvent::TurnStart { agent_idx } => {
                if !quiet {
                    println!(
                        "== Turn ({}) {} ==",
                        agent_idx + 1,
                        conversation::agent_name(agent_idx)
                    );
                }
            }
            ConversationEvent::Token { text, .. } => {
                if !quiet {
                    print!("{text}");
                }
            }
            ConversationEvent::Thinking { .. } | ConversationEvent::ToolEvent { .. } => {}
            ConversationEvent::TurnDone { .. } => {
                if !quiet {
                    println!();
                }
            }
            ConversationEvent::Done => {
                if !quiet {
                    println!("== Done ==");
                }
                break;
            }
            ConversationEvent::Error { code, message } => {
                eprintln!("[error:{}] {}", code.as_str(), message);
            }
        }
    }
    Ok(())
}

fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_state_transitions() {
        let mut tracker = WorkingTracker::new();
        tracker.apply_event(&ConversationEvent::TurnStart { agent_idx: 0 });
        assert_eq!(tracker.agent_working, [true, false]);
        tracker.apply_event(&ConversationEvent::TurnStart { agent_idx: 1 });
        assert_eq!(tracker.agent_working, [true, true]);
        tracker.apply_event(&ConversationEvent::TurnDone { agent_idx: 0 });
        assert_eq!(tracker.agent_working, [false, true]);
        tracker.apply_event(&ConversationEvent::Done);
        assert_eq!(tracker.agent_working, [false, false]);
        tracker.apply_event(&ConversationEvent::TurnStart { agent_idx: 1 });
        tracker.apply_event(&ConversationEvent::Error {
            code: crate::error::ErrorCode::ParseFailed,
            message: "x".into(),
        });
        assert_eq!(tracker.agent_working, [false, false]);
    }

    #[test]
    fn non_interactive_runs_use_debug_mode() {
        let args = Cli {
            agent_a: "claude".into(),
            agent_b: "claude".into(),
            turns: 1,
            debug: false,
            output: crate::cli::OutputFormat::Text,
            audit_log: None,
            quiet: false,
            initial_prompt: Some("Start".into()),
        };

        let debug_mode = args.debug || !is_interactive();
        assert_eq!(debug_mode, !is_interactive());
    }
}
