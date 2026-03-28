use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct TmuxPanes {
    pane_a: String,
    pane_b: String,
}

pub fn open_agent_windows(
    conversation_id: &str,
    agent_a_log_path: &Path,
    agent_b_log_path: &Path,
) -> std::io::Result<Option<TmuxPanes>> {
    if std::env::var_os("TMUX").is_none() {
        return Ok(None);
    }

    let original_pane = current_pane_id().ok();

    let agent_a_abs = absolutize_path(agent_a_log_path)?;
    let agent_b_abs = absolutize_path(agent_b_log_path)?;

    let command_a = watch_command(
        short_conv_id(conversation_id).as_str(),
        "A",
        agent_a_abs.as_str(),
    );
    let command_b = watch_command(
        short_conv_id(conversation_id).as_str(),
        "B",
        agent_b_abs.as_str(),
    );

    // Layout in same window:
    // top: dialogue pane
    // bottom-left: agent A log
    // bottom-right: agent B log
    let pane_a = split_bottom_pane(&command_a)?;
    let pane_b = split_right_pane(&pane_a, &command_b)?;

    if let Some(pane_id) = original_pane {
        let _ = Command::new("tmux")
            .arg("select-pane")
            .arg("-t")
            .arg(pane_id)
            .status();
    }

    Ok(Some(TmuxPanes { pane_a, pane_b }))
}

fn split_bottom_pane(command: &str) -> std::io::Result<String> {
    let out = Command::new("tmux")
        .arg("split-window")
        .arg("-v")
        .arg("-p")
        .arg("30")
        .arg("-P")
        .arg("-F")
        .arg("#{pane_id}")
        .arg("bash")
        .arg("-lc")
        .arg(command)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn split_right_pane(target: &str, command: &str) -> std::io::Result<String> {
    let out = Command::new("tmux")
        .arg("split-window")
        .arg("-h")
        .arg("-t")
        .arg(target)
        .arg("-p")
        .arg("50")
        .arg("-P")
        .arg("-F")
        .arg("#{pane_id}")
        .arg("bash")
        .arg("-lc")
        .arg(command)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn current_pane_id() -> std::io::Result<String> {
    let out = Command::new("tmux")
        .arg("display-message")
        .arg("-p")
        .arg("#{pane_id}")
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn short_conv_id(id: &str) -> String {
    id.chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn shell_single_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

fn absolutize_path(path: &Path) -> std::io::Result<String> {
    if path.is_absolute() {
        return Ok(path.display().to_string());
    }
    let cwd = std::env::current_dir()?;
    Ok(cwd.join(path).display().to_string())
}

fn watch_command(conv_short: &str, label: &str, log_path: &str) -> String {
    let path_q = shell_single_quote(log_path);
    let sed = colorize_sed();
    format!(
        "printf '\\033[1;37m\\n[antiphon {conv_short} {label}] %s\\033[0m\\n\\n' {path_q} ; \
tail -n +1 -F {path_q} | {sed}"
    )
}

fn colorize_sed() -> &'static str {
    "sed -u \
        -e \"s/^\\\\[exec\\\\]/\\\\x1b[36m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[raw\\\\]/\\\\x1b[2m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[token\\\\]/\\\\x1b[32m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[turn\\\\.start\\\\]/\\\\x1b[33m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[turn\\\\.done\\\\]/\\\\x1b[32m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[turn\\\\.error\\\\]/\\\\x1b[31m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[run\\\\.start\\\\]/\\\\x1b[34m&\\\\x1b[0m/\" \
        -e \"s/^\\\\[run\\\\.done\\\\]/\\\\x1b[35m&\\\\x1b[0m/\" \
        -e \"s/^prompt:/\\\\x1b[33m&\\\\x1b[0m/\""
}

pub fn close_panes(panes: &TmuxPanes) {
    let _ = Command::new("tmux")
        .arg("kill-pane")
        .arg("-t")
        .arg(&panes.pane_a)
        .status();
    let _ = Command::new("tmux")
        .arg("kill-pane")
        .arg("-t")
        .arg(&panes.pane_b)
        .status();
}
