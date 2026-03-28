use std::io::{Write, stderr};
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::ToolStreamEventKind;
use crate::app::Preset;
use crate::app::WorkingTracker;
use crate::conversation::{self, ConversationControl, ConversationEvent, RoutingMode};
use crate::error::AppError;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SYSPROMPT_HEIGHT: usize = 10;
/// Header strings for conversation agent turns. Update when `conversation::agent_name` changes.
const AGENT_HEADERS: &[(&str, usize)] = &[("Aria", 0), ("Basil", 1)];
const TRI_CHAT_SIDE_PAD_MIN: usize = 3;
const TRI_CHAT_SIDE_PAD_MAX: usize = 12;
const TRI_CHAT_SIDE_PAD_DIVISOR: usize = 3;
const ARIA_PURPLE_THINK_FRAMES: [&str; 6] = ["🟣", "🟪", "🔮", "💜", "🪻", "👾"];
const BASIL_NATURE_THINK_FRAMES: [&str; 6] = ["🌿", "🍃", "🌱", "🪴", "🌳", "🌲"];
const ARIA_REASONING_GLYPH_FRAMES: [char; 6] = ['✦', '✧', '⋆', '✶', '✧', '✦'];
const BASIL_REASONING_GLYPH_FRAMES: [char; 6] = ['❀', '✿', '❃', '✾', '✿', '❀'];
const AGENT_COMMAND_CHOICES: [&str; 3] = ["claude", "codex", "codex-api"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Classic,
    TriPaneThinking,
    Orb,
}

impl LayoutMode {
    const fn next(self) -> Self {
        match self {
            Self::Classic => Self::TriPaneThinking,
            Self::TriPaneThinking => Self::Orb,
            Self::Orb => Self::Classic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalState {
    Hidden,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandActionId {
    Relaunch,
    PauseResume,
    Quit,
    Clear,
    EditPrompt,
    EditTurns,
    ToggleAgentA,
    ToggleAgentB,
    Mode,
    Layout,
    Thinking,
    Tmux,
    Select,
    SysPromptA,
    SysPromptB,
    PresetMode,
    Help,
}

#[derive(Debug, Clone)]
pub enum UiAction {
    Quit {
        prompt: String,
        agent_a: String,
        agent_b: String,
        turns: usize,
        routing_mode: RoutingMode,
        tri_pane_layout: bool,
        orb_layout: bool,
        thinking_expanded: bool,
        show_tmux_panels: bool,
        agent_system_prompts: [String; 2],
        presets: Vec<Preset>,
        active_preset_idx: Option<usize>,
    },
    Relaunch {
        prompt: String,
        agent_a: String,
        agent_b: String,
        turns: usize,
        routing_mode: RoutingMode,
        tri_pane_layout: bool,
        orb_layout: bool,
        thinking_expanded: bool,
        show_tmux_panels: bool,
        agent_system_prompts: [String; 2],
        presets: Vec<Preset>,
        active_preset_idx: Option<usize>,
    },
}

fn agent_color(idx: usize) -> Color {
    // Soft RGB palette — works well on dark terminals, easily extended for N agents
    const PALETTE: &[(u8, u8, u8)] = &[
        (170, 120, 240), // soft purple (agent 0)
        (240, 165, 40),  // warm amber  (agent 1)
        (160, 130, 240), // soft violet (agent 2)
        (100, 210, 130), // soft green  (agent 3)
        (240, 100, 120), // soft coral  (agent 4)
    ];
    let (r, g, b) = PALETTE[idx % PALETTE.len()];
    Color::Rgb { r, g, b }
}

fn agent_color_pale(idx: usize) -> Color {
    // Blend the full agent color 50% toward a light grey (150, 150, 150)
    let Color::Rgb { r, g, b } = agent_color(idx) else {
        unreachable!()
    };
    const MID: u8 = 150;
    Color::Rgb {
        r: (r / 2).saturating_add(MID / 2),
        g: (g / 2).saturating_add(MID / 2),
        b: (b / 2).saturating_add(MID / 2),
    }
}

fn smoothstep01(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix_u8(a: u8, b: u8, t: f64) -> u8 {
    let t = t.clamp(0.0, 1.0);
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

fn orb_base_color(op: f64) -> (u8, u8, u8) {
    let aria = (202, 156, 248);
    let middle = (244, 236, 220);
    let basil = (224, 204, 118);

    if op <= 0.5 {
        let t = smoothstep01(op * 2.0);
        (
            mix_u8(aria.0, middle.0, t),
            mix_u8(aria.1, middle.1, t),
            mix_u8(aria.2, middle.2, t),
        )
    } else {
        let t = smoothstep01((op - 0.5) * 2.0);
        (
            mix_u8(middle.0, basil.0, t),
            mix_u8(middle.1, basil.1, t),
            mix_u8(middle.2, basil.2, t),
        )
    }
}

fn orb_active_accent(active_agent: Option<usize>) -> Option<(u8, u8, u8)> {
    match active_agent {
        Some(0) => Some((184, 98, 255)),
        Some(1) => Some((255, 214, 84)),
        _ => None,
    }
}

pub async fn run_tui(
    mut events_rx: mpsc::Receiver<ConversationEvent>,
    control_tx: watch::Sender<ConversationControl>,
    initial_prompt: String,
    initial_agents: [String; 2],
    initial_turns: usize,
    initial_routing_mode: RoutingMode,
    initial_tri_pane_layout: bool,
    initial_orb_layout: bool,
    initial_thinking_expanded: bool,
    initial_show_tmux_panels: bool,
    initial_agent_system_prompts: [String; 2],
    initial_presets: Vec<Preset>,
    initial_active_preset_idx: Option<usize>,
) -> Result<UiAction, AppError> {
    let mut term = stderr();
    enable_raw_mode()?;
    execute!(term, Hide, EnableMouseCapture)?;
    let mut mouse_capture_enabled = true;

    let _guard = TerminalGuard;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut events_closed = false;

    let mut state = UiState::new(
        initial_prompt,
        initial_agents,
        initial_turns,
        initial_routing_mode,
        initial_tri_pane_layout,
        initial_orb_layout,
        initial_thinking_expanded,
        initial_show_tmux_panels,
        initial_agent_system_prompts,
        initial_presets,
        initial_active_preset_idx,
    );
    state.render(&mut term)?;

    loop {
        tokio::select! {
            maybe_event = events_rx.recv(), if !events_closed => {
                if let Some(ev) = maybe_event {
                    state.apply_conversation_event(ev);
                    while let Ok(extra) = events_rx.try_recv() {
                        state.apply_conversation_event(extra);
                    }
                    sync_mouse_capture(
                        &mut term,
                        &mut mouse_capture_enabled,
                        state.mouse_capture,
                    )?;
                    state.render(&mut term)?;
                } else {
                    events_closed = true;
                    state.on_conversation_closed();
                    sync_mouse_capture(&mut term, &mut mouse_capture_enabled, state.mouse_capture)?;
                    state.render(&mut term)?;
                }
            }
            maybe_term_ev = events.next() => {
                if let Some(Ok(ev)) = maybe_term_ev {
                    // Mouse moves never change state — skip the re-render to avoid flicker.
                    let is_inert = matches!(ev,
                        Event::Mouse(m) if matches!(m.kind,
                            MouseEventKind::Moved | MouseEventKind::Drag(_)
                        )
                    );
                    if let Some(action) = handle_key_event(ev, &control_tx, &mut state) {
                        return Ok(action);
                    }
                    sync_mouse_capture(
                        &mut term,
                        &mut mouse_capture_enabled,
                        state.mouse_capture,
                    )?;
                    if !is_inert {
                        state.render(&mut term)?;
                    }
                }
            }
            _ = ticker.tick() => {
                state.anim_frame = state.anim_frame.wrapping_add(1);
                if state.working.agent_working.iter().any(|v| *v) {
                    state.spinner_frame = (state.spinner_frame + 1) % SPINNER.len();
                }
                // Spring orb toward target position
                let any_working = state.working.agent_working.iter().any(|v| *v);
                let target_pos: f32 = if any_working {
                    match state.active_agent {
                        Some(0) => 0.1,
                        Some(1) => 0.9,
                        _ => 0.5,
                    }
                } else {
                    0.5
                };
                state.orb_pos += (target_pos - state.orb_pos) * 0.07;
                // Skip re-render while a modal/overlay is open — nothing
                // animates behind it and the constant redraws cause visible flicker.
                let modal_open = state.preset_mode_active()
                    || state.modal_state == ModalState::Help;
                if !modal_open {
                    state.render(&mut term)?;
                }
            }
        }
    }
}

fn handle_key_event(
    ev: Event,
    control_tx: &watch::Sender<ConversationControl>,
    state: &mut UiState,
) -> Option<UiAction> {
    match ev {
        Event::Key(KeyEvent {
            code, modifiers, ..
        }) => {
            if state.sysprompt_edit.is_some() {
                match code {
                    KeyCode::Esc => state.cancel_sysprompt_edit(),
                    KeyCode::Tab => state.sysprompt_switch_target(),
                    _ if is_ctrl_s(code, modifiers) => {
                        state.confirm_sysprompt_edit();
                    }
                    KeyCode::Enter => state.sysprompt_insert_char('\n'),
                    KeyCode::Left => state.sysprompt_move_left(),
                    KeyCode::Right => state.sysprompt_move_right(),
                    KeyCode::Home => state.sysprompt_move_home(),
                    KeyCode::End => state.sysprompt_move_end(),
                    KeyCode::Backspace => state.sysprompt_backspace(),
                    KeyCode::Delete => state.sysprompt_delete(),
                    KeyCode::Char(c)
                        if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
                    {
                        state.sysprompt_insert_char(c);
                    }
                    _ => {}
                }
                return None;
            }
            if state.editing_prompt {
                let selecting = modifiers.contains(KeyModifiers::SHIFT);
                match code {
                    KeyCode::Esc => {
                        state.editing_prompt = false;
                        state.edit_buffer.clear();
                        state.edit_cursor = 0;
                        state.edit_selection_anchor = None;
                    }
                    KeyCode::Enter => {
                        state.commit_prompt_edit_and_exit();
                    }
                    _ if is_ctrl_s(code, modifiers) => {
                        state.prompt = state.edit_buffer.clone();
                    }
                    KeyCode::Char(c)
                        if modifiers.contains(KeyModifiers::CONTROL)
                            && c.eq_ignore_ascii_case(&'a') =>
                    {
                        state.prompt_select_all();
                    }
                    KeyCode::Left => {
                        state.move_prompt_cursor_left(selecting);
                    }
                    KeyCode::Right => {
                        state.move_prompt_cursor_right(selecting);
                    }
                    KeyCode::Home => {
                        state.move_prompt_cursor_home(selecting);
                    }
                    KeyCode::End => {
                        state.move_prompt_cursor_end(selecting);
                    }
                    KeyCode::Backspace => {
                        state.prompt_backspace();
                    }
                    KeyCode::Delete => {
                        state.prompt_delete();
                    }
                    KeyCode::Char(c)
                        if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
                    {
                        state.insert_prompt_char(c);
                    }
                    _ => {}
                }
                return None;
            }
            if state.editing_turns {
                match code {
                    KeyCode::Esc => {
                        state.editing_turns = false;
                        state.turns_buffer.clear();
                    }
                    KeyCode::Enter => state.commit_turn_edit_and_exit(),
                    _ if is_ctrl_s(code, modifiers) => state.commit_turn_edit_and_exit(),
                    KeyCode::Backspace => {
                        state.turns_buffer.pop();
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        state.turns_buffer.push(c);
                    }
                    _ => {}
                }
                return None;
            }

            if state.agent_chooser.is_some() {
                match code {
                    KeyCode::Up | KeyCode::Char('k') => state.agent_chooser_up(),
                    KeyCode::Down | KeyCode::Char('j') => state.agent_chooser_down(),
                    KeyCode::Enter => state.confirm_agent_chooser(),
                    KeyCode::Esc => state.cancel_agent_chooser(),
                    _ => {}
                }
                return None;
            }

            if state.preset_mode_active() {
                if matches!(state.preset_panel_state, PresetPanelState::Naming { .. }) {
                    match code {
                        KeyCode::Esc => state.preset_panel_state = PresetPanelState::Idle,
                        KeyCode::Enter => state.preset_confirm_naming(),
                        _ if is_ctrl_s(code, modifiers) => state.preset_confirm_naming(),
                        KeyCode::Backspace => state.preset_naming_backspace(),
                        KeyCode::Left => state.preset_naming_left(),
                        KeyCode::Right => state.preset_naming_right(),
                        KeyCode::Char(c)
                            if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
                        {
                            state.preset_naming_insert(c);
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => state.preset_list_up(),
                        KeyCode::Down | KeyCode::Char('j') => state.preset_list_down(),
                        KeyCode::Enter => state.preset_load_selected(),
                        KeyCode::Esc => state.preset_panel_state = PresetPanelState::Idle,
                        _ if is_ctrl_s(code, modifiers) => state.preset_open_naming(),
                        _ if is_ctrl_d(code, modifiers) => state.preset_delete_selected(),
                        _ => {}
                    }
                }
                return None;
            }

            match code {
                KeyCode::Char('?') | KeyCode::Char('h') => {
                    return state.execute_command_action(CommandActionId::Help, control_tx);
                }
                KeyCode::Esc => {
                    if state.modal_state == ModalState::Help {
                        state.modal_state = ModalState::Hidden;
                        state.last_size = None;
                        return None;
                    }
                    if state.run_is_active_or_paused() {
                        state.stop_run(control_tx);
                        return None;
                    }
                }
                _ => {}
            }

            match (code, modifiers) {
                (KeyCode::Char('q'), KeyModifiers::CONTROL)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    return state.execute_command_action(CommandActionId::Quit, control_tx);
                }
                (KeyCode::Char('p'), _) => {
                    return state.execute_command_action(CommandActionId::PauseResume, control_tx);
                }
                (KeyCode::Char('w'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::EditPrompt, control_tx);
                }
                (KeyCode::Char('q'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::SysPromptA, control_tx);
                }
                (KeyCode::Char('e'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::SysPromptB, control_tx);
                }
                (KeyCode::Char('v'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::Select, control_tx);
                }
                (KeyCode::Char(c), _) if ('1'..='9').contains(&c) => {
                    state.turns = c.to_digit(10).unwrap_or(1) as usize;
                }
                (KeyCode::Char('r'), _) => {
                    return state.execute_command_action(CommandActionId::Relaunch, control_tx);
                }
                (KeyCode::Char('c'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::Clear, control_tx);
                }
                (KeyCode::Char('x'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::Mode, control_tx);
                }
                (KeyCode::Char('y'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::Layout, control_tx);
                }
                (KeyCode::Char('n'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::Thinking, control_tx);
                }
                (KeyCode::Char('b'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::Tmux, control_tx);
                }
                (KeyCode::Char('a'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::ToggleAgentA, control_tx);
                }
                (KeyCode::Char('d'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::ToggleAgentB, control_tx);
                }
                (KeyCode::Char('s'), modifiers) if modifiers.is_empty() => {
                    return state.execute_command_action(CommandActionId::PresetMode, control_tx);
                }
                (KeyCode::Char('`'), _) => {
                    return state.execute_command_action(CommandActionId::EditTurns, control_tx);
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    if state.auto_scroll {
                        state.scroll = state.max_scroll();
                    }
                    if state.scroll > 0 {
                        state.scroll -= 1;
                    }
                    state.auto_scroll = false;
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    let max = state.max_scroll();
                    if state.scroll < max {
                        state.scroll += 1;
                        state.auto_scroll = state.scroll >= max;
                    } else {
                        state.auto_scroll = true;
                    }
                }
                (KeyCode::PageUp, _) => {
                    if state.auto_scroll {
                        state.scroll = state.max_scroll();
                    }
                    state.scroll = state.scroll.saturating_sub(10);
                    state.auto_scroll = false;
                }
                (KeyCode::PageDown, _) => {
                    let max = state.max_scroll();
                    state.scroll = (state.scroll + 10).min(max);
                    state.auto_scroll = state.scroll >= max;
                }
                (KeyCode::Home, _) => {
                    state.scroll = 0;
                    state.auto_scroll = false;
                }
                (KeyCode::End, _) => {
                    state.scroll = state.max_scroll();
                    state.auto_scroll = true;
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            _ if !state.mouse_capture => {}
            MouseEventKind::ScrollUp => {
                if state.auto_scroll {
                    state.scroll = state.max_scroll();
                }
                state.scroll = state.scroll.saturating_sub(3);
                state.auto_scroll = false;
            }
            MouseEventKind::ScrollDown => {
                let max = state.max_scroll();
                state.scroll = (state.scroll + 3).min(max);
                state.auto_scroll = state.scroll >= max;
            }
            _ => {}
        },
        _ => {}
    }
    None
}

fn sync_mouse_capture(
    term: &mut impl Write,
    mouse_capture_enabled: &mut bool,
    desired_enabled: bool,
) -> Result<(), AppError> {
    if *mouse_capture_enabled == desired_enabled {
        return Ok(());
    }
    if desired_enabled {
        execute!(term, EnableMouseCapture)?;
    } else {
        execute!(term, DisableMouseCapture)?;
    }
    *mouse_capture_enabled = desired_enabled;
    Ok(())
}

fn agent_command_choice_index(current: &str) -> usize {
    let lower = current.to_ascii_lowercase();
    if lower.contains("codex-api") {
        2
    } else if lower.contains("codex") {
        1
    } else if lower.contains("claude") {
        0
    } else {
        0
    }
}

fn is_ctrl_s(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char(c) if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'s'))
}

fn is_ctrl_d(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char(c) if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'d'))
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut err = stderr();
        let _ = execute!(err, Show, DisableMouseCapture);
        let _ = disable_raw_mode();
    }
}

#[derive(Debug, Clone, Default)]
struct TurnRecord {
    agent_idx: usize,
    main_text: String,
    main_chunks: Vec<String>,
    timeline: Vec<ThinkingTimelineRecord>,
    saw_main_message: bool,
    pending_leading_newlines: usize,
}

#[derive(Debug, Clone)]
struct ToolEventRecord {
    kind: ToolStreamEventKind,
    tool_type: String,
    text: String,
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
enum ThinkingTimelineRecord {
    Reasoning(String),
    Tool(ToolEventRecord),
    MessageBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingLineKind {
    Blank,
    Reasoning,
    ToolHeader,
    ToolFooter,
    ToolUse,
    ToolResult,
    ToolError,
    ToolContinuation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThinkingDisplayLine {
    kind: ThinkingLineKind,
    text: String,
}

#[derive(Debug, Clone)]
struct SysPromptEditState {
    active_agent_idx: usize,
    buffers: [String; 2],
    cursors: [usize; 2], // byte offsets within each buffer
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentChooserState {
    agent_idx: usize,
    cursor: usize,
}

#[derive(Debug)]
enum PresetPanelState {
    Idle,
    FocusedList {
        cursor: usize,
    },
    Naming {
        list_cursor: usize,
        buffer: String,
        cursor: usize,
    },
}

#[derive(Debug)]
struct UiState {
    working: WorkingTracker,
    active_agent: Option<usize>,
    spinner_frame: usize,
    anim_frame: usize,
    orb_pos: f32,
    prompt: String,
    launched_prompt: Option<String>,
    run_started: bool,
    run_failed: bool,
    agent_cmds: [String; 2],
    turns: usize,
    routing_mode: RoutingMode,
    editing_prompt: bool,
    edit_buffer: String,
    edit_cursor: usize,
    edit_selection_anchor: Option<usize>,
    editing_turns: bool,
    turns_buffer: String,
    lines: Vec<String>,
    turns_log: Vec<TurnRecord>,
    active_turn_idx: Option<usize>,
    tail_lines: Vec<String>,
    current_turn_line: Option<usize>,
    scroll: usize,
    auto_scroll: bool,
    paused: bool,
    mouse_capture: bool,
    finished: bool,
    completed: bool,
    frame_drawn: bool,
    last_size: Option<(usize, usize)>,
    layout_mode: LayoutMode,
    thinking_expanded: bool,
    show_tmux_panels: bool,
    modal_state: ModalState,
    agent_system_prompts: [String; 2],
    sysprompt_edit: Option<SysPromptEditState>,
    agent_chooser: Option<AgentChooserState>,
    presets: Vec<Preset>,
    active_preset_idx: Option<usize>,
    preset_panel_state: PresetPanelState,
}

#[derive(Debug, Clone, Copy)]
struct FooterView {
    auto_scroll: bool,
    completed: bool,
    run_started: bool,
    run_failed: bool,
    paused: bool,
    mouse_capture: bool,
    editing_prompt: bool,
    editing_turns: bool,
    agent_chooser: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterBadgeState {
    Ready,
    Live,
    Paused,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy)]
struct StateLineView<'a> {
    working: &'a WorkingTracker,
    spinner_frame: usize,
    paused: bool,
}

impl UiState {
    fn new(
        prompt: String,
        agent_cmds: [String; 2],
        turns: usize,
        routing_mode: RoutingMode,
        tri_pane_layout: bool,
        orb_layout: bool,
        thinking_expanded: bool,
        show_tmux_panels: bool,
        agent_system_prompts: [String; 2],
        initial_presets: Vec<Preset>,
        initial_active_preset_idx: Option<usize>,
    ) -> Self {
        Self {
            working: WorkingTracker::new(),
            active_agent: None,
            spinner_frame: 0,
            anim_frame: 0,
            orb_pos: 0.5,
            prompt,
            launched_prompt: None,
            run_started: false,
            run_failed: false,
            agent_cmds,
            turns,
            routing_mode,
            editing_prompt: false,
            edit_buffer: String::new(),
            edit_cursor: 0,
            edit_selection_anchor: None,
            editing_turns: false,
            turns_buffer: String::new(),
            lines: Vec::new(),
            turns_log: Vec::new(),
            active_turn_idx: None,
            tail_lines: Vec::new(),
            current_turn_line: None,
            scroll: 0,
            auto_scroll: true,
            paused: false,
            mouse_capture: true,
            finished: false,
            completed: false,
            frame_drawn: false,
            last_size: None,
            layout_mode: if orb_layout {
                LayoutMode::Orb
            } else if tri_pane_layout {
                LayoutMode::TriPaneThinking
            } else {
                LayoutMode::Classic
            },
            thinking_expanded,
            show_tmux_panels,
            modal_state: ModalState::Hidden,
            agent_system_prompts,
            sysprompt_edit: None,
            agent_chooser: None,
            presets: initial_presets,
            active_preset_idx: initial_active_preset_idx,
            preset_panel_state: PresetPanelState::Idle,
        }
    }

    fn apply_conversation_event(&mut self, event: ConversationEvent) {
        self.working.apply_event(&event);
        match event {
            ConversationEvent::TurnStart { agent_idx } => {
                if !self.run_started {
                    self.launched_prompt = Some(self.prompt.clone());
                }
                self.run_started = true;
                self.run_failed = false;
                self.completed = false;
                self.active_agent = Some(agent_idx);
                self.turns_log.push(TurnRecord {
                    agent_idx,
                    ..TurnRecord::default()
                });
                self.active_turn_idx = Some(self.turns_log.len() - 1);
                self.lines
                    .push(conversation::agent_name(agent_idx).to_string());
                // "N▎ " prefix: digit encodes agent so renderer knows which color to use
                self.lines.push(format!("{agent_idx}▎ "));
                self.current_turn_line = Some(self.lines.len() - 1);
            }
            ConversationEvent::Token { agent_idx, text } => {
                self.append_token(agent_idx, &text);
                self.append_turn_main(agent_idx, &text);
            }
            ConversationEvent::Thinking { agent_idx, text } => {
                self.append_turn_thinking(agent_idx, &text);
            }
            ConversationEvent::ToolEvent {
                agent_idx,
                kind,
                tool_type,
                text,
                tool_call_id,
            } => {
                self.append_turn_tool_event(
                    agent_idx,
                    kind,
                    &tool_type,
                    &text,
                    tool_call_id.as_deref(),
                );
            }
            ConversationEvent::TurnDone { agent_idx } => {
                self.flush_turn_main_suffix(agent_idx);
                self.lines.push(String::new());
                self.current_turn_line = None;
                self.active_turn_idx = None;
            }
            ConversationEvent::Done => {
                self.completed = true;
                self.run_failed = false;
                self.run_started = false;
                self.active_agent = None;
                self.launched_prompt = None;
                let msg = "  ✿ all done!  ·  press r to relaunch or Ctrl-Q to quit".to_string();
                self.lines.push(msg.clone());
                self.tail_lines.push(msg);
            }
            ConversationEvent::Error { code, message } => {
                self.run_failed = true;
                self.run_started = false;
                self.active_agent = None;
                self.launched_prompt = None;
                let msg = format!("  ✗ error:{}  {}", code.as_str(), message);
                self.lines.push(msg.clone());
                self.tail_lines.push(msg);
                self.completed = true;
            }
        }
    }

    fn on_conversation_closed(&mut self) {
        if self.completed {
            let msg = "  stream closed".to_string();
            self.lines.push(msg.clone());
            self.tail_lines.push(msg);
        }
    }

    fn append_token(&mut self, agent_idx: usize, token: &str) {
        if self.current_turn_line.is_none() {
            self.lines.push(format!("{agent_idx}▎ "));
            self.current_turn_line = Some(self.lines.len() - 1);
        }
        let current = self.current_turn_line.expect("current line set");

        let mut parts = token.split('\n');
        if let Some(first) = parts.next() {
            self.lines[current].push_str(first);
        }

        for part in parts {
            self.lines.push(format!("{agent_idx}▎ {part}"));
            self.current_turn_line = Some(self.lines.len() - 1);
        }
    }

    fn append_turn_main(&mut self, agent_idx: usize, text: &str) {
        if let Some(turn_idx) = self.active_turn_idx
            && let Some(turn) = self.turns_log.get_mut(turn_idx)
            && turn.agent_idx == agent_idx
        {
            append_main_token_with_boundaries(turn, text);
            return;
        }

        if let Some(turn) = self
            .turns_log
            .iter_mut()
            .rev()
            .find(|t| t.agent_idx == agent_idx)
        {
            append_main_token_with_boundaries(turn, text);
        }
    }

    fn flush_turn_main_suffix(&mut self, agent_idx: usize) {
        if let Some(turn_idx) = self.active_turn_idx
            && let Some(turn) = self.turns_log.get_mut(turn_idx)
            && turn.agent_idx == agent_idx
        {
            flush_pending_main_chunk_newlines(turn);
            return;
        }
        if let Some(turn) = self
            .turns_log
            .iter_mut()
            .rev()
            .find(|t| t.agent_idx == agent_idx)
        {
            flush_pending_main_chunk_newlines(turn);
        }
    }

    fn append_turn_thinking(&mut self, agent_idx: usize, text: &str) {
        if let Some(turn_idx) = self.active_turn_idx
            && let Some(turn) = self.turns_log.get_mut(turn_idx)
            && turn.agent_idx == agent_idx
        {
            append_reasoning_timeline_record(turn, text);
            return;
        }

        if let Some(turn) = self
            .turns_log
            .iter_mut()
            .rev()
            .find(|t| t.agent_idx == agent_idx)
        {
            append_reasoning_timeline_record(turn, text);
        }
    }

    fn append_turn_tool_event(
        &mut self,
        agent_idx: usize,
        kind: ToolStreamEventKind,
        tool_type: &str,
        text: &str,
        tool_call_id: Option<&str>,
    ) {
        let upsert_turn_event = |turn: &mut TurnRecord| {
            let label = text.trim();
            let item_type = tool_type.trim();
            let event_id = tool_call_id.map(str::trim).filter(|id| !id.is_empty());
            if label.is_empty() {
                return;
            }
            let last_tool = turn.timeline.iter().rev().find_map(|entry| match entry {
                ThinkingTimelineRecord::Reasoning(_) => None,
                ThinkingTimelineRecord::Tool(tool) => Some(tool),
                ThinkingTimelineRecord::MessageBoundary => None,
            });
            if last_tool.is_some_and(|prev| {
                prev.kind == kind
                    && prev.tool_type == item_type
                    && prev.text == label
                    && prev.tool_call_id.as_deref() == event_id
            }) {
                return;
            }
            // Collapse use/result pairs into a single timeline row by upgrading
            // the matching unresolved tool use entry when completion arrives.
            if matches!(
                kind,
                ToolStreamEventKind::Result | ToolStreamEventKind::Error
            ) && let Some(match_id) = event_id
                && let Some(prev_use) = turn.timeline.iter_mut().rev().find_map(|entry| match entry
                {
                    ThinkingTimelineRecord::Reasoning(_) => None,
                    ThinkingTimelineRecord::Tool(event) => {
                        if event.kind == ToolStreamEventKind::Use
                            && event.tool_call_id.as_deref() == Some(match_id)
                        {
                            Some(event)
                        } else {
                            None
                        }
                    }
                    ThinkingTimelineRecord::MessageBoundary => None,
                })
            {
                prev_use.kind = kind;
                prev_use.tool_type = item_type.to_string();
                prev_use.text = label.to_string();
                prev_use.tool_call_id = Some(match_id.to_string());
                return;
            }
            turn.timeline
                .push(ThinkingTimelineRecord::Tool(ToolEventRecord {
                    kind,
                    tool_type: item_type.to_string(),
                    text: label.to_string(),
                    tool_call_id: event_id.map(ToOwned::to_owned),
                }));
        };

        if let Some(turn_idx) = self.active_turn_idx
            && let Some(turn) = self.turns_log.get_mut(turn_idx)
            && turn.agent_idx == agent_idx
        {
            upsert_turn_event(turn);
            return;
        }

        if let Some(turn) = self
            .turns_log
            .iter_mut()
            .rev()
            .find(|t| t.agent_idx == agent_idx)
        {
            upsert_turn_event(turn);
        }
    }

    fn clear_chat(&mut self) {
        if !self.working.agent_working.iter().any(|working| *working) {
            self.active_agent = None;
        }
        self.lines.clear();
        self.turns_log.clear();
        self.tail_lines.clear();
        self.current_turn_line = None;
        self.active_turn_idx = None;
        self.scroll = 0;
        self.auto_scroll = true;
        self.completed = false;

        // If a turn is currently active, recreate the current speaker block so streaming continues cleanly.
        if let Some(agent_idx) = self
            .active_agent
            .filter(|idx| self.working.agent_working[*idx])
        {
            self.turns_log.push(TurnRecord {
                agent_idx,
                ..TurnRecord::default()
            });
            self.active_turn_idx = Some(self.turns_log.len() - 1);
            self.lines
                .push(conversation::agent_name(agent_idx).to_string());
            self.lines.push(format!("{agent_idx}▎ "));
            self.current_turn_line = Some(self.lines.len() - 1);
        }
    }

    fn prompt_char_len(&self) -> usize {
        self.edit_buffer.chars().count()
    }

    fn prompt_byte_idx(&self, char_idx: usize) -> usize {
        if char_idx == 0 {
            return 0;
        }
        self.edit_buffer
            .char_indices()
            .nth(char_idx)
            .map_or(self.edit_buffer.len(), |(idx, _)| idx)
    }

    fn prompt_selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.edit_selection_anchor?;
        if anchor == self.edit_cursor {
            return None;
        }
        Some((anchor.min(self.edit_cursor), anchor.max(self.edit_cursor)))
    }

    fn clear_prompt_selection(&mut self) {
        self.edit_selection_anchor = None;
    }

    fn move_prompt_cursor_to(&mut self, pos: usize, selecting: bool) {
        let next = pos.min(self.prompt_char_len());
        if selecting {
            if self.edit_selection_anchor.is_none() {
                self.edit_selection_anchor = Some(self.edit_cursor);
            }
        } else {
            self.edit_selection_anchor = None;
        }
        self.edit_cursor = next;
    }

    fn move_prompt_cursor_left(&mut self, selecting: bool) {
        if !selecting && let Some((start, _)) = self.prompt_selection_range() {
            self.edit_cursor = start;
            self.clear_prompt_selection();
            return;
        }
        self.move_prompt_cursor_to(self.edit_cursor.saturating_sub(1), selecting);
    }

    fn move_prompt_cursor_right(&mut self, selecting: bool) {
        if !selecting && let Some((_, end)) = self.prompt_selection_range() {
            self.edit_cursor = end;
            self.clear_prompt_selection();
            return;
        }
        self.move_prompt_cursor_to(self.edit_cursor.saturating_add(1), selecting);
    }

    fn move_prompt_cursor_home(&mut self, selecting: bool) {
        if !selecting && let Some((start, _)) = self.prompt_selection_range() {
            self.edit_cursor = start;
            self.clear_prompt_selection();
            return;
        }
        self.move_prompt_cursor_to(0, selecting);
    }

    fn move_prompt_cursor_end(&mut self, selecting: bool) {
        if !selecting && let Some((_, end)) = self.prompt_selection_range() {
            self.edit_cursor = end;
            self.clear_prompt_selection();
            return;
        }
        self.move_prompt_cursor_to(self.prompt_char_len(), selecting);
    }

    fn prompt_select_all(&mut self) {
        self.edit_selection_anchor = Some(0);
        self.edit_cursor = self.prompt_char_len();
    }

    fn delete_prompt_selection(&mut self) -> bool {
        let Some((start, end)) = self.prompt_selection_range() else {
            return false;
        };
        let start_byte = self.prompt_byte_idx(start);
        let end_byte = self.prompt_byte_idx(end);
        self.edit_buffer.replace_range(start_byte..end_byte, "");
        self.edit_cursor = start;
        self.clear_prompt_selection();
        true
    }

    fn insert_prompt_char(&mut self, c: char) {
        let _ = self.delete_prompt_selection();
        let byte_idx = self.prompt_byte_idx(self.edit_cursor);
        self.edit_buffer.insert(byte_idx, c);
        self.edit_cursor = self.edit_cursor.saturating_add(1);
    }

    fn prompt_backspace(&mut self) {
        if self.delete_prompt_selection() {
            return;
        }
        if self.edit_cursor == 0 {
            return;
        }
        let start = self.prompt_byte_idx(self.edit_cursor - 1);
        let end = self.prompt_byte_idx(self.edit_cursor);
        self.edit_buffer.replace_range(start..end, "");
        self.edit_cursor -= 1;
    }

    fn prompt_delete(&mut self) {
        if self.delete_prompt_selection() {
            return;
        }
        let len = self.prompt_char_len();
        if self.edit_cursor >= len {
            return;
        }
        let start = self.prompt_byte_idx(self.edit_cursor);
        let end = self.prompt_byte_idx(self.edit_cursor + 1);
        self.edit_buffer.replace_range(start..end, "");
    }

    fn open_sysprompt_edit(&mut self, agent_idx: usize) {
        let buffers = self.agent_system_prompts.clone();
        let cursors = buffers.clone().map(|buffer| buffer.len());
        self.sysprompt_edit = Some(SysPromptEditState {
            active_agent_idx: agent_idx,
            buffers,
            cursors,
        });
    }

    fn confirm_sysprompt_edit(&mut self) {
        if let Some(edit) = self.sysprompt_edit.take() {
            self.agent_system_prompts.clone_from_slice(&edit.buffers);
        }
    }

    fn cancel_sysprompt_edit(&mut self) {
        self.sysprompt_edit = None;
    }

    fn sysprompt_switch_target(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        edit.active_agent_idx = (edit.active_agent_idx + 1) % edit.buffers.len();
    }

    fn sysprompt_insert_char(&mut self, c: char) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        edit.buffers[agent_idx].insert(edit.cursors[agent_idx], c);
        edit.cursors[agent_idx] += c.len_utf8();
    }

    fn sysprompt_backspace(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        let cursor = edit.cursors[agent_idx];
        if cursor == 0 {
            return;
        }
        // Find the previous char boundary in the active draft.
        let prev = edit.buffers[agent_idx][..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        edit.buffers[agent_idx].remove(prev);
        edit.cursors[agent_idx] = prev;
    }

    fn sysprompt_delete(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        let cursor = edit.cursors[agent_idx];
        if cursor >= edit.buffers[agent_idx].len() {
            return;
        }
        edit.buffers[agent_idx].remove(cursor);
    }

    fn sysprompt_move_left(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        let cursor = edit.cursors[agent_idx];
        if cursor == 0 {
            return;
        }
        edit.cursors[agent_idx] = edit.buffers[agent_idx][..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    fn sysprompt_move_right(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        let cursor = edit.cursors[agent_idx];
        if cursor >= edit.buffers[agent_idx].len() {
            return;
        }
        let ch = edit.buffers[agent_idx][cursor..].chars().next().unwrap();
        edit.cursors[agent_idx] += ch.len_utf8();
    }

    fn sysprompt_move_home(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        // Find start of current line in the active draft.
        let before = &edit.buffers[agent_idx][..edit.cursors[agent_idx]];
        edit.cursors[agent_idx] = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    }

    fn sysprompt_move_end(&mut self) {
        let Some(edit) = self.sysprompt_edit.as_mut() else {
            return;
        };
        let agent_idx = edit.active_agent_idx;
        // Find end of current line in the active draft.
        let after = &edit.buffers[agent_idx][edit.cursors[agent_idx]..];
        edit.cursors[agent_idx] += after.find('\n').unwrap_or(after.len());
    }

    fn has_staged_prompt(&self) -> bool {
        self.run_started
            && self
                .launched_prompt
                .as_deref()
                .is_some_and(|launched| launched != self.prompt)
    }

    fn run_is_active_or_paused(&self) -> bool {
        self.run_started || self.paused || self.working.agent_working.iter().any(|working| *working)
    }

    fn stop_run(&mut self, control_tx: &watch::Sender<ConversationControl>) {
        self.paused = false;
        let _ = control_tx.send(ConversationControl::Stop);
    }

    fn commit_prompt_edit_and_exit(&mut self) {
        self.prompt = self.edit_buffer.clone();
        self.editing_prompt = false;
        self.edit_buffer.clear();
        self.edit_cursor = 0;
        self.edit_selection_anchor = None;
    }

    fn commit_turn_edit_and_exit(&mut self) {
        if let Ok(v) = self.turns_buffer.parse::<usize>()
            && v > 0
        {
            self.turns = v;
        }
        self.editing_turns = false;
        self.turns_buffer.clear();
    }

    fn open_agent_chooser(&mut self, agent_idx: usize) {
        self.agent_chooser = Some(AgentChooserState {
            agent_idx,
            cursor: agent_command_choice_index(&self.agent_cmds[agent_idx]),
        });
    }

    fn cancel_agent_chooser(&mut self) {
        self.agent_chooser = None;
    }

    fn agent_chooser_up(&mut self) {
        let Some(chooser) = self.agent_chooser.as_mut() else {
            return;
        };
        chooser.cursor = chooser.cursor.saturating_sub(1);
    }

    fn agent_chooser_down(&mut self) {
        let Some(chooser) = self.agent_chooser.as_mut() else {
            return;
        };
        chooser.cursor = (chooser.cursor + 1).min(AGENT_COMMAND_CHOICES.len().saturating_sub(1));
    }

    fn confirm_agent_chooser(&mut self) {
        let Some(chooser) = self.agent_chooser.take() else {
            return;
        };
        self.agent_cmds[chooser.agent_idx] = AGENT_COMMAND_CHOICES
            .get(chooser.cursor)
            .unwrap_or(&AGENT_COMMAND_CHOICES[0])
            .to_string();
    }

    fn execute_command_action(
        &mut self,
        action: CommandActionId,
        control_tx: &watch::Sender<ConversationControl>,
    ) -> Option<UiAction> {
        match action {
            CommandActionId::Quit => {
                let _ = control_tx.send(ConversationControl::Stop);
                self.finished = true;
                Some(UiAction::Quit {
                    prompt: self.prompt.clone(),
                    agent_a: self.agent_cmds[0].clone(),
                    agent_b: self.agent_cmds[1].clone(),
                    turns: self.turns,
                    routing_mode: self.routing_mode,
                    tri_pane_layout: self.layout_mode == LayoutMode::TriPaneThinking,
                    orb_layout: self.layout_mode == LayoutMode::Orb,
                    thinking_expanded: self.thinking_expanded,
                    show_tmux_panels: self.show_tmux_panels,
                    agent_system_prompts: self.agent_system_prompts.clone(),
                    presets: self.presets.clone(),
                    active_preset_idx: self.active_preset_idx,
                })
            }
            CommandActionId::PauseResume => {
                self.paused = !self.paused;
                let target = if self.paused {
                    ConversationControl::Pause
                } else {
                    ConversationControl::Run
                };
                let _ = control_tx.send(target);
                None
            }
            CommandActionId::EditPrompt => {
                self.editing_prompt = true;
                self.edit_buffer = self.prompt.clone();
                self.edit_cursor = self.prompt_char_len();
                self.edit_selection_anchor = None;
                None
            }
            CommandActionId::Select => {
                self.mouse_capture = !self.mouse_capture;
                if !self.mouse_capture {
                    self.auto_scroll = false;
                }
                None
            }
            CommandActionId::ToggleAgentA => {
                self.open_agent_chooser(0);
                None
            }
            CommandActionId::ToggleAgentB => {
                self.open_agent_chooser(1);
                None
            }
            CommandActionId::Relaunch => {
                if !self.working.agent_working.iter().any(|v| *v) {
                    let _ = control_tx.send(ConversationControl::Stop);
                    return Some(UiAction::Relaunch {
                        prompt: self.prompt.clone(),
                        agent_a: self.agent_cmds[0].clone(),
                        agent_b: self.agent_cmds[1].clone(),
                        turns: self.turns,
                        routing_mode: self.routing_mode,
                        tri_pane_layout: self.layout_mode == LayoutMode::TriPaneThinking,
                        orb_layout: self.layout_mode == LayoutMode::Orb,
                        thinking_expanded: self.thinking_expanded,
                        show_tmux_panels: self.show_tmux_panels,
                        agent_system_prompts: self.agent_system_prompts.clone(),
                        presets: self.presets.clone(),
                        active_preset_idx: self.active_preset_idx,
                    });
                }
                None
            }
            CommandActionId::Clear => {
                self.clear_chat();
                None
            }
            CommandActionId::Mode => {
                self.routing_mode = self.routing_mode.next();
                None
            }
            CommandActionId::Layout => {
                self.layout_mode = self.layout_mode.next();
                None
            }
            CommandActionId::Thinking => {
                self.thinking_expanded = !self.thinking_expanded;
                None
            }
            CommandActionId::Tmux => {
                self.show_tmux_panels = !self.show_tmux_panels;
                None
            }
            CommandActionId::SysPromptA => {
                self.open_sysprompt_edit(0);
                None
            }
            CommandActionId::SysPromptB => {
                self.open_sysprompt_edit(1);
                None
            }
            CommandActionId::PresetMode => {
                self.preset_open_mode();
                None
            }
            CommandActionId::EditTurns => {
                self.editing_turns = true;
                self.turns_buffer = self.turns.to_string();
                None
            }
            CommandActionId::Help => {
                self.modal_state = match self.modal_state {
                    ModalState::Help => {
                        self.last_size = None;
                        ModalState::Hidden
                    }
                    _ => ModalState::Help,
                };
                None
            }
        }
    }

    fn render(&mut self, out: &mut impl Write) -> Result<(), AppError> {
        queue!(out, BeginSynchronizedUpdate)?;
        let (width, height) = terminal::size()?;
        let width = width.max(1) as usize;
        let height = height.max(2) as usize;

        let size = (width, height);
        if !self.frame_drawn || self.last_size != Some(size) {
            queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
            self.frame_drawn = true;
            self.last_size = Some(size);
        }

        if width < 50 || height < 16 {
            let msg = "Terminal too small. Resize to at least 50x16.";
            queue!(out, MoveTo(0, 0), Print(msg))?;
            out.flush()?;
            return Ok(());
        }

        let prompt_h = self.prompt_panel_height(height);

        let prompt_y = 1usize;
        self.render_briefing_panel(out, 1, prompt_y, width - 2, prompt_h)?;

        let conv_y = prompt_y + prompt_h;
        let footer_top = height.saturating_sub(2);
        let state_line_y = footer_top.saturating_sub(1);
        let conv_h = state_line_y.saturating_sub(conv_y);
        let body_height = conv_h.saturating_sub(2);
        if self.layout_mode == LayoutMode::Classic {
            const PANEL_PAD_X: usize = 1;
            let content_x = 2 + PANEL_PAD_X;
            let content_w = width.saturating_sub(4 + PANEL_PAD_X * 2);
            let wrapped = wrap_lines(&self.lines, content_w, false);
            self.clamp_scroll(wrapped.len(), body_height);
            draw_box(
                out,
                1,
                conv_y,
                width - 2,
                conv_h,
                Some("✦ Conversation"),
                None,
            )?;

            for row in 0..body_height {
                let idx = self.scroll + row;
                queue!(out, MoveTo(content_x as u16, (conv_y + 1 + row) as u16))?;
                if let Some(line) = wrapped.get(idx) {
                    render_conv_line(out, line, content_w, false)?;
                } else {
                    queue!(out, Print(" ".repeat(content_w)))?;
                }
            }
        } else if self.layout_mode == LayoutMode::TriPaneThinking {
            const PANEL_PAD_X: usize = 1;
            let panel_total_w = width - 2;
            // 3 sub-boxes with their own borders => reserve 6 columns for borders.
            let panel_inner = panel_total_w.saturating_sub(6);
            // mid gets 40%, left and right share the remaining 60% equally.
            let mid_inner = (panel_inner * 2) / 5;
            let side_inner = panel_inner.saturating_sub(mid_inner);
            let left_inner = side_inner / 2 + side_inner % 2;
            let right_inner = side_inner / 2;

            let left_w = left_inner + 2;
            let mid_w = mid_inner + 2;
            let right_w = right_inner + 2;

            let left_x = 1usize;
            let mid_x = left_x + left_w;
            let right_x = mid_x + mid_w;
            let left_border = if self.active_agent == Some(0) {
                Some(agent_color(0))
            } else {
                None
            };
            let right_border = if self.active_agent == Some(1) {
                Some(agent_color(1))
            } else {
                None
            };
            let left_title = self.thinking_panel_title(0);
            let right_title = self.thinking_panel_title(1);

            // System prompt boxes (top of each side panel)
            let sysprompt_h = SYSPROMPT_HEIGHT.min(conv_h / 2);
            let left_chooser_open = self
                .agent_chooser
                .is_some_and(|chooser| chooser.agent_idx == 0);
            let right_chooser_open = self
                .agent_chooser
                .is_some_and(|chooser| chooser.agent_idx == 1);
            let left_sp_border = self
                .sysprompt_edit
                .as_ref()
                .filter(|e| e.active_agent_idx == 0)
                .map(|_| agent_color(0))
                .or_else(|| left_chooser_open.then(|| agent_color(0)));
            let right_sp_border = self
                .sysprompt_edit
                .as_ref()
                .filter(|e| e.active_agent_idx == 1)
                .map(|_| agent_color(1))
                .or_else(|| right_chooser_open.then(|| agent_color(1)));
            draw_box(
                out,
                left_x,
                conv_y,
                left_w,
                sysprompt_h,
                Some(if left_chooser_open {
                    "Aria Agent [a]"
                } else {
                    "Aria System Prompt [q]"
                }),
                left_sp_border,
            )?;
            draw_box(
                out,
                right_x,
                conv_y,
                right_w,
                sysprompt_h,
                Some(if right_chooser_open {
                    "Basil Agent [d]"
                } else {
                    "Basil System Prompt [e]"
                }),
                right_sp_border,
            )?;

            // Thinking/conversation boxes (below top system-prompt row)
            let think_y = conv_y + sysprompt_h;
            let think_h = conv_h.saturating_sub(sysprompt_h);
            draw_box(
                out,
                left_x,
                think_y,
                left_w,
                think_h,
                Some(&left_title),
                left_border,
            )?;
            let preset_border = self.preset_mode_active().then_some(Color::Rgb {
                r: 130,
                g: 160,
                b: 220,
            });
            // Preset box occupies the same height as the side system-prompt boxes.
            draw_box(
                out,
                mid_x,
                conv_y,
                mid_w,
                sysprompt_h,
                Some(if self.preset_mode_active() {
                    "◈ Preset Mode [s]"
                } else {
                    "◈ Presets [s]"
                }),
                preset_border,
            )?;
            render_preset_panel_content(
                out,
                mid_x + 1 + PANEL_PAD_X,
                conv_y + 1,
                mid_w.saturating_sub(2 + PANEL_PAD_X * 2),
                sysprompt_h.saturating_sub(2),
                self,
            )?;
            draw_box(
                out,
                mid_x,
                think_y,
                mid_w,
                think_h,
                Some("✦ Conversation"),
                None,
            )?;
            draw_box(
                out,
                right_x,
                think_y,
                right_w,
                think_h,
                Some(&right_title),
                right_border,
            )?;

            let left_content_w = left_w.saturating_sub(2 + PANEL_PAD_X * 2);
            let mid_content_w = mid_w.saturating_sub(2 + PANEL_PAD_X * 2);
            let right_content_w = right_w.saturating_sub(2 + PANEL_PAD_X * 2);

            // The top side boxes can either show system prompts or the focused agent chooser.
            render_top_side_panel_content(
                out,
                left_x + 1 + PANEL_PAD_X,
                conv_y + 1,
                left_content_w,
                sysprompt_h.saturating_sub(2),
                0,
                self,
            )?;
            render_top_side_panel_content(
                out,
                right_x + 1 + PANEL_PAD_X,
                conv_y + 1,
                right_content_w,
                sysprompt_h.saturating_sub(2),
                1,
                self,
            )?;

            let think_body_height = think_h.saturating_sub(2);
            let (wrapped_a, wrapped_c, wrapped_b) =
                self.build_tri_pane_rows(left_content_w, mid_content_w, right_content_w);
            self.clamp_scroll(wrapped_c.len(), think_body_height);

            for row in 0..think_body_height {
                let idx = self.scroll + row;

                queue!(
                    out,
                    MoveTo(
                        (left_x + 1 + PANEL_PAD_X) as u16,
                        (think_y + 1 + row) as u16
                    )
                )?;
                if let Some(line) = wrapped_a.get(idx) {
                    render_thinking_line(
                        out,
                        line,
                        left_content_w,
                        0,
                        self.spinner_frame,
                        self.working.agent_working[0] && self.active_agent == Some(0),
                    )?;
                } else {
                    queue!(out, Print(" ".repeat(left_content_w)))?;
                }

                queue!(
                    out,
                    MoveTo((mid_x + 1 + PANEL_PAD_X) as u16, (think_y + 1 + row) as u16)
                )?;
                if let Some(line) = wrapped_c.get(idx) {
                    render_conv_line(out, line, mid_content_w, true)?;
                } else {
                    queue!(out, Print(" ".repeat(mid_content_w)))?;
                }

                queue!(
                    out,
                    MoveTo(
                        (right_x + 1 + PANEL_PAD_X) as u16,
                        (think_y + 1 + row) as u16
                    )
                )?;
                if let Some(line) = wrapped_b.get(idx) {
                    render_thinking_line(
                        out,
                        line,
                        right_content_w,
                        1,
                        self.spinner_frame,
                        self.working.agent_working[1] && self.active_agent == Some(1),
                    )?;
                } else {
                    queue!(out, Print(" ".repeat(right_content_w)))?;
                }
            }
        } else {
            // LayoutMode::Orb — tri-pane with animated orb in the centre column.
            const PANEL_PAD_X: usize = 1;
            let panel_total_w = width - 2;
            // 3 sub-boxes with their own borders => reserve 6 columns for borders.
            let panel_inner = panel_total_w.saturating_sub(6);
            // mid gets 40%, left and right share the remaining 60% equally.
            let mid_inner = (panel_inner * 2) / 5;
            let side_inner = panel_inner.saturating_sub(mid_inner);
            let left_inner = side_inner / 2 + side_inner % 2;
            let right_inner = side_inner / 2;

            let left_w = left_inner + 2;
            let mid_w = mid_inner + 2;
            let right_w = right_inner + 2;

            let left_x = 1usize;
            let mid_x = left_x + left_w;
            let right_x = mid_x + mid_w;
            let left_border = if self.active_agent == Some(0) {
                Some(agent_color(0))
            } else {
                None
            };
            let right_border = if self.active_agent == Some(1) {
                Some(agent_color(1))
            } else {
                None
            };
            let left_title = self.thinking_panel_title(0);
            let right_title = self.thinking_panel_title(1);

            // System prompt boxes (top of each side panel)
            let sysprompt_h = SYSPROMPT_HEIGHT.min(conv_h / 2);
            let left_chooser_open = self
                .agent_chooser
                .is_some_and(|chooser| chooser.agent_idx == 0);
            let right_chooser_open = self
                .agent_chooser
                .is_some_and(|chooser| chooser.agent_idx == 1);
            let left_sp_border = self
                .sysprompt_edit
                .as_ref()
                .filter(|e| e.active_agent_idx == 0)
                .map(|_| agent_color(0))
                .or_else(|| left_chooser_open.then(|| agent_color(0)));
            let right_sp_border = self
                .sysprompt_edit
                .as_ref()
                .filter(|e| e.active_agent_idx == 1)
                .map(|_| agent_color(1))
                .or_else(|| right_chooser_open.then(|| agent_color(1)));
            draw_box(
                out,
                left_x,
                conv_y,
                left_w,
                sysprompt_h,
                Some(if left_chooser_open {
                    "Aria Agent [a]"
                } else {
                    "Aria System Prompt [q]"
                }),
                left_sp_border,
            )?;
            draw_box(
                out,
                right_x,
                conv_y,
                right_w,
                sysprompt_h,
                Some(if right_chooser_open {
                    "Basil Agent [d]"
                } else {
                    "Basil System Prompt [e]"
                }),
                right_sp_border,
            )?;

            // Thinking/conversation boxes (below top system-prompt row)
            let think_y = conv_y + sysprompt_h;
            let think_h = conv_h.saturating_sub(sysprompt_h);
            draw_box(
                out,
                left_x,
                think_y,
                left_w,
                think_h,
                Some(&left_title),
                left_border,
            )?;
            let preset_border = self.preset_mode_active().then_some(Color::Rgb {
                r: 130,
                g: 160,
                b: 220,
            });
            // Give ~40% to presets, ~60% to orb — presets needs at least 4 rows (2 inner: name + hint)
            let preset_top_h = ((sysprompt_h * 2) / 5).max(4).min(sysprompt_h);
            let preset_bot_h = sysprompt_h.saturating_sub(preset_top_h);
            let preset_bot_y = conv_y + preset_top_h;
            draw_box(
                out,
                mid_x,
                conv_y,
                mid_w,
                preset_top_h,
                Some(if self.preset_mode_active() {
                    "◈ Preset Mode [s]"
                } else {
                    "◈ Presets [s]"
                }),
                preset_border,
            )?;
            render_preset_panel_content(
                out,
                mid_x + 1 + PANEL_PAD_X,
                conv_y + 1,
                mid_w.saturating_sub(2 + PANEL_PAD_X * 2),
                preset_top_h.saturating_sub(2),
                self,
            )?;
            render_orb_panel(
                out,
                self.anim_frame,
                self.orb_pos,
                self.active_agent,
                self.working.agent_working.iter().any(|v| *v),
                mid_x,
                preset_bot_y,
                mid_w,
                preset_bot_h,
            )?;
            draw_box(
                out,
                mid_x,
                think_y,
                mid_w,
                think_h,
                Some("✦ Conversation"),
                None,
            )?;
            draw_box(
                out,
                right_x,
                think_y,
                right_w,
                think_h,
                Some(&right_title),
                right_border,
            )?;

            let left_content_w = left_w.saturating_sub(2 + PANEL_PAD_X * 2);
            let mid_content_w = mid_w.saturating_sub(2 + PANEL_PAD_X * 2);
            let right_content_w = right_w.saturating_sub(2 + PANEL_PAD_X * 2);

            // The top side boxes can either show system prompts or the focused agent chooser.
            render_top_side_panel_content(
                out,
                left_x + 1 + PANEL_PAD_X,
                conv_y + 1,
                left_content_w,
                sysprompt_h.saturating_sub(2),
                0,
                self,
            )?;
            render_top_side_panel_content(
                out,
                right_x + 1 + PANEL_PAD_X,
                conv_y + 1,
                right_content_w,
                sysprompt_h.saturating_sub(2),
                1,
                self,
            )?;

            let think_body_height = think_h.saturating_sub(2);
            let (wrapped_a, wrapped_c, wrapped_b) =
                self.build_tri_pane_rows(left_content_w, mid_content_w, right_content_w);
            self.clamp_scroll(wrapped_c.len(), think_body_height);

            for row in 0..think_body_height {
                let idx = self.scroll + row;

                queue!(
                    out,
                    MoveTo(
                        (left_x + 1 + PANEL_PAD_X) as u16,
                        (think_y + 1 + row) as u16
                    )
                )?;
                if let Some(line) = wrapped_a.get(idx) {
                    render_thinking_line(
                        out,
                        line,
                        left_content_w,
                        0,
                        self.spinner_frame,
                        self.working.agent_working[0] && self.active_agent == Some(0),
                    )?;
                } else {
                    queue!(out, Print(" ".repeat(left_content_w)))?;
                }

                queue!(
                    out,
                    MoveTo((mid_x + 1 + PANEL_PAD_X) as u16, (think_y + 1 + row) as u16)
                )?;
                if let Some(line) = wrapped_c.get(idx) {
                    render_conv_line(out, line, mid_content_w, true)?;
                } else {
                    queue!(out, Print(" ".repeat(mid_content_w)))?;
                }

                queue!(
                    out,
                    MoveTo(
                        (right_x + 1 + PANEL_PAD_X) as u16,
                        (think_y + 1 + row) as u16
                    )
                )?;
                if let Some(line) = wrapped_b.get(idx) {
                    render_thinking_line(
                        out,
                        line,
                        right_content_w,
                        1,
                        self.spinner_frame,
                        self.working.agent_working[1] && self.active_agent == Some(1),
                    )?;
                } else {
                    queue!(out, Print(" ".repeat(right_content_w)))?;
                }
            }
        }

        if self.preset_mode_active() {
            render_preset_mode_overlay(out, 1, conv_y, width.saturating_sub(2), conv_h, self)?;
        }

        render_footer_state_line(
            out,
            state_line_y,
            width.saturating_sub(2),
            StateLineView {
                working: &self.working,
                spinner_frame: self.spinner_frame,
                paused: self.paused,
            },
        )?;

        render_footer_lines(
            out,
            footer_top,
            width.saturating_sub(2),
            FooterView {
                auto_scroll: self.auto_scroll,
                completed: self.completed,
                run_started: self.run_started,
                run_failed: self.run_failed,
                paused: self.paused,
                mouse_capture: self.mouse_capture,
                editing_prompt: self.editing_prompt,
                editing_turns: self.editing_turns,
                agent_chooser: self.agent_chooser.map(|chooser| chooser.agent_idx),
            },
        )?;

        // Render help modal if open.
        if self.modal_state == ModalState::Help {
            render_help_modal(out, width, height)?;
        }

        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        Ok(())
    }

    fn build_tri_pane_rows(
        &self,
        left_thinking_width: usize,
        main_width: usize,
        right_thinking_width: usize,
    ) -> (
        Vec<ThinkingDisplayLine>,
        Vec<String>,
        Vec<ThinkingDisplayLine>,
    ) {
        let mut left_rows = Vec::new();
        let mut main_rows = Vec::new();
        let mut right_rows = Vec::new();

        for turn in &self.turns_log {
            let think_width = if turn.agent_idx == 0 {
                left_thinking_width
            } else {
                right_thinking_width
            };
            let timeline_segments = if turn.timeline.is_empty() {
                Vec::new()
            } else {
                timeline_segments(&turn.timeline)
            };

            let main_chunks: Vec<String> = if turn.main_chunks.is_empty() {
                vec![String::new()]
            } else {
                turn.main_chunks.clone()
            };
            let segment_count = main_chunks.len().max(timeline_segments.len()).max(1);

            for seg_idx in 0..segment_count {
                let mut main_block: Vec<String> = Vec::new();
                if seg_idx == 0 {
                    main_block.push(conversation::agent_name(turn.agent_idx).to_string());
                }
                let chunk = main_chunks
                    .get(seg_idx)
                    .map(String::as_str)
                    .unwrap_or_default();
                if chunk.is_empty() {
                    main_block.push(format!("{}▎ ", turn.agent_idx));
                } else {
                    for part in chunk.split('\n') {
                        main_block.push(format!("{}▎ {}", turn.agent_idx, part));
                    }
                }
                let main_wrapped = wrap_lines(&main_block, main_width, true);

                let mut segment_thinking_rows: Vec<ThinkingDisplayLine> = Vec::new();
                if !turn.timeline.is_empty() {
                    if seg_idx == 0 {
                        segment_thinking_rows.push(ThinkingDisplayLine {
                            kind: ThinkingLineKind::ToolHeader,
                            text: "┌ tools".to_string(),
                        });
                    }
                    if let Some(segment) = timeline_segments.get(seg_idx) {
                        append_timeline_segment_display_lines(
                            &mut segment_thinking_rows,
                            segment,
                            think_width,
                        );
                    }
                    if seg_idx + 1 == segment_count {
                        segment_thinking_rows.push(ThinkingDisplayLine {
                            kind: ThinkingLineKind::ToolFooter,
                            text: "└".to_string(),
                        });
                    }
                }

                if !self.thinking_expanded {
                    let target = main_wrapped.len().max(1);
                    truncate_thinking_rows(&mut segment_thinking_rows, target, think_width);
                }

                let block_h = if self.thinking_expanded {
                    main_wrapped.len().max(segment_thinking_rows.len()).max(1)
                } else {
                    main_wrapped.len().max(1)
                };
                for i in 0..block_h {
                    main_rows.push(main_wrapped.get(i).cloned().unwrap_or_default());
                    if turn.agent_idx == 0 {
                        left_rows.push(segment_thinking_rows.get(i).cloned().unwrap_or(
                            ThinkingDisplayLine {
                                kind: ThinkingLineKind::Blank,
                                text: String::new(),
                            },
                        ));
                        right_rows.push(ThinkingDisplayLine {
                            kind: ThinkingLineKind::Blank,
                            text: String::new(),
                        });
                    } else {
                        left_rows.push(ThinkingDisplayLine {
                            kind: ThinkingLineKind::Blank,
                            text: String::new(),
                        });
                        right_rows.push(segment_thinking_rows.get(i).cloned().unwrap_or(
                            ThinkingDisplayLine {
                                kind: ThinkingLineKind::Blank,
                                text: String::new(),
                            },
                        ));
                    }
                }

                if seg_idx + 1 < segment_count {
                    left_rows.push(ThinkingDisplayLine {
                        kind: ThinkingLineKind::Blank,
                        text: String::new(),
                    });
                    main_rows.push(String::new());
                    right_rows.push(ThinkingDisplayLine {
                        kind: ThinkingLineKind::Blank,
                        text: String::new(),
                    });
                }
            }

            left_rows.push(ThinkingDisplayLine {
                kind: ThinkingLineKind::Blank,
                text: String::new(),
            });
            main_rows.push(String::new());
            right_rows.push(ThinkingDisplayLine {
                kind: ThinkingLineKind::Blank,
                text: String::new(),
            });
        }

        let tail_wrapped = wrap_lines(&self.tail_lines, main_width, true);
        for line in tail_wrapped {
            left_rows.push(ThinkingDisplayLine {
                kind: ThinkingLineKind::Blank,
                text: String::new(),
            });
            main_rows.push(line);
            right_rows.push(ThinkingDisplayLine {
                kind: ThinkingLineKind::Blank,
                text: String::new(),
            });
        }

        (left_rows, main_rows, right_rows)
    }

    fn max_scroll(&self) -> usize {
        let (width, height) = match self.last_size {
            Some(v) => v,
            None => return 0,
        };
        self.compute_max_scroll(width, height)
    }

    fn thinking_panel_title(&self, agent_idx: usize) -> String {
        let symbol =
            if self.working.agent_working[agent_idx] && self.active_agent == Some(agent_idx) {
                if agent_idx == 1 {
                    BASIL_NATURE_THINK_FRAMES[self.spinner_frame % BASIL_NATURE_THINK_FRAMES.len()]
                        .to_string()
                } else {
                    ARIA_PURPLE_THINK_FRAMES[self.spinner_frame % ARIA_PURPLE_THINK_FRAMES.len()]
                        .to_string()
                }
            } else {
                "*".to_string()
            };
        let name = conversation::agent_name(agent_idx);
        format!("{symbol} {name} Thinking")
    }

    fn compute_max_scroll(&self, width: usize, height: usize) -> usize {
        if width < 50 || height < 16 {
            return 0;
        }

        let prompt_h = self.prompt_panel_height(height);
        let conv_y = 1usize + prompt_h;
        let footer_top = height.saturating_sub(2);
        let state_line_y = footer_top.saturating_sub(1);
        let conv_h = state_line_y.saturating_sub(conv_y);
        let body_height = conv_h.saturating_sub(2);

        if self.layout_mode == LayoutMode::Classic {
            const PANEL_PAD_X: usize = 1;
            let conv_width = width.saturating_sub(4 + PANEL_PAD_X * 2);
            let wrapped = wrap_lines(&self.lines, conv_width, false);
            return wrapped.len().saturating_sub(body_height);
        }

        const PANEL_PAD_X: usize = 1;
        let sysprompt_h = SYSPROMPT_HEIGHT.min(conv_h / 2);
        let think_h = conv_h.saturating_sub(sysprompt_h);
        let tri_body_height = think_h.saturating_sub(2);
        let panel_total_w = width - 2;
        let panel_inner = panel_total_w.saturating_sub(6);
        let mid_inner = (panel_inner * 2) / 5;
        let side_inner = panel_inner.saturating_sub(mid_inner);
        let left_inner = side_inner / 2 + side_inner % 2;
        let right_inner = side_inner / 2;

        let left_content_w = (left_inner + 2).saturating_sub(2 + PANEL_PAD_X * 2);
        let mid_content_w = (mid_inner + 2).saturating_sub(2 + PANEL_PAD_X * 2);
        let right_content_w = (right_inner + 2).saturating_sub(2 + PANEL_PAD_X * 2);

        let (_a, c, _b) = self.build_tri_pane_rows(left_content_w, mid_content_w, right_content_w);
        c.len().saturating_sub(tri_body_height)
    }

    fn prompt_panel_height(&self, height: usize) -> usize {
        let mut prompt_h = 10usize;
        while height.saturating_sub(3 + prompt_h + 1 + 2) < 6 && prompt_h > 6 {
            prompt_h -= 1;
        }
        prompt_h
    }

    fn render_briefing_panel(
        &self,
        out: &mut impl Write,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Result<(), AppError> {
        // Classic-mode system prompt editor: takes over the entire briefing slot.
        if let Some(edit) = &self.sysprompt_edit
            && self.layout_mode == LayoutMode::Classic
        {
            let name = if edit.active_agent_idx == 0 {
                "Aria"
            } else {
                "Basil"
            };
            let title = format!("{name} System Prompt");
            draw_box(
                out,
                x,
                y,
                width,
                height,
                Some(&title),
                Some(agent_color(edit.active_agent_idx)),
            )?;
            let body_y = y + 1;
            let body_h = height.saturating_sub(2);
            let body_x = x + 1;
            let body_w = width.saturating_sub(2);
            // Hint line at the very bottom of the body
            if body_h > 0 {
                let hint_y = body_y + body_h - 1;
                let hint = fit_with_ellipsis("Tab switch  ·  Ctrl-S save  ·  Esc cancel", body_w);
                queue!(
                    out,
                    MoveTo(body_x as u16, hint_y as u16),
                    SetForegroundColor(Color::Rgb {
                        r: 100,
                        g: 105,
                        b: 125
                    }),
                    Print(pad_to_width(&hint, body_w)),
                    SetAttribute(Attribute::Reset)
                )?;
            }
            // Editor occupies body rows above the hint
            let edit_h = body_h.saturating_sub(1);
            if edit_h > 0 {
                let buffer = &edit.buffers[edit.active_agent_idx];
                let cursor = edit.cursors[edit.active_agent_idx];
                render_prompt_editor(
                    out,
                    body_x,
                    body_y,
                    body_w,
                    edit_h,
                    buffer,
                    // render_prompt_editor takes a char-index cursor; convert from byte offset
                    buffer[..cursor].chars().count(),
                    None,
                )?;
            }
            return Ok(());
        }

        let prompt_border = if self.editing_prompt {
            Some(Color::Rgb {
                r: 200,
                g: 150,
                b: 50,
            })
        } else {
            None
        };
        draw_box_centered_title(out, x, y, width, height, "A N T I P H O N", prompt_border)?;

        let layout = mission_panel_layout(x, y, width, height);
        let MissionPanelLayout {
            agent_x,
            agent_w,
            prompt_x,
            prompt_w,
            telem_x,
            telem_w,
            body_y,
            body_h,
        } = layout;
        if agent_w > 0 {
            render_divider_at(out, agent_x + agent_w, body_y, body_h)?;
            render_agents_panel(out, self, agent_x, body_y, agent_w, body_h)?;
        }
        render_divider_at(out, telem_x - 1, body_y, body_h)?;
        render_knot_panel(out, self.anim_frame, telem_x, body_y, telem_w, body_h)?;

        let prompt_body_w = prompt_w;
        let prompt_body_h = body_h;
        if self.editing_prompt {
            render_prompt_editor(
                out,
                prompt_x,
                body_y,
                prompt_body_w,
                prompt_body_h,
                &self.edit_buffer,
                self.edit_cursor,
                self.prompt_selection_range(),
            )?;
        } else if prompt_body_h > 0 {
            let hint = fit_with_ellipsis("briefing [w]", prompt_body_w);
            queue!(
                out,
                MoveTo(prompt_x as u16, body_y as u16),
                SetForegroundColor(Color::Rgb {
                    r: 180,
                    g: 186,
                    b: 201,
                }),
                SetAttribute(Attribute::Bold),
                Print(pad_to_width(&hint, prompt_body_w)),
                SetAttribute(Attribute::Reset)
            )?;

            let prompt_lines = wrap_line(&self.prompt, prompt_body_w, false);
            for row in 1..prompt_body_h {
                let line = prompt_lines.get(row - 1).map(|s| s.as_str()).unwrap_or("");
                queue!(
                    out,
                    MoveTo(prompt_x as u16, (body_y + row) as u16),
                    SetForegroundColor(Color::DarkGrey),
                    Print(pad_to_width(line, prompt_body_w)),
                    SetAttribute(Attribute::Reset)
                )?;
            }
        }

        Ok(())
    }

    fn clamp_scroll(&mut self, total_rows: usize, body_height: usize) {
        let max_scroll = total_rows.saturating_sub(body_height);
        if self.auto_scroll {
            self.scroll = max_scroll;
        }
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    fn preset_mode_active(&self) -> bool {
        !matches!(self.preset_panel_state, PresetPanelState::Idle)
    }

    fn preset_selected_idx(&self) -> Option<usize> {
        if self.presets.is_empty() {
            return None;
        }
        let cursor = match self.preset_panel_state {
            PresetPanelState::Idle => self.active_preset_idx.unwrap_or(0),
            PresetPanelState::FocusedList { cursor } => cursor,
            PresetPanelState::Naming { list_cursor, .. } => list_cursor,
        };
        Some(cursor.min(self.presets.len().saturating_sub(1)))
    }

    fn preset_open_mode(&mut self) {
        let cursor = self
            .active_preset_idx
            .unwrap_or(0)
            .min(self.presets.len().saturating_sub(1));
        self.preset_panel_state = PresetPanelState::FocusedList { cursor };
    }

    fn preset_open_naming(&mut self) {
        let list_cursor = self.preset_selected_idx().unwrap_or(0);
        // Pre-fill with the active preset's name so "save same name = update".
        // Typing a different name always creates a new preset.
        let initial_name = self
            .active_preset_idx
            .and_then(|i| self.presets.get(i))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let cursor = initial_name.len();
        self.preset_panel_state = PresetPanelState::Naming {
            list_cursor,
            buffer: initial_name,
            cursor,
        };
    }

    fn preset_delete_selected(&mut self) {
        let Some(idx) = self.preset_selected_idx() else {
            return;
        };
        if idx >= self.presets.len() {
            return;
        }
        self.presets.remove(idx);
        self.active_preset_idx = match self.active_preset_idx {
            Some(_) if self.presets.is_empty() => None,
            Some(active_idx) if active_idx == idx => Some(idx.min(self.presets.len() - 1)),
            Some(active_idx) if active_idx > idx => Some(active_idx - 1),
            other => other,
        };
        self.preset_panel_state = PresetPanelState::FocusedList {
            cursor: idx.min(self.presets.len().saturating_sub(1)),
        };
    }

    fn preset_list_up(&mut self) {
        if let PresetPanelState::FocusedList { cursor } = &mut self.preset_panel_state {
            *cursor = cursor.saturating_sub(1);
        }
    }

    fn preset_list_down(&mut self) {
        if let PresetPanelState::FocusedList { cursor } = &mut self.preset_panel_state {
            let max = self.presets.len().saturating_sub(1);
            *cursor = (*cursor + 1).min(max);
        }
    }

    fn preset_load_selected(&mut self) {
        let Some(cursor) = self.preset_selected_idx() else {
            return;
        };
        if let Some(preset) = self.presets.get(cursor).cloned() {
            self.prompt = preset.prompt;
            self.agent_system_prompts[0] = preset.agent_a_system_prompt;
            self.agent_system_prompts[1] = preset.agent_b_system_prompt;
            self.active_preset_idx = Some(cursor);
        }
    }

    fn preset_naming_backspace(&mut self) {
        if let PresetPanelState::Naming { buffer, cursor, .. } = &mut self.preset_panel_state {
            if *cursor > 0 {
                let prev = buffer[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                buffer.drain(prev..*cursor);
                *cursor = prev;
            }
        }
    }

    fn preset_naming_left(&mut self) {
        if let PresetPanelState::Naming { buffer, cursor, .. } = &mut self.preset_panel_state {
            if *cursor > 0 {
                let prev = buffer[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                *cursor = prev;
            }
        }
    }

    fn preset_naming_right(&mut self) {
        if let PresetPanelState::Naming { buffer, cursor, .. } = &mut self.preset_panel_state {
            if *cursor < buffer.len() {
                let char_len = buffer[*cursor..].chars().next().map_or(0, |c| c.len_utf8());
                *cursor += char_len;
            }
        }
    }

    fn preset_naming_insert(&mut self, c: char) {
        if let PresetPanelState::Naming { buffer, cursor, .. } = &mut self.preset_panel_state {
            buffer.insert(*cursor, c);
            *cursor += c.len_utf8();
        }
    }

    fn preset_confirm_naming(&mut self) {
        let (list_cursor, name) = match &self.preset_panel_state {
            PresetPanelState::Naming {
                list_cursor,
                buffer,
                ..
            } => (*list_cursor, buffer.trim().to_string()),
            _ => return,
        };
        if !name.is_empty() {
            let new_preset = Preset {
                name: name.clone(),
                prompt: self.prompt.clone(),
                agent_a_system_prompt: self.agent_system_prompts[0].clone(),
                agent_b_system_prompt: self.agent_system_prompts[1].clone(),
            };
            let selected_idx =
                if let Some(existing_idx) = self.presets.iter().position(|p| p.name == name) {
                    self.presets[existing_idx] = new_preset;
                    existing_idx
                } else {
                    self.presets.push(new_preset);
                    self.presets.len() - 1
                };
            self.active_preset_idx = Some(selected_idx);
            self.preset_panel_state = PresetPanelState::FocusedList {
                cursor: selected_idx,
            };
        } else {
            self.preset_panel_state = PresetPanelState::FocusedList {
                cursor: list_cursor.min(self.presets.len().saturating_sub(1)),
            };
        }
    }
}

fn render_footer_state_line(
    out: &mut impl Write,
    y: usize,
    inner_width: usize,
    view: StateLineView<'_>,
) -> Result<(), AppError> {
    queue!(out, MoveTo(1, y as u16))?;
    let working = view.working.agent_working.iter().any(|v| *v);
    let divider_color = if view.paused {
        Color::Rgb {
            r: 96,
            g: 84,
            b: 62,
        }
    } else if working {
        if (view.spinner_frame / 2) % 2 == 0 {
            Color::Rgb {
                r: 64,
                g: 90,
                b: 94,
            }
        } else {
            Color::Rgb {
                r: 70,
                g: 96,
                b: 101,
            }
        }
    } else {
        Color::Rgb {
            r: 54,
            g: 58,
            b: 68,
        }
    };

    queue!(
        out,
        SetForegroundColor(divider_color),
        Print("─".repeat(inner_width)),
        SetAttribute(Attribute::Reset)
    )?;

    Ok(())
}

fn fit_with_ellipsis(input: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let input_len = input.chars().count();
    if input_len <= width {
        return input.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let mut clipped: String = input.chars().take(width - 1).collect();
    clipped.push('…');
    clipped
}

struct MissionPanelLayout {
    agent_x: usize,
    agent_w: usize,
    prompt_x: usize,
    prompt_w: usize,
    telem_x: usize,
    telem_w: usize,
    body_y: usize,
    body_h: usize,
}

fn mission_panel_layout(x: usize, y: usize, width: usize, height: usize) -> MissionPanelLayout {
    let content_w = width.saturating_sub(2);
    let body_y = y + 1;
    let body_h = height.saturating_sub(2);
    let telemetry_w = content_w.saturating_sub(content_w.saturating_mul(7) / 10 + 1);
    let telemetry_w = telemetry_w.clamp(16, 28).min(content_w.saturating_sub(10));
    // Agent rail: only visible when panel is wide enough (terminal ~>= 100 cols)
    let agent_w = if width >= 98 {
        26usize.min(content_w.saturating_sub(telemetry_w + 20))
    } else {
        0
    };
    let agent_x = x + 1;
    let prompt_x = agent_x + if agent_w > 0 { agent_w + 1 } else { 0 };
    let telem_x = x + 1 + content_w - telemetry_w;
    let prompt_w = telem_x.saturating_sub(prompt_x + 1);
    MissionPanelLayout {
        agent_x,
        agent_w,
        prompt_x,
        prompt_w,
        telem_x,
        telem_w: telemetry_w,
        body_y,
        body_h,
    }
}

fn render_divider_at(
    out: &mut impl Write,
    divider_x: usize,
    body_y: usize,
    body_h: usize,
) -> Result<(), AppError> {
    let divider_color = Color::Rgb {
        r: 72,
        g: 76,
        b: 92,
    };
    queue!(out, SetForegroundColor(divider_color))?;
    for row in 0..body_h {
        queue!(
            out,
            MoveTo(divider_x as u16, (body_y + row) as u16),
            Print("│")
        )?;
    }
    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn routing_mode_short(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::PromptOnlyToAgentA => "relay-a",
        RoutingMode::PromptToAAndB => "relay-ab",
    }
}

fn layout_mode_short(mode: LayoutMode) -> &'static str {
    match mode {
        LayoutMode::Classic => "classic",
        LayoutMode::TriPaneThinking => "tri-pane",
        LayoutMode::Orb => "orb",
    }
}

fn agent_model(cmd: &str) -> String {
    if cmd == "claude" {
        std::env::var("CLAUDE_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "\u{2014}".to_string())
    } else if cmd == "codex" || cmd == "codex-api" {
        let model = ["OPENAI_MODEL", "CODEX_API_MODEL"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "\u{2014}".to_string());
        if cmd == "codex-api" {
            format!("API: {}", model)
        } else {
            model
        }
    } else {
        "\u{2014}".to_string()
    }
}

fn render_knot_panel(
    out: &mut impl Write,
    anim_frame: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<(), AppError> {
    if width == 0 || height == 0 {
        return Ok(());
    }

    use std::f64::consts::TAU;

    // 108 frames = full rotation ~8.6s at 80ms/tick — meditative pace
    let theta = (anim_frame % 108) as f64 * TAU / 108.0;
    // 28° tilt around X so the three-lobe structure reads clearly
    let tilt = 28.0_f64.to_radians();

    let cx = width as f64 / 2.0;
    let cy = height as f64 / 2.0;
    let d = 5.0_f64;
    // Projected half-extent = (knot_radius / D) * scale; solve to fill panel
    let scale = (cx.min(cy * 2.0) * 0.78) * d / 3.0;

    let cells = width * height;
    let mut zbuf: Vec<f64> = vec![f64::NEG_INFINITY; cells];
    let mut depthbuf: Vec<f32> = vec![-1.0_f32; cells]; // -1 = empty
    let mut huebuf: Vec<f32> = vec![0.0_f32; cells]; // 0=purple, 1=amber

    const SAMPLES: usize = 900;

    for i in 0..SAMPLES {
        let t = i as f64 * TAU / SAMPLES as f64;

        let kx = t.sin() + 2.0 * (2.0 * t).sin();
        let ky = t.cos() - 2.0 * (2.0 * t).cos();
        let kz = -(3.0 * t).sin();

        let rx = kx * theta.cos() + kz * theta.sin();
        let rz = -kx * theta.sin() + kz * theta.cos();
        let ry = ky;

        let tx = rx;
        let ty = ry * tilt.cos() - rz * tilt.sin();
        let tz = ry * tilt.sin() + rz * tilt.cos();

        let denom = tz + d;
        if denom <= 0.1 {
            continue;
        }
        let px = tx / denom * scale + cx;
        let py = ty / denom * scale * 0.5 + cy;

        let col = px.round() as isize;
        let row = py.round() as isize;
        if col < 0 || col >= width as isize || row < 0 || row >= height as isize {
            continue;
        }
        let idx = row as usize * width + col as usize;

        if tz > zbuf[idx] {
            zbuf[idx] = tz;
            depthbuf[idx] = ((tz + 2.3) / 4.6).clamp(0.0, 1.0) as f32;
            // sin(t) oscillates once per revolution — the two colors naturally
            // trade places at each crossing as the strand loops through itself
            huebuf[idx] = ((t.sin() + 1.0) * 0.5) as f32; // 0=purple, 1=amber
        }
    }

    // Wire-feel depth chars: sparse dot at back → focal point at front
    const DEPTH_CHARS: &[char] = &['.', '·', ':', '+', '*', '◆'];

    // Purple (agent 0) ↔ amber (agent 1), luminance driven by depth
    let knot_color = |depth: f32, hue: f32| -> (u8, u8, u8) {
        let (pr, pg, pb) = (170.0_f32, 120.0_f32, 240.0_f32); // soft purple
        let (ar, ag, ab) = (240.0_f32, 165.0_f32, 40.0_f32); // warm amber
        let br = pr + (ar - pr) * hue;
        let bg = pg + (ag - pg) * hue;
        let bb = pb + (ab - pb) * hue;
        // Back recedes to near-black; front glows at full saturation
        let lum = 0.12 + depth * 0.88;
        (
            (br * lum).round().clamp(0.0, 255.0) as u8,
            (bg * lum).round().clamp(0.0, 255.0) as u8,
            (bb * lum).round().clamp(0.0, 255.0) as u8,
        )
    };

    for row in 0..height {
        queue!(out, MoveTo(x as u16, (y + row) as u16))?;
        for col in 0..width {
            let idx = row * width + col;
            let dn = depthbuf[idx];
            if dn < 0.0 {
                queue!(out, Print(' '))?;
            } else {
                let ci = (dn * (DEPTH_CHARS.len() - 1) as f32).round() as usize;
                let (r, g, b) = knot_color(dn, huebuf[idx]);
                queue!(
                    out,
                    SetForegroundColor(Color::Rgb { r, g, b }),
                    Print(DEPTH_CHARS[ci]),
                    SetAttribute(Attribute::Reset)
                )?;
            }
        }
    }
    Ok(())
}

fn render_orb_panel(
    out: &mut impl Write,
    anim_frame: usize,
    orb_pos: f32,
    active_agent: Option<usize>,
    is_working: bool,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<(), AppError> {
    if width == 0 || height == 0 {
        return Ok(());
    }

    let f = anim_frame as f64;
    let w = width as f64;
    let h = height as f64;
    let op = orb_pos as f64;
    let morph = smoothstep01(op);

    // Wider range: 10%–90% so the orb clearly hugs one side when active
    let orb_cx = w * (0.10 + op * 0.80);
    let orb_cy = (h - 1.0) / 2.0;

    // Aria leans crystalline, Basil leans organic; both share the same envelope while morphing.
    let phase = if is_working {
        match active_agent {
            Some(1) => f * 0.24,
            _ => f * 0.18,
        }
    } else {
        f * 0.06
    };

    let (base_r, base_g, base_b) = orb_base_color(op);
    let active_pulse = if is_working {
        0.16 + 0.10 * (phase * 0.9).sin().abs()
    } else {
        0.0
    };
    let (cr, cg, cb) = if let Some((ar, ag, ab)) = orb_active_accent(active_agent) {
        (
            mix_u8(base_r, ar, active_pulse) as f64,
            mix_u8(base_g, ag, active_pulse) as f64,
            mix_u8(base_b, ab, active_pulse) as f64,
        )
    } else {
        (base_r as f64, base_g as f64, base_b as f64)
    };

    // Sigma in visual units (dx in chars, dy*2 for aspect → uniform visual space)
    let sigma = orb_cy * 2.0;

    // ASCII chars: sparse outer → dense core
    const CHARS: &[u8] = b" . :+*@";
    const EDGE_FLOOR: f64 = 0.58;

    for row in 0..height {
        queue!(out, MoveTo(x as u16, (y + row) as u16))?;
        for col in 0..width {
            let dx_v = col as f64 - orb_cx;
            let dy_v = (row as f64 - orb_cy) * 2.0; // visual units

            // Shared orb silhouette.
            let d2_v = dx_v * dx_v + dy_v * dy_v;
            let radius = d2_v.sqrt();
            let envelope = (-d2_v / (2.0 * sigma * sigma)).exp();
            let theta = dy_v.atan2(dx_v);

            let aria_facets = (dx_v * 0.44 + phase).sin().abs().powf(1.7)
                * (dy_v * 0.72 - phase * 0.55).cos().abs().powf(1.3);
            let aria_ribbon_center = (dx_v * 0.32 + phase * 1.15).sin() * sigma * 0.20;
            let aria_ribbon =
                (-(dy_v - aria_ribbon_center).powi(2) / (2.0 * (sigma * 0.16).powi(2))).exp();
            let aria_energy = (aria_facets * 0.58 + aria_ribbon * 0.42) * envelope;

            let basil_spiral_center = sigma * (0.36 + 0.09 * (theta * 2.0 + phase * 0.35).sin());
            let basil_spiral =
                (-(radius - basil_spiral_center).powi(2) / (2.0 * (sigma * 0.13).powi(2))).exp();
            let basil_leaves = ((theta * 3.0 - phase * 0.42).cos().abs()).powf(2.2)
                * (-(radius - sigma * 0.52).powi(2) / (2.0 * (sigma * 0.20).powi(2))).exp();
            let basil_energy = (basil_spiral * 0.54 + basil_leaves * 0.46) * envelope;

            let shell_radius = sigma * (0.77 + 0.03 * (phase * 0.4).sin());
            let shell = (-(radius - shell_radius).powi(2) / (2.0 * (sigma * 0.12).powi(2))).exp();
            let ambient = (-d2_v / (2.0 * (sigma * 1.35) * (sigma * 1.35))).exp() * 0.17;
            let brightness =
                (aria_energy * (1.0 - morph) + basil_energy * morph + shell * 0.30 + ambient)
                    .clamp(0.0, 1.0);

            if brightness < 0.06 {
                queue!(out, Print(' '))?;
            } else {
                let ci =
                    ((brightness * (CHARS.len() - 1) as f64).round() as usize).min(CHARS.len() - 1);
                let edge_mix = if brightness < 0.40 {
                    1.0 - brightness / 0.40
                } else {
                    0.0
                };
                let color_strength = brightness * (1.0 - edge_mix) + EDGE_FLOOR * edge_mix;
                let glow = brightness * brightness * brightness * (0.38 + active_pulse * 0.30);
                let highlight = 248.0 + active_pulse * 7.0;
                let r = (cr * color_strength + highlight * glow).clamp(0.0, 255.0) as u8;
                let g = (cg * color_strength + highlight * glow).clamp(0.0, 255.0) as u8;
                let b = (cb * color_strength + highlight * glow).clamp(0.0, 255.0) as u8;
                queue!(
                    out,
                    SetForegroundColor(Color::Rgb { r, g, b }),
                    Print(CHARS[ci] as char),
                    SetAttribute(Attribute::Reset)
                )?;
            }
        }
    }
    Ok(())
}

fn render_agents_panel(
    out: &mut impl Write,
    state: &UiState,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<(), AppError> {
    let turns_display = if state.editing_turns {
        if state.turns_buffer.is_empty() {
            "_".to_string()
        } else {
            format!("{}_", state.turns_buffer)
        }
    } else {
        state.turns.to_string()
    };
    let mut rows = vec![
        format!("turns: {} [1-9/`]", turns_display),
        format!("mode: {} [x]", routing_mode_short(state.routing_mode)),
        format!("layout: {} [y]", layout_mode_short(state.layout_mode)),
        format!(
            "think: {} [n]",
            if state.thinking_expanded {
                "expand"
            } else {
                "wrap"
            }
        ),
        format!(
            "tmux: {} [b]",
            if state.show_tmux_panels { "on" } else { "off" }
        ),
    ];
    if state.has_staged_prompt() {
        rows.push("next: staged".to_string());
    }
    if !state.mouse_capture {
        rows.push("copy: enabled".to_string());
    }

    // Row 0: "SETTINGS" header
    if height > 0 {
        queue!(
            out,
            MoveTo(x as u16, y as u16),
            SetForegroundColor(Color::Rgb {
                r: 90,
                g: 95,
                b: 115
            }),
            Print(pad_to_width(&fit_with_ellipsis("SETTINGS", width), width)),
            SetAttribute(Attribute::Reset)
        )?;
    }

    // Rows 1+: data rows
    for row in 0..height.saturating_sub(1) {
        queue!(out, MoveTo(x as u16, (y + 1 + row) as u16))?;
        let line = rows.get(row).map_or("", String::as_str);
        let fg = if row == 0 && state.editing_turns {
            Color::Rgb {
                r: 180,
                g: 150,
                b: 255,
            }
        } else {
            Color::Rgb {
                r: 125,
                g: 130,
                b: 146,
            }
        };
        queue!(
            out,
            SetForegroundColor(fg),
            Print(pad_to_width(&fit_with_ellipsis(line, width), width)),
            SetAttribute(Attribute::Reset)
        )?;
    }
    Ok(())
}

fn render_top_side_panel_content(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    agent_idx: usize,
    state: &UiState,
) -> Result<(), AppError> {
    if state
        .agent_chooser
        .is_some_and(|chooser| chooser.agent_idx == agent_idx)
    {
        return render_agent_chooser_content(out, x, y, width, height, state, Some(agent_idx));
    }

    render_sysprompt_box_content(out, x, y, width, height, agent_idx, state)
}

fn render_agent_chooser_content(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    state: &UiState,
    fixed_agent_idx: Option<usize>,
) -> Result<(), AppError> {
    if width == 0 || height == 0 {
        return Ok(());
    }

    let Some(chooser) = state.agent_chooser else {
        return Ok(());
    };
    let agent_idx = fixed_agent_idx.unwrap_or(chooser.agent_idx);
    let is_target_panel = chooser.agent_idx == agent_idx;
    let dim = Color::Rgb {
        r: 100,
        g: 105,
        b: 125,
    };
    let text = Color::Rgb {
        r: 185,
        g: 190,
        b: 205,
    };

    if !is_target_panel {
        let summary = format!(
            "{} chooser open",
            conversation::agent_name(chooser.agent_idx).to_lowercase()
        );
        queue!(
            out,
            MoveTo(x as u16, y as u16),
            SetForegroundColor(dim),
            Print(pad_to_width(&fit_with_ellipsis(&summary, width), width)),
            SetAttribute(Attribute::Reset)
        )?;
        for row in 1..height {
            queue!(
                out,
                MoveTo(x as u16, (y + row) as u16),
                Print(" ".repeat(width))
            )?;
        }
        return Ok(());
    }

    let current_choice = agent_command_choice_index(&state.agent_cmds[agent_idx]);
    let hint_row = height.saturating_sub(1);
    for row in 0..hint_row.min(AGENT_COMMAND_CHOICES.len()) {
        let choice = AGENT_COMMAND_CHOICES[row];
        let cursor = if chooser.cursor == row { ">" } else { " " };
        let current = if current_choice == row { "*" } else { " " };
        let line = format!("{cursor}{current} {choice}");
        queue!(
            out,
            MoveTo(x as u16, (y + row) as u16),
            SetForegroundColor(if chooser.cursor == row {
                agent_color(agent_idx)
            } else {
                text
            }),
            SetAttribute(if chooser.cursor == row {
                Attribute::Bold
            } else {
                Attribute::Reset
            }),
            Print(pad_to_width(&fit_with_ellipsis(&line, width), width)),
            SetAttribute(Attribute::Reset)
        )?;
    }
    for row in AGENT_COMMAND_CHOICES.len()..hint_row {
        queue!(
            out,
            MoveTo(x as u16, (y + row) as u16),
            Print(" ".repeat(width))
        )?;
    }

    let hint = fit_with_ellipsis("↑↓ move  ·  Enter apply  ·  Esc cancel", width);
    queue!(
        out,
        MoveTo(x as u16, (y + hint_row) as u16),
        SetForegroundColor(dim),
        Print(pad_to_width(&hint, width)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn footer_badge_state(view: &FooterView) -> FooterBadgeState {
    if view.run_failed {
        FooterBadgeState::Error
    } else if view.completed {
        FooterBadgeState::Done
    } else if view.paused {
        FooterBadgeState::Paused
    } else if view.run_started {
        FooterBadgeState::Live
    } else {
        FooterBadgeState::Ready
    }
}

fn footer_badge_label(state: FooterBadgeState) -> &'static str {
    match state {
        FooterBadgeState::Ready => "READY",
        FooterBadgeState::Live => "LIVE",
        FooterBadgeState::Paused => "PAUSED",
        FooterBadgeState::Done => "DONE",
        FooterBadgeState::Error => "ERROR",
    }
}

fn footer_badge_colors(state: FooterBadgeState) -> (Color, Color) {
    match state {
        FooterBadgeState::Ready => (
            Color::Rgb {
                r: 62,
                g: 70,
                b: 84,
            },
            Color::White,
        ),
        FooterBadgeState::Live => (
            Color::Rgb {
                r: 78,
                g: 164,
                b: 170,
            },
            Color::Rgb {
                r: 14,
                g: 22,
                b: 24,
            },
        ),
        FooterBadgeState::Paused => (
            Color::Rgb {
                r: 212,
                g: 156,
                b: 76,
            },
            Color::Rgb {
                r: 28,
                g: 20,
                b: 10,
            },
        ),
        FooterBadgeState::Done => (
            Color::Rgb {
                r: 96,
                g: 170,
                b: 112,
            },
            Color::Rgb {
                r: 12,
                g: 22,
                b: 16,
            },
        ),
        FooterBadgeState::Error => (
            Color::Rgb {
                r: 204,
                g: 92,
                b: 92,
            },
            Color::Rgb {
                r: 28,
                g: 12,
                b: 12,
            },
        ),
    }
}

fn footer_primary_action(view: &FooterView) -> (&'static str, &'static str) {
    if view.editing_prompt || view.editing_turns {
        ("enter", "save")
    } else if view.paused {
        ("p", "resume")
    } else if view.run_started {
        ("p", "pause")
    } else if view.completed {
        ("r", "relaunch")
    } else {
        ("r", "launch")
    }
}

fn render_footer_lines(
    out: &mut impl Write,
    y_top: usize,
    inner_width: usize,
    view: FooterView,
) -> Result<(), AppError> {
    let sep = Color::Rgb {
        r: 45,
        g: 48,
        b: 60,
    };
    let label = Color::Rgb {
        r: 100,
        g: 105,
        b: 125,
    };
    let label_active = Color::Rgb {
        r: 184,
        g: 190,
        b: 205,
    };
    let accent = Color::Rgb {
        r: 78,
        g: 164,
        b: 170,
    };

    let badge_state = footer_badge_state(&view);
    let badge_text = format!(" {} ", footer_badge_label(badge_state));
    let badge_width = badge_text.chars().count();
    let (badge_bg, badge_fg) = footer_badge_colors(badge_state);
    let active_action = footer_primary_action(&view);

    // Row 1: runtime controls
    queue!(out, MoveTo(1, y_top as u16))?;
    let mut row1_visual = 0usize;
    if view.editing_prompt {
        row1_visual += draw_footer_action(
            out,
            "Ctrl-S",
            "save",
            label_active,
            accent,
            active_action == ("Ctrl-S", "save"),
        )?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "enter", "save", label_active, accent, false)?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "esc", "cancel", label, accent, false)?;
    } else if view.editing_turns {
        row1_visual += draw_footer_action(out, "0-9", "type", label, accent, false)?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(
            out,
            "Ctrl-S",
            "save",
            label_active,
            accent,
            active_action == ("Ctrl-S", "save"),
        )?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "enter", "save", label_active, accent, false)?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "esc", "cancel", label, accent, false)?;
    } else if let Some(agent_idx) = view.agent_chooser {
        row1_visual += draw_footer_action(out, "↑↓", "move", label_active, accent, false)?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "enter", "apply", label_active, accent, false)?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(
            out,
            "esc",
            if agent_idx == 0 {
                "cancel:aria"
            } else {
                "cancel:basil"
            },
            label,
            accent,
            false,
        )?;
    } else {
        let relaunch_label = if view.completed { "relaunch" } else { "launch" };
        row1_visual += draw_footer_action(
            out,
            "r",
            relaunch_label,
            if active_action == ("r", relaunch_label) {
                label_active
            } else {
                label
            },
            accent,
            active_action == ("r", relaunch_label),
        )?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(
            out,
            "p",
            if view.paused { "resume" } else { "pause" },
            if active_action == ("p", if view.paused { "resume" } else { "pause" }) {
                label_active
            } else {
                label
            },
            accent,
            active_action == ("p", if view.paused { "resume" } else { "pause" }),
        )?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "c", "clear", label, accent, false)?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(
            out,
            "↑↓",
            if view.auto_scroll {
                "scroll"
            } else {
                "scroll:manual"
            },
            label,
            accent,
            false,
        )?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(
            out,
            "v",
            if view.mouse_capture {
                "select:text"
            } else {
                "select:scroll"
            },
            if !view.mouse_capture {
                label_active
            } else {
                label
            },
            accent,
            false,
        )?;
        row1_visual += draw_footer_sep(out, sep)?;
        row1_visual += draw_footer_action(out, "Ctrl-Q", "quit", label, accent, false)?;
    }

    let row1_pad = inner_width.saturating_sub(row1_visual + badge_width);
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        Print(" ".repeat(row1_pad)),
        SetBackgroundColor(badge_bg),
        SetForegroundColor(badge_fg),
        SetAttribute(Attribute::Bold),
        Print(&badge_text),
        SetAttribute(Attribute::Reset)
    )?;

    Ok(())
}

fn draw_footer_action(
    out: &mut impl Write,
    key: &str,
    label_text: &str,
    label_color: Color,
    accent_color: Color,
    active: bool,
) -> Result<usize, AppError> {
    let key_text = format!("[{key}]");
    let label_full = format!(" {label_text}");
    if active {
        queue!(
            out,
            SetBackgroundColor(accent_color),
            SetForegroundColor(Color::Rgb {
                r: 12,
                g: 24,
                b: 26,
            }),
            SetAttribute(Attribute::Bold),
            Print(&key_text),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(label_color),
            Print(&label_full),
            SetAttribute(Attribute::Reset)
        )?;
    } else {
        queue!(
            out,
            SetForegroundColor(Color::White),
            SetAttribute(Attribute::Bold),
            Print(&key_text),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(label_color),
            Print(&label_full),
            SetAttribute(Attribute::Reset)
        )?;
    }
    Ok(key_text.chars().count() + label_full.chars().count())
}

fn draw_footer_sep(out: &mut impl Write, sep_color: Color) -> Result<usize, AppError> {
    let sep = "  ·  ";
    queue!(
        out,
        SetForegroundColor(sep_color),
        Print(sep),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(sep.chars().count())
}

/// Returns `(agent_idx, prefix_byte_len)` if `line` starts with `"N▎ "`.
/// The digit N encodes the agent index; byte_len is 5 (1 + 3 + 1 in UTF-8).
fn gutter_info(line: &str) -> Option<(usize, usize)> {
    let mut chars = line.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(d), Some('▎'), Some(' ')) if d.is_ascii_digit() => {
            let agent_idx = (d as u8 - b'0') as usize;
            let byte_len = d.len_utf8() + '▎'.len_utf8() + ' '.len_utf8();
            Some((agent_idx, byte_len))
        }
        _ => None,
    }
}

fn chat_body_padding(
    agent_idx: usize,
    body_width: usize,
    directional_chat: bool,
) -> (usize, usize) {
    if !directional_chat {
        return (0, 0);
    }

    // Keep at least one cell for content.
    let max_side_pad = body_width.saturating_sub(1);
    let side_pad = body_width
        .saturating_div(TRI_CHAT_SIDE_PAD_DIVISOR)
        .clamp(TRI_CHAT_SIDE_PAD_MIN, TRI_CHAT_SIDE_PAD_MAX)
        .min(max_side_pad);
    match agent_idx {
        0 => (0, side_pad),
        _ => (side_pad, 0),
    }
}

fn render_conv_line(
    out: &mut impl Write,
    line: &str,
    available_width: usize,
    directional_chat: bool,
) -> Result<(), AppError> {
    queue!(out, SetAttribute(Attribute::Reset))?;

    // Agent header lines: static table avoids per-line format! allocations.
    for &(header, idx) in AGENT_HEADERS {
        if line == header {
            if !directional_chat {
                queue!(
                    out,
                    SetForegroundColor(agent_color(idx)),
                    SetAttribute(Attribute::Bold),
                    Print(pad_to_width(line, available_width)),
                    SetAttribute(Attribute::Reset)
                )?;
                return Ok(());
            }

            let color = agent_color(idx);
            let body_width = available_width.saturating_sub(3);
            let (left_pad, right_pad) = chat_body_padding(idx, body_width, directional_chat);
            let content_width = body_width.saturating_sub(left_pad + right_pad);

            if idx == 1 {
                queue!(out, Print(" ".repeat(left_pad)))?;
            }

            queue!(
                out,
                Print(" "),
                SetForegroundColor(color),
                Print("▎"),
                Print(" "),
                SetAttribute(Attribute::Bold),
                Print(pad_to_width(line, content_width)),
                SetAttribute(Attribute::Reset),
                Print(" ".repeat(right_pad))
            )?;
            return Ok(());
        }
    }

    // Gutter content lines: "N▎ text" — render colored bar, body in default
    if let Some((agent_idx, byte_len)) = gutter_info(line) {
        let rest = &line[byte_len..];
        let color = agent_color(agent_idx);
        let body_width = available_width.saturating_sub(3);
        let (left_pad, right_pad) = chat_body_padding(agent_idx, body_width, directional_chat);
        let content_width = body_width.saturating_sub(left_pad + right_pad);
        if directional_chat && agent_idx == 1 {
            // In directional mode, Basil's gutter should travel with the right-shifted text block.
            queue!(out, Print(" ".repeat(left_pad)))?;
            queue!(
                out,
                Print(" "),
                SetForegroundColor(color),
                Print("▎"),
                SetAttribute(Attribute::Reset),
                Print(" ")
            )?;
            queue!(
                out,
                Print(pad_to_width(rest, content_width)),
                Print(" ".repeat(right_pad))
            )?;
        } else {
            // " ▎ " — space replaces the invisible digit, bar in agent color.
            queue!(
                out,
                Print(" "),
                SetForegroundColor(color),
                Print("▎"),
                SetAttribute(Attribute::Reset),
                Print(" ")
            )?;
            queue!(
                out,
                Print(" ".repeat(left_pad)),
                Print(pad_to_width(rest, content_width)),
                Print(" ".repeat(right_pad))
            )?;
        }
        return Ok(());
    }

    if line.starts_with("  ✿") {
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 110,
                g: 200,
                b: 120
            }),
            Print(pad_to_width(line, available_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    if line.starts_with("  ✗") {
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 220,
                g: 80,
                b: 80
            }),
            Print(pad_to_width(line, available_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    queue!(out, Print(pad_to_width(line, available_width)))?;
    Ok(())
}

fn thinking_line_text_width(kind: ThinkingLineKind, available_width: usize) -> usize {
    match kind {
        ThinkingLineKind::ToolUse | ThinkingLineKind::ToolResult | ThinkingLineKind::ToolError => {
            available_width.saturating_sub(2)
        }
        ThinkingLineKind::ToolContinuation => available_width.saturating_sub(2),
        ThinkingLineKind::Reasoning => available_width.saturating_sub(2),
        _ => available_width,
    }
}

fn format_tool_event_text(tool_type: &str, text: &str) -> String {
    let normalized_type = tool_type.trim();
    if normalized_type.is_empty() {
        return text.to_string();
    }
    format!("[{normalized_type}] {text}")
}

fn append_main_token_with_boundaries(turn: &mut TurnRecord, token: &str) {
    turn.main_text.push_str(token);

    fn current_chunk_mut(turn: &mut TurnRecord) -> &mut String {
        if turn.main_chunks.is_empty() {
            turn.main_chunks.push(String::new());
        }
        turn.main_chunks.last_mut().expect("main chunk initialized")
    }

    let mut in_leading_prefix = true;
    for ch in token.chars() {
        if in_leading_prefix {
            if ch == '\n' {
                turn.pending_leading_newlines += 1;
                continue;
            }
            // Consume deferred leading newlines only when we see the next non-newline.
            // If the next visible char is non-whitespace, pairs of newlines are treated as
            // message boundaries; any remainder is preserved in text.
            let mut preserved_newlines = turn.pending_leading_newlines;
            if !ch.is_whitespace() && turn.saw_main_message {
                let boundaries = turn.pending_leading_newlines / 2;
                for _ in 0..boundaries {
                    turn.timeline.push(ThinkingTimelineRecord::MessageBoundary);
                    turn.main_chunks.push(String::new());
                }
                preserved_newlines = turn.pending_leading_newlines % 2;
            }
            for _ in 0..preserved_newlines {
                current_chunk_mut(turn).push('\n');
            }
            turn.pending_leading_newlines = 0;
            in_leading_prefix = false;
        }
        if !turn.saw_main_message && !ch.is_whitespace() {
            turn.saw_main_message = true;
            if turn.main_chunks.is_empty() {
                turn.main_chunks.push(String::new());
            }
        }
        current_chunk_mut(turn).push(ch);
    }
}

fn append_reasoning_timeline_record(turn: &mut TurnRecord, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(ThinkingTimelineRecord::Reasoning(existing)) = turn.timeline.last_mut() {
        existing.push_str(text);
        return;
    }
    turn.timeline
        .push(ThinkingTimelineRecord::Reasoning(text.to_string()));
}

fn append_reasoning_display_lines(
    rows: &mut Vec<ThinkingDisplayLine>,
    timeline: &[ThinkingTimelineRecord],
    think_width: usize,
) {
    for entry in timeline {
        if let ThinkingTimelineRecord::Reasoning(text) = entry {
            for line in text.split('\n').filter(|segment| !segment.is_empty()) {
                for wrapped in wrap_line(line, think_width, false) {
                    rows.push(ThinkingDisplayLine {
                        kind: ThinkingLineKind::Reasoning,
                        text: wrapped,
                    });
                }
            }
        }
    }
}

fn timeline_segments(timeline: &[ThinkingTimelineRecord]) -> Vec<&[ThinkingTimelineRecord]> {
    let mut segments = Vec::new();
    let mut segment_start = 0;
    for (idx, entry) in timeline.iter().enumerate() {
        if matches!(entry, ThinkingTimelineRecord::MessageBoundary) {
            segments.push(&timeline[segment_start..idx]);
            segment_start = idx + 1;
        }
    }
    segments.push(&timeline[segment_start..]);
    segments
}

fn append_timeline_segment_display_lines(
    rows: &mut Vec<ThinkingDisplayLine>,
    segment: &[ThinkingTimelineRecord],
    think_width: usize,
) {
    for entry in segment {
        match entry {
            ThinkingTimelineRecord::Reasoning(_) => {
                append_reasoning_display_lines(rows, std::slice::from_ref(entry), think_width)
            }
            ThinkingTimelineRecord::Tool(tool_event) => {
                let event_text = format_tool_event_text(&tool_event.tool_type, &tool_event.text);
                rows.extend(wrap_tool_line(tool_event.kind, &event_text, think_width));
            }
            ThinkingTimelineRecord::MessageBoundary => {}
        }
    }
}

fn truncate_thinking_rows(rows: &mut Vec<ThinkingDisplayLine>, target: usize, think_width: usize) {
    if rows.len() <= target {
        return;
    }

    rows.truncate(target);
    if let Some(last) = rows.last_mut() {
        if last.kind == ThinkingLineKind::Blank {
            last.kind = ThinkingLineKind::Reasoning;
            last.text = "…".to_string();
        } else if !last.text.ends_with("…") {
            let line_width = thinking_line_text_width(last.kind, think_width);
            let mut clipped = fit_to_width(&last.text, line_width.saturating_sub(1));
            clipped.push('…');
            last.text = clipped;
        }
    }
}

fn flush_pending_main_chunk_newlines(turn: &mut TurnRecord) {
    if turn.pending_leading_newlines == 0 {
        return;
    }
    if turn.main_chunks.is_empty() {
        turn.main_chunks.push(String::new());
    }
    if let Some(chunk) = turn.main_chunks.last_mut() {
        for _ in 0..turn.pending_leading_newlines {
            chunk.push('\n');
        }
    }
    turn.pending_leading_newlines = 0;
}

fn wrap_tool_line(
    kind: ToolStreamEventKind,
    text: &str,
    available_width: usize,
) -> Vec<ThinkingDisplayLine> {
    let wrap_width = available_width.saturating_sub(2);
    let wrapped = wrap_line(text, wrap_width, false);
    if wrapped.is_empty() {
        return vec![ThinkingDisplayLine {
            kind: ThinkingLineKind::Blank,
            text: String::new(),
        }];
    }

    let first_kind = match kind {
        ToolStreamEventKind::Use => ThinkingLineKind::ToolUse,
        ToolStreamEventKind::Result => ThinkingLineKind::ToolResult,
        ToolStreamEventKind::Error => ThinkingLineKind::ToolError,
    };

    let mut out = Vec::with_capacity(wrapped.len());
    for (i, chunk) in wrapped.into_iter().enumerate() {
        out.push(ThinkingDisplayLine {
            kind: if i == 0 {
                first_kind
            } else {
                ThinkingLineKind::ToolContinuation
            },
            text: chunk,
        });
    }
    out
}

fn render_thinking_line(
    out: &mut impl Write,
    line: &ThinkingDisplayLine,
    available_width: usize,
    agent_idx: usize,
    spinner_frame: usize,
    animate_reasoning: bool,
) -> Result<(), AppError> {
    if line.kind == ThinkingLineKind::Blank {
        queue!(out, Print(" ".repeat(available_width)))?;
        return Ok(());
    }

    if line.kind == ThinkingLineKind::ToolHeader || line.kind == ThinkingLineKind::ToolFooter {
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 95,
                g: 100,
                b: 118
            }),
            Print(pad_to_width(&line.text, available_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    if line.kind == ThinkingLineKind::ToolUse {
        let prefix = "▶ ";
        let body_width = available_width.saturating_sub(prefix.chars().count());
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 230,
                g: 190,
                b: 90
            }),
            Print(prefix),
            SetForegroundColor(Color::Rgb {
                r: 220,
                g: 225,
                b: 235
            }),
            Print(pad_to_width(&line.text, body_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    if line.kind == ThinkingLineKind::ToolResult {
        let prefix = "✓ ";
        let body_width = available_width.saturating_sub(prefix.chars().count());
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 110,
                g: 205,
                b: 125
            }),
            Print(prefix),
            SetForegroundColor(Color::Rgb {
                r: 220,
                g: 225,
                b: 235
            }),
            Print(pad_to_width(&line.text, body_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    if line.kind == ThinkingLineKind::ToolError {
        let prefix = "✗ ";
        let body_width = available_width.saturating_sub(prefix.chars().count());
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 220,
                g: 90,
                b: 90
            }),
            Print(prefix),
            SetForegroundColor(Color::Rgb {
                r: 220,
                g: 225,
                b: 235
            }),
            Print(pad_to_width(&line.text, body_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    if line.kind == ThinkingLineKind::ToolContinuation {
        let prefix = "  ";
        let body_width = available_width.saturating_sub(prefix.chars().count());
        queue!(
            out,
            SetForegroundColor(Color::Rgb {
                r: 170,
                g: 176,
                b: 190
            }),
            Print(prefix),
            Print(pad_to_width(&line.text, body_width)),
            SetAttribute(Attribute::Reset)
        )?;
        return Ok(());
    }

    let reasoning_glyph = reasoning_prefix_glyph(agent_idx, spinner_frame, animate_reasoning);
    let prefix = format!("{reasoning_glyph} ");
    let body_width = available_width.saturating_sub(prefix.chars().count());
    let (prefix_color, text_color) = if animate_reasoning {
        if agent_idx == 1 {
            let prefix_frame = spinner_frame % 3;
            let prefix_color = match prefix_frame {
                0 => Color::Rgb {
                    r: 120,
                    g: 205,
                    b: 145,
                },
                1 => Color::Rgb {
                    r: 105,
                    g: 190,
                    b: 132,
                },
                _ => Color::Rgb {
                    r: 92,
                    g: 175,
                    b: 122,
                },
            };
            let text_color = Color::Rgb {
                r: 173,
                g: 228,
                b: 189,
            };
            (prefix_color, text_color)
        } else {
            let prefix_frame = spinner_frame % 3;
            let prefix_color = match prefix_frame {
                0 => Color::Rgb {
                    r: 178,
                    g: 145,
                    b: 240,
                },
                1 => Color::Rgb {
                    r: 190,
                    g: 156,
                    b: 246,
                },
                _ => Color::Rgb {
                    r: 165,
                    g: 132,
                    b: 232,
                },
            };
            let text_color = Color::Rgb {
                r: 218,
                g: 204,
                b: 248,
            };
            (prefix_color, text_color)
        }
    } else {
        (
            Color::Rgb {
                r: 80,
                g: 85,
                b: 100,
            },
            agent_color(agent_idx),
        )
    };
    queue!(
        out,
        SetForegroundColor(prefix_color),
        Print(prefix),
        SetForegroundColor(text_color),
        Print(pad_to_width(&line.text, body_width)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn reasoning_prefix_glyph(agent_idx: usize, spinner_frame: usize, animate_reasoning: bool) -> char {
    if !animate_reasoning {
        return '·';
    }
    if agent_idx == 1 {
        BASIL_REASONING_GLYPH_FRAMES[spinner_frame % BASIL_REASONING_GLYPH_FRAMES.len()]
    } else {
        ARIA_REASONING_GLYPH_FRAMES[spinner_frame % ARIA_REASONING_GLYPH_FRAMES.len()]
    }
}

fn render_sysprompt_box_content(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize, // usable text rows inside the box (sysprompt_h - 2 border rows)
    agent_idx: usize,
    state: &UiState,
) -> Result<(), AppError> {
    if height == 0 || width == 0 {
        return Ok(());
    }

    // Last row: agent command · model [hotkey]
    let cmd = state
        .agent_cmds
        .get(agent_idx)
        .map(|s| s.as_str())
        .unwrap_or("\u{2014}");
    let model = agent_model(cmd);
    let hotkey = if agent_idx == 0 { "[a]" } else { "[d]" };
    let agent_info = format!("{} {}", model, hotkey);
    let info_y = y + height - 1;
    queue!(
        out,
        MoveTo(x as u16, info_y as u16),
        SetForegroundColor(agent_color_pale(agent_idx)),
        Print(pad_to_width(&fit_with_ellipsis(&agent_info, width), width)),
        SetAttribute(Attribute::Reset)
    )?;

    let content_y = y;
    let content_h = height.saturating_sub(1);
    if content_h == 0 {
        return Ok(());
    }

    let is_editing = state
        .sysprompt_edit
        .as_ref()
        .is_some_and(|e| e.active_agent_idx == agent_idx);

    if is_editing {
        let edit = state.sysprompt_edit.as_ref().unwrap();
        // Bottom row is the hint line; rows above are the editor
        let hint_row = content_h.saturating_sub(1);
        let edit_h = hint_row;

        // Hint line
        let hint = fit_with_ellipsis("Tab switch  ·  Ctrl-S save  ·  Esc cancel", width);
        queue!(
            out,
            MoveTo(x as u16, (content_y + hint_row) as u16),
            SetForegroundColor(Color::Rgb {
                r: 100,
                g: 105,
                b: 125
            }),
            Print(pad_to_width(&hint, width)),
            SetAttribute(Attribute::Reset)
        )?;

        // Editor (char-index cursor converted from byte offset)
        if edit_h > 0 {
            let buffer = &edit.buffers[agent_idx];
            let cursor = edit.cursors[agent_idx];
            render_prompt_editor(
                out,
                x,
                content_y,
                width,
                edit_h,
                buffer,
                buffer[..cursor].chars().count(),
                None,
            )?;
        }
    } else {
        let text = state
            .sysprompt_edit
            .as_ref()
            .map(|edit| edit.buffers[agent_idx].as_str())
            .unwrap_or(&state.agent_system_prompts[agent_idx]);
        if text.is_empty() {
            // Dim placeholder
            let placeholder = fit_with_ellipsis("(no system prompt)", width);
            queue!(
                out,
                MoveTo(x as u16, content_y as u16),
                SetForegroundColor(Color::Rgb {
                    r: 70,
                    g: 74,
                    b: 88
                }),
                Print(pad_to_width(&placeholder, width)),
                SetAttribute(Attribute::Reset)
            )?;
            for row in 1..content_h {
                queue!(
                    out,
                    MoveTo(x as u16, (content_y + row) as u16),
                    Print(" ".repeat(width))
                )?;
            }
        } else {
            let wrapped = wrap_multiline_text(text, width, false);
            for row in 0..content_h {
                queue!(out, MoveTo(x as u16, (content_y + row) as u16))?;
                if let Some(line) = wrapped.get(row) {
                    queue!(
                        out,
                        SetForegroundColor(Color::Rgb {
                            r: 185,
                            g: 190,
                            b: 205
                        }),
                        Print(pad_to_width(line, width)),
                        SetAttribute(Attribute::Reset)
                    )?;
                } else {
                    queue!(out, Print(" ".repeat(width)))?;
                }
            }
        }
    }
    Ok(())
}

fn render_preset_panel_content(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    state: &UiState,
) -> Result<(), AppError> {
    if height == 0 || width == 0 {
        return Ok(());
    }
    let dim = Color::Rgb {
        r: 70,
        g: 75,
        b: 90,
    };
    let text = Color::Rgb {
        r: 180,
        g: 186,
        b: 201,
    };
    let accent = Color::Rgb {
        r: 130,
        g: 160,
        b: 220,
    };

    match &state.preset_panel_state {
        PresetPanelState::Naming { .. } => {
            if height > 0 {
                let label = fit_with_ellipsis("same name → update  ·  new name → create", width);
                queue!(
                    out,
                    MoveTo(x as u16, y as u16),
                    SetForegroundColor(accent),
                    Print(pad_to_width(&label, width)),
                    SetAttribute(Attribute::Reset),
                )?;
            }
            if height > 1 {
                let hint = fit_with_ellipsis("Enter / Ctrl-S confirm  ·  Esc cancel", width);
                queue!(
                    out,
                    MoveTo(x as u16, (y + 1) as u16),
                    SetForegroundColor(dim),
                    Print(pad_to_width(&hint, width)),
                    SetAttribute(Attribute::Reset),
                )?;
            }
            for row in 2..height {
                queue!(
                    out,
                    MoveTo(x as u16, (y + row) as u16),
                    Print(" ".repeat(width)),
                )?;
            }
        }
        _ => {
            if height > 0 {
                let name = state
                    .preset_selected_idx()
                    .and_then(|i| state.presets.get(i))
                    .map(|p| p.name.as_str())
                    .unwrap_or("— none —");
                let prefix = "▸ ";
                let avail = width.saturating_sub(prefix.chars().count());
                let name_fitted = fit_with_ellipsis(name, avail);
                queue!(
                    out,
                    MoveTo(x as u16, y as u16),
                    SetForegroundColor(dim),
                    Print(prefix),
                    SetForegroundColor(text),
                    Print(pad_to_width(&name_fitted, avail)),
                    SetAttribute(Attribute::Reset),
                )?;
            }
            for row in 1..height {
                queue!(
                    out,
                    MoveTo(x as u16, (y + row) as u16),
                    Print(" ".repeat(width)),
                )?;
            }
        }
    }
    Ok(())
}

fn render_preset_mode_overlay(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    state: &UiState,
) -> Result<(), AppError> {
    if width < 12 || height < 6 {
        return Ok(());
    }

    let max_overlay_w = width.saturating_sub(2);
    let overlay_w = width.saturating_sub(4).min(84).max(24.min(max_overlay_w));
    let overlay_x = x + (width.saturating_sub(overlay_w)) / 2;
    let overlay_max_h = height.saturating_sub(2).max(4);
    let visible = overlay_max_h
        .saturating_sub(4)
        .min(state.presets.len().max(1));
    let overlay_h = (visible + 4).min(overlay_max_h);
    let overlay_y = y + (height.saturating_sub(overlay_h)) / 2;
    let cursor = state.preset_selected_idx().unwrap_or(0);
    let scroll = if cursor >= visible {
        cursor + 1 - visible
    } else {
        0
    };

    draw_box(
        out,
        overlay_x,
        overlay_y,
        overlay_w,
        overlay_h,
        Some("Preset Mode"),
        None,
    )?;

    let inner_x = overlay_x + 1;
    let inner_y = overlay_y + 1;
    let inner_w = overlay_w.saturating_sub(2);
    let hint = fit_with_ellipsis(
        "j/k or ↑/↓ move  ·  Enter load  ·  Ctrl-S save  ·  Ctrl-D delete  ·  Esc close",
        inner_w,
    );
    queue!(
        out,
        MoveTo(inner_x as u16, inner_y as u16),
        SetForegroundColor(Color::Rgb {
            r: 130,
            g: 160,
            b: 220
        }),
        Print(pad_to_width(&hint, inner_w)),
        SetAttribute(Attribute::Reset),
    )?;

    match &state.preset_panel_state {
        PresetPanelState::Naming { buffer, cursor, .. } => {
            let label = fit_with_ellipsis("Save preset as:", inner_w);
            queue!(
                out,
                MoveTo(inner_x as u16, (inner_y + 1) as u16),
                Print(pad_to_width(&label, inner_w)),
            )?;
            if overlay_h > 3 {
                let char_cursor = buffer[..*cursor].chars().count();
                render_prompt_editor(
                    out,
                    inner_x,
                    inner_y + 2,
                    inner_w,
                    1,
                    buffer,
                    char_cursor,
                    None,
                )?;
            }
        }
        PresetPanelState::FocusedList { .. } | PresetPanelState::Idle => {
            let prefix_w = 2usize;
            let avail = inner_w.saturating_sub(prefix_w);
            if state.presets.is_empty() {
                queue!(
                    out,
                    MoveTo(inner_x as u16, (inner_y + 1) as u16),
                    SetForegroundColor(Color::DarkGrey),
                    Print(pad_to_width(
                        "No presets yet. Press Ctrl-S to save the current session.",
                        inner_w,
                    )),
                    SetAttribute(Attribute::Reset),
                )?;
            } else {
                for row in 0..visible {
                    let pidx = scroll + row;
                    let Some(preset) = state.presets.get(pidx) else {
                        break;
                    };
                    let item_y = inner_y + 1 + row;
                    let is_sel = pidx == cursor;
                    let prefix = if is_sel { "▸ " } else { "  " };
                    let name = fit_with_ellipsis(&preset.name, avail);

                    if is_sel {
                        queue!(
                            out,
                            MoveTo(inner_x as u16, item_y as u16),
                            SetBackgroundColor(Color::Rgb {
                                r: 40,
                                g: 50,
                                b: 70
                            }),
                            SetForegroundColor(Color::Rgb {
                                r: 200,
                                g: 210,
                                b: 240
                            }),
                            SetAttribute(Attribute::Bold),
                            Print(prefix),
                            Print(pad_to_width(&name, avail)),
                            SetAttribute(Attribute::Reset),
                        )?;
                    } else {
                        queue!(
                            out,
                            MoveTo(inner_x as u16, item_y as u16),
                            SetForegroundColor(Color::Rgb {
                                r: 140,
                                g: 146,
                                b: 161
                            }),
                            Print(prefix),
                            Print(pad_to_width(&name, avail)),
                            SetAttribute(Attribute::Reset),
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn draw_box(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    title: Option<&str>,
    border_color: Option<Color>,
) -> Result<(), AppError> {
    if width < 2 || height < 2 {
        return Ok(());
    }

    let bc = border_color.unwrap_or(Color::Rgb {
        r: 110,
        g: 115,
        b: 135,
    });
    // Title text slightly brighter/whiter than the border line
    let tc = match border_color {
        Some(c) => c,
        None => Color::Rgb {
            r: 160,
            g: 165,
            b: 185,
        },
    };

    queue!(out, SetForegroundColor(bc))?;
    queue!(out, MoveTo(x as u16, y as u16), Print("╭"))?;
    queue!(out, MoveTo((x + width - 1) as u16, y as u16), Print("╮"))?;
    queue!(out, MoveTo(x as u16, (y + height - 1) as u16), Print("╰"))?;
    queue!(
        out,
        MoveTo((x + width - 1) as u16, (y + height - 1) as u16),
        Print("╯")
    )?;

    queue!(out, MoveTo((x + 1) as u16, y as u16))?;
    if let Some(t) = title {
        let (t_capped, right_dashes) = box_title_layout(t, width);
        queue!(out, Print("─ "))?;
        queue!(out, SetForegroundColor(tc), Print(&t_capped))?;
        queue!(out, SetForegroundColor(bc), Print(" "))?;
        if right_dashes > 0 {
            queue!(out, Print("─".repeat(right_dashes)))?;
        }
    } else {
        queue!(out, Print("─".repeat(width - 2)))?;
    }

    queue!(
        out,
        MoveTo((x + 1) as u16, (y + height - 1) as u16),
        Print("─".repeat(width - 2))
    )?;

    for row in (y + 1)..(y + height - 1) {
        queue!(out, MoveTo(x as u16, row as u16), Print("│"))?;
        queue!(out, MoveTo((x + width - 1) as u16, row as u16), Print("│"))?;
    }

    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn draw_box_centered_title(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    title: &str,
    border_color: Option<Color>,
) -> Result<(), AppError> {
    if width < 2 || height < 2 {
        return Ok(());
    }

    let bc = border_color.unwrap_or(Color::Rgb {
        r: 110,
        g: 115,
        b: 135,
    });
    let tc = match border_color {
        Some(c) => c,
        None => Color::Rgb {
            r: 160,
            g: 165,
            b: 185,
        },
    };

    let inner_w = width.saturating_sub(2);
    let t = fit_to_display_width(title, inner_w.saturating_sub(2));
    let t_len = display_width(&t);
    let total_dashes = inner_w.saturating_sub(t_len + 2);
    let left_dashes = total_dashes / 2;
    let right_dashes = total_dashes.saturating_sub(left_dashes);

    // Corners
    queue!(out, SetForegroundColor(bc))?;
    queue!(out, MoveTo(x as u16, y as u16), Print("╭"))?;
    queue!(out, MoveTo((x + width - 1) as u16, y as u16), Print("╮"))?;
    queue!(out, MoveTo(x as u16, (y + height - 1) as u16), Print("╰"))?;
    queue!(
        out,
        MoveTo((x + width - 1) as u16, (y + height - 1) as u16),
        Print("╯")
    )?;

    // Top line: centered title in a single write — no overwrite
    queue!(
        out,
        MoveTo((x + 1) as u16, y as u16),
        SetForegroundColor(bc),
        Print("─".repeat(left_dashes)),
        Print(" "),
        SetForegroundColor(tc),
        Print(&t),
        SetForegroundColor(bc),
        Print(" "),
        Print("─".repeat(right_dashes)),
    )?;

    // Bottom line
    queue!(
        out,
        MoveTo((x + 1) as u16, (y + height - 1) as u16),
        Print("─".repeat(width - 2))
    )?;

    // Vertical sides
    for row in (y + 1)..(y + height - 1) {
        queue!(out, MoveTo(x as u16, row as u16), Print("│"))?;
        queue!(out, MoveTo((x + width - 1) as u16, row as u16), Print("│"))?;
    }

    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct PromptEditorLine {
    start_char: usize,
    chars: Vec<char>,
}

fn wrap_prompt_editor_lines(text: &str, width: usize) -> Vec<PromptEditorLine> {
    if width == 0 {
        return vec![PromptEditorLine {
            start_char: 0,
            chars: Vec::new(),
        }];
    }

    let mut lines = vec![PromptEditorLine {
        start_char: 0,
        chars: Vec::new(),
    }];

    for (idx, ch) in text.chars().enumerate() {
        if ch == '\n' {
            lines.push(PromptEditorLine {
                start_char: idx + 1,
                chars: Vec::new(),
            });
            continue;
        }

        let current = lines.last_mut().expect("line exists");
        current.chars.push(ch);
        if current.chars.len() >= width {
            lines.push(PromptEditorLine {
                start_char: idx + 1,
                chars: Vec::new(),
            });
        }
    }

    lines
}

fn render_prompt_editor(
    out: &mut impl Write,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    text: &str,
    cursor: usize,
    selection: Option<(usize, usize)>,
) -> Result<(), AppError> {
    let lines = wrap_prompt_editor_lines(text, width);
    let cursor = cursor.min(text.chars().count());

    for row in 0..height {
        let line = lines.get(row);
        let line_start = line.map(|l| l.start_char).unwrap_or(usize::MAX);
        let line_chars = line.map(|l| l.chars.as_slice()).unwrap_or(&[]);
        let line_end = line_start.saturating_add(line_chars.len());
        let cursor_col = if line_start <= cursor && cursor <= line_end {
            Some(cursor - line_start)
        } else {
            None
        };

        for col in 0..width {
            let idx = line_start.saturating_add(col);
            let glyph = line_chars.get(col).copied().unwrap_or(' ');
            let selected = selection
                .is_some_and(|(start, end)| col < line_chars.len() && idx >= start && idx < end);
            let cursor_here = cursor_col.is_some_and(|c| c == col);

            queue!(
                out,
                MoveTo((x + col) as u16, (y + row) as u16),
                SetAttribute(Attribute::Reset),
                SetForegroundColor(Color::White)
            )?;
            if selected {
                queue!(
                    out,
                    SetBackgroundColor(Color::Rgb {
                        r: 70,
                        g: 90,
                        b: 130
                    })
                )?;
            } else {
                queue!(out, SetBackgroundColor(Color::Reset))?;
            }
            if cursor_here {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            queue!(out, Print(glyph))?;
        }
    }

    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn wrap_lines(lines: &[String], width: usize, directional_chat: bool) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in lines {
        wrapped.extend(wrap_line(line, width, directional_chat));
    }
    wrapped
}

fn wrap_multiline_text(text: &str, width: usize, directional_chat: bool) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in text.split('\n') {
        wrapped.extend(wrap_line(line, width, directional_chat));
    }
    wrapped
}

fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

fn fit_to_display_width(input: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in input.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_w > width {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out
}

fn box_title_layout(title: &str, width: usize) -> (String, usize) {
    let capped = fit_to_display_width(title, width.saturating_sub(6));
    let right_dashes = width.saturating_sub(5 + display_width(&capped));
    (capped, right_dashes)
}

fn fit_to_width(input: &str, width: usize) -> String {
    input.chars().take(width).collect()
}

fn pad_to_width(input: &str, width: usize) -> String {
    let mut s = fit_to_width(input, width);
    let len = s.chars().count();
    if len < width {
        s.push_str(&" ".repeat(width - len));
    }
    s
}

fn wrap_line(line: &str, width: usize, directional_chat: bool) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if width == 0 {
        return vec![String::new()];
    }

    // Prefix handling:
    // - gutter lines "N▎ text": preserve "N▎ " on every wrapped continuation line
    // - plain indented lines: preserve leading spaces
    let (first_prefix, continuation, left_pad, right_pad, content) =
        if let Some((_agent_idx, byte_len)) = gutter_info(line) {
            let base_prefix = line[..byte_len].to_string();
            let base_body_width = width.saturating_sub(base_prefix.chars().count());
            let (left_pad, right_pad) =
                chat_body_padding(_agent_idx, base_body_width, directional_chat);
            (
                base_prefix.clone(),
                base_prefix,
                left_pad,
                right_pad,
                &line[byte_len..],
            )
        } else {
            let indent = line.chars().take_while(|c| *c == ' ').count();
            let prefix = " ".repeat(indent);
            let content = &line[indent..];
            (prefix.clone(), prefix, 0, 0, content)
        };

    let first_avail = width
        .saturating_sub(first_prefix.chars().count())
        .saturating_sub(left_pad)
        .saturating_sub(right_pad);
    let cont_avail = width
        .saturating_sub(continuation.chars().count())
        .saturating_sub(left_pad)
        .saturating_sub(right_pad);

    // Keep punctuation attached to words by splitting only on whitespace.
    let mut words = content.split_whitespace();
    let Some(first_word) = words.next() else {
        return vec![first_prefix];
    };

    let mut out: Vec<String> = Vec::new();
    let mut current_line = first_word.to_string();
    let mut current_line_char_len: usize = first_word.chars().count();
    let mut is_first_line = true;

    for word in words {
        let avail = if is_first_line {
            first_avail
        } else {
            cont_avail
        };
        let word_len = word.chars().count();
        let needed = current_line_char_len + 1 + word_len;
        if avail > 0 && needed <= avail {
            current_line.push(' ');
            current_line.push_str(word);
            current_line_char_len = needed;
        } else {
            let prefix = if is_first_line {
                &first_prefix
            } else {
                &continuation
            };
            out.push(format!("{prefix}{current_line}"));
            is_first_line = false;
            current_line.clear();
            current_line.push_str(word);
            current_line_char_len = word_len;
        }
    }

    let prefix = if is_first_line {
        &first_prefix
    } else {
        &continuation
    };
    out.push(format!("{prefix}{current_line}"));

    out
}

fn render_help_key_cell(
    out: &mut impl Write,
    x: usize,
    y: usize,
    key: &str,
    label: &str,
    col_w: usize,
    key_color: Color,
    label_color: Color,
) -> Result<(), AppError> {
    let bracket_col = Color::Rgb {
        r: 90,
        g: 95,
        b: 110,
    };
    // "[key]  label" — key takes key.len()+3 chars, label fills the rest
    let label_w = col_w.saturating_sub(key.len() + 3);
    queue!(
        out,
        MoveTo(x as u16, y as u16),
        SetForegroundColor(bracket_col),
        Print("["),
        SetForegroundColor(key_color),
        SetAttribute(Attribute::Bold),
        Print(key),
        SetForegroundColor(bracket_col),
        SetAttribute(Attribute::Reset),
        Print("]  "),
        SetForegroundColor(label_color),
        Print(pad_to_width(label, label_w)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn render_help_modal(out: &mut impl Write, width: usize, height: usize) -> Result<(), AppError> {
    if width < 40 || height < 10 {
        return Ok(());
    }

    let modal_width = (width.saturating_sub(4)).min(84);
    let modal_height = (height.saturating_sub(2)).min(26);
    let modal_x = (width.saturating_sub(modal_width)) / 2;
    let modal_y = (height.saturating_sub(modal_height)) / 2;

    draw_box(
        out,
        modal_x,
        modal_y,
        modal_width,
        modal_height,
        Some("Help: Keyboard Map"),
        None,
    )?;

    let inner_x = modal_x + 1;
    let inner_w = modal_width.saturating_sub(2);
    let max_y = modal_y + modal_height.saturating_sub(2);

    // Clear interior so underlying content doesn't show through
    let blank = " ".repeat(inner_w);
    for row in (modal_y + 1)..=max_y {
        queue!(
            out,
            MoveTo(inner_x as u16, row as u16),
            SetAttribute(Attribute::Reset),
            Print(&blank)
        )?;
    }

    let mut y = modal_y + 2;

    // Palette
    let col_aria = Color::Rgb {
        r: 178,
        g: 145,
        b: 240,
    }; // Aria purple
    let col_basil = Color::Rgb {
        r: 120,
        g: 205,
        b: 145,
    }; // Basil green
    let col_shared = Color::Rgb {
        r: 220,
        g: 180,
        b: 100,
    }; // shared gold
    let col_cat = Color::Rgb {
        r: 180,
        g: 180,
        b: 100,
    }; // category headers
    let col_key = Color::Rgb {
        r: 230,
        g: 230,
        b: 230,
    }; // misc keys
    let col_label = Color::Rgb {
        r: 180,
        g: 186,
        b: 201,
    }; // descriptions
    let col_dim = Color::Rgb {
        r: 90,
        g: 95,
        b: 110,
    }; // dividers/brackets/footer

    // Three equal columns with a 2-char gap between each
    let gap: usize = 2;
    let col_w = (inner_w.saturating_sub(gap * 2)) / 3;
    let x1 = inner_x;
    let x2 = inner_x + col_w + gap;
    let x3 = inner_x + (col_w + gap) * 2;

    // ── AGENT COLUMNS ─────────────────────────────────────────────────────
    // Headers: ◀ ARIA  |  ◈ SHARED  |  BASIL ▶
    if y < max_y {
        queue!(
            out,
            MoveTo(x1 as u16, y as u16),
            SetForegroundColor(col_aria),
            SetAttribute(Attribute::Bold),
            Print(pad_to_width("◀  ARIA", col_w)),
            MoveTo(x2 as u16, y as u16),
            SetForegroundColor(col_shared),
            Print(pad_to_width("◈  SHARED", col_w)),
            MoveTo(x3 as u16, y as u16),
            SetForegroundColor(col_basil),
            Print(pad_to_width("BASIL  ▶", col_w)),
            SetAttribute(Attribute::Reset)
        )?;
        y += 1;
    }

    // Divider under headers
    if y < max_y {
        let bar: String = "─".repeat(col_w);
        queue!(
            out,
            MoveTo(x1 as u16, y as u16),
            SetForegroundColor(col_dim),
            Print(&bar),
            MoveTo(x2 as u16, y as u16),
            Print(&bar),
            MoveTo(x3 as u16, y as u16),
            Print(&bar),
            SetAttribute(Attribute::Reset)
        )?;
        y += 1;
    }

    // Agent key rows: [a]/[q] Aria  |  [w]/[s] Shared  |  [d]/[e] Basil
    let agent_rows: &[(&str, &str, &str, &str, &str, &str)] = &[
        ("q", "Prompt", "w", "Brief", "e", "Prompt"),
        ("a", "Model", "s", "Presets", "d", "Model"),
    ];
    for &(ak, al, sk, sl, bk, bl) in agent_rows {
        if y >= max_y {
            break;
        }
        render_help_key_cell(out, x1, y, ak, al, col_w, col_aria, col_label)?;
        render_help_key_cell(out, x2, y, sk, sl, col_w, col_shared, col_label)?;
        render_help_key_cell(out, x3, y, bk, bl, col_w, col_basil, col_label)?;
        y += 1;
    }

    y += 1; // blank row

    // ── MISC COLUMNS ──────────────────────────────────────────────────────
    // Headers: LIFECYCLE  |  CONFIG  |  VIEW
    if y < max_y {
        queue!(
            out,
            MoveTo(x1 as u16, y as u16),
            SetForegroundColor(col_cat),
            SetAttribute(Attribute::Bold),
            Print(pad_to_width("LIFECYCLE", col_w)),
            MoveTo(x2 as u16, y as u16),
            Print(pad_to_width("CONFIG", col_w)),
            MoveTo(x3 as u16, y as u16),
            Print(pad_to_width("VIEW", col_w)),
            SetAttribute(Attribute::Reset)
        )?;
        y += 1;
    }

    let lifecycle: &[(&str, &str)] = &[
        ("r", "Run"),
        ("p", "Pause"),
        ("Esc", "Back/Stop"),
        ("c", "Clear"),
        ("Ctrl-Q", "Quit"),
    ];
    let config: &[(&str, &str)] = &[
        ("1-9", "Turns"),
        ("`", "Edit Turns"),
        ("x", "Routing"),
        ("y", "Layout"),
    ];
    let view: &[(&str, &str)] = &[("n", "Thinking"), ("v", "Selection"), ("b", "Tmux")];

    let misc_rows = lifecycle.len().max(config.len()).max(view.len());
    for i in 0..misc_rows {
        if y >= max_y {
            break;
        }
        if let Some(&(k, l)) = lifecycle.get(i) {
            render_help_key_cell(out, x1, y, k, l, col_w, col_key, col_label)?;
        }
        if let Some(&(k, l)) = config.get(i) {
            render_help_key_cell(out, x2, y, k, l, col_w, col_key, col_label)?;
        }
        if let Some(&(k, l)) = view.get(i) {
            render_help_key_cell(out, x3, y, k, l, col_w, col_key, col_label)?;
        }
        y += 1;
    }

    y += 1; // blank row

    // ── NAVIGATION ────────────────────────────────────────────────────────
    if y < max_y {
        queue!(
            out,
            MoveTo(inner_x as u16, y as u16),
            SetForegroundColor(col_cat),
            SetAttribute(Attribute::Bold),
            Print("NAVIGATION"),
            SetAttribute(Attribute::Reset)
        )?;
        y += 1;
    }

    let nav_entries: &[(&str, &str)] = &[
        ("↑/↓", "Scroll"),
        ("j/k", "Vim scroll"),
        ("PgUp/Dn", "Page"),
        ("Home/End", "Jump"),
        ("?/h", "Help"),
    ];

    if y < max_y {
        let mut nx = inner_x;
        for &(k, l) in nav_entries {
            // "[k] l  " — measure display width conservatively
            let cell_len = 1 + k.len() + 2 + l.len() + 2; // [k]  l  (ASCII approximation)
            if nx + cell_len > inner_x + inner_w {
                break;
            }
            queue!(
                out,
                MoveTo(nx as u16, y as u16),
                SetForegroundColor(col_dim),
                Print("["),
                SetForegroundColor(col_key),
                SetAttribute(Attribute::Bold),
                Print(k),
                SetForegroundColor(col_dim),
                SetAttribute(Attribute::Reset),
                Print("] "),
                SetForegroundColor(col_label),
                Print(l),
                Print("  "),
                SetAttribute(Attribute::Reset)
            )?;
            nx += cell_len;
        }
    }

    // Footer centred at bottom of modal
    let footer = "Press ? / h / Esc to close";
    let footer_y = modal_y + modal_height.saturating_sub(2);
    if footer_y < height {
        let footer_x = inner_x + (inner_w.saturating_sub(footer.len())) / 2;
        queue!(
            out,
            MoveTo(footer_x as u16, footer_y as u16),
            SetForegroundColor(col_dim),
            Print(footer),
            SetAttribute(Attribute::Reset)
        )?;
    }

    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> UiState {
        UiState::new(
            "Initial prompt".to_string(),
            ["claude".to_string(), "codex".to_string()],
            6,
            RoutingMode::PromptOnlyToAgentA,
            false,
            false,
            false,
            false,
            [String::new(), String::new()],
            Vec::new(),
            None,
        )
    }

    fn preset(name: &str, prompt: &str, agent_a: &str, agent_b: &str) -> Preset {
        Preset {
            name: name.to_string(),
            prompt: prompt.to_string(),
            agent_a_system_prompt: agent_a.to_string(),
            agent_b_system_prompt: agent_b.to_string(),
        }
    }

    fn mission_run_state(state: &UiState) -> &'static str {
        match footer_badge_state(&FooterView {
            auto_scroll: state.auto_scroll,
            completed: state.completed,
            run_started: state.run_started,
            run_failed: state.run_failed,
            paused: state.paused,
            mouse_capture: state.mouse_capture,
            editing_prompt: state.editing_prompt,
            editing_turns: state.editing_turns,
            agent_chooser: state.agent_chooser.map(|chooser| chooser.agent_idx),
        }) {
            FooterBadgeState::Ready => "ready",
            FooterBadgeState::Live => "live",
            FooterBadgeState::Paused => "paused",
            FooterBadgeState::Done => "done",
            FooterBadgeState::Error => "error",
        }
    }

    fn mission_active_agent(state: &UiState) -> &'static str {
        match state.active_agent {
            Some(0) => "aria",
            Some(1) => "basil",
            _ => "none",
        }
    }

    fn agent_runtime_status(state: &UiState, agent_idx: usize) -> &'static str {
        match (state.working.agent_working[agent_idx], state.active_agent == Some(agent_idx)) {
            (true, true) => "live",
            (true, false) => "busy",
            (false, true) => "focus",
            (false, false) => "idle",
        }
    }

    #[test]
    fn mission_panel_split_preserves_readable_left_and_right_columns() {
        let layout = mission_panel_layout(1, 1, 78, 6);
        assert!(layout.prompt_w > layout.telem_w);
        assert!(layout.telem_w >= 16);
    }

    #[test]
    fn prompt_panel_height_stays_standard_during_live_run() {
        let mut state = test_state();
        state.run_started = true;
        state.launched_prompt = Some(state.prompt.clone());
        assert_eq!(state.prompt_panel_height(24), 10);
    }

    #[test]
    fn prompt_edit_key_opens_editor_during_live_run() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.run_started = true;
        state.launched_prompt = Some(state.prompt.clone());

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(state.editing_prompt);
        assert_eq!(state.edit_buffer, state.prompt);
        assert_eq!(state.edit_cursor, state.prompt_char_len());
        assert_eq!(state.edit_selection_anchor, None);

        state.edit_buffer = "Updated live prompt".to_string();
        state.edit_cursor = state.prompt_char_len();
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.prompt, "Updated live prompt");
        assert!(!state.editing_prompt);
    }

    #[test]
    fn prompt_editor_ctrl_s_saves_without_exiting() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "Updated live prompt".to_string();
        state.edit_cursor = "Updated live prompt".chars().count();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.prompt, "Updated live prompt");
        assert!(state.editing_prompt);
        assert_eq!(state.edit_buffer, "Updated live prompt");
    }

    #[test]
    fn prompt_editor_ctrl_a_selects_all_and_replaces_text() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "hello".to_string();
        state.edit_cursor = state.prompt_char_len();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.prompt_selection_range(), Some((0, 5)));

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_buffer, "X");
        assert_eq!(state.edit_cursor, 1);
        assert_eq!(state.prompt_selection_range(), None);
    }

    #[test]
    fn prompt_editor_shift_navigation_and_delete_respect_selection() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "abcde".to_string();
        state.edit_cursor = 2;

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.prompt_selection_range(), Some((2, 5)));

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_buffer, "ab");
        assert_eq!(state.edit_cursor, 2);
        assert_eq!(state.prompt_selection_range(), None);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.prompt_selection_range(), Some((0, 1)));

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_buffer, "b");
        assert_eq!(state.edit_cursor, 0);
        assert_eq!(state.prompt_selection_range(), None);
    }

    #[test]
    fn prompt_editor_plain_arrows_collapse_selection_first() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "abcdef".to_string();
        state.edit_selection_anchor = Some(1);
        state.edit_cursor = 4;
        assert_eq!(state.prompt_selection_range(), Some((1, 4)));

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_cursor, 1);
        assert_eq!(state.prompt_selection_range(), None);

        state.edit_selection_anchor = Some(1);
        state.edit_cursor = 4;
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_cursor, 4);
        assert_eq!(state.prompt_selection_range(), None);
    }

    #[test]
    fn prompt_editor_plain_home_end_collapse_selection_first() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "abcdef".to_string();

        state.edit_selection_anchor = Some(1);
        state.edit_cursor = 5;
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_cursor, 1);
        assert_eq!(state.prompt_selection_range(), None);

        state.edit_selection_anchor = Some(1);
        state.edit_cursor = 5;
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.edit_cursor, 5);
        assert_eq!(state.prompt_selection_range(), None);
    }

    #[test]
    fn tab_is_ignored_in_normal_mode() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.modal_state, ModalState::Hidden);
    }

    #[test]
    fn tab_does_not_open_any_modal_while_editing_prompt() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "hello".to_string();
        state.edit_cursor = 5;

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.modal_state, ModalState::Hidden);
    }

    #[test]
    fn q_and_e_open_the_expected_sysprompt_editors() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut aria_state = test_state();
        let mut basil_state = test_state();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &control_tx,
            &mut aria_state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            &control_tx,
            &mut basil_state,
        );

        assert_eq!(
            aria_state
                .sysprompt_edit
                .as_ref()
                .map(|edit| edit.active_agent_idx),
            Some(0)
        );
        assert_eq!(
            basil_state
                .sysprompt_edit
                .as_ref()
                .map(|edit| edit.active_agent_idx),
            Some(1)
        );
    }

    #[test]
    fn a_and_d_open_the_expected_agent_choosers() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut aria_state = test_state();
        let mut basil_state = test_state();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            &control_tx,
            &mut aria_state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            &control_tx,
            &mut basil_state,
        );

        assert_eq!(
            aria_state.agent_chooser,
            Some(AgentChooserState {
                agent_idx: 0,
                cursor: 0,
            })
        );
        assert_eq!(
            basil_state.agent_chooser,
            Some(AgentChooserState {
                agent_idx: 1,
                cursor: 1,
            })
        );
    }

    #[test]
    fn agent_chooser_navigation_and_enter_commit_selected_command() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.agent_cmds[0], "codex-api");
        assert_eq!(state.agent_chooser, None);
    }

    #[test]
    fn agent_chooser_esc_cancels_without_changing_command() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        let original = state.agent_cmds[1].clone();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.agent_cmds[1], original);
        assert_eq!(state.agent_chooser, None);
    }

    #[test]
    fn top_side_panel_renders_agent_chooser_choices() {
        let mut state = test_state();
        state.open_agent_chooser(1);

        let mut out = Vec::new();
        render_top_side_panel_content(&mut out, 0, 0, 32, 5, 1, &state)
            .expect("render should succeed");

        let rendered = String::from_utf8_lossy(&out);
        assert!(rendered.contains("claude"));
        assert!(rendered.contains("codex"));
        assert!(rendered.contains("codex-api"));
        assert!(rendered.contains("Enter apply"));
    }

    #[test]
    fn tab_switches_sysprompt_targets_without_losing_drafts() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.agent_system_prompts = ["aria saved".to_string(), "basil saved".to_string()];
        state.open_sysprompt_edit(0);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        let edit = state
            .sysprompt_edit
            .as_ref()
            .expect("editor should stay open");
        assert_eq!(edit.active_agent_idx, 1);
        assert_eq!(edit.buffers[0], "aria saved!");
        assert_eq!(edit.buffers[1], "basil saved");
        assert_eq!(state.agent_system_prompts[0], "aria saved");
    }

    #[test]
    fn ctrl_s_saves_sysprompt_and_exits_editor() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.agent_system_prompts = ["aria saved".to_string(), String::new()];
        state.open_sysprompt_edit(0);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.agent_system_prompts[0], "aria saved!");
        assert!(
            state.sysprompt_edit.is_none(),
            "editor should be closed after Ctrl+S"
        );
    }

    #[test]
    fn esc_exits_sysprompt_before_prompt_edit_when_both_are_active() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.editing_prompt = true;
        state.edit_buffer = "brief".to_string();
        state.edit_cursor = 5;
        state.open_sysprompt_edit(0);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert!(state.sysprompt_edit.is_none());
        assert!(state.editing_prompt);
    }

    #[test]
    fn esc_stops_run_only_after_local_modes_are_cleared() {
        let (control_tx, mut control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.run_started = true;
        state.paused = true;
        state.editing_turns = true;
        state.turns_buffer = "12".to_string();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(!state.editing_turns);
        assert_eq!(*control_rx.borrow(), ConversationControl::Run);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(!state.paused);
        assert_eq!(*control_rx.borrow_and_update(), ConversationControl::Stop);
    }

    #[test]
    fn normal_mode_digits_set_turns_immediately() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.turns, 9);
        assert!(!state.editing_turns);
    }

    #[test]
    fn backtick_opens_turn_editor_and_digits_stay_local() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.turns = 4;

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(state.editing_turns);
        assert_eq!(state.turns_buffer, "4");

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.turns, 4);
        assert_eq!(state.turns_buffer, "42");

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );
        assert_eq!(state.turns, 42);
        assert!(!state.editing_turns);
    }

    #[test]
    fn global_rebinds_drive_same_state_changes() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        let starting_mode = state.routing_mode;
        let starting_layout = state.layout_mode;

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.routing_mode, starting_mode.next());
        assert_eq!(state.layout_mode, starting_layout.next());
        assert!(state.thinking_expanded);
        assert!(!state.mouse_capture);
        assert!(state.show_tmux_panels);
    }

    #[test]
    fn s_enters_preset_mode_and_j_k_move_selection() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.presets = vec![
            preset("Alpha", "Prompt A", "Aria A", "Basil A"),
            preset("Beta", "Prompt B", "Aria B", "Basil B"),
            preset("Gamma", "Prompt C", "Aria C", "Basil C"),
        ];
        state.active_preset_idx = Some(1);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::FocusedList { cursor: 1 }
        ));

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::FocusedList { cursor: 2 }
        ));

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::FocusedList { cursor: 1 }
        ));
    }

    #[test]
    fn preset_mode_enter_loads_selected_preset_without_exiting_mode() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.presets = vec![
            preset("Alpha", "Prompt A", "Aria A", "Basil A"),
            preset("Beta", "Prompt B", "Aria B", "Basil B"),
        ];

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.prompt, "Prompt B");
        assert_eq!(
            state.agent_system_prompts,
            ["Aria B".to_string(), "Basil B".to_string()]
        );
        assert_eq!(state.active_preset_idx, Some(1));
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::FocusedList { cursor: 1 }
        ));
    }

    #[test]
    fn preset_mode_ctrl_s_opens_save_flow_and_ctrl_s_confirms_new_preset() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.prompt = "Saved prompt".to_string();
        state.agent_system_prompts = ["Aria saved".to_string(), "Basil saved".to_string()];

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::Naming { .. }
        ));

        for c in "Demo".chars() {
            handle_key_event(
                Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
                &control_tx,
                &mut state,
            );
        }
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.presets.len(), 1);
        assert_eq!(state.presets[0].name, "Demo");
        assert_eq!(state.presets[0].prompt, "Saved prompt");
        assert_eq!(state.presets[0].agent_a_system_prompt, "Aria saved");
        assert_eq!(state.presets[0].agent_b_system_prompt, "Basil saved");
        assert_eq!(state.active_preset_idx, Some(0));
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::FocusedList { cursor: 0 }
        ));
    }

    #[test]
    fn preset_mode_ctrl_d_deletes_the_selected_preset() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.presets = vec![
            preset("Alpha", "Prompt A", "Aria A", "Basil A"),
            preset("Beta", "Prompt B", "Aria B", "Basil B"),
        ];
        state.active_preset_idx = Some(1);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );

        assert_eq!(state.presets.len(), 1);
        assert_eq!(state.presets[0].name, "Beta");
        assert_eq!(state.active_preset_idx, Some(0));
        assert!(matches!(
            state.preset_panel_state,
            PresetPanelState::FocusedList { cursor: 0 }
        ));
    }

    #[test]
    fn preset_mode_esc_closes_before_run_stop() {
        let (control_tx, mut control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();
        state.presets = vec![preset("Alpha", "Prompt A", "Aria A", "Basil A")];
        state.run_started = true;
        state.paused = true;

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert!(matches!(state.preset_panel_state, PresetPanelState::Idle));
        assert_eq!(*control_rx.borrow(), ConversationControl::Run);

        handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );
        assert_eq!(*control_rx.borrow_and_update(), ConversationControl::Stop);
    }

    #[test]
    fn plain_q_opens_sysprompt_not_quit() {
        let (control_tx, _control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();

        let action = handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &control_tx,
            &mut state,
        );

        assert!(action.is_none());
        assert!(!state.finished);
    }

    #[test]
    fn ctrl_q_quits_cleanly() {
        let (control_tx, mut control_rx) = watch::channel(ConversationControl::Run);
        let mut state = test_state();

        let action = handle_key_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            &control_tx,
            &mut state,
        );

        assert!(matches!(action, Some(UiAction::Quit { .. })));
        assert!(state.finished);
        assert_eq!(*control_rx.borrow_and_update(), ConversationControl::Stop);
    }

    #[test]
    fn staged_marker_only_appears_when_prompt_diverges_from_live_run() {
        let mut state = test_state();
        state.run_started = true;
        state.launched_prompt = Some("Launch mission brief".to_string());
        state.prompt = "Launch mission brief".to_string();

        let mut rows = vec![
            format!("state:{}", mission_run_state(&state)),
            format!("active:{}", mission_active_agent(&state)),
            format!(
                "turns:{} mode:{}",
                state.turns,
                routing_mode_short(state.routing_mode)
            ),
            format!(
                "layout:{} think:{}",
                layout_mode_short(state.layout_mode),
                if state.thinking_expanded {
                    "expand"
                } else {
                    "wrap"
                }
            ),
            format!(
                "tmux:{} a:{} b:{}",
                if state.show_tmux_panels { "on" } else { "off" },
                agent_runtime_status(&state, 0),
                agent_runtime_status(&state, 1)
            ),
        ];
        if state.has_staged_prompt() {
            rows.push("next:staged".to_string());
        }
        assert!(!rows.iter().any(|line| line == "next:staged"));

        state.prompt = "Edited next launch brief".to_string();
        if state.has_staged_prompt() {
            rows.push("next:staged".to_string());
        }
        assert!(rows.iter().any(|line| line == "next:staged"));
        assert_eq!(
            state.launched_prompt.as_deref(),
            Some("Launch mission brief")
        );
    }

    #[test]
    fn run_end_clears_live_prompt_tracking_state() {
        let mut state = test_state();
        state.run_started = true;
        state.launched_prompt = Some("Launch mission brief".to_string());
        state.prompt = "Edited next launch brief".to_string();
        state.active_agent = Some(1);

        state.apply_conversation_event(ConversationEvent::Done);

        assert!(!state.run_started);
        assert!(state.launched_prompt.is_none());
        assert!(state.active_agent.is_none());
    }

    #[test]
    fn error_clears_active_agent_focus_state() {
        let mut state = test_state();
        state.run_started = true;
        state.active_agent = Some(0);

        state.apply_conversation_event(ConversationEvent::Error {
            code: crate::error::ErrorCode::Cancelled,
            message: "stopped".to_string(),
        });

        assert!(state.run_failed);
        assert!(!state.run_started);
        assert!(state.active_agent.is_none());
    }

    #[test]
    fn footer_badge_state_tracks_runtime_priority() {
        let base = FooterView {
            auto_scroll: true,
            completed: false,
            run_started: false,
            run_failed: false,
            paused: false,
            mouse_capture: true,
            editing_prompt: false,
            editing_turns: false,
            agent_chooser: None,
        };

        assert_eq!(footer_badge_state(&base), FooterBadgeState::Ready);
        assert_eq!(
            footer_badge_state(&FooterView {
                run_started: true,
                ..base
            }),
            FooterBadgeState::Live
        );
        assert_eq!(
            footer_badge_state(&FooterView {
                paused: true,
                run_started: true,
                ..base
            }),
            FooterBadgeState::Paused
        );
        assert_eq!(
            footer_badge_state(&FooterView {
                completed: true,
                ..base
            }),
            FooterBadgeState::Done
        );
        assert_eq!(
            footer_badge_state(&FooterView {
                run_failed: true,
                completed: true,
                ..base
            }),
            FooterBadgeState::Error
        );
    }

    #[test]
    fn agent_headers_match_conversation_agent_names() {
        for &(header, idx) in AGENT_HEADERS {
            let expected = conversation::agent_name(idx);
            assert_eq!(
                header, expected,
                "AGENT_HEADERS[{idx}] out of sync with agent_name()"
            );
        }
    }

    #[test]
    fn thinking_panel_title_uses_basil_nature_emoji_animation_when_basil_is_active() {
        let mut state = test_state();
        state.active_agent = Some(1);
        state.working.agent_working[1] = true;
        state.spinner_frame = 2;

        assert_eq!(state.thinking_panel_title(1), "🌱 Basil Thinking");
    }

    #[test]
    fn thinking_panel_title_uses_purple_emoji_animation_for_aria_when_active() {
        let mut state = test_state();
        state.active_agent = Some(0);
        state.working.agent_working[0] = true;
        state.spinner_frame = 1;

        assert_eq!(state.thinking_panel_title(0), "🟪 Aria Thinking");
    }

    #[test]
    fn thinking_panel_title_is_static_when_agent_is_not_active() {
        let mut state = test_state();
        state.active_agent = Some(0);
        state.working.agent_working[1] = true;
        state.spinner_frame = 3;

        assert_eq!(state.thinking_panel_title(1), "* Basil Thinking");
    }

    #[test]
    fn reasoning_prefix_glyph_uses_agent_specific_animation_frames() {
        assert_eq!(reasoning_prefix_glyph(0, 0, true), '✦');
        assert_eq!(reasoning_prefix_glyph(0, 2, true), '⋆');
        assert_eq!(reasoning_prefix_glyph(1, 0, true), '❀');
        assert_eq!(reasoning_prefix_glyph(1, 3, true), '✾');
    }

    #[test]
    fn reasoning_prefix_glyph_falls_back_to_static_dot_when_not_animating() {
        assert_eq!(reasoning_prefix_glyph(0, 0, false), '·');
        assert_eq!(reasoning_prefix_glyph(1, 4, false), '·');
    }

    #[test]
    fn orb_palette_morphs_aria_to_basil_without_dark_edge_colors() {
        let left = orb_base_color(0.1);
        let middle = orb_base_color(0.5);
        let right = orb_base_color(0.9);

        assert!(left.0 >= 150 && left.1 >= 120 && left.2 >= 220);
        assert!(middle.0 >= 220 && middle.1 >= 210 && middle.2 >= 200);
        assert!(right.0 >= 200 && right.1 >= 180 && right.2 >= 100);
    }

    #[test]
    fn tri_pane_rows_include_tool_timeline_in_thinking_panel() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "call_1".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "planning".to_string(),
        });

        let (left, _mid, right) = state.build_tri_pane_rows(28, 28, 28);
        let left_kinds = left.iter().map(|line| &line.kind).collect::<Vec<_>>();
        let left_text = left
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(left_kinds.contains(&&ThinkingLineKind::ToolHeader));
        assert!(left_kinds.contains(&&ThinkingLineKind::ToolResult));
        assert!(left_kinds.contains(&&ThinkingLineKind::Reasoning));
        assert!(left_text.contains("[function_call_output]"));
        assert!(left_text.contains("call_1"));
        assert!(left_text.contains("planning"));
        assert!(
            right
                .iter()
                .all(|line| line.kind == ThinkingLineKind::Blank)
        );
    }

    #[test]
    fn thinking_timeline_keeps_reasoning_tools_and_boundaries_in_arrival_order() {
        let mut state = test_state();
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "plan".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " reflect".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "First message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\n\nSecond message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "after boundary".to_string(),
        });

        let turn = state.turns_log.last().expect("turn exists");
        assert_eq!(turn.timeline.len(), 5);
        match &turn.timeline[0] {
            ThinkingTimelineRecord::Reasoning(text) => assert_eq!(text, "plan"),
            other => panic!("expected first reasoning entry, got {other:?}"),
        }
        match &turn.timeline[1] {
            ThinkingTimelineRecord::Tool(tool) => {
                assert_eq!(tool.kind, ToolStreamEventKind::Use);
                assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));
            }
            other => panic!("expected tool entry, got {other:?}"),
        }
        match &turn.timeline[2] {
            ThinkingTimelineRecord::Reasoning(text) => assert_eq!(text, " reflect"),
            other => panic!("expected second reasoning entry, got {other:?}"),
        }
        assert!(matches!(
            &turn.timeline[3],
            ThinkingTimelineRecord::MessageBoundary
        ));
        match &turn.timeline[4] {
            ThinkingTimelineRecord::Reasoning(text) => assert_eq!(text, "after boundary"),
            other => panic!("expected post-boundary reasoning entry, got {other:?}"),
        }
        assert_eq!(turn.main_chunks.len(), 2);

        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " later".to_string(),
        });
        let turn = state.turns_log.last().expect("turn exists");
        assert_eq!(turn.timeline.len(), 5);
        match &turn.timeline[4] {
            ThinkingTimelineRecord::Reasoning(text) => assert_eq!(text, "after boundary later"),
            other => panic!("expected merged post-boundary reasoning entry, got {other:?}"),
        }
    }

    #[test]
    fn tri_pane_rows_interleave_timeline_entries_within_each_message_segment() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "plan".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " reflect".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "First message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\n\nSecond message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "call_1".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " after boundary".to_string(),
        });

        let (left, mid, right) = state.build_tri_pane_rows(40, 40, 40);
        let left_non_blank = left
            .iter()
            .filter(|line| line.kind != ThinkingLineKind::Blank)
            .map(|line| (line.kind, line.text.clone()))
            .collect::<Vec<_>>();

        assert_eq!(left_non_blank[0].0, ThinkingLineKind::ToolHeader);
        assert_eq!(left_non_blank[1].0, ThinkingLineKind::Reasoning);
        assert_eq!(left_non_blank[1].1, "plan");
        assert_eq!(left_non_blank[2].0, ThinkingLineKind::ToolResult);
        assert!(
            left_non_blank[2]
                .1
                .contains("[function_call_output] call_1")
        );
        assert_eq!(left_non_blank[3].0, ThinkingLineKind::Reasoning);
        assert_eq!(left_non_blank[3].1, " reflect");
        assert_eq!(left_non_blank[4].0, ThinkingLineKind::Reasoning);
        assert_eq!(left_non_blank[4].1, " after boundary");
        assert_eq!(
            left_non_blank.last().expect("footer row").0,
            ThinkingLineKind::ToolFooter
        );
        assert!(
            right
                .iter()
                .all(|line| line.kind == ThinkingLineKind::Blank)
        );

        let mid_joined = mid.join("\n");
        assert!(mid_joined.contains("0▎ First message."));
        assert!(mid_joined.contains("0▎ Second message."));
    }

    #[test]
    fn tri_pane_rows_preserve_multi_event_order_across_message_breaks() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "plan".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " reflect".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Error,
            tool_type: "command_execution".to_string(),
            text: "bash: rg missing".to_string(),
            tool_call_id: Some("item_2".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " recover".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "First message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\n\nSecond message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " after boundary".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "call_1".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " done".to_string(),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(44, 44, 44);
        let rows = left
            .iter()
            .map(|line| (line.kind, line.text.clone()))
            .collect::<Vec<_>>();
        let non_blank = rows
            .iter()
            .filter(|(kind, _)| *kind != ThinkingLineKind::Blank)
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(non_blank[0].0, ThinkingLineKind::ToolHeader);
        assert_eq!(
            non_blank[1],
            (ThinkingLineKind::Reasoning, "plan".to_string())
        );
        assert_eq!(non_blank[2].0, ThinkingLineKind::ToolResult);
        assert!(non_blank[2].1.contains("[function_call_output] call_1"));
        assert_eq!(
            non_blank[3],
            (ThinkingLineKind::Reasoning, " reflect".to_string())
        );
        assert_eq!(non_blank[4].0, ThinkingLineKind::ToolError);
        assert!(
            non_blank[4]
                .1
                .contains("[command_execution] bash: rg missing")
        );
        assert_eq!(
            non_blank[5],
            (ThinkingLineKind::Reasoning, " recover".to_string())
        );
        assert_eq!(
            non_blank[6],
            (
                ThinkingLineKind::Reasoning,
                " after boundary done".to_string()
            )
        );
        assert_eq!(
            non_blank.last().expect("footer row").0,
            ThinkingLineKind::ToolFooter
        );

        let first_segment_last = rows
            .iter()
            .position(|entry| *entry == (ThinkingLineKind::Reasoning, " recover".to_string()))
            .expect("last first-segment reasoning row");
        let second_segment_first = rows
            .iter()
            .position(|entry| {
                *entry
                    == (
                        ThinkingLineKind::Reasoning,
                        " after boundary done".to_string(),
                    )
            })
            .expect("first second-segment reasoning row");
        assert!(
            rows[first_segment_last + 1..second_segment_first]
                .iter()
                .any(|(kind, _)| *kind == ThinkingLineKind::Blank),
            "message boundaries should still add a blank separator between thinking segments"
        );
    }

    #[test]
    fn tri_pane_collapsed_rows_truncate_combined_interleaved_timeline() {
        let mut state = test_state();
        state.thinking_expanded = false;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "plan".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: " reflect".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "First message.".to_string(),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(40, 40, 40);
        let left_non_blank = left
            .iter()
            .filter(|line| line.kind != ThinkingLineKind::Blank)
            .map(|line| (line.kind, line.text.clone()))
            .collect::<Vec<_>>();

        assert_eq!(left_non_blank.len(), 2);
        assert_eq!(left_non_blank[0].0, ThinkingLineKind::ToolHeader);
        assert_eq!(left_non_blank[1].0, ThinkingLineKind::Reasoning);
        assert_eq!(left_non_blank[1].1, "plan…");
    }

    #[test]
    fn tri_pane_rows_render_agent_b_timeline_in_right_lane() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 1 });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 1,
            text: "plan".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 1,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_b".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 1,
            text: " reflect".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 1,
            text: "Basil message.".to_string(),
        });

        let (left, mid, right) = state.build_tri_pane_rows(40, 40, 40);
        let right_non_blank = right
            .iter()
            .filter(|line| line.kind != ThinkingLineKind::Blank)
            .map(|line| (line.kind, line.text.clone()))
            .collect::<Vec<_>>();

        assert!(left.iter().all(|line| line.kind == ThinkingLineKind::Blank));
        assert_eq!(right_non_blank[0].0, ThinkingLineKind::ToolHeader);
        assert_eq!(
            right_non_blank[1],
            (ThinkingLineKind::Reasoning, "plan".to_string())
        );
        assert_eq!(right_non_blank[2].0, ThinkingLineKind::ToolUse);
        assert!(right_non_blank[2].1.contains("[function_call] shell"));
        assert_eq!(
            right_non_blank[3],
            (ThinkingLineKind::Reasoning, " reflect".to_string())
        );
        assert_eq!(
            right_non_blank.last().expect("footer row").0,
            ThinkingLineKind::ToolFooter
        );

        let mid_joined = mid.join("\n");
        assert!(mid_joined.contains("Basil"));
        assert!(mid_joined.contains("1▎ Basil message."));
    }

    #[test]
    fn tool_result_replaces_prior_use_row_in_timeline() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "shell".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "call_1".to_string(),
            tool_call_id: Some("call_1".to_string()),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(32, 32, 32);
        let timeline_rows = left
            .iter()
            .filter(|line| {
                matches!(
                    line.kind,
                    ThinkingLineKind::ToolUse
                        | ThinkingLineKind::ToolResult
                        | ThinkingLineKind::ToolError
                )
            })
            .count();
        assert_eq!(timeline_rows, 1);
        assert!(
            left.iter()
                .any(|line| line.kind == ThinkingLineKind::ToolResult)
        );
        assert!(
            !left
                .iter()
                .any(|line| line.kind == ThinkingLineKind::ToolUse)
        );
    }

    #[test]
    fn reasoning_prefix_is_not_reclassified_as_tool_row() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Thinking {
            agent_idx: 0,
            text: "✓ this is normal reasoning".to_string(),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(34, 34, 34);
        assert!(left.iter().any(|line| {
            line.kind == ThinkingLineKind::Reasoning
                && line.text.contains("✓ this is normal reasoning")
        }));
        assert!(
            !left
                .iter()
                .any(|line| line.kind == ThinkingLineKind::ToolResult)
        );
    }

    #[test]
    fn wrapped_tool_lines_keep_continuation_kind() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "command_execution".to_string(),
            text: "very long tool name that wraps across rows".to_string(),
            tool_call_id: Some("item_1".to_string()),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(14, 20, 14);
        let use_rows = left
            .iter()
            .filter(|line| line.kind == ThinkingLineKind::ToolUse)
            .count();
        let continuation_rows = left
            .iter()
            .filter(|line| line.kind == ThinkingLineKind::ToolContinuation)
            .count();
        assert_eq!(use_rows, 1);
        assert!(continuation_rows >= 1);
    }

    #[test]
    fn interleaved_tool_completions_match_by_call_id() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "first".to_string(),
            tool_call_id: Some("call_a".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "second".to_string(),
            tool_call_id: Some("call_b".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "call_a".to_string(),
            tool_call_id: Some("call_a".to_string()),
        });

        let turn = state.turns_log.last().expect("turn exists");
        let tools = turn
            .timeline
            .iter()
            .filter_map(|entry| match entry {
                ThinkingTimelineRecord::Reasoning(_) => None,
                ThinkingTimelineRecord::Tool(tool) => Some(tool),
                ThinkingTimelineRecord::MessageBoundary => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].kind, ToolStreamEventKind::Result);
        assert_eq!(tools[0].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(tools[1].kind, ToolStreamEventKind::Use);
        assert_eq!(tools[1].tool_call_id.as_deref(), Some("call_b"));
    }

    #[test]
    fn unmatched_or_missing_completion_id_appends_separate_row() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Use,
            tool_type: "function_call".to_string(),
            text: "first".to_string(),
            tool_call_id: Some("call_a".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "call_x".to_string(),
            tool_call_id: Some("call_x".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "function_call_output".to_string(),
            text: "unknown".to_string(),
            tool_call_id: None,
        });

        let turn = state.turns_log.last().expect("turn exists");
        let tools = turn
            .timeline
            .iter()
            .filter_map(|entry| match entry {
                ThinkingTimelineRecord::Reasoning(_) => None,
                ThinkingTimelineRecord::Tool(tool) => Some(tool),
                ThinkingTimelineRecord::MessageBoundary => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].kind, ToolStreamEventKind::Use);
        assert_eq!(tools[0].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(tools[1].kind, ToolStreamEventKind::Result);
        assert_eq!(tools[1].tool_call_id.as_deref(), Some("call_x"));
        assert_eq!(tools[2].kind, ToolStreamEventKind::Result);
        assert_eq!(tools[2].tool_call_id, None);
    }

    #[test]
    fn thinking_timeline_inserts_blank_breaks_between_message_tool_groups() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "First message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "command_execution".to_string(),
            text: "bash: rg --files".to_string(),
            tool_call_id: Some("item_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "command_execution".to_string(),
            text: "bash: sed -n '1,260p' README.md".to_string(),
            tool_call_id: Some("item_2".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\n\nSecond message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "command_execution".to_string(),
            text: "bash: sed -n '1,260p' src/ui.rs".to_string(),
            tool_call_id: Some("item_3".to_string()),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(44, 44, 44);
        let rows = left.iter().map(|line| line.kind).collect::<Vec<_>>();
        let first_tool_idx = rows
            .iter()
            .position(|kind| *kind == ThinkingLineKind::ToolResult)
            .expect("first tool row");
        let second_group_tool_idx = rows
            .iter()
            .rposition(|kind| *kind == ThinkingLineKind::ToolResult)
            .expect("second tool row");
        assert!(second_group_tool_idx > first_tool_idx);
        assert!(
            rows[first_tool_idx + 1..second_group_tool_idx]
                .iter()
                .any(|kind| *kind == ThinkingLineKind::Blank),
            "tool groups from separate message chunks should be separated by a blank timeline row"
        );
    }

    #[test]
    fn thinking_timeline_detects_split_separator_across_token_chunks() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "First message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "command_execution".to_string(),
            text: "bash: rg --files".to_string(),
            tool_call_id: Some("item_1".to_string()),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\n".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\nSecond message.".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::ToolEvent {
            agent_idx: 0,
            kind: ToolStreamEventKind::Result,
            tool_type: "command_execution".to_string(),
            text: "bash: sed -n '1,260p' src/ui.rs".to_string(),
            tool_call_id: Some("item_2".to_string()),
        });

        let (left, _mid, _right) = state.build_tri_pane_rows(44, 44, 44);
        let rows = left.iter().map(|line| line.kind).collect::<Vec<_>>();
        let first_tool_idx = rows
            .iter()
            .position(|kind| *kind == ThinkingLineKind::ToolResult)
            .expect("first tool row");
        let second_tool_idx = rows
            .iter()
            .rposition(|kind| *kind == ThinkingLineKind::ToolResult)
            .expect("second tool row");
        assert!(
            rows[first_tool_idx + 1..second_tool_idx]
                .iter()
                .any(|kind| *kind == ThinkingLineKind::Blank),
            "split newline separators across tokens should still create a timeline break"
        );
    }

    #[test]
    fn tri_pane_main_text_preserves_leading_space_across_token_chunks() {
        let mut state = test_state();
        state.thinking_expanded = true;
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "Hello".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: " world".to_string(),
        });

        let (_left, mid, _right) = state.build_tri_pane_rows(44, 80, 44);
        let joined = mid.join("\n");
        assert!(
            joined.contains("0▎ Hello world"),
            "chunked token spaces should be preserved in tri-pane conversation rows"
        );
    }

    #[test]
    fn turn_done_flushes_pending_newline_only_token_chunk() {
        let mut state = test_state();
        state.apply_conversation_event(ConversationEvent::TurnStart { agent_idx: 0 });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "Hello".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::Token {
            agent_idx: 0,
            text: "\n".to_string(),
        });
        state.apply_conversation_event(ConversationEvent::TurnDone { agent_idx: 0 });

        let turn = state.turns_log.last().expect("turn exists");
        assert_eq!(turn.main_text, "Hello\n");
        assert_eq!(turn.main_chunks, vec!["Hello\n".to_string()]);
        assert_eq!(turn.pending_leading_newlines, 0);
    }

    #[test]
    fn fit_to_display_width_respects_wide_emoji_cells() {
        assert_eq!(fit_to_display_width("🌱 Basil", 1), "");
        assert_eq!(fit_to_display_width("🌱 Basil", 2), "🌱");
        assert_eq!(fit_to_display_width("🌱 Basil", 8), "🌱 Basil");
    }

    #[test]
    fn box_title_layout_uses_display_width_for_dash_fill() {
        let (_title, dashes) = box_title_layout("🌱 Basil Thinking", 28);
        assert_eq!(dashes, 6);
    }

    #[test]
    fn chat_body_padding_is_disabled_outside_directional_mode() {
        assert_eq!(chat_body_padding(0, 24, false), (0, 0));
        assert_eq!(chat_body_padding(1, 24, false), (0, 0));
    }

    #[test]
    fn chat_body_padding_is_agent_specific_when_directional() {
        assert_eq!(chat_body_padding(0, 24, true), (0, 8));
        assert_eq!(chat_body_padding(1, 24, true), (8, 0));
    }

    #[test]
    fn wrap_line_preserves_basil_gutter_prefix_for_directional_mode() {
        let wrapped = wrap_line("1▎ one two three", 12, true);
        assert!(wrapped.iter().all(|line| line.starts_with("1▎ ")));
    }

    #[test]
    fn wrap_line_preserves_aria_gutter_prefix_for_directional_mode() {
        let wrapped = wrap_line("0▎ one two three", 12, true);
        assert!(wrapped.iter().all(|line| line.starts_with("0▎ ")));
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = Vec::new();
        let bytes = input.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("stripped ansi should remain utf8")
    }

    #[test]
    fn render_conv_line_applies_basil_left_padding_once() {
        let mut out = Vec::new();
        render_conv_line(&mut out, "1▎ hi", 20, true).expect("render succeeds");
        let rendered = String::from_utf8(out).expect("utf8 render output");
        let visible = strip_ansi(&rendered);
        assert_eq!(visible.chars().count(), 20);
        assert_eq!(visible, "      ▎ hi          ");
    }

    #[test]
    fn render_conv_line_applies_aria_right_padding_once() {
        let mut out = Vec::new();
        render_conv_line(&mut out, "0▎ hi", 20, true).expect("render succeeds");
        let rendered = String::from_utf8(out).expect("utf8 render output");
        let visible = strip_ansi(&rendered);
        assert_eq!(visible.chars().count(), 20);
        assert_eq!(visible, " ▎ hi               ");
    }

    #[test]
    fn render_conv_line_shifts_basil_turn_header_with_lane_inset() {
        let header = "Basil";
        let mut out = Vec::new();
        render_conv_line(&mut out, header, 20, true).expect("render succeeds");
        let rendered = String::from_utf8(out).expect("utf8 render output");
        let visible = strip_ansi(&rendered);
        let body_width = 20usize.saturating_sub(3);
        let (left_pad, right_pad) = chat_body_padding(1, body_width, true);
        let content_width = body_width.saturating_sub(left_pad + right_pad);
        let expected = format!(
            "{} ▎ {}{}",
            " ".repeat(left_pad),
            pad_to_width(header, content_width),
            " ".repeat(right_pad)
        );
        assert_eq!(visible.chars().count(), 20);
        assert_eq!(visible, expected);
    }

    #[test]
    fn render_conv_line_shifts_aria_turn_header_with_gutter_in_directional_mode() {
        let header = "Aria";
        let mut out = Vec::new();
        render_conv_line(&mut out, header, 20, true).expect("render succeeds");
        let rendered = String::from_utf8(out).expect("utf8 render output");
        let visible = strip_ansi(&rendered);
        let body_width = 20usize.saturating_sub(3);
        let (left_pad, right_pad) = chat_body_padding(0, body_width, true);
        let content_width = body_width.saturating_sub(left_pad + right_pad);
        let expected = format!(
            "{} ▎ {}{}",
            " ".repeat(left_pad),
            pad_to_width(header, content_width),
            " ".repeat(right_pad)
        );
        assert_eq!(visible.chars().count(), 20);
        assert_eq!(visible, expected);
    }

    #[test]
    fn render_conv_line_keeps_plain_turn_header_in_non_directional_mode() {
        let header = "Basil";
        let mut out = Vec::new();
        render_conv_line(&mut out, header, 20, false).expect("render succeeds");
        let rendered = String::from_utf8(out).expect("utf8 render output");
        let visible = strip_ansi(&rendered);
        assert_eq!(visible.chars().count(), 20);
        assert_eq!(visible, pad_to_width(header, 20));
    }

    #[test]
    fn render_conv_line_keeps_basil_gutter_aligned_on_wrapped_lines() {
        let wrapped = wrap_line("1▎ one two three four five six", 14, true);
        assert!(wrapped.len() > 1, "expected wrapped continuation lines");

        let body_width = 20usize.saturating_sub(3);
        let (left_pad, _) = chat_body_padding(1, body_width, true);
        let expected_prefix = format!("{}▎ ", " ".repeat(left_pad + 1));

        for line in wrapped {
            let mut out = Vec::new();
            render_conv_line(&mut out, &line, 20, true).expect("render succeeds");
            let rendered = String::from_utf8(out).expect("utf8 render output");
            let visible = strip_ansi(&rendered);
            assert_eq!(visible.chars().count(), 20);
            assert!(
                visible.starts_with(&expected_prefix),
                "expected Basil gutter to move with lane inset; got: {visible:?}"
            );
        }
    }
}
