use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "antiphon",
    version,
    about = "Two AI agents in call-and-response dialogue"
)]
pub struct Cli {
    #[arg(long = "agent-a", default_value = "claude")]
    pub agent_a: String,

    #[arg(long = "agent-b", default_value = "claude")]
    pub agent_b: String,

    #[arg(long, default_value_t = 10)]
    pub turns: usize,

    #[arg(long, default_value_t = false)]
    pub debug: bool,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    #[arg(long)]
    pub audit_log: Option<PathBuf>,

    #[arg(long, default_value_t = false)]
    pub quiet: bool,

#[arg(last = true)]
    pub initial_prompt: Option<String>,
}
