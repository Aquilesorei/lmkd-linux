# Memory Guardian (mgd) — Design Specification

## Linux Desktop Low-Memory Killer Daemon

**Author:** Caleb Zongo (Aquilesorei)
**Version:** 0.1.0-draft
**Date:** May 2026
**License:** TBD (GPL-2.0 recommended for kernel compatibility)

---

## 1. Problem Statement

### 1.1 The Problem

Linux desktop systems with limited RAM (8–16 GB) freeze under memory pressure when running modern development workloads (IDEs, browsers, CLI tools). The system becomes completely unresponsive, forcing a hard power-off and risking data loss.

### 1.2 Root Cause

The Linux kernel's memory management was designed for servers, not interactive desktops:

- **No process priority awareness**: The kernel treats all processes equally for swap/reclaim decisions. A background updater gets the same treatment as the focused IDE.
- **Lazy swap-in**: When memory pressure subsides, the kernel does not proactively bring pages back from swap. Applications remain slow even after RAM frees up.
- **OOM Killer triggers too late**: By the time the Out-Of-Memory killer activates, the system is already frozen — the user can't even open a terminal.
- **MGLRU behavioral change**: Kernel 6.1+ introduced Multi-Gen LRU, which interprets swappiness values more aggressively than the old LRU algorithm. The same `swappiness=60` on kernel 7.0 causes significantly more swapping than on kernel 5.15.
- **No desktop awareness**: The kernel has no concept of "foreground window," "compositor," or "user-interactive process."

### 1.3 How Android Solved This

Android runs on devices with 4–8 GB RAM and never freezes because it has:

- **lmkd (Low Memory Killer Daemon)**: Monitors memory pressure via PSI and kills background apps before the system slows down.
- **App priority tiers**: Foreground, visible, service, cached — each tier has different kill thresholds.
- **Proactive reclaim**: Aggressively reclaims memory before OOM.
- **Process lifecycle**: Apps are checkpointed, killed, and restored transparently.
- **zram tuning**: Compressed swap in RAM with conservative swappiness.

Linux desktop has **none of these mechanisms** integrated into a cohesive system.

### 1.4 Goal

Build a memory management daemon for Linux desktops that:

1. Prevents system freezes by acting before OOM
2. Kills/suspends/checkpoints processes based on user-defined priority
3. Integrates with the desktop environment (Wayland compositor awareness)
4. Notifies the user of actions taken
5. Supports save/restore of killed processes (CRIU)
6. Works with existing kernel interfaces (PSI, cgroups, oom_score_adj)
7. Requires no kernel modifications in Phase 1

---

## 2. Architecture

### 2.1 Implementation Strategy: Three Phases

#### Phase 1: Pure Userspace (Option C) — MVP

- Daemon reads `/proc/pressure/memory` and `/proc/[pid]/`
- Uses existing `oom_score_adj` (-1000 to 1000) for priority
- Sends SIGSTOP/SIGTERM/SIGKILL based on priority and pressure
- Desktop notifications via D-Bus
- CRIU checkpoint/restore for eligible processes
- No kernel changes required
- **Target: Shippable standalone project**

#### Phase 2: Kernel + Userspace (Option B) — Enhanced

- Small kernel patch: expose per-process swap statistics
- Better cgroup v2 integration for memory limits per priority tier
- Kernel-side PSI threshold triggers (avoid polling overhead)
- Proactive swap-in hook when free memory exceeds threshold
- **Target: Submit patches to linux-mm mailing list**

#### Phase 3: Pure Kernel (Option A) — Long-Term

- Modify `mm/oom_kill.c` to read priority metadata from procfs
- Add new syscall or procfs interface for desktop priority hints
- Change page reclaim in `mm/vmscan.c` to respect priority tiers
- Desktop-aware MGLRU mode
- **Target: Upstream into mainline kernel (multi-year effort)**

### 2.2 System Architecture (Phase 1)

```
┌─────────────────────────────────────────────────────────┐
│                      User Space                         │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐              │
│  │ App A    │  │ App B    │  │ App C    │              │
│  │ pri: 10  │  │ pri: 45  │  │ pri: 80  │              │
│  │ CRITICAL │  │ NORMAL   │  │ EXPENDBL │              │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘              │
│       │              │              │                    │
│  ┌────▼──────────────▼──────────────▼─────────────────┐ │
│  │              mgd (Memory Guardian Daemon)           │ │
│  │                                                     │ │
│  │  ┌─────────┐ ┌──────────┐ ┌──────────┐            │ │
│  │  │ Monitor │ │ Priority │ │ Executor │            │ │
│  │  │         │ │ Registry │ │          │            │ │
│  │  │ • PSI   │ │          │ │ • Freeze │            │ │
│  │  │ • RSS   │ │ • Config │ │ • CRIU   │            │ │
│  │  │ • Swap  │ │ • Focus  │ │ • Kill   │            │ │
│  │  │ • Thold │ │ • .desk  │ │ • Notify │            │ │
│  │  └────┬────┘ └────┬─────┘ └────┬─────┘            │ │
│  │       │           │            │                    │ │
│  │  ┌────▼───────────▼────────────▼─────────────────┐ │ │
│  │  │              Decision Engine                   │ │ │
│  │  │                                                │ │ │
│  │  │  IF pressure > threshold:                      │ │ │
│  │  │    1. Calculate RAM deficit                     │ │ │
│  │  │    2. Sort by priority (highest number first)   │ │ │
│  │  │    3. Execute action (freeze → checkpoint → kill)│ │
│  │  │    4. Stop when enough RAM freed                │ │ │
│  │  └────────────────────────────────────────────────┘ │ │
│  └─────────────────────┬───────────────────────────────┘ │
│                        │                                  │
│  ┌─────────────────────▼───────────────────────────────┐ │
│  │              Integration Layer                       │ │
│  │                                                      │ │
│  │  • D-Bus notifications (with Restore button)         │ │
│  │  • Wayland compositor protocol (focused window)      │ │
│  │  • CRIU interface (checkpoint/restore)                │ │
│  │  • Systemd journal (structured logging)              │ │
│  └──────────────────────────────────────────────────────┘ │
│                                                           │
├───────────────────────────────────────────────────────────┤
│                      Kernel                               │
│                                                           │
│  /proc/pressure/memory        PSI memory pressure         │
│  /proc/[pid]/status           Per-process memory stats    │
│  /proc/[pid]/oom_score_adj    OOM priority (-1000..1000)  │
│  /proc/[pid]/cgroup           cgroup membership           │
│  /proc/sys/vm/swappiness      Swap aggressiveness         │
│  /sys/kernel/mm/lru_gen/      MGLRU configuration         │
│  cgroups v2                   Memory limits per group     │
│                                                           │
└───────────────────────────────────────────────────────────┘
```

---

## 3. Priority System

### 3.1 Priority Tiers

| Tier | Range | Name | Behavior | Examples |
|------|-------|------|----------|----------|
| 0 | 0–9 | SYSTEM | Never touch | systemd, kernel threads |
| 1 | 10–19 | CRITICAL | Never kill, never freeze | Compositor (KWin, Mutter), PulseAudio/PipeWire, display server |
| 2 | 20–39 | HIGH | Kill only as absolute last resort | Focused/foreground application |
| 3 | 40–59 | NORMAL | Default tier for all applications | Visible but unfocused apps, browsers, IDEs |
| 4 | 60–79 | LOW | Freeze/checkpoint early | Background services, updaters, indexers |
| 5 | 80–100 | EXPENDABLE | Kill first, no warning | Cached processes, inactive browser tabs, preview generators |

### 3.2 Dynamic Priority Adjustment

Priority is not static. The daemon adjusts priority dynamically based on:

- **Compositor focus**: The focused window's process gets promoted to tier 2 (HIGH). When it loses focus, it returns to its base priority.
- **Recency**: Processes that were recently interactive get a temporary priority boost (decay over 5 minutes).
- **User override**: CLI or notification actions can permanently pin a process priority.

```
Base Priority (from config)
  + Focus Boost (-20 if focused window)
  + Recency Boost (-10 if used in last 60s, -5 if last 5min)
  + User Override (absolute, ignores other modifiers)
  = Effective Priority (clamped to 0–100)
```

### 3.3 Priority Sources (in order of precedence)

#### Source 1: User CLI Override (highest precedence)
```bash
mgd priority --pid 1234 --set 20
mgd priority --name firefox --set 40
mgd priority --class "org.kde.kate" --set 35
```

#### Source 2: Configuration File
```toml
# /etc/mgd/priorities.toml

[defaults]
# Default priority for unlisted applications
default_priority = 50

[system]
# These are hardcoded and cannot be overridden
# systemd = 0
# kthreadd = 0
# compositor = 10

[apps.webstorm]
match = "webstorm"          # match against process name or cmdline
priority = 35
checkpoint = true           # safe to CRIU checkpoint
max_memory_mb = 2048        # optional: soft limit before deprioritization

[apps.firefox]
match = "firefox"
priority = 45
checkpoint = true
# Firefox spawns multiple content processes
child_priority = 55         # content processes get lower priority than main

[apps.software_updater]
match = "plasma-discover"
priority = 75
checkpoint = false          # just kill, don't bother saving

[apps.file_manager]
match = "dolphin"
priority = 50
checkpoint = true
```

#### Source 3: Desktop File Metadata (lowest precedence)
```ini
# /usr/share/applications/webstorm.desktop
[Desktop Entry]
Name=WebStorm
Exec=webstorm
X-MemoryGuardian-Priority=35
X-MemoryGuardian-Checkpoint=true
```

### 3.4 Mapping to oom_score_adj

The daemon translates its priority system to the kernel's `oom_score_adj` so the kernel's own OOM killer respects the same hierarchy as a fallback:

| mgd Priority | oom_score_adj | Meaning |
|---------------|---------------|---------|
| 0–9 | -1000 | OOM-immune |
| 10–19 | -900 | Almost immune |
| 20–39 | -500 | Protected |
| 40–59 | 0 | Default |
| 60–79 | 500 | Preferred target |
| 80–100 | 900 | Kill first |

---

## 4. Memory Monitoring

### 4.1 Data Sources

#### PSI (Pressure Stall Information)
```
/proc/pressure/memory
some avg10=25.00 avg60=10.00 avg300=5.00 total=133037544
full avg10=15.00 avg60=5.00  avg300=2.00 total=124524167
```

- `some`: At least one task stalled on memory (partial stall)
- `full`: All tasks stalled on memory (complete stall)
- `avg10`: Percentage of time stalled in last 10 seconds
- `avg60`: Percentage of time stalled in last 60 seconds
- `avg300`: Percentage of time stalled in last 5 minutes

#### Per-Process Memory
```
/proc/[pid]/status → VmRSS, VmSwap, RssAnon, RssFile
/proc/[pid]/statm  → resident pages, shared pages
/proc/[pid]/oom_score → kernel's OOM score
```

#### System Memory
```
/proc/meminfo → MemTotal, MemAvailable, SwapTotal, SwapFree
```

### 4.2 Pressure Thresholds

| Level | PSI avg10 (some) | Action |
|-------|-----------------|--------|
| NORMAL | 0–10 | No action. Routine monitoring. |
| ELEVATED | 10–25 | Warning logged. Increase polling frequency to 500ms. |
| HIGH | 25–50 | Freeze lowest priority EXPENDABLE processes (SIGSTOP). |
| CRITICAL | 50–70 | CRIU checkpoint LOW priority processes. Kill EXPENDABLE. |
| EMERGENCY | 70+ | SIGTERM/SIGKILL up the priority chain. Notify user. |

### 4.3 RAM Threshold (Alternative Trigger)

In addition to PSI, the daemon monitors absolute memory availability:

| Condition | Action |
|-----------|--------|
| MemAvailable > 20% of total | No action |
| MemAvailable 10–20% of total | Elevated monitoring |
| MemAvailable 5–10% of total | Begin freeze/checkpoint cycle |
| MemAvailable < 5% of total | Emergency kill cycle |

### 4.4 New Process Spawn Trigger

When a new process is spawned (detected via `netlink proc connector` or `fanotify`) and available memory is below 15%, the daemon preemptively reclaims from lowest priority processes to make room. This prevents the situation where launching a new app pushes the system into a freeze.

### 4.5 Polling Strategy

| State | Poll Interval | Reason |
|-------|---------------|--------|
| NORMAL | 2000ms | Low overhead, no urgency |
| ELEVATED | 500ms | Need faster reaction |
| HIGH | 200ms | Active intervention happening |
| CRITICAL | 100ms | Every millisecond counts |

---

## 5. Execution Engine

### 5.1 Action Escalation

When memory pressure is detected, the daemon escalates through actions in order. It stops as soon as enough memory is freed.

```
Step 1: FREEZE (SIGSTOP)
  → Target: EXPENDABLE tier (80–100)
  → Effect: Process stops executing, pages become reclaimable
  → Reversible: Yes, instant (SIGCONT)
  → RAM freed: Indirect (pages can be reclaimed by kernel)
  → Notification: Silent (no user notification)

Step 2: CHECKPOINT (CRIU dump + SIGKILL)
  → Target: LOW tier (60–79), then NORMAL tier (40–59)
  → Effect: Full process state saved to disk, process killed
  → Reversible: Yes, user can restore later
  → RAM freed: Full RSS of process
  → Notification: Yes, with [Restore] button
  → Prerequisite: checkpoint=true in config

Step 3: TERMINATE (SIGTERM)
  → Target: NORMAL tier (40–59), only if checkpoint=false
  → Effect: Graceful shutdown, app can save state
  → Reversible: No (app must be relaunched manually)
  → RAM freed: Full RSS after exit
  → Notification: Yes, with app name and memory freed
  → Grace period: 5 seconds before SIGKILL

Step 4: KILL (SIGKILL)
  → Target: Any non-CRITICAL process if EMERGENCY
  → Effect: Immediate termination, no cleanup
  → Reversible: No
  → RAM freed: Full RSS immediately
  → Notification: Yes, urgent
  → Only when: PSI avg10 > 70 or MemAvailable < 2%
```

### 5.2 RAM Deficit Calculation

```
target_free = total_ram * 0.15          # Want 15% free after action
current_free = available_ram
deficit = target_free - current_free    # How much we need to free

IF deficit <= 0:
    no action needed

FOR process IN sorted_by_priority(descending):  # Highest number = kill first
    IF process.priority <= 19:
        BREAK  # Never touch CRITICAL or SYSTEM

    IF deficit <= 0:
        BREAK  # Freed enough

    action = select_action(process, pressure_level)
    execute(action, process)
    deficit -= process.rss
```

### 5.3 Process Selection Algorithm

```
candidates = get_all_processes()
    .filter(pid != self)
    .filter(priority > 19)           # Never touch SYSTEM/CRITICAL
    .sort_by(effective_priority DESC) # Highest number = least important

FOR candidate IN candidates:
    IF candidate.rss < 10MB:
        SKIP  # Not worth killing, negligible memory

    IF candidate.checkpoint_eligible AND pressure < EMERGENCY:
        action = CHECKPOINT
    ELIF pressure >= CRITICAL:
        action = TERMINATE
    ELIF pressure >= HIGH:
        action = FREEZE
    ELSE:
        action = FREEZE

    YIELD (candidate, action)
```

---

## 6. CRIU Integration (Save & Restore)

### 6.1 Overview

CRIU (Checkpoint/Restore In Userspace) allows saving a running process's entire state to disk and restoring it later exactly where it left off. This brings Android-style app lifecycle to Linux desktop.

### 6.2 Checkpoint Flow

```
1. Daemon decides to checkpoint process P (pid=1234)
2. Create snapshot directory: /var/lib/mgd/snapshots/1234_firefox_20260524_201530/
3. Execute: criu dump --tree 1234 --images-dir <snapshot_dir> --shell-job --leave-stopped
4. Verify dump succeeded (check exit code + image integrity)
5. Record metadata:
   {
     "pid": 1234,
     "name": "firefox",
     "cmdline": "/usr/lib64/firefox/firefox",
     "rss_mb": 811,
     "priority": 45,
     "timestamp": "2026-05-24T20:15:30Z",
     "snapshot_dir": "/var/lib/mgd/snapshots/1234_firefox_20260524_201530/",
     "desktop_file": "/usr/share/applications/firefox.desktop",
     "status": "saved"
   }
6. Kill the process (if not already stopped by CRIU)
7. Send notification with [Restore] button
```

### 6.3 Restore Flow

```
1. User clicks [Restore] or runs: mgd restore firefox
2. Daemon reads metadata from snapshot directory
3. Execute: criu restore --images-dir <snapshot_dir> --shell-job
4. Verify restore succeeded
5. Update oom_score_adj for restored process
6. Clean up snapshot directory
7. Notify: "Firefox restored (811MB)"
```

### 6.4 Checkpoint Compatibility

Not all processes can be checkpointed. Known limitations:

| Category | Checkpointable | Reason |
|----------|---------------|--------|
| Terminal apps (vim, htop) | Yes | Simple process state |
| Java/JVM apps (WebStorm, IntelliJ) | Yes (usually) | JVM state is self-contained |
| Firefox/Chrome | Partial | GPU processes may fail; main process works |
| Wayland compositor | No | GPU state, display server |
| Audio server (PipeWire) | No | Hardware device state |
| Apps with open sockets | Maybe | TCP connections may timeout |
| Apps using DRM/GPU | No | GPU memory not dumpable |

The config file specifies `checkpoint = true/false` per app to avoid attempting impossible checkpoints.

### 6.5 Snapshot Storage Management

```toml
# /etc/mgd/config.toml

[snapshots]
directory = "/var/lib/mgd/snapshots"
max_total_size_gb = 10          # Auto-delete oldest if exceeded
max_snapshot_age_hours = 24     # Auto-delete after 24h
max_snapshots_per_app = 3       # Keep last 3 snapshots per app
```

---

## 7. Desktop Integration

### 7.1 Wayland Compositor Integration

The daemon needs to know which window is focused to dynamically boost its process priority.

#### KDE Plasma (KWin)
- Use `org.kde.KWin` D-Bus interface
- `org.kde.KWin.activeWindow()` → returns window ID
- Map window ID → PID via `/proc/[pid]/environ` or `_NET_WM_PID`

#### GNOME (Mutter)
- Use `org.gnome.Shell.Eval` or `org.gnome.Shell.Extensions`
- `global.display.focus_window.get_pid()`

#### Generic Wayland
- `wlr-foreign-toplevel-management-unstable-v1` protocol (for wlroots-based compositors)
- `ext-foreign-toplevel-list-v1` (standardized protocol, newer)

#### Fallback
- If compositor integration fails, skip dynamic focus boost
- Static priorities from config still work

### 7.2 D-Bus Notifications

```xml
<!-- org.freedesktop.Notifications -->
<notification>
  <app_name>Memory Guardian</app_name>
  <summary>Saved & closed "Software Updater"</summary>
  <body>Freed 140MB of memory for your workflow.</body>
  <actions>
    <action key="restore">Restore Now</action>
    <action key="later">Restore Later</action>
    <action key="settings">Settings</action>
  </actions>
  <hints>
    <hint key="urgency">1</hint>  <!-- normal -->
    <hint key="category">device</hint>
  </hints>
</notification>
```

#### Notification Types

| Event | Urgency | Actions |
|-------|---------|---------|
| Process frozen (SIGSTOP) | Low | [Resume] |
| Process checkpointed | Normal | [Restore Now] [Restore Later] |
| Process terminated | Normal | [Relaunch] |
| Process killed (emergency) | Critical | [Details] |
| Multiple processes killed | Critical | [View Log] [Settings] |

### 7.3 System Tray / Status Area

Optional KDE/GNOME widget showing:
- Current memory pressure level (green/yellow/red)
- Number of frozen processes
- Number of saved snapshots available for restore
- Quick access to restore menu

### 7.4 D-Bus Service Interface

```xml
<!-- org.mgd.MemoryGuardian -->
<interface name="org.mgd.MemoryGuardian">
  <method name="SetPriority">
    <arg name="pid" type="u" direction="in"/>
    <arg name="priority" type="u" direction="in"/>
  </method>
  <method name="GetStatus">
    <arg name="status" type="s" direction="out"/>  <!-- JSON -->
  </method>
  <method name="RestoreProcess">
    <arg name="snapshot_id" type="s" direction="in"/>
  </method>
  <method name="ListSnapshots">
    <arg name="snapshots" type="s" direction="out"/>  <!-- JSON array -->
  </method>
  <method name="Pause">  <!-- Temporarily disable daemon -->
  </method>
  <method name="Resume">
  </method>
  <signal name="ProcessAction">
    <arg name="action" type="s"/>  <!-- frozen/checkpointed/killed -->
    <arg name="pid" type="u"/>
    <arg name="name" type="s"/>
    <arg name="freed_mb" type="u"/>
  </signal>
</interface>
```

---

## 8. Configuration

### 8.1 Main Configuration

```toml
# /etc/mgd/config.toml

[daemon]
poll_interval_ms = 2000             # Normal state polling
log_level = "info"                  # debug, info, warn, error
log_file = "/var/log/mgd/mgd.log"

[thresholds]
# PSI-based thresholds (avg10 values)
elevated_psi = 10
high_psi = 25
critical_psi = 50
emergency_psi = 70

# RAM-based thresholds (percentage of MemAvailable)
elevated_ram_pct = 20
high_ram_pct = 10
critical_ram_pct = 5
emergency_ram_pct = 2

# Target free memory after reclaim action
target_free_pct = 15

[actions]
# Grace period for SIGTERM before SIGKILL
sigterm_timeout_secs = 5

# Minimum RSS to consider killing (don't bother with tiny processes)
min_rss_mb = 10

# Enable CRIU checkpoint/restore
enable_checkpoint = true

# Enable SIGSTOP freezing
enable_freeze = true

[focus]
# Dynamic priority boost for focused window
focus_boost = 20
recency_boost_60s = 10
recency_boost_5min = 5

[compositor]
# Auto-detect or specify
type = "auto"   # auto, kwin, mutter, wlroots, none

[notifications]
enabled = true
show_freeze = false         # Don't notify on SIGSTOP (too noisy)
show_checkpoint = true
show_kill = true

[snapshots]
directory = "/var/lib/mgd/snapshots"
max_total_size_gb = 10
max_snapshot_age_hours = 24
max_snapshots_per_app = 3
```

### 8.2 Per-App Priority Configuration

```toml
# /etc/mgd/priorities.toml

[defaults]
default_priority = 50
default_checkpoint = false

# System processes (hardcoded, cannot be overridden by user)
# Priority 0: systemd, kernel threads
# Priority 10: compositor, audio server, display manager

[apps.firefox]
match = "firefox"
match_type = "name"             # name, cmdline, class, desktop
priority = 45
checkpoint = true
child_priority = 55             # Content processes
child_match = "-contentproc"    # How to identify child processes

[apps.webstorm]
match = "webstorm"
match_type = "cmdline"
priority = 35
checkpoint = true
max_memory_mb = 2048            # Soft limit: deprioritize if exceeded

[apps.claude]
match = "claude"
match_type = "name"
priority = 40
checkpoint = false              # CLI tool with active connections

[apps.kate]
match = "kate"
match_type = "name"
priority = 45
checkpoint = true

[apps.dolphin]
match = "dolphin"
match_type = "name"
priority = 55
checkpoint = true

[apps.discover]
match = "plasma-discover"
match_type = "name"
priority = 75
checkpoint = false              # Just kill it

[apps.baloo]
match = "baloo_file"
match_type = "name"
priority = 85
checkpoint = false              # File indexer, expendable
```

### 8.3 User Override File

```toml
# ~/.config/mgd/overrides.toml
# User-specific overrides (highest precedence)

[apps.firefox]
priority = 30    # User considers Firefox more important than default
```

---

## 9. CLI Interface

### 9.1 Commands

```
mgd                              # Show daemon status
mgd status                       # Detailed status (pressure, process list, frozen/saved)
mgd status --json                # Machine-readable output

mgd priority                     # List all process priorities
mgd priority --pid 1234          # Show priority for PID
mgd priority --pid 1234 --set 20 # Set priority for PID
mgd priority --name firefox --set 40
mgd priority --class "org.kde.kate" --set 35

mgd freeze --pid 1234            # Manually freeze a process
mgd unfreeze --pid 1234          # Resume a frozen process

mgd checkpoint --pid 1234        # Manually checkpoint a process
mgd restore                      # List available snapshots
mgd restore --id <snapshot_id>   # Restore a specific snapshot
mgd restore --latest firefox     # Restore most recent Firefox snapshot

mgd log                          # Show recent actions
mgd log --since "1 hour ago"     # Filter by time
mgd log --action kill            # Filter by action type

mgd pause                        # Temporarily disable daemon
mgd resume                       # Re-enable daemon

mgd config                       # Show effective configuration
mgd config --validate            # Validate config files
```

### 9.2 Status Output

```
$ mgd status

Memory Guardian v0.1.0
──────────────────────────────────────

System Memory:
  RAM:  8.6 / 15.3 GiB used (56%)
  Swap: 272 / 8192 MiB used (3%)
  Available: 6.7 GiB

Pressure (PSI avg10):
  some: 0.00%  ██░░░░░░░░ NORMAL
  full: 0.00%  ██░░░░░░░░ NORMAL

State: NORMAL (polling every 2000ms)

Active Processes (by priority):
  PRI  PID     RSS      NAME
   10  5223    191 MiB  kwin_wayland
   10  1892     45 MiB  pipewire
   35  526548  1.8 GiB  webstorm
   40  514445  260 MiB  claude
   45  21884   804 MiB  firefox (main)
   55  408009  694 MiB  firefox (tab)
   55  371430  711 MiB  firefox (tab)
   75  401956  138 MiB  plasma-discover

Frozen: 0
Saved Snapshots: 0
Actions (last hour): 0
```

---

## 10. Project Structure

```
mgd/
├── Cargo.toml
├── README.md
├── LICENSE
│
├── src/
│   ├── main.rs                 # Entry point, daemon setup
│   ├── lib.rs                  # Public API
│   │
│   ├── monitor/
│   │   ├── mod.rs
│   │   ├── psi.rs              # PSI reader (/proc/pressure/memory)
│   │   ├── memory.rs           # /proc/meminfo parser
│   │   ├── process.rs          # Per-process memory stats
│   │   └── procwatch.rs        # New process spawn detection (netlink)
│   │
│   ├── priority/
│   │   ├── mod.rs
│   │   ├── registry.rs         # Priority store + lookup
│   │   ├── config.rs           # TOML config parser
│   │   ├── desktop.rs          # .desktop file X-MemoryGuardian parser
│   │   ├── dynamic.rs          # Focus boost + recency decay
│   │   └── oom.rs              # oom_score_adj synchronization
│   │
│   ├── engine/
│   │   ├── mod.rs
│   │   ├── decision.rs         # Pressure → action mapping
│   │   ├── calculator.rs       # RAM deficit calculation
│   │   └── selector.rs         # Process selection algorithm
│   │
│   ├── executor/
│   │   ├── mod.rs
│   │   ├── freezer.rs          # SIGSTOP / SIGCONT
│   │   ├── checkpoint.rs       # CRIU dump / restore
│   │   ├── killer.rs           # SIGTERM / SIGKILL
│   │   └── snapshot.rs         # Snapshot storage management
│   │
│   ├── compositor/
│   │   ├── mod.rs
│   │   ├── kwin.rs             # KDE KWin D-Bus integration
│   │   ├── mutter.rs           # GNOME Mutter integration
│   │   ├── wlroots.rs          # wlroots protocol integration
│   │   └── fallback.rs         # No compositor (static priority only)
│   │
│   ├── notify/
│   │   ├── mod.rs
│   │   ├── dbus.rs             # D-Bus notification sender
│   │   └── actions.rs          # Handle notification button clicks
│   │
│   ├── dbus/
│   │   ├── mod.rs
│   │   └── service.rs          # org.mgd.MemoryGuardian interface
│   │
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── status.rs           # mgd status
│   │   ├── priority.rs         # mgd priority
│   │   ├── restore.rs          # mgd restore
│   │   └── log.rs              # mgd log
│   │
│   └── logger.rs               # Structured logging (journald)
│
├── config/
│   ├── config.toml             # Default daemon config
│   ├── priorities.toml         # Default app priorities
│   └── mgd.service             # Systemd user service
│
├── tests/
│   ├── integration/
│   │   ├── test_psi.rs         # PSI monitoring tests
│   │   ├── test_priority.rs    # Priority resolution tests
│   │   ├── test_decision.rs    # Decision engine tests
│   │   └── test_freeze.rs      # Freeze/unfreeze tests
│   └── fixtures/
│       ├── proc_pressure/      # Mock /proc/pressure data
│       └── proc_meminfo/       # Mock /proc/meminfo data
│
└── docs/
    ├── DESIGN_SPEC.md           # This document
    ├── CONTRIBUTING.md
    ├── KERNEL_PATCHES.md        # Phase 2/3 kernel work
    └── COMPARISON.md            # Comparison with Android lmkd
```

---

## 11. Phase 2: Kernel Enhancements

### 11.1 Proposed Kernel Patches

Once Phase 1 proves the concept, these kernel patches would improve the system:

#### Patch 1: Per-Process Swap Statistics in procfs
```
/proc/[pid]/swap_stats
  swap_in_count: 1234      # Number of pages swapped in
  swap_out_count: 5678     # Number of pages swapped out
  swap_in_bytes: 5046272   # Total bytes swapped in
  swap_out_bytes: 23265280 # Total bytes swapped out
  last_swap_in: 1716580130 # Timestamp of last swap-in
  last_swap_out: 1716580125
```

**Justification**: The daemon currently can only see VmSwap (current swap usage), not swap activity. Knowing swap-in/out rates per process would enable smarter decisions about which processes are actively thrashing.

#### Patch 2: PSI Threshold Triggers
```c
// Instead of polling /proc/pressure/memory every 200ms,
// register a callback when PSI exceeds a threshold

// New file: /proc/pressure/memory_threshold
// Write: "some 25000" (25% in microseconds per second)
// Poll/epoll: triggers when threshold exceeded
```

**Justification**: Polling PSI every 100-200ms at CRITICAL level wastes CPU. Kernel-side triggers would be zero-overhead until pressure actually occurs.

#### Patch 3: Proactive Swap-In
```c
// New sysctl: vm.proactive_swap_in
// When enabled and MemAvailable > threshold:
//   - Kernel proactively decompresses/swaps-in pages
//   - Prioritizes recently-used pages (based on LRU generation)
//   - Rate-limited to avoid flooding memory bus
```

**Justification**: This is the core problem — kernel swaps out pages, memory frees up, pages stay in swap. This patch would fix it at the source.

#### Patch 4: Desktop-Aware MGLRU Hints
```c
// New procfs: /proc/[pid]/lru_hint
// Values:
//   0 = default (kernel decides)
//   1 = prefer-resident (keep in RAM, avoid swapping)
//   2 = prefer-swap (okay to swap, low priority)
//
// The daemon writes hints, MGLRU respects them during page reclaim
```

**Justification**: MGLRU currently has no way to know which processes are interactive. These hints would let the userspace daemon inform the kernel's page reclaim decisions.

### 11.2 Kernel Submission Strategy

1. Start with Patch 2 (PSI triggers) — smallest, most likely to be accepted
2. Then Patch 1 (swap stats) — useful for monitoring tools beyond mgd
3. Then Patch 4 (LRU hints) — more controversial, needs benchmarks
4. Finally Patch 3 (proactive swap-in) — biggest change, needs extensive testing

Target mailing lists:
- `linux-mm@kvack.org` (memory management)
- `linux-kernel@vger.kernel.org` (general)
- `linux-api@vger.kernel.org` (new userspace APIs)

---

## 12. Phase 3: Full Kernel Integration

### 12.1 Long-Term Vision

Integrate priority-aware memory management directly into the kernel:

#### Modified OOM Killer (`mm/oom_kill.c`)
- Read priority from `/proc/[pid]/mem_priority` (new procfs entry)
- Kill lowest priority process first, not highest `oom_score`
- Support "checkpoint before kill" mode via new OOM policy

#### Modified Page Reclaim (`mm/vmscan.c`)
- MGLRU respects per-process priority for page aging
- High-priority process pages age slower (stay in hot generation longer)
- Low-priority process pages age faster (reclaimed first)

#### New Syscall
```c
// sys_mem_priority(pid_t pid, int priority, int flags)
// Sets memory management priority for a process
// Flags:
//   MEM_PRI_CHECKPOINT  - checkpoint before kill
//   MEM_PRI_NOSWAP      - never swap this process
//   MEM_PRI_PREFSWAP    - prefer to swap this process
```

### 12.2 Timeline Estimate

- Phase 1 (userspace): 3–6 months to v1.0
- Phase 2 (kernel patches): 6–12 months for acceptance
- Phase 3 (full integration): 2–5 years for mainline

---

## 13. Comparison with Existing Solutions

| Feature | Android lmkd | systemd-oomd | earlyoom | mgd (this project) |
|---------|-------------|--------------|----------|---------------------|
| PSI-based monitoring | Yes | Yes | No (RSS only) | Yes |
| Priority tiers | Yes (4 tiers) | No | No | Yes (6 tiers) |
| Compositor awareness | Yes (Activity Manager) | No | No | Yes (Wayland) |
| Process checkpoint | Yes (onSaveInstanceState) | No | No | Yes (CRIU) |
| Process restore | Yes (app lifecycle) | No | No | Yes |
| User notification | Yes (app closed toast) | No | No | Yes (D-Bus) |
| Dynamic focus priority | Yes | No | No | Yes |
| Config per app | Yes (manifest) | Partial (cgroups) | No | Yes (TOML + .desktop) |
| Kernel modifications | Yes (custom) | No | No | No (Phase 1) / Yes (Phase 2-3) |
| Desktop integration | N/A (is the desktop) | Minimal | None | Full (tray, notifications, CLI) |
| New process preemption | Yes | No | No | Yes (netlink proc connector) |

---

## 14. Testing Strategy

### 14.1 Unit Tests
- PSI parser with mock `/proc/pressure/memory` data
- Priority resolution (config + focus + recency + override)
- Decision engine (pressure level → action selection)
- RAM deficit calculator

### 14.2 Integration Tests
- Spawn test processes, set priorities, verify kill order
- CRIU checkpoint/restore cycle
- D-Bus notification send/receive
- Compositor focus detection

### 14.3 Stress Tests
- Simulate memory pressure with `stress-ng`
- Verify daemon prevents OOM on 8GB, 16GB systems
- Measure daemon's own memory/CPU overhead
- Test with real workloads: WebStorm + Firefox + Claude CLI

### 14.4 Acceptance Criteria
- System never freezes under any workload that would previously cause a freeze
- Daemon's own overhead: < 10MB RSS, < 1% CPU
- Action latency: < 500ms from threshold breach to first kill
- CRIU restore success rate: > 90% for supported apps
- No false positives: zero kills during normal (non-pressure) operation

---

## 15. Dependencies

### Runtime
- Linux kernel 6.1+ (PSI, MGLRU, cgroups v2)
- CRIU 3.17+ (checkpoint/restore)
- D-Bus (notifications)
- systemd (service management)

### Build
- Rust 1.75+ (async runtime, procfs parsing)
- Tokio (async I/O)
- zbus (D-Bus client)
- serde + toml (configuration)
- clap (CLI)
- tracing (structured logging)

### Optional
- KDE Frameworks (KWin D-Bus API)
- libnotify (notification fallback)

---

## 16. Open Questions

1. **Should the daemon run as root or user?** Root gives access to all processes and CRIU. User service is safer but limited. Recommendation: systemd user service with select capabilities (CAP_KILL, CAP_SYS_PTRACE).

2. **How to handle multi-user systems?** Each user runs their own daemon instance. System processes are managed by a root-level instance.

3. **Should frozen processes count toward memory pressure?** SIGSTOP doesn't free pages, it just stops CPU usage. The kernel can still reclaim frozen process pages. Need to test if this is effective.

4. **CRIU and Wayland**: Can Wayland clients be checkpointed? The Wayland socket connection will be lost. Need to test if apps reconnect on restore.

5. **Interaction with systemd-oomd**: Should mgd replace or coexist with systemd-oomd? Recommendation: replace, since mgd is a superset.

6. **Flatpak/Snap isolation**: Can mgd see and manage sandboxed processes? Need to verify cgroup visibility.

---

## 17. References

- Android lmkd source: `system/memory/lmkd/` in AOSP
- PSI documentation: `Documentation/accounting/psi.rst` in kernel tree
- MGLRU documentation: `Documentation/mm/multigen_lru.rst`
- CRIU documentation: https://criu.org/Main_Page
- cgroups v2 memory controller: `Documentation/admin-guide/cgroup-v2.rst`
- systemd-oomd: `man systemd-oomd`
- earlyoom: https://github.com/rfjakob/earlyoom
