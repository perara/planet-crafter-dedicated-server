# The dedicated server with what the one-time reading of the game's tables needs (Wine,
# Xvfb). Everything it keeps is in /data: worlds/, server.env, rules.json, access.json, the
# game tables and backups. Mount your game folder at /game (read-only), or start once with
# -it -e STEAM_USER=yourname to download it. See README "Docker".
FROM rust:1-trixie AS build
WORKDIR /src/server
# .cargo/config.toml: the binary finds libsteam_api.so next to itself.
COPY .cargo/ /src/.cargo/
COPY server/ ./
RUN cargo build --release --locked \
  && cp "$(ls -t target/release/build/steamworks-sys-*/out/libsteam_api.so | head -1)" target/release/

FROM debian:trixie-slim
# Wine's X11 libraries are only recommended packages: without them it cannot make the game's
# window, even under Xvfb.
RUN apt-get update \
  && apt-get install -y --no-install-recommends wine wine64 xvfb xauth python3 curl unzip ca-certificates \
    lib32gcc-s1 lib32stdc++6 procps tini \
    libxcomposite1 libxcursor1 libxfixes3 libxi6 libxinerama1 libxrandr2 libxrender1 libxxf86vm1 \
    libgl1 libegl1 libdbus-1-3 \
  && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/server/target/release/pcds-server /src/server/target/release/libsteam_api.so /opt/pcds/bin/
COPY extractor/prebuilt/PcdsExtractor.dll /opt/pcds/bin/
COPY server/web/index.html /opt/pcds/web/index.html
COPY pcds README.md LICENSE /opt/pcds/
COPY docker/entrypoint.sh /opt/pcds/entrypoint.sh
RUN useradd --uid 1000 --home-dir /data/home --no-create-home pcds \
  && mkdir -p /data/home && chown -R pcds:pcds /data
ENV PCDS_HOME=/data PCDS_CONTAINER=1 PCDS_GAME_DIR=/game PCDS_ADMIN_BIND=0.0.0.0:7782 \
  HOME=/data/home PATH=/opt/pcds:$PATH
VOLUME /data
WORKDIR /data
EXPOSE 7777/udp 7782/tcp
ENTRYPOINT ["tini", "--", "/opt/pcds/entrypoint.sh"]
