use std::io::Write;

use anyhow::Error;
use serde::Serialize;

use crate::cli::OutputFormat;
use crate::error::AppError;

#[derive(Debug, Serialize)]
struct JsonErrorPayload<'a> {
    error: JsonErrorInner<'a>,
}

#[derive(Debug, Serialize)]
struct JsonErrorInner<'a> {
    code: &'a str,
    message: String,
}

pub fn eprint_error(err: &Error, output: OutputFormat) {
    match output {
        OutputFormat::Text => {
            let _ = writeln!(std::io::stderr(), "error: {err}");
        }
        OutputFormat::Json => {
            let app = err.downcast_ref::<AppError>();
            let (code, message) = match app {
                Some(e) => (e.code().as_str(), e.to_string()),
                None => ("internal_error", err.to_string()),
            };

            let payload = JsonErrorPayload {
                error: JsonErrorInner { code, message },
            };

            match serde_json::to_string_pretty(&payload) {
                Ok(s) => {
                    let _ = writeln!(std::io::stdout(), "{s}");
                }
                Err(ser_err) => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "error: failed to serialize JSON error payload: {ser_err}"
                    );
                }
            }
        }
    }
}
