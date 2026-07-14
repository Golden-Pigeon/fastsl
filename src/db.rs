//! Read-only reader over `runs.swanlab` (SQLite) plus the per-run config/metadata files.
//!
//! Reproduces swanboard's `get_project_info` / `get_experiment_info` response `data` shapes
//! (see `controller/project.py`, `controller/experiment.py`). Opened strictly read-only so the
//! live SwanLab SDK writer is never disturbed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use rusqlite::{types::ValueRef, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::summary_cache::SummaryCache;
use crate::uistate::UiOverlay;

/// Bytes read from the tail of a shard to recover its last line (scalar lines ~90B).
const TAIL_BYTES: u64 = 65536;

/// swanboard `COLOR_LIST` (utils/font.py). `dark` mirrors `light`.
const LIGHT_COLORS: [&str; 16] = [
    "#528d59", "#587ad2", "#c24d46", "#9cbe5d", "#6ebad3", "#dfb142", "#6d4ba4", "#8cc5b7",
    "#892d58", "#40877c", "#d0703c", "#d47694", "#e3b292", "#b15fbb", "#905f4a", "#989fa3",
];

fn color_list() -> Value {
    let arr: Vec<Value> = LIGHT_COLORS.iter().map(|c| json!(c)).collect();
    json!({ "light": arr, "dark": arr })
}

/// Open `runs.swanlab` read-only. WAL sidecars (-wal/-shm) are read transparently.
pub fn open_ro(logdir: &Path) -> Result<Connection> {
    let db = logdir.join("runs.swanlab");
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| anyhow!("open {db:?} read-only: {e}"))?;
    Ok(conn)
}

fn safe_join(base: &Path, parts: &[&str]) -> Option<std::path::PathBuf> {
    let mut path = base.to_path_buf();
    for part in parts {
        let part_path = Path::new(part);
        if part_path.is_absolute() {
            return None;
        }
        if !part_path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        path.push(part_path);
    }
    Some(path)
}

fn safe_run_path(logdir: &Path, run_id: &str, parts: &[&str]) -> Option<std::path::PathBuf> {
    let root = logdir.canonicalize().ok()?;
    let base = safe_join(logdir, &[run_id])?;
    let path = safe_join(&base, parts)?;
    let resolved = path.canonicalize().ok()?;
    if resolved.starts_with(&root) {
        Some(resolved)
    } else {
        None
    }
}

fn safe_read_to_string(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn safe_is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| !m.file_type().is_symlink() && m.is_dir())
        .unwrap_or(false)
}

fn safe_file_size(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| !m.file_type().is_symlink() && m.is_file())
        .map(|m| m.len())
}

fn vref_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::from(String::from_utf8_lossy(b).into_owned()),
    }
}

/// Parsed `files/config.yaml`; `None` if the file is absent (matches swanboard's existence check).
fn load_config(logdir: &Path, run_id: &str) -> Option<Value> {
    let p = safe_run_path(logdir, run_id, &["files", "config.yaml"])?;
    let s = safe_read_to_string(&p)?;
    Some(serde_yaml::from_str::<Value>(&s).unwrap_or(Value::Null))
}

/// Parsed `files/swanlab-metadata.json`; `{}` if absent or empty (matches swanboard).
fn load_meta(logdir: &Path, run_id: &str) -> Value {
    let Some(p) = safe_run_path(logdir, run_id, &["files", "swanlab-metadata.json"]) else {
        return json!({});
    };
    match safe_read_to_string(&p) {
        Some(s) if !s.trim().is_empty() => {
            serde_json::from_str::<Value>(&s).unwrap_or_else(|_| json!({}))
        }
        _ => json!({}),
    }
}

/// `GET /api/v1/project` — project fields + logdir + experiments[] + colors.
pub fn project_info(logdir: &Path, overlay: &UiOverlay) -> Result<Value> {
    let conn = open_ro(logdir)?;

    // Project row (Project.__dict__ order).
    let pcols = [
        "id",
        "name",
        "description",
        "sum",
        "charts",
        "more",
        "pinned_opened",
        "hidden_opened",
        "version",
        "create_time",
        "update_time",
    ];
    let mut data = query_one_object(&conn, "project", &pcols, "id=1", &[])?
        .ok_or_else(|| anyhow!("project not found"))?;

    data.insert("logdir".into(), json!(logdir.to_string_lossy()));

    // Experiments: full column set (model_to_dict), minus project_id, plus experiment_id + config.
    let ecols = [
        "id",
        "run_id",
        "name",
        "description",
        "sort",
        "status",
        "show",
        "light",
        "dark",
        "pinned_opened",
        "hidden_opened",
        "more",
        "version",
        "create_time",
        "finish_time",
        "update_time",
    ];
    let sql = format!(
        "SELECT {} FROM experiment WHERE project_id=1 ORDER BY id",
        ecols.join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut experiments: Vec<Value> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (i, c) in ecols.iter().enumerate() {
            obj.insert((*c).to_string(), vref_to_json(row.get_ref(i)?));
        }
        let id = obj.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let run_id = obj
            .get("run_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        obj.insert("experiment_id".into(), json!(id));
        if let Some(v) = obj.get_mut("show") {
            *v = overlay.apply_experiment_show(id, v.take());
        }
        if let Some(cfg) = load_config(logdir, &run_id) {
            obj.insert("config".into(), cfg);
        }
        experiments.push(Value::Object(obj));
    }

    data.insert("experiments".into(), Value::Array(experiments));
    data.insert("colors".into(), color_list());
    Ok(Value::Object(data))
}

/// `GET /api/v1/experiment/{id}` — experiment fields (no project_id) + config + system.
/// Returns `Ok(None)` when the experiment does not exist.
pub fn experiment_info(logdir: &Path, id: i64, overlay: &UiOverlay) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let cols = [
        "id",
        "run_id",
        "name",
        "description",
        "sort",
        "status",
        "show",
        "more",
        "version",
        "create_time",
        "update_time",
        "finish_time",
    ];
    let mut obj = match query_one_object(&conn, "experiment", &cols, "id=?1", &[&id])? {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Some(v) = obj.get_mut("show") {
        *v = overlay.apply_experiment_show(id, v.take());
    }
    let run_id = obj
        .get("run_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(cfg) = load_config(logdir, &run_id) {
        obj.insert("config".into(), cfg);
    }
    obj.insert("system".into(), load_meta(logdir, &run_id));
    Ok(Some(Value::Object(obj)))
}

/// Selects the given columns from one row of `table` matching `where_clause`.
fn query_one_object(
    conn: &Connection,
    table: &str,
    cols: &[&str],
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Option<Map<String, Value>>> {
    let sql = format!(
        "SELECT {} FROM {} WHERE {}",
        cols.join(","),
        table,
        where_clause
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(row) => {
            let mut obj = Map::new();
            for (i, c) in cols.iter().enumerate() {
                obj.insert((*c).to_string(), vref_to_json(row.get_ref(i)?));
            }
            Ok(Some(obj))
        }
        None => Ok(None),
    }
}

// ---------------------------------- summaries (T3) ----------------------------------

/// Result of reading a tag's last value.
enum TagRead {
    /// The tag's log folder is missing → swanboard reports "TypeError".
    MissingDir,
    /// Folder present but the last line couldn't be read/parsed → swanboard skips it.
    ReadFail,
    /// The last logged `data` value.
    Value(Value),
}

fn mtime_ns(m: &fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Reads the last non-empty line by seeking to the file tail (vs swanboard's full readlines()).
fn tail_last_line(path: &Path, size: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    if size == 0 {
        return None;
    }
    let start = size.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

/// Last logged `data` value for a tag folder, cached by last-shard file identity.
fn tag_last_value(logdir: &Path, run_id: &str, folder: &str, cache: &SummaryCache) -> TagRead {
    let dir = match safe_run_path(logdir, run_id, &["logs", folder]) {
        Some(p) if safe_is_dir(&p) => p,
        _ => return TagRead::MissingDir,
    };
    // Last .log shard by lexical sort (matches swanboard get_tag_files + list.sort()).
    let mut logs: Vec<String> = match fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".log"))
            .collect(),
        Err(_) => return TagRead::ReadFail,
    };
    if logs.is_empty() {
        return TagRead::ReadFail;
    }
    logs.sort();
    let file = dir.join(logs.last().unwrap());
    let fmeta = match fs::symlink_metadata(&file) {
        Ok(m) if !m.file_type().is_symlink() && m.is_file() => m,
        _ => return TagRead::ReadFail,
    };
    let key = format!("{run_id}/{folder}/{}/{}", mtime_ns(&fmeta), fmeta.len());
    if let Some(v) = cache.get(&key) {
        return TagRead::Value((*v).clone());
    }
    let line = match tail_last_line(&file, fmeta.len()) {
        Some(l) => l,
        None => return TagRead::ReadFail,
    };
    let parsed: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(_) => return TagRead::ReadFail,
    };
    let data = parsed.get("data").cloned().unwrap_or(Value::Null);
    cache.insert(key, Arc::new(data.clone()));
    TagRead::Value(data)
}

/// Computes `tag_last_value` for many (run_id, folder) jobs in parallel over scoped threads.
fn parallel_tag_values(
    jobs: &[(String, String)],
    logdir: &Path,
    cache: &SummaryCache,
) -> Vec<TagRead> {
    let n = jobs.len();
    if n == 0 {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(8)
        .min(32);
    let results: Vec<std::sync::Mutex<Option<TagRead>>> =
        (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for t in 0..threads {
            let results = &results;
            s.spawn(move || {
                let mut i = t;
                while i < n {
                    let (run_id, folder) = &jobs[i];
                    let v = tag_last_value(logdir, run_id, folder, cache);
                    *results[i].lock().unwrap() = Some(v);
                    i += threads;
                }
            });
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}

/// `GET /api/v1/project/summaries` — {tags:[...], summaries:{exp_name:{tag_name:value}}}.
pub fn project_summaries(logdir: &Path, cache: &SummaryCache) -> Result<Value> {
    let conn = open_ro(logdir)?;

    let mut estmt =
        conn.prepare("SELECT id,name,run_id FROM experiment WHERE project_id=1 ORDER BY id")?;
    let exps: Vec<(i64, String, String)> = estmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|x| x.ok())
        .collect();

    // Tags ordered by (experiment_id, create_time) — drives tag_names column order.
    let mut tstmt = conn
        .prepare("SELECT experiment_id,name,sort FROM tag ORDER BY experiment_id, create_time")?;
    let tags: Vec<(i64, String, i64)> = tstmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|x| x.ok())
        .collect();

    let mut tag_names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (_, name, _) in &tags {
        if seen.insert(name.clone()) {
            tag_names.push(name.clone());
        }
    }

    let mut by_exp: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
    for (eid, name, sort) in &tags {
        by_exp.entry(*eid).or_default().push((name.clone(), *sort));
    }

    // Flatten to per-tag jobs (exp order), then compute last values in parallel.
    let mut meta: Vec<(String, String)> = Vec::new(); // (exp_name, tag_name)
    let mut jobs: Vec<(String, String)> = Vec::new(); // (run_id, folder)
    for (eid, ename, run_id) in &exps {
        if let Some(ts) = by_exp.get(eid) {
            for (tname, sort) in ts {
                meta.push((ename.clone(), tname.clone()));
                jobs.push((run_id.clone(), sort.to_string()));
            }
        }
    }
    let values = parallel_tag_values(&jobs, logdir, cache);

    let mut summaries = Map::new();
    for (_, ename, _) in &exps {
        summaries
            .entry(ename.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    for ((ename, tname), res) in meta.into_iter().zip(values) {
        let obj = summaries
            .get_mut(&ename)
            .and_then(|v| v.as_object_mut())
            .unwrap();
        match res {
            TagRead::Value(v) => {
                obj.insert(tname, v);
            }
            TagRead::MissingDir => {
                obj.insert(tname, Value::String("TypeError".into()));
            }
            TagRead::ReadFail => {}
        }
    }

    Ok(json!({ "tags": tag_names, "summaries": Value::Object(summaries) }))
}

/// Warms the summary cache using the exact same read path as `/project/summaries`, then flushes
/// the persistent sidecar when `--cache-dir` is enabled. The returned payload is intentionally
/// discarded by the caller; this function exists to reuse the verified swanboard-compatible logic.
pub fn prewarm_project_summaries(logdir: &Path, cache: &SummaryCache) -> Result<()> {
    let _ = project_summaries(logdir, cache)?;
    if let Err(e) = cache.flush() {
        tracing::warn!("failed to flush persistent summary cache: {e}");
    }
    Ok(())
}

/// `GET /api/v1/experiment/{id}/summary` — {summaries:[{key,value}...]} ordered by tag.sort.
pub fn experiment_summary(logdir: &Path, id: i64, cache: &SummaryCache) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let run_id: Option<String> = conn
        .query_row("SELECT run_id FROM experiment WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let run_id = match run_id {
        Some(r) => r,
        None => return Ok(None),
    };

    let mut stmt = conn.prepare("SELECT folder,name,sort FROM tag WHERE experiment_id=?1")?;
    let tags: Vec<(Option<String>, String, i64)> = stmt
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|x| x.ok())
        .collect();

    let existing: HashSet<String> = safe_run_path(logdir, &run_id, &["logs"])
        .filter(|p| safe_is_dir(p))
        .and_then(|logs_dir| fs::read_dir(logs_dir).ok())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();

    // swanboard places each item at temp[sort]; sorts are 0..len-1.
    let len = tags.len();
    let mut slots: Vec<Option<Value>> = (0..len).map(|_| None).collect();
    for (folder_opt, name, sort) in tags {
        let folder = folder_opt.unwrap_or_else(|| sort.to_string());
        let value = if !existing.contains(&folder) {
            Value::String("TypeError".into())
        } else {
            match tag_last_value(logdir, &run_id, &folder, cache) {
                TagRead::Value(v) => v,
                _ => Value::Null,
            }
        };
        let item = json!({ "key": name, "value": value });
        let idx = sort as usize;
        if idx < len {
            slots[idx] = Some(item);
        } else {
            slots.push(Some(item));
        }
    }
    let summaries: Vec<Value> = slots.into_iter().flatten().collect();
    Ok(Some(json!({ "summaries": summaries })))
}

// ---------------------------------- charts (T4) ----------------------------------

/// Chart types shown on the multi-experiment project comparison page.
const ALLOW_TYPES: [&str; 4] = ["default", "line", "image", "audio"];

/// Full Project.__dict__ (11 fields), used as nested `project_id` in project charts/namespaces.
fn project_dict(conn: &Connection) -> Result<Value> {
    let pcols = [
        "id",
        "name",
        "description",
        "sum",
        "charts",
        "more",
        "pinned_opened",
        "hidden_opened",
        "version",
        "create_time",
        "update_time",
    ];
    let o = query_one_object(conn, "project", &pcols, "id=1", &[])?
        .ok_or_else(|| anyhow!("project not found"))?;
    Ok(Value::Object(o))
}

/// The nested project reached via peewee model_to_dict FK recursion reports `charts` as null
/// (unlike Project.__dict__ which reports the real value). Used inside namespaces.
fn nullify_charts(project: &Value) -> Value {
    let mut p = project.clone();
    if let Some(o) = p.as_object_mut() {
        o.insert("charts".into(), Value::Null);
    }
    p
}

/// model_to_dict(experiment) with nested project — used as namespaces'/charts' `experiment_id`.
fn experiment_full(conn: &Connection, id: i64, project: &Value) -> Result<Option<Value>> {
    let cols = [
        "id",
        "run_id",
        "name",
        "description",
        "sort",
        "status",
        "show",
        "light",
        "dark",
        "pinned_opened",
        "hidden_opened",
        "more",
        "version",
        "create_time",
        "finish_time",
        "update_time",
    ];
    let mut o = match query_one_object(conn, "experiment", &cols, "id=?1", &[&id])? {
        Some(o) => o,
        None => return Ok(None),
    };
    o.insert("project_id".into(), project.clone());
    Ok(Some(Value::Object(o)))
}

/// Chart columns (model_to_dict-equivalent, minus the FK fields which are added separately).
fn chart_base_cols() -> [&'static str; 12] {
    [
        "id",
        "name",
        "description",
        "system",
        "type",
        "reference",
        "config",
        "status",
        "sort",
        "more",
        "create_time",
        "update_time",
    ]
}

/// Chart id list for a namespace, ordered by display id (peewee backref default).
fn namespace_chart_ids(conn: &Connection, ns_id: i64, filter_types: bool) -> Result<Vec<i64>> {
    let sql = "SELECT c.id, c.type FROM display d JOIN chart c ON d.chart_id=c.id \
               WHERE d.namespace_id=?1 ORDER BY d.id";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([ns_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let (cid, ctype) = row?;
        if !filter_types || ALLOW_TYPES.contains(&ctype.as_str()) {
            ids.push(cid);
        }
    }
    Ok(ids)
}

/// Applies swanboard's get_pinned_and_hidden: dynamic pinned(-1)/hidden(-2) namespaces,
/// removing pinned/hidden chart ids from regular namespaces. `charts` = (id, status, sort).
fn apply_pinned_hidden(
    charts: &[(i64, i64, i64)],
    mut ns_list: Vec<Map<String, Value>>,
    dyn_expid: &Value,
    dyn_projid: &Value,
    pinned_opened: &Value,
    hidden_opened: &Value,
) -> Vec<Value> {
    if charts.is_empty() || ns_list.is_empty() {
        return ns_list.into_iter().map(Value::Object).collect();
    }
    let mut pinned: Vec<(i64, i64)> = Vec::new(); // (sort, id)
    let mut hidden: Vec<(i64, i64)> = Vec::new();
    for &(id, status, sort) in charts {
        if status == 1 {
            pinned.push((sort, id));
        } else if status == -1 {
            hidden.push((sort, id));
        }
        if status != 0 {
            for ns in &mut ns_list {
                if let Some(a) = ns.get_mut("charts").and_then(|v| v.as_array_mut()) {
                    a.retain(|x| x.as_i64() != Some(id));
                }
            }
        }
    }
    ns_list.retain(|ns| {
        ns.get("charts")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    });

    let mut out: Vec<Value> = Vec::new();
    if !pinned.is_empty() {
        pinned.sort_by_key(|&(s, _)| s);
        let ids: Vec<Value> = pinned.iter().map(|&(_, id)| json!(id)).collect();
        out.push(json!({
            "id": -1, "name": "pinned", "charts": ids, "opened": pinned_opened,
            "experiment_id": dyn_expid, "project_id": dyn_projid
        }));
    }
    out.extend(ns_list.into_iter().map(Value::Object));
    if !hidden.is_empty() {
        hidden.sort_by_key(|&(s, _)| s);
        let ids: Vec<Value> = hidden.iter().map(|&(_, id)| json!(id)).collect();
        out.push(json!({
            "id": -2, "name": "hidden", "charts": ids, "opened": hidden_opened,
            "experiment_id": dyn_expid, "project_id": dyn_projid
        }));
    }
    out
}

/// Builds single-experiment charts + namespaces (get_exp_charts + clear_field experiment_id).
fn build_experiment_charts(
    conn: &Connection,
    id: i64,
    overlay: &UiOverlay,
) -> Result<Option<(Vec<Value>, Vec<Value>)>> {
    let exists: Option<i64> = conn
        .query_row("SELECT id FROM experiment WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let project = nullify_charts(&project_dict(conn)?);
    let exp_full = experiment_full(conn, id, &project)?.unwrap();
    let pinned_opened = overlay.apply_experiment_pinned(
        id,
        exp_full.get("pinned_opened").cloned().unwrap_or(json!(1)),
    );
    let hidden_opened = overlay.apply_experiment_hidden(
        id,
        exp_full.get("hidden_opened").cloned().unwrap_or(json!(0)),
    );

    // Charts.
    let ccols = chart_base_cols();
    let sql = format!(
        "SELECT {} FROM chart WHERE experiment_id=?1 ORDER BY id",
        ccols.join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([id])?;
    let mut chart_meta: Vec<(i64, i64, i64)> = Vec::new(); // (id, status, sort)
    let mut charts_out: Vec<Value> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut o = Map::new();
        for (i, c) in ccols.iter().enumerate() {
            o.insert((*c).to_string(), vref_to_json(row.get_ref(i)?));
        }
        let cid = o.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let db_status = o.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
        let status = overlay.chart_status(cid, db_status);
        if status != db_status {
            o.insert("status".into(), json!(status));
        }
        let sort = o.get("sort").and_then(|v| v.as_i64()).unwrap_or(0);
        // Sources: tag names; source_map {tag_name: experiment_id(int)}.
        let mut sstmt = conn.prepare(
            "SELECT t.name, s.error FROM source s JOIN tag t ON s.tag_id=t.id \
             WHERE s.chart_id=?1 ORDER BY s.id",
        )?;
        let srows = sstmt.query_map([cid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let (mut source, mut source_map, mut error) = (Vec::new(), Map::new(), Map::new());
        for sr in srows {
            let (tname, err) = sr?;
            source.push(Value::String(tname.clone()));
            source_map.insert(tname.clone(), json!(id));
            if let Some(e) = err {
                if !e.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(&e) {
                        error.insert(tname, v);
                    }
                }
            }
        }
        // experiment_id is present during pinned/hidden processing, then cleared on output.
        o.insert("project_id".into(), Value::Null);
        o.insert("error".into(), Value::Object(error));
        o.insert("source".into(), Value::Array(source));
        o.insert("multi".into(), Value::Bool(false));
        o.insert("source_map".into(), Value::Object(source_map));
        chart_meta.push((cid, status, sort));
        charts_out.push(Value::Object(o));
    }

    // Namespaces (model_to_dict + charts:[ids]).
    let ncols = [
        "id",
        "name",
        "description",
        "sort",
        "opened",
        "more",
        "create_time",
        "update_time",
    ];
    let sql = format!(
        "SELECT {} FROM namespace WHERE experiment_id=?1 ORDER BY sort",
        ncols.join(",")
    );
    let mut nstmt = conn.prepare(&sql)?;
    let mut nrows = nstmt.query([id])?;
    let mut ns_list: Vec<Map<String, Value>> = Vec::new();
    while let Some(row) = nrows.next()? {
        let mut o = Map::new();
        for (i, c) in ncols.iter().enumerate() {
            o.insert((*c).to_string(), vref_to_json(row.get_ref(i)?));
        }
        let nid = o.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some(v) = o.get_mut("opened") {
            *v = overlay.apply_namespace_opened(nid, v.take());
        }
        o.insert("experiment_id".into(), exp_full.clone());
        o.insert("project_id".into(), Value::Null);
        let ids = namespace_chart_ids(conn, nid, false)?;
        o.insert(
            "charts".into(),
            Value::Array(ids.into_iter().map(|i| json!(i)).collect()),
        );
        ns_list.push(o);
    }

    let namespaces = apply_pinned_hidden(
        &chart_meta,
        ns_list,
        &exp_full,
        &Value::Null,
        &pinned_opened,
        &hidden_opened,
    );
    // clear_field(charts, "experiment_id") — charts never carried it in output above.
    Ok(Some((charts_out, namespaces)))
}

/// `GET /api/v1/experiment/{id}/chart` — {charts, namespaces}. None if experiment missing.
pub fn experiment_charts(logdir: &Path, id: i64, overlay: &UiOverlay) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    match build_experiment_charts(&conn, id, overlay)? {
        Some((charts, namespaces)) => {
            Ok(Some(json!({ "charts": charts, "namespaces": namespaces })))
        }
        None => Ok(None),
    }
}

/// `GET /api/v1/experiment/{id}/status` — status + charts payload.
pub fn experiment_status(logdir: &Path, id: i64, overlay: &UiOverlay) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let row: Option<(i64, String, Option<String>)> = conn
        .query_row(
            "SELECT status,update_time,finish_time FROM experiment WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (status, update_time, finish_time) = match row {
        Some(x) => x,
        None => return Ok(None),
    };
    let (charts, namespaces) = build_experiment_charts(&conn, id, overlay)?.unwrap();
    Ok(Some(json!({
        "status": status,
        "update_time": update_time,
        "finish_time": finish_time,
        "charts": { "charts": charts, "namespaces": namespaces },
    })))
}

/// `GET /api/v1/project/charts` — multi-experiment comparison charts + namespaces.
pub fn project_charts(logdir: &Path, overlay: &UiOverlay) -> Result<Value> {
    project_charts_filtered(logdir, overlay, None)
}

/// Shared builder for the multi-experiment comparison view.
///
/// `filter = None` reproduces swanboard's `/project/charts` byte-for-byte. `filter = Some(ids)`
/// (fastsl comparison groups) restricts the comparison to those experiment ids: each chart's
/// `source`/`source_map` keeps only in-group experiments, charts with no in-group source are
/// dropped, and namespaces drop those charts. An empty `ids` slice yields no charts.
pub fn project_charts_filtered(
    logdir: &Path,
    overlay: &UiOverlay,
    filter: Option<&[i64]>,
) -> Result<Value> {
    let conn = open_ro(logdir)?;
    let project = project_dict(&conn)?;
    let project_nested = nullify_charts(&project);
    let proj_id = project.get("id").and_then(|v| v.as_i64()).unwrap_or(1);
    let pinned_opened = overlay.apply_project_pinned(
        proj_id,
        project.get("pinned_opened").cloned().unwrap_or(json!(1)),
    );
    let hidden_opened = overlay.apply_project_hidden(
        proj_id,
        project.get("hidden_opened").cloned().unwrap_or(json!(0)),
    );
    // swanboard's get_proj_charts uses ONE source_map dict shared across all charts, so every
    // chart ends up carrying the accumulated union of all contributing experiments.
    let mut source_map_all = Map::new();

    // Group filter: restrict each chart's source join to the selected experiment ids. Ids come
    // from our own DB resolution (i64), so inlining them is injection-safe; an empty group matches
    // nothing. Charts that survive the filter are recorded so namespaces can drop the rest.
    let filter_clause = match filter {
        Some([]) => " AND 1=0".to_string(),
        Some(ids) => format!(
            " AND t.experiment_id IN ({})",
            ids.iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        None => String::new(),
    };
    let source_sql = format!(
        "SELECT e.name, e.id, s.error FROM source s JOIN tag t ON s.tag_id=t.id \
         JOIN experiment e ON t.experiment_id=e.id WHERE s.chart_id=?1{filter_clause} ORDER BY s.id"
    );
    let mut kept_charts: std::collections::HashSet<i64> = std::collections::HashSet::new();

    // Charts (type in ALLOW_TYPES). chart.__dict__: experiment_id null, project_id nested.
    let ccols = chart_base_cols();
    let placeholders = ALLOW_TYPES
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {} FROM chart WHERE project_id=1 AND type IN ({}) ORDER BY id",
        ccols.join(","),
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = ALLOW_TYPES
        .iter()
        .map(|t| t as &dyn rusqlite::ToSql)
        .collect();
    let mut rows = stmt.query(params.as_slice())?;
    let mut chart_meta: Vec<(i64, i64, i64)> = Vec::new();
    let mut charts_out: Vec<Value> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut o = Map::new();
        for (i, c) in ccols.iter().enumerate() {
            o.insert((*c).to_string(), vref_to_json(row.get_ref(i)?));
        }
        let cid = o.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let db_status = o.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
        let status = overlay.chart_status(cid, db_status);
        if status != db_status {
            o.insert("status".into(), json!(status));
        }
        let sort = o.get("sort").and_then(|v| v.as_i64()).unwrap_or(0);
        // Sources: experiment names; source_map {exp_name: exp_id}.
        let mut sstmt = conn.prepare(&source_sql)?;
        let srows = sstmt.query_map([cid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let (mut source, mut error) = (Vec::new(), Map::new());
        for sr in srows {
            let (ename, eid, err) = sr?;
            source.push(Value::String(ename.clone()));
            source_map_all.insert(ename.clone(), json!(eid));
            if let Some(e) = err {
                if !e.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(&e) {
                        error.insert(ename, v);
                    }
                }
            }
        }
        // In a group view, a chart with no in-group experiment contributes nothing — drop it.
        if filter.is_some() && source.is_empty() {
            continue;
        }
        kept_charts.insert(cid);
        o.insert("experiment_id".into(), Value::Null);
        o.insert("project_id".into(), project.clone());
        o.insert("error".into(), Value::Object(error));
        o.insert("source".into(), Value::Array(source));
        o.insert("multi".into(), Value::Bool(true));
        // source_map assigned after the loop (shared accumulated union).
        chart_meta.push((cid, status, sort));
        charts_out.push(Value::Object(o));
    }

    // Assign the shared accumulated source_map to every chart (swanboard aliasing behaviour).
    let shared_map = Value::Object(source_map_all);
    for c in charts_out.iter_mut() {
        if let Some(o) = c.as_object_mut() {
            o.insert("source_map".into(), shared_map.clone());
        }
    }

    // Namespaces (project-level), only those with allowed charts.
    let ncols = [
        "id",
        "name",
        "description",
        "sort",
        "opened",
        "more",
        "create_time",
        "update_time",
    ];
    let sql = format!(
        "SELECT {} FROM namespace WHERE project_id=1 ORDER BY sort",
        ncols.join(",")
    );
    let mut nstmt = conn.prepare(&sql)?;
    let mut nrows = nstmt.query([])?;
    let mut ns_list: Vec<Map<String, Value>> = Vec::new();
    while let Some(row) = nrows.next()? {
        let mut o = Map::new();
        for (i, c) in ncols.iter().enumerate() {
            o.insert((*c).to_string(), vref_to_json(row.get_ref(i)?));
        }
        let nid = o.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut ids = namespace_chart_ids(&conn, nid, true)?;
        if filter.is_some() {
            ids.retain(|id| kept_charts.contains(id)); // drop charts filtered out of the group
        }
        if ids.is_empty() {
            continue; // swanboard drops namespaces without allowed charts
        }
        if let Some(v) = o.get_mut("opened") {
            *v = overlay.apply_namespace_opened(nid, v.take());
        }
        o.insert("experiment_id".into(), Value::Null);
        o.insert("project_id".into(), project_nested.clone());
        o.insert(
            "charts".into(),
            Value::Array(ids.into_iter().map(|i| json!(i)).collect()),
        );
        ns_list.push(o);
    }

    let namespaces = apply_pinned_hidden(
        &chart_meta,
        ns_list,
        &Value::Null,
        &project,
        &pinned_opened,
        &hidden_opened,
    );
    Ok(json!({ "charts": charts_out, "namespaces": namespaces }))
}

// ---------------------------------- write overlay support (T6) ----------------------------------

/// Whether a namespace row exists (for PATCH validation).
pub fn namespace_exists(logdir: &Path, id: i64) -> Result<bool> {
    let conn = open_ro(logdir)?;
    let found: Option<i64> = conn
        .query_row("SELECT id FROM namespace WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    Ok(found.is_some())
}

/// Whether an experiment row exists (for PATCH validation).
pub fn experiment_exists(logdir: &Path, id: i64) -> Result<bool> {
    let conn = open_ro(logdir)?;
    Ok(run_id_of(&conn, id)?.is_some())
}

/// Whether an experiment with the given `run_id` exists (fastsl alias/group validation).
pub fn run_id_exists(logdir: &Path, run_id: &str) -> Result<bool> {
    let conn = open_ro(logdir)?;
    let found: Option<i64> = conn
        .query_row("SELECT id FROM experiment WHERE run_id=?1", [run_id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(found.is_some())
}

/// Resolves experiment `run_id`s to row ids, preserving input order and silently dropping any
/// run_id that no longer exists. Translates a fastsl group's members into the id filter for
/// [`project_charts_filtered`].
pub fn ids_for_run_ids(logdir: &Path, run_ids: &[String]) -> Result<Vec<i64>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }
    let conn = open_ro(logdir)?;
    let mut stmt = conn.prepare("SELECT id FROM experiment WHERE run_id=?1")?;
    let mut ids = Vec::new();
    for rid in run_ids {
        if let Some(id) = stmt.query_row([rid], |r| r.get::<_, i64>(0)).optional()? {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Whether a project row exists (for PATCH validation).
pub fn project_exists(logdir: &Path, id: i64) -> Result<bool> {
    let conn = open_ro(logdir)?;
    let found: Option<i64> = conn
        .query_row("SELECT id FROM project WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    Ok(found.is_some())
}

/// A chart's owner: swanboard routes status recompute to project charts when the chart
/// carries a `project_id`, otherwise to its experiment's charts.
pub enum ChartOwner {
    Project(i64),
    Experiment(i64),
}

/// Resolves the owning project/experiment for a chart id (None if the chart does not exist).
pub fn chart_owner(logdir: &Path, chart_id: i64) -> Result<Option<ChartOwner>> {
    let conn = open_ro(logdir)?;
    let row: Option<(Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT project_id, experiment_id FROM chart WHERE id=?1",
            [chart_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        None => None,
        Some((Some(pid), _)) => Some(ChartOwner::Project(pid)),
        Some((None, Some(eid))) => Some(ChartOwner::Experiment(eid)),
        Some((None, None)) => None,
    })
}

/// Reproduces `update_charts_status`' response body: `{groups: [...]}` where each namespace's
/// `charts` id list is replaced by the full chart objects (or null if not found), with the
/// overlay (including the just-set status) already merged in.
pub fn chart_status_groups(
    logdir: &Path,
    chart_id: i64,
    overlay: &UiOverlay,
) -> Result<Option<Value>> {
    let payload = match chart_owner(logdir, chart_id)? {
        Some(ChartOwner::Project(pid)) => project_charts_for(logdir, pid, overlay)?,
        Some(ChartOwner::Experiment(eid)) => match experiment_charts(logdir, eid, overlay)? {
            Some(v) => v,
            None => return Ok(None),
        },
        None => return Ok(None),
    };

    let charts = payload
        .get("charts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let namespaces = payload
        .get("namespaces")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let find_chart = |cid: Option<i64>| -> Value {
        charts
            .iter()
            .find(|c| c.get("id").and_then(|x| x.as_i64()) == cid)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut groups: Vec<Value> = Vec::new();
    for ns in namespaces {
        let mut ns_obj = ns.as_object().cloned().unwrap_or_default();
        let ids = ns_obj
            .get("charts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let replaced: Vec<Value> = ids.iter().map(|idv| find_chart(idv.as_i64())).collect();
        ns_obj.insert("charts".into(), Value::Array(replaced));
        groups.push(Value::Object(ns_obj));
    }
    Ok(Some(json!({ "groups": groups })))
}

/// `project_charts` for an arbitrary project id (the public entry hard-codes project 1).
/// swanboard is single-project, so this only ever sees id 1, but the lookup stays explicit.
fn project_charts_for(logdir: &Path, _project_id: i64, overlay: &UiOverlay) -> Result<Value> {
    project_charts(logdir, overlay)
}

/// Experiment in `Experiment.__dict__()` shape (nested full project, no light/dark/pinned/hidden),
/// with the `show` override merged — used as the `change_experiment_visibility` response body.
pub fn experiment_dict(logdir: &Path, id: i64, overlay: &UiOverlay) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let cols = [
        "id",
        "run_id",
        "name",
        "description",
        "sort",
        "status",
        "show",
        "more",
        "version",
        "create_time",
        "update_time",
        "finish_time",
    ];
    let mut o = match query_one_object(&conn, "experiment", &cols, "id=?1", &[&id])? {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Some(v) = o.get_mut("show") {
        *v = overlay.apply_experiment_show(id, v.take());
    }
    let project = project_dict(&conn)?;
    let mut out = Map::new();
    out.insert("id".into(), o.remove("id").unwrap_or(Value::Null));
    out.insert("project_id".into(), project);
    for k in [
        "run_id",
        "name",
        "description",
        "sort",
        "status",
        "show",
        "more",
        "version",
        "create_time",
        "update_time",
        "finish_time",
    ] {
        out.insert(k.into(), o.remove(k).unwrap_or(Value::Null));
    }
    Ok(Some(Value::Object(out)))
}

// ---------------------------------- tag time-series (T5) ----------------------------------

/// LTTB downsample threshold (swanboard default).
const LTTB_THRESHOLD: usize = 1500;

fn point_index(v: &Value) -> i64 {
    v.get("index").and_then(|x| x.as_i64()).unwrap_or(0)
}
fn point_data_f64(v: &Value) -> f64 {
    v.get("data").and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// Per-bucket capacities: floor(total/target) each, remainder spread over the first buckets.
fn calc_bucket_capacity(total: usize, target: usize) -> Vec<usize> {
    let cap = total / target;
    let rem = total % target;
    let mut list = vec![cap; target];
    for slot in list.iter_mut().take(rem) {
        *slot += 1;
    }
    list
}

/// Picks the bucket point farthest from the line through its endpoints (LTTB triangle rule).
fn sample_a_bucket(bucket: &[Value]) -> Value {
    if bucket.len() <= 2 {
        return bucket[0].clone();
    }
    let tail = &bucket[0];
    let head = &bucket[bucket.len() - 1];
    let (ti, td) = (point_index(tail) as f64, point_data_f64(tail));
    let (hi, hd) = (point_index(head) as f64, point_data_f64(head));
    let k = if (hi - ti).abs() < f64::EPSILON {
        0.0
    } else {
        (hd - td) / (hi - ti)
    };
    let b = hd - k * hi;
    let denom = (k * k + 1.0).sqrt();
    let mut max_dist = 0.0f64;
    let mut max_i = 0usize;
    for (i, point) in bucket.iter().enumerate().take(bucket.len() - 1).skip(1) {
        let bi = point_index(point) as f64;
        let bd = point_data_f64(point);
        let dist = (k * bi + b - bd).abs() / denom;
        if dist > max_dist {
            max_dist = dist;
            max_i = i;
        }
    }
    bucket[max_i].clone()
}

/// Largest-Triangle-Three-Buckets downsampling — point-for-point port of swanboard's `lttb`.
fn lttb(data: Vec<Value>, threshold: usize) -> Vec<Value> {
    let n = data.len();
    if n <= threshold {
        return data;
    }
    let bucket_n = threshold - 2;
    let inner_len = n - 2;
    let caps = calc_bucket_capacity(inner_len, bucket_n);
    let mut sampled: Vec<Value> = Vec::with_capacity(threshold);
    sampled.push(data[0].clone());
    let mut now_bucket: Vec<Value> = Vec::new();
    let mut now_bucket_n = 0usize;
    for d in &data[1..n - 1] {
        now_bucket.push(d.clone());
        if now_bucket.len() < caps[now_bucket_n] {
            continue;
        }
        sampled.push(sample_a_bucket(&now_bucket));
        now_bucket.clear();
        now_bucket_n += 1;
        if now_bucket_n == bucket_n - 1 {
            break;
        }
    }
    sampled.push(data[n - 1].clone());
    sampled
}

/// Reads every point across a tag's shards (all lines of all `.log` files).
fn read_all_tag_points(tag_dir: &Path) -> Vec<Value> {
    let mut logs: Vec<String> = match fs::read_dir(tag_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".log"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    logs.sort();
    let mut data = Vec::new();
    for f in logs {
        if let Some(s) = safe_read_to_string(&tag_dir.join(&f)) {
            for line in s.lines() {
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    data.push(v);
                }
            }
        }
    }
    data
}

/// `GET /api/v1/experiment/{id}/tag/{tag}` — full (LTTB-downsampled) series + max/min.
/// `Ok(None)` when the experiment or tag/folder is missing.
pub fn tag_data(logdir: &Path, id: i64, tag: &str) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let run_id: Option<String> = conn
        .query_row("SELECT run_id FROM experiment WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let run_id = match run_id {
        Some(r) => r,
        None => return Ok(None),
    };
    let folder: Option<Option<String>> = conn
        .query_row(
            "SELECT folder FROM tag WHERE name=?1 AND experiment_id=?2",
            rusqlite::params![tag, id],
            |r| r.get(0),
        )
        .optional()?;
    let folder = match folder {
        Some(Some(f)) => f,
        _ => return Ok(None),
    };

    let tag_dir = match safe_run_path(logdir, &run_id, &["logs", &folder]) {
        Some(p) if safe_is_dir(&p) => p,
        _ => return Ok(None),
    };

    let mut data = read_all_tag_points(&tag_dir);
    if data.is_empty() {
        return Ok(Some(json!({ "sum": 0, "list": [], "experiment_id": id })));
    }
    data.sort_by_key(point_index);
    if let Some(last) = data.last_mut() {
        if let Some(o) = last.as_object_mut() {
            o.insert("_last".into(), json!(true));
        }
    }

    // max/min from _summary.json, else computed from the data (swanboard COMPAT path).
    let (max_v, min_v) = {
        let sp = tag_dir.join("_summary.json");
        match safe_read_to_string(&sp).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            Some(sum) => (
                sum.get("max").cloned().unwrap_or(Value::Null),
                sum.get("min").cloned().unwrap_or(Value::Null),
            ),
            None => {
                let vals: Vec<f64> = data.iter().map(point_data_f64).collect();
                let mx = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let mn = vals.iter().cloned().fold(f64::INFINITY, f64::min);
                (json!(mx), json!(mn))
            }
        }
    };

    let sum = data.len();
    let list = lttb(data, LTTB_THRESHOLD);
    Ok(Some(json!({
        "sum": sum,
        "max": max_v,
        "min": min_v,
        "list": list,
        "experiment_id": id,
    })))
}

// ---------------------------------- media / logs / requirements (T6) ----------------------------------

fn run_id_of(conn: &Connection, id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT run_id FROM experiment WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?)
}

/// `GET /api/v1/experiment/{id}/recent_log` — console logs (+ optional error). None → 404.
pub fn recent_logs(logdir: &Path, id: i64) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let run_id = match run_id_of(&conn, id)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let cdir = match safe_run_path(logdir, &run_id, &["console"]) {
        Some(p) if safe_is_dir(&p) => p,
        _ => return Ok(None),
    };
    let mut consoles: Vec<String> = fs::read_dir(&cdir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();

    let mut error: Option<Vec<String>> = None;
    if let Some(pos) = consoles.iter().position(|f| f == "error.log") {
        if let Some(s) = safe_read_to_string(&cdir.join("error.log")) {
            error = Some(s.split('\n').map(|x| x.to_string()).collect());
        }
        consoles.remove(pos);
    }
    if consoles.is_empty() {
        return match error {
            Some(e) => Ok(Some(json!({ "error": e }))),
            None => Ok(None),
        };
    }
    if consoles.len() > 1 {
        // Filenames are YYYY-MM-DD.log → lexical desc == chronological desc.
        consoles.sort();
        consoles.reverse();
    }
    let mut logs: Vec<String> = Vec::new();
    for f in &consoles {
        let content = safe_read_to_string(&cdir.join(f)).unwrap_or_default();
        let mut lines: Vec<String> = content.split('\n').map(|x| x.to_string()).collect();
        lines.append(&mut logs);
        logs = lines;
    }
    if logs.is_empty() {
        return Ok(None);
    }
    let last = if !logs[logs.len() - 1].is_empty() {
        &logs[logs.len() - 1]
    } else {
        &logs[logs.len() - 2]
    };
    let end = last.split(' ').next().unwrap_or("");
    let start = logs[0].split(' ').next().unwrap_or("");
    let mut data = json!({ "recent": [start, end], "logs": logs });
    if let Some(e) = error {
        data["error"] = json!(e);
    }
    Ok(Some(data))
}

/// `GET /api/v1/experiment/{id}/requirements`. None → the file is absent.
pub fn requirements(logdir: &Path, id: i64) -> Result<Option<Value>> {
    let conn = open_ro(logdir)?;
    let run_id = match run_id_of(&conn, id)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let Some(p) = safe_run_path(logdir, &run_id, &["files", "requirements.txt"]) else {
        return Ok(None);
    };
    match safe_read_to_string(&p) {
        Some(s) => {
            let lines: Vec<&str> = s.split('\n').collect();
            Ok(Some(json!({ "requirements": lines })))
        }
        None => Ok(None),
    }
}

/// Resolves the on-disk media file for `GET /api/v1/media/{path}?tag=&experiment_id=`.
pub fn media_file(
    logdir: &Path,
    id: i64,
    tag: &str,
    rel: &str,
) -> Result<Option<std::path::PathBuf>> {
    let conn = open_ro(logdir)?;
    let run_id = match run_id_of(&conn, id)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let folder: Option<Option<String>> = conn
        .query_row(
            "SELECT folder FROM tag WHERE name=?1 AND experiment_id=?2",
            rusqlite::params![tag, id],
            |r| r.get(0),
        )
        .optional()?;
    let folder = match folder {
        Some(Some(f)) => f,
        _ => return Ok(None),
    };
    Ok(safe_run_path(logdir, &run_id, &["media", &folder, rel])
        .filter(|p| safe_file_size(p).is_some()))
}
