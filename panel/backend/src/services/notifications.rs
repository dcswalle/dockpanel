use sqlx::PgPool;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Shared HTTP client for webhook notifications (reuses connections).
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    })
}

// ── Real-time notification broadcast (SSE) ─────────────────────────────────

/// Global broadcast sender for real-time notification delivery.
/// Initialized once from main.rs at startup via `init_notif_broadcast`.
static NOTIF_TX: OnceLock<broadcast::Sender<(Uuid, String)>> = OnceLock::new();

/// Register the broadcast sender (called once from main.rs).
pub fn init_notif_broadcast(tx: broadcast::Sender<(Uuid, String)>) {
    NOTIF_TX.set(tx).ok();
}

/// Notification channels for delivering alerts.
pub struct NotifyChannels {
    pub email: Option<String>,
    pub slack_url: Option<String>,
    pub discord_url: Option<String>,
    pub pagerduty_key: Option<String>,
    pub webhook_url: Option<String>,
    /// Comma-separated alert types to suppress from external channels (Gap #69)
    pub muted_types: String,
}

/// Gap #70: Load a custom notification template from settings, or use default formatting.
async fn format_message(pool: &PgPool, channel: &str, subject: &str, message: &str, severity: &str) -> String {
    let key = format!("notif_template_{channel}");
    let template: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM settings WHERE key = $1"
    ).bind(&key).fetch_optional(pool).await.ok().flatten();

    if let Some((tmpl,)) = template {
        if !tmpl.is_empty() {
            return tmpl.replace("{{title}}", subject)
                .replace("{{message}}", message)
                .replace("{{severity}}", severity)
                .replace("{{timestamp}}", &chrono::Utc::now().to_rfc3339());
        }
    }

    // Default format per channel
    match channel {
        "slack" => format!("*{subject}*\n{message}"),
        "discord" => format!("**{subject}**\n{message}"),
        _ => format!("{subject}\n\n{message}"),
    }
}

/// Derive severity string from subject line (for webhook/pagerduty payloads).
fn derive_severity(subject: &str) -> &'static str {
    if subject.contains("FAIL") || subject.contains("down") || subject.contains("critical") {
        "critical"
    } else if subject.contains("warning") {
        "warning"
    } else if subject.contains("Resolved") || subject.contains("back up") {
        "info"
    } else {
        "error"
    }
}

/// Send a notification via all configured channels.
pub async fn send_notification(
    pool: &PgPool,
    channels: &NotifyChannels,
    subject: &str,
    message: &str,
    body_html: &str,
) {
    let client = http_client();
    let severity = derive_severity(subject);

    // Email — supports custom template via notif_template_email
    if let Some(ref email) = channels.email {
        let email_template: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM settings WHERE key = 'notif_template_email'"
        ).fetch_optional(pool).await.ok().flatten();

        let html = if let Some((tmpl,)) = email_template {
            if !tmpl.is_empty() {
                tmpl.replace("{{title}}", subject)
                    .replace("{{message}}", message)
                    .replace("{{severity}}", severity)
                    .replace("{{timestamp}}", &chrono::Utc::now().to_rfc3339())
            } else {
                body_html.to_string()
            }
        } else {
            body_html.to_string()
        };

        if let Err(e) = crate::services::email::send_email(pool, email, subject, &html).await {
            tracing::warn!("Alert email failed: {e}");
        }
    }

    // Slack webhook — supports custom template via notif_template_slack
    if let Some(ref url) = channels.slack_url {
        if !url.is_empty() {
            let text = format_message(pool, "slack", subject, message, severity).await;
            let _ = client
                .post(url)
                .json(&serde_json::json!({ "text": text }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }

    // Discord webhook — supports custom template via notif_template_discord
    if let Some(ref url) = channels.discord_url {
        if !url.is_empty() {
            let content = format_message(pool, "discord", subject, message, severity).await;
            let _ = client
                .post(url)
                .json(&serde_json::json!({ "content": content }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }

    // PagerDuty Events API v2
    if let Some(ref key) = channels.pagerduty_key {
        if !key.is_empty() {
            let event_action = if subject.contains("Resolved") || subject.contains("back up") {
                "resolve"
            } else {
                "trigger"
            };
            let _ = client
                .post("https://events.pagerduty.com/v2/enqueue")
                .json(&serde_json::json!({
                    "routing_key": key,
                    "event_action": event_action,
                    "payload": {
                        "summary": subject,
                        "source": "DockPanel",
                        "severity": severity,
                        "custom_details": { "message": message },
                    },
                }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }

    // Generic webhook (GAP 31) — supports custom template via notif_template_webhook
    if let Some(ref url) = channels.webhook_url {
        if !url.is_empty() {
            let custom_message = format_message(pool, "webhook", subject, message, severity).await;
            let _ = client
                .post(url)
                .json(&serde_json::json!({
                    "title": subject,
                    "message": custom_message,
                    "severity": severity,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "source": "dockpanel"
                }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }
}

/// Resolve panel base URL from settings → env → fallback.
/// Used to build "Open runbook" links in notification payloads.
async fn panel_base_url(pool: &PgPool) -> String {
    if let Ok(Some((url,))) = sqlx::query_as::<_, (String,)>(
        "SELECT value FROM settings WHERE key = 'base_url'",
    )
    .fetch_optional(pool)
    .await
    {
        if !url.is_empty() {
            return url.trim_end_matches('/').to_string();
        }
    }
    std::env::var("BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

/// Phase 4 W3: build the runbook excerpt + URL pair for a given alert type.
///
/// Returns `(None, None)` when no runbook exists (DB row or const default).
/// Returns `(Some excerpt, Some url)` when the runbook is loadable AND a
/// `base_url` is configured; `(Some excerpt, None)` when no `base_url` is set
/// (excerpt still useful for chat/webhook channels, URL omitted).
///
/// Used by both `try_fire_alert` (initial page) and
/// `services::alert_engine::check_escalations` (re-pages on escalation)
/// so escalation notifications carry the same runbook payload as the
/// original fire (W2 consistency repair).
pub async fn load_runbook_payload(
    pool: &PgPool,
    alert_type: &str,
) -> (Option<String>, Option<String>) {
    let runbook = crate::services::alert_runbooks::get_runbook(pool, alert_type).await;
    let excerpt = runbook
        .as_ref()
        .map(|r| crate::services::alert_runbooks::excerpt(&r.runbook_md, 280));
    let url = if runbook.is_some() {
        let base = panel_base_url(pool).await;
        if base.is_empty() {
            None
        } else {
            Some(format!("{base}/alerts/runbooks/{alert_type}"))
        }
    } else {
        None
    };
    (excerpt, url)
}

/// Phase 4 W2: Send a notification with runbook attachment.
/// Used by `try_fire_alert` and `check_escalations` (W3) — non-alert
/// callers stay on `send_notification`.
///
/// Per-channel handling:
/// - email: full markdown rendered via pulldown-cmark, appended to body_html
/// - slack/discord: link + 280-char excerpt appended to message
/// - pagerduty: runbook_url + runbook_excerpt added to custom_details
/// - webhook: runbook_url + runbook_excerpt as top-level keys
pub async fn send_notification_with_runbook(
    pool: &PgPool,
    channels: &NotifyChannels,
    subject: &str,
    message: &str,
    body_html: &str,
    runbook_excerpt: Option<&str>,
    runbook_url: Option<&str>,
) {
    let client = http_client();
    let severity = derive_severity(subject);

    // Email — appends rendered runbook HTML to body_html.
    if let Some(ref email) = channels.email {
        let email_template: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM settings WHERE key = 'notif_template_email'",
        )
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

        let runbook_html = runbook_excerpt
            .map(|md| render_runbook_html(md))
            .unwrap_or_default();
        let runbook_url_str = runbook_url.unwrap_or("");

        let html = if let Some((tmpl,)) = email_template {
            if !tmpl.is_empty() {
                tmpl.replace("{{title}}", subject)
                    .replace("{{message}}", message)
                    .replace("{{severity}}", severity)
                    .replace("{{timestamp}}", &chrono::Utc::now().to_rfc3339())
                    .replace("{{runbook_excerpt}}", &runbook_html)
                    .replace("{{runbook_url}}", runbook_url_str)
            } else {
                append_runbook_to_html(body_html, &runbook_html, runbook_url_str)
            }
        } else {
            append_runbook_to_html(body_html, &runbook_html, runbook_url_str)
        };

        if let Err(e) = crate::services::email::send_email(pool, email, subject, &html).await {
            tracing::warn!("Alert email failed: {e}");
        }
    }

    // Slack — append `*Runbook:* <url|view>\n_excerpt_` to message.
    if let Some(ref url) = channels.slack_url {
        if !url.is_empty() {
            let mut text = format_message(pool, "slack", subject, message, severity).await;
            if let Some(excerpt) = runbook_excerpt {
                if let Some(rurl) = runbook_url {
                    text.push_str(&format!("\n\n*Runbook:* <{rurl}|view>\n_{excerpt}_"));
                } else {
                    text.push_str(&format!("\n\n*Runbook:* _{excerpt}_"));
                }
            }
            let _ = client
                .post(url)
                .json(&serde_json::json!({ "text": text }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }

    // Discord — append **Runbook:** [view](url)\n*excerpt*
    if let Some(ref url) = channels.discord_url {
        if !url.is_empty() {
            let mut content = format_message(pool, "discord", subject, message, severity).await;
            if let Some(excerpt) = runbook_excerpt {
                if let Some(rurl) = runbook_url {
                    content.push_str(&format!("\n\n**Runbook:** [view]({rurl})\n*{excerpt}*"));
                } else {
                    content.push_str(&format!("\n\n**Runbook:** *{excerpt}*"));
                }
            }
            let _ = client
                .post(url)
                .json(&serde_json::json!({ "content": content }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }

    // PagerDuty — extend custom_details with runbook fields.
    if let Some(ref key) = channels.pagerduty_key {
        if !key.is_empty() {
            let event_action = if subject.contains("Resolved") || subject.contains("back up") {
                "resolve"
            } else {
                "trigger"
            };
            let mut custom_details = serde_json::json!({ "message": message });
            if let Some(excerpt) = runbook_excerpt {
                custom_details["runbook_excerpt"] = serde_json::json!(excerpt);
            }
            if let Some(rurl) = runbook_url {
                custom_details["runbook_url"] = serde_json::json!(rurl);
            }
            let _ = client
                .post("https://events.pagerduty.com/v2/enqueue")
                .json(&serde_json::json!({
                    "routing_key": key,
                    "event_action": event_action,
                    "payload": {
                        "summary": subject,
                        "source": "DockPanel",
                        "severity": severity,
                        "custom_details": custom_details,
                    },
                }))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }

    // Generic webhook — top-level runbook keys.
    if let Some(ref url) = channels.webhook_url {
        if !url.is_empty() {
            let custom_message = format_message(pool, "webhook", subject, message, severity).await;
            let mut payload = serde_json::json!({
                "title": subject,
                "message": custom_message,
                "severity": severity,
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "source": "dockpanel",
            });
            if let Some(excerpt) = runbook_excerpt {
                payload["runbook_excerpt"] = serde_json::json!(excerpt);
            }
            if let Some(rurl) = runbook_url {
                payload["runbook_url"] = serde_json::json!(rurl);
            }
            let _ = client
                .post(url)
                .json(&payload)
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        }
    }
}

/// Render markdown to safe HTML using pulldown-cmark. Wrapped in catch_unwind
/// defensively — admin-authored input is trusted but the parser is third-party.
fn render_runbook_html(md: &str) -> String {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let result = catch_unwind(AssertUnwindSafe(|| {
        let parser = pulldown_cmark::Parser::new(md);
        let mut html = String::with_capacity(md.len() * 2);
        pulldown_cmark::html::push_html(&mut html, parser);
        html
    }));
    match result {
        Ok(html) => html,
        Err(_) => {
            tracing::warn!("pulldown-cmark panicked rendering runbook; falling back to raw text");
            html_escape(md)
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn append_runbook_to_html(body_html: &str, runbook_html: &str, runbook_url: &str) -> String {
    if runbook_html.is_empty() {
        return body_html.to_string();
    }
    let link = if runbook_url.is_empty() {
        String::new()
    } else {
        format!(
            "<p style=\"margin:16px 0 8px\"><a href=\"{runbook_url}\" \
             style=\"color:#3b82f6;text-decoration:none;font-weight:600\">Open runbook in panel →</a></p>"
        )
    };
    format!(
        "{body_html}\
         <hr style=\"margin:24px 0;border:none;border-top:1px solid #e5e7eb\"/>\
         <h3 style=\"font-family:sans-serif;color:#111827;margin:0 0 12px\">Runbook</h3>\
         <div style=\"font-family:sans-serif;color:#374151;line-height:1.5\">{runbook_html}</div>\
         {link}"
    )
}

/// Get notification channels for a user from their alert_rules.
/// Checks server-specific rules first, falls back to global (server_id IS NULL).
pub async fn get_user_channels(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
) -> Option<NotifyChannels> {
    // Try server-specific rules first, then global
    let rule: Option<(bool, Option<String>, Option<String>, Option<String>, Option<String>, String)> = if let Some(sid) = server_id {
        let specific: Option<(bool, Option<String>, Option<String>, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT notify_email, notify_slack_url, notify_discord_url, notify_pagerduty_key, notify_webhook_url, muted_types \
             FROM alert_rules WHERE user_id = $1 AND server_id = $2",
        )
        .bind(user_id)
        .bind(sid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

        if specific.is_some() {
            specific
        } else {
            sqlx::query_as(
                "SELECT notify_email, notify_slack_url, notify_discord_url, notify_pagerduty_key, notify_webhook_url, muted_types \
                 FROM alert_rules WHERE user_id = $1 AND server_id IS NULL",
            )
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        }
    } else {
        sqlx::query_as(
            "SELECT notify_email, notify_slack_url, notify_discord_url, notify_pagerduty_key, notify_webhook_url, muted_types \
             FROM alert_rules WHERE user_id = $1 AND server_id IS NULL",
        )
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
    };

    let (notify_email, slack_url, discord_url, pagerduty_key, webhook_url, muted_types) = rule?;

    // Look up user email if email notifications are enabled
    let email = if notify_email {
        sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    Some(NotifyChannels {
        email,
        slack_url,
        discord_url,
        pagerduty_key,
        webhook_url,
        muted_types,
    })
}

/// Phase 4 W3: look up the escalation_policy_id for a user/server pair.
/// Mirrors `get_user_channels` row-resolution: server-specific row wins,
/// global (server_id IS NULL) row is the fallback, no row at all → None.
pub async fn get_user_policy_id(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
) -> Option<Uuid> {
    if let Some(sid) = server_id {
        let specific: Option<(Option<Uuid>,)> = sqlx::query_as(
            "SELECT escalation_policy_id FROM alert_rules \
             WHERE user_id = $1 AND server_id = $2",
        )
        .bind(user_id)
        .bind(sid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        if let Some((Some(pid),)) = specific {
            return Some(pid);
        }
        // Specific row existed but policy NULL → preserve "no policy"; only
        // fall back to global when the specific row is absent entirely.
        if specific.is_some() {
            return None;
        }
    }
    let global: Option<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT escalation_policy_id FROM alert_rules \
         WHERE user_id = $1 AND server_id IS NULL",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    global.and_then(|(p,)| p)
}

/// Phase 4 W3: load + decode an `escalation_policies` row's `steps` array.
/// Returns the parsed `Vec<EscalationStep>` or empty vec on any failure.
pub async fn load_escalation_steps(
    pool: &PgPool,
    policy_id: Uuid,
) -> Vec<crate::models::EscalationStep> {
    let row: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT steps FROM escalation_policies WHERE id = $1",
    )
    .bind(policy_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((steps_json,)) = row else { return Vec::new(); };
    serde_json::from_value(steps_json).unwrap_or_else(|e| {
        tracing::warn!("Failed to decode escalation_policy {policy_id} steps: {e}");
        Vec::new()
    })
}

/// Phase 4 W3: dispatch a single escalation step's payload.
///
/// Resolves the step's `route` against on-call schedules and user IDs,
/// then sends `send_notification_with_runbook` for each resolved
/// channel-set. `alert_owner_id` is the user_id on the alerts row —
/// used as the fallback "channels of record" for `all_channels` routes
/// and synthetic-webhook routes.
pub async fn dispatch_escalation_step(
    pool: &PgPool,
    alert_owner_id: Uuid,
    alert_owner_server_id: Option<Uuid>,
    alert_type: &str,
    step: &crate::models::EscalationStep,
    subject: &str,
    message: &str,
    body_html: &str,
    runbook_excerpt: Option<&str>,
    runbook_url: Option<&str>,
) {
    let route = &step.route;
    if let Some(url) = route.strip_prefix("webhook:") {
        // Direct webhook bypass — synthesize a NotifyChannels with only
        // the webhook_url populated.
        let synthetic = NotifyChannels {
            email: None,
            slack_url: None,
            discord_url: None,
            pagerduty_key: None,
            webhook_url: Some(url.to_string()),
            muted_types: String::new(),
        };
        send_notification_with_runbook(
            pool,
            &synthetic,
            subject,
            message,
            body_html,
            runbook_excerpt,
            runbook_url,
        )
        .await;
        return;
    }

    if route == "all_channels" {
        // Fan out to the alert's owner — preserves pre-W3 default behaviour.
        fanout_to_user(
            pool,
            alert_owner_id,
            alert_owner_server_id,
            alert_type,
            subject,
            message,
            body_html,
            runbook_excerpt,
            runbook_url,
        )
        .await;
        return;
    }

    // on_call_schedule:<uuid> or user:<uuid> → routes resolve to user IDs.
    let users = crate::services::on_call::route_to_user_ids(pool, route).await;
    if users.is_empty() {
        tracing::debug!(
            "dispatch_escalation_step: route {route} resolved to no users (schedule empty? unknown shape?) — skipping page"
        );
        return;
    }
    for uid in users {
        fanout_to_user(
            pool,
            uid,
            alert_owner_server_id,
            alert_type,
            subject,
            message,
            body_html,
            runbook_excerpt,
            runbook_url,
        )
        .await;
    }
}

/// Phase 4 W3: send to one user's channels with that user's own mute
/// preference applied. Used by `dispatch_escalation_step` to honour the
/// routed user's per-type mute even when escalation routes them in.
async fn fanout_to_user(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
    alert_type: &str,
    subject: &str,
    message: &str,
    body_html: &str,
    runbook_excerpt: Option<&str>,
    runbook_url: Option<&str>,
) {
    let Some(channels) = get_user_channels(pool, user_id, server_id).await else {
        return;
    };
    let is_muted = if !channels.muted_types.is_empty() {
        channels
            .muted_types
            .split(',')
            .map(|s| s.trim())
            .any(|t| t == alert_type)
    } else {
        false
    };
    if is_muted {
        tracing::debug!(
            "Alert type '{alert_type}' muted for routed user {user_id} — skipping external channels"
        );
        return;
    }
    send_notification_with_runbook(
        pool,
        &channels,
        subject,
        message,
        body_html,
        runbook_excerpt,
        runbook_url,
    )
    .await;
}

/// Check if an alert type is enabled for a user.
pub async fn is_alert_enabled(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
    alert_type: &str,
) -> bool {
    let column = match alert_type {
        "cpu" => "alert_cpu",
        "memory" => "alert_memory",
        "disk" => "alert_disk",
        "offline" => "alert_offline",
        "backup_failure" => "alert_backup_failure",
        "ssl_expiry" => "alert_ssl_expiry",
        "service_down" => "alert_service_health",
        "gpu_utilization" | "gpu_temperature" | "gpu_vram" => "alert_gpu",
        _ => return true,
    };

    // Try server-specific, then global
    let query = format!(
        "SELECT {column} FROM alert_rules WHERE user_id = $1 AND server_id {}",
        if server_id.is_some() {
            "= $2"
        } else {
            "IS NULL"
        }
    );

    let result: Option<(bool,)> = if let Some(sid) = server_id {
        // Server-specific first
        let specific: Option<(bool,)> = sqlx::query_as(&query)
            .bind(user_id)
            .bind(sid)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

        if specific.is_some() {
            specific
        } else {
            let global_query = format!(
                "SELECT {column} FROM alert_rules WHERE user_id = $1 AND server_id IS NULL"
            );
            sqlx::query_as(&global_query)
                .bind(user_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
        }
    } else {
        sqlx::query_as(&query)
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
    };

    // Default to true if no rules exist (alerts enabled by default)
    result.map(|r| r.0).unwrap_or(true)
}

/// Get threshold settings for a user/server.
pub async fn get_thresholds(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
) -> (i32, i32, i32, i32, i32, i32, String) {
    // (cpu_threshold, cpu_duration, mem_threshold, mem_duration, disk_threshold, cooldown, ssl_days)
    let row: Option<(i32, i32, i32, i32, i32, i32, String)> = if let Some(sid) = server_id {
        let specific: Option<(i32, i32, i32, i32, i32, i32, String)> = sqlx::query_as(
            "SELECT cpu_threshold, cpu_duration, memory_threshold, memory_duration, \
             disk_threshold, cooldown_minutes, ssl_warning_days \
             FROM alert_rules WHERE user_id = $1 AND server_id = $2",
        )
        .bind(user_id)
        .bind(sid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

        if specific.is_some() {
            specific
        } else {
            sqlx::query_as(
                "SELECT cpu_threshold, cpu_duration, memory_threshold, memory_duration, \
                 disk_threshold, cooldown_minutes, ssl_warning_days \
                 FROM alert_rules WHERE user_id = $1 AND server_id IS NULL",
            )
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        }
    } else {
        None
    };

    row.unwrap_or((90, 5, 90, 5, 85, 60, "30,14,7,3,1".to_string()))
}

/// Get GPU-specific threshold settings for a user/server.
/// Returns (gpu_util_threshold, gpu_util_duration, gpu_temp_threshold, gpu_vram_threshold, cooldown).
pub async fn get_gpu_thresholds(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
) -> (i32, i32, i32, i32, i32) {
    let row: Option<(i32, i32, i32, i32, i32)> = if let Some(sid) = server_id {
        let specific: Option<(i32, i32, i32, i32, i32)> = sqlx::query_as(
            "SELECT gpu_util_threshold, gpu_util_duration, gpu_temp_threshold, \
             gpu_vram_threshold, cooldown_minutes \
             FROM alert_rules WHERE user_id = $1 AND server_id = $2",
        )
        .bind(user_id)
        .bind(sid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

        if specific.is_some() {
            specific
        } else {
            sqlx::query_as(
                "SELECT gpu_util_threshold, gpu_util_duration, gpu_temp_threshold, \
                 gpu_vram_threshold, cooldown_minutes \
                 FROM alert_rules WHERE user_id = $1 AND server_id IS NULL",
            )
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        }
    } else {
        None
    };

    row.unwrap_or((95, 5, 85, 95, 60))
}

/// Fire an alert: check cooldown, record in alerts table, send notification.
/// Convenience wrapper that ignores errors (for callers that don't need retry).
pub async fn fire_alert(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
    site_id: Option<Uuid>,
    alert_type: &str,
    severity: &str,
    title: &str,
    message: &str,
) {
    let _ = try_fire_alert(pool, user_id, server_id, site_id, alert_type, severity, title, message).await;
}

/// Fire an alert with Result return for retry support.
pub async fn try_fire_alert(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
    site_id: Option<Uuid>,
    alert_type: &str,
    severity: &str,
    title: &str,
    message: &str,
) -> Result<(), String> {
    // Check if this alert type is enabled
    if !is_alert_enabled(pool, user_id, server_id, alert_type).await {
        return Ok(());
    }

    // Record in alerts table
    sqlx::query(
        "INSERT INTO alerts (user_id, server_id, site_id, alert_type, severity, title, message) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(user_id)
    .bind(server_id)
    .bind(site_id)
    .bind(alert_type)
    .bind(severity)
    .bind(title)
    .bind(message)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to record alert: {e}"))?;

    // Also store in panel notification center (bell icon) — notify all admins
    notify_panel(pool, None, title, message, severity, "alert", None).await;

    // Build the notification payload once — both the NULL-policy fan-out and the
    // policy-driven fan-out reuse it.
    let subject = format!("DockPanel Alert: {title}");
    let html = format!(
        "<div style=\"font-family:sans-serif;max-width:600px;margin:0 auto\">\
         <h2 style=\"color:{}\">{title}</h2>\
         <p>{message}</p>\
         <p style=\"color:#6b7280;font-size:14px\">Time: {}</p>\
         </div>",
        match severity {
            "critical" => "#ef4444",
            "warning" => "#f59e0b",
            _ => "#3b82f6",
        },
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
    );
    // Phase 4 W2 + W3: attach runbook to the notification payload.
    // Same helper is reused by check_escalations so escalation re-pages
    // carry the runbook excerpt + URL too.
    let (runbook_excerpt, runbook_url) = load_runbook_payload(pool, alert_type).await;

    // Phase 4 W3: if the alert_rules row attaches an escalation policy,
    // page only the channels routed by step 0 (e.g. the current on-call
    // user). NULL policy_id preserves the pre-W3 behaviour exactly.
    let policy_id = get_user_policy_id(pool, user_id, server_id).await;
    if let Some(pid) = policy_id {
        let steps = load_escalation_steps(pool, pid).await;
        if let Some(step0) = steps.first() {
            dispatch_escalation_step(
                pool,
                user_id,
                server_id,
                alert_type,
                step0,
                &subject,
                message,
                &html,
                runbook_excerpt.as_deref(),
                runbook_url.as_deref(),
            )
            .await;
            return Ok(());
        }
        tracing::warn!(
            "Alert rule references escalation_policy {pid} with empty/invalid steps — falling back to default channel fan-out"
        );
    }

    // NULL policy (or fallback for malformed policy) → pre-W3 behaviour:
    // page the alert owner's channels with their own mute prefs applied.
    fanout_to_user(
        pool,
        user_id,
        server_id,
        alert_type,
        &subject,
        message,
        &html,
        runbook_excerpt.as_deref(),
        runbook_url.as_deref(),
    )
    .await;

    Ok(())
}

/// Insert notification into the panel notification center (bell icon).
/// Pass user_id = None to notify all admins.
/// Also broadcasts via SSE for real-time delivery.
pub async fn notify_panel(
    db: &sqlx::PgPool,
    user_id: Option<uuid::Uuid>,
    title: &str,
    message: &str,
    severity: &str,
    category: &str,
    link: Option<&str>,
) {
    // Build JSON payload once for SSE broadcast
    let notif_json = serde_json::json!({
        "title": title,
        "message": message,
        "severity": severity,
        "category": category,
        "link": link,
    })
    .to_string();

    if let Some(uid) = user_id {
        let _ = sqlx::query(
            "INSERT INTO panel_notifications (user_id, title, message, severity, category, link) VALUES ($1, $2, $3, $4, $5, $6)"
        ).bind(uid).bind(title).bind(message).bind(severity).bind(category).bind(link)
        .execute(db).await;

        // Broadcast to SSE subscribers
        if let Some(tx) = NOTIF_TX.get() {
            let _ = tx.send((uid, notif_json));
        }
    } else {
        let admins: Vec<(uuid::Uuid,)> = sqlx::query_as("SELECT id FROM users WHERE role = 'admin'")
            .fetch_all(db).await.unwrap_or_default();
        for (admin_id,) in &admins {
            let _ = sqlx::query(
                "INSERT INTO panel_notifications (user_id, title, message, severity, category, link) VALUES ($1, $2, $3, $4, $5, $6)"
            ).bind(admin_id).bind(title).bind(message).bind(severity).bind(category).bind(link)
            .execute(db).await;

            // Broadcast to SSE subscribers
            if let Some(tx) = NOTIF_TX.get() {
                let _ = tx.send((*admin_id, notif_json.clone()));
            }
        }
    }
}

/// Resolve a firing alert and send recovery notification.
pub async fn resolve_alert(
    pool: &PgPool,
    user_id: Uuid,
    server_id: Option<Uuid>,
    site_id: Option<Uuid>,
    alert_type: &str,
    title: &str,
    message: &str,
) {
    // Resolve firing alerts of this type
    let query = if server_id.is_some() {
        "UPDATE alerts SET status = 'resolved', resolved_at = NOW() \
         WHERE user_id = $1 AND server_id = $2 AND alert_type = $3 AND status = 'firing'"
    } else if site_id.is_some() {
        "UPDATE alerts SET status = 'resolved', resolved_at = NOW() \
         WHERE user_id = $1 AND site_id = $2 AND alert_type = $3 AND status = 'firing'"
    } else {
        return;
    };

    let Some(id) = server_id.or(site_id) else {
        tracing::warn!("resolve_alert called with no server_id or site_id");
        return;
    };
    let _ = sqlx::query(query)
        .bind(user_id)
        .bind(id)
        .bind(alert_type)
        .execute(pool)
        .await;

    // Send recovery notification
    if let Some(channels) = get_user_channels(pool, user_id, server_id).await {
        let subject = format!("DockPanel Resolved: {title}");
        let html = format!(
            "<div style=\"font-family:sans-serif;max-width:600px;margin:0 auto\">\
             <h2 style=\"color:#10b981\">{title}</h2>\
             <p>{message}</p>\
             <p style=\"color:#6b7280;font-size:14px\">Time: {}</p>\
             </div>",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        );
        send_notification(pool, &channels, &subject, message, &html).await;
    }

    // Panel notification center
    notify_panel(pool, Some(user_id), &format!("Resolved: {}", title), message, "info", "alert", None).await;
}
