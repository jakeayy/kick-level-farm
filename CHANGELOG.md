# Changelog

All notable changes to kick-level-farm are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

Add user-facing changes under **Unreleased** as you make them. When releasing, rename that
section to the new version (`## [x.y.z] - YYYY-MM-DD`), bump `version` in `Cargo.toml`, and
the release workflow uses that section as the release notes.

## [Unreleased]

## [0.2.1] - 2026-11-13

### Added

- Docker images of releases are tagged with their version (e.g. `0.2.0` and `0.2`), so a
  deployment can be pinned to a release.

### Changed

- Drop planning reuses channel checks from the last 5 minutes instead of re-checking offline
  participating channels on every refresh, for every campaign.
- Channel points are fetched only for channels that are live, not for every followed channel
  on the first check.
- `analytics.jsonl` is rotated at 10 MB. The previous file is kept as `analytics.jsonl.1`.

### Fixed

- The Daily Reward (and other daily challenges) is claimed every day again, not only on the
  first day after a start. Kick reuses the same challenge id every day, and the bot remembered
  claims by id alone (#6).
- The login instructions no longer claim that the `session_token` always starts with `kat_`.
  Some accounts have a `12345678%7C…` token instead, which already worked (#2).
- An unfollowed channel is dropped right away. Before, the bot could keep a watch slot on it
  after its stream ended, until it was restarted.
- A viewer connection that dies silently (router restart, VPN or IP change) is noticed within
  75 seconds and reconnected. Before, watch time could be lost for up to about 15 minutes.
- When Kick is slow, delayed refreshes and progress checks no longer run back to back.
- Stopping the bot (Ctrl+C, `docker stop`) no longer waits for a slow refresh, and the final
  check is time-limited, so the summary and the "stopped" notification go out in time.
- Claims and unboxing aren't resent after a timeout. This logged misleading "claiming …
  failed" warnings when the first attempt had worked.
- `kick-level-farm login` creates the token file readable only by you from the start. Before,
  it was briefly readable by other users.
- Channel links in `streamers` such as `https://kick.com/xqc/`, `kick.com/xqc` or
  `www.kick.com/xqc` work. Before, they were treated as nonexistent channels.

## [0.2.0] - 2026-11-02

### Added

- **Rewards are unboxed automatically** (`[farm] unbox_rewards`), so won emotes and badges are
  ready to use right away.
- **Health checks:** the bot writes a heartbeat file, `kick-level-farm health` reports whether
  it's alive (used by the Docker image's `HEALTHCHECK`), and `[health] heartbeat_url` pings
  services like healthchecks.io or Uptime Kuma.
- **Config changes apply live:** edit `config.toml` while the bot runs, and streamers,
  priorities, drops, notification and timing settings update within about 10 seconds.
- `kick-level-farm selftest` checks that Kick's public API still matches what the bot expects.
- Release downloads and Docker images come with signed build provenance
  (`gh attestation verify`).

### Changed

- The bot always watches up to **2** streams. `max_concurrent` was removed because Kick pays
  channel points on at most 2 channels at once. Old configs that still set it keep working, and
  the setting is ignored with a warning.

## [0.1.0] - 2026-11-02

First release.

### Added

- Level XP, channel points and Daily Reward farming by emulating the kick.com web player.
- Farming every followed channel by default, and a popular-stream fallback (level XP only).
- Drop farming and auto-claiming, prioritising badges, then emotes, then items (items opt-in).
- Realtime updates for points, level-ups, channels going live and category changes.
- Discord webhook notifications.
- Docker image (amd64/arm64) and static Linux and Windows binaries.

[Unreleased]: https://github.com/jakeayy/kick-level-farm/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/jakeayy/kick-level-farm/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jakeayy/kick-level-farm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jakeayy/kick-level-farm/releases/tag/v0.1.0
