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
- **t+71s** — Pressure dropped to Normal. The 30 frozen processes were
  unfrozen in staggered batches (capped per cycle, gated on RAM headroom)
  over the next few cycles, avoiding a bounce back into pressure. No orphans.

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

### Optional privileged features (opt-in)

mgd runs fully unprivileged out of the box. A few features need a small OS
privilege; each is opt-in and granted to a file on disk (not to the daemon —
`AmbientCapabilities=` does not work for a `--user` service). The `mgd` group
gates who may use them. Full rationale: [docs/PRIVILEGE_DESIGN.md](docs/PRIVILEGE_DESIGN.md).

The easiest way to enable all of them is the installer's opt-in flag (it prompts
for `sudo` and is safe to re-run):

```bash
./install.sh --privileged
```

This creates the `mgd` group, installs the zram-compact grant, installs and caps
the swap-reclaim helper, and caps `criu` — each step independent and skippable.
Prefer to do it by hand? The individual commands are below.

```bash
# one-time: create the group and add yourself (log out/in to take effect)
sudo groupadd -f mgd
sudo usermod -aG mgd "$USER"
```

zram compact (Fix 1 — no capability, sysfs group-write grant):
```bash
sudo install -m 0644 packaging/mgd-zram.conf /etc/tmpfiles.d/mgd-zram.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/mgd-zram.conf
```

CRIU checkpoint/restore without root (two narrow caps on the `criu` binary):
```bash
sudo setcap cap_checkpoint_restore,cap_sys_ptrace+ep "$(command -v criu)"
# optional — only if you want live TCP connections (e.g. browsers) to survive
# restore; widens the grant by one capability:
sudo setcap cap_checkpoint_restore,cap_sys_ptrace,cap_net_admin+ep "$(command -v criu)"
```

> **Caveat:** a distro package update of `criu` **resets its file capabilities**.
> Re-run the `setcap` above after upgrading criu, or mgd will fall back to
> SIGKILL again (it logs the criu privilege failure with the exact command to
> re-run).

> Each step above is independent — skipping one disables only that feature; mgd
> logs it unavailable at startup and continues. The swap-reclaim helper and its
> install are documented in `docs/PRIVILEGE_DESIGN.md`.

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

Running mgd as root is not recommended and not required for normal
operation. CRIU checkpointing works unprivileged once the `criu` binary
is granted two narrow capabilities (`CAP_CHECKPOINT_RESTORE` +
`CAP_SYS_PTRACE`) — **no root**. Without that grant the daemon simply
falls back to SIGKILL when checkpoint fails. See the opt-in setup below.

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