//! Kick's realtime push channel (Centrifugo, JSON protocol), used for instant updates:
//!   * `private-channelpoints-{uid}` -> `PointsUpdated {channel_id, points, balance, reason}`
//!   * `private-{uid}`               -> `GamificationLevelUp {level}`, `GamificationWatchTimeGoalReached`
//!   * `channel.{channel_id}`        -> `App\Events\StreamerIsLive`, `App\Events\StopStreamBroadcast`
//!   * `private-livestream.{id}`     -> `App\Events\LivestreamUpdated` (title / category changes)
//!
//! Handshake (from the web bundle):
//!   POST web.kick.com/api/v1/realtime/connection        -> centrifugo websocket url
//!   POST web.kick.com/api/v1/realtime/auth/connection   -> connection JWT  (ttl ~45 min)
//!   POST web.kick.com/api/v1/realtime/auth/channel      -> JWT per private channel
//!   WS  {"connect":{"token","name":"js"},"id":1}, {"subscribe":{"channel","token"?,"flag":1},"id":n}
//!   Server pings are `{}` and must be answered with `{}`.

use crate::api::{KickClient, USER_AGENT, is_unauthorized};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Connected,
    PointsUpdated {
        channel_id: u64,
        points: i64,
        balance: i64,
        reason: String,
    },
    LevelUp {
        level: u32,
    },
    WatchGoalReached,
    StreamerLive {
        channel_id: u64,
    },
    StreamerOffline {
        channel_id: u64,
    },
    /// Title or category of a watched livestream changed.
    LivestreamUpdated {
        livestream_id: u64,
    },
    Unauthorized,
}

enum Cmd {
    WatchChannel(u64),
    WatchLivestream(u64),
    UnwatchLivestream(u64),
}

pub struct Realtime {
    tx: mpsc::UnboundedSender<Cmd>,
    cancel: CancellationToken,
}

impl Realtime {
    pub fn spawn(client: KickClient, user_id: u64) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = Task {
            client,
            user_id,
            events: ev_tx,
            channels: BTreeSet::new(),
            livestreams: BTreeSet::new(),
            next_id: 1,
            pending: HashMap::new(),
        };
        tokio::spawn(task.run(cmd_rx, cancel.clone()));
        (Self { tx: cmd_tx, cancel }, ev_rx)
    }

    /// Subscribe to live/offline events of a channel (idempotent).
    pub fn watch_channel(&self, channel_id: u64) {
        let _ = self.tx.send(Cmd::WatchChannel(channel_id));
    }

    /// Get notified about title/category changes of a livestream we're watching.
    pub fn watch_livestream(&self, livestream_id: u64) {
        let _ = self.tx.send(Cmd::WatchLivestream(livestream_id));
    }

    pub fn unwatch_livestream(&self, livestream_id: u64) {
        let _ = self.tx.send(Cmd::UnwatchLivestream(livestream_id));
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Task {
    client: KickClient,
    user_id: u64,
    events: mpsc::UnboundedSender<Event>,
    /// public `channel.{id}` subscriptions (kept across reconnects)
    channels: BTreeSet<u64>,
    /// private `private-livestream.{id}` subscriptions (kept across reconnects)
    livestreams: BTreeSet<u64>,
    next_id: u64,
    /// request id -> channel, to report subscribe errors
    pending: HashMap<u64, String>,
}

enum End {
    Cancelled,
    Reconnect(String),
}

impl Task {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>, cancel: CancellationToken) {
        let mut backoff = Duration::from_secs(2);
        loop {
            let res = tokio::select! {
                _ = cancel.cancelled() => return,
                r = self.connect() => r,
            };
            match res {
                Ok((ws, ttl)) => {
                    backoff = Duration::from_secs(2);
                    let _ = self.events.send(Event::Connected);
                    match self.session(ws, ttl, &mut rx, &cancel).await {
                        End::Cancelled => return,
                        End::Reconnect(why) => tracing::debug!("realtime: reconnecting ({why})"),
                    }
                    tokio::time::sleep(Duration::from_millis(500 + fastrand::u64(0..1500))).await;
                }
                Err(e) => {
                    if is_unauthorized(&e) {
                        let _ = self.events.send(Event::Unauthorized);
                    }
                    tracing::warn!("realtime connect failed: {e:#}; retry in {backoff:?}");
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(120));
                }
            }
            // Commands that arrived while disconnected.
            while let Ok(c) = rx.try_recv() {
                self.apply_offline(c);
            }
        }
    }

    fn apply_offline(&mut self, c: Cmd) {
        match c {
            Cmd::WatchChannel(id) => {
                self.channels.insert(id);
            }
            Cmd::WatchLivestream(id) => {
                self.livestreams.insert(id);
            }
            Cmd::UnwatchLivestream(id) => {
                self.livestreams.remove(&id);
            }
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    async fn connect(&mut self) -> anyhow::Result<(Ws, Duration)> {
        let url = self.client.realtime_url(self.user_id).await?;
        let token = self.client.realtime_connection_token(self.user_id).await?;
        let mut req = url.as_str().into_client_request()?;
        let h = req.headers_mut();
        h.insert("Origin", HeaderValue::from_static("https://kick.com"));
        h.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(req),
        )
        .await??;

        self.next_id = 1;
        self.pending.clear();
        ws.send(Message::Text(
            json!({"connect":{"token":token,"name":"js"},"id":1})
                .to_string()
                .into(),
        ))
        .await?;
        // Wait for the connect reply to learn the token TTL.
        loop {
            let m = tokio::time::timeout(Duration::from_secs(15), ws.next())
                .await?
                .ok_or_else(|| anyhow::anyhow!("closed during connect"))??;
            let Message::Text(t) = m else { continue };
            for line in t.lines().filter(|l| !l.trim().is_empty()) {
                let v: Value = serde_json::from_str(line).unwrap_or_default();
                if let Some(err) = v.get("error") {
                    anyhow::bail!("connect rejected: {err}");
                }
                if let Some(c) = v.get("connect") {
                    let ttl = c.get("ttl").and_then(Value::as_u64).unwrap_or(2700);
                    return Ok((ws, Duration::from_secs(ttl)));
                }
            }
        }
    }

    async fn subscribe(
        &mut self,
        ws: &mut Ws,
        channel: String,
        private: bool,
    ) -> anyhow::Result<()> {
        let id = self.id();
        let msg = if private {
            let token = self.client.realtime_channel_token(&channel).await?;
            json!({"subscribe":{"channel":channel,"token":token,"flag":1},"id":id})
        } else {
            json!({"subscribe":{"channel":channel,"flag":1},"id":id})
        };
        self.pending.insert(id, channel);
        ws.send(Message::Text(msg.to_string().into())).await?;
        Ok(())
    }

    async fn session(
        &mut self,
        mut ws: Ws,
        ttl: Duration,
        rx: &mut mpsc::UnboundedReceiver<Cmd>,
        cancel: &CancellationToken,
    ) -> End {
        // Reconnect (fresh tokens) a bit before the connection token expires.
        let refresh_at = Instant::now()
            + ttl
                .saturating_sub(Duration::from_secs(120))
                .max(Duration::from_secs(60));
        let uid = self.user_id;
        for (ch, private) in [
            (format!("private-channelpoints-{uid}"), true),
            (format!("private-{uid}"), true),
        ] {
            if let Err(e) = self.subscribe(&mut ws, ch.clone(), private).await {
                tracing::warn!("realtime: subscribing to {ch} failed: {e:#}");
            }
        }
        // Subscriptions requested while we were connecting.
        while let Ok(c) = rx.try_recv() {
            self.apply_offline(c);
        }
        let ids: Vec<u64> = self.channels.iter().copied().collect();
        for id in ids {
            if let Err(e) = self
                .subscribe(&mut ws, format!("channel.{id}"), false)
                .await
            {
                return End::Reconnect(e.to_string());
            }
        }
        let ls: Vec<u64> = self.livestreams.iter().copied().collect();
        for id in ls {
            if let Err(e) = self
                .subscribe(&mut ws, format!("private-livestream.{id}"), true)
                .await
            {
                tracing::debug!("realtime: livestream {id} subscription failed: {e:#}");
            }
        }
        tracing::info!(
            "realtime connected ({} subscription(s))",
            self.channels.len() + self.livestreams.len() + 2
        );

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = ws.close(None).await;
                    return End::Cancelled;
                }
                _ = tokio::time::sleep_until(refresh_at) => {
                    let _ = ws.close(None).await;
                    return End::Reconnect("token refresh".into());
                }
                c = rx.recv() => match c {
                    None => return End::Cancelled,
                    Some(Cmd::WatchChannel(id)) => {
                        if self.channels.insert(id)
                            && let Err(e) = self.subscribe(&mut ws, format!("channel.{id}"), false).await {
                                return End::Reconnect(e.to_string());
                            }
                    }
                    Some(Cmd::WatchLivestream(id)) => {
                        if self.livestreams.insert(id)
                            && let Err(e) = self.subscribe(&mut ws, format!("private-livestream.{id}"), true).await {
                                tracing::debug!("realtime: livestream {id} subscription failed: {e:#}");
                            }
                    }
                    Some(Cmd::UnwatchLivestream(id)) => {
                        if self.livestreams.remove(&id) {
                            let rid = self.id();
                            let m = json!({"unsubscribe":{"channel":format!("private-livestream.{id}")},"id":rid});
                            if let Err(e) = ws.send(Message::Text(m.to_string().into())).await {
                                return End::Reconnect(e.to_string());
                            }
                        }
                    }
                },
                m = ws.next() => match m {
                    Some(Ok(Message::Text(t))) => {
                        // Several JSON replies may be batched in one frame, newline separated.
                        for line in t.lines() {
                            let line = line.trim();
                            if line.is_empty() { continue; }
                            if line == "{}" {
                                if let Err(e) = ws.send(Message::Text("{}".into())).await {
                                    return End::Reconnect(e.to_string());
                                }
                                continue;
                            }
                            self.on_reply(line);
                        }
                    }
                    Some(Ok(Message::Close(f))) => return End::Reconnect(format!("closed: {f:?}")),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return End::Reconnect(e.to_string()),
                    None => return End::Reconnect("stream ended".into()),
                },
            }
        }
    }

    fn on_reply(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            let ch = self.pending.remove(&id);
            if let Some(err) = v.get("error") {
                tracing::warn!(
                    "realtime: request for {} failed: {err}",
                    ch.unwrap_or_default()
                );
            }
            return;
        }
        let Some(push) = v.get("push") else { return };
        let ev = parse_push(push);
        if let Some(ev) = ev {
            tracing::debug!("realtime: {ev:?}");
            let _ = self.events.send(ev);
        }
    }
}

fn channel_id_of(channel: &str) -> Option<u64> {
    channel.strip_prefix("channel.")?.parse().ok()
}

/// Turns a Centrifugo `push` frame into an [`Event`] the miner cares about.
fn parse_push(push: &Value) -> Option<Event> {
    let channel = push
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = push.pointer("/pub/data")?;
    let event = data
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Payload is usually a JSON-encoded string, sometimes an object.
    let payload: Value = match data.get("data") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(o) => o.clone(),
        None => Value::Null,
    };
    let num = |k: &str| {
        payload
            .get(k)
            .and_then(|x| x.as_i64().or_else(|| x.as_str()?.parse().ok()))
    };
    match event {
        "PointsUpdated" => Some(Event::PointsUpdated {
            channel_id: num("channel_id").unwrap_or(0) as u64,
            points: num("points").unwrap_or(0),
            balance: num("balance").unwrap_or(0),
            reason: payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "GamificationLevelUp" => Some(Event::LevelUp {
            level: num("level").unwrap_or(0) as u32,
        }),
        "GamificationWatchTimeGoalReached" => Some(Event::WatchGoalReached),
        e if e.ends_with("StreamerIsLive") => {
            channel_id_of(channel).map(|id| Event::StreamerLive { channel_id: id })
        }
        e if e.ends_with("LivestreamUpdated") => channel
            .strip_prefix("private-livestream.")
            .and_then(|id| id.parse().ok())
            .map(|id| Event::LivestreamUpdated { livestream_id: id }),
        e if e.ends_with("StopStreamBroadcast") => {
            channel_id_of(channel).map(|id| Event::StreamerOffline { channel_id: id })
        }
        _ => {
            tracing::trace!("realtime: {channel} {event}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(channel: &str, event: &str, data: Value) -> Value {
        json!({ "channel": channel, "pub": { "data": { "event": event, "data": data } } })
    }

    #[test]
    fn points_updated_with_string_payload() {
        let p = push(
            "private-channelpoints-1",
            "PointsUpdated",
            Value::String(
                r#"{"channel_id":26869410,"points":10,"balance":20,"reason":"EARNED"}"#.into(),
            ),
        );
        assert_eq!(
            parse_push(&p),
            Some(Event::PointsUpdated {
                channel_id: 26869410,
                points: 10,
                balance: 20,
                reason: "EARNED".into()
            })
        );
    }

    #[test]
    fn level_up_with_object_payload_and_string_number() {
        let p = push("private-1", "GamificationLevelUp", json!({ "level": "14" }));
        assert_eq!(parse_push(&p), Some(Event::LevelUp { level: 14 }));
    }

    #[test]
    fn live_offline_and_category_changes() {
        let live = push("channel.188532", "App\\Events\\StreamerIsLive", json!({}));
        assert_eq!(
            parse_push(&live),
            Some(Event::StreamerLive { channel_id: 188532 })
        );
        let off = push(
            "channel.188532",
            "App\\Events\\StopStreamBroadcast",
            json!({}),
        );
        assert_eq!(
            parse_push(&off),
            Some(Event::StreamerOffline { channel_id: 188532 })
        );
        let upd = push(
            "private-livestream.135547435",
            "App\\Events\\LivestreamUpdated",
            json!({}),
        );
        assert_eq!(
            parse_push(&upd),
            Some(Event::LivestreamUpdated {
                livestream_id: 135547435
            })
        );
        let goal = push("private-1", "GamificationWatchTimeGoalReached", json!({}));
        assert_eq!(parse_push(&goal), Some(Event::WatchGoalReached));
    }

    #[test]
    fn unknown_or_malformed_pushes_are_ignored() {
        assert_eq!(
            parse_push(&push(
                "chatrooms.1.v2",
                "App\\Events\\ChatMessageEvent",
                json!({})
            )),
            None
        );
        assert_eq!(parse_push(&json!({ "channel": "x" })), None);
        // A live event on a channel name we can't parse yields nothing rather than a bogus id.
        assert_eq!(
            parse_push(&push(
                "channel_abc",
                "App\\Events\\StreamerIsLive",
                json!({})
            )),
            None
        );
    }
}
