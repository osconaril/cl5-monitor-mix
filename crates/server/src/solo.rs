//! Mix-local solo bookkeeping. Only the bridge worker owns this state, so
//! console readback, scene recalls and phone requests are handled in order.
use std::collections::{BTreeSet, HashMap, VecDeque};

use serde::Serialize;

use crate::console::LEVEL;

pub const OFF: i16 = -32768;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SoloStatus {
    pub owner: String,
    pub channels: BTreeSet<u8>,
}

struct ActiveSolo {
    status: SoloStatus,
    saved: Vec<i16>,
    scene_number: Option<u16>,
}

#[derive(Default)]
pub struct Solos {
    active: HashMap<u8, ActiveSolo>,
    // A console echoes our own writes. Those echoes must not replace the
    // pre-solo level with Off; other values are engineer/phone changes.
    expected: HashMap<(u8, u8), VecDeque<i16>>,
    // Restore writes are deliberately retained across a dropped reconnect. A
    // generic RCP `OK` is the acknowledgement for one restore command.
    restore_acks_remaining: usize,
}

impl Solos {
    pub fn statuses(&self) -> HashMap<u8, SoloStatus> {
        self.active
            .iter()
            .map(|(&mix, solo)| (mix, solo.status.clone()))
            .collect()
    }

    pub fn observe(&mut self, input: u8, mix: u8, value: i16) {
        let expected = self.expected.entry((input, mix)).or_default();
        if expected.front() == Some(&value) {
            expected.pop_front();
            return;
        }
        if let Some(solo) = self.active.get_mut(&mix) {
            solo.saved[input as usize] = value;
        }
    }

    pub fn pending(&self, mix: u8) -> bool {
        self.expected
            .iter()
            .any(|((_, m), values)| *m == mix && !values.is_empty())
    }

    pub fn restoring(&self) -> bool {
        self.restore_acks_remaining > 0
    }

    /// A dropped connection cannot deliver any remaining restore acknowledgements.
    /// Forget that connection's in-flight count so the next hydrated connection
    /// can determine which sends still need restoring.
    pub fn reconnect_after_drop(&mut self) {
        if self.restoring() {
            self.expected.clear();
            self.restore_acks_remaining = 0;
        }
    }

    /// Records an RCP acknowledgement for a restore command. Returns true
    /// exactly once, when the last restore command has been acknowledged.
    pub fn acknowledge_restore(&mut self) -> bool {
        if self.restore_acks_remaining == 0 {
            return false;
        }
        self.restore_acks_remaining -= 1;
        if self.restore_acks_remaining == 0 {
            self.active.clear();
            self.expected.clear();
            return true;
        }
        false
    }

    /// A hydrated desk may already have every saved level. There is then no
    /// command to acknowledge, but the recovery is still complete.
    pub fn complete_restore_without_writes(&mut self) {
        debug_assert_eq!(self.restore_acks_remaining, 0);
        self.active.clear();
        self.expected.clear();
    }

    /// Returns exact wire levels, including fractional levels on restoration.
    /// The browser never supplies these values or a raw RCP command.
    pub fn set(
        &mut self,
        mix: u8,
        owner: &str,
        channels: BTreeSet<u8>,
        values: &HashMap<String, String>,
        scene_number: Option<u16>,
    ) -> Result<Vec<(u8, i16)>, &'static str> {
        if mix >= 24 || channels.iter().any(|input| *input >= 80) {
            return Err("That channel or mix is not available.");
        }
        if self.pending(mix) {
            return Err("Solo is still updating. Try again in a moment.");
        }
        if let Some(solo) = self.active.get(&mix) {
            if solo.status.owner != owner {
                return Err("Solo is controlled by the performer who started it.");
            }
        } else if channels.is_empty() {
            return Ok(Vec::new());
        } else {
            let saved: Option<Vec<i16>> = (0..80)
                .map(|input| {
                    values
                        .get(&format!("{LEVEL} {input} {mix}"))?
                        .parse::<i16>()
                        .ok()
                        .filter(|v| *v == OFF || (-13800..=1000).contains(v))
                })
                .collect();
            self.active.insert(
                mix,
                ActiveSolo {
                    status: SoloStatus {
                        owner: owner.to_owned(),
                        channels: (0..80).collect(),
                    },
                    saved: saved
                        .ok_or("Mix levels are unavailable. Wait for the console to reconnect.")?,
                    scene_number,
                },
            );
        }
        let solo = self.active.get_mut(&mix).expect("active solo");
        let mut changes = Vec::new();
        for input in 0..80 {
            let was_audible = solo.status.channels.contains(&input);
            let audible = channels.is_empty() || channels.contains(&input);
            if was_audible != audible {
                let value = if audible {
                    solo.saved[input as usize]
                } else {
                    OFF
                };
                changes.push((input, value));
                self.expected
                    .entry((input, mix))
                    .or_default()
                    .push_back(value);
            }
        }
        solo.status.channels = channels;
        if solo.status.channels.is_empty() {
            self.active.remove(&mix);
        }
        Ok(changes)
    }

    /// Completes active solos after a new connection has hydrated its current
    /// console state. A desk-side change wins unless the channel is still Off;
    /// a recalled scene never receives levels captured from the old scene.
    pub fn restore_after_reconnect(
        &mut self,
        values: &HashMap<String, String>,
        scene_number: Option<u16>,
    ) -> (Vec<(u8, u8, i16)>, bool) {
        // A connection can drop partway through a prior restore. Its queued
        // acknowledgements belong to that dead connection, so hydrate and
        // recompute from the desk again rather than treating them as complete.
        self.expected.clear();
        self.restore_acks_remaining = 0;
        let mut writes = Vec::new();
        let mut scene_changed = false;
        for (&mix, solo) in &self.active {
            if solo.scene_number != scene_number {
                scene_changed = true;
                continue;
            }
            for input in 0..80 {
                let current = values
                    .get(&format!("{LEVEL} {input} {mix}"))
                    .and_then(|value| value.parse::<i16>().ok());
                if current == Some(OFF) {
                    writes.push((input, mix, solo.saved[input as usize]));
                }
            }
        }
        if scene_changed {
            self.active.clear();
        } else {
            self.restore_acks_remaining = writes.len();
        }
        (writes, scene_changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solo_restores_fractional_levels_and_keeps_engineer_changes() {
        let values = (0..80)
            .map(|input| (format!("{LEVEL} {input} 0"), "-350".into()))
            .collect();
        let mut solos = Solos::default();
        let writes = solos
            .set(0, "Singer", [0].into(), &values, Some(1))
            .unwrap();
        assert_eq!(writes.len(), 79);
        assert!(writes
            .iter()
            .all(|(input, value)| *input != 0 && *value == OFF));
        assert!(solos
            .set(0, "Singer", BTreeSet::new(), &values, Some(1))
            .is_err());
        for (input, value) in writes {
            solos.observe(input, 0, value);
        }
        solos.observe(1, 0, -725);
        solos.observe(0, 0, -125);
        assert!(solos.set(0, "Other", [2].into(), &values, Some(1)).is_err());
        let writes = solos
            .set(0, "Singer", [0, 2].into(), &values, Some(1))
            .unwrap();
        assert_eq!(writes, vec![(2, -350)]);
        solos.observe(2, 0, -350);
        let writes = solos
            .set(0, "Singer", BTreeSet::new(), &values, Some(1))
            .unwrap();
        assert_eq!(writes.len(), 78);
        assert!(writes.contains(&(1, -725)));
        assert!(writes.contains(&(3, -350)));
        assert!(!writes.iter().any(|(input, _)| *input == 0 || *input == 2));
        assert!(solos.statuses().is_empty());
    }

    #[test]
    fn unavailable_levels_and_invalid_channels_do_not_start_solo() {
        let mut solos = Solos::default();
        for (mix, channels) in [(24, [0].into()), (0, [80].into()), (0, [0].into())] {
            assert!(solos
                .set(mix, "Singer", channels, &HashMap::new(), Some(1))
                .is_err());
            assert!(solos.statuses().is_empty());
        }
    }

    #[test]
    fn reconnect_restores_only_off_channels_and_discards_a_changed_scene() {
        let values = (0..80)
            .map(|input| (format!("{LEVEL} {input} 0"), "-350".into()))
            .collect();
        let mut solos = Solos::default();
        solos
            .set(0, "Singer", [0].into(), &values, Some(1))
            .unwrap();
        let current = (0..80)
            .map(|input| {
                (
                    format!("{LEVEL} {input} 0"),
                    if input == 1 {
                        "-625".into()
                    } else {
                        OFF.to_string()
                    },
                )
            })
            .collect();
        let (writes, scene_changed) = solos.restore_after_reconnect(&current, Some(1));
        assert!(!scene_changed);
        assert!(writes.contains(&(2, 0, -350)));
        assert!(!writes.iter().any(|(input, _, _)| *input == 1));

        solos
            .set(0, "Singer", [0].into(), &values, Some(1))
            .unwrap();
        let (writes, scene_changed) = solos.restore_after_reconnect(&current, Some(2));
        assert!(writes.is_empty());
        assert!(scene_changed);
    }
}
