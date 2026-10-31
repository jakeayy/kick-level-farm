//! Thin client for the (undocumented) endpoints the kick.com web app uses.
//! All of them accept the `session_token` cookie value as a Bearer token.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode, header};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";
/// Public constant shipped in the web bundle as NEXT_PUBLIC_WEBSOCKET_CLIENT_TOKEN.
pub const WS_CLIENT_TOKEN: &str = "e1393935a959b4020a4491574f6490129f678acdaa92760471263db43487f823";
pub const VIEWER_WS_URL: &str = "wss://websockets.kick.com/viewer/v1/connect";

const MAIN: &str = "https://kick.com";
const WEB: &str = "https://web.kick.com/api";
const WS_API: &str = "https://websockets.kick.com";

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    NotFound,
    RateLimited,
    Status(StatusCode, String),
}

impl std::error::Error for ApiError {}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "unauthorized — session token invalid or expired"),
            ApiError::NotFound => write!(f, "not found"),
            ApiError::RateLimited => write!(f, "rate limited (429)"),
            ApiError::Status(s, b) => write!(f, "HTTP {s}: {}", b.chars().take(200).collect::<String>()),
        }
    }
}

#[derive(Clone)]
pub struct KickClient {
    http: reqwest::Client,
    token: String,
}

// ---------- response types ----------

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: u64,
    pub username: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Level {
    pub level: u32,
    pub progress_xp: u64,
    pub total_xp: u64,
    pub xp_to_next_level: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelInfo {
    pub id: u64,
    #[serde(default)]
    pub is_banned: bool,
    pub livestream: Option<LiveInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveInfo {
    pub id: u64,
    #[serde(default)]
    pub is_live: bool,
    pub vod_id: Option<String>,
    #[serde(default, alias = "viewers")]
    pub viewer_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PointsSettings {
    #[serde(default)]
    pub channel_points_enabled: bool,
    #[serde(default = "default_interval")]
    pub web_watch_event_interval_seconds: u64,
}
fn default_interval() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize)]
pub struct Challenge {
    pub id: String,
    pub name: String,
    pub status: String,
    pub condition: ChallengeCondition,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChallengeCondition {
    #[serde(default)]
    pub progress: u64,
    #[serde(default)]
    pub threshold: u64,
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl Challenge {
    pub fn is_claimable(&self) -> bool {
        let s = self.status.to_ascii_lowercase();
        if matches!(s.as_str(), "claimed" | "expired" | "locked") {
            return false;
        }
        matches!(s.as_str(), "completed" | "claimable" | "ready" | "ready_to_claim")
            || (self.condition.threshold > 0 && self.condition.progress >= self.condition.threshold)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FollowedChannel {
    pub channel_slug: String,
    #[serde(default)]
    pub is_live: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct FollowedPage {
    #[serde(rename = "nextCursor")]
    next_cursor: Option<Value>,
    #[serde(default)]
    channels: Vec<FollowedChannel>,
}

#[derive(Debug, Clone)]
pub struct PopularStream {
    pub slug: String,
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

// ---------- client ----------

impl KickClient {
    pub fn new(token: String) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse()?);
        headers.insert(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9".parse()?);
        headers.insert("x-app-platform", "web".parse()?);
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { http, token })
    }

    async fn request<T: DeserializeOwned>(&self, method: Method, url: &str) -> Result<T> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let res = self
                .http
                .request(method.clone(), url)
                .bearer_auth(&self.token)
                .header(header::ORIGIN, MAIN)
                .header(header::REFERER, format!("{MAIN}/"))
                .send()
                .await;
            let res = match res {
                Ok(r) => r,
                Err(e) if attempt < 3 => {
                    tracing::debug!("request {url} failed ({e}), retrying");
                    tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("{method} {url}")),
            };
            let status = res.status();
            if status.is_success() {
                let body = res.text().await?;
                return serde_json::from_str(&body).with_context(|| {
                    format!("decoding {url}: {}", body.chars().take(300).collect::<String>())
                });
            }
            match status {
                StatusCode::UNAUTHORIZED => return Err(ApiError::Unauthorized.into()),
                StatusCode::NOT_FOUND => return Err(ApiError::NotFound.into()),
                StatusCode::TOO_MANY_REQUESTS if attempt < 4 => {
                    let wait = 5 * attempt as u64 + fastrand::u64(0..5);
                    tracing::warn!("rate limited on {url}, backing off {wait}s");
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                StatusCode::TOO_MANY_REQUESTS => return Err(ApiError::RateLimited.into()),
                s if s.is_server_error() && attempt < 3 => {
                    tokio::time::sleep(Duration::from_secs(3 * attempt as u64)).await;
                }
                s => {
                    let body = res.text().await.unwrap_or_default();
                    // Cloudflare challenge pages come back as 403 HTML.
                    if s == StatusCode::FORBIDDEN && body.contains("cf-") {
                        bail!("blocked by Cloudflare on {url} (HTTP 403)");
                    }
                    return Err(ApiError::Status(s, body).into());
                }
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.request(Method::GET, url).await
    }

    pub async fn me(&self) -> Result<User> {
        self.get(&format!("{MAIN}/api/v1/user")).await
    }

    /// Global account level (the `level` chat badge).
    pub async fn level(&self) -> Result<Level> {
        Ok(self.get::<Data<Level>>(&format!("{WEB}/v1/gamification/user/level")).await?.data)
    }

    pub async fn channel_info(&self, slug: &str) -> Result<ChannelInfo> {
        self.get(&format!("{MAIN}/api/v2/channels/{slug}/info")).await
    }

    /// Channel points balance on a channel.
    pub async fn points(&self, slug: &str) -> Result<i64> {
        #[derive(Deserialize)]
        struct P {
            points: i64,
        }
        Ok(self.get::<Data<P>>(&format!("{MAIN}/api/v2/channels/{slug}/points")).await?.data.points)
    }

    pub async fn points_settings(&self, slug: &str) -> Result<PointsSettings> {
        Ok(self
            .get::<Data<PointsSettings>>(&format!(
                "{MAIN}/api/v2/channels/{slug}/channel-points/channel-settings"
            ))
            .await?
            .data)
    }

    pub async fn challenges(&self) -> Result<Vec<Challenge>> {
        Ok(self.get::<Data<Vec<Challenge>>>(&format!("{WEB}/v1/gamification/challenges")).await?.data)
    }

    pub async fn claim_challenge(&self, id: &str) -> Result<Value> {
        self.request(Method::POST, &format!("{WEB}/v1/gamification/challenges/{id}/claim")).await
    }

    pub async fn streak(&self) -> Result<Value> {
        Ok(self.get::<Data<Value>>(&format!("{WEB}/v1/gamification/streak")).await?.data)
    }

    /// Interval (seconds) the web client uses for `tracking.user.watch.livestream`.
    pub async fn watch_event_interval(&self) -> Result<u64> {
        let v: Value = self.get(&format!("{MAIN}/api/internal/settings/global")).await?;
        v.pointer("/events/tracking.user.watch.livestream/interval")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("interval missing from global settings"))
    }

    pub async fn followed(&self) -> Result<Vec<FollowedChannel>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let url = match &cursor {
                Some(c) => format!("{MAIN}/api/v2/channels/followed?cursor={c}"),
                None => format!("{MAIN}/api/v2/channels/followed"),
            };
            let page: FollowedPage = self.get(&url).await?;
            let n = page.channels.len();
            out.extend(page.channels);
            cursor = match page.next_cursor {
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(Value::String(s)) if !s.is_empty() => Some(s),
                _ => None,
            };
            if cursor.is_none() || n == 0 {
                break;
            }
        }
        Ok(out)
    }

    pub async fn popular_streams(&self, lang: &str, limit: u32) -> Result<Vec<PopularStream>> {
        let v: Value = self
            .get(&format!("{MAIN}/stream/livestreams/{lang}?page=1&limit={limit}&sort=desc"))
            .await?;
        let list = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|s| {
                Some(PopularStream {
                    slug: s.pointer("/channel/slug")?.as_str()?.to_string(),
                })
            })
            .collect())
    }

    /// One-shot token for the viewer websocket (`/viewer/v1/connect?token=`).
    pub async fn viewer_token(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct T {
            token: String,
        }
        let res = self
            .http
            .get(format!("{WS_API}/viewer/v1/token"))
            .bearer_auth(&self.token)
            .header("X-CLIENT-TOKEN", WS_CLIENT_TOKEN)
            .header(header::ORIGIN, MAIN)
            .header(header::REFERER, format!("{MAIN}/"))
            .send()
            .await?;
        match res.status() {
            s if s.is_success() => Ok(res.json::<Data<T>>().await?.data.token),
            StatusCode::UNAUTHORIZED => Err(ApiError::Unauthorized.into()),
            s => Err(ApiError::Status(s, res.text().await.unwrap_or_default()).into()),
        }
    }
}

pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Unauthorized))
}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::NotFound))
}
