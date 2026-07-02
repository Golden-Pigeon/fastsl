//! swanboard-compatible response envelope: `{"code":0,"message":"success","data":{...}}`.
//! Mirrors `swanboard/module/resp.py` (codes: 0 ok, 3404 not-found, 3409 conflict, 3500 data error).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

/// 200 success with a `data` object.
pub fn success(data: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"code": 0, "message": "success", "data": data})),
    )
        .into_response()
}

/// 200 success with no `data` field — mirrors swanboard's `SUCCESS_200(None)`
/// (`_ResponseBody` omits `data` when it is `None`).
pub fn success_empty() -> Response {
    (
        StatusCode::OK,
        Json(json!({"code": 0, "message": "success"})),
    )
        .into_response()
}

/// 422 request-parameter error (code 3422) — mirrors swanboard's `PARAMS_ERROR_422`.
pub fn params_error(message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"code": 3422, "message": message})),
    )
        .into_response()
}

/// 404 resource not found (code 3404).
pub fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"code": 3404, "message": message})),
    )
        .into_response()
}

/// 500 server-side data format error (code 3500).
pub fn data_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"code": 3500, "message": message})),
    )
        .into_response()
}
