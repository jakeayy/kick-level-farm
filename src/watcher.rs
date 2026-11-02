//! Emulates the kick.com web player's presence on `wss://websockets.kick.com/viewer/v1/connect`.
//!
//! Protocol observed from the web client (bundle `WebSocketProvider` + watch-tracking hook):
//!
//! ```text
//! GET  /viewer/v1/token (Bearer + X-CLIENT-TOKEN)       -> one-shot connect token
//! WS   connect?token=...
//! send {"type":"user_event","data":{"message":{"name":"tracking.user.watch.livestream",
//!       "channel_id":..,"livestream_id":..,"vod_id":".."}}}   on start, then every `interval` s
//! send {"type":"channel_handshake","data":{"message":{"channelId":"<id>"}}}  every 15 s
//! send {"type":"ping"}                                                     every 30 s
//! send {"type":"channel_disconnect",...}                                   when leaving
//! ```

use crate::api::{KickClient, USER_AGENT, VIEWER_WS_URL, is_unauthorized};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use tokio_util::sync::CancellationToken;

const PING_EVERY: Duration = Duration::from_secs(30);
const HANDSHAKE_EVERY: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub slug: String,
    pub channel_id: u64,
    pub livestream_id: u64,
    pub vod_id: String,
}

#[derive(Default)]
pub struct WatchStats {
    /// watch events sent per channel id
    pub events: Mutex<HashMap<u64, u64>>,
    /// set when the server rejects our credentials
    pub unauthorized: AtomicBool,
}

impl WatchStats {
    pub fn events_for(&self, channel_id: u64) -> u64 {
        self.events
            .lock()
            .unwrap()
            .get(&channel_id)
            .copied()
            .unwrap_or(0)
    }
}

enum Cmd {
    Add(WatchTarget),
    Remove(u64),
}

pub struct Connection {
    tx: mpsc::UnboundedSender<Cmd>,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

impl Connection {
    pub fn spawn(
        client: KickClient,
        interval: Duration,
        handshake: bool,
        stats: Arc<WatchStats>,
        label: String,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = Task {
            client,
            interval,
            handshake,
            stats,
            label,
            targets: HashMap::new(),
            seen_types: HashSet::new(),
        };
        let join = tokio::spawn(task.run(rx, cancel.clone()));
        Self { tx, cancel, join }
    }

    pub fn add(&self, t: WatchTarget) {
        let _ = self.tx.send(Cmd::Add(t));
    }

    pub fn remove(&self, channel_id: u64) {
        let _ = self.tx.send(Cmd::Remove(channel_id));
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.join).await;
    }
}

struct Slot {
    target: WatchTarget,
    next_event: Instant,
}

struct Task {
    client: KickClient,
    interval: Duration,
    handshake: bool,
    stats: Arc<WatchStats>,
    label: String,
    targets: HashMap<u64, Slot>,
    seen_types: HashSet<String>,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn jitter(d: Duration, max_ms: u64) -> Duration {
    d + Duration::from_millis(fastrand::u64(0..=max_ms))
}

fn msg(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

fn watch_event(t: &WatchTarget) -> Message {
    msg(json!({"type":"user_event","data":{"message":{
        "name":"tracking.user.watch.livestream",
        "channel_id": t.channel_id,
        "livestream_id": t.livestream_id,
        "vod_id": t.vod_id,
    }}}))
}

fn handshake_msg(channel_id: u64) -> Message {
    msg(
        json!({"type":"channel_handshake","data":{"message":{"channelId": channel_id.to_string()}}}),
    )
}

fn disconnect_msg(channel_id: u64) -> Message {
    msg(
        json!({"type":"channel_disconnect","data":{"message":{"channelId": channel_id.to_string()}}}),
    )
}

impl Task {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>, cancel: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        loop {
            // Nothing to watch: idle until we get a target (or are cancelled).
            while self.targets.is_empty() {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    c = rx.recv() => match c {
                        Some(c) => self.apply_offline(c),
                        None => return,
                    },
                }
            }

            let ws = tokio::select! {
                _ = cancel.cancelled() => return,
                r = self.connect() => r,
            };
            let ws = match ws {
                Ok(ws) => {
                    backoff = Duration::from_secs(1);
                    ws
                }
                Err(e) => {
                    if is_unauthorized(&e) {
                        self.stats.unauthorized.store(true, Ordering::Relaxed);
                    }
                    tracing::warn!(
                        "[{}] viewer socket connect failed: {e:#}; retry in {backoff:?}",
                        self.label
                    );
                    let deadline = Instant::now() + backoff;
                    backoff = (backoff.mul_f32(1.6)).min(Duration::from_secs(60));
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep_until(deadline) => break,
                            c = rx.recv() => match c {
                                Some(c) => self.apply_offline(c),
                                None => return,
                            },
                        }
                    }
                    continue;
                }
            };

            match self.session(ws, &mut rx, &cancel).await {
                SessionEnd::Cancelled | SessionEnd::ChannelClosed => return,
                SessionEnd::Dropped(reason) => {
                    tracing::info!(
                        "[{}] viewer socket dropped ({reason}); reconnecting",
                        self.label
                    );
                    tokio::time::sleep(jitter(Duration::from_millis(500), 1500)).await;
                }
            }
        }
    }

    fn apply_offline(&mut self, c: Cmd) {
        match c {
            Cmd::Add(t) => {
                self.targets.insert(
                    t.channel_id,
                    Slot {
                        next_event: Instant::now(),
                        target: t,
                    },
                );
            }
            Cmd::Remove(id) => {
                self.targets.remove(&id);
            }
        }
    }

    async fn connect(&self) -> anyhow::Result<Ws> {
        let token = self.client.viewer_token().await?;
        let mut req = format!("{VIEWER_WS_URL}?token={token}").into_client_request()?;
        let h = req.headers_mut();
        h.insert("Origin", HeaderValue::from_static("https://kick.com"));
        h.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        h.insert(
            "Accept-Language",
            HeaderValue::from_static("en-US,en;q=0.9"),
        );
        let (ws, _resp) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(req),
        )
        .await??;
        tracing::debug!("[{}] viewer socket connected", self.label);
        Ok(ws)
    }

    async fn send_event(&mut self, ws: &mut Ws, id: u64) -> anyhow::Result<()> {
        let Some(slot) = self.targets.get_mut(&id) else {
            return Ok(());
        };
        ws.send(watch_event(&slot.target)).await?;
        slot.next_event = Instant::now() + jitter(self.interval, 1500);
        let n = {
            let mut ev = self.stats.events.lock().unwrap();
            let e = ev.entry(id).or_default();
            *e += 1;
            *e
        };
        tracing::debug!("[{}] watch event #{n} -> {}", self.label, slot.target.slug);
        Ok(())
    }

    async fn session(
        &mut self,
        mut ws: Ws,
        rx: &mut mpsc::UnboundedReceiver<Cmd>,
        cancel: &CancellationToken,
    ) -> SessionEnd {
        let now = Instant::now();
        let mut next_ping = now + PING_EVERY;
        let mut next_handshake = now + HANDSHAKE_EVERY;
        let mut tick = tokio::time::interval(Duration::from_secs(1));

        // Initial burst, mirroring the web player: watch event (if due) then handshake.
        let ids: Vec<u64> = self.targets.keys().copied().collect();
        for id in ids {
            let due = self.targets[&id].next_event <= now;
            if due && let Err(e) = self.send_event(&mut ws, id).await {
                return SessionEnd::Dropped(e.to_string());
            }
            if self.handshake
                && let Err(e) = ws.send(handshake_msg(id)).await
            {
                return SessionEnd::Dropped(e.to_string());
            }
        }

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    for id in self.targets.keys() {
                        let _ = ws.send(disconnect_msg(*id)).await;
                    }
                    let _ = ws.close(None).await;
                    return SessionEnd::Cancelled;
                }
                c = rx.recv() => match c {
                    None => {
                        let _ = ws.close(None).await;
                        return SessionEnd::ChannelClosed;
                    }
                    Some(Cmd::Add(t)) => {
                        let id = t.channel_id;
                        self.targets.insert(id, Slot { next_event: Instant::now(), target: t });
                        if let Err(e) = self.send_event(&mut ws, id).await {
                            return SessionEnd::Dropped(e.to_string());
                        }
                        if self.handshake {
                            let _ = ws.send(handshake_msg(id)).await;
                        }
                    }
                    Some(Cmd::Remove(id)) => {
                        if self.targets.remove(&id).is_some() {
                            let _ = ws.send(disconnect_msg(id)).await;
                        }
                        if self.targets.is_empty() {
                            let _ = ws.close(None).await;
                            return SessionEnd::Dropped("no targets".into());
                        }
                    }
                },
                m = ws.next() => match m {
                    Some(Ok(Message::Text(t))) => self.on_text(&t),
                    Some(Ok(Message::Close(f))) => {
                        return SessionEnd::Dropped(format!("closed by server: {f:?}"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return SessionEnd::Dropped(e.to_string()),
                    None => return SessionEnd::Dropped("stream ended".into()),
                },
                _ = tick.tick() => {
                    let now = Instant::now();
                    if now >= next_ping {
                        if let Err(e) = ws.send(msg(json!({"type":"ping"}))).await {
                            return SessionEnd::Dropped(e.to_string());
                        }
                        next_ping = now + PING_EVERY;
                    }
                    if self.handshake && now >= next_handshake {
                        for id in self.targets.keys() {
                            if let Err(e) = ws.send(handshake_msg(*id)).await {
                                return SessionEnd::Dropped(e.to_string());
                            }
                        }
                        next_handshake = now + HANDSHAKE_EVERY;
                    }
                    let due: Vec<u64> = self
                        .targets
                        .iter()
                        .filter(|(_, s)| s.next_event <= now)
                        .map(|(id, _)| *id)
                        .collect();
                    for id in due {
                        if let Err(e) = self.send_event(&mut ws, id).await {
                            return SessionEnd::Dropped(e.to_string());
                        }
                    }
                }
            }
        }
    }

    /// Log any server message type we haven't seen before — useful for protocol discovery.
    fn on_text(&mut self, t: &str) {
        let kind = serde_json::from_str::<Value>(t)
            .ok()
            .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "<non-json>".into());
        if kind == "pong" {
            return;
        }
        if self.seen_types.insert(kind.clone()) {
            tracing::info!(
                "[{}] new viewer-socket message type `{kind}`: {}",
                self.label,
                t.chars().take(300).collect::<String>()
            );
        } else {
            tracing::debug!(
                "[{}] <- {}",
                self.label,
                t.chars().take(300).collect::<String>()
            );
        }
    }
}

enum SessionEnd {
    Cancelled,
    ChannelClosed,
    Dropped(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_of(m: Message) -> Value {
        match m {
            Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn messages_match_the_web_player() {
        let t = WatchTarget {
            slug: "x".into(),
            channel_id: 26239555,
            livestream_id: 135292453,
            vod_id: "01a19303-04b8-79fd-b4b8-bec1788c7fd8".into(),
        };
        assert_eq!(
            json_of(watch_event(&t)),
            json!({"type":"user_event","data":{"message":{
                "name":"tracking.user.watch.livestream",
                "channel_id":26239555,"livestream_id":135292453,
                "vod_id":"01a19303-04b8-79fd-b4b8-bec1788c7fd8"}}})
        );
        // The web player sends channelId as a *string* in these two.
        assert_eq!(
            json_of(handshake_msg(26239555)),
            json!({"type":"channel_handshake","data":{"message":{"channelId":"26239555"}}})
        );
        assert_eq!(
            json_of(disconnect_msg(26239555)),
            json!({"type":"channel_disconnect","data":{"message":{"channelId":"26239555"}}})
        );
    }
}
