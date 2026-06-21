# MGD Decision Tree

## 1. Evictor Main Loop (5s cycle)

```mermaid
flowchart TD
    A([5s cycle tick / PSI wakeup]) --> B[Read PSI\nsome_avg10, full_avg10]
    B --> C{Apply swap overrides}
    C -->|swap ≥ 95%| C1[force → Critical]
    C -->|swap ≥ 98% + Critical+ for ≥45s| C2[escalate → Emergency]
    C -->|full_avg10 ≥ 20%| C3[floor → Critical]
    C --> D[effective_level]
    C1 --> D
    C2 --> D
    C3 --> D

    D --> E{effective_level?}

    E -->|Normal / PSI timeout| F[Idle path]
    F --> F1[CPU throttle update\nmemory.max cap restore\ncheck_idle_process_reclaim prio≥50\nNormal-only · no check_early here]
    F1 --> Z([next cycle])

    E -->|Elevated+| G[Pre-action path]
    G --> G1[zram compact\nif zram.compact_on_elevated]
    G1 --> G2[page cache drop\nif cache_drop.enabled]
    G2 --> G3[check_early_process_reclaim\nprio 50-59 background only\n30s cooldown · top 3 · 50% RSS]
    G3 --> H[plan — see Diagram 2]
    H --> I{decisions empty?}
    I -->|yes| Z
    I -->|no| J[dispatch loop]
    J --> K{action?}
    K -->|Freeze| L[SIGSTOP\n→ if success: FREEZE_RECLAIM\n100% RSS to zram]
    K -->|Terminate| M[SIGTERM async\n→ 5s → SIGKILL]
    K -->|Kill| N[SIGKILL]
    K -->|Checkpoint| O[mgd-checkpoint CRIU dump\n→ SIGKILL after success\n→ fallback Kill if CRIU fails]
    L --> P[wake recovery thread\nrecovery_wake condvar]
    M --> P
    N --> P
    O --> P
    P --> Q{Kill/Terminate/\nCheckpoint dispatched?}
    Q -->|yes| R[signal reclaim_wake condvar\n→ wake maintenance early]
    Q -->|no| Z
    R --> Z

    E -->|Emergency sustained ≥ threshold| Q[systemctl hibernate\none-shot]
    Q --> Z
```

---

## 2. Per-Process Decision — `plan()` + `determine_process_action()`

```mermaid
flowchart TD
    A([process candidate]) --> B{rss+swap > 10MB?}
    B -->|no| SKIP([skip])
    B -->|yes| C{prio ≤ 19?}
    C -->|yes — system/critical tier| SKIP
    C -->|no| D{on protect list?}
    D -->|yes| SKIP
    D -->|no| E{swap_exhausted\nAND prio ≥ 80?}
    E -->|yes| KILL([Kill])
    E -->|no| F{pressure level?}

    F -->|Elevated| G{prio ≥ 60?}
    G -->|yes| FREEZE([Freeze])
    G -->|no| NONE([None — skip])

    F -->|High| H{prio ≥ 80?}
    H -->|yes| TERM([Terminate])
    H -->|no| H2{prio ≥ 60?}
    H2 -->|yes| FREEZE
    H2 -->|no| NONE

    F -->|Critical| I{checkpoint_override set?}
    I -->|override=true + CRIU eligible| CP([Checkpoint])
    I -->|override=false| IO{swap_ratio > 0.5?}
    IO -->|yes| KILL
    IO -->|no| TERM
    I -->|not set| J{swap_ratio > 0.5\nAND prio ≥ 60?}
    J -->|yes — data on disk already| KILL
    J -->|no| K{prio ≥ 75?}
    K -->|yes| TERM
    K -->|no| L{CRIU eligible?}
    L -->|yes| CP
    L -->|no| M{prio ≥ 60?}
    M -->|yes| TERM
    M -->|no| KILL

    F -->|Emergency| KILL
```

---

## 3. Post-Freeze Reclaim

```mermaid
flowchart LR
    A([Action::Freeze dispatched]) --> B{SIGSTOP success?}
    B -->|no| END([log fail, continue])
    B -->|yes — process immobile| C[lookup cgroup_path\nfrom plan_procs]
    C --> D{cgroup_path found?}
    D -->|no| END
    D -->|yes| E[reclaim_cgroup\n100% RSS → zram\nno re-fault risk]
    E --> F{result?}
    F -->|Ok| G[log FREEZE_RECLAIM]
    F -->|EAGAIN — nothing reclaimable| END
    F -->|other error| END
    G --> END
```

---

## 4. Recovery Loop (3s cycle)

```mermaid
flowchart TD
    A([3s cycle]) --> B{effective_level == Normal?}
    B -->|no| Z([wait])
    B -->|yes| C[scan FrozenRegistry]
    C --> D{frozen ≥ 15s?}
    D -->|yes| E[SIGCONT unfreeze\npid-recycle guard]
    D -->|no| F[scan CheckpointRegistry]
    E --> F
    F --> G{checkpointed entries?}
    G -->|no| Z
    G -->|yes| H{safe_to_restore?\navail > EMA baseline + rss headroom}
    H -->|no| Z
    H -->|yes| I[restore lightest first\nmgd-checkpoint restore]
    I --> J{attempts ≥ 3?}
    J -->|yes| K[abandon entry]
    J -->|no| Z
    K --> Z
```

## 5. Maintenance Loop (60s cadence or kill-triggered)

```mermaid
flowchart TD
    A([condvar wait_timeout 60s]) --> B{woken by reclaim_wake\nor timeout?}
    B -->|timeout — normal cadence| C{pressure == Normal\nAND some_avg60 < 5%?}
    B -->|reclaim_wake signal\nevictor just killed/checkpointed| D[kill-triggered reclaim path\nskip calm gate]
    C -->|yes — calm| E[check_proactive_reclaim\nall gates apply]
    C -->|no| F[skip reclaim\ncalibration + plugin check only]
    D --> G[check_proactive_reclaim\nOOM headroom gate still applies\ncalm gate bypassed]
    E --> H[flush calibration if dirty\nplugin restart check]
    F --> H
    G --> H
    H --> A
```

**Gate comparison:**

| Gate | Calm path | Kill-triggered path |
|------|-----------|-------------------|
| `pressure == Normal` | required | **bypassed** |
| `some_avg60 < 5%` | required | **bypassed** |
| cooldown elapsed | required | required |
| `zram_used ≥ min_mb` | required | required |
| OOM headroom `avail > footprint × 1.5` | required | required |

---

## Priority Tiers (config/priorities.toml)

| Range | Tier | Examples | Behaviour |
|-------|------|----------|-----------|
| 0–19 | System/critical | kwin_wayland, pipewire, systemd | **Never touched** |
| 20–49 | Protected apps | claude, firefox, video-calls | Nothing at Elevated/High · Checkpoint (CRIU) or Kill (no CRIU) at Critical · Kill at Emergency |
| 50–59 | Normal background | generic user apps | `check_early_process_reclaim` target (50% RSS, 30s cooldown) · Nothing at Elevated/High · Checkpoint or Kill at Critical |
| 60–79 | Expendable background | baloo, tracker, updates | Freeze+FREEZE_RECLAIM at Elevated · Freeze at High · Checkpoint/Terminate/Kill at Critical |
| 80–100 | Expendable heavy | msedge, electron apps | Freeze+FREEZE_RECLAIM at Elevated · Terminate at High · Terminate (swap_ratio≤0.5) or Kill (swap_ratio>0.5) at Critical · Kill at swap_exhausted |

**Foreground priority adjustment:** the active foreground process (reported by DE plugin via `ActiveWindow`) gets `prio = max(prio - 25, 20)` before entering `plan()`. This REDUCES effective priority — a prio 80 browser in focus becomes prio 55, below the Freeze threshold at Elevated (need ≥ 60). It never elevates prio 20–49 processes; they stay protected.

---

## Pressure Level Thresholds (defaults, overridable via `[psi]`)

| Level | some_avg10 | Trigger |
|-------|-----------|---------|
| Normal | < 5% | idle reclaim, throttle only |
| Elevated | ≥ 5% | early reclaim + plan → Freeze prio≥60 |
| High | ≥ 25% | plan → Terminate prio≥80, Freeze prio≥60 |
| Critical | ≥ 50% | plan → Kill/Terminate/Checkpoint |
| Emergency | ≥ 70% | Kill all · hibernate if sustained |

Overrides: `full_avg10 ≥ 20%` forces Critical floor. `swap ≥ 95%` forces Critical. `swap ≥ 98%` + Critical sustained 45s → Emergency.
