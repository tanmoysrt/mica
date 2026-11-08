//! Long operations of the daemon, with done and total counts, so that the
//! CLI can show a bar. `mica status --json` lists them under `tasks`.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Tasks {
    running: Mutex<HashMap<String, Arc<Task>>>,
}

pub struct Task {
    what: &'static str,
    pub done: AtomicU64,
    pub total: AtomicU64,
}

/// Removes its task when it drops, also at an error.
pub struct Running<'a> {
    tasks: &'a Tasks,
    disk: String,
    pub task: Arc<Task>,
}

impl Tasks {
    /// `what` names the work, such as "copy". The key is the disk it makes or changes.
    pub fn start(&self, disk: &str, what: &'static str) -> Running<'_> {
        let task = Arc::new(Task { what, done: AtomicU64::new(0), total: AtomicU64::new(0) });
        self.running.lock().unwrap().insert(disk.to_string(), task.clone());
        Running { tasks: self, disk: disk.to_string(), task }
    }

    pub fn list(&self) -> Value {
        let running = self.running.lock().unwrap();
        let mut tasks: Vec<Value> = running
            .iter()
            .map(|(disk, task)| {
                json!({ "disk": disk, "what": task.what, "done": task.done.load(Ordering::Relaxed), "total": task.total.load(Ordering::Relaxed) })
            })
            .collect();
        tasks.sort_by(|a, b| a["disk"].as_str().cmp(&b["disk"].as_str()));
        Value::Array(tasks)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.tasks.running.lock().unwrap().remove(&self.disk);
    }
}
