use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use mgd_common::util::unix_timestamp_secs;

const MAX_EVENTS: usize = 100;

pub struct ActionEvent {
    pub timestamp: u64,
    pub action: mgd_common::logger::LogAction,
    pub pid: u32,
    pub name: String,
    pub detail: String,
}

pub type EventLog = Arc<Mutex<VecDeque<ActionEvent>>>;

pub fn new_log() -> EventLog {
    Arc::new(Mutex::new(VecDeque::with_capacity(MAX_EVENTS)))
}

pub fn push(log: &EventLog, action: mgd_common::logger::LogAction, pid: u32, name: &str, detail: &str) {
    let event = ActionEvent {
        timestamp: unix_timestamp_secs(),
        action,
        pid,
        name: name.to_string(),
        detail: detail.to_string(),
    };
    let mut q = log.lock().unwrap();
    if q.len() >= MAX_EVENTS {
        q.pop_front();
    }
    q.push_back(event);
}
