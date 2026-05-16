//! Phase 4 W4: agent-side panel-update receiver.
//!
//! When the central panel orchestrator pushes an update to a remote
//! server, it POSTs `/panel/update` here. This handler kicks off
//! `update.sh` locally (background, detached process group) and returns
//! 202 immediately. The orchestrator polls `/panel/update/status` for
//! terminal state.
//!
//! Distinct from `routes::updates` (OS-package apt-get management); this
//! is panel self-update specifically.

use axum::{routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::AppState;

const UPDATE_SCRIPT: &str = "/opt/dockpanel/scripts/update.sh";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentUpdateState {
    Idle,
    InFlight {
        target_version: String,
        started_at: chrono::DateTime<chrono::Utc>,
        last_log_line: Option<String>,
    },
    Succeeded {
        version: String,
        completed_at: chrono::DateTime<chrono::Utc>,
    },
    Failed {
        reason: String,
        at: chrono::DateTime<chrono::Utc>,
    },
}

/// Process-local state. The agent doesn't survive `update.sh`'s
/// `systemctl restart dockpanel-agent` either, so this state is wiped on
/// the next process boot. The orchestrator detects "succeeded" by polling
/// `/health` for the new version after the agent restarts. We expose this
/// state as a best-effort window during the apply itself.
static STATE: Mutex<AgentUpdateState> = Mutex::new(AgentUpdateState::Idle);

#[derive(Debug, Deserialize)]
struct UpdateRequest {
    target_version: String,
}

#[derive(Debug, Serialize)]
struct ApplyResponse {
    accepted: bool,
    target_version: String,
}

/// Hand-rolled validator (matches backend's `validate_target_version`).
fn validate_target_version(v: &str) -> bool {
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

/// POST /panel/update — kick off update.sh with the target version.
/// Returns 202 with the accepted target. Caller polls /panel/update/status.
async fn apply_panel_update(
    Json(body): Json<UpdateRequest>,
) -> Result<Json<ApplyResponse>, (axum::http::StatusCode, String)> {
    let target = body.target_version.trim().to_string();
    if !validate_target_version(&target) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!("invalid target_version: {target}"),
        ));
    }
    if !std::path::Path::new(UPDATE_SCRIPT).exists() {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("update script not found at {UPDATE_SCRIPT}"),
        ));
    }

    // Already-in-flight guard.
    {
        let s = STATE.lock().unwrap();
        if matches!(*s, AgentUpdateState::InFlight { .. }) {
            return Err((
                axum::http::StatusCode::CONFLICT,
                "an update is already in flight".into(),
            ));
        }
    }
    {
        let mut s = STATE.lock().unwrap();
        *s = AgentUpdateState::InFlight {
            target_version: target.clone(),
            started_at: chrono::Utc::now(),
            last_log_line: None,
        };
    }

    let target_clone = target.clone();
    tokio::spawn(async move {
        run_update_subprocess(target_clone).await;
    });

    Ok(Json(ApplyResponse {
        accepted: true,
        target_version: target,
    }))
}

async fn run_update_subprocess(target: String) {
    let mut cmd = Command::new("bash");
    cmd.arg(UPDATE_SCRIPT)
        .env("INSTALL_FROM_RELEASE", "1")
        .env("DOCKPANEL_NO_SELF_REFRESH", "1")
        .env("DOCKPANEL_VERSION", &target)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut s = STATE.lock().unwrap();
            *s = AgentUpdateState::Failed {
                reason: format!("spawn failed: {e}"),
                at: chrono::Utc::now(),
            };
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Some(s) = stdout {
        tokio::spawn(async move {
            let mut reader = BufReader::new(s).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.is_empty() {
                    continue;
                }
                tracing::info!(target: "panel_update", "{line}");
                let mut st = STATE.lock().unwrap();
                if let AgentUpdateState::InFlight { last_log_line, .. } = &mut *st {
                    *last_log_line = Some(line.chars().take(256).collect());
                }
            }
        });
    }
    if let Some(s) = stderr {
        tokio::spawn(async move {
            let mut reader = BufReader::new(s).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.is_empty() {
                    continue;
                }
                tracing::warn!(target: "panel_update", "{line}");
            }
        });
    }

    // We may not survive long enough to record terminal state — update.sh
    // will systemctl restart dockpanel-agent partway through and our
    // process dies. The orchestrator detects success by reading the
    // agent's restarted /health endpoint.
    match tokio::time::timeout(Duration::from_secs(900), child.wait()).await {
        Ok(Ok(status)) => {
            let mut s = STATE.lock().unwrap();
            if status.success() {
                *s = AgentUpdateState::Succeeded {
                    version: target.clone(),
                    completed_at: chrono::Utc::now(),
                };
            } else {
                *s = AgentUpdateState::Failed {
                    reason: format!("update.sh exit status {status}"),
                    at: chrono::Utc::now(),
                };
            }
        }
        Ok(Err(e)) => {
            let mut s = STATE.lock().unwrap();
            *s = AgentUpdateState::Failed {
                reason: format!("wait error: {e}"),
                at: chrono::Utc::now(),
            };
        }
        Err(_) => {
            let mut s = STATE.lock().unwrap();
            *s = AgentUpdateState::Failed {
                reason: "update.sh timed out after 15min".into(),
                at: chrono::Utc::now(),
            };
        }
    }
}

/// GET /panel/update/status — return the current state. Idle once the
/// process has restarted; orchestrator should also check /health version.
async fn get_panel_update_status() -> Json<serde_json::Value> {
    let s = STATE.lock().unwrap().clone();
    let value = serde_json::to_value(&s).unwrap_or(serde_json::json!({ "state": "idle" }));
    Json(value)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/panel/update", post(apply_panel_update))
        .route("/panel/update/status", get(get_panel_update_status))
}
