# MissionControl

Mission Control: a WordPress-compatible CMS in Rust, built on Rocket with Propel, Ignite and Orbit.

Part of the RocketBoost family: [Propel](https://github.com/lajennylove/Propel) (templates), [Ignite](https://github.com/lajennylove/Ignite) (admin panel) and [Orbit](https://github.com/lajennylove/Orbit) (ORM).

Work in progress.

## Crates

- `mission-control`: the library entry point; builds the Rocket application for the prebuilt binary and for a site's own binary.
- `mission-control-core`: content, users, hooks and configuration.
- `mission-control-app`: the prebuilt binary; adds setup mode and restart on top of the library.
- `mission-control-testing`: shared test helpers, SQLite and MariaDB test database setup.

## Restart after setup

A site with no `.env` starts in setup mode. When setup finishes, the binary starts again in normal mode:

- **Started by hand:** it shuts down gracefully and replaces itself (`exec`) with the same arguments and environment.
- **Under systemd or a container:** it shuts down gracefully and exits with status 0, and the supervisor starts it again. The restart policy must cover a clean exit: `Restart=always` for systemd (`on-failure` does not restart status 0), `--restart unless-stopped` or `always` for Docker.

If `.env` is broken after setup (for example the database is unreachable), the restarted process stops with an error, and under `Restart=always` or Docker `always` the supervisor starts it again and again: a crash loop. Read the log (`journalctl -u <unit>` or `docker logs`), fix `.env`, and the next start works.

Mission Control detects systemd (`INVOCATION_ID` or `NOTIFY_SOCKET`) and containers (`/.dockerenv`, `/run/.containerenv` or `container`). Set `MC_RESTART=exec` or `MC_RESTART=exit` to choose yourself.

## License

GPL-2.0-or-later, like WordPress. See `LICENSE`. Copyright (c) 2026 Jenny Martinez.
