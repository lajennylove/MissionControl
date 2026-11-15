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

## Deploy

Both ways start in setup mode when there is no `.env`: open `/wp-admin/install.php`, finish the wizard, and the site restarts into normal mode. The restart needs a policy that covers a clean exit (see above). Neither way holds a secret: `DATABASE_URL` and `IGNITE_SECRET_KEY` live in the install folder's `.env`.

- **systemd:** `deploy/mission-control.service` (`Restart=always`, `RestartSec=2`, runs as the `mission-control` user, read-only file system except the install folder). The binary is installed root-owned at `/usr/local/bin/mission-control-app`; only the install folder is writable for the service. The install steps are in the file's header. CI runs `systemd-analyze verify` on it.
- **Docker:** `docker build -t mission-control .`, then `docker run -d --restart unless-stopped -p 8000:8000 -v mission-control-data:/data mission-control`, or `docker compose -f deploy/docker-compose.yml up -d`. `/data` is the install folder (`.env`, `database.sqlite`, `uploads/`, `themes/`); the default theme is copied there on first start. The container runs as uid 10001, so a bind mount must be writable for it (`chown 10001:10001 ./data`). Setup mode is open to the first visitor, so finish setup right after the first start, or publish the port on loopback only until then (`-p 127.0.0.1:8000:8000`, as the Compose file does). To pass settings instead of using the wizard, use `--env-file` with a git-ignored file, not `-e`. The image listens on `0.0.0.0:8000` (`ROCKET_ADDRESS`, `ROCKET_PORT`); put a reverse proxy with TLS in front.

## License

GPL-2.0-or-later, like WordPress. See `LICENSE`. Copyright (c) 2026 Jenny Martinez.
