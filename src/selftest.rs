//! `kick-level-farm selftest`: checks that Kick's public API still matches what the bot
//! expects, using the bot's own client and response types (no login needed).
//! Run nightly in CI so Kick-side changes are noticed before users report them.

use crate::api::KickClient;
use anyhow::{Result, bail};

struct Report {
    failed: usize,
}

impl Report {
    fn ok(&self, name: &str, detail: impl std::fmt::Display) {
        println!("✓ {name}: {detail}");
    }

    fn warn(&self, name: &str, detail: impl std::fmt::Display) {
        println!("⚠ {name}: {detail}");
    }

    fn fail(&mut self, name: &str, detail: impl std::fmt::Display) {
        self.failed += 1;
        println!("✗ {name}: {detail}");
    }
}

pub async fn run() -> Result<()> {
    // Anonymous client: an empty token sends no Authorization header.
    let client = KickClient::new(String::new())?;
    let mut r = Report { failed: 0 };

    match client.watch_event_interval().await {
        Ok(120) => r.ok("watch-event interval", "120s"),
        Ok(s) => r.warn(
            "watch-event interval",
            format!("{s}s (was 120s; the bot adapts, but RESEARCH.md should be updated)"),
        ),
        Err(e) => r.fail("watch-event interval", format!("{e:#}")),
    }

    let popular = match client.popular_streams("en", 10).await {
        Ok(p) if !p.is_empty() => {
            r.ok("popular streams", format!("{} live", p.len()));
            p
        }
        Ok(_) => {
            r.fail("popular streams", "empty list");
            Vec::new()
        }
        Err(e) => {
            r.fail("popular streams", format!("{e:#}"));
            Vec::new()
        }
    };

    // Channel info must expose what the viewer socket needs: ids, vod_id, categories.
    let mut category = None;
    for p in popular.iter().take(5) {
        match client.channel_info(&p.slug).await {
            Ok(info) => match info.livestream.filter(|l| l.is_live) {
                Some(l) if l.vod_id.is_some() && !l.categories.is_empty() => {
                    category = l.categories.first().map(|c| c.id);
                    r.ok(
                        "channel info",
                        format!(
                            "{} → channel {}, livestream {}, category {}",
                            p.slug,
                            info.id,
                            l.id,
                            l.category_names()
                        ),
                    );
                    break;
                }
                Some(_) => r.warn(
                    "channel info",
                    format!("{}: missing vod_id or categories", p.slug),
                ),
                None => continue, // went offline between the two calls
            },
            Err(e) => {
                r.fail("channel info", format!("{}: {e:#}", p.slug));
                break;
            }
        }
    }
    if category.is_none() && !popular.is_empty() {
        r.fail(
            "channel info",
            "no live channel exposed vod_id + categories",
        );
    }

    if let Some(cat) = category {
        match client.live_in_category(cat, 5).await {
            Ok(v) if !v.is_empty() => {
                r.ok("live by category", format!("{} in category {cat}", v.len()))
            }
            Ok(_) => r.fail("live by category", format!("empty for category {cat}")),
            Err(e) => r.fail("live by category", format!("{e:#}")),
        }
    }

    match client.drop_campaigns().await {
        Ok(c) => {
            let active = c.iter().filter(|x| x.status == "active").count();
            r.ok(
                "drop campaigns",
                format!("{} total, {active} active", c.len()),
            );
        }
        Err(e) => r.fail("drop campaigns", format!("{e:#}")),
    }

    // Without a session the viewer-token endpoint must refuse cleanly (403/401), not change shape.
    match client.viewer_token().await {
        Ok(_) => r.warn(
            "viewer token",
            "issued without login (unexpected, check auth flow)",
        ),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("403") || msg.contains("unauthorized") {
                r.ok("viewer token", "refused without login, as expected");
            } else {
                r.fail("viewer token", msg);
            }
        }
    }

    if r.failed > 0 {
        bail!("{} check(s) failed: Kick's API may have changed", r.failed);
    }
    println!("all checks passed");
    Ok(())
}
