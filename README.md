# lmkd-linux

Userspace memory pressure manager for Linux desktop.

The Linux kernel swaps processes out on memory spikes but never actively reclaims — pages sit in swap indefinitely even when RAM is available, and the OOM killer only fires once the system is already unresponsive. There is no priority system: the compositor and a background file indexer are treated identically.

`lmkd-linux` monitors PSI (Pressure Stall Information) and manages the reclaim cycle the kernel skips — freezing, checkpointing, or killing processes in priority order before stall time reaches the point of no return, then restoring them when pressure clears.

**Core features:**

- **Reactive eviction** — PSI-triggered freeze (SIGSTOP), checkpoint (CRIU), or kill in priority order; unfreezes when pressure clears
- **Spike mode** — detects oscillating build tools (cargo, cmake, docker) and proactively frees RAM *before* the next allocation peak hits, without killing anything
- **CPU throttling (App Nap)** — writes `cpu.weight` + `cpu.max` quotas to background cgroups at elevated pressure; restores on pressure drop or foreground change
- **Memory caps** — `memory.max` on expendable background cgroups at High+ pressure; kernel reclaims from them first
- **Idle cgroup reclaim** — pushes idle background process pages to zram at Normal pressure, recovering RAM without freezing
- **Composite pressure score** — 55% PSI + 20% swap saturation + 15% GPU UMA residency + 10% swap I/O rate; catches fast-onset thrashing before PSI averages react
- **Priority-aware** — configurable tiers; foreground window (via DE plugin) gets -25 priority adjustment, shielding it from eviction
- **CRIU integration** — checkpoint before kill when possible; restore on recovery
- **Proactive zram compaction** — compacts fragmented zram pools at Elevated+ pressure
- **Emergency hibernate** — sustained Emergency pressure triggers `systemctl hibernate` as a last resort (off by default)

**Note:** `mgd` does not replace the Linux kernel OOM killer or `systemd-oomd`. It operates purely as a userspace prioritization layer that preemptively freezes or deprioritizes processes under pressure, while all kernel-level memory management remains fully active as the final safety mechanism.

## How it works

Instead of reacting to raw instantaneous metrics, `lmkd-linux` operates as a **damped feedback controller** to determine the system's memory pressure:

1. **Continuous Pressure Score ($P$):** Combines weighted system signals: **55% PSI memory stall**, **20% swap saturation**, **15% UMA GPU memory residency** (crucial for Intel Iris Xe and other integrated GPUs where graphics memory competing for system RAM is otherwise invisible to tools like `free`), and **10% swap I/O rate** (catches fast-onset thrashing before the 10s PSI average smooths it).
2. **Pressure Trend ($T = dP/dt$):** Measures the velocity of pressure changes to accelerate reactions to sudden spikes while ignoring brief transient blips.
3. **Damped State Machine:** Coordinates escalation and recovery. Upward transitions require pressure to persist for at least 2 cycles (ticks) unless a rapid critical spike occurs. Downward transitions require sustained calm (1–2 minutes) to prevent system oscillation.

Once a target pressure level is determined:
```
ELEVATED  →  zram compaction + SIGSTOP low-priority processes
HIGH      →  page-cache drop + SIGSTOP/SIGTERM expendable processes
CRITICAL  →  CRIU checkpoint or kill
EMERGENCY →  SIGKILL anything non-critical
```

If a desktop plugin (like `mgd-kde`) is active, the daemon dynamically protects your **active foreground window** by temporarily reducing its priority, ensuring it is shielded from eviction actions.


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
| 0–19 | System/critical | kwin_wayland, pipewire, plasmashell |
| 20–49 | Protected | RustRover, WebStorm, Firefox, Claude CLI |
| 50–59 | Normal background | generic user apps |
| 60–79 | Expendable background | plasma-discover, baloo, trackers |
| 80–100 | Expendable heavy | msedge, browser tabs, AI inference |

**Foreground adjustment:** the active window (reported by the DE plugin) gets its effective priority reduced by 25 before `plan()`, shielding it from eviction at its current pressure level.

## Requirements

**Kernel:** 5.19+ for full feature support · 4.20+ minimum (core eviction only)

| Kernel | What works |
|--------|-----------|
| 4.20+ | PSI monitoring, reactive freeze/kill, priority tiers |
| 5.2+  | + per-cgroup PSI (more precise pressure, falls back to global) |
| 5.8+  | + `CAP_PERFMON` for zero-privilege PSI trigger subprocess |
| **5.19+** | **+ `memory.reclaim` idle cgroup reclaim — full feature set** |

**Distro minimum versions** (ships kernel ≥ 5.19):

| Distro | Minimum version |
|--------|----------------|
| Fedora | 37+ |
| Ubuntu | 22.10+ (kernel 5.19) — *22.04 LTS ships 5.15, idle reclaim degrades gracefully* |
| Pop!\_OS | 22.04 LTS (ships 5.19 via OEM kernel) |
| Linux Mint | 21.2+ (Ubuntu 22.04 base — see Ubuntu note) |
| Arch Linux | Rolling — always supported |
| openSUSE Tumbleweed | Rolling — always supported |
| Debian | 12 Bookworm+ (kernel 6.1) |

**Architecture:** x86\_64. ARM64 untested but no x86-specific code.

**Other requirements:** cgroup v2 unified hierarchy must be the active cgroup mode (default on all distros listed above).

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
mgctl status                  # current pressure level + frozen/checkpointed counts
mgctl list                    # list all currently frozen processes
mgctl unfreeze firefox        # manually unfreeze by name (substring match)
mgctl unfreeze 12345          # manually unfreeze by PID
mgctl reload                  # hot-reload config without restarting daemon (SIGHUP)
mgctl doctor                  # print environment diagnostic + feature support report
mgctl calibrate [--dry-run]   # derive recommended PSI thresholds based on historic load
mgctl calibrate --apply       # apply previously generated calibration settings

mgctl restart                 # restart the mgd service
mgctl start | stop            # start / stop the mgd service
mgctl service                 # systemd unit state (active, PID, uptime, memory)
mgctl logs                    # last 50 daemon log lines
mgctl logs -f                 # follow daemon logs live
```

`status`, `list`, `unfreeze`, and `reload` talk to the running daemon via a Unix
domain socket at `$XDG_RUNTIME_DIR/mgd.sock` (fallback: `/tmp/mgd-<uid>.sock`).
The lifecycle commands (`restart`/`start`/`stop`/`service`/`logs`) wrap
`systemctl --user` / `journalctl --user` — they work even when the daemon is
down, which the socket commands cannot.

> `mgctl status` is the daemon's live view (pressure, frozen counts); `mgctl
> service` is the systemd unit view (active state, PID, uptime).

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

## Documentation

| Doc | What it covers |
|-----|---------------|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | End-to-end design spec — all threads, modules, data flows, and design rationale |
| [docs/DECISION_TREE.md](docs/DECISION_TREE.md) | Mermaid flowcharts — evictor loop, per-process plan(), reclaim, recovery, maintenance |
| [docs/PRIVILEGE_DESIGN.md](docs/PRIVILEGE_DESIGN.md) | Privilege split rationale — why caps live on helper binaries, not the daemon |

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
- [x] CRIU checkpoint with kill fallback (`mgd-checkpoint` validating wrapper — no caps on the daemon itself)
- [x] Frozen process registry
- [x] Session logging
- [x] Systemd user service
- [x] TOML config (per-app priorities without recompiling)
- [x] Unix socket IPC (`mgctl status`, `mgctl list`, `mgctl unfreeze`)
- [x] SIGTERM / SIGHUP handling (graceful shutdown + hot config reload)
- [x] Process protect list in config (`[[protect]]` entries)
- [x] Per-process `checkpoint = true/false` override in config
- [x] Log rotation (`log_keep` in config, default 10 files)
- [x] Cargo workspace + plugin architecture — `mgd-common`, `mgd`, `mgctl`, `mgd-zram`, plugin scaffolds
- [x] Plugin IPC protocol types (`PluginMessage`, `CoreMessage`)
- [x] Registry persistence across daemon restart
- [x] fdinfo GPU sweep cost reduction (pluginized decoupled architecture)
- [x] `mgctl doctor` + `mgctl calibrate` portability UX
- [x] PSI kernel trigger via `mgd-psi-trigger` capped subprocess (`cap_perfmon+ep`) with epoll fallback and auto-respawn
- [ ] Benchmark harness vs earlyoom / nohang / systemd-oomd
- [ ] COSMIC DE / Pop!_OS 24 plugin (`mgd-cosmic`)

## Tested on

Fedora 44 · kernel 7.0.9 · HP Spectre x360 · i7-13700H · 16GB · KDE Plasma 6.6 Wayland

## License

GPL-2.0

## Author

Caleb Zongo — [github.com/Aquilesorei](https://github.com/Aquilesorei)