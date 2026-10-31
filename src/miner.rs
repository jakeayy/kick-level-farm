use crate::api::{self, KickClient, Level, LiveInfo, User};
use crate::config::{Config, Priority};
use crate::watcher::{Connection, WatchStats, WatchTarget};
use anyhow::{Result, bail};
use serde_json::json;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const POPULAR_CACHE: Duration = Duration::from_secs(600);
const POLITE_DELAY: Duration = Duration::from_millis(350);

#[derive(Debug, Default)]
struct Streamer {
    slug: String,
    /// position in config (`order` priority); fallback streams get usize::MAX
    index: usize,
    fallback: bool,
    channel_id: Option<u64>,
    live: Option<LiveInfo>,
    banned: bool,
    missing: bool,
    points: Option<i64>,
    start_points: Option<i64>,
    points_enabled: Option<bool>,
    watched: Duration,
}

impl Streamer {
    fn new(slug: &str, index: usize, fallback: bool) -> Self {
        Self { slug: slug.to_string(), index, fallback, ..Default::default() }
    }

    fn target(&self) -> Option<WatchTarget> {
        let live = self.live.as_ref()?;
        if !live.is_live || self.banned {
            return None;
        }
        Some(WatchTarget {
            slug: self.slug.clone(),
            channel_id: self.channel_id?,
            livestream_id: live.id,
            vod_id: live.vod_id.clone()?,
        })
    }

    fn viewers(&self) -> u64 {
        self.live.as_ref().map(|l| l.viewer_count).unwrap_or(0)
    }
}

struct Active {
    target: WatchTarget,
    since: Instant,
    conn: Option<Connection>,
}

pub struct Miner {
    cfg: Config,
    client: KickClient,
    user: User,
    interval: Duration,
    stats: Arc<WatchStats>,
    streamers: HashMap<String, Streamer>,
    active: HashMap<String, Active>,
    shared: Option<Connection>,
    popular: Vec<api::PopularStream>,
    popular_at: Option<Instant>,
    start_level: Option<Level>,
    last_level: Option<Level>,
    started: Instant,
    claimed: HashSet<String>,
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
}

impl Miner {
    pub async fn new(cfg: Config, client: KickClient) -> Result<Self> {
        let user = client.me().await.map_err(|e| {
            if api::is_unauthorized(&e) {
                anyhow::anyhow!("session token rejected — run `kick-level-farm login` again")
            } else {
                e
            }
        })?;
        let interval = match cfg.experiments.watch_event_interval_secs {
            Some(s) => {
                tracing::warn!("experiment: overriding watch event interval to {s}s");
                Duration::from_secs(s.max(10))
            }
            None => Duration::from_secs(client.watch_event_interval().await.unwrap_or(120)),
        };
        let mut streamers = HashMap::new();
        for (i, s) in cfg.streamers.iter().enumerate() {
            streamers.entry(s.clone()).or_insert_with(|| Streamer::new(s, i, false));
        }
        Ok(Self {
            cfg,
            client,
            user,
            interval,
            stats: Arc::new(WatchStats::default()),
            streamers,
            active: HashMap::new(),
            shared: None,
            popular: Vec::new(),
            popular_at: None,
            start_level: None,
            last_level: None,
            started: Instant::now(),
            claimed: HashSet::new(),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        tracing::info!(
            "logged in as {} (id {}), watch-event interval {}s, max {} concurrent stream(s), {} configured streamer(s)",
            self.user.username,
            self.user.id,
            self.interval.as_secs(),
            self.cfg.farm.max_concurrent,
            self.streamers.len()
        );
        if self.cfg.experiments.shared_socket {
            tracing::warn!("experiment: multiplexing all channels over one viewer socket");
            self.shared = Some(self.spawn_conn("shared".into()));
        }

        let mut refresh = tokio::time::interval(Duration::from_secs(self.cfg.farm.refresh_live_secs));
        let mut poll = tokio::time::interval(Duration::from_secs(self.cfg.farm.poll_progress_secs));
        poll.reset_after(Duration::from_secs(5)); // let refresh run first
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);

        let result = loop {
            tokio::select! {
                _ = &mut ctrl_c => {
                    tracing::info!("Ctrl-C received, shutting down…");
                    break Ok(());
                }
                _ = refresh.tick() => {
                    if let Err(e) = self.refresh_and_reconcile().await {
                        if api::is_unauthorized(&e) { break Err(e); }
                        tracing::warn!("refresh failed: {e:#}");
                    }
                }
                _ = poll.tick() => {
                    if let Err(e) = self.poll_progress().await {
                        if api::is_unauthorized(&e) { break Err(e); }
                        tracing::warn!("progress poll failed: {e:#}");
                    }
                }
            }
            if self.stats.unauthorized.load(Ordering::Relaxed) {
                break Err(anyhow::anyhow!("viewer socket rejected the session token"));
            }
        };

        // Best-effort final poll for the summary.
        let _ = self.poll_progress().await;
        let slugs: Vec<String> = self.active.keys().cloned().collect();
        for s in slugs {
            self.stop(&s, "shutdown").await;
        }
        if let Some(c) = self.shared.take() {
            c.shutdown().await;
        }
        self.summary();
        result
    }

    fn spawn_conn(&self, label: String) -> Connection {
        Connection::spawn(
            self.client.clone(),
            self.interval,
            self.cfg.experiments.send_handshake,
            self.stats.clone(),
            label,
        )
    }

    // ---------------- live status ----------------

    async fn refresh_and_reconcile(&mut self) -> Result<()> {
        if self.cfg.farm.include_followed {
            match self.client.followed().await {
                Ok(list) => {
                    let base = self.cfg.streamers.len();
                    for (i, f) in list.iter().enumerate() {
                        let slug = f.channel_slug.to_lowercase();
                        let s = self
                            .streamers
                            .entry(slug.clone())
                            .or_insert_with(|| Streamer::new(&slug, base + i, false));
                        // A followed channel that was a fallback becomes a regular one.
                        if s.fallback {
                            s.fallback = false;
                            s.index = base + i;
                        }
                        if !f.is_live {
                            s.live = None;
                        }
                    }
                    // Only hit /info for followed channels that are actually live.
                    let live: HashSet<String> =
                        list.iter().filter(|f| f.is_live).map(|f| f.channel_slug.to_lowercase()).collect();
                    self.refresh_slugs(|s| !s.fallback && (s.index < base || live.contains(&s.slug)))
                        .await?;
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!("could not fetch followed channels: {e:#}");
                    self.refresh_slugs(|s| !s.fallback).await?;
                }
            }
        } else {
            self.refresh_slugs(|s| !s.fallback).await?;
        }
        // Re-check fallback streams we are currently watching.
        let watched_fallbacks: HashSet<String> = self
            .active
            .keys()
            .filter(|k| self.streamers.get(*k).is_some_and(|s| s.fallback))
            .cloned()
            .collect();
        self.refresh_slugs(|s| watched_fallbacks.contains(&s.slug)).await?;

        self.reconcile().await
    }

    async fn refresh_slugs(&mut self, filter: impl Fn(&Streamer) -> bool) -> Result<()> {
        let slugs: Vec<String> = self
            .streamers
            .values()
            .filter(|s| !s.missing && filter(s))
            .map(|s| s.slug.clone())
            .collect();
        for slug in slugs {
            self.refresh_one(&slug).await?;
            tokio::time::sleep(POLITE_DELAY).await;
        }
        Ok(())
    }

    async fn refresh_one(&mut self, slug: &str) -> Result<()> {
        let info = match self.client.channel_info(slug).await {
            Ok(i) => i,
            Err(e) if api::is_not_found(&e) => {
                tracing::warn!("channel `{slug}` does not exist — ignoring it");
                if let Some(s) = self.streamers.get_mut(slug) {
                    s.missing = true;
                    s.live = None;
                }
                return Ok(());
            }
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("info for {slug} failed: {e:#}");
                return Ok(());
            }
        };
        let need_settings;
        {
            let s = self.streamers.get_mut(slug).expect("streamer exists");
            let was_live = s.live.as_ref().is_some_and(|l| l.is_live);
            s.channel_id = Some(info.id);
            s.banned = info.is_banned;
            s.live = info.livestream.filter(|l| l.is_live);
            let is_live = s.live.is_some();
            if is_live != was_live && !s.fallback {
                tracing::info!("{} is now {}", slug, if is_live { "LIVE" } else { "offline" });
            }
            need_settings = is_live && s.points_enabled.is_none();
        }
        if need_settings {
            if let Ok(ps) = self.client.points_settings(slug).await {
                let s = self.streamers.get_mut(slug).unwrap();
                s.points_enabled = Some(ps.channel_points_enabled);
                if ps.web_watch_event_interval_seconds != self.interval.as_secs()
                    && self.cfg.experiments.watch_event_interval_secs.is_none()
                {
                    tracing::debug!(
                        "{slug}: channel watch interval {}s differs from global {}s",
                        ps.web_watch_event_interval_seconds,
                        self.interval.as_secs()
                    );
                }
            }
        }
        Ok(())
    }

    fn cmp_priority(&self, a: &Streamer, b: &Streamer) -> CmpOrdering {
        let opt = |x: Option<i64>| x.unwrap_or(i64::MAX);
        for p in &self.cfg.farm.priority {
            let o = match p {
                Priority::Order => a.index.cmp(&b.index),
                Priority::PointsAscending => opt(a.points).cmp(&opt(b.points)),
                Priority::PointsDescending => b.points.unwrap_or(-1).cmp(&a.points.unwrap_or(-1)),
                Priority::ViewersAscending => a.viewers().cmp(&b.viewers()),
                Priority::ViewersDescending => b.viewers().cmp(&a.viewers()),
                Priority::PointsEnabled => b
                    .points_enabled
                    .unwrap_or(false)
                    .cmp(&a.points_enabled.unwrap_or(false)),
            };
            if o != CmpOrdering::Equal {
                return o;
            }
        }
        a.index.cmp(&b.index)
    }

    async fn reconcile(&mut self) -> Result<()> {
        let max = self.cfg.farm.max_concurrent;
        let mut cands: Vec<&Streamer> = self
            .streamers
            .values()
            .filter(|s| !s.fallback && s.target().is_some())
            .collect();
        cands.sort_by(|a, b| self.cmp_priority(a, b));
        let mut desired: Vec<String> = cands.iter().take(max).map(|s| s.slug.clone()).collect();

        // Popular fallback is for level XP only. XP is per account, so a single stream is
        // enough, and it's only used when none of the configured/followed channels are live.
        if desired.is_empty() && self.cfg.farm.fallback_to_popular {
            self.fill_with_popular(&mut desired, 1).await?;
        }

        let desired_targets: HashMap<String, WatchTarget> = desired
            .iter()
            .filter_map(|s| Some((s.clone(), self.streamers.get(s)?.target()?)))
            .collect();

        // Stop streams that are no longer wanted or whose livestream changed.
        let to_stop: Vec<(String, &'static str)> = self
            .active
            .iter()
            .filter_map(|(slug, a)| match desired_targets.get(slug) {
                None if self.streamers.get(slug).and_then(|s| s.target()).is_none() => {
                    Some((slug.clone(), "went offline"))
                }
                None => Some((slug.clone(), "higher-priority stream available")),
                Some(t) if *t != a.target => Some((slug.clone(), "new livestream session")),
                Some(_) => None,
            })
            .collect();
        for (slug, why) in to_stop {
            self.stop(&slug, why).await;
        }
        for (slug, t) in desired_targets {
            if !self.active.contains_key(&slug) {
                self.start(t);
            }
        }
        if self.active.is_empty() {
            tracing::info!("nothing live to watch right now; checking again in {}s", self.cfg.farm.refresh_live_secs);
        }
        Ok(())
    }

    async fn fill_with_popular(&mut self, desired: &mut Vec<String>, max: usize) -> Result<()> {
        // Keep fallback streams we're already on (sticky) as long as they are live.
        let mut current: Vec<String> = self
            .active
            .keys()
            .filter(|k| self.streamers.get(*k).is_some_and(|s| s.fallback && s.target().is_some()))
            .cloned()
            .collect();
        current.sort();
        for slug in current {
            if desired.len() >= max {
                return Ok(());
            }
            if !desired.contains(&slug) {
                desired.push(slug);
            }
        }
        if desired.len() >= max {
            return Ok(());
        }
        if self.popular_at.is_none_or(|t| t.elapsed() > POPULAR_CACHE) {
            match self.client.popular_streams(&self.cfg.farm.fallback_language, 24).await {
                Ok(p) => {
                    self.popular = p;
                    self.popular_at = Some(Instant::now());
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::warn!("could not fetch popular streams: {e:#}"),
            }
        }
        // Skip the very top (huge streams), pick from the rest for variety.
        let mut pool: Vec<String> = self
            .popular
            .iter()
            .map(|p| p.slug.to_lowercase())
            .filter(|s| !desired.contains(s) && !self.streamers.get(s).is_some_and(|x| !x.fallback))
            .collect();
        fastrand::shuffle(&mut pool);
        for slug in pool {
            if desired.len() >= max {
                break;
            }
            self.streamers
                .entry(slug.clone())
                .or_insert_with(|| Streamer::new(&slug, usize::MAX, true));
            self.refresh_one(&slug).await?;
            if self.streamers.get(&slug).and_then(|s| s.target()).is_some() {
                desired.push(slug);
            }
            tokio::time::sleep(POLITE_DELAY).await;
        }
        Ok(())
    }

    fn start(&mut self, t: WatchTarget) {
        let s = &self.streamers[&t.slug];
        tracing::info!(
            "▶ watching {}{} (channel {}, livestream {}{})",
            t.slug,
            if s.fallback { " [fallback: level XP only]" } else { "" },
            t.channel_id,
            t.livestream_id,
            match s.points_enabled {
                _ if s.fallback => "",
                Some(true) => ", channel points on",
                Some(false) => ", channel points off",
                None => "",
            }
        );
        let conn = match &self.shared {
            Some(shared) => {
                shared.add(t.clone());
                None
            }
            None => {
                let c = self.spawn_conn(t.slug.clone());
                c.add(t.clone());
                Some(c)
            }
        };
        self.active.insert(t.slug.clone(), Active { target: t, since: Instant::now(), conn });
    }

    async fn stop(&mut self, slug: &str, why: &str) {
        let Some(a) = self.active.remove(slug) else { return };
        let elapsed = a.since.elapsed();
        if let Some(s) = self.streamers.get_mut(slug) {
            s.watched += elapsed;
        }
        match a.conn {
            Some(c) => c.shutdown().await,
            None => {
                if let Some(shared) = &self.shared {
                    shared.remove(a.target.channel_id);
                }
            }
        }
        tracing::info!("■ stopped {slug} after {} ({why})", fmt_dur(elapsed));
    }

    // ---------------- progress ----------------

    async fn poll_progress(&mut self) -> Result<()> {
        // Account level / XP
        let level = self.client.level().await?;
        let start = self.start_level.get_or_insert_with(|| level.clone()).clone();
        let gained = level.total_xp.saturating_sub(start.total_xp);
        let since_last = self
            .last_level
            .as_ref()
            .map(|l| level.total_xp.saturating_sub(l.total_xp))
            .unwrap_or(0);
        let hours = self.started.elapsed().as_secs_f64() / 3600.0;
        if let Some(prev) = &self.last_level {
            if level.level > prev.level {
                tracing::info!("★ LEVEL UP! {} -> {}", prev.level, level.level);
            }
        }
        tracing::info!(
            "level {} — {}/{} XP to next (total {}, +{} since last poll, +{} this session{})",
            level.level,
            level.progress_xp,
            level.progress_xp + level.xp_to_next_level,
            level.total_xp,
            since_last,
            gained,
            if hours > 0.05 { format!(", ~{:.0} XP/h", gained as f64 / hours) } else { String::new() }
        );
        self.last_level = Some(level.clone());

        // Channel points on everything we're watching (+ configured streamers once).
        let slugs: Vec<String> = self
            .streamers
            .values()
            // Channel points are only farmed/tracked on configured or followed channels.
            .filter(|s| !s.fallback && !s.missing && (self.active.contains_key(&s.slug) || s.points.is_none()))
            .map(|s| s.slug.clone())
            .collect();
        for slug in slugs {
            match self.client.points(&slug).await {
                Ok(p) => {
                    let s = self.streamers.get_mut(&slug).unwrap();
                    let prev = s.points;
                    s.start_points.get_or_insert(p);
                    s.points = Some(p);
                    if let Some(prev) = prev {
                        if p != prev {
                            tracing::info!(
                                "{slug}: {:+} channel points (now {p}, +{} this session)",
                                p - prev,
                                p - s.start_points.unwrap_or(p)
                            );
                        }
                    }
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::debug!("points for {slug}: {e:#}"),
            }
            tokio::time::sleep(POLITE_DELAY).await;
        }

        if self.cfg.farm.claim_daily_rewards {
            self.check_challenges().await?;
        }
        self.write_analytics(&level);
        Ok(())
    }

    async fn check_challenges(&mut self) -> Result<()> {
        let list = match self.client.challenges().await {
            Ok(l) => l,
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("challenges: {e:#}");
                return Ok(());
            }
        };
        for c in list {
            let key = format!("{}:{}", c.id, c.status);
            if c.is_claimable() && !self.claimed.contains(&c.id) {
                match self.client.claim_challenge(&c.id).await {
                    Ok(v) => {
                        self.claimed.insert(c.id.clone());
                        tracing::info!("🎁 claimed `{}`: {}", c.name, v.to_string().chars().take(400).collect::<String>());
                    }
                    Err(e) => tracing::warn!("claiming `{}` failed: {e:#}", c.name),
                }
            } else if c.condition.kind == "watch_time_minutes" && c.status == "in_progress" {
                tracing::info!("challenge `{}`: {}/{} min watched", c.name, c.condition.progress, c.condition.threshold);
            } else {
                tracing::debug!("challenge `{}` [{key}]", c.name);
            }
        }
        Ok(())
    }

    fn write_analytics(&self, level: &Level) {
        let Some(path) = &self.cfg.experiments.analytics_file else { return };
        let points: HashMap<&str, i64> = self
            .streamers
            .values()
            .filter(|s| !s.fallback)
            .filter_map(|s| Some((s.slug.as_str(), s.points?)))
            .collect();
        let watching: Vec<&str> = self.active.keys().map(String::as_str).collect();
        let events: u64 = self.stats.events.lock().unwrap().values().sum();
        let line = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "level": level.level,
            "total_xp": level.total_xp,
            "progress_xp": level.progress_xp,
            "watching": watching,
            "concurrent": watching.len(),
            "shared_socket": self.cfg.experiments.shared_socket,
            "interval_secs": self.interval.as_secs(),
            "watch_events_sent": events,
            "points": points,
        });
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            tracing::debug!("analytics write failed: {e}");
        }
    }

    fn summary(&self) {
        let elapsed = self.started.elapsed();
        println!("\n──────── session summary ({}) ────────", fmt_dur(elapsed));
        if let (Some(a), Some(b)) = (&self.start_level, &self.last_level) {
            println!(
                "level {} -> {}   XP {} -> {} (+{})",
                a.level,
                b.level,
                a.total_xp,
                b.total_xp,
                b.total_xp.saturating_sub(a.total_xp)
            );
        }
        let mut rows: Vec<&Streamer> = self
            .streamers
            .values()
            .filter(|s| s.watched > Duration::ZERO)
            .collect();
        rows.sort_by(|a, b| b.watched.cmp(&a.watched));
        for s in rows {
            if s.fallback {
                println!("  {:<24} watched {:>7}  [fallback: level XP only]", s.slug, fmt_dur(s.watched));
                continue;
            }
            let gained = match (s.start_points, s.points) {
                (Some(a), Some(b)) => format!("{:+}", b - a),
                _ => "-".into(),
            };
            let events = s.channel_id.map(|id| self.stats.events_for(id)).unwrap_or(0);
            println!(
                "  {:<24} watched {:>7}  events {:>4}  points {:>8} ({gained})",
                s.slug,
                fmt_dur(s.watched),
                events,
                s.points.map(|p| p.to_string()).unwrap_or("-".into()),
            );
        }
    }
}

pub async fn status(cfg: &Config, client: &KickClient) -> Result<()> {
    let me = match client.me().await {
        Ok(u) => u,
        Err(e) if api::is_unauthorized(&e) => bail!("session token rejected — run `kick-level-farm login`"),
        Err(e) => return Err(e),
    };
    println!("account: {} (id {})", me.username, me.id);
    let l = client.level().await?;
    println!(
        "level:   {}  ({}/{} XP to next, total {})",
        l.level,
        l.progress_xp,
        l.progress_xp + l.xp_to_next_level,
        l.total_xp
    );
    if let Ok(s) = client.streak().await {
        println!("streak:  {}", s.get("current_streak").cloned().unwrap_or_default());
    }
    if let Ok(list) = client.challenges().await {
        for c in list {
            println!(
                "challenge: {:<20} {:>4}/{:<4} {:<20} [{}]{}",
                c.name,
                c.condition.progress,
                c.condition.threshold,
                c.condition.kind,
                c.status,
                if c.is_claimable() { "  <- claimable" } else { "" }
            );
        }
    }
    if !cfg.streamers.is_empty() {
        println!("\n{:<24} {:<8} {:>10}  points", "streamer", "live", "viewers");
    }
    for slug in &cfg.streamers {
        let info = client.channel_info(slug).await;
        let pts = client.points(slug).await.ok();
        let enabled = client.points_settings(slug).await.ok().map(|p| p.channel_points_enabled);
        match info {
            Ok(i) => {
                let live = i.livestream.as_ref().filter(|l| l.is_live);
                println!(
                    "{:<24} {:<8} {:>10}  {}{}",
                    slug,
                    if live.is_some() { "LIVE" } else { "offline" },
                    live.map(|l| l.viewer_count.to_string()).unwrap_or_default(),
                    pts.map(|p| p.to_string()).unwrap_or("-".into()),
                    if enabled == Some(false) { " (points disabled)" } else { "" }
                );
            }
            Err(e) => println!("{slug:<24} error: {e:#}"),
        }
        tokio::time::sleep(POLITE_DELAY).await;
    }
    Ok(())
}
