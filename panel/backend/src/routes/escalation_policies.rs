//! Phase 4 W3: escalation policy admin API.
//!
//! Admin-only CRUD over `escalation_policies`. Policies are referenced
//! from `alert_rules.escalation_policy_id` (nullable FK with ON DELETE
//! SET NULL — operator removing a policy reverts every rule that used it
//! back to the pre-W3 hardcoded 15/30-min cadence).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AdminUser;
use crate::error::{err, internal_error, ApiError};
use crate::models::EscalationStep;
use crate::AppState;

#[derive(Serialize, sqlx::FromRow)]
pub struct PolicyDto {
    pub id: Uuid,
    pub name: String,
    pub steps: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How many alert_rules currently reference this policy. Surfaced in
    /// the admin list so operators know what they're about to detach when
    /// they hit Delete.
    #[sqlx(default)]
    pub used_by_rule_count: i64,
}

#[derive(Deserialize)]
pub struct PolicyInput {
    pub name: String,
    pub steps: Vec<EscalationStep>,
}

fn validate_input(input: &PolicyInput) -> Result<(), ApiError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1-200 characters",
        ));
    }
    if input.steps.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "policy must contain at least one step",
        ));
    }
    if input.steps.len() > 10 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "policy cannot exceed 10 steps",
        ));
    }
    if input.steps[0].after_minutes != 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "first step must have after_minutes = 0",
        ));
    }

    // Strictly increasing after_minutes.
    for pair in input.steps.windows(2) {
        if pair[1].after_minutes <= pair[0].after_minutes {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "after_minutes must strictly increase across steps",
            ));
        }
    }

    // Route shape validation.
    for (i, step) in input.steps.iter().enumerate() {
        validate_route(&step.route).map_err(|m| {
            err(
                StatusCode::BAD_REQUEST,
                &format!("step {i}: {m}"),
            )
        })?;
    }
    Ok(())
}

fn validate_route(route: &str) -> Result<(), &'static str> {
    if route == "all_channels" {
        return Ok(());
    }
    if let Some(uuid_str) = route.strip_prefix("on_call_schedule:") {
        Uuid::parse_str(uuid_str)
            .map(|_| ())
            .map_err(|_| "invalid schedule UUID after on_call_schedule:")
    } else if let Some(uuid_str) = route.strip_prefix("user:") {
        Uuid::parse_str(uuid_str)
            .map(|_| ())
            .map_err(|_| "invalid user UUID after user:")
    } else if let Some(url) = route.strip_prefix("webhook:") {
        if url.starts_with("http://") || url.starts_with("https://") {
            Ok(())
        } else {
            Err("webhook route must use http:// or https:// scheme")
        }
    } else {
        Err("route must be one of: on_call_schedule:<uuid>, user:<uuid>, all_channels, webhook:<url>")
    }
}

/// GET /api/escalation-policies — Admin: list all policies.
pub async fn list_policies(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Vec<PolicyDto>>, ApiError> {
    let rows: Vec<PolicyDto> = sqlx::query_as(
        "SELECT p.id, p.name, p.steps, p.created_at, p.updated_at, \
                COALESCE((SELECT COUNT(*) FROM alert_rules WHERE escalation_policy_id = p.id), 0) AS used_by_rule_count \
         FROM escalation_policies p ORDER BY p.name ASC",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| internal_error("list policies", e))?;
    Ok(Json(rows))
}

/// GET /api/escalation-policies/{id} — Admin: fetch a single policy.
pub async fn get_policy(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> Result<Json<PolicyDto>, ApiError> {
    let row: Option<PolicyDto> = sqlx::query_as(
        "SELECT p.id, p.name, p.steps, p.created_at, p.updated_at, \
                COALESCE((SELECT COUNT(*) FROM alert_rules WHERE escalation_policy_id = p.id), 0) AS used_by_rule_count \
         FROM escalation_policies p WHERE p.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| internal_error("get policy", e))?;
    match row {
        Some(p) => Ok(Json(p)),
        None => Err(err(StatusCode::NOT_FOUND, "Policy not found")),
    }
}

/// POST /api/escalation-policies — Admin: create a policy.
pub async fn create_policy(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(input): Json<PolicyInput>,
) -> Result<Json<PolicyDto>, ApiError> {
    validate_input(&input)?;

    let steps_json = serde_json::to_value(&input.steps)
        .map_err(|e| internal_error("serialize policy steps", e))?;

    let row: PolicyDto = sqlx::query_as(
        "INSERT INTO escalation_policies (name, steps) \
         VALUES ($1, $2) \
         RETURNING id, name, steps, created_at, updated_at, 0::bigint AS used_by_rule_count",
    )
    .bind(input.name.trim())
    .bind(&steps_json)
    .fetch_one(&state.db)
    .await
    .map_err(|e| internal_error("create policy", e))?;
    Ok(Json(row))
}

/// PUT /api/escalation-policies/{id} — Admin: replace a policy.
pub async fn update_policy(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
    Json(input): Json<PolicyInput>,
) -> Result<Json<PolicyDto>, ApiError> {
    validate_input(&input)?;

    let steps_json = serde_json::to_value(&input.steps)
        .map_err(|e| internal_error("serialize policy steps", e))?;

    let row: Option<PolicyDto> = sqlx::query_as(
        "UPDATE escalation_policies \
         SET name = $2, steps = $3, updated_at = NOW() \
         WHERE id = $1 \
         RETURNING id, name, steps, created_at, updated_at, \
            COALESCE((SELECT COUNT(*) FROM alert_rules WHERE escalation_policy_id = id), 0) AS used_by_rule_count",
    )
    .bind(id)
    .bind(input.name.trim())
    .bind(&steps_json)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| internal_error("update policy", e))?;

    match row {
        Some(p) => Ok(Json(p)),
        None => Err(err(StatusCode::NOT_FOUND, "Policy not found")),
    }
}

/// DELETE /api/escalation-policies/{id} — Admin: delete a policy.
///
/// `alert_rules.escalation_policy_id` is `ON DELETE SET NULL`, so any rule
/// that referenced this policy reverts to the pre-W3 default cadence.
/// In-flight alerts whose `escalation_step_index > 0` keep their index but
/// won't advance further (next tick finds no policy to chain against).
pub async fn delete_policy(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = sqlx::query("DELETE FROM escalation_policies WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| internal_error("delete policy", e))?;
    if result.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "Policy not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_route_accepts_all_channels() {
        assert!(validate_route("all_channels").is_ok());
    }

    #[test]
    fn validate_route_accepts_well_formed_schedule_route() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert!(validate_route(&format!("on_call_schedule:{uuid}")).is_ok());
    }

    #[test]
    fn validate_route_accepts_well_formed_user_route() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert!(validate_route(&format!("user:{uuid}")).is_ok());
    }

    #[test]
    fn validate_route_accepts_https_webhook() {
        assert!(validate_route("webhook:https://hooks.example.com/abc").is_ok());
    }

    #[test]
    fn validate_route_rejects_unknown_shape() {
        assert!(validate_route("garbage").is_err());
        assert!(validate_route("phone:555-0100").is_err());
    }

    #[test]
    fn validate_route_rejects_bad_uuid() {
        assert!(validate_route("on_call_schedule:not-a-uuid").is_err());
        assert!(validate_route("user:also-not").is_err());
    }

    #[test]
    fn validate_route_rejects_non_http_webhook() {
        assert!(validate_route("webhook:ftp://nope").is_err());
        assert!(validate_route("webhook:").is_err());
    }

    #[test]
    fn validate_input_rejects_empty_steps() {
        let input = PolicyInput {
            name: "test".to_string(),
            steps: vec![],
        };
        assert!(validate_input(&input).is_err());
    }

    #[test]
    fn validate_input_rejects_first_step_with_nonzero_after_minutes() {
        let input = PolicyInput {
            name: "test".to_string(),
            steps: vec![EscalationStep {
                after_minutes: 5,
                route: "all_channels".to_string(),
            }],
        };
        assert!(validate_input(&input).is_err());
    }

    #[test]
    fn validate_input_rejects_non_increasing_after_minutes() {
        let input = PolicyInput {
            name: "test".to_string(),
            steps: vec![
                EscalationStep {
                    after_minutes: 0,
                    route: "all_channels".to_string(),
                },
                EscalationStep {
                    after_minutes: 5,
                    route: "all_channels".to_string(),
                },
                EscalationStep {
                    after_minutes: 5,
                    route: "all_channels".to_string(),
                },
            ],
        };
        assert!(validate_input(&input).is_err());
    }

    #[test]
    fn validate_input_accepts_well_formed_three_step_chain() {
        let input = PolicyInput {
            name: "On-call escalation".to_string(),
            steps: vec![
                EscalationStep {
                    after_minutes: 0,
                    route: "on_call_schedule:550e8400-e29b-41d4-a716-446655440000".to_string(),
                },
                EscalationStep {
                    after_minutes: 5,
                    route: "user:550e8400-e29b-41d4-a716-446655440000".to_string(),
                },
                EscalationStep {
                    after_minutes: 15,
                    route: "all_channels".to_string(),
                },
            ],
        };
        assert!(validate_input(&input).is_ok());
    }

    #[test]
    fn validate_input_rejects_more_than_ten_steps() {
        let mut steps = Vec::new();
        for i in 0..11 {
            steps.push(EscalationStep {
                after_minutes: i as i32 * 5,
                route: "all_channels".to_string(),
            });
        }
        let input = PolicyInput {
            name: "test".to_string(),
            steps,
        };
        assert!(validate_input(&input).is_err());
    }
}
