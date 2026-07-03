# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      clang \
      libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

# wget is the healthcheck probe: bookworm-slim ships neither curl nor wget,
# and it is a few MB smaller than curl.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates \
      libstdc++6 \
      wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group ads \
    && mkdir -p /data \
    && chown ads:ads /data

COPY --from=builder /app/target/release/ads /usr/local/bin/ads

EXPOSE 8787

# Store and workspace from CMD live under /data so they survive container
# replacement.
VOLUME /data

USER ads

# The / static route is the only unauthenticated endpoint, which makes it
# the liveness probe (API routes need the bearer token).
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8787/ || exit 1

# `ads serve` exits at startup unless ADS_WEB_TOKEN (or --auth-token)
# provides the API bearer token; supply it at run time:
#   docker run -e ADS_WEB_TOKEN=... -p 8787:8787 -v ads-data:/data IMAGE
CMD ["ads", "serve", "--bind", "0.0.0.0:8787", "--store", "/data/store", "--workspace", "/data/workspace"]
