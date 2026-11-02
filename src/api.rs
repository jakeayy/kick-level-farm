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
pub const WS_CLIENT_TOKEN: &str =
    "e1393935a959b4020a4491574f6490129f678acdaa92760471263db43487f823";
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
            ApiError::Status(s, b) => {
                write!(f, "HTTP {s}: {}", b.chars().take(200).collect::<String>())
            }
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
    #[serde(default)]
    pub categories: Vec<LiveCategory>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveCategory {
    pub id: u64,
    #[serde(default)]
    pub name: String,
}

impl LiveInfo {
    pub fn in_category(&self, category_id: u64) -> bool {
        self.categories.iter().any(|c| c.id == category_id)
    }

    pub fn category_names(&self) -> String {
        let n: Vec<&str> = self.categories.iter().map(|c| c.name.as_str()).collect();
        if n.is_empty() {
            "?".into()
        } else {
            n.join(", ")
        }
    }
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
        matches!(
            s.as_str(),
            "completed" | "claimable" | "ready" | "ready_to_claim"
        ) || (self.condition.threshold > 0 && self.condition.progress >= self.condition.threshold)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoxedReward {
    pub id: String,
    #[serde(default)]
    pub display_label: String,
    #[serde(default)]
    pub rarity: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub source_label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DropCampaign {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub progress_units: u64,
    #[serde(default)]
    pub rewards: Vec<DropReward>,
    #[serde(default)]
    pub user_app_connected: bool,
    #[serde(default)]
    pub connect_url: String,
    pub category: Option<DropCategory>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DropCategory {
    #[serde(default)]
    pub id: u64,
    pub name: String,
}

/// A campaign from the public list (`/v1/drops/campaigns`).
#[derive(Debug, Clone, Deserialize)]
pub struct ActiveCampaign {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub status: String,
    pub starts_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub connect_url: String,
    pub category: DropCategory,
    /// Participating channels. Empty = any stream in the category counts.
    #[serde(default)]
    pub channels: Vec<CampaignChannel>,
    #[serde(default)]
    pub rewards: Vec<CampaignReward>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CampaignChannel {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CampaignReward {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub required_units: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DropReward {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub claimed: bool,
    /// 0.0 ..= 1.0
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub required_units: u64,
}

impl DropReward {
    pub fn is_claimable(&self, campaign_units: u64) -> bool {
        !self.claimed
            && (self.progress >= 0.9999
                || (self.required_units > 0 && campaign_units >= self.required_units))
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
        self.request_body(method, url, None).await
    }

    async fn post_json<T: DeserializeOwned>(&self, url: &str, body: Value) -> Result<T> {
        self.request_body(Method::POST, url, Some(&body)).await
    }

    async fn request_body<T: DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .request(method.clone(), url)
                .header(header::ORIGIN, MAIN)
                .header(header::REFERER, format!("{MAIN}/"));
            // An empty token = anonymous client (public endpoints only, used by `selftest`).
            if !self.token.is_empty() {
                req = req.bearer_auth(&self.token);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let res = req.send().await;
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
                    format!(
                        "decoding {url}: {}",
                        body.chars().take(300).collect::<String>()
                    )
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
        // An invalid/expired token gets HTTP 200 with `{}` here, not a 401.
        let v: Value = self.get(&format!("{MAIN}/api/v1/user")).await?;
        if v.get("id").is_none() {
            return Err(ApiError::Unauthorized.into());
        }
        Ok(serde_json::from_value(v)?)
    }

    /// Global account level (the `level` chat badge).
    pub async fn level(&self) -> Result<Level> {
        Ok(self
            .get::<Data<Level>>(&format!("{WEB}/v1/gamification/user/level"))
            .await?
            .data)
    }

    pub async fn channel_info(&self, slug: &str) -> Result<ChannelInfo> {
        self.get(&format!("{MAIN}/api/v2/channels/{slug}/info"))
            .await
    }

    /// Channel points balance on a channel.
    pub async fn points(&self, slug: &str) -> Result<i64> {
        #[derive(Deserialize)]
        struct P {
            points: i64,
        }
        Ok(self
            .get::<Data<P>>(&format!("{MAIN}/api/v2/channels/{slug}/points"))
            .await?
            .data
            .points)
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
        Ok(self
            .get::<Data<Vec<Challenge>>>(&format!("{WEB}/v1/gamification/challenges"))
            .await?
            .data)
    }

    pub async fn claim_challenge(&self, id: &str) -> Result<Value> {
        self.request(
            Method::POST,
            &format!("{WEB}/v1/gamification/challenges/{id}/claim"),
        )
        .await
    }

    /// Watch-progress for drop campaigns you have participated in.
    pub async fn drops_progress(&self) -> Result<Vec<DropCampaign>> {
        Ok(self
            .get::<Data<Vec<DropCampaign>>>(&format!("{WEB}/v1/drops/progress"))
            .await?
            .data)
    }

    pub async fn drop_campaigns(&self) -> Result<Vec<ActiveCampaign>> {
        Ok(self
            .get::<Data<Vec<ActiveCampaign>>>(&format!("{WEB}/v1/drops/campaigns"))
            .await?
            .data)
    }

    /// Live channel slugs in a category, most viewers first.
    pub async fn live_in_category(&self, category_id: u64, limit: u32) -> Result<Vec<String>> {
        let v: Value = self
            .get(&format!("{WEB}/v1/livestreams?category_id={category_id}&limit={limit}&sort=viewer_count_desc"))
            .await?;
        Ok(v.pointer("/data/livestreams")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.pointer("/channel/slug")?.as_str().map(str::to_lowercase))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn claim_drop(&self, reward_id: &str, campaign_id: &str) -> Result<Value> {
        self.post_json(
            &format!("{WEB}/v1/drops/claim"),
            serde_json::json!({"reward_id": reward_id, "campaign_id": campaign_id}),
        )
        .await
    }

    /// Websocket URL of the realtime (Centrifugo) service.
    pub async fn realtime_url(&self, user_id: u64) -> Result<String> {
        let v: Value = self
            .post_json(
                &format!("{WEB}/v1/realtime/connection"),
                serde_json::json!({
                    "client": {"id": user_id.to_string(), "type": "web"},
                    "capabilities": {"accepted_providers": [{"provider": "centrifugo"}]}
                }),
            )
            .await?;
        v.pointer("/data/connections")
            .and_then(Value::as_array)
            .and_then(|c| {
                c.iter()
                    .find(|x| x.get("provider").and_then(Value::as_str) == Some("centrifugo"))
                    .and_then(|x| x.pointer("/credentials/url")?.as_str().map(str::to_owned))
            })
            .ok_or_else(|| anyhow!("no centrifugo connection offered: {v}"))
    }

    pub async fn realtime_connection_token(&self, user_id: u64) -> Result<String> {
        #[derive(Deserialize)]
        struct T {
            token: String,
        }
        Ok(self
            .post_json::<Data<T>>(
                &format!("{WEB}/v1/realtime/auth/connection"),
                serde_json::json!({"client_id": user_id.to_string()}),
            )
            .await?
            .data
            .token)
    }

    pub async fn realtime_channel_token(&self, channel: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct T {
            token: String,
        }
        Ok(self
            .post_json::<Data<T>>(
                &format!("{WEB}/v1/realtime/auth/channel"),
                serde_json::json!({"channel": channel}),
            )
            .await?
            .data
            .token)
    }

    /// Rewards won from chests/drops that haven't been opened ("unboxed") on the website yet.
    pub async fn boxed_rewards(&self) -> Result<Vec<BoxedReward>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            rewards: Vec<BoxedReward>,
        }
        Ok(self
            .get::<Data<R>>(&format!("{WEB}/v1/rewards/boxed"))
            .await?
            .data
            .rewards)
    }

    /// Opens boxed rewards so they become usable (same call as the website's unbox animation).
    pub async fn mark_unboxed(&self, ids: &[String]) -> Result<Value> {
        self.post_json(
            &format!("{WEB}/v1/rewards/mark-unboxed"),
            serde_json::json!({ "ids": ids }),
        )
        .await
    }

    pub async fn streak(&self) -> Result<Value> {
        Ok(self
            .get::<Data<Value>>(&format!("{WEB}/v1/gamification/streak"))
            .await?
            .data)
    }

    /// Interval (seconds) the web client uses for `tracking.user.watch.livestream`.
    pub async fn watch_event_interval(&self) -> Result<u64> {
        let v: Value = self
            .get(&format!("{MAIN}/api/internal/settings/global"))
            .await?;
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
            .get(&format!(
                "{MAIN}/stream/livestreams/{lang}?page=1&limit={limit}&sort=desc"
            ))
            .await?;
        let list = v
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture<T: DeserializeOwned>(name: &str) -> T {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn channel_info_live() {
        let info: ChannelInfo = fixture("channel_info.json");
        assert_eq!(info.id, 188532);
        assert!(!info.is_banned);
        let live = info.livestream.expect("live");
        assert!(live.is_live);
        assert_eq!(live.id, 135276139);
        assert_eq!(
            live.vod_id.as_deref(),
            Some("01a192aa-7ca8-7b39-b0ab-8c5718095a8b")
        );
        assert_eq!(live.viewer_count, 9196);
        assert!(live.in_category(15));
        assert!(!live.in_category(14138));
        assert_eq!(live.category_names(), "Just Chatting");
    }

    #[test]
    fn channel_info_offline() {
        let info: ChannelInfo = fixture("channel_info_offline.json");
        assert!(info.livestream.is_none());
    }

    #[test]
    fn level() {
        let l = fixture::<Data<Level>>("level.json").data;
        assert_eq!(
            (l.level, l.progress_xp, l.total_xp, l.xp_to_next_level),
            (13, 219, 2602, 379)
        );
    }

    #[test]
    fn challenge_claimability() {
        let list = fixture::<Data<Vec<Challenge>>>("challenges.json").data;
        let by_id = |id: &str| list.iter().find(|c| c.id == id).unwrap();
        assert!(!by_id("daily").is_claimable(), "22/60 isn't done yet");
        assert!(
            by_id("done").is_claimable(),
            "180/180 in_progress is claimable"
        );
        assert!(!by_id("claimed").is_claimable(), "already claimed");
        assert_eq!(by_id("daily").condition.kind, "watch_time_minutes");
    }

    #[test]
    fn drop_reward_claimability() {
        let progress = fixture::<Data<Vec<DropCampaign>>>("drops_progress.json").data;
        let mw4 = &progress[0];
        assert!(!mw4.rewards[0].is_claimable(mw4.progress_units));
        let riot = &progress[1];
        assert!(!riot.rewards[0].is_claimable(riot.progress_units), "14/30");
        assert!(!riot.user_app_connected && !riot.connect_url.is_empty());
        // Progress reaching 100% completes a reward.
        let done = DropReward {
            progress: 1.0,
            ..riot.rewards[0].clone()
        };
        assert!(done.is_claimable(0));
        // Unit count alone also completes a reward.
        let r = DropReward {
            id: "x".into(),
            name: "x".into(),
            claimed: false,
            progress: 0.0,
            required_units: 30,
        };
        assert!(r.is_claimable(30) && !r.is_claimable(29));
        assert!(!DropReward { claimed: true, ..r }.is_claimable(30));
    }

    #[test]
    fn drop_campaigns_parse() {
        let c = fixture::<Data<Vec<ActiveCampaign>>>("drop_campaigns.json").data;
        assert_eq!(c.len(), 6);
        assert_eq!(c[1].channels[0].slug, "Eddie");
        assert!(c[0].ends_at.is_some());
    }

    #[test]
    fn boxed_rewards_parse() {
        #[derive(Deserialize)]
        struct R {
            rewards: Vec<BoxedReward>,
        }
        let r = fixture::<Data<R>>("boxed_rewards.json").data.rewards;
        assert_eq!(r.len(), 1);
        assert_eq!(
            (
                r[0].display_label.as_str(),
                r[0].rarity.as_str(),
                r[0].kind.as_str()
            ),
            ("Tssk", "uncommon", "emote")
        );
        assert_eq!(r[0].source_label, "Welcome Reward");
    }

    #[test]
    fn unauthorized_detection_survives_context() {
        let e = anyhow::Error::from(ApiError::Unauthorized).context("while doing something");
        assert!(is_unauthorized(&e));
        assert!(!is_not_found(&e));
        assert!(is_not_found(&anyhow::Error::from(ApiError::NotFound)));
    }
}
