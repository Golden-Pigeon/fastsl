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
/// Maximum number of comparison groups persisted in one sidecar.
pub const MAX_GROUPS: usize = 100;
/// Maximum experiments in one comparison group.
pub const MAX_GROUP_MEMBERS: usize = 500;

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
    /// fastsl-only: experiment display alias, keyed by `run_id` (stable across DB rebuilds).
    /// Absence (or an empty value) means "show the DB `name`".
    #[serde(default)]
    pub experiment_alias: HashMap<String, String>,
    /// fastsl-only: user-defined comparison groups (ordered).
    #[serde(default)]
    pub groups: Vec<Group>,
    /// Monotonic group-id counter; never reused after a delete.
    #[serde(default)]
    pub next_group_id: i64,
}

/// A user-defined comparison group. `members` are experiment `run_id`s, order preserved.
#[derive(Serialize, Deserialize, Clone)]
pub struct Group {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
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

    /// Non-empty alias for an experiment `run_id`, or `None` to fall back to the DB name.
    pub fn alias(&self, run_id: &str) -> Option<&str> {
        self.experiment_alias
            .get(run_id)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
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
    fn mutate<T, F: FnOnce(&mut UiOverlay) -> T>(&self, f: F) -> Result<T> {
        let _write_guard = self.write_lock.lock().unwrap();
        let mut next = self.inner.read().unwrap().clone();
        let result = f(&mut next);
        self.persist(&next)?;
        *self.inner.write().unwrap() = next;
        Ok(result)
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

    // ---- fastsl-only: aliases + comparison groups (persisted to the same sidecar) ----

    /// Sets or clears an experiment's alias. `Some(non-empty)` sets it; `None` or an empty
    /// string removes the entry so the experiment reverts to its DB `name`.
    pub fn set_experiment_alias(&self, run_id: &str, alias: Option<String>) -> Result<()> {
        self.mutate(|o| match alias {
            Some(a) if !a.is_empty() => {
                o.experiment_alias.insert(run_id.to_string(), a);
            }
            _ => {
                o.experiment_alias.remove(run_id);
            }
        })
    }

    /// Creates a group with a fresh monotonic id. Returns `None` after the group limit is reached.
    pub fn create_group(&self, name: String, members: Vec<String>) -> Result<Option<Group>> {
        self.mutate(|o| {
            if o.groups.len() >= MAX_GROUPS {
                return None;
            }
            let id = o.next_group_id.max(1);
            o.next_group_id = id + 1;
            let g = Group {
                id,
                name,
                members: dedup_preserve(members),
            };
            o.groups.push(g.clone());
            Some(g)
        })
    }

    /// Renames a group. Returns `false` if no group has that id.
    pub fn rename_group(&self, gid: i64, name: String) -> Result<bool> {
        let mut found = false;
        self.mutate(|o| {
            if let Some(g) = o.groups.iter_mut().find(|g| g.id == gid) {
                g.name = name;
                found = true;
            }
        })?;
        Ok(found)
    }

    /// Deletes a group. Returns `false` if no group has that id.
    pub fn delete_group(&self, gid: i64) -> Result<bool> {
        let mut found = false;
        self.mutate(|o| {
            let before = o.groups.len();
            o.groups.retain(|g| g.id != gid);
            found = o.groups.len() != before;
        })?;
        Ok(found)
    }

    /// Adds a member (run_id) to a group, de-duplicated. Returns `false` if the group is missing
    /// or already at the member limit.
    pub fn add_member(&self, gid: i64, run_id: &str) -> Result<bool> {
        self.mutate(|o| {
            let Some(g) = o.groups.iter_mut().find(|g| g.id == gid) else {
                return false;
            };
            if g.members.iter().any(|m| m == run_id) {
                return true;
            }
            if g.members.len() >= MAX_GROUP_MEMBERS {
                return false;
            }
            g.members.push(run_id.to_string());
            true
        })
    }

    /// Removes a member (run_id) from a group. Returns `false` if the group is missing
    /// (removal is idempotent for a member that was not present).
    pub fn remove_member(&self, gid: i64, run_id: &str) -> Result<bool> {
        let mut found = false;
        self.mutate(|o| {
            if let Some(g) = o.groups.iter_mut().find(|g| g.id == gid) {
                g.members.retain(|m| m != run_id);
                found = true;
            }
        })?;
        Ok(found)
    }
}

/// Drops duplicate run_ids while preserving first-seen order.
fn dedup_preserve(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_sidecar(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("fastsl-uistate-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(SIDECAR_NAME)
    }

    #[test]
    fn alias_set_clear_and_reload_from_disk() {
        let path = temp_sidecar("alias");
        let ui = UiState::load(path.clone());

        ui.set_experiment_alias("run-abc", Some("baseline".into()))
            .unwrap();
        assert_eq!(ui.snapshot().alias("run-abc"), Some("baseline"));

        // Reload from disk proves persistence.
        let reloaded = UiState::load(path.clone());
        assert_eq!(reloaded.snapshot().alias("run-abc"), Some("baseline"));

        // Empty string clears → reverts to DB name (None).
        reloaded
            .set_experiment_alias("run-abc", Some(String::new()))
            .unwrap();
        assert_eq!(reloaded.snapshot().alias("run-abc"), None);
        assert_eq!(UiState::load(path).snapshot().alias("run-abc"), None);
    }

    #[test]
    fn group_crud_persists_and_ids_are_monotonic() {
        let path = temp_sidecar("groups");
        let ui = UiState::load(path.clone());

        let g1 = ui
            .create_group("baseline".into(), vec!["run-a".into(), "run-a".into()])
            .unwrap()
            .unwrap();
        let g2 = ui.create_group("ablation".into(), vec![]).unwrap().unwrap();
        assert_eq!(g1.id, 1);
        assert_eq!(g2.id, 2, "ids are monotonic");
        assert_eq!(g1.members, vec!["run-a"], "members de-duplicated");

        assert!(ui.add_member(g2.id, "run-b").unwrap());
        assert!(ui.add_member(g2.id, "run-b").unwrap(), "add is idempotent");
        assert!(ui.rename_group(g1.id, "baseline-v2".into()).unwrap());
        assert!(!ui.rename_group(999, "missing".into()).unwrap());

        // Delete g1; g2's id is never reused by the next create.
        assert!(ui.delete_group(g1.id).unwrap());
        let g3 = ui.create_group("third".into(), vec![]).unwrap().unwrap();
        assert_eq!(g3.id, 3, "deleted id 1 is not reused");

        // Reload and verify persisted shape.
        let groups = UiState::load(path).snapshot().groups;
        let ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![2, 3]);
        let g2_reloaded = groups.iter().find(|g| g.id == 2).unwrap();
        assert_eq!(g2_reloaded.name, "ablation");
        assert_eq!(g2_reloaded.members, vec!["run-b"]);
    }

    #[test]
    fn remove_member_reports_group_presence() {
        let path = temp_sidecar("remove-member");
        let ui = UiState::load(path);
        let g = ui
            .create_group("g".into(), vec!["run-a".into(), "run-b".into()])
            .unwrap()
            .unwrap();

        assert!(ui.remove_member(g.id, "run-a").unwrap());
        assert!(
            ui.remove_member(g.id, "run-a").unwrap(),
            "removing an absent member still returns true (group exists)"
        );
        assert!(
            !ui.remove_member(404, "run-a").unwrap(),
            "missing group returns false"
        );
        assert_eq!(ui.snapshot().groups[0].members, vec!["run-b"]);
    }
}
