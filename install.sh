#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
SERVICE_DIR="$HOME/.config/systemd/user"
CONFIG_DIR="$HOME/.config/mgd"
SERVICE_NAME="mgd.service"

# Opt-in privileged features (group + zram-compact sysfs grant + capped swap
# reclaim helper). Off by default — the daemon runs fully unprivileged without
# them. Enable with: ./install.sh --privileged
WITH_PRIVILEGED=0
HELPER_DEST="/usr/local/bin/mgd-zram-reclaim"
for arg in "$@"; do
    case "$arg" in
        --privileged) WITH_PRIVILEGED=1 ;;
        -h|--help)
            echo "Usage: $0 [--privileged]"
            echo "  --privileged   also install opt-in privileged features (needs sudo):"
            echo "                 mgd group, zram-compact sysfs grant, capped swap-reclaim helper."
            echo "                 See docs/PRIVILEGE_DESIGN.md."
            exit 0 ;;
        *) echo "unknown argument: $arg (try --help)" >&2; exit 1 ;;
    esac
done

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

if [[ "$WITH_PRIVILEGED" == 1 ]]; then
    command -v setcap >/dev/null 2>&1 || die "setcap not found (needed for --privileged) — install libcap (Fedora: libcap; Debian: libcap2-bin)"
    command -v sudo   >/dev/null 2>&1 || die "sudo not found (needed for --privileged)"
fi

ok "Dependencies OK"

# ── build ─────────────────────────────────────────────────────────────────────
echo "Building release binaries..."
cargo build --bin mgd --bin mgctl --bin mgd-zram-reclaim --bin mgd-checkpoint --bin mgd-psi-trigger --bin mgd-kde --bin mgd-gpu-intel --bin mgd-gpu-amd --release 2>&1 | tail -3
ok "Build complete"

# ── stop existing service if running ─────────────────────────────────────────
if systemctl --user is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    echo "Stopping existing service..."
    systemctl --user stop "$SERVICE_NAME"
fi

# ── install binary ────────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
if [[ "$WITH_PRIVILEGED" == 1 ]]; then
    DAEMON_DEST="/usr/local/bin/mgd"
    sudo install -m 0755 target/release/mgd "$DAEMON_DEST"
    if sudo setcap cap_sys_nice+ep "$DAEMON_DEST"; then
        ok "Daemon binary installed + capped with CAP_SYS_NICE at $DAEMON_DEST"
    else
        warn "Could not setcap CAP_SYS_NICE on daemon binary"
    fi
    # Remove local unprivileged binary to avoid path confusion
    rm -f "$BIN_DIR/mgd"
else
    cp target/release/mgd   "$BIN_DIR/mgd"
    if [[ -f "/usr/local/bin/mgd" ]]; then
        sudo rm -f "/usr/local/bin/mgd"
    fi
fi
cp target/release/mgctl "$BIN_DIR/mgctl"
cp target/release/mgd-checkpoint "$BIN_DIR/mgd-checkpoint"
cp target/release/mgd-psi-trigger "$BIN_DIR/mgd-psi-trigger"
cp target/release/mgd-kde "$BIN_DIR/mgd-kde"
cp target/release/mgd-gpu-intel "$BIN_DIR/mgd-gpu-intel"
cp target/release/mgd-gpu-amd "$BIN_DIR/mgd-gpu-amd"
chmod +x "$BIN_DIR"/mgd*
ok "Binaries installed"

# ── install service ───────────────────────────────────────────────────────────
mkdir -p "$SERVICE_DIR"
cp config/mgd.service "$SERVICE_DIR/$SERVICE_NAME"
if [[ "$WITH_PRIVILEGED" == 1 ]]; then
    sed -i 's|ExecStart=.*|ExecStart=/usr/local/bin/mgd\nCPUWeight=10000|g' "$SERVICE_DIR/$SERVICE_NAME"
    ok "Service file updated for CPU weight"
fi
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

# ── optional privileged features (opt-in) ────────────────────────────────────
# Each grant is independent; skipping one disables only that feature. The daemon
# detects a missing grant at runtime, logs once, and continues unprivileged.
# See docs/PRIVILEGE_DESIGN.md for the full rationale.
if [[ "$WITH_PRIVILEGED" == 1 ]]; then
    echo
    echo "Installing opt-in privileged features (sudo required)..."

    # mgd group — gates who may use the capped helper / sysfs grant.
    sudo groupadd -f mgd
    if ! id -nG "$USER" | tr ' ' '\n' | grep -qx mgd; then
        sudo usermod -aG mgd "$USER"
        warn "Added $USER to the 'mgd' group — log out and back in for it to take effect"
    fi
    ok "mgd group ready"

    # Fix 1 — zram compact: sysfs group-write grant (no capability, no binary).
    sudo install -m 0644 packaging/mgd-zram.conf /etc/tmpfiles.d/mgd-zram.conf
    if sudo systemd-tmpfiles --create /etc/tmpfiles.d/mgd-zram.conf 2>/dev/null; then
        ok "zram compact grant installed (/sys/block/zram0/compact group-writable)"
    else
        warn "tmpfiles grant installed but --create failed (no zram0 yet?) — applies on next boot"
    fi

    # Fix 2 — swap reclaim: capped helper (CAP_SYS_ADMIN, never SUID root).
    # The daemon looks for the helper in /usr/local/bin then /usr/bin; install.sh
    # uses /usr/local/bin (manual install convention). Distro packages use /usr/bin.
    sudo install -m 0750 -o root -g mgd target/release/mgd-zram-reclaim "$HELPER_DEST"
    sudo setcap cap_sys_admin+ep "$HELPER_DEST"
    ok "swap reclaim helper installed + capped at $HELPER_DEST"
    warn "  reclaim stays OFF until you set [reclaim] proactive_swap_reclaim = true in priorities.toml"

    # Fix 3 — checkpoint helper: capped helper (CAP_CHECKPOINT_RESTORE, CAP_SYS_PTRACE, CAP_NET_ADMIN, never SUID root).
    CHECKPOINT_HELPER_DEST="/usr/local/bin/mgd-checkpoint"
    sudo install -m 0750 -o root -g mgd target/release/mgd-checkpoint "$CHECKPOINT_HELPER_DEST"
    if sudo setcap cap_checkpoint_restore,cap_sys_ptrace,cap_net_admin+ep "$CHECKPOINT_HELPER_DEST"; then
        ok "checkpoint helper installed + capped at $CHECKPOINT_HELPER_DEST"
    else
        warn "could not setcap checkpoint helper (kernel may lack CAP_CHECKPOINT_RESTORE)"
    fi

    # Fix 4 — PSI trigger helper: cap_perfmon+ep (Linux 6.0+ requires it for
    # /proc/pressure/* writes). The daemon spawns this, polls its stdout for
    # pressure events, and stays fully unprivileged itself.
    PSI_TRIGGER_DEST="/usr/local/bin/mgd-psi-trigger"
    sudo install -m 0755 target/release/mgd-psi-trigger "$PSI_TRIGGER_DEST"
    if sudo setcap cap_perfmon+ep "$PSI_TRIGGER_DEST"; then
        ok "PSI trigger helper installed + capped at $PSI_TRIGGER_DEST"
    else
        warn "could not setcap PSI trigger helper — daemon falls back to 5s polling"
    fi
else
    warn "Privileged features skipped (default). To enable zram compact + swap reclaim:"
    warn "  ./install.sh --privileged    (or follow docs/PRIVILEGE_DESIGN.md manually)"
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
echo "To reload config live:    mgctl reload"
echo "To view status:           mgctl status        (daemon: pressure + frozen)"
echo "To view service state:    mgctl service       (systemd: active, PID, uptime)"
echo "To list frozen processes: mgctl list"
echo "To view logs:             mgctl logs -f"
echo "To restart / stop:        mgctl restart  |  mgctl stop"
