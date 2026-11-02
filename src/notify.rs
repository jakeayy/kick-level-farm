//! Discord webhook notifications.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyEvent {
    /// Bot started.
    Started,
    /// Bot stopped (with the session summary).
    Stopped,
    /// Account level went up.
    LevelUp,
    /// Daily Reward / event challenge claimed.
    RewardClaimed,
    /// Drop reward claimed.
    DropClaimed,
    /// A farmed streamer went live.
    StreamerLive,
    /// Fatal problems, e.g. the session token expired.
    Error,
}

impl NotifyEvent {
    pub fn all() -> Vec<Self> {
        use NotifyEvent::*;
        vec![
            Started,
            Stopped,
            LevelUp,
            RewardClaimed,
            DropClaimed,
            StreamerLive,
            Error,
        ]
    }

    fn color(self) -> u32 {
        match self {
            NotifyEvent::Started => 0x53FC18, // kick green
            NotifyEvent::Stopped => 0x8B8B8B,
            NotifyEvent::LevelUp => 0xF5C518,
            NotifyEvent::RewardClaimed | NotifyEvent::DropClaimed => 0xA970FF,
            NotifyEvent::StreamerLive => 0xE91916,
            NotifyEvent::Error => 0xFF3B30,
        }
    }
}

#[derive(Clone, Default)]
pub struct Notifier {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    http: reqwest::Client,
    url: String,
    username: String,
    events: HashSet<NotifyEvent>,
    pending: Mutex<Vec<JoinHandle<()>>>,
}

impl Notifier {
    pub fn new(url: Option<String>, events: &[NotifyEvent], username: &str) -> Self {
        let Some(url) = url.filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        else {
            return Self::default();
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            inner: Some(Arc::new(Inner {
                http,
                url,
                username: username.to_string(),
                events: events.iter().copied().collect(),
                pending: Mutex::new(Vec::new()),
            })),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Fire-and-forget: posts an embed in the background.
    pub fn send(&self, ev: NotifyEvent, title: impl Into<String>, description: impl Into<String>) {
        let Some(inner) = &self.inner else { return };
        if !inner.events.contains(&ev) {
            return;
        }
        let body = json!({
            "username": inner.username,
            "embeds": [{
                "title": title.into(),
                "description": description.into(),
                "color": ev.color(),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }]
        });
        let inner2 = inner.clone();
        let h = tokio::spawn(async move { inner2.post(body).await });
        let mut p = inner.pending.lock().unwrap();
        p.retain(|h| !h.is_finished());
        p.push(h);
    }

    /// Wait (bounded) for queued notifications, e.g. before exiting.
    pub async fn flush(&self) {
        let Some(inner) = &self.inner else { return };
        let handles: Vec<_> = inner.pending.lock().unwrap().drain(..).collect();
        for h in handles {
            let _ = tokio::time::timeout(Duration::from_secs(10), h).await;
        }
    }
}

impl Inner {
    async fn post(&self, body: serde_json::Value) {
        for attempt in 0..3 {
            match self.http.post(&self.url).json(&body).send().await {
                Ok(r) if r.status().is_success() => return,
                Ok(r) if r.status().as_u16() == 429 => {
                    // Discord tells us how long to wait.
                    let wait = r
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                        .unwrap_or(2.0);
                    tokio::time::sleep(Duration::from_secs_f64(wait.min(30.0) + 0.25)).await;
                }
                Ok(r) => {
                    tracing::warn!("discord webhook returned HTTP {}", r.status());
                    return;
                }
                Err(e) if attempt < 2 => {
                    tracing::debug!("discord webhook failed: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => tracing::warn!("discord webhook failed: {e}"),
            }
        }
    }
}
