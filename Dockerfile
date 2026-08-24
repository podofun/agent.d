# agentd runtime image.
#
# Build:  docker build -t agentd .
# Run:    docker run -p 7777:7777 -v agentd-data:/var/lib/agentd \
#           -v ./examples/docker:/etc/agentd:ro agentd
#
# The native shell sandbox (Landlock + seccomp + network namespaces) is not
# available under default container seccomp profiles, so sandboxed shell
# actions fail closed inside this image — the container boundary is the
# confinement layer. Secrets default to the `env` backend
# (AGENTD_SECRET_<NAME>); set AGENTD_SECRETS=dir:/run/secrets to read
# Docker/Kubernetes secret mounts instead.

FROM rust:1-slim-bookworm AS builder

# mlua vendors Lua 5.4 (needs a C compiler); aws-lc-rs needs cmake.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p daemon -p agentd-cli

FROM debian:bookworm-slim

RUN useradd --system --create-home --home-dir /home/agentd agentd \
    && mkdir -p /var/lib/agentd /etc/agentd \
    && chown -R agentd:agentd /var/lib/agentd

COPY --from=builder /src/target/release/agentd /usr/local/bin/agentd
COPY --from=builder /src/target/release/agentctl /usr/local/bin/agentctl

# Config is mounted at /etc/agentd; state and data live on one volume.
ENV AGENTD_CONFIG=/etc/agentd/config.toml \
    AGENTD_INIT=/etc/agentd/init.lua \
    AGENTD_GRANTS=/etc/agentd/grants.toml \
    XDG_DATA_HOME=/var/lib/agentd \
    XDG_STATE_HOME=/var/lib/agentd \
    AGENTD_ADDR=0.0.0.0:7777 \
    AGENTD_SECRETS=env

USER agentd
VOLUME /var/lib/agentd
EXPOSE 7777

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s CMD ["bash", "-c", \
    "exec 3<>/dev/tcp/127.0.0.1/7777 && printf 'GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3 && grep -q ' 200 ' <&3"]

ENTRYPOINT ["agentd"]
