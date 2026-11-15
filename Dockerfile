# Mission Control container image (GPL-2.0-or-later).
#
# Build:   docker build -t mission-control .
# Run:     docker run -d --name mission-control --restart unless-stopped \
#            -p 8000:8000 -v mission-control-data:/data mission-control
# Setup:   with no configuration the container starts in setup mode: open
#          http://localhost:8000/mc-admin/install.php. When the wizard finishes, the binary exits
#          with status 0 and Docker starts it again in normal mode. That needs
#          `--restart unless-stopped` (or `always`); `on-failure` does not restart status 0.
#          See deploy/docker-compose.yml for the same as a Compose file.
#
# Warning: setup mode is open to the first visitor. Finish setup right after the first start, or
# publish the port on loopback only until it is done: `-p 127.0.0.1:8000:8000`.
#
# No secrets are baked into the image. DATABASE_URL and IGNITE_SECRET_KEY live in /data/.env,
# which the wizard writes into the volume (or pass them with `--env-file` pointing at a
# git-ignored file; never `-e` on a shared shell history).
#
# /data is the install folder and the working directory: .env, database.sqlite, uploads/ and
# themes/. A named volume is filled with the default theme on first start. With a bind mount,
# make the folder writable for uid 10001 first (`chown 10001:10001 ./data`).

# Stage 1: build the binary. The full Rust image has the C compiler SQLite needs.
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mission-control-app

# Stage 2: a slim runtime with only the binary, the default theme and the entrypoint. The
# installer views are compiled into the binary.
FROM debian:bookworm-slim
# CA certificates for TLS connections to a remote database.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin mission-control \
    && mkdir /data \
    && chown mission-control:mission-control /data
COPY --from=builder /src/target/release/mission-control-app /usr/local/bin/mission-control-app
COPY themes /usr/share/mission-control/themes
COPY deploy/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
# Listen on every interface inside the container; publish the port with -p.
ENV ROCKET_ADDRESS=0.0.0.0 \
    ROCKET_PORT=8000
# The install folder; keep it on a volume so .env, the database and uploads survive.
VOLUME /data
WORKDIR /data
# Never run as root.
USER mission-control
EXPOSE 8000
ENTRYPOINT ["docker-entrypoint.sh"]
