# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build --workspace           # build all crates (zero errors, zero warnings)
cargo build --release             # production build (strips + LTO) — all workspace members
cargo build                       # dev build
cargo build -p <crate>            # build a single crate: mgd, mgctl, mgd-zram, mgd-gpu-intel, mgd-kde, mgd-common …
cargo test --workspace            # run the full test suite across all crates
cargo test test_parse_normal      # run single test by name
cargo run --bin mgd               # run daemon directly
cargo run --bin mgd -- freeze 1234   # legacy direct-signal CLI (no daemon needed)
cargo run --bin mgctl -- status      # control client → talks to running daemon
```

Install after build (unprivileged user service):
```bash
cp target/release/mgd   ~/.local/bin/mgd
cp target/release/mgctl ~/.local/bin/mgctl
cp config/mgd.service ~/.config/systemd/user/
systemctl --user enable --now mgd.service
```

`./install.sh` does the above plus the optional privileged bits: `setcap` on `criu`,
and installs the capped `mgd-zram-reclaim` helper + `packaging/mgd-zram.conf` tmpfiles
grant (see `docs/PRIVILEGE_DESIGN.md`). Privileged features degrade gracefully if skipped.

## Architecture

### Cargo workspace layout

```
lmkd-linux/
├── Cargo.toml              ← workspace root
├── mgd-common/             ← shared library: protocol, socket, logger, error, output, util
├── mgd/                    ← core daemon (portable, no desktop deps)
├── mgctl/                  ← CLI control client
├── mgd-checkpoint/         ← validating CRIU wrapper helper (mgd-checkpoint binary)
├── mgd-zram/               ← zram compact + proactive reclaim plugin (mgd-zram-reclaim binary)
├── mgd-gpu-intel/          ← Intel Iris Xe / UMA fdinfo watcher plugin (scaffold)
├── mgd-kde/                ← KDE Plasma 6+ watcher plugin (scaffold)
├── mgd-gpu-amd/            ← AMD APU equivalent (scaffold)
├── mgd-gnome/              ← GNOME Shell 45+ watcher (scaffold)
└── mgd-cosmic/             ← COSMIC DE / Pop!_OS 24 (scaffold)
```

**Binaries:**
- **`mgd`** — the core daemon. Four actor threads wired together in `mgd/src/main.rs`.
- **`mgctl`** — thin control client (`mgctl/src/main.rs`). Talks to the daemon over a Unix socket.
- **`mgd-checkpoint`** (`mgd-checkpoint/src/main.rs`) — validating CRIU wrapper. Accepts `dump <pid> <images-dir>` or `restore <images-dir>`. Validates caller owns the PID (in `user.slice`), images dir is inside caller's home, then raises ambient caps and execs criu with cleared env. Exit codes: 0 ok / 1 bad args / 2 security fail / 3 criu fail.
- **`mgd-zram-reclaim`** (`mgd-zram/src/reclaim.rs`) — minimal capped helper (`cap_sys_admin+ep`, `0750 root:mgd`). `swapoff`+`swapon` each zram device. Enforces its own OOM-headroom floor; exit code tells the daemon what happened. Never SUID-root.

**Shared library (`mgd-common`):** `socket_path()`, `Logger`, `MgdError`, `locked_print`/`locked_eprint`, `sync_print!`, `home_dir()`, the plugin IPC `protocol` types (`PluginMessage`, `CoreMessage`, `Metric`, `PluginAction`), and `psi` — PSI source resolution (`resolve_pressure_source()`: cgroup-first probe with global fallback, `trigger_armable()`), shared by the daemon and `mgctl doctor` so both report the same source.

### Daemon threads (spawned in `src/main.rs`)

1. **PressureResponder** (`src/evictor.rs`) — 5s poll. The eviction loop. Also runs CPU throttling and idle cgroup reclaim at Normal pressure.
2. **RecoveryManager** (`src/recovery.rs`) — 3s poll. Unfreeze/restore when healthy.
3. **IPC server** (`src/ipc.rs`) — Unix socket, serves `mgctl` requests and dispatches plugin connections to `plugin_server.rs`.
4. **MaintenanceManager** (`src/maintenance.rs`) — 60s poll. Slow/blocking housekeeping (idle plasma-discover reap, proactive swap reclaim, calibration sampling/persistence) kept off the evictor loop. Acts only at Normal pressure.

All threads share `Arc<Mutex<FrozenRegistry>>` + `Arc<Mutex<CheckpointRegistry>>` and a shared `Arc<Logger>`. They exit when `should_shutdown()` flips (set by SIGINT/SIGTERM handler).

### Module map

**`src/monitor/`** — reads system state, no side effects
- `psi.rs` — reads PSI from the per-session cgroup (`user.slice/user-<uid>.slice/user@<uid>.service/memory.pressure`) when usable, else global `/proc/pressure/memory` (resolved once, `pressure_source()`). Maps `some_avg10` to `PressureLevel`. Resolution logic lives in `mgd_common::psi` (shared with `mgctl doctor`). `PsiTrigger` (kernel epoll trigger, zero-CPU idle) arms on the same source, falling back to global when the cgroup file is root-owned (the normal case — systemd doesn't chown the `user@<uid>.service` node itself, even on ≥254). Thresholds (defaults, overridable via `[psi]` config block — `PsiThresholds`, validated, hot-reloadable): `<5%` Normal, `≥5%` Elevated, `≥25%` High, `≥50%` Critical, `≥70%` Emergency. Accelerator: `full_avg10 ≥ 20%` (`full_critical_pct`) forces Critical floor regardless of `some_avg10`. `elevated_pct` also sets the kernel-trigger arm threshold (armed once at startup — restart, not SIGHUP, to re-arm). `pressure_level_with()` is the pure mapping; `pressure_level()` reads the loaded config.
- `process.rs` — lists processes from `/proc/*/status`. Filters to own UID + `user.slice` cgroup only. `Process { pid, name, exe_basename, rss_kb, swap_kb, oom_score }`. `exe_basename` is the untruncated basename of `/proc/PID/exe`, used for `.desktop` category lookup (`name` is truncated to 15 chars by the kernel).
- `meminfo.rs` — reads `/proc/meminfo`, returns `MemInfo` (available/total/swap, `swap_used_pct()`).
- `gpu.rs` — per-process GPU residency from `/proc/<pid>/fdinfo/*` DRM accounting (zero privileges). Dedups dup'd fds by `drm-client-id`. Gated by a `/dev/dri/` symlink check + a 30s TTL cache (`GPU_CACHE`, `LazyLock<Mutex<…>>`) so the fdinfo walk doesn't run inline every evictor cycle. `restart_plasmashell()` cycles plasmashell via `kquitapp6` + `kstart`. Backs the optional plasmashell GPU-leak watcher.
- `cache.rs` — `drop_caches(paths)` advises page cache out via `posix_fadvise(DONTNEED)` over configured dir trees (clean pages only; dirty pages written back first, no data loss). Hand-rolled `~`/`*` expansion, bounded by `MAX_DEPTH`/`MAX_FILES`. Run before freezing at High+ pressure.
- `zram.rs` — zram introspection + compaction. `zram_used_mb`/`zram_orig_mb` (compressed vs decompressed footprint) feed the reclaim gates; `compact(dev)` writes `/sys/block/<dev>/compact` to free fragmentation-stranded pages (needs the `mgd-zram.conf` group-writable grant, else EACCES → degrade).

**`src/engine/`** — pure decision logic, no I/O
- `decision.rs` — `get_priority(name, exe_basename) -> u8` delegates to loaded config (0–100, higher = kill sooner). `plan(level, procs, available_kb, total_kb, swap_exhausted)` returns `Vec<Decision>` without executing. Target: RAM-scaled free % (configurable via `[thresholds]`). Skips processes <10MB (rss+swap). Hard rule: never touch priority ≤19. Freeze counts 0 toward the deficit (SIGSTOP frees nothing); only kill/terminate/checkpoint credit full RSS. Acquires the config lock once per call.
- `health.rs` — `HealthBaseline` tracks EMA of available RAM during Normal pressure. `safe_to_restore(available_kb, total_kb, rss_kb)` gates CRIU restores to prevent restore-kill loops.
- `calibrate.rs` — `Calibrator`, passive `[psi]` calibration (suggest-don't-apply). Seconds-weighted 1%-bin histograms of `some_avg10` split benign vs stalling (`full_avg10 ≥ 1%`) + a `full_avg10` histogram and debounced stall-episode counter. Fed by the evictor (5s) and maintenance (60s calm samples — the evictor sleeps when calm, so maintenance builds the noise floor); samples during active interventions (non-empty registries) are excluded so the daemon never calibrates off pressure it's treating. After ≥24h + ≥10 stall episodes, `suggest()` yields `elevated_pct` (benign p95 + 2%, capped by stall-onset p10) and `full_critical_pct` (stall-time full p95); upper tiers ratio-derived and emitted commented-out. Pure module — serialization is string-based (`to_toml`/`from_toml`); file I/O lives in `maintenance.rs`.

**`src/executor/`** — executes decisions
- `freezer.rs` — SIGSTOP/SIGCONT via `libc::kill`. `freeze_checked`/`unfreeze_checked` abort if PID start_time changed (recycle guard).
- `killer.rs` — `terminate()` does SIGTERM→wait→SIGKILL; `kill()` does immediate SIGKILL.
- `checkpoint.rs` — CRIU dump to `~/.local/share/mgd/snapshots/<pid>_<name>/`, SIGKILL after successful dump. Falls back gracefully if CRIU missing.
- `registry.rs` — `FrozenRegistry`: `HashMap<pid, entry>` (name, timestamp, start_time), persisted to `~/.local/share/mgd/state/`. `CheckpointRegistry`: checkpointed PIDs with snapshot dir, RSS, restore attempt count, also persisted. Both loaded at startup via `::load()`; `start_time` re-checked on re-adopt to guard PID recycle.
- `mod.rs` — shared `OpResult`, `read_start_time()`, `home_dir()`.

**`src/config.rs`** — hot-reloadable config behind `Arc<RwLock<CompiledConfig>>` (SIGHUP or `mgctl reload` swaps it). Loads `~/.config/mgd/priorities.toml`, then `/etc/mgd/priorities.toml`, then the built-in `config/priorities.toml` embedded via `include_str!`. Compiles `[[apps]]` to `(Regex, priority, checkpoint_override)`, `[[protect]]` to regexes, and scans XDG `.desktop` files into an `exe_basename → priority` index via `[category_priorities]`. Also carries the feature settings: `[zram]` (compact), `[reclaim]` (proactive swap reclaim), `[cache_drop]` (page-cache drop) — minutes→seconds at compile time — `[psi]` (pressure-tier boundaries, validated as a set, defaults on invalid), `[idle_reclaim]` (idle cgroup memory reclaim at Normal pressure: `enabled`, `idle_sec`, `rss_min_mb`, `reclaim_pct`, `global_cooldown_sec`, `max_swap_occupancy_pct`), and `[thresholds]` (`target_available_pct` — explicit override above the calibration file and RAM-scaling defaults).

**`src/ipc.rs`** — Unix socket server at `$XDG_RUNTIME_DIR/mgd.sock` (fallback `/tmp/mgd-<uid>.sock`). Newline-delimited text protocol: request `<cmd> [arg]\n`, response `OK <data>\n` / `ERR <msg>\n`. Commands: `status`, `list`, `reload`, `unfreeze <pid|name>`. Handles stale sockets, refuses to double-bind, caps at 8 concurrent connections. Incoming connections are classified on the first line: mgctl text commands go through the normal handler; connections whose first line parses as `PluginMessage::Identify` are handed off to `plugin_server::serve_plugin_connection()`.

**`src/plugin_server.rs`** — plugin connection handler and plugin state cache. `init_plugins()` is called at startup to auto-spawn DE and GPU plugin binaries: detects `XDG_CURRENT_DESKTOP` (spawns `mgd-kde`, `mgd-gnome`, or `mgd-cosmic`) and the DRM driver (`i915`/`xe` → `mgd-gpu-intel`, `amdgpu` → `mgd-gpu-amd`) by reading `/sys/class/drm/*/device/driver` symlinks. `serve_plugin_connection()` runs a writer thread per plugin (pushes `CoreMessage` broadcasts) and a reader loop routing `PluginMessage` variants: `Identify` (logged), `Observation` (GPU KB cached in `GPU_CACHE`), `ActionRequest` (KillPid executed directly; other actions approved and echoed back for plugin execution), `QueryGpu` (returns cached value), `ActiveWindow` (stores PID in `ACTIVE_FOREGROUND_PID`). `broadcast_pressure()` pushes `CoreMessage::PressureChanged` to all live plugin senders each evictor cycle.

**`src/logger.rs`** — appends structured lines to `~/memlogs/mgd_<YYYY-MM-DD_HH-MM-SS>.log` (local time, via `localtime_r`). New file per session; keeps `log_keep` files (rotation sorts by filename — the zero-padded stamp sorts chronologically).

**`src/recovery.rs`** — RecoveryManager. 3s poll. Acts only at Normal pressure. Unfreezes processes frozen ≥15s (hysteresis). Restores one checkpointed process per cycle (lightest first), gated by `HealthBaseline::safe_to_restore()`. Abandons after 3 failed restore attempts.

**`src/maintenance.rs`** — MaintenanceManager. 60s poll, acts only at Normal pressure (under pressure the evictor owns all process actions — the two must never act concurrently). `check_plasma_discover()` SIGTERMs an idle, oversized plasma-discover (CPU-idle sample blocks; PID-recycle guarded; KDE relaunches on demand). `check_proactive_reclaim()` runs the capped `mgd-zram-reclaim` helper — all gates live here (swap %, min zram used, OOM headroom vs decompressed footprint, `some_avg60 < 5%`); helper resolved by absolute path, disabled-for-session + logged once if absent. Owns passive-calibration I/O: feeds calm 60s PSI samples to the shared `Calibrator`, flushes its aggregates to `~/.local/share/mgd/calibration_state.toml` every 10 min when dirty (plus a final flush in `main.rs` at shutdown), and writes the ready-to-paste `[psi]` suggestion to `~/.local/share/mgd/calibration_suggestion.toml` once the data gates pass (rewritten only on change). `mgctl doctor` reads both files read-only. Loop subtracts blocking work time from the sleep to hold the ~60s period.

**`src/main.rs`** — `mgd` entry point. Legacy `freeze`/`unfreeze <pid>` subcommands bypass the loop. Otherwise: calls `try_elevate_scheduler_priority()` (attempts `SCHED_RR` prio 20, falls back to `nice -20` on `EPERM`, logs result), cleans orphaned snapshots, calls `plugin_server::init_plugins()` to auto-spawn matching DE/GPU plugins, installs SIGINT/SIGTERM (→shutdown) and SIGHUP (→reload) handlers, spawns the four threads, joins them, then runs `shutdown_unfreeze()` with PID-recycle safety. Also flushes the calibrator on clean shutdown.

**`src/error.rs`** — `MgdError` unified error enum.

**`src/output.rs`** — `locked_print`/`locked_eprint` + `sync_print!` macro for thread-safe stdout/stderr.

**`src/util.rs`** — `home_dir()`.

## Optional features (off by default, gated by config)

Each is a no-op unless enabled in `priorities.toml`. All cooldowns are not armed on failure.

**In the evictor loop — always active (per-cycle, no config gate):**
- **CPU throttling (App Nap)** — `update_cpu_throttling()` writes `cpu.weight=1` and a `cpu.max` quota to background process cgroups (priority ≥60, not the active foreground process). `ThrottledState` is tiered (None / Light / Heavy) with a 10s debounce. Foreground cgroups are unthrottled instantly on active-window change. All throttled cgroups are restored on daemon shutdown.
- **Swap escalation overrides** — if swap ≥95% full the effective pressure is forced to Critical; if swap ≥98% at Critical+ for ≥45s it is escalated to Emergency.
- **Cgroup kill cooldown** — after Kill/Terminate, the victim's cgroup path is suppressed for 45s so its RSS isn't re-counted toward the deficit.

**In the evictor loop — pre-action (before `plan()`, cheaper-first):**
- **`[zram] compact_on_elevated`** — at Elevated+, compact each zram pool (`compact_zram`) to free fragmentation-stranded pages; skips pools below `min_used_mb`. EACCES (grant absent) → disabled for session, logged once.
- **`[cache_drop] enabled`** — at the configured `trigger` level+, drop page cache for `paths` via `posix_fadvise(DONTNEED)` (`check_cache_drop`). Cooldown-gated.

**In the evictor loop — Normal pressure only:**
- **`[idle_reclaim] enabled`** — `check_idle_process_reclaim()`: background processes (not active foreground, not frozen) idle for ≥`idle_sec` (default 180s) with RSS ≥`rss_min_mb` get `memory.reclaim` written for `reclaim_pct`% of their RSS. Skipped when swap occupancy exceeds `max_swap_occupancy_pct`. Global cooldown: `global_cooldown_sec`. Runs both during active cycles (Normal pressure) and during PSI timeout (calm) periods so background cgroups are reclaimed proactively.

**In the maintenance loop** (Normal pressure only, 60s):
- **`[plasma_discover] watch_memory`** — SIGTERM an idle, oversized plasma-discover (KDE relaunches on demand). Gated by `rss_threshold_mb`, a CPU-idle sample over `idle_check_secs`, and a cooldown. (Handled by `mgd-kde` plugin; stub remains in maintenance for backwards compat.)
- **`[reclaim] proactive_swap_reclaim`** — run `mgd-zram-reclaim` to pull compressed pages back to RAM. Requires swap ≥ `threshold_pct`, zram used ≥ `min_zram_used_mb`, `some_avg60 < 5%`, and an OOM-headroom margin (`avail > decompressed_footprint × headroom_mult`).

## Key design constraints

- External dependencies: `libc`, `serde`, `toml`, `regex`, `serde_json` (plugin wire protocol — JSON for debuggability). No `chrono`, no `tokio`.
- `FrozenRegistry` and `CheckpointRegistry` are persisted to `~/.local/share/mgd/state/` on every mutation. Loaded at startup with `start_time` recycle-guard re-checks. Orphaned snapshot dirs are cleaned at startup for entries absent from the recovered registry.
- `plan()` is a dry run — `evictor.rs` excludes already-frozen PIDs (so their RSS isn't double-counted) and skips re-freezing. Evictor also excludes recently-killed cgroup paths (45s cooldown) to prevent double-counting.
- `evictor.rs` forces effective pressure to Critical when swap is ≥95% full, and escalates to Emergency if Critical+ pressure persists with ≥98% swap for ≥45s — catches slow pre-OOM that PSI alone misses.
- CRIU checkpoint/restore is mediated by `mgd-checkpoint` wrapper (absolute path, validated args, ambient caps → execs criu with cleared env). `mgd-checkpoint` absent or failing ⇒ falls back to SIGKILL and logs once. `CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE` on `criu` required; `CAP_NET_ADMIN` for live TCP restore. SIGSTOP/SIGCONT unprivileged for own processes.
- Privilege split (see `docs/PRIVILEGE_DESIGN.md`): the user service holds no caps. Privileged paths are opt-in and degrade if absent — `mgd-checkpoint` (validates and proxies criu caps), the capped `mgd-zram-reclaim` helper (`cap_sys_admin+ep`, swapoff/swapon) for proactive reclaim, and the `mgd-zram.conf` tmpfiles grant making `/sys/block/<dev>/compact` group-writable for zram compaction. All resolved by absolute path; never PATH, never attacker-controllable input.
- At startup, `try_elevate_scheduler_priority()` attempts `SCHED_RR` priority 20 (`CAP_SYS_NICE` required); falls back to `nice -20` on `EPERM`; logs whichever succeeded.
