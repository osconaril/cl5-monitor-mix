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

    async fn accept(self: Arc<Self>, stream: TcpStream) -> io::Result<()> {
        {
            let mut active = self.active_connections.lock().await;
            if *active >= *self.connection_limit.read().await {
                return Ok(());
            }
            *active += 1;
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
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            ["get", path, channel, mix] => {
                let key = send_key(path, channel.parse().unwrap_or(0), mix.parse().unwrap_or(0));
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
            ["set", path, channel, mix, value] => {
                let key = send_key(path, channel.parse().unwrap_or(0), mix.parse().unwrap_or(0));
                self.state.lock().await.values.insert(key, (*value).into());
                let wire = format!("NOTIFY set {path} {channel} {mix} {value}");
                let _ = self.notifications.send(wire);
                "OK".into()
            }
            ["sscurrent_ex", "MIXER:Lib/Scene"] => {
                let current = self.state.lock().await.current_scene;
                format!("OK sscurrent_ex MIXER:Lib/Scene {current}")
            }
            ["ssinfo_ex", "MIXER:Lib/Scene", number] => {
                let number = number.parse().unwrap_or(0);
                let state = self.state.lock().await;
                match state.scenes.get(&number) {
                    Some(scene) => {
                        format!("OK ssinfo_ex MIXER:Lib/Scene {number} \"{}\"", scene.name)
                    }
                    None => "ERROR scene not found".into(),
                }
            }
            ["ssrecall_ex", "MIXER:Lib/Scene", number] => {
                self.recall(number.parse().unwrap_or(0)).await
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

fn send_key(path: &str, channel: usize, mix: usize) -> String {
    format!("{path} {channel} {mix}")
}
fn default_value(path: &str) -> &'static str {
    if path.ends_with("/On") || path.ends_with("/Pan") {
        "0"
    } else {
        "-32768"
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
}
