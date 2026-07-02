//! Persistent summary last-value cache (T7).
//!
//! Keys are the same identity already used by the summary reader:
//! `run_id/folder/last_shard_mtime_ns/last_shard_size`. A persisted hit is therefore safe:
//! if the writer appends or rewrites the shard, the key changes and the stale value is ignored.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use moka::sync::Cache;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SUMMARY_CACHE_NAME: &str = "fastsl-summary-cache.json";

#[derive(Default, Serialize, Deserialize, Clone)]
struct PersistedSummaryCache {
    logdir: String,
    #[serde(default)]
    entries: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct SummaryCache {
    memory: Cache<String, Arc<Value>>,
    path: Option<PathBuf>,
    persisted: Arc<RwLock<PersistedSummaryCache>>,
    flush_lock: Arc<Mutex<()>>,
}

impl SummaryCache {
    pub fn new(max_capacity: u64, cache_dir: Option<&Path>, logdir: &Path) -> Self {
        let memory = Cache::new(max_capacity);
        let logdir_key = canonical_key(logdir);
        let path = cache_dir.map(|dir| {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!("failed to create summary cache dir {:?}: {e}", dir);
            }
            dir.join(SUMMARY_CACHE_NAME)
        });

        let mut persisted = PersistedSummaryCache {
            logdir: logdir_key.clone(),
            entries: HashMap::new(),
        };
        if let Some(path) = &path {
            match std::fs::read_to_string(path)
                .ok()
                .and_then(|s| serde_json::from_str::<PersistedSummaryCache>(&s).ok())
            {
                Some(loaded) if loaded.logdir == logdir_key => {
                    for (key, value) in &loaded.entries {
                        memory.insert(key.clone(), Arc::new(value.clone()));
                    }
                    persisted = loaded;
                    tracing::info!(
                        "loaded {} summary cache entries from {:?}",
                        persisted.entries.len(),
                        path
                    );
                }
                Some(_) => {
                    tracing::info!(
                        "ignoring summary cache for a different logdir at {:?}",
                        path
                    );
                }
                None => {}
            }
        }

        Self {
            memory,
            path,
            persisted: Arc::new(RwLock::new(persisted)),
            flush_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn get(&self, key: &str) -> Option<Arc<Value>> {
        self.memory.get(key)
    }

    pub fn insert(&self, key: String, value: Arc<Value>) {
        self.memory.insert(key.clone(), value.clone());
        if self.path.is_some() {
            self.persisted
                .write()
                .unwrap()
                .entries
                .insert(key, (*value).clone());
        }
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let _flush_guard = self.flush_lock.lock().unwrap();
        let snapshot = self.persisted.read().unwrap().clone();
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(io::Error::other)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".fastsl-summary-cache-")
            .tempfile_in(dir)?;
        tmp.write_all(&bytes)?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(path)?;
        Ok(())
    }
}

fn canonical_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::SummaryCache;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("fastsl-{name}-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn persists_values_and_reloads_by_exact_identity_key() {
        let cache_dir = unique_dir("summary-cache-reload");
        let logdir = cache_dir.join("swanlog");
        std::fs::create_dir_all(&logdir).unwrap();

        let cache = SummaryCache::new(100, Some(&cache_dir), &logdir);
        cache.insert(
            "run-1/0/123/456".to_string(),
            Arc::new(json!({"loss": 0.25})),
        );
        cache.flush().unwrap();

        let reloaded = SummaryCache::new(100, Some(&cache_dir), &logdir);
        assert_eq!(
            *reloaded.get("run-1/0/123/456").unwrap(),
            json!({"loss": 0.25})
        );

        std::fs::remove_dir_all(cache_dir).ok();
    }

    #[test]
    fn ignores_persisted_values_for_a_different_logdir() {
        let cache_dir = unique_dir("summary-cache-logdir-isolation");
        let logdir_a = cache_dir.join("swanlog-a");
        let logdir_b = cache_dir.join("swanlog-b");
        std::fs::create_dir_all(&logdir_a).unwrap();
        std::fs::create_dir_all(&logdir_b).unwrap();

        let cache_a = SummaryCache::new(100, Some(&cache_dir), &logdir_a);
        cache_a.insert("run-1/0/123/456".to_string(), Arc::new(json!(42)));
        cache_a.flush().unwrap();

        let cache_b = SummaryCache::new(100, Some(&cache_dir), &logdir_b);
        assert!(cache_b.get("run-1/0/123/456").is_none());

        std::fs::remove_dir_all(cache_dir).ok();
    }
}
