//! Plugin ↔ Core IPC protocol types.
//!
//! Plugins connect to the mgd Unix socket, identify themselves, receive
//! `PressureChanged` broadcasts, and send observations / action requests.
//! Core makes all kill/freeze decisions — plugins are observers only.

use serde::{Deserialize, Serialize};

// ── Plugin → Core ─────────────────────────────────────────────────────────────

/// Messages that a plugin sends to the core daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    /// First message after connect: plugin introduces itself.
    Identify {
        name: String,
        version: String,
        capabilities: Vec<String>,
    },

    /// A new measurement the plugin wants core to factor into decisions.
    Observation {
        plugin: String,
        metric: Metric,
        pid: Option<u32>,
        value: f64,
    },

    /// Plugin requests that core take an action (core decides whether to approve).
    ActionRequest {
        plugin: String,
        action: PluginAction,
        reason: String,
    },

    /// Request the latest cached GPU footprint for a specific PID.
    QueryGpu {
        pid: u32,
    },

    /// Active window change reported by a desktop watcher plugin.
    ActiveWindow {
        pid: Option<u32>,
    },
}

/// Typed metric kinds a plugin can report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// GPU memory currently resident in system RAM (includes shared), KB.
    GpuResidentKb,
    /// Imported dma-buf KB also counted by other clients — subtract from resident for true pressure.
    GpuSharedKb,
    /// All GEM BOs the client has handles to (resident + non-resident + shared overhead), KB.
    GpuTotalKb,
    /// Purgeable KB — shrinker free path, no migration needed.
    GpuPurgeableKb,
    /// Process RSS, KB (alternative source, e.g. cgroup accounting).
    RssKb,
    /// Custom metric — name identifies the plugin-specific meaning.
    Custom { name: String },
}

/// Actions a plugin may request from core.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginAction {
    /// Restart a named DE process (e.g. plasmashell).
    RestartProcess { name: String },
    /// Suggest freezing a specific PID.
    FreezePid { pid: u32 },
    /// Suggest killing a specific PID.
    KillPid { pid: u32 },
}

// ── Core → Plugin ─────────────────────────────────────────────────────────────

/// Messages the core daemon broadcasts to connected plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreMessage {
    /// Sent whenever the effective pressure level changes.
    PressureChanged {
        /// New level name: `"normal"`, `"elevated"`, `"high"`, `"critical"`, `"emergency"`.
        level: String,
    },

    /// Response to a plugin's `ActionRequest`.
    ActionResponse {
        /// The action being responded to.
        action: PluginAction,
        /// Whether core will perform the action.
        approved: bool,
        /// Reason for denial, if not approved.
        reason: Option<String>,
    },

    /// Response to `QueryGpu`.
    GpuObservation {
        pid: u32,
        /// GPU resident KB (includes shared).
        kb: u64,
        /// Shared/imported dma-buf KB (double-counted in resident).
        shared_kb: u64,
        /// Total allocated KB (diagnostic only).
        total_kb: u64,
        /// Purgeable KB (shrinker free path).
        purgeable_kb: u64,
    },

    /// Sent by core immediately before it exits. Plugins should disconnect.
    Shutdown,

    /// Sent after the daemon reloads its config (SIGHUP or `mgctl reload`).
    /// Plugins should discard cached config and re-read from disk.
    ConfigReload,
}
