use axum::{
    http::StatusCode,
    routing::post,
    Json, Router,
};

use super::{is_valid_domain, AppState};
use crate::services::backup_drill;

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: &str) -> ApiErr {
    (status, Json(serde_json::json!({ "error": msg })))
}

#[derive(serde::Deserialize)]
pub struct DrillSiteRequest {
    pub domain: String,
    pub filename: String,
}

/// POST /backups/drill/site — End-to-end site drill.
async fn drill_site(
    Json(req): Json<DrillSiteRequest>,
) -> Result<Json<backup_drill::DrillResult>, ApiErr> {
    if !is_valid_domain(&req.domain) {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid domain"));
    }
    if req.filename.is_empty() || req.filename.contains("..") || req.filename.contains('/') {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid filename"));
    }
    let result = backup_drill::drill_site_backup(&req.domain, &req.filename)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct DrillDbRequest {
    pub db_type: String,
    pub db_name: String,
    pub filename: String,
}

/// POST /backups/drill/db — DB drill: scratch engine container + full restore + row probe.
async fn drill_db(
    Json(req): Json<DrillDbRequest>,
) -> Result<Json<backup_drill::DrillResult>, ApiErr> {
    // db_type whitelist mirrors the engines we know how to spin.
    match req.db_type.as_str() {
        "mysql" | "mariadb" | "postgres" | "postgresql" => {}
        _ => return Err(err(StatusCode::BAD_REQUEST, "Unsupported db_type")),
    }
    let result = backup_drill::drill_db_backup(&req.db_type, &req.db_name, &req.filename)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    Ok(Json(result))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/backups/drill/site", post(drill_site))
        .route("/backups/drill/db", post(drill_db))
}
