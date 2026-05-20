#!/usr/bin/env bash
#
# DockPanel Updater
# Pulls latest code, rebuilds binaries + frontend, restarts services.
# Preserves database, secrets, and configuration.
#
# Usage: bash scripts/update.sh
#        INSTALL_FROM_RELEASE=1 bash scripts/update.sh  # Download pre-built binaries
#
set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
BOLD='\033[1m'
NC='\033[0m'

log()    { echo -e "${GREEN}[+]${NC} $1"; }
warn()   { echo -e "${YELLOW}[!]${NC} $1"; }
error()  { echo -e "${RED}[x]${NC} $1" >&2; }

# ── Checks ────────────────────────────────────────────────────────────────
if [ "$EUID" -ne 0 ]; then
    error "Run as root"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
AGENT_SRC="$REPO_DIR/panel/agent"
API_SRC="$REPO_DIR/panel/backend"
CLI_SRC="$REPO_DIR/panel/cli"
FRONTEND_DIR="$REPO_DIR/panel/frontend"
AGENT_BIN="/usr/local/bin/dockpanel-agent"
API_BIN="/usr/local/bin/dockpanel-api"
CLI_BIN="/usr/local/bin/dockpanel"
INSTALL_FROM_RELEASE="${INSTALL_FROM_RELEASE:-0}"
GITHUB_REPO="ovexro/dockpanel"

# ── Mode detection (must run BEFORE self-refresh) ─────────────────────────
# Self-refresh is gated on INSTALL_FROM_RELEASE=1, so the auto-detect that
# flips it to 1 has to happen first. v2.8.15 and earlier had this in the
# wrong order: a user running `bash update.sh` (no env vars) entered with
# INSTALL_FROM_RELEASE=0, failed the self-refresh check, then got bumped
# to 1 by auto-detect — but with the stale local script still running.
# Result: binaries upgrade fine, but script-side fixes (unit files, nginx
# tweaks, install-agent.sh deploy) never reach pre-v2.8.16 panels.
if [ "$INSTALL_FROM_RELEASE" != "1" ] && [ ! -d "$AGENT_SRC/src" ]; then
    log "No source found — switching to pre-built binary download"
    INSTALL_FROM_RELEASE=1
fi

# Auto-detect: if Rust toolchain isn't available, use release binaries.
# Production VPS installs typically don't have cargo on PATH (and usually
# don't have enough RAM to compile rustc's dep tree — proc-macro2 OOMs at
# ~1-2 GB). Fall back to the pre-built artifacts on the matching tag rather
# than asking the operator to install rustup just to update.
if [ "$INSTALL_FROM_RELEASE" != "1" ] \
   && ! command -v cargo > /dev/null 2>&1 \
   && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    log "Rust toolchain not found — switching to pre-built binary download"
    log "(set BUILD_FROM_SOURCE=1 to force compile-from-source instead)"
    INSTALL_FROM_RELEASE=1
fi

# Explicit opt-in to keep compile-from-source behaviour even when cargo
# is on PATH (e.g. for developers iterating on a checkout).
if [ "${BUILD_FROM_SOURCE:-0}" = "1" ]; then
    INSTALL_FROM_RELEASE=0
fi

# ── Self-refresh ──────────────────────────────────────────────────────────
# In binary-release mode, the on-disk copy of this script can lag the
# repo by several releases (it's only refreshed by re-running install.sh).
# That means a bug in update.sh — like the 405-rollback bug fixed in
# v2.7.13 — strands operators unable to upgrade. Pull the latest script
# from the latest release tag and re-exec ourselves before running any
# update logic. SELF_REFRESHED=1 prevents an infinite re-exec loop.
# DOCKPANEL_NO_SELF_REFRESH=1 — set by the panel-internal orchestrator
# (services/panel_update.rs) so it can stream a single update.sh invocation's
# stdout into its state machine without a mid-flight re-exec breaking the pipe.
# SSH-operator flow keeps self-refresh on by default.
if [ "${SELF_REFRESHED:-0}" != "1" ] && [ "$INSTALL_FROM_RELEASE" = "1" ] \
   && [ "${DOCKPANEL_NO_SELF_REFRESH:-0}" != "1" ]; then
    LATEST_TAG=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" 2>/dev/null \
        | grep -m1 '"tag_name"' | cut -d'"' -f4 || true)
    if [ -n "$LATEST_TAG" ]; then
        REMOTE_URL="https://raw.githubusercontent.com/${GITHUB_REPO}/${LATEST_TAG}/scripts/update.sh"
        TMP=$(mktemp)
        if curl -fsSL "$REMOTE_URL" -o "$TMP" 2>/dev/null && [ -s "$TMP" ]; then
            # Compare to current to avoid an unnecessary re-exec on every run
            if ! cmp -s "$TMP" "${BASH_SOURCE[0]}"; then
                log "Refreshing update.sh from $LATEST_TAG (current copy is stale)"
                cp "$TMP" "${BASH_SOURCE[0]}" 2>/dev/null || true
                rm -f "$TMP"
                export SELF_REFRESHED=1
                exec bash "${BASH_SOURCE[0]}" "$@"
            fi
            rm -f "$TMP"
        else
            rm -f "$TMP"
        fi
    fi
fi

# For source builds, verify source exists
if [ "$INSTALL_FROM_RELEASE" != "1" ] && [ ! -d "$AGENT_SRC/src" ]; then
    error "Cannot find agent source at $AGENT_SRC"
    exit 1
fi

echo ""
echo -e "${GREEN}${BOLD}DockPanel Updater${NC}"
echo ""

# ── Sync repo to origin/main ──────────────────────────────────────────────
# Both modes need a fresh tree: the canonical systemd unit
# (panel/agent/dockpanel-agent.service), nginx templates, install-agent.sh,
# and a few other repo-resident files are deployed from $REPO_DIR. Without a
# pull, `bash /opt/dockpanel/scripts/update.sh` would download new binaries
# but redeploy the OLD canonical unit — exactly what stranded the v2.8.13 →
# v2.8.14 upgrade-path test (RuntimeDirectory=dockpanel and /var/cache/nginx
# in ReadWritePaths never reached the deployed unit). v2.8.15.
#
# `git pull --ff-only` doesn't cover installs cloned with `-b vX.Y.Z` (those
# end up on a detached HEAD with no `main` known locally), so the sync uses
# `git fetch origin main` + `git reset --hard FETCH_HEAD` to forcibly track
# main. Local edits to /opt/dockpanel are unsupported (it's a deploy
# artifact, not a working tree) — `git stash` captures any incidental drift
# in case anyone wants to inspect it post-upgrade.
if [ -d "$REPO_DIR/.git" ]; then
    log "Syncing repo to latest origin/main..."
    (cd "$REPO_DIR" && {
        git stash -q 2>/dev/null || true
        if git fetch --depth=1 origin main 2>/dev/null; then
            git reset --hard FETCH_HEAD 2>&1 | tail -1 >/dev/null || true
        else
            log "Warning: git fetch failed — deploying from existing on-disk source"
        fi
    }) || log "Warning: repo sync failed — deploying from existing on-disk source"
fi

# ── Backup database before upgrade ────────────────────────────────────────
BACKUP_DIR="/var/backups/dockpanel/db"
mkdir -p "$BACKUP_DIR"
log "Backing up database..."
if docker exec dockpanel-postgres pg_dump -U dockpanel dockpanel | gzip > "$BACKUP_DIR/pre-upgrade-$(date +%Y%m%d%H%M%S).sql.gz"; then
    log "Database backup saved to $BACKUP_DIR/"
else
    error "Database backup failed, aborting upgrade"
    exit 1
fi

# ── Build or download binaries ────────────────────────────────────────────
if [ "$INSTALL_FROM_RELEASE" = "1" ]; then
    # Download pre-built binaries from GitHub Releases
    ARCH=$(uname -m)
    case "$ARCH" in
        x86_64)  DL_ARCH="amd64" ;;
        aarch64) DL_ARCH="arm64" ;;
        *) error "Unsupported architecture: $ARCH"; exit 1 ;;
    esac

    # DOCKPANEL_VERSION=vX.Y.Z (or vX.Y.Z-rc.N) — pin to a specific release
    # tag instead of /releases/latest. Lets the panel-internal orchestrator
    # honour the operator's candidate-channel pick and the in-process
    # "available version" snapshot, instead of racing /releases/latest mid-
    # apply (which could shift to a newer GA between poll and click).
    if [ -n "${DOCKPANEL_VERSION:-}" ]; then
        RELEASE_TAG="$DOCKPANEL_VERSION"
        log "Pinned release: $RELEASE_TAG (DOCKPANEL_VERSION)"
    else
        log "Fetching latest release..."
        RELEASE_TAG=$(curl -sf "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
        if [ -z "$RELEASE_TAG" ]; then
            error "Could not determine latest release. Check https://github.com/${GITHUB_REPO}/releases"
            exit 1
        fi
        log "Latest release: $RELEASE_TAG"
    fi
    BASE_URL="https://github.com/${GITHUB_REPO}/releases/download/${RELEASE_TAG}"

    log "Downloading agent (${DL_ARCH})..."
    curl -sfL "${BASE_URL}/dockpanel-agent-linux-${DL_ARCH}" -o /tmp/dockpanel-agent-new
    chmod +x /tmp/dockpanel-agent-new

    log "Downloading API (${DL_ARCH})..."
    curl -sfL "${BASE_URL}/dockpanel-api-linux-${DL_ARCH}" -o /tmp/dockpanel-api-new
    chmod +x /tmp/dockpanel-api-new

    log "Downloading CLI (${DL_ARCH})..."
    curl -sfL "${BASE_URL}/dockpanel-cli-linux-${DL_ARCH}" -o /tmp/dockpanel-cli-new
    chmod +x /tmp/dockpanel-cli-new

    # Download and extract frontend
    log "Downloading frontend..."
    curl -sfL "${BASE_URL}/dockpanel-frontend.tar.gz" -o /tmp/dockpanel-frontend.tar.gz
    FE_DIR="/opt/dockpanel/frontend"
    mkdir -p "$FE_DIR"
    tar xzf /tmp/dockpanel-frontend.tar.gz -C "$FE_DIR"
    rm -f /tmp/dockpanel-frontend.tar.gz
    log "Frontend updated"
else
    # Build from source
    # Detect Rust toolchain
    if command -v cargo &> /dev/null; then
        CARGO_CMD="cargo"
    elif [ -f "$HOME/.cargo/bin/cargo" ]; then
        CARGO_CMD="$HOME/.cargo/bin/cargo"
    else
        error "Rust toolchain not found, but BUILD_FROM_SOURCE=1 was requested."
        error "Recommended: drop BUILD_FROM_SOURCE=1 — update.sh will auto-fetch pre-built binaries."
        error "If you really want to compile from source: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        error "(note: building from source needs ~4 GB RAM — most production VPSes won't have it)"
        exit 1
    fi

    log "Building agent..."
    (cd "$AGENT_SRC" && $CARGO_CMD build --release 2>&1 | tail -1)

    log "Building API..."
    (cd "$API_SRC" && $CARGO_CMD build --release 2>&1 | tail -1)

    log "Building CLI..."
    (cd "$CLI_SRC" && $CARGO_CMD build --release 2>&1 | tail -1)

    if [ -d "$FRONTEND_DIR" ]; then
        log "Building frontend..."
        (cd "$FRONTEND_DIR" && npm ci --silent 2>/dev/null || npm install --silent 2>/dev/null)
        (cd "$FRONTEND_DIR" && npx vite build 2>&1 | tail -3)
        log "Frontend rebuilt"
    fi
fi

# ── Ensure required directories exist (may be new in this version) ────────
log "Ensuring required directories exist..."
mkdir -p /etc/dockpanel/ssl /var/run/dockpanel /var/backups/dockpanel
mkdir -p /var/www/acme/.well-known/acme-challenge
mkdir -p /var/lib/dockpanel/git
# Directories needed by agent ReadWritePaths (created only if missing).
# v2.8.13 expanded the RWP list — systemd fails the namespace mount on
# missing entries, so pre-create everything the canonical unit references.
for d in /etc/postfix /etc/dovecot /var/vmail /var/spool/postfix /run/opendkim /var/lib/nginx \
         /etc/cloudflared /etc/modsecurity /etc/fail2ban /etc/powerdns /etc/letsencrypt \
         /var/cache/nginx/fastcgi \
         /etc/ufw /var/lib/ufw; do
    [ -d "$d" ] || mkdir -p "$d" 2>/dev/null || true
done
echo "d /run/dockpanel 0755 root root -" > /etc/tmpfiles.d/dockpanel.conf 2>/dev/null || true

# v2.8.17: drop apt lock-wait config so agent's apt-get install/update/purge
# waits up to 5 min for the dpkg lock instead of failing immediately when
# unattended-upgrades is running in the background (common on fresh Debian).
# Idempotent — overwrites on every update.sh run, no apt operation needed.
if command -v apt-get &> /dev/null; then
    mkdir -p /etc/apt/apt.conf.d
    cat > /etc/apt/apt.conf.d/99-dockpanel-lock-wait.conf << 'APT_EOF'
DPkg::Lock::Timeout "300";
APT_EOF

    # v2.8.19: pre-v2.8.19 cloudflared installs wrote a literal
    # `$(lsb_release -cs)` into /etc/apt/sources.list.d/cloudflared.list
    # (single-quoted bash didn't expand the substitution). Once landed, the
    # broken file made every subsequent `apt-get update` on the box fail —
    # blocking unrelated installs (Redis, WAF, …). Repair it here so
    # operators upgrading via update.sh get an unblocked apt without manual
    # intervention.
    if [ -f /etc/apt/sources.list.d/cloudflared.list ] && \
       grep -qF '$(lsb_release' /etc/apt/sources.list.d/cloudflared.list; then
        log "Removing broken cloudflared apt source (literal \$(lsb_release ...) — fixed in v2.8.19)"
        rm -f /etc/apt/sources.list.d/cloudflared.list
    fi
fi

# ── Refresh systemd service files (may have changed between versions) ─────
log "Updating systemd service files..."
# Agent unit — deploy from repo (single source of truth: panel/agent/dockpanel-agent.service)
# v2.8.13: existing installs upgrading from v2.8.12 or earlier get the strict sandbox here.
cp "$AGENT_SRC/dockpanel-agent.service" /etc/systemd/system/dockpanel-agent.service
chmod 644 /etc/systemd/system/dockpanel-agent.service

cat > /etc/systemd/system/dockpanel-api.service << 'EOF'
[Unit]
Description=DockPanel API
After=network.target docker.service dockpanel-agent.service
Wants=dockpanel-agent.service
StartLimitBurst=5
StartLimitIntervalSec=60

[Service]
Type=simple
ExecStart=/usr/local/bin/dockpanel-api
Restart=always
RestartSec=5
Environment=RUST_LOG=info
EnvironmentFile=/etc/dockpanel/api.env
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectKernelLogs=yes
ProtectKernelModules=yes
ProtectSystem=no
ReadWritePaths=/var/run/dockpanel /tmp
MemoryMax=1G
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
EOF

# ── Update nginx frontend path if needed ──────────────────────────────────
if [ "$INSTALL_FROM_RELEASE" = "1" ]; then
    FE_DIST="/opt/dockpanel/frontend/dist"
    for conf in /etc/nginx/sites-enabled/dockpanel-panel.conf /etc/nginx/conf.d/dockpanel-panel.conf; do
        if [ -f "$conf" ] && grep -q "panel/frontend/dist" "$conf" 2>/dev/null; then
            sed -i "s|/opt/dockpanel/panel/frontend/dist|${FE_DIST}|g" "$conf"
            log "Updated nginx frontend path in $conf"
            nginx -t > /dev/null 2>&1 && nginx -s reload > /dev/null 2>&1
        fi
    done
else
    FE_DIST="${REPO_DIR}/panel/frontend/dist"
fi

# ── Drop install-agent.sh into FE_ROOT (#56, v2.8.14) ─────────────────────
# Panel SPA-fallback nginx serves $uri before falling back to index.html.
# Without the script present in FE_ROOT, `curl {panel}/install-agent.sh | bash`
# returns the SPA HTML and fails with 'syntax error near unexpected token'.
if [ -f "${REPO_DIR}/scripts/install-agent.sh" ] && [ -d "$FE_DIST" ]; then
    cp "${REPO_DIR}/scripts/install-agent.sh" "$FE_DIST/install-agent.sh"
    chmod 644 "$FE_DIST/install-agent.sh"
    log "Refreshed install-agent.sh in $FE_DIST"
fi

# ── Migrate panel nginx config to bind IPv6 (fixes site-vhost dual-stack hijack) ──
# v2.8.3: agent site templates declare `listen [::]:443 ssl;` (dual-stack), but
# pre-v2.8.3 setup.sh bound the panel to IPv4 only. The first SSL site to be
# provisioned then became the de-facto default for IPv6 traffic — WordPress
# would 301 panel-domain requests to its canonical home_url. Fix: ensure the
# panel vhost also has `listen [::]:80;` and `listen [::]:443 ssl;`. Plain
# (no `ipv6only=on`) so the panel listens dual-stack on the same socket as
# every site, and nginx routes by server_name across the shared listener.
# Strip any `ipv6only=on` certbot may have added on prior installs to keep
# the listener options consistent across vhosts (otherwise nginx errors with
# "duplicate listen options" when sites add their own [::]:443 ssl block).
NGINX_NEEDS_RELOAD=0
for conf in /etc/nginx/sites-enabled/dockpanel-panel.conf /etc/nginx/conf.d/dockpanel-panel.conf; do
    [ -f "$conf" ] || continue
    if ! grep -qE 'listen \[::\]:80' "$conf"; then
        sed -i -E '0,/^([[:space:]]*)listen ([^;]*):80;[[:space:]]*$/{s||\1listen \2:80;\n\1listen [::]:80;|}' "$conf"
        sed -i -E '0,/^([[:space:]]*)listen 80;[[:space:]]*$/{s||\1listen 80;\n\1listen [::]:80;|}' "$conf"
        log "Added IPv6 :80 listen to $conf"
        NGINX_NEEDS_RELOAD=1
    fi
    if grep -qE 'listen [^;]*:443 ssl' "$conf" && ! grep -qE 'listen \[::\]:443' "$conf"; then
        sed -i -E '0,/^([[:space:]]*)listen ([^;]*):443 ssl;[[:space:]]*$/{s||\1listen \2:443 ssl;\n\1listen [::]:443 ssl;|}' "$conf"
        log "Added IPv6 :443 ssl listen to $conf"
        NGINX_NEEDS_RELOAD=1
    fi
    # Strip `ipv6only=on` from panel listens if previously added — site vhosts
    # use plain `[::]:443 ssl;` and nginx rejects mixing the two on a shared socket.
    if grep -qE 'listen \[::\]:(80|443 ssl) ipv6only=on' "$conf"; then
        sed -i -E 's|^([[:space:]]*)listen \[::\]:80 ipv6only=on;|\1listen [::]:80;|' "$conf"
        sed -i -E 's|^([[:space:]]*)listen \[::\]:443 ssl ipv6only=on;|\1listen [::]:443 ssl;|' "$conf"
        log "Stripped ipv6only=on from $conf for shared-socket compatibility"
        NGINX_NEEDS_RELOAD=1
    fi
done
# Strip `ipv6only=on` from site vhosts left over from v2.8.3.
# v2.8.3 baked the option into agent templates AND added it via update.sh;
# v2.8.4 reverted the template but dropped this site-vhost cleanup. Result:
# v2.8.3-installed sites kept `[::]:443 ssl ipv6only=on` while the panel vhost
# (cleaned by the loop above) used plain `[::]:443 ssl` — nginx rejects the
# mix as "duplicate listen options" on the shared socket. Bringing them back
# in line restores reload-ability without touching site config in any other way.
if [ -d /etc/nginx/sites-enabled ]; then
    for site_conf in /etc/nginx/sites-enabled/*.conf; do
        [ -f "$site_conf" ] || continue
        case "$(basename "$site_conf")" in
            dockpanel-panel.conf) continue ;;
        esac
        if grep -qE 'listen \[::\]:(80|443 ssl) ipv6only=on' "$site_conf"; then
            sed -i -E 's|^([[:space:]]*)listen \[::\]:80 ipv6only=on;|\1listen [::]:80;|' "$site_conf"
            sed -i -E 's|^([[:space:]]*)listen \[::\]:443 ssl ipv6only=on;|\1listen [::]:443 ssl;|' "$site_conf"
            log "Stripped ipv6only=on from $site_conf for shared-socket compatibility"
            NGINX_NEEDS_RELOAD=1
        fi
    done
fi
# ── v2.8.22: ensure panel vhost includes dockpanel-panel.locations/*.conf ─
# Drop-in dir for path-mounted tool reverse-proxies. Webmail uses this in
# v2.8.22+ (writes webmail.conf on install, deletes on remove). Pre-v2.8.22
# panel vhosts lack the include directive — inject it just before the
# server-block closing brace via awk (sed nested-brace handling is fragile).
mkdir -p /etc/nginx/conf.d/dockpanel-panel.locations
for conf in /etc/nginx/sites-enabled/dockpanel-panel.conf /etc/nginx/conf.d/dockpanel-panel.conf; do
    [ -f "$conf" ] || continue
    if ! grep -q "dockpanel-panel.locations" "$conf"; then
        # Inject before the FIRST top-level `}` (the server block close).
        # Panel vhost has exactly one top-level `}`, so single-match is safe.
        if awk '
            /^}/ && !done {
                print "    # Drop-in location blocks for path-mounted tools (webmail, etc.)";
                print "    include /etc/nginx/conf.d/dockpanel-panel.locations/*.conf;";
                print "";
                done=1
            }
            { print }
        ' "$conf" > "$conf.new" && [ -s "$conf.new" ]; then
            mv "$conf.new" "$conf"
            log "Added dockpanel-panel.locations include to $conf"
            NGINX_NEEDS_RELOAD=1
        else
            rm -f "$conf.new"
            log "WARN: failed to inject panel-locations include into $conf — skipped"
        fi
    fi
done

if [ "$NGINX_NEEDS_RELOAD" = "1" ]; then
    if nginx -t > /dev/null 2>&1; then
        nginx -s reload > /dev/null 2>&1 && log "Nginx reloaded after IPv6 listen + panel-locations migration"
    else
        log "WARN: nginx -t failed after IPv6 listen + panel-locations migration; not reloading. Check sites-enabled/."
    fi
fi

# ── v2.10.1: heal pre-v2.10.1 webmail nginx fragment ──────────────
# v2.8.22 → v2.10.0 wrote a fragment without sub_filter — Roundcube's
# root-anchored "/?_task=..." URLs leaked through to the panel SPA,
# so Open landed on the dashboard and login form posted to /. Detect
# the old shape (no sub_filter) and regenerate with the current
# template; the agent only writes on Install click, so without this
# migration users would have to Remove + Install Webmail to recover.
WEBMAIL_FRAG="/etc/nginx/conf.d/dockpanel-panel.locations/webmail.conf"
if [ -f "$WEBMAIL_FRAG" ] && ! grep -q "sub_filter" "$WEBMAIL_FRAG"; then
    RCUBE_PORT=$(docker inspect --format '{{range $p, $b := .NetworkSettings.Ports}}{{range $b}}{{.HostPort}}{{end}}{{end}}' dockpanel-roundcube 2>/dev/null | head -1)
    [ -z "$RCUBE_PORT" ] && RCUBE_PORT=8888
    cat > "$WEBMAIL_FRAG" <<EOF
# DockPanel webmail (Roundcube) reverse-proxy — managed by agent, do not edit
# v2.10.1: sub_filter rewrites Roundcube's root-anchored URLs (form action,
# comm_path) under /webmail/ so navigation doesn't land on the panel root.
location /webmail/ {
    proxy_pass http://127.0.0.1:${RCUBE_PORT}/;
    proxy_http_version 1.1;
    proxy_set_header Host \$host;
    proxy_set_header X-Real-IP \$remote_addr;
    proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto \$scheme;
    proxy_set_header X-Forwarded-Host \$host;
    proxy_set_header Accept-Encoding "";
    proxy_redirect / /webmail/;
    sub_filter '"/?_task=' '"/webmail/?_task=';
    sub_filter_once off;
    sub_filter_types text/html application/json application/javascript text/javascript;
    proxy_read_timeout 300s;
    client_max_body_size 25M;
}
EOF
    if nginx -t > /dev/null 2>&1; then
        nginx -s reload > /dev/null 2>&1 && log "Healed webmail nginx fragment to v2.10.1 shape (port ${RCUBE_PORT})"
    else
        log "WARN: webmail fragment heal failed nginx -t; reverting"
        rm -f "$WEBMAIL_FRAG"
    fi
fi

# Ensure BASE_URL is set in api.env for CORS
if [ -f /etc/dockpanel/api.env ] && ! grep -q "BASE_URL" /etc/dockpanel/api.env; then
    # Detect panel URL from nginx config
    PANEL_DOMAIN=""
    for conf in /etc/nginx/sites-enabled/dockpanel-panel.conf /etc/nginx/conf.d/dockpanel-panel.conf; do
        if [ -f "$conf" ]; then
            PANEL_DOMAIN=$(grep "server_name" "$conf" | head -1 | awk '{print $2}' | tr -d ';')
            break
        fi
    done
    if [ -n "$PANEL_DOMAIN" ] && [ "$PANEL_DOMAIN" != "_" ]; then
        echo "BASE_URL=https://${PANEL_DOMAIN}" >> /etc/dockpanel/api.env
        log "Added BASE_URL=https://${PANEL_DOMAIN} to api.env"
    fi
fi

# ── Deploy binaries ───────────────────────────────────────────────────────
# Note: ~2-5s downtime during binary swap is expected for self-hosted deployments.
log "Backing up current binaries..."
cp "$AGENT_BIN" "${AGENT_BIN}.bak" 2>/dev/null || true
cp "$API_BIN" "${API_BIN}.bak" 2>/dev/null || true
cp "$CLI_BIN" "${CLI_BIN}.bak" 2>/dev/null || true

log "Stopping services..."
systemctl stop dockpanel-agent dockpanel-api 2>/dev/null || true

if [ "$INSTALL_FROM_RELEASE" = "1" ]; then
    mv /tmp/dockpanel-agent-new "$AGENT_BIN"
    mv /tmp/dockpanel-api-new "$API_BIN"
    mv /tmp/dockpanel-cli-new "$CLI_BIN"
else
    cp "$AGENT_SRC/target/release/dockpanel-agent" "$AGENT_BIN"
    cp "$API_SRC/target/release/dockpanel-api" "$API_BIN"
    cp "$CLI_SRC/target/release/dockpanel" "$CLI_BIN"
fi
chmod +x "$AGENT_BIN" "$API_BIN" "$CLI_BIN"
log "Binaries updated (agent: $(du -h "$AGENT_BIN" | cut -f1), api: $(du -h "$API_BIN" | cut -f1), cli: $(du -h "$CLI_BIN" | cut -f1))"

systemctl daemon-reload
systemctl start dockpanel-agent
sleep 1
systemctl start dockpanel-api
log "Services restarted"

# ── Health check with rollback ────────────────────────────────────────────
rollback() {
    error "Health check failed, rolling back..."
    cp "${AGENT_BIN}.bak" "$AGENT_BIN" 2>/dev/null || true
    cp "${API_BIN}.bak" "$API_BIN" 2>/dev/null || true
    cp "${CLI_BIN}.bak" "$CLI_BIN" 2>/dev/null || true
    systemctl daemon-reload
    systemctl restart dockpanel-agent dockpanel-api
    warn "Rolled back to previous binaries"
    exit 1
}

log "Running post-deploy health check..."
sleep 20

# Basic health endpoint
if ! curl -sf --max-time 30 http://127.0.0.1:3080/api/health > /dev/null 2>&1; then
    rollback
fi
log "Health check: /api/health OK"

# Auth subsystem (setup-status is unauthenticated, tests DB connectivity).
# Note: this endpoint is GET-only — using POST returns 405 and triggered an
# unconditional rollback on every update before this fix.
if ! curl -sf --max-time 30 http://127.0.0.1:3080/api/auth/setup-status > /dev/null 2>&1; then
    rollback
fi
log "Health check: /api/auth/setup-status OK"

# Agent reachable (non-fatal — agent may start slower)
if ! curl -sf --max-time 30 http://127.0.0.1:3080/api/system/info > /dev/null 2>&1; then
    warn "Agent connectivity check failed (non-fatal, agent may still be starting)"
fi

# CLI health check (non-fatal)
if ! dockpanel --version > /dev/null 2>&1; then
    warn "CLI health check failed (non-fatal)"
fi

log "Health checks passed"
# Clean up backups
rm -f "${AGENT_BIN}.bak" "${API_BIN}.bak" "${CLI_BIN}.bak"

echo ""
echo -e "${GREEN}${BOLD}Update complete!${NC}"
echo ""
echo -e "  Agent: $(systemctl is-active dockpanel-agent)"
echo -e "  API:   $(systemctl is-active dockpanel-api)"
echo -e "  Version: $($CLI_BIN --version 2>/dev/null || echo 'unknown')"
echo ""
