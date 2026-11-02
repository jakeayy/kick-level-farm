//! Decides which drop campaigns are worth actively watching.

use crate::api::{ActiveCampaign, DropCampaign};
use crate::config::DropsConfig;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RewardKind {
    // Order = priority.
    Badge,
    Emote,
    Item,
}

impl RewardKind {
    pub fn label(self) -> &'static str {
        match self {
            RewardKind::Badge => "badge",
            RewardKind::Emote => "emote",
            RewardKind::Item => "item",
        }
    }
}

/// Kick has no reward-type field, so classify by name. Rewards from publishers that need
/// a linked game account (`connect_url`) are in-game items unless named as badge/emote.
pub fn classify(reward_name: &str, needs_link: bool) -> RewardKind {
    let n = reward_name.to_lowercase();
    if n.contains("badge") {
        RewardKind::Badge
    } else if n.contains("emote") {
        RewardKind::Emote
    } else if needs_link {
        RewardKind::Item
    } else {
        // Kick-native campaigns only hand out chat cosmetics.
        RewardKind::Emote
    }
}

#[derive(Debug, Clone)]
pub struct DropPlan {
    pub campaign_id: String,
    pub campaign_name: String,
    /// The next reward we're working towards.
    pub reward_name: String,
    pub kind: RewardKind,
    pub category_id: u64,
    pub category_name: String,
    /// Allowed channel slugs; empty = any live stream in the category.
    pub channels: Vec<String>,
    pub remaining_minutes: u64,
}

impl DropPlan {
    pub fn accepts(&self, slug: &str) -> bool {
        self.channels.is_empty() || self.channels.iter().any(|c| c == slug)
    }
}

/// Campaigns to farm, best first. `skipped` collects human-readable reasons, keyed by campaign id.
pub fn plan(
    campaigns: &[ActiveCampaign],
    progress: &[DropCampaign],
    cfg: &DropsConfig,
    skipped: &mut HashMap<String, String>,
) -> Vec<DropPlan> {
    let now = chrono::Utc::now();
    let progress: HashMap<&str, &DropCampaign> = progress.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut out = Vec::new();

    for c in campaigns {
        if c.status != "active" {
            continue;
        }
        if c.starts_at.is_some_and(|t| t > now) || c.ends_at.is_some_and(|t| t <= now) {
            continue;
        }
        let needs_link = !c.connect_url.is_empty();
        let prog = progress.get(c.id.as_str());
        let units = prog.map(|p| p.progress_units).unwrap_or(0);
        let claimed = |rid: &str| {
            prog.is_some_and(|p| p.rewards.iter().any(|r| r.id == rid && r.claimed))
        };

        // Rewards still to earn, cheapest first.
        let mut open: Vec<_> = c
            .rewards
            .iter()
            .filter(|r| !claimed(&r.id) && units < r.required_units)
            .filter(|r| match classify(&r.name, needs_link) {
                RewardKind::Badge => cfg.badges,
                RewardKind::Emote => cfg.emotes,
                RewardKind::Item => cfg.items,
            })
            .collect();
        open.sort_by_key(|r| r.required_units);
        let Some(next) = open.first() else { continue };

        // Linking can only be checked once the campaign shows up in our progress.
        if needs_link && prog.is_some_and(|p| !p.user_app_connected) {
            skipped.insert(
                c.id.clone(),
                format!(
                    "drop campaign `{}` needs a linked account ({}) — skipping",
                    c.name,
                    c.connect_url.split('?').next().unwrap_or_default()
                ),
            );
            continue;
        }

        let best_kind = open.iter().map(|r| classify(&r.name, needs_link)).min().unwrap_or(RewardKind::Item);
        out.push((
            c.ends_at,
            DropPlan {
                campaign_id: c.id.clone(),
                campaign_name: c.name.clone(),
                reward_name: next.name.clone(),
                kind: best_kind,
                category_id: c.category.id,
                category_name: c.category.name.clone(),
                channels: c.channels.iter().map(|ch| ch.slug.to_lowercase()).collect(),
                remaining_minutes: next.required_units.saturating_sub(units),
            },
        ));
    }

    // badge > emote > item, then the campaign ending soonest, then the closest reward.
    out.sort_by(|(ea, a), (eb, b)| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| ea.unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC).cmp(&eb.unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)))
            .then_with(|| a.remaining_minutes.cmp(&b.remaining_minutes))
    });
    out.into_iter().map(|(_, p)| p).collect()
}
