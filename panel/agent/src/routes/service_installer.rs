use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::time::Duration;
use crate::safe_cmd::{safe_command, safe_command_unsandboxed};

use super::AppState;

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: &str) -> ApiErr {
    (status, Json(serde_json::json!({ "error": msg })))
}

fn ok(msg: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "message": msg }))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/services/install-status", get(install_status))
        .route("/services/install/php", post(install_php))
        .route("/services/install/certbot", post(install_certbot))
        .route("/services/install/ufw", post(install_ufw))
        .route("/services/install/fail2ban", post(install_fail2ban))
        .route("/services/install/powerdns", post(install_powerdns))
        .route("/services/install/redis", post(install_redis))
        .route("/services/install/nodejs", post(install_nodejs))
        .route("/services/install/composer", post(install_composer))
        .route("/services/uninstall/php", post(uninstall_php))
        .route("/services/uninstall/certbot", post(uninstall_certbot))
        .route("/services/uninstall/ufw", post(uninstall_ufw))
        .route("/services/uninstall/fail2ban", post(uninstall_fail2ban))
        .route("/services/uninstall/powerdns", post(uninstall_powerdns))
        .route("/services/uninstall/redis", post(uninstall_redis))
        .route("/services/uninstall/nodejs", post(uninstall_nodejs))
        .route("/services/uninstall/composer", post(uninstall_composer))
        .route("/services/install/waf", post(install_waf))
        .route("/services/uninstall/waf", post(uninstall_waf))
        .route("/services/install/cloudflared", post(install_cloudflared))
        .route("/services/uninstall/cloudflared", post(uninstall_cloudflared))
        .route("/services/cloudflared/configure", post(configure_cloudflared))
        .route("/services/cloudflared/status", get(cloudflared_status))
}

// ── Status check ────────────────────────────────────────────────────────

async fn install_status() -> Result<Json<serde_json::Value>, ApiErr> {
    let pdns_installed = is_installed("pdns-server").await;
    let pdns_running = is_active("pdns").await;

    let php_installed = is_installed("php-fpm").await || is_installed("php8.4-fpm").await || is_installed("php8.3-fpm").await || is_installed("php8.2-fpm").await || is_installed("php8.1-fpm").await;
    let php_running = is_active("php8.4-fpm").await || is_active("php8.3-fpm").await || is_active("php8.2-fpm").await || is_active("php8.1-fpm").await;
    let certbot_installed = which("certbot").await;
    let ufw_installed = which("ufw").await;
    let ufw_active = is_ufw_active().await;
    let fail2ban_installed = is_installed("fail2ban").await;
    let fail2ban_running = is_active("fail2ban").await;

    // Detect installed PHP version
    let php_version = detect_php_version().await;

    let redis_installed = which("redis-server").await || is_installed("redis-server").await;
    let redis_running = is_active("redis-server").await;

    let nodejs_installed = which("node").await;
    let composer_installed = which("composer").await;

    // Ubuntu 24.04 (Noble) renamed libmodsecurity3 → libmodsecurity3t64 in the
    // time_t-64 ABI transition (virtual-provides, no transitional shim). Check
    // both names so the install-state detect works on Noble and Debian alike.
    let waf_installed = std::path::Path::new("/etc/modsecurity/modsecurity.conf").exists()
        && (is_installed("libmodsecurity3").await
            || is_installed("libmodsecurity3t64").await);

    let cloudflared_installed = which("cloudflared").await;
    let cloudflared_running = is_active("cloudflared").await;

    Ok(Json(serde_json::json!({
        "php": { "installed": php_installed, "running": php_running, "version": php_version },
        "certbot": { "installed": certbot_installed },
        "ufw": { "installed": ufw_installed, "active": ufw_active },
        "fail2ban": { "installed": fail2ban_installed, "running": fail2ban_running },
        "powerdns": { "installed": pdns_installed, "running": pdns_running },
        "redis": { "installed": redis_installed, "running": redis_running },
        "nodejs": { "installed": nodejs_installed },
        "composer": { "installed": composer_installed },
        "waf": { "installed": waf_installed },
        "cloudflared": { "installed": cloudflared_installed, "running": cloudflared_running },
    })))
}

// ── PHP installer ───────────────────────────────────────────────────────

async fn install_php() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing PHP...");

    // Detect best PHP version available
    let version = detect_available_php().await.unwrap_or_else(|| "8.3".to_string());

    let packages = format!(
        "php{v}-fpm php{v}-cli php{v}-mysql php{v}-pgsql php{v}-sqlite3 \
         php{v}-curl php{v}-gd php{v}-mbstring php{v}-xml php{v}-zip \
         php{v}-bcmath php{v}-intl php{v}-readline php{v}-opcache",
        v = version
    );

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", &format!("DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confnew install -y {packages}")])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "PHP install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("apt install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PHP install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Enable and start PHP-FPM
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["enable", &format!("php{version}-fpm")]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["start", &format!("php{version}-fpm")]).output()).await;

    tracing::info!("PHP {version} installed");
    Ok(ok(&format!("PHP {version} with FPM installed and started")))
}

// ── Certbot installer ───────────────────────────────────────────────────

async fn install_certbot() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing Certbot...");

    // Remove old apt-based certbot first (if present) to avoid conflicts
    let _ = tokio::time::timeout(
        Duration::from_secs(120),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "systemctl stop certbot.timer 2>/dev/null; systemctl disable certbot.timer 2>/dev/null; DEBIAN_FRONTEND=noninteractive apt-get purge -y certbot python3-certbot-nginx 2>/dev/null; true"])
            .output()
    ).await;

    // Strategy: snap (gets certbot 4.x with ARI support for 45-day certs)
    // Fallback: pip (works when snap is unavailable)
    let snap_ok = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "snap install --classic certbot && ln -sf /snap/bin/certbot /usr/bin/certbot"])
            .output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Install nginx plugin separately (non-fatal if it fails — certbot still works)
    if snap_ok {
        let _ = tokio::time::timeout(
            Duration::from_secs(120),
            safe_command_unsandboxed("sh", &[])
                .args(["-c", "snap set certbot trust-plugin-with-root=ok && snap install certbot-nginx"])
                .output()
        ).await;
    }

    if snap_ok {
        tracing::info!("Certbot installed via snap (4.x with ARI support)");
        // snap auto-renewal runs via snap.certbot.renew.timer
        let _ = tokio::time::timeout(Duration::from_secs(30),
            safe_command("systemctl").args(["enable", "snap.certbot.renew.timer"]).output()).await;
        let _ = tokio::time::timeout(Duration::from_secs(30),
            safe_command("systemctl").args(["start", "snap.certbot.renew.timer"]).output()).await;
        return Ok(ok("Certbot 4.x installed via snap with nginx plugin and auto-renewal (ARI-ready for 45-day certs)"));
    }

    tracing::warn!("Snap certbot failed, falling back to pip...");

    // Fallback: pip install (gets latest certbot from PyPI)
    let pip_ok = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y python3-venv && \
                python3 -m venv /opt/certbot && \
                /opt/certbot/bin/pip install --upgrade pip && \
                /opt/certbot/bin/pip install certbot certbot-nginx && \
                ln -sf /opt/certbot/bin/certbot /usr/bin/certbot"])
            .output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false);

    if pip_ok {
        tracing::info!("Certbot installed via pip");
        // Create systemd timer for auto-renewal
        let timer_unit = "[Unit]\nDescription=Certbot renewal timer\n\n[Timer]\nOnCalendar=*-*-* 00,12:00:00\nRandomizedDelaySec=3600\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n";
        let service_unit = "[Unit]\nDescription=Certbot renewal\n\n[Service]\nType=oneshot\nExecStart=/usr/bin/certbot renew --quiet --deploy-hook \"systemctl reload nginx\"\n";
        std::fs::write("/etc/systemd/system/certbot.timer", timer_unit).ok();
        std::fs::write("/etc/systemd/system/certbot.service", service_unit).ok();
        let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["daemon-reload"]).output()).await;
        let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["enable", "certbot.timer"]).output()).await;
        let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["start", "certbot.timer"]).output()).await;
        return Ok(ok("Certbot installed via pip with nginx plugin and auto-renewal timer"));
    }

    Err(err(StatusCode::INTERNAL_SERVER_ERROR, "Certbot install failed: both snap and pip methods failed"))
}

// ── UFW installer ───────────────────────────────────────────────────────

async fn install_ufw() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing UFW...");

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confnew install -y ufw"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "UFW install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("apt install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("UFW install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Configure default rules
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["default", "deny", "incoming"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["default", "allow", "outgoing"]).output()).await;

    // Open essential ports
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["allow", "22/tcp"]).output()).await;   // SSH
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["allow", "80/tcp"]).output()).await;   // HTTP
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["allow", "443/tcp"]).output()).await;  // HTTPS
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["allow", "587/tcp"]).output()).await;  // SMTP submission
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["allow", "993/tcp"]).output()).await;  // IMAPS

    // Enable (--force to skip interactive prompt)
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").args(["--force", "enable"]).output()).await;

    tracing::info!("UFW installed and enabled with default rules");
    Ok(ok("UFW installed — SSH, HTTP, HTTPS, SMTP, IMAPS ports opened"))
}

// ── Fail2Ban installer ──────────────────────────────────────────────────

async fn install_fail2ban() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing Fail2Ban...");

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confnew install -y fail2ban"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Fail2Ban install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("apt install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Fail2Ban install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Write default jail config
    let jail_config = r#"[DEFAULT]
bantime = 3600
findtime = 600
maxretry = 5

[sshd]
enabled = true

[nginx-http-auth]
enabled = true

[nginx-limit-req]
enabled = true

[postfix]
enabled = true

[dovecot]
enabled = true
"#;

    let _ = tokio::fs::write("/etc/fail2ban/jail.local", jail_config).await;

    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["enable", "fail2ban"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["restart", "fail2ban"]).output()).await;

    tracing::info!("Fail2Ban installed with default jails");
    Ok(ok("Fail2Ban installed with SSH, Nginx, Postfix, Dovecot jails"))
}

// ── PowerDNS installer ──────────────────────────────────────────────────

/// Addresses pdns should bind, or `None` to leave PowerDNS on its `0.0.0.0`
/// default.
///
/// Ubuntu runs systemd-resolved, whose stub listeners own `127.0.0.53:53` and
/// `127.0.0.54:53`. A wildcard bind therefore fails with EADDRINUSE and pdns
/// crash-loops, so when port 53 is already spoken for we pin pdns to the real
/// interface addresses and leave the stub resolver alone. Distros without a
/// stub listener (Debian 12 ships none) keep the wildcard bind, which also
/// keeps pdns listening on addresses added after install.
async fn pdns_local_addresses() -> Option<String> {
    // apt's postinst starts pdns with the stock config, so pdns may be holding
    // :53 itself by now. Ignore its own sockets — only a *foreign* listener
    // means the wildcard bind is unavailable.
    let listeners = safe_command("ss").args(["-tulnpH", "sport", "=", ":53"]).output().await
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let busy = listeners
        .lines()
        .any(|l| !l.trim().is_empty() && !l.contains("pdns"));
    if !busy {
        return None;
    }

    let out = safe_command("ip").args(["-o", "addr", "show", "scope", "global"]).output().await.ok()?;
    let mut addrs: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(3))
        .filter_map(|cidr| cidr.split('/').next())
        .map(str::to_string)
        .collect();
    if addrs.is_empty() {
        return None;
    }
    // Loopback stays reachable for local tooling; 127.0.0.1 never collides with
    // the stub listeners, which sit on .53/.54.
    addrs.push("127.0.0.1".to_string());
    Some(addrs.join(","))
}

/// The panel's PostgreSQL password, parsed out of the API's env file.
///
/// `setup.sh` writes `DATABASE_URL=postgresql://dockpanel:<pw>@127.0.0.1:<port>/dockpanel`
/// to `/etc/dockpanel/api.env` (mode 600, and the agent runs as root). Env vars
/// are checked first so a non-standard deployment can still override.
async fn panel_db_password() -> Option<String> {
    fn password_from_url(url: &str) -> Option<String> {
        let creds = url.split("://").nth(1)?.split('@').next()?;
        let pw = creds.split(':').nth(1)?;
        (!pw.is_empty()).then(|| pw.to_string())
    }

    if let Ok(pw) = std::env::var("PANEL_DB_PASSWORD") {
        if !pw.is_empty() {
            return Some(pw);
        }
    }
    if let Some(pw) = std::env::var("DATABASE_URL").ok().as_deref().and_then(password_from_url) {
        return Some(pw);
    }

    let env_file = tokio::fs::read_to_string("/etc/dockpanel/api.env").await.ok()?;
    env_file
        .lines()
        .find_map(|l| l.trim().strip_prefix("DATABASE_URL="))
        .map(|v| v.trim_matches(['"', '\''].as_ref()))
        .and_then(password_from_url)
}

/// Whether pdns settled into `active`.
///
/// A failing pdns is restarted by systemd, so a single probe can catch it
/// mid-`activating` and read as healthy. Poll until it is unambiguously up or
/// the budget runs out.
async fn pdns_is_active() -> bool {
    for _ in 0..10 {
        let state = safe_command("systemctl").args(["is-active", "pdns"]).output().await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if state == "active" {
            return true;
        }
        if state == "failed" || state == "inactive" {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// The most useful line from pdns's journal, for the operator-facing error.
async fn pdns_failure_detail() -> String {
    let journal = safe_command("journalctl")
        .args(["-u", "pdns", "--no-pager", "-n", "40", "-o", "cat"])
        .output().await
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    journal
        .lines()
        .rev()
        .find(|l| l.contains("Fatal error") || l.contains("Unable to bind") || l.contains("error"))
        .map(|l| l.trim().chars().take(300).collect::<String>())
        .unwrap_or_else(|| "check `journalctl -u pdns` for details".to_string())
}

/// Write `/etc/powerdns/pdns.conf` through the unsandboxed escape hatch.
///
/// `/etc/powerdns` is in the agent unit's `ReadWritePaths`, and systemd creates
/// it at unit start — but `apt-get purge pdns-server` (see the uninstaller)
/// deletes the directory, which detaches that bind mount for the remaining life
/// of the agent process. A direct `tokio::fs::write` then fails EROFS on every
/// reinstall until the agent restarts. Staging through `/var/lib/dockpanel` (a
/// live ReadWritePaths entry) and moving the file into place with `install`
/// sidesteps the detached mount entirely — and keeps the embedded API key off
/// both the process argv and a world-readable config.
async fn write_pdns_conf(contents: &str) -> Result<(), ApiErr> {
    const STAGED: &str = "/var/lib/dockpanel/pdns.conf.staged";

    // The staged copy carries the generated API key (and, on the pgsql path, the
    // database password), so create it 0600 rather than letting the umask decide
    // — and remove it on every exit path, not just the happy one.
    let staged_result = stage_and_install_pdns_conf(STAGED, contents).await;
    let _ = tokio::fs::remove_file(STAGED).await;
    staged_result
}

async fn stage_and_install_pdns_conf(staged: &str, contents: &str) -> Result<(), ApiErr> {
    // `mode` comes from tokio's own OpenOptions on unix — no std trait import needed.
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(staged)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to stage pdns.conf: {e}")))?;
    {
        use tokio::io::AsyncWriteExt;
        f.write_all(contents.as_bytes()).await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to stage pdns.conf: {e}")))?;
        f.flush().await.ok();
    }

    let placed = tokio::time::timeout(
        Duration::from_secs(60),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", &format!(
                "mkdir -p /etc/powerdns && install -o root -g pdns -m 640 {staged} /etc/powerdns/pdns.conf"
            )])
            .output(),
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Writing pdns.conf timed out"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to write pdns.conf: {e}")))?;

    if !placed.status.success() {
        let stderr = String::from_utf8_lossy(&placed.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!(
            "Failed to write pdns.conf: {}", stderr.trim().chars().take(300).collect::<String>()
        )));
    }
    Ok(())
}

async fn install_powerdns(body: axum::body::Bytes) -> Result<Json<serde_json::Value>, ApiErr> {
    // Optional JSON body {"backend": "sqlite" | "pgsql"}. Absent/empty/invalid → pgsql
    // (back-compat: older panels POST no body). SQLite removes the dependency on the
    // panel's containerized PostgreSQL that broke installs in issue #63.
    let use_sqlite = serde_json::from_slice::<serde_json::Value>(&body).ok()
        .and_then(|v| v.get("backend").and_then(|b| b.as_str()).map(str::to_string))
        .as_deref() == Some("sqlite");
    let backend_name = if use_sqlite { "sqlite" } else { "pgsql" };
    tracing::info!("Installing PowerDNS (backend={backend_name})...");

    // 1. Install packages (backend-specific)
    let pkgs = if use_sqlite {
        "pdns-server pdns-backend-sqlite3 sqlite3"
    } else {
        "pdns-server pdns-backend-pgsql"
    };
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", &format!("DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confnew install -y {pkgs}")])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "PowerDNS install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("apt install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PowerDNS install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // 2. Generate the HTTP-API key (shared by both backends)
    let api_key: String = {
        use rand::Rng;
        let mut rng = rand::rng();
        (0..32).map(|_| rng.sample(rand::distr::Alphanumeric) as char).collect()
    };

    // 3. Backend-specific storage setup
    if use_sqlite {
        // /var/lib/powerdns is NOT in the agent's ReadWritePaths, so create the DB
        // and load the schema via the unsandboxed escape hatch (same mechanism apt
        // uses above) — otherwise ProtectSystem=strict EROFS's the write. The schema
        // ships with pdns-backend-sqlite3 (plain or gzipped depending on distro).
        let setup = "mkdir -p /var/lib/powerdns; \
            SCHEMA=/usr/share/doc/pdns-backend-sqlite3/schema.sqlite3.sql; \
            if [ ! -s /var/lib/powerdns/pdns.sqlite3 ]; then \
              if [ -f \"$SCHEMA\" ]; then sqlite3 /var/lib/powerdns/pdns.sqlite3 < \"$SCHEMA\"; \
              elif [ -f \"$SCHEMA.gz\" ]; then zcat \"$SCHEMA.gz\" | sqlite3 /var/lib/powerdns/pdns.sqlite3; fi; \
            fi; \
            chown -R pdns:pdns /var/lib/powerdns";
        let sql_out = tokio::time::timeout(
            Duration::from_secs(120),
            safe_command_unsandboxed("sh", &[]).args(["-c", setup]).output()
        ).await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "PowerDNS SQLite schema load timed out"))?
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("SQLite setup failed: {e}")))?;
        if !sql_out.status.success() {
            let stderr = String::from_utf8_lossy(&sql_out.stderr);
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PowerDNS SQLite setup failed: {}", stderr.chars().take(300).collect::<String>())));
        }
    } else {
        // Create the pdns database in the panel's PostgreSQL container + load schema.
        let db_exists = tokio::time::timeout(
            Duration::from_secs(120),
            safe_command("docker")
                .args(["exec", "dockpanel-postgres", "psql", "-U", "dockpanel", "-lqt"])
                .output()
        ).await
            .ok()
            .and_then(|r| r.ok())
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("pdns"))
            .unwrap_or(false);

        if !db_exists {
            let _ = tokio::time::timeout(
                Duration::from_secs(120),
                safe_command("docker")
                    .args(["exec", "dockpanel-postgres", "psql", "-U", "dockpanel", "-c", "CREATE DATABASE pdns;"])
                    .output()
            ).await;

            // Load PowerDNS schema
            let schema_path = "/usr/share/doc/pdns-backend-pgsql/schema.pgsql.sql";
            if tokio::fs::metadata(schema_path).await.is_ok() {
                // Use shell pipe to feed schema to psql
                let _ = tokio::time::timeout(
                    Duration::from_secs(120),
                    safe_command_unsandboxed("sh", &[])
                        .args(["-c", &format!("cat {} | docker exec -i dockpanel-postgres psql -U dockpanel -d pdns", schema_path)])
                        .output()
                ).await;
            }
        }
    }

    // 4. Build the backend-specific config
    let mut pdns_conf = if use_sqlite {
        format!(r#"# DockPanel PowerDNS configuration (SQLite backend)
launch=gsqlite3
gsqlite3-database=/var/lib/powerdns/pdns.sqlite3

# HTTP API
api=yes
api-key={api_key}
webserver=yes
webserver-address=127.0.0.1
webserver-port=8081
webserver-allow-from=127.0.0.1

# SOA defaults
default-soa-content=ns1.@ hostmaster.@ 0 10800 3600 604800 3600
"#)
    } else {
        // Use the same DB password as the panel's postgres connection (never
        // hardcode). The agent unit carries no EnvironmentFile — it is started
        // with `Environment=RUST_LOG=info` and nothing else — so PANEL_DB_PASSWORD
        // and DATABASE_URL are never set here and the old env-var lookup always
        // fell through to a freshly generated random password. That password
        // matches no role, so pdns failed authentication on every start and the
        // pgsql backend could not have worked on any install. Read the real
        // credential from the API's env file instead.
        let pdns_db_password = panel_db_password().await.ok_or_else(|| err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read the panel database password from /etc/dockpanel/api.env — \
             install PowerDNS with the SQLite backend instead, or repair DATABASE_URL",
        ))?;

        format!(r#"# DockPanel PowerDNS configuration
launch=gpgsql
gpgsql-host=127.0.0.1
gpgsql-port=5450
gpgsql-dbname=pdns
gpgsql-user=dockpanel
gpgsql-password={pdns_db_password}

# HTTP API
api=yes
api-key={api_key}
webserver=yes
webserver-address=127.0.0.1
webserver-port=8081
webserver-allow-from=127.0.0.1

# SOA defaults
default-soa-content=ns1.@ hostmaster.@ 0 10800 3600 604800 3600
"#)
    };

    // 5. Pin the listening addresses when something already owns port 53
    if let Some(addrs) = pdns_local_addresses().await {
        tracing::info!("Port 53 already in use — binding PowerDNS to {addrs}");
        pdns_conf.push_str(&format!(
            "\n# Port 53 was already claimed at install time (systemd-resolved stub\n\
             # listener on 127.0.0.53/127.0.0.54 on Ubuntu), so bind explicitly.\n\
             local-address={addrs}\n"
        ));
    }

    // 6. Write PowerDNS config
    write_pdns_conf(&pdns_conf).await?;

    // 7. Enable and start. `enable` stays best-effort, but a pdns that never
    //    comes up must not be reported as a successful install — that silence
    //    is what let the port-53 conflict ship unnoticed.
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["reset-failed", "pdns"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["enable", "pdns"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["restart", "pdns"]).output()).await;

    if !pdns_is_active().await {
        let detail = pdns_failure_detail().await;
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!(
            "PowerDNS was installed and configured ({backend_name} backend) but the service failed to start: {detail}"
        )));
    }

    tracing::info!("PowerDNS installed with API key (backend={backend_name})");

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("PowerDNS installed and configured ({backend_name} backend)"),
        "api_url": "http://127.0.0.1:8081",
        "api_key": api_key,
        "backend": backend_name,
    })))
}

// ── Redis installer ────────────────────────────────────────────────

async fn install_redis() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing Redis...");

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get update && DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confnew install -y redis-server"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Redis install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("apt install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Redis install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Enable and start Redis
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["enable", "redis-server"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["start", "redis-server"]).output()).await;

    // Verify Redis is responding
    let verify = tokio::time::timeout(
        Duration::from_secs(10),
        safe_command("redis-cli").arg("ping").output()
    ).await;
    let verified = verify
        .ok()
        .and_then(|r| r.ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_uppercase() == "PONG")
        .unwrap_or(false);

    tracing::info!("Redis installed, ping verified: {verified}");
    Ok(ok("Redis installed and started"))
}

// ── Node.js installer ──────────────────────────────────────────────

async fn install_nodejs() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing Node.js...");

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confnew install -y nodejs"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Node.js install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Node.js install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Node.js install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Verify
    let ver = tokio::time::timeout(
        Duration::from_secs(10),
        safe_command("node").arg("--version").output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    tracing::info!("Node.js {ver} installed");
    Ok(ok(&format!("Node.js {ver} with npm installed")))
}

// ── Composer installer ─────────────────────────────────────────────

async fn install_composer() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing Composer...");

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "curl -sS https://getcomposer.org/installer | php -- --install-dir=/usr/local/bin --filename=composer"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Composer install timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Composer install failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Composer install failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Verify
    let ver = tokio::time::timeout(
        Duration::from_secs(10),
        safe_command("composer").arg("--version").output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    tracing::info!("Composer installed: {ver}");
    Ok(ok("Composer installed globally at /usr/local/bin/composer"))
}

// ── PHP uninstaller ─────────────────────────────────────────────────────

async fn uninstall_php() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling PHP...");

    let version = detect_php_version().await.unwrap_or_else(|| "8.3".to_string());

    // Stop and disable PHP-FPM
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["stop", &format!("php{version}-fpm")]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["disable", &format!("php{version}-fpm")]).output()).await;

    // Purge all PHP packages for this version
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", &format!("DEBIAN_FRONTEND=noninteractive apt-get purge -y php{version}-* && apt-get autoremove -y")])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "PHP uninstall timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PHP uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PHP uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    tracing::info!("PHP {version} uninstalled");
    Ok(ok(&format!("PHP {version} purged and removed")))
}

// ── Certbot uninstaller ─────────────────────────────────────────────────

async fn uninstall_certbot() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling Certbot...");

    // Stop all possible renewal timers
    let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["stop", "certbot.timer"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["disable", "certbot.timer"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["stop", "snap.certbot.renew.timer"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["disable", "snap.certbot.renew.timer"]).output()).await;

    // Remove snap certbot (if installed)
    let _ = tokio::time::timeout(
        Duration::from_secs(120),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "snap remove certbot 2>/dev/null; snap remove certbot-nginx 2>/dev/null; true"])
            .output()
    ).await;

    // Remove pip certbot (if installed)
    if std::path::Path::new("/opt/certbot").exists() {
        let _ = std::fs::remove_dir_all("/opt/certbot");
        let _ = std::fs::remove_file("/etc/systemd/system/certbot.timer");
        let _ = std::fs::remove_file("/etc/systemd/system/certbot.service");
        let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["daemon-reload"]).output()).await;
    }

    // Remove apt certbot (if installed)
    let _ = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y certbot python3-certbot-nginx 2>/dev/null; apt-get autoremove -y 2>/dev/null; true"])
            .output()
    ).await;

    // Clean up symlink
    let _ = std::fs::remove_file("/usr/bin/certbot");

    tracing::info!("Certbot uninstalled");
    Ok(ok("Certbot and auto-renewal timer removed"))
}

// ── UFW uninstaller ─────────────────────────────────────────────────────

async fn uninstall_ufw() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling UFW...");

    // Disable UFW first
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("ufw").arg("disable").output()).await;

    // Purge UFW
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y ufw && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "UFW uninstall timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("UFW uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("UFW uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    tracing::info!("UFW uninstalled");
    Ok(ok("UFW disabled and removed"))
}

// ── Fail2Ban uninstaller ────────────────────────────────────────────────

async fn uninstall_fail2ban() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling Fail2Ban...");

    // Stop and disable fail2ban
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["stop", "fail2ban"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["disable", "fail2ban"]).output()).await;

    // Purge fail2ban
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y fail2ban && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Fail2Ban uninstall timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Fail2Ban uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Fail2Ban uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Remove custom jail config
    let _ = tokio::fs::remove_file("/etc/fail2ban/jail.local").await;

    tracing::info!("Fail2Ban uninstalled");
    Ok(ok("Fail2Ban stopped and purged with jail config removed"))
}

// ── PowerDNS uninstaller ────────────────────────────────────────────────

async fn uninstall_powerdns() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling PowerDNS...");

    // Stop and disable pdns
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["stop", "pdns"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["disable", "pdns"]).output()).await;

    // Purge PowerDNS packages
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y pdns-server pdns-backend-pgsql pdns-backend-sqlite3 && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "PowerDNS uninstall timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PowerDNS uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("PowerDNS uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Remove config file (but keep the pdns database — user may want DNS records)
    let _ = tokio::fs::remove_file("/etc/powerdns/pdns.conf").await;

    tracing::info!("PowerDNS uninstalled (database preserved)");
    Ok(ok("PowerDNS purged and config removed (database preserved)"))
}

// ── Redis uninstaller ───────────────────────────────────────────────────

async fn uninstall_redis() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling Redis...");

    // Stop and disable redis-server
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["stop", "redis-server"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), safe_command("systemctl").args(["disable", "redis-server"]).output()).await;

    // Purge redis
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y redis-server && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Redis uninstall timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Redis uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Redis uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    tracing::info!("Redis uninstalled");
    Ok(ok("Redis stopped and purged"))
}

// ── Node.js uninstaller ─────────────────────────────────────────────────

async fn uninstall_nodejs() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling Node.js...");

    // Purge nodejs
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y nodejs && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Node.js uninstall timed out after 300s"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Node.js uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Node.js uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Remove nodesource apt repo if present
    let _ = tokio::fs::remove_file("/etc/apt/sources.list.d/nodesource.list").await;

    tracing::info!("Node.js uninstalled");
    Ok(ok("Node.js purged and nodesource repo removed"))
}

// ── Composer uninstaller ────────────────────────────────────────────────

async fn uninstall_composer() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling Composer...");

    // Composer is just a binary — remove it. Wrapped in systemd-run because
    // /usr/local/bin is under /usr, which the agent's ProtectSystem=strict
    // sandbox makes read-only.
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        safe_command_unsandboxed("rm", &[]).args(["-f", "/usr/local/bin/composer"]).output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Composer uninstall timed out"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Composer uninstall failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Composer uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    tracing::info!("Composer uninstalled");
    Ok(ok("Composer binary removed from /usr/local/bin"))
}

// ── Helpers ─────────────────────────────────────────────────────────────

async fn is_installed(package: &str) -> bool {
    tokio::time::timeout(
        Duration::from_secs(120),
        safe_command("dpkg").args(["-l", package]).output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("ii"))
        .unwrap_or(false)
}

async fn is_active(service: &str) -> bool {
    tokio::time::timeout(
        Duration::from_secs(120),
        safe_command("systemctl").args(["is-active", "--quiet", service]).output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn which(cmd: &str) -> bool {
    tokio::time::timeout(
        Duration::from_secs(120),
        safe_command("which").arg(cmd).output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn is_ufw_active() -> bool {
    tokio::time::timeout(
        Duration::from_secs(120),
        safe_command("ufw").arg("status").output()
    ).await
        .ok()
        .and_then(|r| r.ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Status: active"))
        .unwrap_or(false)
}

async fn detect_php_version() -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        safe_command("php").args(["-r", "echo PHP_MAJOR_VERSION.'.'.PHP_MINOR_VERSION;"]).output()
    ).await.ok()?.ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

// ── WAF (ModSecurity3 + OWASP CRS) installer ───────────────────────

async fn install_waf() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing WAF (ModSecurity3 + OWASP CRS)...");

    // 1. Install libmodsecurity3 and nginx connector
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get update && \
                DEBIAN_FRONTEND=noninteractive apt-get install -y libmodsecurity3 libnginx-mod-http-modsecurity"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "WAF install timed out"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("apt install: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR,
            &format!("WAF install failed (libmodsecurity3 or nginx module not available): {}",
                stderr.chars().take(400).collect::<String>())));
    }

    // 2. Create directory structure
    let dirs = ["/etc/modsecurity", "/etc/modsecurity/sites", "/var/log/modsecurity"];
    for dir in dirs {
        std::fs::create_dir_all(dir).ok();
    }

    // 3. Download OWASP CRS v4
    let crs_dir = "/etc/modsecurity/crs";
    if !std::path::Path::new(&format!("{crs_dir}/crs-setup.conf")).exists() {
        let dl = tokio::time::timeout(
            Duration::from_secs(120),
            safe_command_unsandboxed("sh", &[])
                .args(["-c", &format!(
                    "cd /tmp && \
                     curl -sL https://github.com/coreruleset/coreruleset/archive/refs/tags/v4.25.0.tar.gz -o crs.tar.gz && \
                     tar xzf crs.tar.gz && \
                     rm -rf {crs_dir} && \
                     mv coreruleset-4.25.0 {crs_dir} && \
                     cp {crs_dir}/crs-setup.conf.example {crs_dir}/crs-setup.conf && \
                     rm -f crs.tar.gz"
                )])
                .output()
        ).await;

        match dl {
            Ok(Ok(o)) if o.status.success() => {
                tracing::info!("OWASP CRS v4.25.0 downloaded");
            }
            _ => {
                tracing::warn!("OWASP CRS download failed — WAF will work without rules");
            }
        }
    }

    // 4. Write base ModSecurity config
    let modsec_conf = r#"# ModSecurity base config (managed by DockPanel)
SecRuleEngine DetectionOnly
SecRequestBodyAccess On
SecRequestBodyLimit 13107200
SecRequestBodyNoFilesLimit 131072
SecResponseBodyAccess Off
SecTmpDir /tmp/
SecDataDir /tmp/
SecAuditEngine RelevantOnly
SecAuditLogRelevantStatus "^(?:5|4(?!04))"
SecAuditLogParts ABIJDEFHZ
SecAuditLogType Serial
SecAuditLog /var/log/modsecurity/modsec_audit.log
SecArgumentSeparator &
SecCookieFormat 0
SecUnicodeMapFile unicode.mapping 20127
SecStatusEngine Off
"#;

    std::fs::write("/etc/modsecurity/modsecurity.conf", modsec_conf)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Write modsec config: {e}")))?;

    // 5. Write unicode mapping (required by ModSecurity)
    if !std::path::Path::new("/etc/modsecurity/unicode.mapping").exists() {
        // Try to copy from default location or create minimal one
        let _ = std::fs::copy(
            "/usr/share/modsecurity-crs/unicode.mapping",
            "/etc/modsecurity/unicode.mapping",
        );
        if !std::path::Path::new("/etc/modsecurity/unicode.mapping").exists() {
            std::fs::write("/etc/modsecurity/unicode.mapping", "").ok();
        }
    }

    // 6. Verify nginx can load the module
    let test = tokio::time::timeout(
        Duration::from_secs(10),
        safe_command("nginx").args(["-t"]).output()
    ).await;

    let nginx_ok = test.ok()
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !nginx_ok {
        tracing::warn!("nginx -t failed after WAF install — module may need manual load_module directive");
    }

    tracing::info!("WAF installed (ModSecurity3 + OWASP CRS)");
    Ok(ok("WAF installed: ModSecurity3 with OWASP Core Rule Set v4.25.0"))
}

async fn uninstall_waf() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling WAF...");

    // Remove WAF directives from all nginx configs
    let sites_dir = "/etc/nginx/sites-enabled";
    if let Ok(entries) = std::fs::read_dir(sites_dir) {
        for entry in entries.flatten() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if content.contains("modsecurity") {
                    let cleaned: String = content.lines()
                        .filter(|l| !l.contains("modsecurity"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = std::fs::write(entry.path(), cleaned);
                }
            }
        }
    }

    // Reload nginx to remove WAF from active configs
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        safe_command("nginx").args(["-s", "reload"]).output()
    ).await;

    // Purge packages
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y libnginx-mod-http-modsecurity libmodsecurity3 libmodsecurity3t64 && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "WAF uninstall timed out"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("WAF uninstall: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR,
            &format!("WAF uninstall failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    // Clean up config files (preserve logs)
    let _ = std::fs::remove_dir_all("/etc/modsecurity");

    tracing::info!("WAF uninstalled");
    Ok(ok("WAF (ModSecurity3) uninstalled"))
}

// ── Cloudflare Tunnel (cloudflared) ─────────────────────────────────

async fn install_cloudflared() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Installing cloudflared...");

    // Pre-resolve the codename in Rust. Inline `$(lsb_release -cs)` in
    // single-quoted bash didn't expand and shipped the literal string into
    // /etc/apt/sources.list.d/cloudflared.list, breaking apt-get update
    // system-wide (issue #57 follow-up, v2.8.19).
    let codename = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("VERSION_CODENAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot detect OS codename from /etc/os-release (need VERSION_CODENAME)"))?;

    let script = format!(
        "curl -sL https://pkg.cloudflare.com/cloudflare-main.gpg -o /usr/share/keyrings/cloudflare-main.gpg && \
         printf 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared %s main\\n' {codename} > /etc/apt/sources.list.d/cloudflared.list && \
         DEBIAN_FRONTEND=noninteractive apt-get update && \
         DEBIAN_FRONTEND=noninteractive apt-get install -y cloudflared"
    );

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        safe_command_unsandboxed("sh", &[]).args(["-c", &script]).output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "cloudflared install timed out"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Install: {e}")))?;

    if !output.status.success() {
        // A partial install can leave a half-written sources file behind which
        // would break every subsequent apt-get update on the box. Wipe it.
        let _ = std::fs::remove_file("/etc/apt/sources.list.d/cloudflared.list");
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cloudflared install failed: {}", stderr.chars().take(400).collect::<String>())));
    }

    std::fs::create_dir_all("/etc/cloudflared").ok();

    let verify = tokio::time::timeout(
        Duration::from_secs(5),
        safe_command("cloudflared").args(["version"]).output()
    ).await;
    let version = verify.ok()
        .and_then(|r| r.ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    tracing::info!("cloudflared installed: {version}");
    Ok(ok(&format!("cloudflared installed: {version}")))
}

async fn uninstall_cloudflared() -> Result<Json<serde_json::Value>, ApiErr> {
    tracing::info!("Uninstalling cloudflared...");

    let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["stop", "cloudflared"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), safe_command("systemctl").args(["disable", "cloudflared"]).output()).await;

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        safe_command_unsandboxed("sh", &[])
            .args(["-c", "DEBIAN_FRONTEND=noninteractive apt-get purge -y cloudflared && apt-get autoremove -y"])
            .output()
    ).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Timeout"))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Uninstall: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed: {}", stderr.chars().take(300).collect::<String>())));
    }

    let _ = std::fs::remove_dir_all("/etc/cloudflared");
    tracing::info!("cloudflared uninstalled");
    Ok(ok("cloudflared uninstalled"))
}

/// POST /services/cloudflared/configure — Configure tunnel with token and ingress rules.
async fn configure_cloudflared(
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let token = body.get("token").and_then(|v| v.as_str()).unwrap_or("");
    if token.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Missing tunnel token"));
    }

    // Validate token format: must be base64-like, no newlines/specifiers/shell chars
    if token.len() < 50 || token.len() > 4096
        || token.contains('\n') || token.contains('\r') || token.contains('\0')
        || token.contains('%') || token.contains('\'') || token.contains('"')
        || token.contains(';') || token.contains('|') || token.contains('&')
        || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '=')
    {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid tunnel token format"));
    }

    std::fs::create_dir_all("/etc/cloudflared").ok();

    // Write the tunnel token to a root-only (0600) EnvironmentFile so it is NOT
    // world-readable in the unit file and NOT exposed on the process command line
    // (cloudflared reads TUNNEL_TOKEN from the environment for `tunnel run`).
    let token_env_path = "/etc/cloudflared/token.env";
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(token_env_path)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Write token file: {e}")))?;
        f.write_all(format!("TUNNEL_TOKEN={token}\n").as_bytes())
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Write token file: {e}")))?;
    }
    // Harden perms even if the file already existed with a looser mode.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(token_env_path, std::fs::Permissions::from_mode(0o600));
    }

    // Write systemd service. The token is supplied via the 0600 EnvironmentFile
    // (TUNNEL_TOKEN), never on the ExecStart command line.
    let service = format!(
        "[Unit]\n\
         Description=Cloudflare Tunnel\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         EnvironmentFile={token_env_path}\n\
         ExecStart=/usr/bin/cloudflared tunnel run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         LimitNOFILE=65536\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    );

    std::fs::write("/etc/systemd/system/cloudflared.service", &service)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Write service: {e}")))?;

    // Reload systemd and start
    let _ = tokio::time::timeout(Duration::from_secs(10), safe_command("systemctl").args(["daemon-reload"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), safe_command("systemctl").args(["enable", "cloudflared"]).output()).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), safe_command("systemctl").args(["restart", "cloudflared"]).output()).await;

    // Check if running
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let running = is_active("cloudflared").await;

    tracing::info!("Cloudflare Tunnel configured, running: {running}");
    Ok(Json(serde_json::json!({
        "ok": true,
        "running": running,
    })))
}

/// GET /services/cloudflared/status — Get tunnel connection status.
async fn cloudflared_status() -> Result<Json<serde_json::Value>, ApiErr> {
    let installed = which("cloudflared").await;
    let running = is_active("cloudflared").await;

    let version = if installed {
        tokio::time::timeout(
            Duration::from_secs(5),
            safe_command("cloudflared").args(["version"]).output()
        ).await.ok()
            .and_then(|r| r.ok())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    };

    let has_service = std::path::Path::new("/etc/systemd/system/cloudflared.service").exists();

    Ok(Json(serde_json::json!({
        "installed": installed,
        "running": running,
        "version": version,
        "configured": has_service,
    })))
}

async fn detect_available_php() -> Option<String> {
    for v in ["8.3", "8.2", "8.1"] {
        let output = tokio::time::timeout(
            Duration::from_secs(120),
            safe_command("apt-cache").args(["show", &format!("php{v}-fpm")]).output()
        ).await;
        if output.ok().and_then(|r| r.ok()).map(|o| o.status.success()).unwrap_or(false) {
            return Some(v.to_string());
        }
    }
    None
}
