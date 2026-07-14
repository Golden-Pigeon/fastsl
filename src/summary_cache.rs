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

/// Filename prefix for the per-logdir persistent summary cache stored in `--cache-dir`.
/// The full name is `{prefix}-{basename}-{hash8}.json` so multiple logdirs can safely
/// share one cache dir without clobbering each other (see `cache_file_name`).
pub const SUMMARY_CACHE_PREFIX: &str = "fastsl-summary-cache";

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
            dir.join(cache_file_name(&logdir_key, logdir))
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

/// Per-logdir cache filename: `{prefix}-{basename}-{hash8}.json`.
///
/// `basename` is the logdir's last path component, sanitized and length-capped so the name
/// stays readable and bounded even for very long/deep logdir paths. `hash8` is a deterministic
/// FNV-1a over the canonical logdir path, which guarantees uniqueness (and disambiguates two
/// logdirs that share a basename). Stability across runs/toolchains is what lets a restart find
/// the same cache file.
fn cache_file_name(logdir_key: &str, logdir: &Path) -> String {
    let basename = logdir
        .file_name()
        .map(|s| sanitize_basename(&s.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    format!(
        "{SUMMARY_CACHE_PREFIX}-{basename}-{:08x}.json",
        fnv1a_32(logdir_key.as_bytes())
    )
}

/// Keeps `[A-Za-z0-9._-]`, replaces every other char with `_`, and caps length at 40 bytes so a
/// deep/long final path segment cannot produce an unwieldy filename.
fn sanitize_basename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}

/// Deterministic 32-bit FNV-1a (no dependency, stable across runs) for the filename suffix.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
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
    fn different_logdirs_get_separate_cache_files_and_keep_their_own_hits() {
        let cache_dir = unique_dir("summary-cache-logdir-isolation");
        let logdir_a = cache_dir.join("swanlog-a");
        let logdir_b = cache_dir.join("swanlog-b");
        std::fs::create_dir_all(&logdir_a).unwrap();
        std::fs::create_dir_all(&logdir_b).unwrap();

        let cache_a = SummaryCache::new(100, Some(&cache_dir), &logdir_a);
        cache_a.insert("run-1/0/123/456".to_string(), Arc::new(json!(42)));
        cache_a.flush().unwrap();

        let cache_b = SummaryCache::new(100, Some(&cache_dir), &logdir_b);
        cache_b.insert("run-2/0/789/012".to_string(), Arc::new(json!(7)));
        cache_b.flush().unwrap();

        // Two logdirs sharing one cache-dir must land in distinct files, not clobber each other.
        let cache_files = std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(super::SUMMARY_CACHE_PREFIX)
            })
            .count();
        assert_eq!(
            cache_files, 2,
            "each logdir should own a distinct cache file"
        );

        // Each reloads only its own value and never sees the other logdir's key.
        let reload_a = SummaryCache::new(100, Some(&cache_dir), &logdir_a);
        assert_eq!(*reload_a.get("run-1/0/123/456").unwrap(), json!(42));
        assert!(reload_a.get("run-2/0/789/012").is_none());

        let reload_b = SummaryCache::new(100, Some(&cache_dir), &logdir_b);
        assert_eq!(*reload_b.get("run-2/0/789/012").unwrap(), json!(7));
        assert!(reload_b.get("run-1/0/123/456").is_none());

        std::fs::remove_dir_all(cache_dir).ok();
    }
}
