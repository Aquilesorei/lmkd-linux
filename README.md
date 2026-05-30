# lmkd-linux

Userspace memory pressure manager for Linux desktop.

The Linux kernel swaps processes out on memory spikes but never actively reclaims — pages sit in swap indefinitely even when RAM is available, and the OOM killer only fires once the system is already unresponsive. There is no priority system: the compositor and a background file indexer are treated identically.

`lmkd-linux` monitors PSI (Pressure Stall Information) and manages the reclaim cycle the kernel skips — freezing, checkpointing, or killing processes in priority order before stall time reaches the point of no return, then restoring them when pressure clears.

## How it works

Reads `/proc/pressure/memory` every 5 seconds. When stall time crosses a threshold, calculates the RAM deficit and works through processes from least to most important until enough is freed.

```
ELEVATED  (avg10 ≥ 5%)   →  SIGSTOP low-priority processes
HIGH      (avg10 ≥ 25%)  →  SIGSTOP + SIGTERM expendable processes
CRITICAL  (avg10 ≥ 50%)  →  CRIU checkpoint or kill
EMERGENCY (avg10 ≥ 70%)  →  SIGKILL anything non-critical
```

If `full_avg10 ≥ 20%` (all tasks stalled), the daemon jumps straight to Critical regardless of `some_avg10`.

Processes are never killed above their priority tier. The compositor and audio server are hardcoded untouchable. Frozen processes are tracked and automatically resumed when pressure drops.

## Stress test

Real memory pressure event on a 16GB system:

- **t+0s** — Pressure crossed Elevated. mgd froze 27 low-priority processes
  in one cycle (browser tabs, file indexers, notifier daemons, IDE helpers).
  Compositor, audio, and foreground IDE untouched.
- **t+15s** — Pressure escalated. CRIU checkpoint attempted on a runaway
  process, fell back to kill when CRIU failed.
- **t+30–40s** — New heavy processes spawned mid-incident (cargo, cef_server)
  were caught and frozen as they appeared.
- **t+51s** — Killed the actual memory hog (a runaway node process, 580 MB).
  System recovered.
- **t+71s** — Pressure dropped to Normal. All 30 frozen processes unfrozen
  in a single cycle. No orphans.

System stayed responsive throughout. No UI freeze, no compositor stutter, no reboot. Daemon RSS: ~6 MB.

## Priority tiers

| Range | Tier | Examples |
|-------|------|---------|
| 0–19 | CRITICAL | kwin_wayland, pipewire, plasmashell |
| 20–39 | HIGH | RustRover, WebStorm |
| 40–59 | NORMAL | Firefox, Claude CLI |
| 60–79 | LOW | plasma-discover, baloo |
| 80–100 | EXPENDABLE | msedge, browser tabs, AI inference |

## Installation

Requires CRIU for checkpoint/restore:
```bash
sudo dnf install criu      # Fedora
sudo apt install criu      # Debian/Ubuntu
```

```bash
git clone https://github.com/Aquilesorei/lmkd-linux
cd lmkd-linux
./install.sh
```

Run as a systemd user service:
```bash
cp config/mgd.service ~/.config/systemd/user/
systemctl --user enable --now mgd.service
```

## Usage

### Daemon
```bash
mgd          # run daemon (normally via systemd)
```

### mgctl — live control CLI
```bash
mgctl status              # current pressure level + frozen/checkpointed counts
mgctl list                # list all currently frozen processes
mgctl unfreeze firefox    # manually unfreeze by name (substring match)
mgctl unfreeze 12345      # manually unfreeze by PID
mgctl reload              # hot-reload config without restarting daemon (SIGHUP)
```

`mgctl` talks to the running daemon via a Unix domain socket at
`$XDG_RUNTIME_DIR/mgd.sock` (fallback: `/tmp/mgd-<uid>.sock`).

### Signals
| Signal | Effect |
|--------|--------|
| `SIGINT` / `SIGTERM` | Graceful shutdown — unfreezes all frozen processes first |
| `SIGHUP` | Hot-reload config on next decision cycle (same as `mgctl reload`) |

systemd sends `SIGTERM` when stopping the service, so frozen processes are
always cleaned up properly on `systemctl stop mgd`.

Logs every action to `~/memlogs/mgd_*.log`.

## Configuration

Edit `~/.config/mgd/priorities.toml` (falls back to `/etc/mgd/priorities.toml`,
then built-in defaults).

```toml
[defaults]
priority = 50    # default for unrecognised processes
log_keep = 10    # keep this many log files in ~/memlogs/ (0 = unlimited)

# Custom priority
[[apps]]
name    = "my-server"
pattern = "^my-server$"
priority = 25

# Force CRIU checkpoint at Critical pressure (never terminate)
[[apps]]
name       = "important-app"
pattern    = "^important-app$"
priority   = 45
checkpoint = true

# Never checkpoint this process (too fast to bother saving)
[[apps]]
name       = "quick-tool"
pattern    = "^quick-tool$"
priority   = 60
checkpoint = false

# Hard protect — mgd will never touch this process regardless of pressure
[[protect]]
name    = "my-vpn"
pattern = "^(openvpn|wg-quick)$"
```

After editing, apply without restart:
```bash
mgctl reload
# or
kill -HUP $(pgrep mgd)
```

## Security model

mgd runs as a user service and manages only processes owned by your user
session. System daemons (snapd, fwupd, root-owned services) are skipped
by design — both because mgd lacks permission to signal them, and because
its scope is deliberately limited to your session.

Running mgd as root or with elevated capabilities is not recommended and
not required for normal operation. CRIU checkpointing has reduced
functionality without CAP_SYS_ADMIN; the daemon falls back to SIGKILL
when checkpoint fails.

## Roadmap

- [x] PSI monitoring
- [x] Priority-based decision engine
- [x] Freeze/unfreeze cycle (SIGSTOP/SIGCONT)
- [x] Kill pipeline (SIGTERM → SIGKILL)
- [x] CRIU checkpoint with kill fallback
- [x] Frozen process registry
- [x] Session logging
- [x] Systemd user service
- [x] TOML config (per-app priorities without recompiling)
- [x] Unix socket IPC (`mgctl status`, `mgctl list`, `mgctl unfreeze`)
- [x] SIGTERM / SIGHUP handling (graceful shutdown + hot config reload)
- [x] Process protect list in config (`[[protect]]` entries)
- [x] Per-process `checkpoint = true/false` override in config
- [x] Log rotation (`log_keep` in config, default 10 files)
- [ ] D-Bus notifications with Restore button
- [ ] Kernel patches (PSI triggers, proactive swap-in, LRU hints)

See [DESIGN_SPEC.md](DESIGN_SPEC.md) for full architecture.

## Tested on

Fedora 44 · kernel 7.0.9 · HP Spectre x360 · i7-13700H · 16GB · KDE Plasma 6.6 Wayland

## License

GPL-2.0

## Author

Caleb Zongo — [github.com/Aquilesorei](https://github.com/Aquilesorei)