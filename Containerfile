# Potluck team server: the binary is built on the official Rust image, then copied into
# slim Debian with git, running as a fixed non-root uid with /data as its only volume.
# Names are fully qualified because podman refuses short names without search registries.
FROM docker.io/library/rust:1.100.0-slim-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN CARGO_PROFILE_RELEASE_STRIP=true cargo build --release --locked -p potluck

FROM docker.io/library/debian:trixie-slim
ARG VERSION=dev
ARG REVISION=unknown
ARG CREATED=1970-01-01T00:00:00Z
LABEL org.opencontainers.image.title="potluck" \
      org.opencontainers.image.description="Self-hosted team server for local-first desktop apps" \
      org.opencontainers.image.source="https://github.com/PhongGuy/potluck" \
      org.opencontainers.image.url="https://github.com/PhongGuy/potluck" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"

# git serves the workspaces; the potluck user owns /data, which a new named volume inherits.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 65532 potluck \
    && useradd --system --uid 65532 --gid 65532 --home-dir /data --no-create-home \
       --shell /usr/sbin/nologin potluck \
    && mkdir /data \
    && chown 65532:65532 /data

COPY --from=builder /src/target/release/potluck /usr/local/bin/potluck

ENV POTLUCK_DATA=/data \
    POTLUCK_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
USER 65532:65532
WORKDIR /data
# health-check reads POTLUCK_DATA to find admin.sock, so it needs no flags.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 CMD ["potluck", "health-check"]
# tini is PID 1 so orphans (detached git maintenance, killed hook descendants) are reaped and
# signals reach potluck; -s keeps that true under an engine's own --init.
ENTRYPOINT ["tini", "-s", "--", "potluck"]
CMD ["serve"]
