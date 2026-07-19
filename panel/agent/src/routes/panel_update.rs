//! Phase 4 W4: agent-side panel-update receiver.
//!
//! When the central panel orchestrator pushes a fleet update to a remote
//! server, it POSTs `/panel/update` here. This handler hands the work to a
//! PID1-owned transient unit and returns 202 immediately; the orchestrator
//! then confirms the outcome from `/health`, not from anything this file
//! says about itself.
//!
//! Distinct from `routes::updates` (OS-package apt-get management); this
//! is panel self-update specifically.
//!
//! ## What this used to do, and why it was wrong (s232, measured on a lab)
//!
//! It ran `/opt/dockpanel/scripts/update.sh`. Two defects, and the second is
//! why the first could not just be patched by shipping the repo:
//!
//! 1. `install-agent.sh` never creates `/opt/dockpanel`. Every fleet update
//!    against a real agent-only box died in 166 ms with `500: update script
//!    not found`, and that installer is the only documented way to add a
//!    remote server — so the feature never worked on any box it targeted.
//! 2. `update.sh` is the *panel* updater: it wants a git repo, a postgres
//!    container, an API, a frontend, nginx. Planting the repo and re-running
//!    it produced `No such container: dockpanel-postgres` → `Database backup
//!    failed, aborting upgrade` — one second *after* the panel had recorded
//!    the server as **succeeded**, because `update.sh` re-execs itself into a
//!    transient unit with `exec systemd-run` (no `--wait`), so the child this
//!    file waits on exits 0 the moment PID1 *accepts* the job. Status was
//!    being derived from the wrong process (lesson #49), and fixing only (1)
//!    would have converted a loud safe failure into a silent false success on
//!    every box in a fleet (lesson #50).
//!
//! So: agent boxes get an agent-only update (`scripts/agent-self-update.sh`,
//! embedded below), full panel installs keep `update.sh`, and success is
//! never inferred from a child's exit status.

use axum::{routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::AppState;

/// The panel updater, present only on full DockPanel installs.
const UPDATE_SCRIPT: &str = "/opt/dockpanel/scripts/update.sh";
/// Present on a full install only; used to tell a panel box from an agent box.
const API_BIN: &str = "/usr/local/bin/dockpanel-api";

/// The agent-only updater. Compiled in rather than read from disk for the same
/// reason the restore procedure is (lessons #35/#38): a remote agent box has no
/// repo, and a script the running binary merely hopes is in sync is drift.
const AGENT_UPDATE_SCRIPT: &str = include_str!("../../../../scripts/agent-self-update.sh");
const AGENT_UPDATE_SCRIPT_PATH: &str = "/var/lib/dockpanel/agent-self-update.sh";
/// Where `agent-self-update.sh` records what actually happened. Read back after
/// the restart it performs, because the agent's in-memory state does not
/// survive its own update.
const AGENT_UPDATE_RESULT_PATH: &str = "/var/lib/dockpanel/last-agent-update.json";

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

/// Process-local state, and deliberately only ever a *negative* signal plus a
/// progress window. An update restarts this process, so this state cannot
/// outlive the thing it describes; the orchestrator confirms success from
/// `/health` (which is what the W4 design §4.5 said to do all along). Nothing
/// here may report `Succeeded` on its own authority — that is exactly the bug
/// this file shipped with.
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
    // A full panel install keeps the panel updater (it has an API, a frontend
    // and a database to move). Everything else — i.e. every box added with
    // install-agent.sh, which is the entire fleet population — gets the
    // agent-only updater, which needs nothing from /opt.
    let mode = if std::path::Path::new(UPDATE_SCRIPT).exists()
        && std::path::Path::new(API_BIN).exists()
    {
        UpdateMode::FullPanel
    } else {
        UpdateMode::AgentOnly
    };

    // Already-in-flight guard. It has to ask whether the in-flight run is
    // actually still running: an updater that fails inside its own transient
    // unit never restarts this process, so `InFlight` would otherwise stick
    // for the life of the agent and every later attempt — including the one
    // fixing the operator's typo — would be refused 409 for ever. Observed on
    // the s232 lab immediately after the previous fix landed.
    {
        let s = STATE.lock().unwrap();
        if let AgentUpdateState::InFlight { started_at, .. } = &*s {
            if !run_has_finished(*started_at) {
                return Err((
                    axum::http::StatusCode::CONFLICT,
                    "an update is already in flight".into(),
                ));
            }
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

    // Materialise the embedded updater before returning 202, so a box that
    // cannot even write it gets a real error instead of a silent no-op.
    if matches!(mode, UpdateMode::AgentOnly) {
        if let Err(e) = write_agent_update_script().await {
            let mut s = STATE.lock().unwrap();
            *s = AgentUpdateState::Idle;
            return Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not stage the agent updater: {e}"),
            ));
        }
    }

    let target_clone = target.clone();
    tokio::spawn(async move {
        run_update_subprocess(target_clone, mode).await;
    });

    Ok(Json(ApplyResponse {
        accepted: true,
        target_version: target,
    }))
}

#[derive(Debug, Clone, Copy)]
enum UpdateMode {
    /// Box has an API binary and the panel updater — update the whole panel.
    FullPanel,
    /// Agent-only box (install-agent.sh). Update just the agent binary.
    AgentOnly,
}

async fn write_agent_update_script() -> std::io::Result<()> {
    tokio::fs::create_dir_all("/var/lib/dockpanel").await?;
    tokio::fs::write(AGENT_UPDATE_SCRIPT_PATH, AGENT_UPDATE_SCRIPT).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            AGENT_UPDATE_SCRIPT_PATH,
            std::fs::Permissions::from_mode(0o700),
        )?;
    }
    Ok(())
}

async fn run_update_subprocess(target: String, mode: UpdateMode) {
    // Both updaters restart dockpanel-agent.service, and that unit is
    // KillMode=control-group — so anything still in this process's cgroup when
    // that happens is SIGTERMed mid-update. PID1 has to own the work
    // (lesson #47). `update.sh` self-detaches the same way; doing it here too
    // means the agent path does not depend on the remote box's copy of a script
    // being new enough to protect itself.
    let unit = format!(
        "dockpanel-agent-update-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let mut cmd = Command::new("systemd-run");
    cmd.arg("--quiet")
        .arg("--collect")
        .arg(format!("--unit={unit}"))
        .arg(format!("--setenv=DOCKPANEL_VERSION={target}"));

    match mode {
        UpdateMode::FullPanel => {
            cmd.arg("--setenv=INSTALL_FROM_RELEASE=1")
                .arg("--setenv=DOCKPANEL_NO_SELF_REFRESH=1")
                .arg("--setenv=DOCKPANEL_UPDATE_DETACHED=1")
                .arg("bash")
                .arg(UPDATE_SCRIPT);
        }
        UpdateMode::AgentOnly => {
            cmd.arg("--setenv=DOCKPANEL_AGENT_UPDATE_DETACHED=1")
                .arg("bash")
                .arg(AGENT_UPDATE_SCRIPT_PATH);
        }
    }

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

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

    // `systemd-run` without `--wait` returns as soon as PID1 has ACCEPTED the
    // job — measured at ~120 ms, while the real update runs for minutes and may
    // fail outright. So a zero exit here means "the work was handed off", never
    // "the work succeeded", and this function must not promote it to
    // `Succeeded`. It used to, and the panel therefore reported a fleet member
    // as updated one second before that member's updater aborted (lesson #49).
    //
    // A NON-zero exit is still real information: PID1 refused the job, so
    // nothing is running and the update definitively did not happen.
    match tokio::time::timeout(Duration::from_secs(900), child.wait()).await {
        Ok(Ok(status)) => {
            if !status.success() {
                let mut s = STATE.lock().unwrap();
                *s = AgentUpdateState::Failed {
                    reason: format!("could not start the update unit ({status})"),
                    at: chrono::Utc::now(),
                };
            }
            // Success case: stay InFlight. Either this process is replaced by
            // the restart (state resets to Idle and /health carries the truth),
            // or the updater's own result file explains the failure.
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
                reason: "updater did not return within 15min".into(),
                at: chrono::Utc::now(),
            };
        }
    }
}

/// The agent-only updater's verdict file. This is the record that survives the
/// restart the update performs, which the in-memory state by definition cannot.
/// `Ok(target)` on success, `Err((stage, reason))` on failure, `None` if no
/// update has ever run here.
fn last_agent_update_result() -> Option<Result<String, (String, String)>> {
    let v = read_agent_update_result()?;
    let field = |k: &str, d: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or(d)
            .to_string()
    };
    match v.get("ok").and_then(|o| o.as_bool()) {
        Some(true) => Some(Ok(field("target_version", "").trim_start_matches('v').to_string())),
        Some(false) => Some(Err((
            field("stage", "unknown"),
            field("reason", "agent update failed"),
        ))),
        None => None,
    }
}

fn read_agent_update_result() -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(AGENT_UPDATE_RESULT_PATH).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Has the run that began at `started_at` already reached a verdict?
///
/// This is the one predicate that decides both "may a new update start" and
/// "does the verdict on disk describe the run we are being asked about". The
/// in-memory state cannot answer either on its own: a successful update
/// replaces this process (so the state resets and says nothing), and a failure
/// inside the transient unit never touches it at all.
///
/// A verdict older than the run's start belongs to a previous run and is
/// ignored — the comparison is deliberately the conservative direction, since
/// the verdict's timestamp has whole-second granularity.
fn run_has_finished(started_at: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(v) = read_agent_update_result() else {
        return false;
    };
    let Some(at) = v.get("at").and_then(|a| a.as_str()) else {
        return false;
    };
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|t| t.with_timezone(&chrono::Utc) >= started_at)
        .unwrap_or(false)
}

/// GET /panel/update/status — a *negative* signal plus a progress window.
///
/// `idle` after the update restarts this process, which is why the caller must
/// treat the running version (always included below, and on `/health`) as the
/// authority on whether an update landed. What this endpoint can say reliably
/// is that something FAILED: either in-process, or — across the restart — from
/// the updater's own result file.
async fn get_panel_update_status() -> Json<serde_json::Value> {
    let s = STATE.lock().unwrap().clone();
    let mut value = serde_json::to_value(&s).unwrap_or(serde_json::json!({ "state": "idle" }));

    // Consult the updater's verdict whenever it describes THIS run, not only
    // once the process has restarted. A failure that stops before the restart
    // (a target release that does not exist, say) leaves this state `in_flight`
    // for ever, and the caller would then wait out its whole 10-minute deadline
    // and report a timeout — when the real, actionable reason had been on disk
    // one second in. Measured on the s232 lab.
    let state_str = value
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let verdict_applies = match state_str.as_str() {
        "idle" => true,
        "in_flight" => value
            .get("started_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|started| run_has_finished(started.with_timezone(&chrono::Utc)))
            .unwrap_or(false),
        _ => false,
    };

    if verdict_applies {
        match last_agent_update_result() {
            Some(Err((stage, reason))) => {
                value = serde_json::json!({
                    "state": "failed",
                    "reason": format!("{reason} (stage {stage})"),
                });
            }
            // The one place `Succeeded` may be constructed, and note WHERE it
            // comes from: the updater's on-disk verdict AND the version this
            // very process reports for itself. It is a statement about the
            // binary that is executing, not a prediction made by the process
            // that launched the update. A panel too old to check `/health`
            // still gets a truthful answer from this.
            Some(Ok(version)) if version == env!("CARGO_PKG_VERSION") => {
                value = serde_json::to_value(AgentUpdateState::Succeeded {
                    version,
                    completed_at: chrono::Utc::now(),
                })
                .unwrap_or(value);
            }
            _ => {}
        }
    }

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "running_version".into(),
            serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
    }
    Json(value)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/panel/update", post(apply_panel_update))
        .route("/panel/update/status", get(get_panel_update_status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_version_validator_matches_the_panel_side() {
        for good in ["v2.11.7", "2.11.7", "v2.11.7-rc.1", "10.20.30"] {
            assert!(validate_target_version(good), "{good} should be accepted");
        }
        for bad in ["", "v", "v2.11", "latest", "2.11.7; rm -rf /", "v2.11.7-alpha"] {
            assert!(!validate_target_version(bad), "{bad} should be rejected");
        }
    }

    /// The embedded updater must never `cp` onto the binary it is replacing:
    /// writing to an executing file fails ETXTBSY, and a restore path that
    /// cannot restore is not a safety net (lesson #48). Renames only.
    #[test]
    fn agent_updater_installs_by_rename_not_by_copy_onto_the_target() {
        for line in AGENT_UPDATE_SCRIPT.lines() {
            let l = line.trim();
            if l.starts_with('#') {
                continue;
            }
            if l.starts_with("cp ") || l.starts_with("cp -") {
                assert!(
                    l.contains("\"$BACKUP\""),
                    "the only copy allowed reads the live binary into the backup path: {l}"
                );
            }
        }
        assert!(AGENT_UPDATE_SCRIPT.contains("mv -f \"$STAGED\" \"$AGENT_BIN\""));
    }

    /// Download, then verify, then install — in that order and as three
    /// separate steps. Once bytes have been streamed into the consumer there is
    /// no verification window left (lesson #25).
    #[test]
    fn agent_updater_verifies_the_download_before_installing_it() {
        let s = AGENT_UPDATE_SCRIPT;
        assert!(s.contains("sha256sum"), "must checksum the download");
        assert!(s.contains("checksums.txt"), "must fetch the release checksums");
        let verify_at = s.find("sha256 mismatch").expect("mismatch guard missing");
        let install_at = s.find("could not move the new binary into place").unwrap();
        assert!(
            verify_at < install_at,
            "the checksum guard must come before the install"
        );
        assert!(
            !s.contains("curl -fsSL --max-time 600 \"${BASE}/${ASSET}\" |"),
            "the binary must be materialised to a file, never piped into a consumer"
        );
    }

    /// It restarts the unit it runs under, so PID1 has to own it (lesson #47),
    /// and a scope is not a substitute — a scope dies with the caller's session.
    #[test]
    fn agent_updater_runs_under_a_pid1_owned_transient_service() {
        assert!(AGENT_UPDATE_SCRIPT.contains("systemd-run"));
        assert!(
            !AGENT_UPDATE_SCRIPT.contains("--scope"),
            "a transient service, never a scope"
        );
        assert!(AGENT_UPDATE_SCRIPT.contains("--collect"));
    }

    /// The rollback branch must not claim it restored anything it did not, and
    /// must not swallow the failure of a restore — that combination is what made
    /// `update.sh`'s own rollback print "Rolled back to previous binaries" over
    /// a box that had not been rolled back at all (lesson #48/#58).
    #[test]
    fn the_rollback_branch_never_swallows_or_overclaims_the_restore() {
        let s = AGENT_UPDATE_SCRIPT;
        let rb = &s[s.find("stage=\"rollback\"").expect("no rollback branch")..];
        let rb = &rb[..rb.find("rm -f \"$BACKUP\"").unwrap_or(rb.len())];
        assert!(
            !rb.contains(concat!("mv -f \"$BACKUP\" \"$AGENT_BIN\" && ", "systemctl restart dockpanel-agent || true")),
            "a restore must not be suffixed with a status-swallowing || true"
        );
        assert!(
            rb.contains("if mv -f \"$BACKUP\" \"$AGENT_BIN\""),
            "the restore's own status has to be branched on"
        );
        assert!(
            rb.contains("COULD NOT RESTORE"),
            "a failed restore needs to say so, loudly, in the verdict"
        );
        assert!(
            rb.contains("running_version"),
            "and the claim must be checked against what the agent reports afterwards"
        );
    }

    /// A run that failed and a run that never happened must be distinguishable
    /// after the restart wipes this process's memory (lesson #52).
    #[test]
    fn agent_updater_always_writes_a_verdict_including_on_abort() {
        let s = AGENT_UPDATE_SCRIPT;
        assert!(s.contains("trap on_exit EXIT"), "needs an abort trap");
        assert!(
            s.contains("write_result false \"aborted at stage"),
            "the trap must record the abort"
        );
        assert!(s.contains("write_result true"), "and record success");
    }

    /// The regression this whole change exists for: nothing in the spawn path
    /// may promote a zero exit status into a success claim, because the process
    /// being waited on is the one that merely HANDS OFF the work.
    #[test]
    fn a_zero_exit_from_the_launcher_is_never_reported_as_success() {
        let src = include_str!("panel_update.rs");
        // Negative control: the exact shape that shipped, and that the s232 lab
        // caught reporting a fleet member updated 124ms after it was asked to be.
        // Assembled rather than written out, because this test greps its own
        // file and a literal here would match itself.
        let bad = concat!("if status.", "success() {");
        let code_hits = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains(bad))
            .count();
        assert_eq!(
            code_hits, 0,
            "a successful launch is not a successful update"
        );
        assert!(
            src.contains("if !status.success() {"),
            "a FAILED launch is still real information and must be recorded"
        );
    }

    /// A failure that stops before the restart leaves the in-memory state
    /// `in_flight` for ever. The caller would then wait out its full deadline
    /// and report a timeout, when the real reason had been on disk one second
    /// in — so the verdict must be consulted while in flight too, and only when
    /// it describes THIS run.
    #[test]
    fn an_in_flight_run_still_surfaces_a_verdict_written_after_it_started() {
        let src = include_str!("panel_update.rs");
        let f = &src[src.find("async fn get_panel_update_status").unwrap()..];
        let f = &f[..f.find("pub fn router").unwrap()];
        assert!(
            f.contains("\"in_flight\" =>"),
            "in_flight must be able to resolve from the verdict file"
        );
        assert!(
            f.contains("run_has_finished("),
            "and only for a verdict at least as new as this run's start"
        );
    }

    /// A failed run must not wedge the box: the in-flight guard and the status
    /// endpoint have to agree on when a run is over, or one failed update makes
    /// the box permanently un-updatable with a 409.
    #[test]
    fn the_in_flight_guard_and_the_status_endpoint_share_one_liveness_predicate() {
        let src = include_str!("panel_update.rs");
        let guard = &src[src.find("async fn apply_panel_update").unwrap()
            ..src.find("async fn run_update_subprocess").unwrap()];
        assert!(
            guard.contains("run_has_finished("),
            "the 409 guard must ask whether the run is actually still running"
        );
        // Negative control: the unconditional form that wedged the lab box.
        assert!(
            !guard.contains(concat!("if matches!(*s, AgentUpdateState::", "InFlight { .. }) {")),
            "an InFlight state alone is not evidence that an update is still running"
        );
    }

    /// A full panel install keeps the panel updater; everything else — the
    /// entire fleet population — gets the agent-only one.
    #[test]
    fn update_mode_is_decided_by_what_the_box_actually_has() {
        let src = include_str!("panel_update.rs");
        assert!(src.contains("UpdateMode::FullPanel"));
        assert!(src.contains("UpdateMode::AgentOnly"));
        assert!(
            src.contains("std::path::Path::new(API_BIN).exists()"),
            "an agent-only box has no API binary — that is the discriminator"
        );
    }
}
