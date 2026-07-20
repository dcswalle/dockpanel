//! What version should a remote agent be running?
//!
//! ## What this used to be, and why it never worked (s233, measured)
//!
//! This endpoint answered with three fields — `version`, `download_url` and
//! `checksum` — read from three `settings` rows. **Nothing in the repo ever
//! wrote those rows.** They were accepted by the admin settings allowlist and
//! by nothing else: no seed, no migration, no installer, no release step, no UI
//! field, no doc. So `download_url` was null on every install that has ever
//! existed, and the agent's updater refused with "No download URL provided".
//!
//! It also required `AuthUser` — a signed user JWT — from a caller that
//! structurally cannot hold one: an agent has a random hex token from its
//! `servers` row. So it 401'd on every request since v2.10.0. The mount comment
//! above the route in `routes/mod.rs` claimed the opposite ("uses Bearer token
//! from servers table"), which is a large part of why this survived review.
//!
//! And the failure was invisible: the agent read `.json()` without checking the
//! status, an error body is valid JSON, the missing `version` field fell back to
//! the agent's own version, and the loop concluded it was up to date.
//!
//! ## What it is now
//!
//! One field, `target_version`, and no download plumbing at all.
//! `scripts/agent-self-update.sh` derives the architecture, the asset URL and
//! the expected digest **on the box** from the release's own `checksums.txt` —
//! so `agent_download_url` and `agent_checksum` were never merely unwritten,
//! they were unnecessary. A single scalar URL could not have been correct for a
//! mixed amd64/arm64 fleet anyway.
//!
//! `null` means "do nothing", and is the answer whenever the operator has not
//! switched agent auto-update on, or the update channel is on hold. The switch
//! is enforced HERE rather than on the agent, because there is no panel→agent
//! configuration push: an agent only ever learns things by asking.

use axum::{extract::State, http::HeaderMap, Json};

use crate::auth::{agent_rate_limit, authenticate_agent};
use crate::error::{internal_error, ApiError};
use crate::AppState;

/// GET /api/agent/version — the version the calling agent should be running.
///
/// Authenticated with the agent's own `servers`-table token.
pub async fn latest_version(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let server_id = authenticate_agent(&state, &headers).await?;
    agent_rate_limit(&state, server_id)?;

    Ok(Json(serde_json::json!({
        "target_version": resolve_agent_target(&state).await?,
    })))
}

/// `None` = the agent must not update itself.
///
/// The target is the release **this panel is running**, never "the newest tag on
/// GitHub": an agent should not get ahead of the panel that drives it, and the
/// release workflow builds the agent assets from the same tag, so the asset is
/// guaranteed to exist for a version the panel itself is on.
async fn resolve_agent_target(state: &AppState) -> Result<Option<String>, ApiError> {
    let setting = |key: &'static str| async move {
        sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&state.db)
            .await
            .map(|r| r.map(|v| v.0))
            .map_err(|e| internal_error("agent target", e))
    };

    // Opt-in, and off unless explicitly switched on. Seeded 'false'.
    if setting("agent_auto_update_enabled").await?.as_deref() != Some("true") {
        return Ok(None);
    }

    // `hold` is the operator saying "nothing moves". It has to win over the
    // auto-update switch, or the one control that stops a fleet mid-incident
    // would not actually stop it.
    if setting("update_channel").await?.as_deref() == Some("hold") {
        return Ok(None);
    }

    Ok(Some(format!("v{}", env!("CARGO_PKG_VERSION"))))
}

#[cfg(test)]
mod tests {
    /// The panel advertises its OWN crate version as the agent's target, so the
    /// two crates must ship the same version or the panel names a tag whose
    /// agent asset was never built. Nothing else in the tree enforces this —
    /// they were equal by convention only (s233).
    #[test]
    fn the_agent_and_api_crates_ship_the_same_version() {
        let ver = |toml: &str| {
            toml.lines()
                .find(|l| l.trim_start().starts_with("version"))
                .and_then(|l| l.split('"').nth(1))
                .map(str::to_string)
                .expect("no version in Cargo.toml")
        };
        let api = ver(include_str!("../../Cargo.toml"));
        let agent = ver(include_str!("../../../agent/Cargo.toml"));
        assert_eq!(
            api, agent,
            "dockpanel-api {api} would advertise v{api} as the agent target, \
             but dockpanel-agent is {agent} — that tag has no agent asset"
        );
    }

    /// The three retired keys must not come back: a reader with no writer
    /// anywhere is what made this endpoint answer null on every install that has
    /// ever existed.
    ///
    /// The key names are assembled rather than written out, because this test
    /// greps its own file and a literal would match itself — the source-pin
    /// prose trap, which caught this very test on its first run.
    #[test]
    fn the_dead_settings_keys_are_gone_from_the_reader_and_both_allowlists() {
        let here = include_str!("agent_updates.rs");
        let settings = include_str!("settings.rs");
        let keys = [
            concat!("agent_", "download_url"),
            concat!("agent_", "checksum"),
            concat!("agent_", "latest_version"),
        ];
        for key in keys {
            let quoted = format!("\"{key}\"");
            assert!(
                !settings.contains(&quoted),
                "{key} is still allowlisted in settings.rs — the update AND the \
                 import allowlist must both drop it"
            );
            // Prose may name them; code may not.
            for line in here.lines() {
                let l = line.trim_start();
                if l.starts_with("//") {
                    continue;
                }
                assert!(!l.contains(&quoted), "{key} is still read here: {l}");
            }
        }
    }
}
