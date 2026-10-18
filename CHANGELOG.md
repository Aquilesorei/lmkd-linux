# Changelog

All notable changes to mgd are recorded here.

---

## [0.2.0] — 2026-10-18

### Added

- **Process leak guard** (`[process_leak_guard]`, `mgctl leak-status`) — detects process families (grouped by exact `/proc/PID/cmdline`) whose live-instance count only ever grows and is never reaped, and terminates them. Motivated by a supervisor forking a fresh worker per request (0→120 procs / 8.4 GB in under an hour).
- **Spike mode idle demotion** — a Tracking spike idle for `idle_ticks_required` ticks (`<idle_cpu_threshold_pct` CPU, RSS growth `<idle_rss_delta_kb`) is demoted back to Observing and its victims released.
- `[spike_mode] victim_min_rss_kb` (default 25 MB rss+swap) — processes below it are never frozen as spike victims.
- `mgctl unfreeze` can release spike-mode victims (queued to the evictor thread, applied within ≤5s).

### Fixed

- **Spike victims frozen indefinitely at Normal pressure** — release/timeout/exit logic only ran inside the full evictor cycle, skipped on calm PSI timeouts. Now also runs on every calm tick.
- **Spike victim release churn** — victims were tagged with an arbitrary initiator spike and released when it exited, even while another spike still needed the headroom; the next cycle froze a fresh batch. Victims are now held while any spike is Tracking and released together.
- Spike mode froze tiny daemons (`gvfsd-fuse`, `fsnotifier`, crash reporters) that freed nothing measurable; a frozen `gvfsd-fuse` can hang any process touching `~/gvfs` in D state.
- Released victims are immune from re-freezing for 60s; the active foreground window is never frozen for headroom.

### Changed

- Built-in config: browsers, IDEs, `plasmashell`/`kwin_wayland` are excluded from spike tracking — they oscillate naturally (GC, indexing), and being tracked made them immune to the reactive evictor while dev tools were frozen on their behalf. `victim_exclude` extended with `cargo|make|ninja|gvfsd.*|fsnotifier`. `max_victim_freeze_sec` defaults to `300` (was `0` = unlimited).
- Internal: `config.rs` split into a `config/` module; `evictor.rs` split into an `evictor/` directory module; daemon startup moved to `init`/`lifecycle`/`memlock` with shared `AppState`.
- Added GPL-2.0 `LICENSE`.

---

## [0.1.0] — 2026-07-20

### Fixed

- **PSI kernel trigger broken on kernel 7.x** (`mgd-psi-trigger`, `mgd/src/monitor/psi.rs`, `mgd-common/src/psi.rs`)
  - Root cause 1: `/proc/pressure/memory` trigger writes return `EINVAL` unconditionally on kernel 7.x — the global PSI file no longer accepts trigger arming regardless of capabilities or format.
  - Root cause 2: Minimum PSI trigger window raised from 1s to 2s on kernel 7.x; window must be an exact multiple of 2s (`1000000`µs returned EINVAL).
  - Fix: `mgd-psi-trigger` now reads `/proc/self/cgroup`, walks the cgroup hierarchy upward, and arms the trigger on the highest writable `memory.pressure` file (typically `user@UID.service/app.slice/memory.pressure`). Window fixed at 2s (`2000000`µs) — valid on pre-7.x kernels (min 500ms) and 7.x+ (min 2s, must be multiple of 2s).
  - Fix: `PsiTrigger` (in-process fallback) uses the same `find_trigger_path()` logic from `mgd_common::psi` and 2s window instead of the broken global PSI file with hardcoded 1s window.
  - Fix: `mgd_common::psi::find_trigger_path()` added — shared by daemon and trigger helper, walks cgroup hierarchy leaf→root and returns the highest writable PSI file.
  - Result: `[responder] PSI kernel trigger armed via mgd-psi-trigger (zero-CPU idle).` — sub-millisecond pressure response restored.

### Changed

- `install.sh` comment for PSI trigger helper updated to reflect cgroup-based approach (kernel 7.x).
- `mgd-psi-trigger/Cargo.toml` description updated.

---

## Earlier work (pre-changelog)

See git log for history prior to this file. Key milestones:

- **v0.4.x** — GPU UMA stats (composite pressure score with GPU weight), important-tier idle reclaim, auto-kill-idle, spike mode proactive headroom manager, PSI calibration, mgd-kde active window tracking, CPU throttling (App Nap), memory.max cgroup caps.
- **v0.3.x** — CRIU checkpoint/restore via `mgd-checkpoint` wrapper, plugin IPC protocol, mgd-gpu-intel / mgd-gpu-amd fdinfo watchers.
- **v0.2.x** — zram reclaim helper (`mgd-zram-reclaim`), page-cache drop, idle cgroup reclaim, PSI subprocess trigger.
- **v0.1.x** — initial daemon: PSI monitoring, freeze/kill/terminate, priority tiers, FrozenRegistry persistence.
