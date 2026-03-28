use std::sync::Arc;
use std::time::Duration;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
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
use crate::workspace::{
    RuntimePaths, ScopeResolution, SettingsScope, WorkspacePaths, WorkspaceRegistry,
    bootstrap_settings_file, initial_workspace_root, resolve_scope,
};

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
pub(crate) struct UiSettings {
    pub(crate) prompt: String,
    pub(crate) agent_a: String,
    pub(crate) agent_b: String,
    pub(crate) turns: usize,
    pub(crate) routing_mode: RoutingMode,
    pub(crate) tri_pane_layout: bool,
    #[serde(default)]
    pub(crate) orb_layout: bool,
    pub(crate) thinking_expanded: bool,
    pub(crate) show_tmux_panels: bool,
    #[serde(default)]
    pub(crate) agent_a_system_prompt: String,
    #[serde(default)]
    pub(crate) agent_b_system_prompt: String,
    #[serde(default)]
    pub(crate) presets: Vec<Preset>,
    #[serde(default)]
    pub(crate) active_preset_idx: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppWorkspaceState {
    pub active_workspace_root: PathBuf,
    pub active_scope: SettingsScope,
    pub active_paths: WorkspacePaths,
    pub launch_workspace_root: Option<PathBuf>,
    pub launch_scope: Option<SettingsScope>,
}

impl AppWorkspaceState {
    fn relaunch_required(&self) -> bool {
        self.launch_workspace_root
            .as_ref()
            .zip(self.launch_scope)
            .is_some_and(|(root, scope)| {
                root != &self.active_workspace_root || scope != self.active_scope
            })
    }
}

pub(crate) fn default_ui_settings() -> UiSettings {
    UiSettings {
        prompt: String::new(),
        agent_a: "claude".to_string(),
        agent_b: "claude".to_string(),
        turns: 10,
        routing_mode: RoutingMode::PromptOnlyToAgentA,
        tri_pane_layout: false,
        orb_layout: false,
        thinking_expanded: false,
        show_tmux_panels: true,
        agent_a_system_prompt: String::new(),
        agent_b_system_prompt: String::new(),
        presets: default_presets(),
        active_preset_idx: None,
    }
}

pub(crate) fn serialize_ui_settings(settings: &UiSettings) -> io::Result<String> {
    serde_json::to_string_pretty(settings)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

pub(crate) fn load_ui_settings(path: &Path) -> Option<UiSettings> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<UiSettings>(&raw).ok()
}

pub(crate) fn load_ui_settings_or_default(path: &Path) -> UiSettings {
    load_ui_settings(path).unwrap_or_else(default_ui_settings)
}

pub(crate) fn save_ui_settings(
    path: &Path,
    settings: &UiSettings,
    presets_modified: bool,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // If the TUI didn't modify presets, re-read them from disk so manual edits survive.
    let mut to_save = settings.clone();
    if !presets_modified {
        if let Some(disk) = load_ui_settings(path) {
            to_save.presets = disk.presets;
            to_save.active_preset_idx = disk.active_preset_idx;
        }
    }
    let payload = serialize_ui_settings(&to_save)?;
    fs::write(path, payload)
}

fn resolve_initial_workspace_state(
    args: &Cli,
) -> Result<(RuntimePaths, WorkspaceRegistry, AppWorkspaceState), AppError> {
    let runtime_paths = RuntimePaths::new(crate::home_dir());
    fs::create_dir_all(&runtime_paths.runtime_home)?;
    fs::create_dir_all(&runtime_paths.global_conversations_dir)?;
    fs::create_dir_all(&runtime_paths.codex_api_home)?;

    let mut registry =
        WorkspaceRegistry::load(&runtime_paths.workspace_registry_path).unwrap_or_default();
    let initial_root = if let Some(path) = args.workspace.as_deref() {
        crate::workspace::normalize_workspace_path(&path.display().to_string())
            .map_err(|err| AppError::InvalidInput(err.to_string()))?
    } else {
        initial_workspace_root(None, &registry)
    };
    if !initial_root.is_dir() {
        return Err(AppError::InvalidInput(format!(
            "workspace path is not a directory: {}",
            initial_root.display()
        )));
    }

    let initial_scope = match resolve_scope(&registry, &initial_root) {
        ScopeResolution::Resolved(scope) => scope,
        ScopeResolution::NeedsChoice { .. } => SettingsScope::Global,
    };
    let active_paths =
        WorkspacePaths::for_workspace(&runtime_paths, initial_root.clone(), initial_scope);
    bootstrap_settings_file(
        &active_paths,
        None,
        &serialize_ui_settings(&default_ui_settings())?,
    )?;
    registry.remember_workspace(&initial_root);
    registry.set_preference(&initial_root, initial_scope);
    registry.save(&runtime_paths.workspace_registry_path)?;

    Ok((
        runtime_paths,
        registry,
        AppWorkspaceState {
            active_workspace_root: initial_root,
            active_scope: initial_scope,
            active_paths,
            launch_workspace_root: None,
            launch_scope: None,
        },
    ))
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
    let (runtime_paths, _registry, mut workspace_state) = resolve_initial_workspace_state(&args)?;
    let settings_path = workspace_state.active_paths.settings_path.clone();
    let audit_base_dir = args
        .audit_log
        .clone()
        .unwrap_or_else(|| workspace_state.active_paths.conversations_dir.clone());

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
            workspace_root: workspace_state.active_workspace_root.clone(),
            codex_api_home: runtime_paths.codex_api_home.clone(),
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

    let loaded = load_ui_settings(&settings_path);
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
            let launch_workspace_root = workspace_state.active_workspace_root.clone();
            let launch_scope = workspace_state.active_scope;
            workspace_state.launch_workspace_root = Some(launch_workspace_root.clone());
            workspace_state.launch_scope = Some(launch_scope);
            let scoped_audit_dir = args
                .audit_log
                .clone()
                .unwrap_or_else(|| workspace_state.active_paths.conversations_dir.clone());
            let audit = Arc::new(AuditSet::create(&scoped_audit_dir)?);
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
                workspace_root: launch_workspace_root,
                codex_api_home: runtime_paths.codex_api_home.clone(),
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
            workspace_state.active_workspace_root.clone(),
            workspace_state.active_scope,
            workspace_state.relaunch_required(),
            workspace_state.launch_workspace_root.clone(),
            workspace_state.launch_scope,
            runtime_paths.clone(),
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
                workspace_root: next_workspace_root,
                settings_scope: next_settings_scope,
            }) => {
                workspace_state.active_workspace_root = next_workspace_root.clone();
                workspace_state.active_scope = next_settings_scope;
                workspace_state.active_paths = WorkspacePaths::for_workspace(
                    &runtime_paths,
                    next_workspace_root.clone(),
                    next_settings_scope,
                );
                let quit_settings = UiSettings {
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
                };
                let _ = save_ui_settings(
                    &workspace_state.active_paths.settings_path,
                    &quit_settings,
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
                workspace_root: next_workspace_root,
                settings_scope: next_settings_scope,
            }) => {
                workspace_state.active_workspace_root = next_workspace_root.clone();
                workspace_state.active_scope = next_settings_scope;
                workspace_state.active_paths = WorkspacePaths::for_workspace(
                    &runtime_paths,
                    next_workspace_root.clone(),
                    next_settings_scope,
                );
                let relaunch_settings = UiSettings {
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
                };
                let _ = save_ui_settings(
                    &workspace_state.active_paths.settings_path,
                    &relaunch_settings,
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
            workspace: None,
            quiet: false,
            initial_prompt: Some("Start".into()),
        };

        let debug_mode = args.debug || !is_interactive();
        assert_eq!(debug_mode, !is_interactive());
    }
}
