//! fastsl-only endpoints: experiment aliases + comparison groups.
//!
//! These live outside the swanboard `/api/v1` contract (all under `/api/v1/fastsl/*`), so they
//! never affect golden byte-compat. Writes persist to the same JSON sidecar as the view-preference
//! overlay via `UiState`; `runs.swanlab` stays strictly read-only. Blocking sidecar/DB work runs on
//! the tokio blocking pool, mirroring `api.rs`.

use axum::{
    extract::{Path, State},
    response::Response,
    Json,
};
use serde_json::{json, Value};

use crate::{
    db, resp,
    uistate::{MAX_GROUPS, MAX_GROUP_MEMBERS},
    AppState,
};

const MAX_ALIAS_BYTES: usize = 256;
const MAX_GROUP_NAME_BYTES: usize = 128;
const MAX_RUN_ID_BYTES: usize = 256;

fn validate_text(value: &str, field: &str, max_bytes: usize) -> Option<Response> {
    (value.is_empty() || value.len() > max_bytes).then(|| {
        resp::params_error(&format!(
            "Request parameter '{field}' must be 1-{max_bytes} bytes"
        ))
    })
}

/// `GET /api/v1/fastsl/ui` — aliases + groups in one payload for the client to merge on load.
pub async fn get_ui(State(st): State<AppState>) -> Response {
    let snap = st.ui.snapshot();
    resp::success(json!({
        "aliases": snap.experiment_alias,
        "groups": snap.groups,
    }))
}

/// `PATCH /api/v1/fastsl/experiment/{run_id}/alias` — body `{alias}`.
/// Absent/null/empty `alias` clears the override so the experiment reverts to its DB `name`.
pub async fn patch_alias(
    State(st): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if run_id.len() > MAX_RUN_ID_BYTES {
        return resp::params_error("Request parameter 'run_id' is too long");
    }
    let alias = match body.get("alias") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= MAX_ALIAS_BYTES => Some(s.clone()),
        Some(Value::String(_)) => {
            return resp::params_error("Request parameter 'alias' is too long")
        }
        Some(_) => return resp::params_error("Request parameter 'alias' must be a string"),
    };
    let logdir = st.logdir.clone();
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        if !db::run_id_exists(&logdir, &run_id)? {
            return Ok(false);
        }
        ui.set_experiment_alias(&run_id, alias)?;
        Ok(true)
    })
    .await;
    match res {
        Ok(Ok(true)) => resp::success_empty(),
        Ok(Ok(false)) => resp::not_found("experiment not found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `POST /api/v1/fastsl/group` — body `{name, members?:[run_id]}`. Non-existent members are
/// dropped. Returns `{group}`.
pub async fn create_group(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.trim(),
        None => return resp::params_error("Request parameter 'name' must be a string"),
    };
    if let Some(response) = validate_text(name, "name", MAX_GROUP_NAME_BYTES) {
        return response;
    }
    let members = match body.get("members") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) if items.len() <= MAX_GROUP_MEMBERS => {
            let mut unique = std::collections::HashSet::new();
            let mut members = Vec::with_capacity(items.len());
            for value in items {
                let Some(run_id) = value.as_str() else {
                    return resp::params_error("Request parameter 'members' must contain strings");
                };
                if let Some(response) = validate_text(run_id, "run_id", MAX_RUN_ID_BYTES) {
                    return response;
                }
                if unique.insert(run_id) {
                    members.push(run_id.to_string());
                }
            }
            members
        }
        Some(Value::Array(_)) => return resp::params_error("Too many group members"),
        Some(_) => return resp::params_error("Request parameter 'members' must be an array"),
    };
    let name = name.to_string();

    let logdir = st.logdir.clone();
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let mut valid = Vec::with_capacity(members.len());
        for m in members {
            if db::run_id_exists(&logdir, &m)? {
                valid.push(m);
            }
        }
        let group = ui.create_group(name, valid)?;
        Ok(group.map(serde_json::to_value).transpose()?)
    })
    .await;
    match res {
        Ok(Ok(Some(group))) => resp::success(json!({ "group": group })),
        Ok(Ok(None)) => resp::params_error(&format!("At most {MAX_GROUPS} groups are allowed")),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `PATCH /api/v1/fastsl/group/{gid}` — body `{name}` (rename).
pub async fn rename_group(
    State(st): State<AppState>,
    Path(gid): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.trim(),
        None => return resp::params_error("Request parameter 'name' must be a string"),
    };
    if let Some(response) = validate_text(name, "name", MAX_GROUP_NAME_BYTES) {
        return response;
    }
    let name = name.to_string();
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || ui.rename_group(gid, name)).await;
    group_ack(res, gid)
}

/// `DELETE /api/v1/fastsl/group/{gid}`.
pub async fn delete_group(State(st): State<AppState>, Path(gid): Path<i64>) -> Response {
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || ui.delete_group(gid)).await;
    group_ack(res, gid)
}

/// `POST /api/v1/fastsl/group/{gid}/member` — body `{run_id}` (add).
pub async fn add_member(
    State(st): State<AppState>,
    Path(gid): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    let run_id = match body.get("run_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return resp::params_error("Request parameter 'run_id' must be a string"),
    };
    if let Some(response) = validate_text(run_id, "run_id", MAX_RUN_ID_BYTES) {
        return response;
    }
    let run_id = run_id.to_string();
    let logdir = st.logdir.clone();
    let ui = st.ui.clone();
    // 0 = ok, 1 = experiment missing, 2 = group missing.
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<u8> {
        if !db::run_id_exists(&logdir, &run_id)? {
            return Ok(1);
        }
        if !ui.add_member(gid, &run_id)? {
            return Ok(2);
        }
        Ok(0)
    })
    .await;
    match res {
        Ok(Ok(0)) => resp::success_empty(),
        Ok(Ok(1)) => resp::not_found("experiment not found"),
        Ok(Ok(_)) => resp::not_found(&format!("Group with id {gid} does not exist.")),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `DELETE /api/v1/fastsl/group/{gid}/member/{run_id}` (idempotent for an absent member).
pub async fn remove_member(
    State(st): State<AppState>,
    Path((gid, run_id)): Path<(i64, String)>,
) -> Response {
    if run_id.len() > MAX_RUN_ID_BYTES {
        return resp::params_error("Request parameter 'run_id' is too long");
    }
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || ui.remove_member(gid, &run_id)).await;
    group_ack(res, gid)
}

/// `GET /api/v1/fastsl/group/{gid}/charts` — same shape as `/project/charts`, restricted to the
/// group's experiments (curves + images + audio).
pub async fn get_group_charts(State(st): State<AppState>, Path(gid): Path<i64>) -> Response {
    let logdir = st.logdir.clone();
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let snap = ui.snapshot();
        let members = match snap.groups.iter().find(|g| g.id == gid) {
            Some(g) => g.members.clone(),
            None => return Ok(None),
        };
        let ids = db::ids_for_run_ids(&logdir, &members)?;
        Ok(Some(db::project_charts_filtered(
            &logdir,
            &snap,
            Some(&ids),
        )?))
    })
    .await;
    match res {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found(&format!("Group with id {gid} does not exist.")),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// Shared "did the group exist?" reply for rename/delete/remove-member.
fn group_ack(res: Result<anyhow::Result<bool>, tokio::task::JoinError>, gid: i64) -> Response {
    match res {
        Ok(Ok(true)) => resp::success_empty(),
        Ok(Ok(false)) => resp::not_found(&format!("Group with id {gid} does not exist.")),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}
