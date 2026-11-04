//! The deliberately small, safe command surface between phones and the CL5.
//!
//! Keeping this parser separate from HTTP/WebSocket handling makes it impossible
//! for a browser request to become an arbitrary RCP command.

use crate::solo::{SoloStatus, Solos};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, mpsc, oneshot, watch, RwLock},
    time::{interval, sleep},
};

pub const LEVEL: &str = "MIXER:Current/InCh/ToMix/Level";
pub const ON: &str = "MIXER:Current/InCh/ToMix/On";
pub const PAN: &str = "MIXER:Current/InCh/ToMix/Pan";
const INPUT_NAME: &str = "MIXER:Current/InCh/Label/Name";
const INPUT_COLOR: &str = "MIXER:Current/InCh/Label/Color";
const MIX_NAME: &str = "MIXER:Current/Mix/Label/Name";
const MIX_ROLE: &str = "MIXER:Current/Mix/Role";
const MIX_BUS_TYPE: &str = "MIXER:Current/Mix/BusType";
const SCENE: &str = "MIXER:Lib/Scene";

/// The complete read-only picture used by the screens. Values intentionally
/// retain Yamaha's wire representation: engineer-set fractional levels must
/// arrive in browsers unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConsoleState {
    pub values: HashMap<String, String>,
    pub scene_number: Option<u16>,
    pub scene_name: Option<String>,
    pub solos: HashMap<u8, SoloStatus>,
    pub solo_notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum BridgeEvent {
    SoloChanged {
        solos: HashMap<u8, SoloStatus>,
        notice: Option<String>,
    },
    Connected,
    Disconnected,
    Value {
        key: String,
        value: String,
    },
    SceneRecalled {
        number: u16,
        name: Option<String>,
    },
}

/// One server-owned RCP connection. It is deliberately independent from the
/// HTTP layer so WebSockets and future routes can only subscribe or submit a
/// validated `PhoneChange`; neither receives a TCP stream.
#[derive(Clone)]
pub struct ConsoleBridge {
    state: Arc<RwLock<ConsoleState>>,
    connected: watch::Receiver<bool>,
    events: broadcast::Sender<BridgeEvent>,
    writes: mpsc::Sender<BridgeCommand>,
}

enum BridgeCommand {
    Change(PhoneChange),
    Group {
        mix: u8,
        channels: Vec<u8>,
        delta: i16,
        reply: oneshot::Sender<Result<(), &'static str>>,
    },
    Solo {
        mix: u8,
        owner: String,
        channels: BTreeSet<u8>,
        reply: oneshot::Sender<Result<(), &'static str>>,
    },
}

impl ConsoleBridge {
    /// A bridge used before the server's TCP worker has been configured.  It
    /// remains safely disconnected, so a route can never accidentally accept
    /// a phone command without a real console connection.
    pub fn offline() -> Self {
        let state = Arc::new(RwLock::new(ConsoleState::default()));
        let (_connected_tx, connected) = watch::channel(false);
        let (events, _) = broadcast::channel(512);
        let (writes, _receiver) = mpsc::channel(1);
        Self {
            state,
            connected,
            events,
            writes,
        }
    }

    pub fn start(address: impl Into<String>) -> Self {
        Self::start_with_event_capacity(address.into(), 512)
    }

    fn start_with_event_capacity(address: String, event_capacity: usize) -> Self {
        let state = Arc::new(RwLock::new(ConsoleState::default()));
        let (connected_tx, connected) = watch::channel(false);
        let (events, _) = broadcast::channel(event_capacity);
        let (writes, receiver) = mpsc::channel(512);
        tokio::spawn(run_bridge(
            address,
            state.clone(),
            connected_tx,
            events.clone(),
            receiver,
        ));
        Self {
            state,
            connected,
            events,
            writes,
        }
    }

    #[cfg(test)]
    pub fn start_for_lag_test(address: impl Into<String>) -> Self {
        Self::start_with_event_capacity(address.into(), 4)
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<BridgeEvent> {
        self.events.subscribe()
    }
    pub async fn state(&self) -> ConsoleState {
        self.state.read().await.clone()
    }

    pub async fn solo(
        &self,
        mix: u8,
        owner: String,
        channels: BTreeSet<u8>,
    ) -> Result<(), &'static str> {
        if !self.is_connected() {
            return Err("Console disconnected — reconnecting.");
        }
        let (reply, result) = oneshot::channel();
        self.writes
            .send(BridgeCommand::Solo {
                mix,
                owner,
                channels,
                reply,
            })
            .await
            .map_err(|_| "Console disconnected — reconnecting.")?;
        result
            .await
            .map_err(|_| "Console disconnected — reconnecting.")?
    }

    pub async fn group(&self, mix: u8, channels: Vec<u8>, delta: i16) -> Result<(), &'static str> {
        if !self.is_connected() {
            return Err("Console disconnected — reconnecting.");
        }
        let (reply, result) = oneshot::channel();
        self.writes
            .send(BridgeCommand::Group {
                mix,
                channels,
                delta,
                reply,
            })
            .await
            .map_err(|_| "Console disconnected — reconnecting.")?;
        result
            .await
            .map_err(|_| "Console disconnected — reconnecting.")?
    }

    /// Validates before queuing, and never retains a command while offline.
    pub async fn change(&self, change: PhoneChange) -> Result<(), &'static str> {
        change.validate()?;
        if !self.is_connected() {
            return Err("Console disconnected — reconnecting.");
        }
        if change.path == PAN && !stereo_mix(&self.state.read().await.values, change.mix) {
            return Err("Pan is available only on a stereo-linked mix.");
        }
        self.writes
            .send(BridgeCommand::Change(change))
            .await
            .map_err(|_| "Console disconnected — reconnecting.")
    }
}

async fn run_bridge(
    address: String,
    state: Arc<RwLock<ConsoleState>>,
    connected: watch::Sender<bool>,
    events: broadcast::Sender<BridgeEvent>,
    mut writes: mpsc::Receiver<BridgeCommand>,
) {
    let mut retry = Duration::from_millis(100);
    loop {
        let _ = connected.send(false);
        if !state.read().await.solos.is_empty() {
            publish_solos(
                &Solos::default(),
                Some("Console disconnected — solo cleared. Check your mix levels.".into()),
                &state,
                &events,
            )
            .await;
        }
        // Commands accepted by the old connection must never replay after a reconnect.
        while writes.try_recv().is_ok() {}
        let _ = events.send(BridgeEvent::Disconnected);
        if let Ok(stream) = TcpStream::connect(&address).await {
            retry = Duration::from_millis(100);
            let clean = serve_connection(stream, &state, &connected, &events, &mut writes).await;
            let _ = connected.send(false);
            if clean {
                let _ = events.send(BridgeEvent::Disconnected);
            }
        }
        sleep(retry).await;
        retry = (retry * 2).min(Duration::from_secs(5));
    }
}

async fn serve_connection(
    stream: TcpStream,
    state: &Arc<RwLock<ConsoleState>>,
    connected: &watch::Sender<bool>,
    events: &broadcast::Sender<BridgeEvent>,
    writes: &mut mpsc::Receiver<BridgeCommand>,
) -> bool {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let snapshot = snapshot_commands();
    for command in &snapshot {
        if write.write_all(command.as_bytes()).await.is_err()
            || write.write_all(b"\n").await.is_err()
        {
            return false;
        }
    }
    let mut in_flight: HashMap<String, VecDeque<String>> = HashMap::new();
    let mut solos = Solos::default();
    let mut outstanding = snapshot.len();
    let mut pending = HashMap::<String, PhoneChange>::new();
    let mut flush = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if line.starts_with("OK ") { outstanding = outstanding.saturating_sub(1); }
                    if let Some(number) = scene_number_reply(&line) {
                        let command = format!("ssinfo_ex {SCENE} {number}\n");
                        if write.write_all(command.as_bytes()).await.is_err() { return false; }
                        outstanding += 1;
                    }
                    if is_scene_recall(&line) {
                        pending.clear();
                        in_flight.clear();
                        while writes.try_recv().is_ok() {}
                        let had_solo = !solos.statuses().is_empty();
                        solos = Solos::default();
                        publish_solos(&solos, had_solo.then(|| "Scene changed — solo cleared".into()), state, events).await;
                        let _ = connected.send(false);
                        for command in snapshot_commands() {
                            if write.write_all(command.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() { return false; }
                            outstanding += 1;
                        }
                    }
                    if let Some((path, input, mix, value)) = value_reply(line.strip_prefix("NOTIFY ").or_else(|| line.strip_prefix("OK ")).unwrap_or(&line)) {
                        let key = format!("{path} {input} {mix}");
                        if let Some(queue) = in_flight.get_mut(&key) {
                            if queue.front().is_some_and(|sent| sent == value) { queue.pop_front(); }
                        }
                        if path == LEVEL {
                            if let (Ok(input), Ok(mix), Ok(value)) = (input.parse::<u8>(), mix.parse::<u8>(), value.parse::<i16>()) {
                                if input < 80 && mix < 24 { solos.observe(input, mix, value); }
                            }
                        }
                    }
                    apply_line(&line, state, events).await;
                    if outstanding == 0 && !*connected.borrow() {
                        let _ = connected.send(true);
                        let _ = events.send(BridgeEvent::Connected);
                    }
                }
                _ => return true,
            },
            change = writes.recv() => match change {
                Some(BridgeCommand::Change(change)) => {
                    if *connected.borrow() { pending.insert(change_key(&change), change); }
                }
                Some(BridgeCommand::Group { mix, channels, delta, reply }) => {
                    if !*connected.borrow() {
                        let _ = reply.send(Err("Console disconnected — reconnecting."));
                        continue;
                    }
                    if !pending.is_empty() || in_flight.values().any(|queue| !queue.is_empty()) || solos.pending(mix) {
                        let _ = reply.send(Err("Mix levels are still updating. Try again in a moment."));
                        continue;
                    }
                    let values = state.read().await.values.clone();
                    match group_levels(mix, &channels, delta, &values) {
                        Err(error) => { let _ = reply.send(Err(error)); }
                        Ok(changes) => {
                            for (input, value) in changes {
                                let key = format!("{LEVEL} {input} {mix}");
                                in_flight.entry(key).or_default().push_back(value.to_string());
                                let command = format!("set {LEVEL} {input} {mix} {value}\n");
                                if write.write_all(command.as_bytes()).await.is_err() { return false; }
                            }
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
                Some(BridgeCommand::Solo { mix, owner, channels, reply }) => {
                    if !*connected.borrow() {
                        let _ = reply.send(Err("Console disconnected — reconnecting."));
                        continue;
                    }
                    if !pending.is_empty() || in_flight.values().any(|queue| !queue.is_empty()) {
                        let _ = reply.send(Err("Mix levels are still updating. Try again in a moment."));
                        continue;
                    }
                    let values = state.read().await.values.clone();
                    match solos.set(mix, &owner, channels, &values) {
                        Err(error) => { let _ = reply.send(Err(error)); }
                        Ok(changes) => {
                            for (input, value) in changes {
                                let command = format!("set {LEVEL} {input} {mix} {value}\n");
                                if write.write_all(command.as_bytes()).await.is_err() { return false; }
                            }
                            publish_solos(&solos, None, state, events).await;
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
                None => return false,
            },
            _ = flush.tick(), if outstanding == 0 && !pending.is_empty() => {
                for (_, change) in std::mem::take(&mut pending) {
                    if change.path == PAN && !stereo_mix(&state.read().await.values, change.mix) { continue; }
                    let Ok(command) = change.rcp() else { continue };
                    in_flight.entry(change_key(&change)).or_default().push_back(command.rsplit(' ').next().expect("command value").to_owned());
                    if write.write_all(command.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() { return false; }
                }
            }
        }
    }
}

fn stereo_mix(values: &HashMap<String, String>, mix: u8) -> bool {
    values
        .get(&format!("{MIX_ROLE} {mix} 0"))
        .is_some_and(|role| matches!(role.trim_matches('"'), "StereoL" | "StereoR"))
}

/// Apply a whole-dB delta to console readback, preserving fractional offsets.
/// Off is negative infinity, so adding a delta leaves it Off. Saturation at
/// either endpoint is the only case where the members' balance can change.
fn group_levels(
    mix: u8,
    channels: &[u8],
    delta: i16,
    values: &HashMap<String, String>,
) -> Result<Vec<(u8, i16)>, &'static str> {
    if mix >= 24
        || channels.is_empty()
        || channels.iter().any(|input| *input >= 80)
        || channels.iter().collect::<BTreeSet<_>>().len() != channels.len()
    {
        return Err("That group or mix is not available.");
    }
    if !(-149..=149).contains(&delta) {
        return Err("Group movement must be a whole dB from -149 to 149.");
    }
    let mut changes = Vec::new();
    for &input in channels {
        let old = values
            .get(&format!("{LEVEL} {input} {mix}"))
            .and_then(|value| value.parse::<i16>().ok())
            .filter(|value| *value == crate::solo::OFF || (-13800..=1000).contains(value))
            .ok_or("Group levels are unavailable. Wait for the console to reconnect.")?;
        let shifted = i32::from(old) + i32::from(delta) * 100;
        let value = if old == crate::solo::OFF || shifted < -13800 {
            crate::solo::OFF
        } else {
            shifted.min(1000) as i16
        };
        if value != old {
            changes.push((input, value));
        }
    }
    Ok(changes)
}

async fn publish_solos(
    solos: &Solos,
    notice: Option<String>,
    state: &Arc<RwLock<ConsoleState>>,
    events: &broadcast::Sender<BridgeEvent>,
) {
    let statuses = solos.statuses();
    let mut state = state.write().await;
    state.solos = statuses.clone();
    state.solo_notice = notice.clone();
    let _ = events.send(BridgeEvent::SoloChanged {
        solos: statuses,
        notice,
    });
}

fn change_key(change: &PhoneChange) -> String {
    format!("{} {} {}", change.path, change.input, change.mix)
}

fn scene_number_reply(line: &str) -> Option<u16> {
    line.strip_prefix("OK sscurrent_ex MIXER:Lib/Scene ")?
        .parse()
        .ok()
}

fn is_scene_recall(line: &str) -> bool {
    line.starts_with("NOTIFY ssrecall_ex MIXER:Lib/Scene ")
}

fn snapshot_commands() -> Vec<String> {
    let mut commands = Vec::with_capacity(5_812);
    for input in 0..80 {
        for mix in 0..24 {
            for path in [LEVEL, ON, PAN] {
                commands.push(format!("get {path} {input} {mix}"));
            }
        }
        for path in [INPUT_NAME, INPUT_COLOR] {
            commands.push(format!("get {path} {input} 0"));
        }
    }
    for mix in 0..24 {
        for path in [MIX_NAME, MIX_BUS_TYPE, MIX_ROLE] {
            commands.push(format!("get {path} {mix} 0"));
        }
    }
    commands.push(format!("sscurrent_ex {SCENE}"));
    commands
}

async fn apply_line(
    line: &str,
    state: &Arc<RwLock<ConsoleState>>,
    events: &broadcast::Sender<BridgeEvent>,
) {
    let text = line
        .strip_prefix("NOTIFY ")
        .or_else(|| line.strip_prefix("OK "))
        .unwrap_or(line);
    if let Some((path, first, second, value)) = value_reply(text) {
        let key = format!("{path} {first} {second}");
        state
            .write()
            .await
            .values
            .insert(key.clone(), value.to_owned());
        let _ = events.send(BridgeEvent::Value {
            key,
            value: value.to_owned(),
        });
        return;
    }
    let words: Vec<_> = text.split_whitespace().collect();
    if words.len() >= 3 && words[0] == "sscurrent_ex" && words[1] == SCENE {
        if let Ok(number) = words[2].parse() {
            state.write().await.scene_number = Some(number);
        }
    }
    if words.len() >= 3 && words[0] == "ssrecall_ex" && words[1] == SCENE {
        if let Ok(number) = words[2].parse() {
            state.write().await.scene_number = Some(number);
        }
    }
    if words.len() >= 4 && words[0] == "ssinfo_ex" && words[1] == SCENE {
        if let Ok(number) = words[2].parse() {
            let name = words[3..].join(" ").trim_matches('"').to_owned();
            state.write().await.scene_name = Some(name.clone());
            let _ = events.send(BridgeEvent::SceneRecalled {
                number,
                name: Some(name),
            });
        }
    }
}

/// Parses an RCP get/set reply while preserving the complete wire value.  Label
/// values are quoted and may contain spaces, so treating the reply as exactly
/// five whitespace-delimited words would silently discard them.
fn value_reply(text: &str) -> Option<(&str, &str, &str, &str)> {
    fn next_word(text: &str) -> Option<(&str, &str)> {
        let text = text.trim_start();
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        (!text[..end].is_empty()).then_some((&text[..end], &text[end..]))
    }

    let (operation, text) = next_word(text)?;
    if !matches!(operation, "set" | "get") {
        return None;
    }
    let (path, text) = next_word(text)?;
    let (first, text) = next_word(text)?;
    let (second, value) = next_word(text)?;
    let value = value.trim_start();
    (!value.is_empty()).then_some((path, first, second, value))
}

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
            PAN if self
                .value
                .parse::<i16>()
                .is_ok_and(|v| (-63..=63).contains(&v) && self.value == v.to_string()) =>
            {
                Ok(())
            }
            _ => Err("That value is not valid for this control."),
        }
    }

    /// Produces only an allow-listed RCP `set` command. `Off` is the protocol's
    /// -infinity sentinel; the browser never gets to select it directly.
    pub fn rcp(&self) -> Result<String, &'static str> {
        self.validate()?;
        let value = if self.path == LEVEL && self.value == "Off" {
            "-32768".to_owned()
        } else if self.path == LEVEL {
            (self.value.parse::<i16>().expect("validated whole dB") * 100).to_string()
        } else {
            self.value.clone()
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
    use fake_console::FakeConsole;
    use std::sync::Arc;
    use tokio::time::{sleep, timeout};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpStream,
    };

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
        assert_eq!(level("-3").rcp().unwrap(), format!("set {LEVEL} 0 0 -300"));
        assert_eq!(
            level("Off").rcp().unwrap(),
            format!("set {LEVEL} 0 0 -32768")
        );
        assert_eq!(
            level("-3.5").validate(),
            Err("Send level must be a whole dB value or Off.")
        );
    }

    #[test]
    fn whole_db_phone_levels_use_the_consoles_hundredth_db_units() {
        for db in -138..=10 {
            assert_eq!(
                level(&db.to_string()).rcp().unwrap(),
                format!("set {LEVEL} 0 0 {}", db * 100)
            );
        }
        for invalid in ["-139", "11", "-3.5", "NaN", "1000"] {
            assert!(level(invalid).rcp().is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn engineer_fractional_level_survives_hydration_and_a_phone_move_uses_db_units() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let engineer = TcpStream::connect(address).await.unwrap();
        let mut engineer = BufReader::new(engineer);
        engineer
            .get_mut()
            .write_all(format!("set {LEVEL} 0 0 -350\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        engineer.read_line(&mut reply).await.unwrap();
        assert_eq!(reply.trim(), "OK");
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        let key = format!("{LEVEL} 0 0");
        assert_eq!(
            bridge.state().await.values.get(&key).map(String::as_str),
            Some("-350")
        );
        bridge.change(level("-3")).await.unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                if bridge.state().await.values.get(&key).map(String::as_str) == Some("-300") {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the console reports minus three dB, not minus 0.03 dB");
        engineer
            .get_mut()
            .write_all(format!("get {LEVEL} 0 0\n").as_bytes())
            .await
            .unwrap();
        loop {
            reply.clear();
            engineer.read_line(&mut reply).await.unwrap();
            if reply.starts_with("OK get") {
                break;
            }
        }
        assert_eq!(reply.trim(), format!("OK get {LEVEL} 0 0 -300"));
    }

    #[tokio::test]
    async fn pan_requires_stereo_readback_and_keeps_other_sends_unchanged() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        let mut change = PhoneChange {
            path: PAN.into(),
            input: 0,
            mix: 0,
            value: "20".into(),
        };
        assert_eq!(
            bridge.change(change.clone()).await,
            Err("Pan is available only on a stereo-linked mix.")
        );
        for value in ["-64", "64", "1.5", "+1", "01"] {
            change.value = value.into();
            assert!(change.validate().is_err());
        }
        let mut engineer = TcpStream::connect(address).await.unwrap();
        engineer
            .write_all(format!("set {MIX_ROLE} 0 0 \"StereoL\"\n").as_bytes())
            .await
            .unwrap();
        wait_value(&bridge, &format!("{MIX_ROLE} 0 0"), "\"StereoL\"").await;
        for value in ["-63", "0", "63"] {
            change.value = value.into();
            bridge.change(change.clone()).await.unwrap();
            wait_value(&bridge, &format!("{PAN} 0 0"), value).await;
        }
        assert_eq!(bridge.state().await.values[&format!("{PAN} 0 1")], "0");
        assert_eq!(bridge.state().await.values[&format!("{PAN} 1 0")], "0");
        engineer
            .write_all(format!("set {PAN} 0 0 -17\nset {MIX_ROLE} 0 0 \"Mono\"\n").as_bytes())
            .await
            .unwrap();
        wait_value(&bridge, &format!("{MIX_ROLE} 0 0"), "\"Mono\"").await;
        wait_value(&bridge, &format!("{PAN} 0 0"), "-17").await;
        assert!(bridge.change(change).await.is_err());
        let mut values = HashMap::new();
        for role in ["0", "1", "\"Mono\"", "\"Unknown\""] {
            values.insert(format!("{MIX_ROLE} 0 0"), role.into());
            assert!(!stereo_mix(&values, 0));
        }
        values.insert(format!("{MIX_ROLE} 0 0"), "\"StereoR\"".into());
        assert!(stereo_mix(&values, 0));
    }

    async fn wait_value(bridge: &ConsoleBridge, key: &str, value: &str) {
        timeout(Duration::from_secs(3), async {
            loop {
                if bridge
                    .state()
                    .await
                    .values
                    .get(key)
                    .is_some_and(|v| v == value)
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("console readback");
    }

    #[test]
    fn group_deltas_preserve_offsets_and_saturate_only_at_endpoints() {
        let values = [-350, -725, 950, -13750, crate::solo::OFF]
            .into_iter()
            .enumerate()
            .map(|(input, value)| (format!("{LEVEL} {input} 0"), value.to_string()))
            .collect();
        assert_eq!(
            group_levels(0, &[0, 1, 2, 3, 4], 2, &values).unwrap(),
            vec![(0, -150), (1, -525), (2, 1000), (3, -13550)]
        );
        assert_eq!(
            group_levels(0, &[0, 1, 2, 3, 4], -1, &values).unwrap(),
            vec![(0, -450), (1, -825), (2, 850), (3, crate::solo::OFF)]
        );
        assert!(group_levels(0, &[0, 1], 0, &values).unwrap().is_empty());
        for (mix, channels, delta) in [
            (24, vec![0], 1),
            (0, vec![], 1),
            (0, vec![0, 0], 1),
            (0, vec![80], 1),
            (0, vec![0, 5], 1),
            (0, vec![0], 150),
        ] {
            assert!(group_levels(mix, &channels, delta, &values).is_err());
        }
    }

    #[tokio::test]
    async fn group_moves_readback_and_leave_other_channels_and_mixes_alone() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        let mut engineer = TcpStream::connect(address).await.unwrap();
        engineer.write_all(format!("set {LEVEL} 0 0 -350\nset {LEVEL} 1 0 -725\nset {LEVEL} 2 0 -950\nset {LEVEL} 0 1 -1234\n").as_bytes()).await.unwrap();
        wait_level(&bridge, 0, 1, "-1234").await;
        bridge.group(0, vec![0, 1], 2).await.unwrap();
        wait_level(&bridge, 0, 0, "-150").await;
        wait_level(&bridge, 1, 0, "-525").await;
        wait_level(&bridge, 2, 0, "-950").await;
        wait_level(&bridge, 0, 1, "-1234").await;
        engineer
            .write_all(format!("set {LEVEL} 1 0 -225\n").as_bytes())
            .await
            .unwrap();
        wait_level(&bridge, 1, 0, "-225").await;
        bridge.group(0, vec![0, 1], -1).await.unwrap();
        wait_level(&bridge, 0, 0, "-250").await;
        wait_level(&bridge, 1, 0, "-325").await;
        // During solo, Off members stay Off. Audible member group moves are
        // observed by solo bookkeeping and retained when solo is cleared.
        engineer
            .write_all(format!("set {LEVEL} 79 0 -900\n").as_bytes())
            .await
            .unwrap();
        wait_level(&bridge, 79, 0, "-900").await;
        bridge.solo(0, "Singer".into(), [0].into()).await.unwrap();
        wait_level(&bridge, 79, 0, "-32768").await;
        bridge.group(0, vec![0, 1], 1).await.unwrap();
        wait_level(&bridge, 0, 0, "-150").await;
        bridge
            .solo(0, "Singer".into(), BTreeSet::new())
            .await
            .unwrap();
        wait_level(&bridge, 0, 0, "-150").await;
        wait_level(&bridge, 1, 0, "-325").await;
        assert!(ConsoleBridge::offline().group(0, vec![0], 1).await.is_err());
    }

    async fn wait_level(bridge: &ConsoleBridge, input: u8, mix: u8, value: &str) {
        timeout(Duration::from_secs(5), async {
            while bridge
                .state()
                .await
                .values
                .get(&format!("{LEVEL} {input} {mix}"))
                .map(String::as_str)
                != Some(value)
            {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("console level readback");
    }

    #[tokio::test]
    async fn personal_solo_round_trips_and_recall_never_restores_old_levels() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        let mut engineer = TcpStream::connect(address).await.unwrap();
        engineer.write_all(format!("set {LEVEL} 0 0 -350\nset {LEVEL} 1 0 -725\nset {LEVEL} 2 0 -950\nset {LEVEL} 1 1 -1234\n").as_bytes()).await.unwrap();
        wait_level(&bridge, 1, 1, "-1234").await;
        bridge.solo(0, "Singer".into(), [0].into()).await.unwrap();
        wait_level(&bridge, 2, 0, "-32768").await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(bridge.state().await.solos[&0].owner, "Singer");
        assert!(bridge
            .solo(0, "Other singer".into(), [2].into())
            .await
            .is_err());
        engineer
            .write_all(format!("set {LEVEL} 1 0 -625\n").as_bytes())
            .await
            .unwrap();
        wait_level(&bridge, 1, 0, "-625").await;
        bridge
            .solo(0, "Singer".into(), BTreeSet::new())
            .await
            .unwrap();
        wait_level(&bridge, 2, 0, "-950").await;
        wait_level(&bridge, 1, 0, "-625").await;
        wait_level(&bridge, 0, 0, "-350").await;
        wait_level(&bridge, 1, 1, "-1234").await;
        sleep(Duration::from_millis(100)).await;
        bridge.solo(0, "Singer".into(), [1].into()).await.unwrap();
        wait_level(&bridge, 0, 0, "-32768").await;
        fake.recall_scene(2).await.unwrap();
        timeout(Duration::from_secs(30), async {
            loop {
                let state = bridge.state().await;
                if state.scene_number == Some(2) && bridge.is_connected() && state.solos.is_empty()
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            bridge.state().await.solo_notice.as_deref(),
            Some("Scene changed — solo cleared")
        );
        bridge
            .solo(0, "Singer".into(), BTreeSet::new())
            .await
            .unwrap();
        wait_level(&bridge, 0, 0, "-1200").await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            bridge.state().await.values[&format!("{LEVEL} 0 0")],
            "-1200"
        );
    }

    async fn ready(bridge: &ConsoleBridge) {
        timeout(Duration::from_secs(30), async {
            while !bridge.is_connected() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bridge hydrates from the fake console");
    }

    #[tokio::test]
    async fn one_bridge_hydrates_once_and_coalesces_phone_writes() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        assert_eq!(fake.total_connections().await, 1);
        assert_eq!(
            bridge.state().await.values.get(&format!("{LEVEL} 0 0")),
            Some(&"-32768".to_owned())
        );
        let hydrated = bridge.state().await;
        assert_eq!(hydrated.scene_number, Some(1));
        assert_eq!(hydrated.scene_name.as_deref(), Some("Soundcheck"));
        let mut events = bridge.subscribe();
        bridge.change(level("-3")).await.unwrap();
        bridge.change(level("-2")).await.unwrap();
        timeout(Duration::from_secs(1), async {
            loop {
                if matches!(events.recv().await, Ok(BridgeEvent::Value { value, .. }) if value == "-200") { break; }
            }
        }).await.expect("latest coalesced write reaches the fake console");
        assert_eq!(fake.total_connections().await, 1);
    }

    #[tokio::test]
    async fn fifty_browser_subscribers_share_one_bridge_and_receive_console_changes() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        let mut browsers: Vec<_> = (0..50).map(|_| bridge.subscribe()).collect();
        bridge.change(level("-3")).await.unwrap();
        for browser in &mut browsers {
            timeout(Duration::from_millis(200), async {
                loop {
                    if matches!(browser.recv().await, Ok(BridgeEvent::Value { value, .. }) if value == "-300") {
                        break;
                    }
                }
            })
            .await
            .expect("each browser receives the shared bridge update");
        }
        assert_eq!(fake.total_connections().await, 1);
    }

    #[tokio::test]
    async fn quoted_multi_word_labels_hydrate_and_notify_the_bridge() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let key = format!("{INPUT_NAME} 0 0");

        let engineer = TcpStream::connect(address).await.unwrap();
        let mut engineer = BufReader::new(engineer);
        engineer
            .get_mut()
            .write_all(format!("set {INPUT_NAME} 0 0 \"Lead Vocal\"\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        engineer.read_line(&mut response).await.unwrap();
        assert_eq!(response.trim(), "OK");
        drop(engineer);

        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        assert_eq!(
            bridge.state().await.values.get(&key),
            Some(&"\"Lead Vocal\"".to_owned()),
            "hydration retains the complete quoted label"
        );

        let mut events = bridge.subscribe();
        let mut engineer = TcpStream::connect(address).await.unwrap();
        engineer
            .write_all(format!("set {INPUT_NAME} 0 0 \"Lead Guitar\"\n").as_bytes())
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            loop {
                if matches!(events.recv().await, Ok(BridgeEvent::Value { key: event_key, value }) if event_key == key && value == "\"Lead Guitar\"") {
                    break;
                }
            }
        })
        .await
        .expect("a quoted multi-word label reaches bridge subscribers");
    }

    #[tokio::test]
    async fn recall_rehydrates_and_a_dropped_connection_reconnects() {
        let fake = Arc::new(FakeConsole::new());
        let address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = ConsoleBridge::start(address.to_string());
        ready(&bridge).await;
        let mut events = bridge.subscribe();
        fake.recall_scene(2).await.unwrap();
        timeout(Duration::from_secs(30), async {
            loop {
                if matches!(events.recv().await, Ok(BridgeEvent::SceneRecalled { number: 2, name: Some(name) }) if name == "Show") {
                    break;
                }
            }
        })
        .await
        .expect("scene recall reaches every subscriber");
        timeout(Duration::from_secs(30), async {
            loop {
                if bridge.state().await.scene_number == Some(2) && bridge.is_connected() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recall rehydrates before writes resume");

        fake.drop_connections();
        timeout(Duration::from_secs(30), async {
            loop {
                if fake.total_connections().await >= 2 && bridge.is_connected() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bridge reconnects after a console drop");
    }
}
