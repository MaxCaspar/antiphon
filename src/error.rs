use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NonInteractive,
    SpawnFailed,
    ParseFailed,
    NonZeroExit,
    Cancelled,
    InvalidInput,
    Io,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NonInteractive => "non_interactive",
            Self::SpawnFailed => "spawn_failed",
            Self::ParseFailed => "parse_failed",
            Self::NonZeroExit => "non_zero_exit",
            Self::Cancelled => "cancelled",
            Self::InvalidInput => "invalid_input",
            Self::Io => "io_error",
        }
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("interactive terminal required; rerun with --debug for deterministic non-TUI mode")]
    NonInteractive,

    #[error("failed to spawn agent command '{cmd}': {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("agent output parse failed for '{agent}': {message}")]
    Parse { agent: String, message: String },

    #[error("agent command '{cmd}' exited with status {status}")]
    NonZeroExit { cmd: String, status: i32 },

    #[error("conversation cancelled")]
    Cancelled,

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl AppError {
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::NonInteractive => ErrorCode::NonInteractive,
            Self::Spawn { .. } => ErrorCode::SpawnFailed,
            Self::Parse { .. } => ErrorCode::ParseFailed,
            Self::NonZeroExit { .. } => ErrorCode::NonZeroExit,
            Self::Cancelled => ErrorCode::Cancelled,
            Self::InvalidInput(..) => ErrorCode::InvalidInput,
            Self::Io(..) => ErrorCode::Io,
        }
    }
}
