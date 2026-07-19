//! Phase 4 W4: panel self-update orchestrator.
//!
//! Lifts `scripts/update.sh`'s SSH-only flow into a panel-UI-driven action.
//! Does NOT reimplement the binary swap or rollback — those live in
//! `update.sh:430-499` and are battle-tested. The orchestrator's job is:
//!
//!   1. Create a persistent snapshot via [`super::panel_snapshot`].
//!   2. Shell out to `update.sh` with `DOCKPANEL_NO_SELF_REFRESH=1` +
//!      `DOCKPANEL_VERSION=$target` so a single subprocess invocation does
//!      the work without mid-flight re-exec.
//!   3. Track state in two places:
//!      - In-process `Arc<RwLock<UpdateState>>` for fast guards against
//!        concurrent applies within one process lifetime.
//!      - `panel_snapshots` rows in the DB for cross-restart truth (the
//!        api process dies mid-swap when update.sh restarts services).
//!   4. On the next process boot, [`finalize_pending_on_startup`] closes
//!      out any in-flight rows by writing `to_version =
//!      CARGO_PKG_VERSION`. Equal `from_version`/`to_version` ⇒ rollback
//!      happened.
//!
//! Out of scope here:
//!   - In-flight rollback is `update.sh`'s `.bak` restore — orchestrator
//!     doesn't touch it.
//!   - Operator-triggered rollback from a snapshot is
//!     [`rollback_to_snapshot`] — restores binaries + DB + /etc/dockpanel
//!     and bounces services. Routes/UI gate this behind a destructive
//!     confirm.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::models::PanelSnapshot;
use crate::services::panel_snapshot::{self, SnapshotTrigger};

/// Path to `scripts/update.sh` on a panel install. setup.sh + update.sh
/// both lay the source tree under `/opt/dockpanel`, so this is stable.
const UPDATE_SCRIPT: &str = "/opt/dockpanel/scripts/update.sh";

/// A snapshot row is considered "in flight" if it has `to_version IS NULL`
/// and was created within this window. Older rows that never finalized are
/// dead and don't block new applies.
///
/// Manual snapshots are excluded everywhere this predicate is used: they are
/// taken on demand and legitimately keep `to_version` NULL forever (see the
/// `panel_snapshots` migration), so treating them as unfinalized updates made
/// a safety snapshot look like an update in progress — and let the
/// abandoned-sweep below stamp them `to_version = 'abandoned'`, which
/// `current_state` then reported as a successful update to "abandoned".
const IN_FLIGHT_WINDOW_MIN: i64 = 15;

/// Maximum length of the captured `update.sh` stdout tail kept in memory
/// for the in-process state.
const LOG_TAIL_MAX: usize = 64;

// ── State ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateState {
    /// No update is currently running.
    Idle,
    /// `update.sh` is executing. The orchestrator may not survive long
    /// enough to transition out of this (the api binary swap will kill
    /// this process), so the DB row is the durable signal.
    InFlight {
        target_version: String,
        snapshot_id: Uuid,
        started_at: DateTime<Utc>,
        last_log_line: Option<String>,
    },
    /// Reconstructed at request-time from the snapshot row when the api
    /// reboots into the new version. `from_version != to_version`.
    Succeeded {
        from_version: String,
        to_version: String,
        completed_at: DateTime<Utc>,
    },
    /// `update.sh`'s in-flight `.bak` rollback fired and the api came back
    /// on the original binary. `from_version == to_version`.
    RolledBack {
        attempted_version: String,
        snapshot_id: Uuid,
        completed_at: DateTime<Utc>,
    },
    /// Orchestrator failed before `update.sh` could run (snapshot error,
    /// validation, missing script, etc.).
    Failed {
        reason: String,
        at: DateTime<Utc>,
    },
}

pub type UpdateStateHandle = Arc<RwLock<UpdateState>>;

pub fn new_state_handle() -> UpdateStateHandle {
    Arc::new(RwLock::new(UpdateState::Idle))
}

// ── Validation ───────────────────────────────────────────────────────────

/// Accepts `vX.Y.Z`, `X.Y.Z`, `vX.Y.Z-rc.N`, `X.Y.Z-rc.N`. No other shapes.
/// Hand-rolled instead of pulling in `regex` for one expression.
pub fn validate_target_version(v: &str) -> bool {
    let v = v.trim_start_matches('v');
    let (core, suffix) = match v.split_once('-') {
        Some((c, s)) => (c, Some(s)),
        None => (v, None),
    };

    let core_parts: Vec<&str> = core.split('.').collect();
    if core_parts.len() != 3 {
        return false;
    }
    for p in &core_parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }

    if let Some(s) = suffix {
        let Some(n) = s.strip_prefix("rc.") else {
            return false;
        };
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

// ── Errors ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum OrchestratorError {
    InvalidTargetVersion(String),
    AlreadyInFlight,
    ScriptMissing(String),
    Snapshot(panel_snapshot::SnapshotError),
    Spawn(std::io::Error),
    Db(sqlx::Error),
}

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchestratorError::InvalidTargetVersion(v) => {
                write!(f, "invalid target version: {v}")
            }
            OrchestratorError::AlreadyInFlight => write!(
                f,
                "another update is already in flight (check /api/update/status)"
            ),
            OrchestratorError::ScriptMissing(p) => {
                write!(f, "update script not found at {p}")
            }
            OrchestratorError::Snapshot(e) => write!(f, "snapshot failed: {e}"),
            OrchestratorError::Spawn(e) => write!(f, "failed to spawn update.sh: {e}"),
            OrchestratorError::Db(e) => write!(f, "db error: {e}"),
        }
    }
}

impl std::error::Error for OrchestratorError {}

impl From<panel_snapshot::SnapshotError> for OrchestratorError {
    fn from(e: panel_snapshot::SnapshotError) -> Self {
        OrchestratorError::Snapshot(e)
    }
}

impl From<sqlx::Error> for OrchestratorError {
    fn from(e: sqlx::Error) -> Self {
        OrchestratorError::Db(e)
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Resolve the current state. In-memory `Idle` may be a lie if a prior
/// process died mid-update; we cross-check the DB for an in-flight row
/// before returning Idle.
pub async fn current_state(handle: &UpdateStateHandle, pool: &PgPool) -> UpdateState {
    {
        let s = handle.read().await;
        if !matches!(*s, UpdateState::Idle) {
            return s.clone();
        }
    }

    // In-memory state says idle. Check DB for an unfinalized in-flight row.
    // Manual snapshots never carry a `to_version`, so they must be excluded or
    // taking a safety snapshot would masquerade as an update in progress.
    let cutoff = Utc::now() - chrono::Duration::minutes(IN_FLIGHT_WINDOW_MIN);
    if let Ok(Some(snap)) = sqlx::query_as::<_, PanelSnapshot>(
        "SELECT * FROM panel_snapshots \
         WHERE to_version IS NULL AND trigger <> 'manual' AND created_at > $1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(cutoff)
    .fetch_optional(pool)
    .await
    {
        let target = parse_target_from_trigger(&snap.trigger).unwrap_or_default();
        return UpdateState::InFlight {
            target_version: target,
            snapshot_id: snap.id,
            started_at: snap.created_at,
            last_log_line: None,
        };
    }

    // Most recent finalized snapshot tells us if the last completed update
    // succeeded or rolled back (read-only summary for UI).
    if let Ok(Some(snap)) = sqlx::query_as::<_, PanelSnapshot>(
        "SELECT * FROM panel_snapshots \
         WHERE to_version IS NOT NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    {
        let from = snap.from_version.clone();
        let to = snap.to_version.clone().unwrap_or_default();
        let attempted = parse_target_from_trigger(&snap.trigger).unwrap_or_else(|| to.clone());
        if to == from && attempted != from {
            return UpdateState::RolledBack {
                attempted_version: attempted,
                snapshot_id: snap.id,
                completed_at: snap.created_at,
            };
        }
        if to != from {
            return UpdateState::Succeeded {
                from_version: from,
                to_version: to,
                completed_at: snap.created_at,
            };
        }
    }

    UpdateState::Idle
}

/// Start the panel-self-update flow.
///
/// 1. Validate target_version + in-flight guard.
/// 2. Create snapshot with trigger=pre-update:<target>.
/// 3. Spawn `update.sh` detached (process group of its own so SIGTERM to
///    the api during binary swap doesn't propagate up).
/// 4. Background task streams stdout into the state handle's
///    `last_log_line` until the api dies or update.sh finishes.
///
/// Returns the InFlight state to the caller; the actual apply progress is
/// observed via `current_state` polling.
pub async fn start_panel_update(
    handle: UpdateStateHandle,
    pool: PgPool,
    target_version: String,
    operator: Option<String>,
) -> Result<UpdateState, OrchestratorError> {
    let target_version = target_version.trim().to_string();
    if !validate_target_version(&target_version) {
        return Err(OrchestratorError::InvalidTargetVersion(target_version));
    }

    if !std::path::Path::new(UPDATE_SCRIPT).exists() {
        return Err(OrchestratorError::ScriptMissing(UPDATE_SCRIPT.into()));
    }

    // Concurrent-apply guard (in-process + DB).
    {
        let s = handle.read().await;
        if matches!(*s, UpdateState::InFlight { .. }) {
            return Err(OrchestratorError::AlreadyInFlight);
        }
    }
    let cutoff = Utc::now() - chrono::Duration::minutes(IN_FLIGHT_WINDOW_MIN);
    // Same exclusion as `current_state`: a manual snapshot is not an update, so
    // it must not lock out the one thing an operator does right after taking it.
    let in_flight_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM panel_snapshots \
         WHERE to_version IS NULL AND trigger <> 'manual' AND created_at > $1",
    )
    .bind(cutoff)
    .fetch_one(&pool)
    .await?;
    if in_flight_count.0 > 0 {
        return Err(OrchestratorError::AlreadyInFlight);
    }

    // Create the pre-update snapshot. If this fails, no state changes.
    let meta = panel_snapshot::create_snapshot(
        &pool,
        SnapshotTrigger::PreUpdate {
            target_version: target_version.clone(),
        },
        operator.clone(),
    )
    .await?;

    let started_at = Utc::now();
    let in_flight = UpdateState::InFlight {
        target_version: target_version.clone(),
        snapshot_id: meta.id,
        started_at,
        last_log_line: None,
    };
    *handle.write().await = in_flight.clone();

    // Spawn update.sh. Detached process group so systemctl stop of
    // dockpanel-api (issued by update.sh) doesn't propagate SIGTERM to
    // the script itself. PID1 reaps when complete.
    let mut cmd = Command::new("bash");
    cmd.arg(UPDATE_SCRIPT)
        .env("INSTALL_FROM_RELEASE", "1")
        .env("DOCKPANEL_NO_SELF_REFRESH", "1")
        .env("DOCKPANEL_VERSION", &target_version)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(OrchestratorError::Spawn)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let handle_clone = handle.clone();
    let target_clone = target_version.clone();
    tokio::spawn(async move {
        stream_update_output(handle_clone, stdout, stderr).await;
        // We may not reach here — update.sh kills the api midway. If we
        // do, log the exit status so the operator sees it in journals.
        match tokio::time::timeout(Duration::from_secs(900), child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(
                    "update.sh (target {target_clone}) exited with status {status}"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!("update.sh wait failed: {e}");
            }
            Err(_) => {
                tracing::warn!("update.sh wait timed out after 15min");
            }
        }
    });

    Ok(in_flight)
}

/// Stream `update.sh` stdout + stderr into the handle's `last_log_line`.
/// This loop exits when both pipes hit EOF — typically because the api
/// process is being killed by `systemctl stop dockpanel-api`.
async fn stream_update_output(
    handle: UpdateStateHandle,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
) {
    let stdout_task = stdout.map(|s| {
        let h = handle.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(s).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.is_empty() {
                    continue;
                }
                update_last_log(&h, &line).await;
                tracing::info!(target: "panel_update", "{line}");
            }
        })
    });
    let stderr_task = stderr.map(|s| {
        let h = handle.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(s).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.is_empty() {
                    continue;
                }
                update_last_log(&h, &line).await;
                tracing::warn!(target: "panel_update", "{line}");
            }
        })
    });
    if let Some(t) = stdout_task {
        let _ = t.await;
    }
    if let Some(t) = stderr_task {
        let _ = t.await;
    }
}

async fn update_last_log(handle: &UpdateStateHandle, line: &str) {
    let truncated = if line.len() > LOG_TAIL_MAX * 4 {
        line.chars().take(LOG_TAIL_MAX * 4).collect::<String>()
    } else {
        line.to_string()
    };
    let mut s = handle.write().await;
    if let UpdateState::InFlight { last_log_line, .. } = &mut *s {
        *last_log_line = Some(truncated);
    }
}

fn parse_target_from_trigger(trigger: &str) -> Option<String> {
    trigger.strip_prefix("pre-update:").map(|s| s.to_string())
}

/// Operator-triggered rollback. Stops services, restores binaries + DB +
/// /etc from snapshot, restarts services. The api process this code runs
/// in WILL be the one stopped — we shell out via setsid+nohup so the
/// orchestrator's restore subprocess survives.
///
/// Returns immediately once the rollback has been kicked off. Status is
/// observed via the new process's [`current_state`] after restart.
pub async fn rollback_to_snapshot(
    pool: PgPool,
    snapshot_id: Uuid,
) -> Result<(), OrchestratorError> {
    // Fetch + validate snapshot upfront so the user gets a synchronous
    // 4xx instead of a 202 + silent failure.
    let snap: Option<PanelSnapshot> =
        sqlx::query_as("SELECT * FROM panel_snapshots WHERE id = $1")
            .bind(snapshot_id)
            .fetch_optional(&pool)
            .await?;
    let snap = snap.ok_or_else(|| {
        OrchestratorError::Snapshot(panel_snapshot::SnapshotError::NotFound(snapshot_id))
    })?;
    if !std::path::Path::new(&snap.file_path).exists() {
        return Err(OrchestratorError::Snapshot(
            panel_snapshot::SnapshotError::FileMissing(snap.file_path.clone().into()),
        ));
    }

    // Run the restore in a detached shell process so the dockpanel-api
    // restart we're about to issue doesn't kill the restore script
    // partway through.
    let helper = format!(
        r#"#!/usr/bin/env bash
set -e
echo "[panel_update] rollback to snapshot {sid} starting" | systemd-cat -t dockpanel-rollback
systemctl stop dockpanel-api dockpanel-agent || true
sleep 1
# Restore via dockpanel-cli helper (or fall back to manual extract+cp).
{cli_invocation}
systemctl daemon-reload
systemctl start dockpanel-agent
sleep 1
systemctl start dockpanel-api
echo "[panel_update] rollback to snapshot {sid} complete" | systemd-cat -t dockpanel-rollback
"#,
        sid = snapshot_id,
        cli_invocation =
            format!("/usr/local/bin/dockpanel snapshot-restore {snapshot_id} >> /var/log/dockpanel-rollback.log 2>&1 || true"),
    );

    // Use a one-shot helper file in /tmp so the detached process has a
    // stable script path.
    let helper_path = format!("/tmp/dockpanel-rollback-{snapshot_id}.sh");
    tokio::fs::write(&helper_path, helper)
        .await
        .map_err(OrchestratorError::Spawn)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &helper_path,
            std::fs::Permissions::from_mode(0o755),
        );
    }

    // We CAN'T use the dockpanel-cli wrapper above on first ship — it
    // doesn't have a snapshot-restore subcommand yet. Inline the restore
    // by spawning the panel_snapshot path directly via a one-shot binary
    // call. For W4 ship #1, call panel_snapshot::restore_snapshot before
    // touching systemctl, then bounce services.
    panel_snapshot::restore_snapshot(&pool, snapshot_id).await?;

    // Bounce services via systemctl in a detached process so SIGTERM to
    // dockpanel-api doesn't kill the restart command.
    let mut bounce = Command::new("bash");
    bounce
        .arg("-c")
        .arg("(systemctl restart dockpanel-agent dockpanel-api) </dev/null >/dev/null 2>&1 &")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        bounce.process_group(0);
    }
    let _ = bounce.spawn().map_err(OrchestratorError::Spawn)?;

    // Clean up the helper file (no longer needed).
    let _ = tokio::fs::remove_file(&helper_path).await;

    Ok(())
}

/// At process startup, close out any in-flight snapshot rows by writing
/// `to_version = CARGO_PKG_VERSION`. Equal `from_version`/`to_version`
/// indicates an in-flight rollback by `update.sh`; differing values are a
/// successful apply.
pub async fn finalize_pending_on_startup(pool: &PgPool) {
    let cutoff = Utc::now() - chrono::Duration::minutes(IN_FLIGHT_WINDOW_MIN);
    let pending: Result<Vec<PanelSnapshot>, _> = sqlx::query_as(
        "SELECT * FROM panel_snapshots \
         WHERE to_version IS NULL AND trigger <> 'manual' AND created_at > $1 \
         ORDER BY created_at ASC",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await;

    let pending = match pending {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("finalize_pending: read failed: {e}");
            return;
        }
    };

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    for snap in &pending {
        let result = sqlx::query("UPDATE panel_snapshots SET to_version = $1 WHERE id = $2")
            .bind(&current_version)
            .bind(snap.id)
            .execute(pool)
            .await;
        match result {
            Ok(_) => {
                if current_version == snap.from_version {
                    tracing::warn!(
                        "Snapshot {} finalized as rolled-back (process restarted on \
                         pre-update version {})",
                        snap.id,
                        current_version
                    );
                } else {
                    tracing::info!(
                        "Snapshot {} finalized as succeeded ({} -> {})",
                        snap.id,
                        snap.from_version,
                        current_version
                    );
                }
            }
            Err(e) => {
                tracing::warn!("finalize_pending: row {} update failed: {e}", snap.id);
            }
        }
    }

    // Mark older, abandoned in-flight rows with a sentinel so they don't
    // perpetually block /api/update/apply. Manual snapshots are exempt — they
    // are never in flight, and stamping one 'abandoned' made it the newest
    // finalized row, which `current_state` reads as a completed update from the
    // running version to a version literally called "abandoned".
    if let Err(e) = sqlx::query(
        "UPDATE panel_snapshots SET to_version = 'abandoned' \
         WHERE to_version IS NULL AND trigger <> 'manual' AND created_at <= $1",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    {
        tracing::warn!("finalize_pending: abandoned-sweep failed: {e}");
    }
}

// ── Fleet (§4.5) ─────────────────────────────────────────────────────────
//
// Operator-initiated rolling update across the user's remote agents.
// Walks plan in order (oldest agent_version first; reachability gate
// excludes servers `last_seen_at > 5 min stale`), POSTs to each agent's
// `/panel/update`, polls `/panel/update/status` until terminal, records
// per-server progress in `fleet_update_runs.progress` JSONB. Halts on
// first failure unless `force_continue: true`.
//
// Per design memo §3.D5: `include_panel: false` default — fleet rolls
// first, panel itself last with a separate explicit click.

use serde::Deserialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetPlanRow {
    pub server_id: Uuid,
    pub name: String,
    pub agent_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetProgressRow {
    pub server_id: Uuid,
    pub status: String, // "pending" | "updating" | "succeeded" | "failed" | "skipped"
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
}

/// Build the ordered plan: oldest version first, ties broken by
/// `last_seen_at desc`. Skips servers staler than 5 minutes (the
/// reachability gate). Skips servers already at target_version.
pub async fn build_fleet_plan(
    pool: &PgPool,
    user_id: Uuid,
    target_version: &str,
) -> Result<Vec<FleetPlanRow>, sqlx::Error> {
    let target_clean = target_version.trim_start_matches('v').to_string();
    let rows: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT id, name, agent_version FROM servers \
         WHERE user_id = $1 \
           AND last_seen_at > NOW() - INTERVAL '5 minutes' \
           AND is_local = false \
         ORDER BY agent_version ASC NULLS FIRST, last_seen_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter(|(_, _, v)| {
            v.as_deref()
                .map(|cur| cur.trim_start_matches('v') != target_clean)
                .unwrap_or(true)
        })
        .map(|(server_id, name, agent_version)| FleetPlanRow {
            server_id,
            name,
            agent_version,
        })
        .collect())
}

/// Create a new fleet_update_runs row and return its id. The caller
/// then spawns a background task that walks the plan via
/// [`execute_fleet_plan`].
pub async fn create_fleet_run(
    pool: &PgPool,
    target_version: &str,
    plan: &[FleetPlanRow],
    halt_on_failure: bool,
    include_panel: bool,
    started_by: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let plan_json = serde_json::to_value(plan).unwrap_or(serde_json::json!([]));
    let initial_progress: Vec<FleetProgressRow> = plan
        .iter()
        .map(|p| FleetProgressRow {
            server_id: p.server_id,
            status: "pending".into(),
            duration_ms: None,
            error: None,
        })
        .collect();
    let progress_json = serde_json::to_value(&initial_progress).unwrap_or(serde_json::json!([]));

    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO fleet_update_runs \
            (target_version, plan, progress, halt_on_failure, include_panel, started_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(target_version)
    .bind(plan_json)
    .bind(progress_json)
    .bind(halt_on_failure)
    .bind(include_panel)
    .bind(started_by)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Walk the plan, POST `/panel/update` to each agent, poll status until
/// terminal, record per-server progress. Writes terminal `outcome` field
/// on completion. Long-running — spawn as a tokio task.
pub async fn execute_fleet_plan(
    pool: PgPool,
    agents: crate::services::agent::AgentRegistry,
    run_id: Uuid,
    plan: Vec<FleetPlanRow>,
    target_version: String,
    halt_on_failure: bool,
) {
    let mut progress: Vec<FleetProgressRow> = plan
        .iter()
        .map(|p| FleetProgressRow {
            server_id: p.server_id,
            status: "pending".into(),
            duration_ms: None,
            error: None,
        })
        .collect();

    let mut any_failed = false;
    let mut halted = false;

    for (idx, row) in plan.iter().enumerate() {
        if halted {
            progress[idx].status = "skipped".into();
            continue;
        }
        let started = std::time::Instant::now();
        progress[idx].status = "updating".into();
        let _ = persist_progress(&pool, run_id, &progress).await;

        let result = update_one_server(&agents, row.server_id, &target_version).await;
        let elapsed_ms = started.elapsed().as_millis() as i64;
        progress[idx].duration_ms = Some(elapsed_ms);

        match result {
            Ok(()) => {
                progress[idx].status = "succeeded".into();
            }
            Err(e) => {
                progress[idx].status = "failed".into();
                progress[idx].error = Some(e.to_string());
                any_failed = true;
                if halt_on_failure {
                    halted = true;
                }
            }
        }
        let _ = persist_progress(&pool, run_id, &progress).await;
    }

    let outcome = if !any_failed {
        "success"
    } else if halted {
        "halted"
    } else {
        "partial"
    };

    let _ = sqlx::query(
        "UPDATE fleet_update_runs SET finished_at = NOW(), outcome = $1 WHERE id = $2",
    )
    .bind(outcome)
    .bind(run_id)
    .execute(&pool)
    .await;

    tracing::info!("Fleet update run {run_id} completed with outcome {outcome}");
}

async fn persist_progress(
    pool: &PgPool,
    run_id: Uuid,
    progress: &[FleetProgressRow],
) -> Result<(), sqlx::Error> {
    let json = serde_json::to_value(progress).unwrap_or(serde_json::json!([]));
    sqlx::query("UPDATE fleet_update_runs SET progress = $1 WHERE id = $2")
        .bind(json)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// POST `/panel/update` to a remote agent, then poll `/panel/update/status`
/// up to ~10 minutes for terminal state. Returns Ok on succeeded, Err on
/// failed/rolled_back/timeout.
async fn update_one_server(
    agents: &crate::services::agent::AgentRegistry,
    server_id: Uuid,
    target_version: &str,
) -> Result<(), String> {
    let handle = agents
        .for_server(server_id)
        .await
        .map_err(|e| format!("agent handle: {e}"))?;

    let payload = Some(serde_json::json!({ "target_version": target_version }));
    handle
        .post("/panel/update", payload)
        .await
        .map_err(|e| format!("POST /panel/update: {e}"))?;

    // Poll status. Total budget ~10 min. The remote agent's update.sh has
    // its own ~3-5 min wall-clock when downloading binaries.
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let resp = match handle.get("/panel/update/status").await {
            Ok(v) => v,
            Err(_) => continue, // agent may be mid-restart; transient
        };
        let state = resp
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        match state.as_str() {
            "succeeded" => return Ok(()),
            "rolled_back" => {
                return Err(format!(
                    "agent rolled back: {}",
                    resp.get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unspecified")
                ));
            }
            "failed" => {
                return Err(format!(
                    "agent failed: {}",
                    resp.get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unspecified")
                ));
            }
            "in_flight" | "idle" => continue,
            _ => continue, // unknown state, keep polling until deadline
        }
    }
    Err("timed out waiting for remote agent to finalize update".into())
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_version_regex_accepts_canonical_shapes() {
        assert!(validate_target_version("v2.10.0"));
        assert!(validate_target_version("2.10.0"));
        assert!(validate_target_version("v2.10.0-rc.1"));
        assert!(validate_target_version("2.10.0-rc.12"));
        assert!(validate_target_version("v10.20.30"));
    }

    #[test]
    fn target_version_regex_rejects_garbage() {
        assert!(!validate_target_version(""));
        assert!(!validate_target_version("v"));
        assert!(!validate_target_version("v2"));
        assert!(!validate_target_version("v2.10"));
        assert!(!validate_target_version("v2.10.0.1"));
        assert!(!validate_target_version("v2.10.0-alpha"));
        assert!(!validate_target_version("v2.10.0-rc"));
        assert!(!validate_target_version("v2.10.0-rc."));
        assert!(!validate_target_version("v2.10.0-rc.a"));
        assert!(!validate_target_version("v2.10.0 "));
        assert!(!validate_target_version("2.10.0; rm -rf /"));
        assert!(!validate_target_version("latest"));
    }

    #[test]
    fn parse_target_from_trigger_matches_pre_update() {
        assert_eq!(
            parse_target_from_trigger("pre-update:v2.10.0"),
            Some("v2.10.0".into())
        );
        assert_eq!(
            parse_target_from_trigger("pre-update:v2.10.0-rc.1"),
            Some("v2.10.0-rc.1".into())
        );
        assert_eq!(parse_target_from_trigger("manual"), None);
        let uuid = Uuid::new_v4();
        assert_eq!(
            parse_target_from_trigger(&format!("fleet:{uuid}")),
            None
        );
    }

    #[tokio::test]
    async fn idle_state_handle_starts_idle() {
        let handle = new_state_handle();
        let s = handle.read().await;
        assert!(matches!(*s, UpdateState::Idle));
    }
}
