# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build --release             # production build (strips + LTO) — builds both bins
cargo build                       # dev build
cargo test                        # run all 19 tests (decision.rs, psi.rs, gpu.rs, firefox.rs)
cargo test test_parse_normal      # run single test by name
cargo run --bin mgd               # run daemon directly
cargo run --bin mgd -- freeze 1234   # legacy direct-signal CLI (no daemon needed)
cargo run --bin mgctl -- status      # control client → talks to running daemon
```

Install after build:
```bash
cp target/release/mgd   ~/.local/bin/mgd
cp target/release/mgctl ~/.local/bin/mgctl
cp config/mgd.service ~/.config/systemd/user/
systemctl --user enable --now mgd.service
```

## Architecture

Two binaries:
- **`mgd`** — the daemon. Three actor threads wired together in `src/main.rs`.
- **`mgctl`** — thin control client (`src/mgctl.rs`). Talks to the daemon over a Unix socket.

Shared library code lives in `src/lib.rs` (just `socket_path()`, used by both bins).

### Daemon threads (spawned in `src/main.rs`)

1. **PressureResponder** (`src/evictor.rs`) — 5s poll. The eviction loop.
2. **RecoveryManager** (`src/recovery.rs`) — 3s poll. Unfreeze/restore when healthy.
3. **IPC server** (`src/ipc.rs`) — Unix socket, serves `mgctl` requests.

All three share `Arc<Mutex<FrozenRegistry>>` + `Arc<Mutex<CheckpointRegistry>>` and a shared `Arc<Logger>`. They exit when `should_shutdown()` flips (set by SIGINT/SIGTERM handler).

### Module map

**`src/monitor/`** — reads system state, no side effects
- `psi.rs` — parses `/proc/pressure/memory`, maps `some_avg10` to `PressureLevel`. Thresholds: `<5%` Normal, `≥5%` Elevated, `≥25%` High, `≥50%` Critical, `≥70%` Emergency. Accelerator: `full_avg10 ≥ 20%` forces Critical floor regardless of `some_avg10`.
- `process.rs` — lists processes from `/proc/*/status`. Filters to own UID + `user.slice` cgroup only. `Process { pid, name, exe_basename, rss_kb, swap_kb, oom_score }`. `exe_basename` is the untruncated basename of `/proc/PID/exe`, used for `.desktop` category lookup (`name` is truncated to 15 chars by the kernel).
- `meminfo.rs` — reads `/proc/meminfo`, returns `MemInfo` (available/total/swap, `swap_used_pct()`).
- `gpu.rs` — per-process GPU residency from `/proc/<pid>/fdinfo/*` DRM accounting (zero privileges). Dedups dup'd fds by `drm-client-id`. `restart_plasmashell()` cycles plasmashell via `kquitapp6` + `kstart`. Backs the optional plasmashell GPU-leak watcher.
- `firefox.rs` — sums RSS across main + content Firefox processes; `trigger_firefox_gc()` sends SIGUSR1 to the main `firefox` PID (lowest PID) to nudge its internal GC. Backs the optional Firefox watcher.

**`src/engine/`** — pure decision logic, no I/O
- `decision.rs` — `get_priority(name, exe_basename) -> u8` delegates to loaded config (0–100, higher = kill sooner). `plan(level, procs, available_kb, total_kb)` returns `Vec<Decision>` without executing. Target: 15% free RAM. Skips processes <10MB (rss+swap). Hard rule: never touch priority ≤19. Freeze counts only 25% of RSS toward deficit. Acquires the config lock once per call.
- `health.rs` — `HealthBaseline` tracks EMA of available RAM during Normal pressure. `safe_to_restore(available_kb, total_kb, rss_kb)` gates CRIU restores to prevent restore-kill loops.

**`src/executor/`** — executes decisions
- `freezer.rs` — SIGSTOP/SIGCONT via `libc::kill`. `freeze_checked`/`unfreeze_checked` abort if PID start_time changed (recycle guard).
- `killer.rs` — `terminate()` does SIGTERM→wait→SIGKILL; `kill()` does immediate SIGKILL.
- `checkpoint.rs` — CRIU dump to `~/.local/share/mgd/snapshots/<pid>_<name>/`, SIGKILL after successful dump. Falls back gracefully if CRIU missing.
- `registry.rs` — `FrozenRegistry`: in-memory `HashMap<pid, entry>` (name, timestamp, start_time). `CheckpointRegistry`: checkpointed PIDs with snapshot dir, RSS, restore attempt count. Neither persisted across restarts.
- `mod.rs` — shared `OpResult`, `read_start_time()`, `home_dir()`.

**`src/config.rs`** — hot-reloadable config behind `Arc<RwLock<CompiledConfig>>` (SIGHUP or `mgctl reload` swaps it). Loads `~/.config/mgd/priorities.toml`, then `/etc/mgd/priorities.toml`, then the built-in `config/priorities.toml` embedded via `include_str!`. Compiles `[[apps]]` to `(Regex, priority, checkpoint_override)`, `[[protect]]` to regexes, and scans XDG `.desktop` files into an `exe_basename → priority` index via `[category_priorities]`. Also carries the `[plasma]` and `[firefox]` watcher settings (minutes→seconds at compile time).

**`src/ipc.rs`** — Unix socket server at `$XDG_RUNTIME_DIR/mgd.sock` (fallback `/tmp/mgd-<uid>.sock`). Newline-delimited text protocol: request `<cmd> [arg]\n`, response `OK <data>\n` / `ERR <msg>\n`. Commands: `status`, `list`, `reload`, `unfreeze <pid|name>`. Handles stale sockets, refuses to double-bind, caps at 8 concurrent connections.

**`src/logger.rs`** — appends structured lines to `~/memlogs/mgd_<unix_ts>.log`. New file per session; keeps `log_keep` files.

**`src/recovery.rs`** — RecoveryManager. 3s poll. Acts only at Normal pressure. Unfreezes processes frozen ≥15s (hysteresis). Restores one checkpointed process per cycle (lightest first), gated by `HealthBaseline::safe_to_restore()`. Abandons after 3 failed restore attempts.

**`src/main.rs`** — `mgd` entry point. Legacy `freeze`/`unfreeze <pid>` subcommands bypass the loop. Otherwise: cleans orphaned snapshots, installs SIGINT/SIGTERM (→shutdown) and SIGHUP (→reload) handlers, spawns the three threads, joins them, then runs `shutdown_unfreeze()` with PID-recycle safety.

**`src/error.rs`** — `MgdError` unified error enum.

**`src/output.rs`** — `locked_print`/`locked_eprint` + `sync_print!` macro for thread-safe stdout/stderr.

**`src/util.rs`** — `home_dir()`.

## Optional watchers (off by default, gated by config)

Both run inside the evictor loop and are no-ops unless enabled in `priorities.toml`.

- **`[plasma] watch_gpu_leak`** — KDE Plasma + Intel UMA workaround. plasmashell leaks GPU memory (allocated from system RAM) over long uptimes; when residency crosses `gpu_leak_threshold_mb` and the `min_restart_interval_min` cooldown has elapsed, restart it and log reclaimed memory. Cooldown not armed on failure.
- **`[firefox] watch_memory`** — preventive GC. Runs ONLY at PressureLevel::Normal (the evictor handles Firefox under pressure; the two never act on Firefox concurrently). When total Firefox RSS crosses `rss_threshold_mb` and the GC cooldown elapsed, sends SIGUSR1 to the main process. `warn_threshold_mb` logs informationally with no cooldown.

## Key design constraints

- External dependencies: `libc`, `serde`, `toml`, `regex`. No `chrono`, no `tokio`.
- `FrozenRegistry` and `CheckpointRegistry` are not persisted — frozen/checkpointed processes are orphaned if the daemon restarts (orphaned snapshots are cleaned at next startup).
- `plan()` is a dry run — `evictor.rs` excludes already-frozen PIDs (so their RSS isn't double-counted) and skips re-freezing.
- `evictor.rs` escalates the pressure level one tier when swap is ≥85% full (`escalate_for_swap`) — catches slow pre-OOM that PSI alone misses.
- CRIU checkpoint/restore needs `CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE` on the `criu` binary (`setcap`, opt-in via `install.sh --privileged`); **root no longer required**. `CAP_NET_ADMIN` additionally needed for `--tcp-established` restore (live TCP, e.g. browsers). criu is resolved by absolute path (never PATH — the binary may be capped); missing/unprivileged ⇒ falls back to SIGKILL and logs once. SIGSTOP/SIGCONT and SIGUSR1 work without elevated privileges for own processes.
