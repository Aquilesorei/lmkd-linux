//! Unit newtypes shared across the daemon, plugins, and wire protocol.
//!
//! `#[serde(transparent)]` keeps both the plugin JSON wire format and the
//! persisted registry files byte-identical to the raw-integer encoding —
//! no migration of `~/.local/share/mgd/state/` is needed (proven by the
//! round-trip tests below).

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

/// Process identity. Not a number — no arithmetic on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Pid(pub u32);

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Delegate so width/alignment specifiers ({:<8}) keep working.
        self.0.fmt(f)
    }
}

/// Memory size in kibibytes — the only stored unit. MB/bytes exist solely as
/// conversion methods, never as fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Kb(pub u64);

impl Kb {
    pub fn mb(self) -> f64 {
        self.0 as f64 / 1024.0
    }

    pub fn bytes(self) -> u64 {
        self.0 * 1024
    }

    pub fn saturating_add(self, rhs: Kb) -> Kb {
        Kb(self.0.saturating_add(rhs.0))
    }

    pub fn saturating_sub(self, rhs: Kb) -> Kb {
        Kb(self.0.saturating_sub(rhs.0))
    }
}

impl Add for Kb {
    type Output = Kb;
    fn add(self, rhs: Kb) -> Kb {
        Kb(self.0 + rhs.0)
    }
}

impl Sub for Kb {
    type Output = Kb;
    fn sub(self, rhs: Kb) -> Kb {
        Kb(self.0 - rhs.0)
    }
}

impl Sum for Kb {
    fn sum<I: Iterator<Item = Kb>>(iter: I) -> Kb {
        Kb(iter.map(|k| k.0).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn pid_and_kb_serialize_as_bare_numbers() {
        assert_eq!(serde_json::to_string(&Pid(1234)).unwrap(), "1234");
        assert_eq!(serde_json::to_string(&Kb(2_048_000)).unwrap(), "2048000");
        assert_eq!(serde_json::from_str::<Pid>("1234").unwrap(), Pid(1234));
        assert_eq!(serde_json::from_str::<Kb>("2048000").unwrap(), Kb(2_048_000));
    }

    /// Frozen-registry shape: `HashMap<Pid, (name, frozen_at, start_time)>`
    /// wrapped in a struct, exactly as `~/.local/share/mgd/state/frozen.json`
    /// is written. The fixture string matches the pre-newtype on-disk format;
    /// the round trip must reproduce it byte-identically.
    #[test]
    fn frozen_registry_state_file_round_trips_unchanged() {
        #[derive(Serialize, Deserialize)]
        struct FrozenShape {
            frozen: HashMap<Pid, (String, u64, u64)>,
        }

        let fixture = r#"{"frozen":{"1234":["firefox",1751700000,54321]}}"#;
        let reg: FrozenShape = serde_json::from_str(fixture).unwrap();
        assert_eq!(
            reg.frozen.get(&Pid(1234)),
            Some(&("firefox".to_string(), 1_751_700_000, 54_321))
        );
        assert_eq!(serde_json::to_string(&reg).unwrap(), fixture);

        // The empty registry (current real state file content) also survives.
        let empty = r#"{"frozen":{}}"#;
        let reg: FrozenShape = serde_json::from_str(empty).unwrap();
        assert_eq!(serde_json::to_string(&reg).unwrap(), empty);
    }

    /// Checkpoint-registry shape: `HashMap<Pid, (name, dir, rss Kb, attempts)>`
    /// as written to `~/.local/share/mgd/state/checkpoint.json`.
    #[test]
    fn checkpoint_registry_state_file_round_trips_unchanged() {
        #[derive(Serialize, Deserialize)]
        struct CheckpointShape {
            checkpointed: HashMap<Pid, (String, std::path::PathBuf, Kb, u32)>,
        }

        let fixture =
            r#"{"checkpointed":{"999":["idea","/home/u/.local/share/mgd/snapshots/999_idea",2048000,1]}}"#;
        let reg: CheckpointShape = serde_json::from_str(fixture).unwrap();
        assert_eq!(reg.checkpointed.get(&Pid(999)).unwrap().2, Kb(2_048_000));
        assert_eq!(serde_json::to_string(&reg).unwrap(), fixture);
    }

    /// Plugin wire format: `Pid`/`Kb` fields in protocol messages must encode
    /// exactly as the raw integers did.
    #[test]
    fn plugin_wire_format_unchanged() {
        use crate::protocol::{CoreMessage, Metric, PluginMessage};

        let obs = PluginMessage::Observation {
            plugin: "mgd-gpu-intel".to_string(),
            metric: Metric::GpuResidentKb,
            pid: Some(Pid(42)),
            value: 1024.0,
        };
        assert_eq!(
            serde_json::to_string(&obs).unwrap(),
            r#"{"type":"observation","plugin":"mgd-gpu-intel","metric":"gpu_resident_kb","pid":42,"value":1024.0}"#
        );

        let gpu = CoreMessage::GpuObservation {
            pid: Pid(42),
            kb: Kb(2048),
            shared_kb: Kb(512),
            total_kb: Kb(4096),
            purgeable_kb: Kb(0),
        };
        assert_eq!(
            serde_json::to_string(&gpu).unwrap(),
            r#"{"type":"gpu_observation","pid":42,"kb":2048,"shared_kb":512,"total_kb":4096,"purgeable_kb":0}"#
        );

        // And the reverse: a pre-newtype message parses into the typed form.
        let parsed: PluginMessage = serde_json::from_str(
            r#"{"type":"query_gpu","pid":7}"#,
        ).unwrap();
        assert!(matches!(parsed, PluginMessage::QueryGpu { pid: Pid(7) }));
    }

    #[test]
    fn kb_conversions() {
        assert_eq!(Kb(2048).mb(), 2.0);
        assert_eq!(Kb(2).bytes(), 2048);
        assert_eq!(Kb(1) + Kb(2), Kb(3));
        assert_eq!(Kb(3) - Kb(1), Kb(2));
        assert_eq!(Kb(u64::MAX).saturating_add(Kb(1)), Kb(u64::MAX));
        assert_eq!(Kb(1).saturating_sub(Kb(2)), Kb(0));
        assert_eq!([Kb(1), Kb(2), Kb(3)].into_iter().sum::<Kb>(), Kb(6));
    }
}
