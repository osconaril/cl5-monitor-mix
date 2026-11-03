//! The deliberately small, safe command surface between phones and the CL5.
//!
//! Keeping this parser separate from HTTP/WebSocket handling makes it impossible
//! for a browser request to become an arbitrary RCP command.

use serde::{Deserialize, Serialize};

pub const LEVEL: &str = "MIXER:Current/InCh/ToMix/Level";
pub const ON: &str = "MIXER:Current/InCh/ToMix/On";
pub const PAN: &str = "MIXER:Current/InCh/ToMix/Pan";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PhoneChange {
    pub path: String,
    pub input: u8,
    pub mix: u8,
    pub value: String,
}

impl PhoneChange {
    /// Validates both the protocol allow-list and the performer-safe value
    /// domain before anything is placed on the console write queue.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(self.path.as_str(), LEVEL | ON | PAN) {
            return Err("That control is not available from Monitor Mix.");
        }
        if self.input >= 80 || self.mix >= 24 {
            return Err("That channel or mix is not available.");
        }
        match self.path.as_str() {
            LEVEL if self.value == "Off" => Ok(()),
            LEVEL => {
                let db = self
                    .value
                    .parse::<i16>()
                    .map_err(|_| "Send level must be a whole dB value or Off.")?;
                if (-138..=10).contains(&db) && self.value == db.to_string() {
                    Ok(())
                } else {
                    Err("Send level must be a whole dB value or Off.")
                }
            }
            ON if matches!(self.value.as_str(), "0" | "1") => Ok(()),
            PAN if self.value.parse::<i16>().is_ok() => Ok(()),
            _ => Err("That value is not valid for this control."),
        }
    }

    /// Produces only an allow-listed RCP `set` command. `Off` is the protocol's
    /// -infinity sentinel; the browser never gets to select it directly.
    pub fn rcp(&self) -> Result<String, &'static str> {
        self.validate()?;
        let value = if self.path == LEVEL && self.value == "Off" {
            "-32768"
        } else {
            &self.value
        };
        Ok(format!(
            "set {} {} {} {value}",
            self.path, self.input, self.mix
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(value: &str) -> PhoneChange {
        PhoneChange {
            path: LEVEL.into(),
            input: 0,
            mix: 0,
            value: value.into(),
        }
    }

    #[test]
    fn only_documented_phone_controls_reach_rcp() {
        let change = PhoneChange {
            path: "MIXER:Current/Scene/Recall".into(),
            input: 0,
            mix: 0,
            value: "1".into(),
        };
        assert_eq!(
            change.rcp(),
            Err("That control is not available from Monitor Mix.")
        );
    }

    #[test]
    fn phone_levels_are_whole_db_or_off() {
        assert_eq!(level("-3").rcp().unwrap(), format!("set {LEVEL} 0 0 -3"));
        assert_eq!(
            level("Off").rcp().unwrap(),
            format!("set {LEVEL} 0 0 -32768")
        );
        assert_eq!(
            level("-3.5").validate(),
            Err("Send level must be a whole dB value or Off.")
        );
    }
}
