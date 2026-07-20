# Changelog

All notable changes to mgd are recorded here.

---

## [Unreleased] — feat/gpu-uma-stats

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
