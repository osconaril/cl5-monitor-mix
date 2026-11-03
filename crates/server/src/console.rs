//! The deliberately small, safe command surface between phones and the CL5.
//!
//! Keeping this parser separate from HTTP/WebSocket handling makes it impossible
//! for a browser request to become an arbitrary RCP command.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, mpsc, watch, RwLock},
    time::{interval, sleep},
};

pub const LEVEL: &str = "MIXER:Current/InCh/ToMix/Level";
pub const ON: &str = "MIXER:Current/InCh/ToMix/On";
pub const PAN: &str = "MIXER:Current/InCh/ToMix/Pan";
const INPUT_NAME: &str = "MIXER:Current/InCh/Label/Name";
const INPUT_COLOR: &str = "MIXER:Current/InCh/Label/Color";
const MIX_NAME: &str = "MIXER:Current/Mix/Label/Name";
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum BridgeEvent {
    Connected,
    Disconnected,
    Value { key: String, value: String },
    SceneRecalled { number: u16, name: Option<String> },
}

/// One server-owned RCP connection. It is deliberately independent from the
/// HTTP layer so WebSockets and future routes can only subscribe or submit a
/// validated `PhoneChange`; neither receives a TCP stream.
#[derive(Clone)]
pub struct ConsoleBridge {
    state: Arc<RwLock<ConsoleState>>,
    connected: watch::Receiver<bool>,
    events: broadcast::Sender<BridgeEvent>,
    writes: mpsc::Sender<PhoneChange>,
}

impl ConsoleBridge {
    pub fn start(address: impl Into<String>) -> Self {
        let state = Arc::new(RwLock::new(ConsoleState::default()));
        let (connected_tx, connected) = watch::channel(false);
        let (events, _) = broadcast::channel(512);
        let (writes, receiver) = mpsc::channel(512);
        tokio::spawn(run_bridge(
            address.into(),
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

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<BridgeEvent> {
        self.events.subscribe()
    }
    pub async fn state(&self) -> ConsoleState {
        self.state.read().await.clone()
    }

    /// Validates before queuing, and never retains a command while offline.
    pub async fn change(&self, change: PhoneChange) -> Result<(), &'static str> {
        change.validate()?;
        if !self.is_connected() {
            return Err("Console disconnected — reconnecting.");
        }
        self.writes
            .send(change)
            .await
            .map_err(|_| "Console disconnected — reconnecting.")
    }
}

async fn run_bridge(
    address: String,
    state: Arc<RwLock<ConsoleState>>,
    connected: watch::Sender<bool>,
    events: broadcast::Sender<BridgeEvent>,
    mut writes: mpsc::Receiver<PhoneChange>,
) {
    let mut retry = Duration::from_millis(100);
    loop {
        let _ = connected.send(false);
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
    writes: &mut mpsc::Receiver<PhoneChange>,
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
                        let _ = connected.send(false);
                        for command in snapshot_commands() {
                            if write.write_all(command.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() { return false; }
                            outstanding += 1;
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
                Some(change) => { pending.insert(change_key(&change), change); }
                None => return false,
            },
            _ = flush.tick(), if outstanding == 0 && !pending.is_empty() => {
                for (_, change) in std::mem::take(&mut pending) {
                    let Ok(command) = change.rcp() else { continue };
                    if write.write_all(command.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() { return false; }
                }
            }
        }
    }
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
        for path in [MIX_NAME, MIX_BUS_TYPE] {
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
    let words: Vec<_> = text.split_whitespace().collect();
    let set = if matches!(words.first(), Some(&"set" | &"get")) {
        Some(&words[1..])
    } else {
        None
    };
    if let Some([path, first, second, value]) = set {
        let key = format!("{path} {first} {second}");
        state
            .write()
            .await
            .values
            .insert(key.clone(), (*value).to_owned());
        let _ = events.send(BridgeEvent::Value {
            key,
            value: (*value).to_owned(),
        });
        return;
    }
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
    use fake_console::FakeConsole;
    use std::sync::Arc;
    use tokio::time::{sleep, timeout};

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
                if matches!(events.recv().await, Ok(BridgeEvent::Value { value, .. }) if value == "-2") { break; }
            }
        }).await.expect("latest coalesced write reaches the fake console");
        assert_eq!(fake.total_connections().await, 1);
    }
}
