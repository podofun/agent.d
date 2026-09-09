# agentd runtime image.
#
# Build:  docker build -t agentd .
# Run:    docker run -p 7777:7777 -v agentd-data:/var/lib/agentd \
#           -v ./examples/docker:/etc/agentd:ro agentd
#
# Your Lua userland is mounted at /etc/agentd, not baked in: init.lua plus any
# files it `import()`s, config.toml, and grants.toml. `import()` resolves
# relative to init.lua's directory and refuses to escape it, so a whole
# multi-file userland tree can be mounted read-only.
#
# The native shell sandbox (Landlock + seccomp + network namespaces) is not
# available under default container seccomp profiles, so sandboxed shell
# actions fail closed inside this image — the container boundary is the
# confinement layer. Secrets default to the `env` backend
# (AGENTD_SECRET_<NAME>); set AGENTD_SECRETS=dir:/run/secrets to read
# Docker/Kubernetes secret mounts instead.

FROM rust:1-alpine AS builder

# musl needs the full C toolchain: mlua vendors Lua 5.4, aws-lc-sys needs cmake
# plus clang/llvm for its bindgen fallback, and nix/libc need kernel headers.
RUN apk add --no-cache build-base cmake perl clang-dev llvm-dev linux-headers

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p daemon -p agentd-cli

FROM alpine:latest

RUN adduser -S -D -H -h /var/lib/agentd agentd \
    && mkdir -p /var/lib/agentd /etc/agentd \
    && chown -R agentd:nogroup /var/lib/agentd

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

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD wget -q -O- http://127.0.0.1:7777/health || exit 1

ENTRYPOINT ["agentd"]
