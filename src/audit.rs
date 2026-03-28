use std::fs::{File, OpenOptions, create_dir_all};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

#[derive(Debug)]
pub struct AuditLogger {
    file: Mutex<File>,
}

impl AuditLogger {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn log(&self, value: Value) {
        let mut payload = value;
        if let Value::Object(ref mut obj) = payload {
            obj.insert("ts_ms".to_string(), json!(now_ms()));
        }

        let Ok(mut f) = self.file.lock() else {
            return;
        };
        let Ok(line) = serde_json::to_string(&payload) else {
            return;
        };
        let _ = writeln!(f, "{line}");
    }
}

#[derive(Debug)]
pub struct AuditSet {
    pub conversation_id: String,
    pub directory: PathBuf,
    run: AuditLogger,
    agent_a: AuditLogger,
    agent_b: AuditLogger,
    live_run: Mutex<File>,
    live_a: Mutex<File>,
    live_b: Mutex<File>,
}

impl AuditSet {
    pub fn create(base_dir: &Path) -> std::io::Result<Self> {
        let conversation_id = generate_conversation_id();
        let directory = base_dir.join(&conversation_id);
        create_dir_all(&directory)?;

        let run = AuditLogger::new(&directory.join("conversation.jsonl"))?;
        let agent_a = AuditLogger::new(&directory.join("agent_a.jsonl"))?;
        let agent_b = AuditLogger::new(&directory.join("agent_b.jsonl"))?;
        let live_run = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("live.log"))?;
        let live_a = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("agent_a_live.log"))?;
        let live_b = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("agent_b_live.log"))?;

        Ok(Self {
            conversation_id,
            directory,
            run,
            agent_a,
            agent_b,
            live_run: Mutex::new(live_run),
            live_a: Mutex::new(live_a),
            live_b: Mutex::new(live_b),
        })
    }

    pub fn log_run(&self, mut value: Value) {
        if let Value::Object(ref mut obj) = value {
            obj.insert(
                "conversation_id".to_string(),
                Value::String(self.conversation_id.clone()),
            );
        }
        self.run.log(value);
    }

    pub fn log_agent(&self, agent_idx: usize, mut value: Value) {
        if let Value::Object(ref mut obj) = value {
            obj.insert(
                "conversation_id".to_string(),
                Value::String(self.conversation_id.clone()),
            );
            obj.insert("agent_idx".to_string(), json!(agent_idx));
        }

        if agent_idx == 0 {
            self.agent_a.log(value);
        } else {
            self.agent_b.log(value);
        }
    }

    pub fn live_agent_path(&self, agent_idx: usize) -> PathBuf {
        if agent_idx == 0 {
            self.directory.join("agent_a_live.log")
        } else {
            self.directory.join("agent_b_live.log")
        }
    }

    pub fn live_line(&self, line: &str) {
        let Ok(mut f) = self.live_run.lock() else {
            return;
        };
        let _ = writeln!(f, "{line}");
    }

    pub fn live_agent_line(&self, agent_idx: usize, line: &str) {
        let lock = if agent_idx == 0 {
            &self.live_a
        } else {
            &self.live_b
        };
        let Ok(mut f) = lock.lock() else {
            return;
        };
        let _ = writeln!(f, "{line}");
    }
}

pub fn generate_conversation_id() -> String {
    let ts = now_ms();
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("conv-{}-{}-{}", ts, pid, seq)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
