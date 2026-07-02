//! Axum handlers for the swanboard-compatible `/api/v1` surface.
//! Blocking DB/file work runs on the tokio blocking pool so the server stays responsive.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::uistate::truthy_to_int;
use crate::{db, resp, AppState};

/// Query params for the media endpoint (`?tag=&experiment_id=`).
#[derive(serde::Deserialize)]
pub struct MediaQuery {
    tag: String,
    experiment_id: i64,
}

/// `GET /api/v1/project`
pub async fn get_project(State(st): State<AppState>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let overlay = st.ui.snapshot();
    match tokio::task::spawn_blocking(move || db::project_info(&logdir, &overlay)).await {
        Ok(Ok(v)) => resp::success(v),
        Ok(Err(e)) => resp::data_error(&format!("Get list experiments failed: {e}")),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}`
pub async fn get_experiment(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let overlay = st.ui.snapshot();
    match tokio::task::spawn_blocking(move || db::experiment_info(&logdir, id, &overlay)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found("experiment not found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/project/summaries`
pub async fn get_project_summaries(State(st): State<AppState>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let cache = st.summary_cache.clone();
    match tokio::task::spawn_blocking(move || {
        let v = db::project_summaries(&logdir, &cache)?;
        if let Err(e) = cache.flush() {
            tracing::warn!("failed to flush persistent summary cache: {e}");
        }
        anyhow::Ok(v)
    })
    .await
    {
        Ok(Ok(v)) => resp::success(v),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}/summary`
pub async fn get_experiment_summary(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let cache = st.summary_cache.clone();
    match tokio::task::spawn_blocking(move || db::experiment_summary(&logdir, id, &cache)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found("experiment not found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/project/charts`
pub async fn get_project_charts(State(st): State<AppState>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let overlay = st.ui.snapshot();
    match tokio::task::spawn_blocking(move || db::project_charts(&logdir, &overlay)).await {
        Ok(Ok(v)) => resp::success(v),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}/chart`
pub async fn get_experiment_chart(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let overlay = st.ui.snapshot();
    match tokio::task::spawn_blocking(move || db::experiment_charts(&logdir, id, &overlay)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found("experiment not found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}/status`
pub async fn get_experiment_status_ep(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let overlay = st.ui.snapshot();
    match tokio::task::spawn_blocking(move || db::experiment_status(&logdir, id, &overlay)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found("experiment not found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}/tag/{tag}` (tag may contain slashes).
pub async fn get_experiment_tag(
    State(st): State<AppState>,
    Path((id, tag)): Path<(i64, String)>,
) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    match tokio::task::spawn_blocking(move || db::tag_data(&logdir, id, &tag)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found("tag not found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}/recent_log`
pub async fn get_recent_log(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    match tokio::task::spawn_blocking(move || db::recent_logs(&logdir, id)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found("No Logs Found"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/experiment/{id}/requirements`
pub async fn get_requirements(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    match tokio::task::spawn_blocking(move || db::requirements(&logdir, id)).await {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::data_error("failed to find requirements"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `GET /api/v1/media/{path}?tag=&experiment_id=` — streams a media file (image/audio/…).
pub async fn get_media(
    State(st): State<AppState>,
    Path(rel): Path<String>,
    Query(q): Query<MediaQuery>,
    request: Request,
) -> Response {
    let logdir: Arc<PathBuf> = st.logdir.clone();
    let resolved =
        tokio::task::spawn_blocking(move || db::media_file(&logdir, q.experiment_id, &q.tag, &rel))
            .await;
    let path = match resolved {
        Ok(Ok(Some(path))) => path,
        Ok(Ok(None)) => return resp::not_found("media not found"),
        Ok(Err(e)) => return resp::data_error(&e.to_string()),
        Err(e) => return resp::data_error(&e.to_string()),
    };
    // Serve via ServeFile so Range/206, conditional headers, and content-type detection
    // match the original swanboard's FastAPI FileResponse (which supports Range).
    match ServeFile::new(&path).oneshot(request).await {
        Ok(res) => res.map(Body::new).into_response(),
        Err(_) => resp::not_found("media not found"),
    }
}

// ---------------------------------- PATCH: view-preference overlay (T6-write) ----------------------------------
//
// swanboard writes these into runs.swanlab; fastsl keeps that DB read-only and records the
// override in the JSON sidecar (AppState.ui), merging it back on the corresponding GETs.

/// Missing-or-null → the JSON body did not provide the required parameter.
fn missing(body: &Value, key: &str) -> bool {
    matches!(body.get(key), None | Some(Value::Null))
}

/// `PATCH /api/v1/namespace/{namespace_id}/opened` — body `{opened, experiment_id?, project_id?}`.
/// Ids -1/-2 toggle the dynamic pinned/hidden group on the experiment or project; positive ids
/// toggle a real namespace. Returns `{code:0,message:success}` (no `data`), like `SUCCESS_200(None)`.
pub async fn patch_namespace_opened(
    State(st): State<AppState>,
    Path(namespace_id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    if missing(&body, "opened") {
        return resp::params_error("Request parameter 'opened'");
    }
    let opened = truthy_to_int(body.get("opened").unwrap());
    let experiment_id = body.get("experiment_id").and_then(|v| v.as_i64());
    let project_id = body.get("project_id").and_then(|v| v.as_i64());

    let logdir: Arc<PathBuf> = st.logdir.clone();
    let ui = st.ui.clone();

    if namespace_id == -1 || namespace_id == -2 {
        if experiment_id.is_none() && project_id.is_none() {
            return resp::params_error("Request parameter 'experiment_id' or 'project_id'");
        }
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            if let Some(eid) = experiment_id {
                if !db::experiment_exists(&logdir, eid)? {
                    return Ok(false);
                }
                if namespace_id == -1 {
                    ui.set_experiment_pinned(eid, opened)?;
                } else {
                    ui.set_experiment_hidden(eid, opened)?;
                }
            } else {
                let pid = project_id.unwrap();
                if !db::project_exists(&logdir, pid)? {
                    return Ok(false);
                }
                if namespace_id == -1 {
                    ui.set_project_pinned(pid, opened)?;
                } else {
                    ui.set_project_hidden(pid, opened)?;
                }
            }
            Ok(true)
        })
        .await;
        // swanboard formats this message with experiment_id (prints "None" when absent).
        let id_str = experiment_id
            .map(|x| x.to_string())
            .unwrap_or_else(|| "None".to_string());
        return match res {
            Ok(Ok(true)) => resp::success_empty(),
            Ok(Ok(false)) => resp::not_found(&format!(
                "Experiment or project with id {id_str} does not exist."
            )),
            Ok(Err(e)) => resp::data_error(&e.to_string()),
            Err(e) => resp::data_error(&e.to_string()),
        };
    }

    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        if !db::namespace_exists(&logdir, namespace_id)? {
            return Ok(false);
        }
        ui.set_namespace_opened(namespace_id, opened)?;
        Ok(true)
    })
    .await;
    match res {
        Ok(Ok(true)) => resp::success_empty(),
        Ok(Ok(false)) => {
            resp::not_found(&format!("Namespace with id {namespace_id} does not exist."))
        }
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `PATCH /api/v1/chart/{chart_id}/status` — body `{status}` (1 pin, 0 normal, -1 hide).
/// Returns `{groups: [...]}` — the recomputed namespaces with full chart objects inlined.
pub async fn patch_chart_status(
    State(st): State<AppState>,
    Path(chart_id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    if missing(&body, "status") {
        return resp::params_error("Request parameter 'status'");
    }
    let status = match body.get("status").unwrap().as_i64() {
        Some(s) => s,
        None => return resp::params_error("Request parameter 'status'"),
    };

    let logdir: Arc<PathBuf> = st.logdir.clone();
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        if db::chart_owner(&logdir, chart_id)?.is_none() {
            return Ok(None);
        }
        ui.set_chart_status(chart_id, status)?;
        let overlay = ui.snapshot();
        db::chart_status_groups(&logdir, chart_id, &overlay)
    })
    .await;
    match res {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::params_error("chart_id not exist"),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}

/// `PATCH /api/v1/experiment/{experiment_id}/show` — body `{show}`.
/// Returns `{experiment: <Experiment.__dict__()>}` with the merged `show`.
pub async fn patch_experiment_show(
    State(st): State<AppState>,
    Path(experiment_id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    if missing(&body, "show") {
        return resp::params_error("Request parameter 'show'");
    }
    let show = truthy_to_int(body.get("show").unwrap());

    let logdir: Arc<PathBuf> = st.logdir.clone();
    let ui = st.ui.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        if !db::experiment_exists(&logdir, experiment_id)? {
            return Ok(None);
        }
        ui.set_experiment_show(experiment_id, show)?;
        let overlay = ui.snapshot();
        Ok(db::experiment_dict(&logdir, experiment_id, &overlay)?
            .map(|exp| json!({ "experiment": exp })))
    })
    .await;
    match res {
        Ok(Ok(Some(v))) => resp::success(v),
        Ok(Ok(None)) => resp::not_found(&format!(
            "Experiment with id {experiment_id} does not exist."
        )),
        Ok(Err(e)) => resp::data_error(&e.to_string()),
        Err(e) => resp::data_error(&e.to_string()),
    }
}
