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
cargo build --bin mgd --bin mgctl --bin mgd-zram-reclaim --release 2>&1 | tail -3
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

    # CRIU (Option A) — narrow caps instead of root, if criu is present.
    # Resolve criu from the SAME fixed locations the daemon probes (it never does
    # a PATH search, since the binary is capped), so we cap exactly the binary mgd
    # will run. Falls back to `command -v` only as a last resort.
    criu_bin=""
    for c in /usr/sbin/criu /usr/bin/criu /sbin/criu /bin/criu \
             /usr/local/sbin/criu /usr/local/bin/criu; do
        if [[ -x "$c" ]]; then criu_bin="$c"; break; fi
    done
    [[ -z "$criu_bin" ]] && criu_bin="$(command -v criu 2>/dev/null || true)"

    if [[ -n "$criu_bin" ]]; then
        if sudo setcap cap_checkpoint_restore,cap_sys_ptrace+ep "$criu_bin"; then
            ok "criu capped (cap_checkpoint_restore,cap_sys_ptrace) at $criu_bin — no root needed"
            warn "  for live TCP restore (browsers), re-cap with cap_net_admin added:"
            warn "    sudo setcap cap_checkpoint_restore,cap_sys_ptrace,cap_net_admin+ep $criu_bin"
            warn "  a criu package UPGRADE resets these caps — re-run install.sh --privileged afterwards"
        else
            warn "could not setcap criu (kernel may lack CAP_CHECKPOINT_RESTORE) — CRIU falls back to kill"
        fi
    else
        warn "criu not found — checkpoint/restore disabled (mgd will SIGKILL instead)"
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
