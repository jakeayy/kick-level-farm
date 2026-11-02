# Changelog

All notable changes to kick-level-farm are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

Add user-facing changes under **Unreleased** as you make them. When releasing, rename that
section to the new version (`## [x.y.z] - YYYY-MM-DD`), bump `version` in `Cargo.toml`, and
the release workflow uses that section as the release notes.

## [Unreleased]

### Added

- Docker images of releases are tagged with their version (e.g. `0.2.0` and `0.2`), so a
  deployment can be pinned to a release.

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

[Unreleased]: https://github.com/jakeayy/kick-level-farm/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jakeayy/kick-level-farm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jakeayy/kick-level-farm/releases/tag/v0.1.0
