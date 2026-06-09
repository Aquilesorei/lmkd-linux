# mgd Roadmap — Future Improvements

Status snapshot: working unprivileged memory-pressure daemon. Clean actor architecture,
escalation ladder (reclaim → freeze → checkpoint → kill), CRIU C/R, minimal deps.

This file tracks the gaps between "good" and "great". Grouped by impact.

---

## P0 — Correctness / robustness gaps

### 0. Cargo workspace + plugin architecture refactor ← **DONE** ✅
**Problem:** monolithic single-crate binary. KDE/Plasma/Intel-GPU-specific code lives in
the core daemon; zram reclaim is a side-car binary with no channel back to core. Adding
a new desktop or GPU watcher means patching `mgd` itself. Nothing is optional at link time.

**Plan:** split into a Cargo workspace of focused crates. All plugins connect to the
existing Unix socket; core broadcasts pressure level changes and plugins send observations
and action requests back. Core makes all kill/freeze decisions — plugins are observers only.

**Workspace layout:**
```
lmkd-linux/
├── Cargo.toml              ← workspace root
├── mgd-common/             ← shared library: protocol, config, logger, error, types
├── mgd/                    ← core daemon (portable, no desktop deps)
├── mgctl/                  ← CLI tool (extracted from src/mgctl.rs)
├── mgd-zram/               ← zram compact + proactive reclaim plugin
├── mgd-gpu-intel/          ← Intel Iris Xe / UMA fdinfo watcher plugin
└── mgd-kde/                ← KDE plasmashell + plasma-discover plugin
```

Scaffold only (empty `src/main.rs` with `todo!()`):
- `mgd-gpu-amd` — AMD APU equivalent
- `mgd-gnome` — GNOME Shell watchers
- `mgd-cosmic` — COSMIC DE (Pop!_OS 24)

**Plugin ecosystem dimensions** — each axis of variation is its own binary:

| Dimension     | Binary               | Targets                          |
|---------------|----------------------|----------------------------------|
| GPU vendor    | mgd-gpu-intel        | Intel iGPU + UMA (fdinfo)        |
|               | mgd-gpu-amd          | AMD APU + UMA                    |
|               | mgd-gpu-nvidia       | NVIDIA (future)                  |
| Desktop env   | mgd-kde              | KDE Plasma 6+                    |
|               | mgd-gnome            | GNOME Shell 45+                  |
|               | mgd-cosmic           | COSMIC DE / Pop!_OS 24           |
| Swap backend  | mgd-zram             | zram compressed swap             |
|               | (mgd-swap-partition) | traditional swap partition       |
| Use case      | (mgd-dev)            | developer workload tuning        |
|               | (mgd-media)          | video editing / streaming        |
| Hardware      | (mgd-laptop)         | battery awareness, thermal       |

Parenthesized entries are future — not scaffolded in #0.

**The socket protocol is the product.** The IPC message format in `mgd-common/src/protocol.rs`
is the stable interface every plugin speaks. Get it right before building more plugins —
changing it later breaks all consumers. A well-designed protocol means anyone can write a
plugin for their hardware or DE without touching core.

**IPC protocol extension** (in `mgd-common/src/protocol.rs`):
```rust
// Plugin → Core
enum PluginMessage {
    Identify { name: String, version: String, capabilities: Vec<String> },
    Observation { plugin: String, metric: Metric, pid: Option<u32>, value: f64 },
    ActionRequest { plugin: String, action: PluginAction, reason: String },
}
// Core → Plugin
enum CoreMessage {
    PressureChanged { level: PressureLevel },
    ActionResponse { action: PluginAction, approved: bool },
    Shutdown,
}
```

Added over the existing socket — existing `mgctl` commands (`status`, `list`, `reload`,
`unfreeze`) are unchanged.

**PSI epoll** happens here too: `mgd/src/monitor/psi.rs` switches from 5s polling to
kernel-native epoll trigger as part of this refactor (see #4 Phase A).

**install.sh** gains environment detection:
```bash
./install.sh --auto          # detect zram/KDE/Intel GPU, install matching plugins
./install.sh --core-only     # mgd + mgctl only
./install.sh --core --zram --kde --intel-gpu   # explicit
```

Each plugin gets a systemd service (`BindsTo=mgd.service`) so plugins start/stop with core.

**Constraints:**
- Zero behavior change in core daemon
- Existing config (`~/.config/mgd/priorities.toml`), socket path, log format unchanged
- No new dependencies beyond current Cargo.toml (`libc`, `serde`, `toml`, `regex`)
- No `unwrap()` on fallible paths
- Each crate must compile independently: `cargo build -p <crate>`

**Definition of done:**
```bash
cargo build --workspace   # zero errors, zero warnings
cargo test --workspace    # all existing tests pass
./install.sh --auto       # installs on Fedora 44 + KDE + Intel
mgctl status              # works identically to before
```

**Effort:** large (structural). No behavior change — it's mechanical move + IPC extension.
Biggest risk is import graph untangling when splitting `config.rs` and `types.rs` into
`mgd-common`. The PSI epoll portion is medium on its own.

---

### 1. Registry persistence across daemon restart
**Problem:** `FrozenRegistry` and `CheckpointRegistry` are in-memory only. On daemon
restart, frozen processes stay SIGSTOPped with no owner, and checkpoint snapshot dirs are
orphaned (cleaned at next startup → lost work).

**Plan:**
- Persist both registries to `~/.local/share/mgd/state/` on every mutation (or debounced).
  Format: line-delimited or small TOML, no new deps.
- Each entry already carries `start_time` — reuse it as the recycle guard on reload.
- On startup, before spawning threads:
  - Re-adopt frozen PIDs whose `start_time` still matches → keep in registry, let
    RecoveryManager unfreeze normally.
  - Frozen PIDs that died/recycled → drop, log once.
  - Checkpoint snapshots: match against persisted registry instead of blanket-cleaning.
    Re-adopt restorable ones; only delete truly orphaned dirs.
- Atomic write (temp + rename) so a crash mid-write can't corrupt state.

**Risk:** stale state re-adopting a recycled PID. Mitigated by `start_time` check — already
the pattern used in `freeze_checked`/`unfreeze_checked`.

**Effort:** medium. Touches `registry.rs`, `main.rs` (startup adoption), new `state.rs`.

---

### 2. fdinfo GPU sweep cost (deferred, now measured)
**Problem:** measured ~56–62 ms full sweep under real load (86 candidates, 24 GPU clients,
~700 MB resident GPU). NOT negligible. Synchronous, blocking, paid inline in the 5s evictor
loop — and it fires at High+ pressure, exactly when the loop most needs to stay responsive.
Cost dominated by GPU clients: each browser/Electron GPU process has hundreds of dup'd DRM
fds; we `read_to_string` every one even though dedup-by-`drm-client-id` collapses the result.

**Plan:**
- Dedup *before* reading: walk `/proc/<pid>/fdinfo/*`, but short-circuit once a
  `drm-client-id` already seen this pass — skip the full read of dup fds.
- Or: read only the first fd per `drm-client-id` (stat/readlink to group, read one).
- Move the sweep off the hot path: GPU residency feeds priority weighting, not the
  kill/freeze decision directly — it can run in MaintenanceManager (60s) and cache results,
  with the evictor reading the cache (extend existing 30s `GPU_CACHE` TTL pattern).
- Re-measure after: target <10 ms or fully off-loop.

**Effort:** small–medium. Touches `monitor/gpu.rs`, maybe `evictor.rs` cache read.

---

## P1 — Portability / decoupling

### 3. Decouple KDE/Plasma-specific features into plugins  — ✅ SUPERSEDED BY #0
**Problem:** plasmashell GPU-leak watcher and plasma-discover reaper couple a general daemon
to one desktop. Dead weight (and confusing) on GNOME/sway/headless.

**Resolution:** #0 (workspace refactor) implements this directly — `mgd-kde` and
`mgd-gpu-intel` become standalone plugin binaries. Core (`mgd`) loses all KDE/GPU-specific
code entirely; desktop specifics live in the plugin crates and their service files.

The earlier direction (generic watched-process config engine + `priorities.d/` presets)
is still valid for a *second* layer — after the workspace ships, a generic `watched_process`
config block can replace the plugin for simple cases. But the structural split (#0) comes
first.

**Effort:** subsumed into #0.

---

### 3b. `mgd doctor` + `mgd calibrate` — portability UX

#### `mgd doctor` — environment introspection

**Problem:** a new user installs mgd on unfamiliar hardware with no feedback on what's
active, what was skipped, and whether thresholds make sense for their machine.

Reads environment, reports detected hardware and enabled/disabled features:
```
mgd doctor

Environment:
  GPU:        Intel Iris Xe        ✓ mgd-gpu-intel active
  Swap:       zram (/dev/zram0)    ✓ mgd-zram active
  Desktop:    KDE Plasma 6.6       ✓ mgd-kde active
  Compositor: KWin Wayland         ✓ kquitapp6/kstart found

Features enabled:
  ✓ PSI epoll monitoring
  ✓ Process freeze/kill cycle
  ✓ plasmashell GPU watcher (mgd-kde)
  ✓ plasma-discover watcher (mgd-kde)
  ✓ zram compact (mgd-zram)
  ✗ AMD GPU watcher   (not applicable — Intel GPU detected)
  ✗ NVIDIA watcher    (not applicable)

Thresholds: using calibration from 2026-05-24
  target_available_pct: 18%  (calibrated from sweep — default was 15%)
  swap_onset_mb:        8200  (swap first observed at this allocation level)
  psi_recovery_secs:    45    (seconds for PSI to return to baseline after load)
```

**Effort:** small. Read-only detection code — no state mutation.

---

#### `mgd calibrate` — controlled pressure sweep (new approach)

**Problem:** the `15%` free-RAM target threshold is hardcoded and machine-agnostic.
On a 32 GB workstation it is over-aggressive (wastes 5 GB of headroom); on an 8 GB laptop
under KDE it may be too relaxed. The old plan (passive EMA only) leaves every machine on
the same default. Active stress-to-crash is dangerous (non-deterministic, IO-driven false
positives, UI freeze as the calibration signal).

**Approach:** controlled step-load pressure sweep — not "push until freeze" but
"step up and observe the curve until a PSI inflection is detected". This gives inflection
points (where the system starts degrading), not crash points.

**3-phase protocol:**

```
Phase 1 — idle baseline (60s)
  Record: PSI some/full avg10+avg60, swap_in rate, MemAvailable
  Output: clean-system baseline fingerprint

Phase 2 — controlled pressure sweep
  Allocate +200 MB of anon memory (mmap/madvise MADV_POPULATE_READ)
  Wait 20s, sample PSI + swap_in rate
  Repeat until STOP condition:
    • PSI full_avg10 > 15%    ← system under real stall pressure
    • swap_in rate spikes      ← kernel is actively swapping
    • user interrupts (Ctrl-C, battery low, thermal throttle detected)
  Record: allocated_at_stop → this is the swap_onset_mb marker
  Release all memory after stop.

Phase 3 — recovery curve (60s)
  Sample PSI every 5s until it returns within 10% of Phase 1 baseline
  Output: psi_recovery_secs — the time constant of the system's damping
```

**Output** — `~/.local/share/mgd/calibration.json` (machine data, not reviewed):
```json
{
  "calibrated_at": "2026-05-24T14:30:00Z",
  "total_ram_mb": 16384,
  "swap_onset_mb": 8200,
  "psi_recovery_secs": 45,
  "baseline_psi_some_avg10": 1.2,
  "baseline_psi_full_avg10": 0.1
}
```

**Derived config** — `~/.config/mgd/calibration.toml` (user-reviewable, opt-in):
```toml
# Generated by mgd calibrate — review before applying with: mgd calibrate --apply
# Replaces hardcoded defaults in the daemon.
[thresholds]
target_available_pct = 18        # derived: swap_onset_mb / total_ram_mb + 3% headroom
psi_recovery_secs    = 45        # used to tune RecoveryManager dwell time
```

**CLI:**
```bash
mgd calibrate               # run sweep, write calibration.json + calibration.toml
mgd calibrate --apply       # copy calibration.toml into active config, reload daemon
mgd calibrate --dry-run     # run sweep, print suggested values, write nothing
```

**Safety constraints (non-negotiable):**
- Never run if battery < 30% or thermal throttle detected (`/sys/class/thermal/`)
- Never run if `some_avg10 > 2%` at start (system already under load)
- Interruptible at any point with clean memory release (signal handler frees all allocations)
- Memory is allocated with `MADV_DONTNEED` available so it can be released before any OOM
- No silent runtime drift — calibration is a one-shot op, not a background loop
- Calibration does NOT auto-apply — it only suggests; user must run `--apply`

**Why this over pure passive observation:**
Passive EMA sees whatever workload happened to run — it cannot isolate the machine's true
pressure curve from workload-specific noise. The step-sweep is reproducible, interruptible,
and short (~5 min total). The key constraint is stopping well before a freeze signal — the
PSI inflection point is a clean, early, deterministic signal.

**How mgd uses the output at runtime:**
```
Before calibration: PSI some_avg10 > 25% → act (magic number)
After calibration:  PSI some_avg10 > (baseline × 20×) → act  (per-device derived)
                    target_available_pct = calibrated value (not 15%)
```

**Effort:** medium. `mgd calibrate` is a new subcommand in `mgctl` (or standalone binary).
Phase 1+3 reuse `monitor/psi.rs`. Phase 2 needs a controlled allocator (safe `mmap` loop).
Output writer is trivial JSON. `--apply` reuses `config::reload()`. Biggest care: the
signal handler that guarantees memory release on interrupt.

---

### 4. PSI thresholds → kernel-native triggers + passive calibration
**Problem:** thresholds 5/25/50/70% are magic numbers, tuned on one machine. Two issues
stacked: (a) the daemon *polls* `/proc/pressure/memory` every 5s instead of letting the
kernel notify it, and (b) the threshold unit (`some_avg10 ≥ 25%`) is abstract and
machine-specific. The kernel already measures pressure (PSI) and can *signal* on a threshold
— we're not using that.

**Insight:** kernel tells *how much* pressure (PSI number). It can also *notify* when a
threshold is crossed (PSI triggers). It cannot pick the threshold — that's policy, always
ours. So the win is: (1) stop polling, use kernel events; (2) express the threshold in a
physical, portable unit (stall-time budget, not abstract %); (3) calibrate the few remaining
knobs passively, never by active probing.

**Plan, ordered:**

- **Phase A — PSI trigger (epoll), replace the poll loop.**
  Write a stall-time budget to the pressure file and `poll()`/epoll the fd; kernel wakes the
  evictor only when crossed. Example: `some 150000 1000000` = wake if >150 ms stall in any
  1 s window.
  - Event-driven → lower latency at the moment that matters, no 5 s busy-poll.
  - Threshold becomes a stall-time budget (ms/window) — physical and portable, far better
    than `some_avg10 ≥ 25%` per machine.
  - Keep a coarse fallback timer so recovery/maintenance cadence still ticks.
  - Multiple triggers can map to the existing tiers (one budget per Elevated/High/Critical).
  - Touches `monitor/psi.rs` + `evictor.rs` loop structure (poll → epoll wait).

- **Phase B — RAM-scaling of the remaining knobs (free, safe).**
  The deficit target / free-RAM floors aren't RAM-relative now. Scale them by total RAM so an
  8 GB and a 64 GB machine get sane defaults with no probing.

- **Phase C — per-cgroup pressure (tighter scope).**
  Daemon already filters to `user.slice`. Read **`user.slice/memory.pressure`** (cgroup-v2
  per-cgroup PSI) instead of system-wide, so other slices' pressure doesn't mislead. Optional:
  watch `memory.events` for kernel `high`/`max`/`oom` notifications; consider setting
  `memory.high` to let the kernel apply backpressure/reclaim before mgd ever acts.

- **Phase D — passive calibration, suggest-don't-apply.**
  Extend the existing `HealthBaseline` EMA: record the PSI/stall value where THIS machine
  actually starts stalling (`full_avg10` spikes), log over days, and emit a
  `calibration_suggestion.toml` the user reviews and opts into. No active memory probing
  (would cause the pressure it's meant to prevent), no silent runtime drift (kills
  reproducibility — the codebase values predictability: recycle guards, dry-run `plan()`).

- **Phase E — config exposure.**
  All budgets/thresholds live in a `[psi]` config block so tuning needs no recompile.

**Why not active auto-probe / continuous adaptation:** probing stresses memory on purpose
(bad at boot, felt later); continuous self-adjustment drifts, is non-reproducible, and forms
a feedback loop (daemon acts → changes pressure → recalibrates off its own intervention →
oscillates). Passive observe + kernel-native triggers + RAM-scaling gets ~90% of the benefit
without those traps.

**Effort:** Phase A medium (epoll rewrite of the loop), B small, C medium, D medium
(mostly the suggestion plumbing), E small. A is the real upgrade — it replaces the
magic-number debate with kernel events and a portable unit.

---

### 5. Fractional deficit credit (deferred Phase 2)
**Problem:** `plan()` currently counts only 25% of a frozen process's RSS toward the deficit
(freeze ≠ free). The fractional multiplier is a fixed guess, not validated against real PSI
recovery behavior.

**Plan:**
- Make the fraction a config value, default current 25%.
- Scope any tuning to **expendable-tier kills only** first (low risk if wrong).
- Validate against real-machine PSI observation: freeze N MB, watch how much `some_avg10`
  actually recovers, back out the true effective fraction.
- Do NOT generalize until measured. Tagged as needing real-machine data.

**Effort:** small code, medium validation. Blocked on observation, not engineering.

---

## P2 — Positioning / proof

### 6. Benchmark vs earlyoom / nohang / systemd-oomd
**Problem:** niche audience, established competitors. No data showing mgd is better.

**Plan:**
- Define a repeatable memory-pressure workload (fork-bomb-lite / tab-storm / `stress-ng`
  vm load) under a memory-capped cgroup or VM.
- Metrics: time-to-first-action, wrong-kill rate (did it kill the foreground app?),
  recovery time, total RSS reclaimed, latency added to interactive process.
- Run identical workload under earlyoom, nohang, systemd-oomd, mgd. Table the results.
- This produces the "why mine better" pitch *and* surfaces real tuning bugs.

**Effort:** medium–large (harness setup). High payoff — converts "portfolio project" into
"validated tool".

### 7. Strategic: COSMIC DE / Pop!_OS 24 ecosystem opportunity
**Context:** COSMIC is pure Wayland from day one (no X11 fallback), built on smithay (Rust).
Shell and compositor are the same process — unlike KDE where plasmashell can be restarted
independently, a smithay memory leak requires a full compositor restart (logout). Target
hardware is Intel-heavy (System76 laptops, HP Spectres, ThinkPads) — exactly UMA. Beta
memory management is less mature than KWin's 20+ years. Users are already reporting slowdowns.

**Opportunity:** mgd is architecturally aligned with what Pop!_OS is building (Rust, UMA
target, plugin per DE). After #0 ships, `mgd-cosmic` becomes a standalone binary that System76
or community contributors can build and maintain — it just needs to speak the mgd socket
protocol. Core need not change at all.

**Path to engagement:**
1. After workspace (#0) + benchmarks (#6): file a detailed bug against COSMIC with
   intel_gpu_top data, plasmashell watcher as reference implementation, and mgd-cosmic as
   the proposed integration point.
2. The UMA + Wayland problem space is one they cannot avoid — having instrumented data
   (exact MB per process, freeze/unfreeze cycle logs) is a stronger filing than the typical
   "my system got slow after 2 hours."
3. If mgd matures, this could be a legitimate contribution to their ecosystem — or at
   minimum gets their attention on a real hardware pain point.

**Action required:** none now. Revisit after #0 + #6 are done.

---

### 8. Optional: system-wide / multi-user scope
**Problem:** scoped to own UID + `user.slice`. Fine for desktop, blocks server use.

**Plan:** likely NOT worth it — conflicts with the unprivileged-first design (system-wide
needs root or broad caps, killing the main security selling point). Document this as a
**deliberate non-goal** rather than a TODO. If pursued, it's a separate privileged mode, not
a flag.

---

## Suggested order

1. ~~(#0) **Cargo workspace + plugin refactor**~~ ✅ Done — workspace live, protocol defined, all tests pass.
2. (#1) **Registry persistence** ← **START HERE** — biggest correctness win, self-contained. `state.rs` lands in `mgd/`.
3. (#2) fdinfo cost — measured pain. Now lives in `mgd-gpu-intel`; easier to fix in isolation.
4. (#3b) `mgd doctor` + `mgd calibrate` — portability UX. Cheap to add; makes mgd usable for anyone.
5. (#4 Phase B+E) RAM-scaling + `[psi]` config exposure — cheap, unblocks portability.
6. (#6) Benchmark harness — converts "portfolio project" to "validated tool". Data drives #4D, #5, and the COSMIC pitch (#7).
7. (#4 Phase C/D) per-cgroup pressure + passive calibration suggestions.
8. (#5) Fractional deficit — blocked on #6 data anyway.
9. (#7) COSMIC/Pop!_OS engagement — file after benchmarks exist.
10. (#8) System-wide — document as non-goal.
