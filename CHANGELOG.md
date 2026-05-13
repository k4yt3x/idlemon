# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2024-05-13

### Added

- Initial `idlemon` daemon implementing `org.gnome.Mutter.IdleMonitor` on the session bus.
- X11 idle-time source via the MIT-SCREEN-SAVER extension (`XScreenSaverQueryInfo`).
- `GetIdletime`, `AddIdleWatch`, `AddUserActiveWatch`, `RemoveWatch` methods and `WatchFired` signal.
- 1 Hz poll loop firing idle and user-active watches per Mutter semantics.
- Single-instance enforcement and clean shutdown on SIGINT/SIGTERM.
- CI workflow running `cargo fmt`, `clippy`, `doc`, and `test` on push and pull requests.
