use serde::Serialize;
use std::collections::VecDeque;
use std::time::Instant;

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u128,
}

impl LogEntry {
    pub fn new(method: &str, path: &str, status: u16, started: Instant) -> Self {
        Self {
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            method: method.to_string(),
            path: path.to_string(),
            status,
            ms: started.elapsed().as_millis(),
        }
    }
}

pub fn push(logs: &mut VecDeque<LogEntry>, entry: LogEntry) {
    if logs.len() >= 80 {
        logs.pop_front();
    }
    logs.push_back(entry);
}
