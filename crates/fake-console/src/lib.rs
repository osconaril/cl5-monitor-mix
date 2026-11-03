//! A deterministic, in-process Yamaha RCP test double.  It never discovers or
//! connects to a physical console.

use std::{collections::HashMap, io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{broadcast, Mutex, RwLock},
    time::sleep,
};

pub const MONO_INPUTS: usize = 72;
pub const STEREO_INPUTS: usize = 8;
pub const MIXES: usize = 24;

#[derive(Clone, Debug)]
pub struct FakeConsole {
    state: Arc<Mutex<State>>,
    notifications: broadcast::Sender<String>,
    reply_delay: Arc<RwLock<Duration>>,
    connection_limit: Arc<RwLock<usize>>,
    active_connections: Arc<Mutex<usize>>,
    total_connections: Arc<Mutex<usize>>,
}

#[derive(Debug)]
struct State {
    values: HashMap<String, String>,
    scenes: HashMap<u16, Scene>,
    current_scene: u16,
}

#[derive(Clone, Debug)]
struct Scene {
    name: String,
    values: HashMap<String, String>,
}

impl Default for FakeConsole {
    fn default() -> Self {
        let (notifications, _) = broadcast::channel(128);
        let mut scenes = HashMap::new();
        scenes.insert(
            1,
            Scene {
                name: "Soundcheck".into(),
                values: HashMap::new(),
            },
        );
        let mut show_values = HashMap::new();
        show_values.insert(
            send_key("MIXER:Current/InCh/ToMix/Level", 0, 0),
            "-1200".into(),
        );
        scenes.insert(
            2,
            Scene {
                name: "Show".into(),
                values: show_values,
            },
        );
        Self {
            state: Arc::new(Mutex::new(State {
                values: HashMap::new(),
                scenes,
                current_scene: 1,
            })),
            notifications,
            reply_delay: Arc::new(RwLock::new(Duration::ZERO)),
            connection_limit: Arc::new(RwLock::new(usize::MAX)),
            active_connections: Arc::new(Mutex::new(0)),
            total_connections: Arc::new(Mutex::new(0)),
        }
    }
}

impl FakeConsole {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a TCP listener and returns its bound address. The spawned task
    /// owns the listener for the rest of the test process.
    pub async fn listen(self: Arc<Self>, address: &str) -> io::Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let console = self.clone();
                tokio::spawn(async move {
                    let _ = console.accept(stream).await;
                });
            }
        });
        Ok(address)
    }

    /// Makes later replies wait for `delay`; set zero to restore normal service.
    pub async fn set_reply_delay(&self, delay: Duration) {
        *self.reply_delay.write().await = delay;
    }
    /// Refuses new connections after this many live clients. Existing clients remain connected.
    pub async fn set_connection_limit(&self, limit: usize) {
        *self.connection_limit.write().await = limit;
    }
    /// Drops all clients by broadcasting a control message. The fake has no real-console side effects.
    pub fn drop_connections(&self) {
        let _ = self.notifications.send("__FAKE_DROP__".into());
    }

    /// Test-only connection accounting lets the bridge prove that browsers do
    /// not fan out into one console connection apiece.
    pub async fn total_connections(&self) -> usize {
        *self.total_connections.lock().await
    }

    async fn accept(self: Arc<Self>, stream: TcpStream) -> io::Result<()> {
        {
            let mut active = self.active_connections.lock().await;
            if *active >= *self.connection_limit.read().await {
                return Ok(());
            }
            *active += 1;
            *self.total_connections.lock().await += 1;
        }
        let result = self.clone().serve(stream).await;
        *self.active_connections.lock().await -= 1;
        result
    }

    async fn serve(self: Arc<Self>, stream: TcpStream) -> io::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read).lines();
        let mut notifications = self.notifications.subscribe();
        loop {
            tokio::select! {
                line = reader.next_line() => match line? {
                    Some(line) => {
                        let response = self.command(&line).await;
                        sleep(*self.reply_delay.read().await).await;
                        write.write_all(response.as_bytes()).await?;
                        write.write_all(b"\n").await?;
                    }
                    None => return Ok(()),
                },
                notice = notifications.recv() => match notice {
                    Ok(notice) if notice == "__FAKE_DROP__" => return Ok(()),
                    Ok(notice) => { write.write_all(notice.as_bytes()).await?; write.write_all(b"\n").await?; }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }

    async fn command(&self, line: &str) -> String {
        let words = match command_words(line) {
            Ok(words) => words,
            Err(()) => return "ERROR malformed command".into(),
        };
        match words.as_slice() {
            [command, path, channel, mix] if command == "get" => {
                let Ok(key) = control_key(path, channel, mix) else {
                    return "ERROR unsupported command".into();
                };
                let value = self
                    .state
                    .lock()
                    .await
                    .values
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| default_value(path).into());
                format!("OK get {path} {channel} {mix} {value}")
            }
            [command, path, channel, mix, value] if command == "set" => {
                let Ok(key) = control_key(path, channel, mix) else {
                    return "ERROR unsupported command".into();
                };
                if !set_value_is_valid(path, value) {
                    return "ERROR invalid value".into();
                }
                self.state.lock().await.values.insert(key, value.clone());
                let wire = format!("NOTIFY set {path} {channel} {mix} {value}");
                let _ = self.notifications.send(wire);
                "OK".into()
            }
            [command, path] if command == "sscurrent_ex" && path == "MIXER:Lib/Scene" => {
                let current = self.state.lock().await.current_scene;
                format!("OK sscurrent_ex MIXER:Lib/Scene {current}")
            }
            [command, path, number] if command == "ssinfo_ex" && path == "MIXER:Lib/Scene" => {
                let Ok(number) = number.parse() else {
                    return "ERROR scene not found".into();
                };
                let state = self.state.lock().await;
                match state.scenes.get(&number) {
                    Some(scene) => {
                        format!("OK ssinfo_ex MIXER:Lib/Scene {number} \"{}\"", scene.name)
                    }
                    None => "ERROR scene not found".into(),
                }
            }
            [command, path, number] if command == "ssrecall_ex" && path == "MIXER:Lib/Scene" => {
                match number.parse() {
                    Ok(number) => self.recall(number).await,
                    Err(_) => "ERROR scene not found".into(),
                }
            }
            _ => "ERROR unsupported command".into(),
        }
    }

    async fn recall(&self, number: u16) -> String {
        let (name, values) = {
            let mut state = self.state.lock().await;
            let Some(scene) = state.scenes.get(&number).cloned() else {
                return "ERROR scene not found".into();
            };
            state.current_scene = number;
            state.values.extend(scene.values.clone());
            (scene.name, scene.values)
        };
        for (key, value) in values {
            let _ = self.notifications.send(format!("NOTIFY set {key} {value}"));
        }
        let _ = self
            .notifications
            .send(format!("NOTIFY ssrecall_ex MIXER:Lib/Scene {number}"));
        let _ = self.notifications.send(format!(
            "NOTIFY ssinfo_ex MIXER:Lib/Scene {number} \"{name}\""
        ));
        "OK".into()
    }
}

/// Splits an RCP line while retaining quoted values on the wire. RCP label
/// commands use quoted names, which may contain spaces.
fn command_words(line: &str) -> Result<Vec<String>, ()> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;

    for character in line.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                word.push(character);
            }
            character if character.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }
    if quoted {
        return Err(());
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

fn send_key(path: &str, channel: usize, mix: usize) -> String {
    format!("{path} {channel} {mix}")
}

/// Validates the subset of RCP controls documented for this fake before it
/// reaches state. Keeping the bounds here prevents bridge tests from passing
/// against inputs or mixes a CL5 Monitor Mix deployment cannot address.
fn control_key(path: &str, first: &str, second: &str) -> Result<String, ()> {
    let first = first.parse::<usize>().map_err(|_| ())?;
    let second = second.parse::<usize>().map_err(|_| ())?;
    let input_path = matches!(
        path,
        "MIXER:Current/InCh/ToMix/Level"
            | "MIXER:Current/InCh/ToMix/On"
            | "MIXER:Current/InCh/ToMix/Pan"
    );
    let input_label_path = matches!(
        path,
        "MIXER:Current/InCh/Label/Name" | "MIXER:Current/InCh/Label/Color"
    );
    let mix_path = matches!(
        path,
        "MIXER:Current/Mix/Label/Name" | "MIXER:Current/Mix/BusType"
    );

    if (input_path && first < MONO_INPUTS + STEREO_INPUTS && second < MIXES)
        || (input_label_path && first < MONO_INPUTS + STEREO_INPUTS && second == 0)
        || (mix_path && first < MIXES && second == 0)
    {
        Ok(send_key(path, first, second))
    } else {
        Err(())
    }
}

fn set_value_is_valid(path: &str, value: &str) -> bool {
    match path {
        "MIXER:Current/InCh/ToMix/Level" => value
            .parse::<i32>()
            .is_ok_and(|level| level == -32768 || (-13800..=1000).contains(&level)),
        "MIXER:Current/InCh/ToMix/On" => matches!(value, "0" | "1"),
        // The precise pan and color code domains remain deliberately
        // unconfirmed in the protocol notes. They are still numeric controls.
        "MIXER:Current/InCh/ToMix/Pan" | "MIXER:Current/InCh/Label/Color" => {
            value.parse::<i32>().is_ok()
        }
        "MIXER:Current/InCh/Label/Name" | "MIXER:Current/Mix/Label/Name" => {
            value.starts_with('"') && value.ends_with('"') && value.len() >= 2
        }
        // BusType is read-only in the documented surface.
        _ => false,
    }
}
fn default_value(path: &str) -> &'static str {
    match path {
        // Labels are strings on the wire, even before an engineer has named
        // them. Returning an empty quoted label keeps a new fake console in
        // the same value domain as a physical console.
        "MIXER:Current/InCh/Label/Name" | "MIXER:Current/Mix/Label/Name" => "\"\"",
        // The fake models BusType as a boolean-like 0/1 field; a new mix is
        // unlinked. The physical CL5's precise encoding remains documented as
        // unconfirmed, but the fake must not return a send-level sentinel here.
        "MIXER:Current/Mix/BusType" => "0",
        path if path.ends_with("/On") || path.ends_with("/Pan") => "0",
        _ => "-32768",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpStream,
        time::{timeout, Duration},
    };

    async fn line(
        reader: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    ) -> String {
        timeout(Duration::from_secs(1), reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn set_from_one_client_notifies_another() {
        let console = Arc::new(FakeConsole::new());
        let addr = console.listen("127.0.0.1:0").await.unwrap();
        let first = TcpStream::connect(addr).await.unwrap();
        let second = TcpStream::connect(addr).await.unwrap();
        let (first_read, mut first_write) = first.into_split();
        let (second_read, mut second_write) = second.into_split();
        let mut first_lines = BufReader::new(first_read).lines();
        let mut second_lines = BufReader::new(second_read).lines();
        // Complete a request first so the second server task has subscribed to
        // broadcasts before the first client mutates state.
        second_write
            .write_all(b"get MIXER:Current/InCh/ToMix/Level 0 0\n")
            .await
            .unwrap();
        assert_eq!(
            line(&mut second_lines).await,
            "OK get MIXER:Current/InCh/ToMix/Level 0 0 -32768"
        );
        first_write
            .write_all(b"set MIXER:Current/InCh/ToMix/Level 0 0 -1200\n")
            .await
            .unwrap();
        assert_eq!(line(&mut first_lines).await, "OK");
        assert_eq!(
            line(&mut second_lines).await,
            "NOTIFY set MIXER:Current/InCh/ToMix/Level 0 0 -1200"
        );
    }

    #[tokio::test]
    async fn scene_recall_announces_new_scene_number_and_name() {
        let console = Arc::new(FakeConsole::new());
        let addr = console.listen("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = client.into_split();
        let mut lines = BufReader::new(read).lines();
        write
            .write_all(b"ssrecall_ex MIXER:Lib/Scene 2\n")
            .await
            .unwrap();
        assert_eq!(line(&mut lines).await, "OK");
        assert_eq!(
            line(&mut lines).await,
            "NOTIFY set MIXER:Current/InCh/ToMix/Level 0 0 -1200"
        );
        assert_eq!(
            line(&mut lines).await,
            "NOTIFY ssrecall_ex MIXER:Lib/Scene 2"
        );
        assert_eq!(
            line(&mut lines).await,
            "NOTIFY ssinfo_ex MIXER:Lib/Scene 2 \"Show\""
        );
    }

    #[tokio::test]
    async fn quoted_multi_word_label_round_trips_over_tcp() {
        let console = Arc::new(FakeConsole::new());
        let addr = console.listen("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = client.into_split();
        let mut lines = BufReader::new(read).lines();

        write
            .write_all(b"set MIXER:Current/InCh/Label/Name 0 0 \"Lead Vocal\"\n")
            .await
            .unwrap();
        assert_eq!(line(&mut lines).await, "OK");
        assert_eq!(
            line(&mut lines).await,
            "NOTIFY set MIXER:Current/InCh/Label/Name 0 0 \"Lead Vocal\""
        );
        write
            .write_all(b"get MIXER:Current/InCh/Label/Name 0 0\n")
            .await
            .unwrap();
        assert_eq!(
            line(&mut lines).await,
            "OK get MIXER:Current/InCh/Label/Name 0 0 \"Lead Vocal\""
        );
    }

    #[tokio::test]
    async fn untouched_labels_and_mix_type_have_typed_defaults_over_tcp() {
        let console = Arc::new(FakeConsole::new());
        let addr = console.listen("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = client.into_split();
        let mut lines = BufReader::new(read).lines();

        for (command, expected) in [
            (
                "get MIXER:Current/InCh/Label/Name 0 0\n",
                "OK get MIXER:Current/InCh/Label/Name 0 0 \"\"",
            ),
            (
                "get MIXER:Current/Mix/Label/Name 0 0\n",
                "OK get MIXER:Current/Mix/Label/Name 0 0 \"\"",
            ),
            (
                "get MIXER:Current/Mix/BusType 0 0\n",
                "OK get MIXER:Current/Mix/BusType 0 0 0",
            ),
        ] {
            write.write_all(command.as_bytes()).await.unwrap();
            assert_eq!(line(&mut lines).await, expected);
        }
    }

    #[tokio::test]
    async fn tcp_rejects_out_of_range_and_invalid_send_controls() {
        let console = Arc::new(FakeConsole::new());
        let addr = console.listen("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = client.into_split();
        let mut lines = BufReader::new(read).lines();

        write
            .write_all(b"set MIXER:Current/InCh/ToMix/Level 79 23 -13800\n")
            .await
            .unwrap();
        assert_eq!(line(&mut lines).await, "OK");
        assert_eq!(
            line(&mut lines).await,
            "NOTIFY set MIXER:Current/InCh/ToMix/Level 79 23 -13800"
        );

        for command in [
            "set MIXER:Current/InCh/ToMix/Level 80 23 -1200\n",
            "set MIXER:Current/InCh/ToMix/Level 79 24 -1200\n",
            "set MIXER:Current/InCh/ToMix/Level nope 0 -1200\n",
        ] {
            write.write_all(command.as_bytes()).await.unwrap();
            assert_eq!(line(&mut lines).await, "ERROR unsupported command");
        }
        write
            .write_all(b"set MIXER:Current/InCh/ToMix/Level 79 23 -13801\n")
            .await
            .unwrap();
        assert_eq!(line(&mut lines).await, "ERROR invalid value");
        write
            .write_all(b"set MIXER:Current/InCh/ToMix/On 79 23 2\n")
            .await
            .unwrap();
        assert_eq!(line(&mut lines).await, "ERROR invalid value");
    }
}
