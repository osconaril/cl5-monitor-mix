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
}

#[derive(Default)]
pub struct Solos {
    active: HashMap<u8, ActiveSolo>,
    // A console echoes our own writes. Those echoes must not replace the
    // pre-solo level with Off; other values are engineer/phone changes.
    expected: HashMap<(u8, u8), VecDeque<i16>>,
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

    /// Returns exact wire levels, including fractional levels on restoration.
    /// The browser never supplies these values or a raw RCP command.
    pub fn set(
        &mut self,
        mix: u8,
        owner: &str,
        channels: BTreeSet<u8>,
        values: &HashMap<String, String>,
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
        let writes = solos.set(0, "Singer", [0].into(), &values).unwrap();
        assert_eq!(writes.len(), 79);
        assert!(writes
            .iter()
            .all(|(input, value)| *input != 0 && *value == OFF));
        assert!(solos.set(0, "Singer", BTreeSet::new(), &values).is_err());
        for (input, value) in writes {
            solos.observe(input, 0, value);
        }
        solos.observe(1, 0, -725);
        solos.observe(0, 0, -125);
        assert!(solos.set(0, "Other", [2].into(), &values).is_err());
        let writes = solos.set(0, "Singer", [0, 2].into(), &values).unwrap();
        assert_eq!(writes, vec![(2, -350)]);
        solos.observe(2, 0, -350);
        let writes = solos.set(0, "Singer", BTreeSet::new(), &values).unwrap();
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
            assert!(solos.set(mix, "Singer", channels, &HashMap::new()).is_err());
            assert!(solos.statuses().is_empty());
        }
    }
}
