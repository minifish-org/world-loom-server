FROM rust:1.92-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime

ARG APP_UID=10001
ARG APP_GID=10001

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid "${APP_GID}" world-loom \
    && useradd --uid "${APP_UID}" --gid "${APP_GID}" --no-create-home world-loom \
    && install -d -o "${APP_UID}" -g "${APP_GID}" /var/lib/world-loom

COPY --from=builder /src/target/release/world-loom-server /usr/local/bin/world-loom-server

ENV WORLD_LOOM_GAME_ADDR=0.0.0.0:25565 \
    WORLD_LOOM_BRIDGE_ADDR=0.0.0.0:18081 \
    WORLD_LOOM_DB_PATH=/var/lib/world-loom/world-loom.sqlite3 \
    WORLD_LOOM_REGION_DIR=/var/lib/world-loom/anvil/region \
    WORLD_LOOM_HEALTH_ORIGIN=http://localhost:3000 \
    WORLD_LOOM_VIEW_DISTANCE_CHUNKS=6

USER ${APP_UID}:${APP_GID}
WORKDIR /var/lib/world-loom

EXPOSE 18081 25565
VOLUME ["/var/lib/world-loom"]

HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=4 \
  CMD curl --fail --silent --show-error \
      --header "Origin: ${WORLD_LOOM_HEALTH_ORIGIN}" \
      http://127.0.0.1:18081/api/vm/net/connect >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/world-loom-server"]
