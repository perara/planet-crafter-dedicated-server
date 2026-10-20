# The dedicated server. Everything it keeps is in /data: worlds/, server.env, rules.json,
# access.json, the game tables (read from the game's files) and backups. Mount your game folder at /game (read-only), or start once with
# -it -e STEAM_USER=yourname to download it. See README "Docker".
FROM rust:1-trixie AS build
WORKDIR /src/server
# .cargo/config.toml: the binary finds libsteam_api.so next to itself.
COPY .cargo/ /src/.cargo/
# The plugin API crate the server builds against (server/Cargo.toml: path = "../plugin-api").
COPY plugin-api/ /src/plugin-api/
COPY server/ ./
RUN cargo build --release --locked \
  && cp "$(ls -t target/release/build/steamworks-sys-*/out/libsteam_api.so | head -1)" target/release/

FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/perara/planet-crafter-dedicated-server" \
      org.opencontainers.image.description="The Planet Crafter dedicated server: hot updates inside the container (docs/docker.md)" \
      org.opencontainers.image.licenses="GPL-3.0-or-later"
# curl and the 32-bit libraries: steamcmd (the game download, steamclient.so for invite codes).
RUN apt-get update \
  && apt-get install -y --no-install-recommends curl ca-certificates lib32gcc-s1 lib32stdc++6 tini \
  && rm -rf /var/lib/apt/lists/*
# One program, /opt/pcds/pcds, with what it uses beside it (as in a release).
COPY --from=build /src/server/target/release/pcds /src/server/target/release/libsteam_api.so /opt/pcds/
# The licences travel with libsteam_api.so (THIRD_PARTY_LICENSES says whose terms are whose).
COPY README.md CHANGELOG.md LICENSE THIRD_PARTY_LICENSES /opt/pcds/
COPY server/web/index.html /opt/pcds/web/index.html
COPY docker/entrypoint.sh /opt/pcds/entrypoint.sh
RUN useradd --uid 1000 --home-dir /data/home --no-create-home pcds \
  && mkdir -p /data/home && chown -R pcds:pcds /data
# The game is the one mounted at /game, or the one STEAM_USER downloaded (pcds container).
ENV PCDS_HOME=/data PCDS_CONTAINER=1 PCDS_ADMIN=0.0.0.0:7782 \
  HOME=/data/home PATH=/opt/pcds:$PATH
VOLUME /data
WORKDIR /data
EXPOSE 7777/udp 7782/tcp
# Healthy while the admin API answers (it answers only once the world runs; the first start
# reads the game tables first). Needs PCDS_ADMIN on, as it is by default.
HEALTHCHECK --interval=30s --timeout=15s --start-period=10m --retries=3 \
  CMD pcds api GET /api/status > /dev/null || exit 1
# The stop (SIGTERM) saves the world: the server needs up to 80 s of the 90 that `docker stop`
# must be given (--stop-timeout 90, Compose stop_grace_period, Kubernetes
# terminationGracePeriodSeconds 120); an image cannot set that itself.
STOPSIGNAL SIGTERM
ENTRYPOINT ["tini", "--", "/opt/pcds/entrypoint.sh"]
