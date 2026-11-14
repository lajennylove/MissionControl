# MissionControl

Mission Control: a WordPress-compatible CMS in Rust, built on Rocket with Propel, Ignite and Orbit.

Part of the RocketBoost family: [Propel](https://github.com/lajennylove/Propel) (templates), [Ignite](https://github.com/lajennylove/Ignite) (admin panel) and [Orbit](https://github.com/lajennylove/Orbit) (ORM).

Work in progress.

## Crates

- `mission-control`: the library entry point; builds the Rocket application for the prebuilt binary and for a site's own binary.
- `mission-control-core`: content, users, hooks and configuration.
- `mission-control-app`: the prebuilt binary; adds setup mode and restart on top of the library.
- `mission-control-testing`: shared test helpers, SQLite and MariaDB test database setup.

## License

GPL-2.0-or-later, like WordPress. See `LICENSE`. Copyright (c) 2026 Jenny Martinez.
