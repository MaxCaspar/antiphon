pub mod agent;
pub mod app;
pub mod audit;
pub mod cli;
pub mod conversation;
pub mod error;
pub mod output;
pub mod tmux;
pub mod ui;
pub mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::cli::Cli;

fn default_home_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("antiphon")
}

fn dir_is_writable(path: &std::path::Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }

    let probe = path.join(".antiphon-write-test");
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

/// Config/data home for antiphon.
///
/// Resolution order:
/// 1. `ANTIPHON_HOME` env var (if set)
/// 2. Platform config dir: `~/.config/antiphon` (Linux/macOS), `%APPDATA%\antiphon` (Windows)
/// 3. `./.antiphon` if the platform config dir is not writable
pub fn home_dir() -> PathBuf {
    if let Ok(val) = std::env::var("ANTIPHON_HOME") {
        return PathBuf::from(val);
    }

    let preferred = default_home_dir();
    if dir_is_writable(&preferred) {
        return preferred;
    }

    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".antiphon")
}

pub async fn run(args: Cli) -> Result<()> {
    app::run(args).await.map_err(anyhow::Error::from)
}

pub async fn run_cli() -> i32 {
    // Ensure the resolved runtime home exists before loading .env or writing settings.
    let _ = std::fs::create_dir_all(home_dir());
    // Load .env from the current working directory first, then the resolved runtime home.
    // Runtime-home values take precedence (loaded last), so explicit overrides win.
    let _ = dotenvy::dotenv();
    let _ = dotenvy::from_path(home_dir().join(".env"));
    let args = Cli::parse();
    let output = args.output;
    match run(args).await {
        Ok(()) => 0,
        Err(err) => {
            output::render::eprint_error(&err, output);
            1
        }
    }
}
