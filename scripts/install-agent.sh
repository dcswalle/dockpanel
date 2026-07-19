#!/usr/bin/env bash
# DockPanel Remote Agent Installer
# Usage: curl -sSL https://panel.example.com/install-agent.sh | sudo bash -s -- \
#   --panel-url https://panel.example.com \
#   --token <agent_token> \
#   --server-id <server_uuid>
#
# This installs ONLY the DockPanel agent binary (no database, no API, no frontend).
# The agent connects back to the panel via HTTPS on port 9443.

set -euo pipefail

PANEL_URL=""
TOKEN=""
SERVER_ID=""
AGENT_PORT="9443"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --panel-url) PANEL_URL="$2"; shift 2 ;;
        --token) TOKEN="$2"; shift 2 ;;
        --server-id) SERVER_ID="$2"; shift 2 ;;
        --port) AGENT_PORT="$2"; shift 2 ;;
        *) echo "Unknown argument: $1"; exit 1 ;;
    esac
done

if [[ -z "$TOKEN" ]]; then
    echo "Error: --token is required"
    echo "Usage: $0 --panel-url <url> --token <token> --server-id <uuid>"
    exit 1
fi

# An agent with no central URL never phones home, so the panel never records a
# `last_seen_at` for it — and the fleet rolling update only considers servers
# seen in the last 5 minutes. The box would install fine and then be invisible
# to every fleet operation, with nothing anywhere saying why. Fail loudly here
# instead. (The panel used to hand out a copy-paste command with an empty
# --panel-url whenever it was installed without a domain; that is fixed too.)
if [[ -z "$PANEL_URL" || -z "$SERVER_ID" ]]; then
    echo "Error: --panel-url and --server-id are required"
    echo "  Without them the agent cannot check in, and a server that never"
    echo "  checks in can never be selected by a fleet update."
    echo "Usage: $0 --panel-url <url> --token <token> --server-id <uuid>"
    exit 1
fi
if [[ "$PANEL_URL" == --* || "$TOKEN" == --* || "$SERVER_ID" == --* ]]; then
    echo "Error: an option value looks like another flag (--panel-url '$PANEL_URL',"
    echo "  --server-id '$SERVER_ID'). One of the values is probably missing."
    exit 1
fi

echo "======================================"
echo "  DockPanel Agent Installer (Remote)"
echo "======================================"
echo ""

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  ARCH_LABEL="amd64" ;;
    aarch64) ARCH_LABEL="arm64" ;;
    arm64)   ARCH_LABEL="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac
echo "[1/7] Architecture: $ARCH_LABEL"

# Detect package manager
detect_pkg_manager() {
    if command -v apt-get &> /dev/null; then
        PKG_MGR="apt"
    elif command -v dnf &> /dev/null; then
        PKG_MGR="dnf"
    elif command -v yum &> /dev/null; then
        PKG_MGR="yum"
    else
        echo "Error: No supported package manager found (apt/dnf/yum)"
        exit 1
    fi
}

pkg_install() {
    case "$PKG_MGR" in
        apt)
            apt-get update -qq > /dev/null 2>&1
            apt-get install -y -qq "$@" > /dev/null 2>&1
            ;;
        dnf) dnf install -y -q "$@" > /dev/null 2>&1 ;;
        yum) yum install -y -q "$@" > /dev/null 2>&1 ;;
    esac
}

# Install dependencies
echo "[2/7] Installing dependencies..."
detect_pkg_manager

# Install Docker
if ! command -v docker &> /dev/null; then
    curl -fsSL https://get.docker.com | sh > /dev/null 2>&1
fi
systemctl enable --now docker > /dev/null 2>&1 || true

# Install curl and openssl if missing
pkg_install curl openssl

# Create directories
echo "[3/7] Creating directories..."
mkdir -p /etc/dockpanel/ssl
mkdir -p /var/run/dockpanel
mkdir -p /var/www
mkdir -p /var/backups/dockpanel
mkdir -p /var/lib/dockpanel/git

# Ensure socket directory persists across reboots
echo "d /run/dockpanel 0755 root root -" > /etc/tmpfiles.d/dockpanel.conf

# Save agent token and server ID
echo "[4/7] Saving configuration..."
echo "$TOKEN" > /etc/dockpanel/agent.token
chmod 600 /etc/dockpanel/agent.token

# Persist agent configuration
# AGENT_TOKEN + AGENT_LISTEN_TCP = direct multi-server TCP access
# DOCKPANEL_* vars = phone-home mode (agent checks in with central panel)
cat > /etc/dockpanel/agent.env << ENVEOF
AGENT_TOKEN=$TOKEN
AGENT_LISTEN_TCP=0.0.0.0:$AGENT_PORT
DOCKPANEL_SERVER_TOKEN=$TOKEN
DOCKPANEL_SERVER_ID=$SERVER_ID
DOCKPANEL_CENTRAL_URL=$PANEL_URL
ENVEOF
chmod 600 /etc/dockpanel/agent.env

# Download agent binary (naming matches GitHub release assets)
echo "[5/7] Downloading agent binary..."
DOWNLOAD_URL="https://github.com/ovexro/dockpanel/releases/latest/download/dockpanel-agent-linux-${ARCH_LABEL}"
if ! curl -fsSL "$DOWNLOAD_URL" -o /usr/local/bin/dockpanel-agent; then
    echo "  Release download failed. Trying panel download..."
    if [[ -n "$PANEL_URL" ]]; then
        curl -fsSL "${PANEL_URL}/api/agent/download?arch=${ARCH_LABEL}" -o /usr/local/bin/dockpanel-agent || {
            echo "Error: Could not download agent binary"
            exit 1
        }
    else
        echo "Error: Could not download agent binary (no --panel-url provided)"
        exit 1
    fi
fi
chmod +x /usr/local/bin/dockpanel-agent

# Generate self-signed TLS cert for agent HTTPS
echo "[6/7] Generating TLS certificate..."
if [[ ! -f /etc/dockpanel/ssl/agent.crt ]]; then
    openssl req -x509 -newkey rsa:2048 -keyout /etc/dockpanel/ssl/agent.key \
        -out /etc/dockpanel/ssl/agent.crt -days 3650 -nodes \
        -subj "/CN=dockpanel-agent" > /dev/null 2>&1
    chmod 600 /etc/dockpanel/ssl/agent.key
fi

# Create systemd service (matching local agent hardening)
echo "[7/7] Creating systemd service..."
cat > /etc/systemd/system/dockpanel-agent.service << 'UNIT'
[Unit]
Description=DockPanel Agent
After=network.target docker.service
Wants=docker.service
StartLimitBurst=5
StartLimitIntervalSec=60

[Service]
Type=simple
ExecStartPre=/bin/sh -c 'mkdir -p /run/dockpanel /var/lib/dockpanel/git'
ExecStart=/usr/local/bin/dockpanel-agent
EnvironmentFile=/etc/dockpanel/agent.env
Environment=RUST_LOG=info
Restart=always
RestartSec=5
NoNewPrivileges=no
ProtectSystem=no
ProtectHome=no
PrivateTmp=no
ProtectKernelLogs=yes
ProtectKernelModules=yes
MemoryMax=512M
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
UNIT

# Allow agent port through firewall
if command -v ufw &> /dev/null; then
    ufw allow ${AGENT_PORT}/tcp > /dev/null 2>&1 || true
elif command -v firewall-cmd &> /dev/null; then
    firewall-cmd --permanent --add-port=${AGENT_PORT}/tcp > /dev/null 2>&1 || true
    firewall-cmd --reload > /dev/null 2>&1 || true
fi

# Start agent. "The command ran" is not the success condition — "the unit is
# active" is. install_powerdns ended in `let _ = systemctl restart` and hid three
# separate bugs behind that silence for two releases (lesson #45), so this
# installer polls and shows the failing journal line rather than printing a
# success banner over a crash loop. `activating` is tolerated because a unit
# with Restart=always can be caught mid-cycle by a single probe.
systemctl daemon-reload
systemctl enable dockpanel-agent >/dev/null 2>&1 || true
systemctl start dockpanel-agent || true

AGENT_OK=0
for _ in $(seq 1 15); do
    STATE=$(systemctl is-active dockpanel-agent 2>/dev/null || true)
    if [[ "$STATE" == "active" ]]; then AGENT_OK=1; break; fi
    [[ "$STATE" == "failed" ]] && break
    sleep 1
done

if [[ "$AGENT_OK" != "1" ]]; then
    echo ""
    echo "Error: dockpanel-agent did not start (systemctl is-active: $(systemctl is-active dockpanel-agent 2>/dev/null || echo unknown))"
    echo "Last journal lines:"
    journalctl -u dockpanel-agent -n 15 --no-pager 2>/dev/null || true
    exit 1
fi

echo ""
echo "======================================"
echo "  DockPanel Agent installed!"
echo "======================================"
echo ""
echo "  Agent listening on: 0.0.0.0:${AGENT_PORT}"
echo "  Token: ${TOKEN:0:12}..."
echo "  Server ID: ${SERVER_ID}"
echo "  Config: /etc/dockpanel/agent.env"
echo ""
echo "  Return to your DockPanel and click"
echo "  'Test Connection' to verify."
echo ""
