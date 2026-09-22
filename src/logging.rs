use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::{SecondsFormat, Utc};
use serde::Serialize;

const MAX_MEMORY_ENTRIES: usize = 1_000;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug)]
pub struct AppLogger {
    entries: Mutex<VecDeque<LogEntry>>,
    next_id: AtomicU64,
    file_path: PathBuf,
}

impl AppLogger {
    pub fn new(data_dir: &Path) -> std::io::Result<Self> {
        let log_dir = data_dir.join("logs");
        fs::create_dir_all(&log_dir)?;
        Ok(Self {
            entries: Mutex::new(VecDeque::with_capacity(MAX_MEMORY_ENTRIES)),
            next_id: AtomicU64::new(1),
            file_path: log_dir.join("app.log"),
        })
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.write("INFO", message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.write("WARN", message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.write("ERROR", message.as_ref());
    }

    pub fn write(&self, level: &str, message: &str) {
        let entry = LogEntry {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            level: level.to_owned(),
            message: redact_secrets(message),
        };

        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() == MAX_MEMORY_ENTRIES {
                entries.pop_front();
            }
            entries.push_back(entry.clone());
        }

        let _ = self.append_to_file(&entry);
    }

    pub fn tail(&self, after: Option<u64>, limit: usize) -> Vec<LogEntry> {
        let limit = limit.clamp(1, MAX_MEMORY_ENTRIES);
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        let filtered: Vec<_> = entries
            .iter()
            .filter(|entry| after.is_none_or(|id| entry.id > id))
            .cloned()
            .collect();
        let start = filtered.len().saturating_sub(limit);
        filtered[start..].to_vec()
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    fn append_to_file(&self, entry: &LogEntry) -> std::io::Result<()> {
        if self
            .file_path
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
        {
            let backup = self.file_path.with_extension("log.1");
            let _ = fs::copy(&self.file_path, backup);
            let _ = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.file_path)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)?;
        writeln!(
            file,
            "{} [{}] {}",
            entry.timestamp, entry.level, entry.message
        )
    }
}

pub(crate) fn redact_secrets(input: &str) -> String {
    let mut output = input.to_owned();
    for marker in [
        "Authorization: Bearer ",
        "authorization: bearer ",
        "CIVITAI_TOKEN=",
    ] {
        let mut search_from = 0;
        while let Some(relative_start) = output[search_from..].find(marker) {
            let start = search_from + relative_start;
            let value_start = start + marker.len();
            let value_end = output[value_start..]
                .find(char::is_whitespace)
                .map_or(output.len(), |offset| value_start + offset);
            output.replace_range(value_start..value_end, "[REDACTED]");
            search_from = value_start + "[REDACTED]".len();
        }
    }
    if let Some(token) = std::env::var_os("CIVITAI_TOKEN") {
        let token = token.to_string_lossy();
        if token.len() >= 8 {
            output = output.replace(token.as_ref(), "[REDACTED]");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_redacts_bearer_and_civitai_tokens() {
        let value = redact_secrets("Authorization: Bearer abc123 CIVITAI_TOKEN=secret-value next");
        assert!(!value.contains("abc123"));
        assert!(!value.contains("secret-value"));
        assert!(value.contains("[REDACTED]"));
    }
}
