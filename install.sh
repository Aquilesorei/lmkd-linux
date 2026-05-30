#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
SERVICE_DIR="$HOME/.config/systemd/user"
CONFIG_DIR="$HOME/.config/mgd"
SERVICE_NAME="mgd.service"

# ── colours ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
ok()   { echo -e "${GREEN}✓${NC} $*"; }
warn() { echo -e "${YELLOW}!${NC} $*"; }
die()  { echo -e "${RED}✗${NC} $*" >&2; exit 1; }

# ── dependency checks ─────────────────────────────────────────────────────────
echo "Checking dependencies..."

command -v cargo   >/dev/null 2>&1 || die "cargo not found — install Rust: https://rustup.rs"
command -v systemctl >/dev/null 2>&1 || die "systemctl not found — systemd required"

if ! command -v criu >/dev/null 2>&1; then
    warn "criu not found — checkpoint/restore will fall back to kill"
    warn "  Fedora:  sudo dnf install criu"
    warn "  Debian:  sudo apt install criu"
fi

ok "Dependencies OK"

# ── build ─────────────────────────────────────────────────────────────────────
echo "Building release binaries..."
cargo build --bin mgd --bin mgctl --release 2>&1 | tail -3
ok "Build complete"

# ── stop existing service if running ─────────────────────────────────────────
if systemctl --user is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    echo "Stopping existing service..."
    systemctl --user stop "$SERVICE_NAME"
fi

# ── install binary ────────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
cp target/release/mgd   "$BIN_DIR/mgd"
cp target/release/mgctl "$BIN_DIR/mgctl"
chmod +x "$BIN_DIR/mgd" "$BIN_DIR/mgctl"
ok "Binaries installed to $BIN_DIR/{mgd,mgctl}"

# ── install service ───────────────────────────────────────────────────────────
mkdir -p "$SERVICE_DIR"
cp config/mgd.service "$SERVICE_DIR/$SERVICE_NAME"
systemctl --user daemon-reload
ok "Service file installed to $SERVICE_DIR/$SERVICE_NAME"

# ── install default config (don't overwrite user edits) ──────────────────────
mkdir -p "$CONFIG_DIR"
if [[ -f "$CONFIG_DIR/priorities.toml" ]]; then
    warn "Config already exists at $CONFIG_DIR/priorities.toml — not overwriting"
    warn "  To reset to defaults: cp config/priorities.toml $CONFIG_DIR/priorities.toml"
else
    cp config/priorities.toml "$CONFIG_DIR/priorities.toml"
    ok "Default config installed to $CONFIG_DIR/priorities.toml"
fi

# ── enable + (re)start ───────────────────────────────────────────────────────
systemctl --user enable "$SERVICE_NAME"
if systemctl --user is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    # Already running — restart to pick up new binary
    systemctl --user restart "$SERVICE_NAME"
    ok "Service restarted"
else
    systemctl --user start "$SERVICE_NAME"
    ok "Service started"
fi

# ── verify ────────────────────────────────────────────────────────────────────
sleep 1
if systemctl --user is-active --quiet "$SERVICE_NAME"; then
    ok "mgd is running"
    echo
    if command -v mgctl >/dev/null 2>&1 || [[ -x "$BIN_DIR/mgctl" ]]; then
        "$BIN_DIR/mgctl" status || true
    else
        systemctl --user status "$SERVICE_NAME" --no-pager -l | head -10 || true
    fi
else
    die "Service failed to start — check: journalctl --user -u mgd.service -n 30"
fi

echo
echo "To customize priorities:  $CONFIG_DIR/priorities.toml"
echo "To reload config live:    mgctl reload     (or: kill -HUP \$(pgrep mgd))"
echo "To view status:           mgctl status"
echo "To list frozen processes: mgctl list"
echo "To view logs:             journalctl --user -u mgd.service -f"
echo "To stop:                  systemctl --user stop mgd.service"
