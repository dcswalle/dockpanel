//! Phase 4 W3: on-call rotation admin API.
//!
//! Admin-only CRUD over `on_call_schedules` plus a `/whoami` endpoint that
//! lets non-admin operators check whether they're currently on-call without
//! exposing the schedule layout to them.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{AdminUser, AuthUser};
use crate::error::{err, internal_error, ApiError};
use crate::services::on_call::resolve_on_call_user;
use crate::AppState;

#[derive(Serialize)]
pub struct MemberInfo {
    pub id: Uuid,
    pub email: String,
}

/// Surface shape: members + current-rotation pointer resolved to `{id, email}`
/// pairs so the UI can render email chips without N+1 round-trips.
#[derive(Serialize)]
pub struct OnCallScheduleDto {
    pub id: Uuid,
    pub name: String,
    pub cadence_days: i32,
    pub anchor_at: DateTime<Utc>,
    /// Members in rotation order. Orphan UUIDs (FK target deleted) appear
    /// with `email = "(deleted user)"` so the operator can prune them.
    pub members: Vec<MemberInfo>,
    pub current_on_call: Option<MemberInfo>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct ScheduleInput {
    pub name: String,
    pub members: Vec<Uuid>,
    pub cadence_days: i32,
    /// Optional anchor override. Defaults to NOW() on create; on update the
    /// stored value is preserved when this field is absent so cadence math
    /// doesn't drift every PUT.
    #[serde(default)]
    pub anchor_at: Option<DateTime<Utc>>,
}

async fn load_member_emails(pool: &sqlx::PgPool, ids: &[Uuid]) -> Vec<MemberInfo> {
    if ids.is_empty() {
        return Vec::new();
    }
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, email FROM users WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    // Preserve the schedule's stored ordering rather than DB row order.
    ids.iter()
        .map(|id| {
            let email = rows
                .iter()
                .find(|(uid, _)| uid == id)
                .map(|(_, e)| e.clone())
                .unwrap_or_else(|| "(deleted user)".to_string());
            MemberInfo { id: *id, email }
        })
        .collect()
}

async fn schedule_to_dto(
    pool: &sqlx::PgPool,
    id: Uuid,
    name: String,
    members: Vec<Uuid>,
    cadence_days: i32,
    anchor_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
) -> OnCallScheduleDto {
    let member_info = load_member_emails(pool, &members).await;
    let current_uid = resolve_on_call_user(pool, id, Utc::now()).await;
    let current = current_uid.and_then(|uid| {
        member_info.iter().find(|m| m.id == uid).map(|m| MemberInfo {
            id: m.id,
            email: m.email.clone(),
        })
    });
    OnCallScheduleDto {
        id,
        name,
        cadence_days,
        anchor_at,
        members: member_info,
        current_on_call: current,
        created_at,
        updated_at,
    }
}

fn validate_input(input: &ScheduleInput) -> Result<(), ApiError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1-200 characters",
        ));
    }
    if input.members.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "members list cannot be empty",
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(input.members.len());
    for m in &input.members {
        if !seen.insert(*m) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "members list contains duplicate user IDs",
            ));
        }
    }
    if !(1..=90).contains(&input.cadence_days) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "cadence_days must be between 1 and 90",
        ));
    }
    Ok(())
}

async fn validate_members_exist(
    pool: &sqlx::PgPool,
    members: &[Uuid],
) -> Result<(), ApiError> {
    let found: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE id = ANY($1)",
    )
    .bind(members)
    .fetch_one(pool)
    .await
    .map_err(|e| internal_error("validate members", e))?;
    if (found as usize) != members.len() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "one or more member IDs do not match existing users",
        ));
    }
    Ok(())
}

/// GET /api/on-call/schedules — Admin: list all rotation schedules.
pub async fn list_schedules(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Vec<OnCallScheduleDto>>, ApiError> {
    let rows: Vec<(Uuid, String, Vec<Uuid>, i32, DateTime<Utc>, DateTime<Utc>, DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT id, name, members, cadence_days, anchor_at, created_at, updated_at \
             FROM on_call_schedules ORDER BY name ASC",
        )
        .fetch_all(&state.db)
        .await
        .map_err(|e| internal_error("list schedules", e))?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, name, members, cadence, anchor, created, updated) in rows {
        out.push(
            schedule_to_dto(&state.db, id, name, members, cadence, anchor, created, updated)
                .await,
        );
    }
    Ok(Json(out))
}

/// GET /api/on-call/schedules/{id} — Admin: fetch one rotation by id.
pub async fn get_schedule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> Result<Json<OnCallScheduleDto>, ApiError> {
    let row: Option<(Uuid, String, Vec<Uuid>, i32, DateTime<Utc>, DateTime<Utc>, DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT id, name, members, cadence_days, anchor_at, created_at, updated_at \
             FROM on_call_schedules WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| internal_error("get schedule", e))?;

    let Some((id, name, members, cadence, anchor, created, updated)) = row else {
        return Err(err(StatusCode::NOT_FOUND, "Schedule not found"));
    };

    Ok(Json(
        schedule_to_dto(&state.db, id, name, members, cadence, anchor, created, updated).await,
    ))
}

/// POST /api/on-call/schedules — Admin: create a rotation.
pub async fn create_schedule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(input): Json<ScheduleInput>,
) -> Result<Json<OnCallScheduleDto>, ApiError> {
    validate_input(&input)?;
    validate_members_exist(&state.db, &input.members).await?;

    let anchor = input.anchor_at.unwrap_or_else(Utc::now);
    let row: (Uuid, String, Vec<Uuid>, i32, DateTime<Utc>, DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as(
            "INSERT INTO on_call_schedules (name, members, cadence_days, anchor_at) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, name, members, cadence_days, anchor_at, created_at, updated_at",
        )
        .bind(input.name.trim())
        .bind(&input.members)
        .bind(input.cadence_days)
        .bind(anchor)
        .fetch_one(&state.db)
        .await
        .map_err(|e| internal_error("create schedule", e))?;

    Ok(Json(
        schedule_to_dto(&state.db, row.0, row.1, row.2, row.3, row.4, row.5, row.6).await,
    ))
}

/// PUT /api/on-call/schedules/{id} — Admin: update an existing rotation.
pub async fn update_schedule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
    Json(input): Json<ScheduleInput>,
) -> Result<Json<OnCallScheduleDto>, ApiError> {
    validate_input(&input)?;
    validate_members_exist(&state.db, &input.members).await?;

    let row: Option<(Uuid, String, Vec<Uuid>, i32, DateTime<Utc>, DateTime<Utc>, DateTime<Utc>)> =
        if let Some(anchor) = input.anchor_at {
            sqlx::query_as(
                "UPDATE on_call_schedules \
                 SET name = $2, members = $3, cadence_days = $4, anchor_at = $5, updated_at = NOW() \
                 WHERE id = $1 \
                 RETURNING id, name, members, cadence_days, anchor_at, created_at, updated_at",
            )
            .bind(id)
            .bind(input.name.trim())
            .bind(&input.members)
            .bind(input.cadence_days)
            .bind(anchor)
            .fetch_optional(&state.db)
            .await
        } else {
            // No anchor in payload → preserve existing anchor so cadence math
            // doesn't reset on every save (e.g. when only re-ordering members).
            sqlx::query_as(
                "UPDATE on_call_schedules \
                 SET name = $2, members = $3, cadence_days = $4, updated_at = NOW() \
                 WHERE id = $1 \
                 RETURNING id, name, members, cadence_days, anchor_at, created_at, updated_at",
            )
            .bind(id)
            .bind(input.name.trim())
            .bind(&input.members)
            .bind(input.cadence_days)
            .fetch_optional(&state.db)
            .await
        }
        .map_err(|e| internal_error("update schedule", e))?;

    let Some((id, name, members, cadence, anchor, created, updated)) = row else {
        return Err(err(StatusCode::NOT_FOUND, "Schedule not found"));
    };

    Ok(Json(
        schedule_to_dto(&state.db, id, name, members, cadence, anchor, created, updated).await,
    ))
}

/// DELETE /api/on-call/schedules/{id} — Admin: remove a rotation.
///
/// Escalation-policy step routes referencing this schedule's UUID are NOT
/// auto-rewritten — the foreign reference is opaque to the FK system. The
/// orphan-route sweep (alert_engine) detects and rewrites them on its next
/// hourly tick.
pub async fn delete_schedule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = sqlx::query("DELETE FROM on_call_schedules WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| internal_error("delete schedule", e))?;

    if result.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "Schedule not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/on-call/whoami — Any authenticated user: am I currently on-call?
///
/// Returns the list of schedules where this user is the current rotation
/// holder. Empty array = not on-call right now.
pub async fn whoami(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let schedules: Vec<(Uuid, String, Vec<Uuid>, i32, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, name, members, cadence_days, anchor_at FROM on_call_schedules",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| internal_error("whoami", e))?;

    let mut out = Vec::new();
    let now = Utc::now();
    for (id, name, _members, _cadence, _anchor) in schedules {
        // Re-resolve via the helper rather than recompute inline so the math
        // stays in exactly one place.
        if let Some(current_uid) = resolve_on_call_user(&state.db, id, now).await {
            if current_uid == claims.sub {
                out.push(serde_json::json!({
                    "schedule_id": id,
                    "schedule_name": name,
                }));
            }
        }
    }
    Ok(Json(out))
}
