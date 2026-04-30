//! Phase 4 W1.2.d: Drill scheduler — runs every 60s, evaluates per-policy
//! drill cron schedules, dispatches end-to-end drills against the latest
//! db + volume backup tied to each due policy.
//!
//! Site backups don't carry `policy_id`, so they're not covered by this
//! scheduler — `backup_verifier::run` (every 6h) handles passive site
//! verification instead. If site policy_id ever lands, this loop should
//! pick up sites with no other change.

use chrono::{Datelike, Timelike};
use sqlx::PgPool;
use uuid::Uuid;

use crate::services::agent::AgentClient;

#[derive(sqlx::FromRow)]
struct DuePolicy {
    id: Uuid,
    name: String,
    drill_schedule: String,
    last_drill_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn run(
    db: PgPool,
    agent: AgentClient,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    tracing::info!("Drill scheduler started");

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = tick(&db, &agent).await {
                    tracing::error!("Drill scheduler error: {e}");
                }
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("Drill scheduler shutting down gracefully");
                break;
            }
        }
    }
}

async fn tick(db: &PgPool, agent: &AgentClient) -> Result<(), String> {
    let now = chrono::Utc::now();

    let policies: Vec<DuePolicy> = sqlx::query_as(
        "SELECT id, name, drill_schedule, last_drill_at \
         FROM backup_policies \
         WHERE enabled = TRUE AND drill_enabled = TRUE"
    )
    .fetch_all(db).await.map_err(|e| e.to_string())?;

    for policy in &policies {
        if !cron_matches_now(&policy.drill_schedule, &now) {
            continue;
        }
        // 90s debounce — same window as backup_policy_executor.
        if let Some(last) = policy.last_drill_at {
            if (now - last).num_seconds() < 90 {
                continue;
            }
        }

        tracing::info!("Drill schedule due for policy '{}' ({})", policy.name, policy.id);

        // Stamp last_drill_at first so a slow agent dispatch can't double-fire
        // on the next tick.
        let _ = sqlx::query(
            "UPDATE backup_policies SET last_drill_at = NOW() WHERE id = $1"
        ).bind(policy.id).execute(db).await;

        dispatch_db_drill(db, agent, policy.id).await;
        dispatch_volume_drill(db, agent, policy.id).await;
    }

    Ok(())
}

/// Find the latest db backup tied to the policy and dispatch a drill.
/// Skip if a drill is already running for that server (concurrency cap = 1/server).
async fn dispatch_db_drill(db: &PgPool, agent: &AgentClient, policy_id: Uuid) {
    let row: Option<(Uuid, Option<Uuid>, String, String, String)> = sqlx::query_as(
        "SELECT id, server_id, db_type, db_name, filename \
         FROM database_backups \
         WHERE policy_id = $1 \
         ORDER BY created_at DESC LIMIT 1"
    ).bind(policy_id).fetch_optional(db).await.ok().flatten();

    let Some((backup_id, server_id, db_type, db_name, filename)) = row else {
        tracing::debug!("No db backup tied to policy {policy_id}; skipping db drill");
        return;
    };

    if has_running_drill_for_server(db, server_id).await {
        tracing::info!("Skipping db drill for policy {policy_id} — another drill running on same server");
        return;
    }

    enqueue_drill(
        db, agent, "database", backup_id, server_id,
        serde_json::json!({
            "db_type": db_type,
            "db_name": db_name,
            "filename": filename,
        }),
        "/backups/drill/db",
    ).await;
}

async fn dispatch_volume_drill(db: &PgPool, agent: &AgentClient, policy_id: Uuid) {
    let row: Option<(Uuid, Option<Uuid>, String, String)> = sqlx::query_as(
        "SELECT id, server_id, container_name, filename \
         FROM volume_backups \
         WHERE policy_id = $1 \
         ORDER BY created_at DESC LIMIT 1"
    ).bind(policy_id).fetch_optional(db).await.ok().flatten();

    let Some((backup_id, server_id, container_name, filename)) = row else {
        tracing::debug!("No volume backup tied to policy {policy_id}; skipping volume drill");
        return;
    };

    if has_running_drill_for_server(db, server_id).await {
        tracing::info!("Skipping volume drill for policy {policy_id} — another drill running on same server");
        return;
    }

    enqueue_drill(
        db, agent, "volume", backup_id, server_id,
        serde_json::json!({
            "container_name": container_name,
            "filename": filename,
        }),
        "/backups/drill/volume",
    ).await;
}

async fn has_running_drill_for_server(db: &PgPool, server_id: Option<Uuid>) -> bool {
    let Some(sid) = server_id else { return false };
    let count: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM backup_drills \
         WHERE server_id = $1 AND status IN ('pending', 'running')"
    ).bind(sid).fetch_one(db).await.ok();
    count.map(|(n,)| n > 0).unwrap_or(false)
}

/// Insert a `running` row in backup_drills and fire the agent call async.
/// Mirrors the trigger_drill route handler's shape so on-demand and scheduled
/// drills end up indistinguishable in the audit trail (backup_drills + Drills tab).
async fn enqueue_drill(
    db: &PgPool,
    agent: &AgentClient,
    backup_type: &str,
    backup_id: Uuid,
    server_id: Option<Uuid>,
    body: serde_json::Value,
    agent_path: &str,
) {
    // INSERT … RETURNING the new id. triggered_by NULL = scheduler-fired.
    let drill_row: Result<(Uuid,), _> = sqlx::query_as(
        "INSERT INTO backup_drills (backup_type, backup_id, server_id, status, started_at) \
         VALUES ($1, $2, $3, 'running', NOW()) RETURNING id"
    )
    .bind(backup_type).bind(backup_id).bind(server_id)
    .fetch_one(db).await;

    let drill_id = match drill_row {
        Ok((id,)) => id,
        Err(e) => {
            tracing::error!("Failed to insert scheduled drill row: {e}");
            return;
        }
    };

    let agent = agent.clone();
    let db = db.clone();
    let agent_path = agent_path.to_string();
    tokio::spawn(async move {
        let result = agent.post(&agent_path, Some(body)).await.map_err(|e| e.to_string());

        match result {
            Ok(data) => {
                let passed = data.get("passed").and_then(|v| v.as_bool()).unwrap_or(false);
                let http_status = data.get("http_status").and_then(|v| v.as_i64()).map(|n| n as i32);
                let body_excerpt = data.get("body_excerpt").and_then(|v| v.as_str()).map(|s| s.to_string());
                let error_message = data.get("error_message").and_then(|v| v.as_str()).map(|s| s.to_string());
                let duration_ms = data.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

                let _ = sqlx::query(
                    "UPDATE backup_drills SET \
                     status = $2, http_status = $3, body_excerpt = $4, \
                     error_message = $5, duration_ms = $6, completed_at = NOW() \
                     WHERE id = $1"
                )
                .bind(drill_id)
                .bind(if passed { "passed" } else { "failed" })
                .bind(http_status).bind(body_excerpt)
                .bind(error_message).bind(duration_ms)
                .execute(&db).await;
            }
            Err(e) => {
                let _ = sqlx::query(
                    "UPDATE backup_drills SET status = 'failed', error_message = $2, completed_at = NOW() WHERE id = $1"
                ).bind(drill_id).bind(&e).execute(&db).await;
            }
        }
    });
}

// ── 5-field cron parser (parity with backup_policy_executor) ────────────────

fn cron_matches_now(schedule: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    let checks = [
        (fields[0], now.minute() as i32),
        (fields[1], now.hour() as i32),
        (fields[2], now.day() as i32),
        (fields[3], now.month() as i32),
        (fields[4], now.weekday().num_days_from_sunday() as i32),
    ];
    checks.iter().all(|(f, v)| field_matches(f, *v))
}

fn field_matches(field: &str, value: i32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(stripped) = field.strip_prefix("*/") {
        if let Ok(step) = stripped.parse::<i32>() {
            return step > 0 && value % step == 0;
        }
        return false;
    }
    field.split(',').any(|part| {
        if let Some((lo, hi)) = part.split_once('-') {
            match (lo.parse::<i32>(), hi.parse::<i32>()) {
                (Ok(l), Ok(h)) => value >= l && value <= h,
                _ => false,
            }
        } else {
            part.parse::<i32>().map(|n| n == value).unwrap_or(false)
        }
    })
}
