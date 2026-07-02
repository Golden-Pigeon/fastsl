//! Read-only-safe view-preference overlay (T6-write).
//!
//! The Vue dashboard mutates chart pin/hide (`chart.status`), namespace collapse
//! (`namespace.opened` / project|experiment `pinned_opened`/`hidden_opened`) and
//! experiment visibility (`experiment.show`). swanboard writes these into
//! `runs.swanlab`. fastsl must never touch that live SDK database, so the overrides
//! are persisted to a JSON sidecar and merged on top of the DB values at read time.
//!
//! Invariant: an **empty** overlay is a no-op — merged output is byte-identical to the
//! raw DB reads verified in T2–T5.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Sidecar filename stored alongside the swanlog dir (never inside `runs.swanlab`).
pub const SIDECAR_NAME: &str = "fastsl-ui.json";

/// Persisted override maps. All keyed by the DB row id; absence means "use the DB value".
#[derive(Default, Serialize, Deserialize, Clone)]
pub struct UiOverlay {
    #[serde(default)]
    pub chart_status: HashMap<i64, i64>,
    #[serde(default)]
    pub namespace_opened: HashMap<i64, i64>,
    #[serde(default)]
    pub experiment_show: HashMap<i64, i64>,
    #[serde(default)]
    pub experiment_pinned_opened: HashMap<i64, i64>,
    #[serde(default)]
    pub experiment_hidden_opened: HashMap<i64, i64>,
    #[serde(default)]
    pub project_pinned_opened: HashMap<i64, i64>,
    #[serde(default)]
    pub project_hidden_opened: HashMap<i64, i64>,
}

impl UiOverlay {
    /// Overridden chart status, or the DB value when no override exists.
    pub fn chart_status(&self, id: i64, db_value: i64) -> i64 {
        self.chart_status.get(&id).copied().unwrap_or(db_value)
    }

    /// Overridden namespace `opened`, applied to a JSON value if present.
    pub fn apply_namespace_opened(&self, id: i64, db_value: Value) -> Value {
        match self.namespace_opened.get(&id) {
            Some(v) => Value::from(*v),
            None => db_value,
        }
    }

    /// Overridden experiment `show`, applied to a JSON value if present.
    pub fn apply_experiment_show(&self, id: i64, db_value: Value) -> Value {
        match self.experiment_show.get(&id) {
            Some(v) => Value::from(*v),
            None => db_value,
        }
    }

    pub fn apply_experiment_pinned(&self, id: i64, db_value: Value) -> Value {
        match self.experiment_pinned_opened.get(&id) {
            Some(v) => Value::from(*v),
            None => db_value,
        }
    }

    pub fn apply_experiment_hidden(&self, id: i64, db_value: Value) -> Value {
        match self.experiment_hidden_opened.get(&id) {
            Some(v) => Value::from(*v),
            None => db_value,
        }
    }

    pub fn apply_project_pinned(&self, id: i64, db_value: Value) -> Value {
        match self.project_pinned_opened.get(&id) {
            Some(v) => Value::from(*v),
            None => db_value,
        }
    }

    pub fn apply_project_hidden(&self, id: i64, db_value: Value) -> Value {
        match self.project_hidden_opened.get(&id) {
            Some(v) => Value::from(*v),
            None => db_value,
        }
    }
}

/// Thread-safe overlay store with JSON persistence.
pub struct UiState {
    path: PathBuf,
    inner: RwLock<UiOverlay>,
    write_lock: Mutex<()>,
}

impl UiState {
    /// Loads the sidecar at `path` (empty overlay if missing or unparseable).
    pub fn load(path: PathBuf) -> Self {
        let overlay = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<UiOverlay>(&s).ok())
            .unwrap_or_default();
        UiState {
            path,
            inner: RwLock::new(overlay),
            write_lock: Mutex::new(()),
        }
    }

    /// Returns a cheap clone of the current overlay for merging into a read response.
    pub fn snapshot(&self) -> UiOverlay {
        self.inner.read().unwrap().clone()
    }

    /// Applies `f` to a cloned overlay, persists it, then swaps memory only after disk succeeds.
    fn mutate<F: FnOnce(&mut UiOverlay)>(&self, f: F) -> Result<()> {
        let _write_guard = self.write_lock.lock().unwrap();
        let mut next = self.inner.read().unwrap().clone();
        f(&mut next);
        self.persist(&next)?;
        *self.inner.write().unwrap() = next;
        Ok(())
    }

    /// Atomic write: serialize to a unique temp file in the same dir, then rename over the target.
    fn persist(&self, overlay: &UiOverlay) -> Result<()> {
        let json = serde_json::to_vec_pretty(overlay)?;
        let dir = self
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".fastsl-ui-")
            .tempfile_in(dir)?;
        tmp.write_all(&json)?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(&self.path)?;
        Ok(())
    }

    pub fn set_chart_status(&self, chart_id: i64, status: i64) -> Result<()> {
        self.mutate(|o| {
            o.chart_status.insert(chart_id, status);
        })
    }

    pub fn set_namespace_opened(&self, namespace_id: i64, opened: i64) -> Result<()> {
        self.mutate(|o| {
            o.namespace_opened.insert(namespace_id, opened);
        })
    }

    pub fn set_experiment_show(&self, experiment_id: i64, show: i64) -> Result<()> {
        self.mutate(|o| {
            o.experiment_show.insert(experiment_id, show);
        })
    }

    pub fn set_experiment_pinned(&self, experiment_id: i64, opened: i64) -> Result<()> {
        self.mutate(|o| {
            o.experiment_pinned_opened.insert(experiment_id, opened);
        })
    }

    pub fn set_experiment_hidden(&self, experiment_id: i64, opened: i64) -> Result<()> {
        self.mutate(|o| {
            o.experiment_hidden_opened.insert(experiment_id, opened);
        })
    }

    pub fn set_project_pinned(&self, project_id: i64, opened: i64) -> Result<()> {
        self.mutate(|o| {
            o.project_pinned_opened.insert(project_id, opened);
        })
    }

    pub fn set_project_hidden(&self, project_id: i64, opened: i64) -> Result<()> {
        self.mutate(|o| {
            o.project_hidden_opened.insert(project_id, opened);
        })
    }
}

/// swanboard truthiness: `1 if value else 0` (JSON bool, non-zero number, non-empty string).
pub fn truthy_to_int(v: &Value) -> i64 {
    let truthy = match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    };
    if truthy {
        1
    } else {
        0
    }
}
