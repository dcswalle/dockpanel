use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[allow(dead_code)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: String,
    pub email_verified: bool,
    #[serde(skip_serializing)]
    pub email_token: Option<String>,
    #[serde(skip_serializing)]
    pub reset_token: Option<String>,
    #[serde(skip_serializing)]
    pub reset_expires: Option<DateTime<Utc>>,
    #[serde(skip_serializing)]
    pub stripe_customer_id: Option<String>,
    #[serde(skip_serializing)]
    pub stripe_subscription_id: Option<String>,
    pub plan: String,
    pub plan_status: String,
    pub plan_server_limit: i32,
    #[serde(skip_serializing)]
    pub totp_secret: Option<String>,
    pub totp_enabled: bool,
    #[serde(skip_serializing)]
    pub recovery_codes: Option<String>,
    pub oauth_provider: Option<String>,
    #[serde(skip_serializing)]
    pub oauth_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Phase 4 W3: rotation = ordered list of user IDs × cadence_days.
/// "Who's on-call at time T" is computed via cadence math against
/// `anchor_at` — no calendar, no per-day overrides.
///
/// Currently route handlers serialize ad-hoc DTOs that augment this with
/// resolved-member info, so this struct sits unused until the next caller
/// wants the raw row shape.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OnCallSchedule {
    pub id: Uuid,
    pub name: String,
    pub members: Vec<Uuid>,
    pub cadence_days: i32,
    pub anchor_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Phase 4 W3: escalation policy = ordered JSONB array of `EscalationStep`s.
/// Referenced from `alert_rules.escalation_policy_id` (NULL = preserve
/// pre-W3 hardcoded 15/30-min behaviour).
///
/// Route handlers serialize ad-hoc DTOs that add `used_by_rule_count`, so
/// the raw row shape sits unused until another caller needs it.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EscalationPolicy {
    pub id: Uuid,
    pub name: String,
    pub steps: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One entry inside `escalation_policies.steps`. Stored as JSONB; decoded
/// when the alert engine evaluates the chain.
///
/// `route` is a discriminated string. Four valid shapes:
///   - `"on_call_schedule:<uuid>"` — resolve the schedule, page the current on-call user's channels.
///   - `"user:<uuid>"` — page a specific user's channels regardless of rotation.
///   - `"all_channels"` — fan out to every channel on the alert's owning `alert_rules` row.
///   - `"webhook:<url>"` — direct outbound webhook bypass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationStep {
    pub after_minutes: i32,
    pub route: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Site {
    pub id: Uuid,
    pub user_id: Uuid,
    pub server_id: Option<Uuid>,
    pub domain: String,
    pub runtime: String,
    pub status: String,
    pub proxy_port: Option<i32>,
    pub php_version: Option<String>,
    pub root_path: Option<String>,
    pub ssl_enabled: bool,
    pub ssl_cert_path: Option<String>,
    pub ssl_key_path: Option<String>,
    pub ssl_expiry: Option<DateTime<Utc>>,
    pub ssl_profile: Option<String>,
    pub ssl_renewal_at: Option<DateTime<Utc>>,
    pub ssl_renewal_checked_at: Option<DateTime<Utc>>,
    pub rate_limit: Option<i32>,
    pub max_upload_mb: i32,
    pub php_memory_mb: i32,
    pub php_max_workers: i32,
    pub custom_nginx: Option<String>,
    pub php_preset: Option<String>,
    pub app_command: Option<String>,
    pub parent_site_id: Option<Uuid>,
    pub synced_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub fastcgi_cache: bool,
    pub redis_cache: bool,
    pub redis_db: i32,
    pub waf_enabled: bool,
    pub waf_mode: String,
    pub csp_policy: Option<String>,
    pub permissions_policy: Option<String>,
    pub bot_protection: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
