# lmkd-linux

Userspace memory pressure manager for Linux desktop.

The Linux kernel swaps processes out on memory spikes but never actively reclaims — pages sit in swap indefinitely even when RAM is available, and the OOM killer only fires once the system is already unresponsive. There is no priority system: the compositor and a background file indexer are treated identically.

`lmkd-linux` monitors PSI (Pressure Stall Information) and manages the reclaim cycle the kernel skips — freezing, checkpointing, or killing processes in priority order before stall time reaches the point of no return, then restoring them when pressure clears.

## How it works

Reads `/proc/pressure/memory` every 2 seconds. When stall time crosses a threshold, calculates the RAM deficit and works through processes from least to most important until enough is freed.

```
ELEVATED  (avg10 > 10%)  →  SIGSTOP low-priority processes
HIGH      (avg10 > 25%)  →  SIGSTOP + SIGTERM expendable processes
CRITICAL  (avg10 > 50%)  →  CRIU checkpoint or kill
EMERGENCY (avg10 > 70%)  →  SIGKILL anything non-critical
```

Processes are never killed above their priority tier. The compositor and audio server are hardcoded untouchable. Frozen processes are tracked and automatically resumed when pressure drops.

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
cargo build --bin mgd --release
cp target/release/mgd ~/.local/bin/mgd
```

Run as a systemd user service:
```bash
cp config/mgd.service ~/.config/systemd/user/
systemctl --user enable --now mgd.service
```

## Usage

```bash
mgd                    # run daemon
mgd freeze <pid>       # manually freeze a process
mgd unfreeze <pid>     # manually unfreeze
```

Logs every action to `~/memlogs/mgd_*.log`.

## Roadmap

- [x] PSI monitoring
- [x] Priority-based decision engine
- [x] Freeze/unfreeze cycle (SIGSTOP/SIGCONT)
- [x] Kill pipeline (SIGTERM → SIGKILL)
- [x] CRIU checkpoint with kill fallback
- [x] Frozen process registry
- [x] Session logging
- [x] Systemd user service
- [ ] TOML config (per-app priorities without recompiling)
- [ ] Wayland compositor focus detection
- [ ] D-Bus notifications with Restore button
- [ ] Kernel patches (PSI triggers, proactive swap-in, LRU hints)

See [DESIGN_SPEC.md](DESIGN_SPEC.md) for full architecture.

## Tested on

Fedora 44 · kernel 7.0.9 · HP Spectre x360 · i7-13700H · 16GB · KDE Plasma 6.6 Wayland

## License

GPL-2.0

## Author

Caleb Zongo — [github.com/Aquilesorei](https://github.com/Aquilesorei)