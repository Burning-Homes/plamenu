# syntax=docker/dockerfile:1.7

# Build bases are pinned by manifest-list digest so a mutable tag cannot change
# a release build silently; bump the tag
# and digest together (`docker buildx imagetools inspect <image:tag>`).
FROM rust:1.98.0-alpine3.22@sha256:2e452153b2dc6bed8ef123c4801cfd3f637e7c092846f974ecdce5e64af3120c AS toolchain

RUN apk add --no-cache build-base perl

FROM toolchain AS builder

ARG PLAMENU_VERSION
ARG PLAMENU_BUILD_CHANNEL=development
ARG PLAMENU_BUILD_NUMBER
ARG PLAMENU_GIT_SHA=unknown
# Bound compilation concurrency for the shared 7 GiB CI workers.
ARG PLAMENU_BUILD_JOBS=2

WORKDIR /src
COPY . .
ENV SQLX_OFFLINE=true
RUN test -z "${PLAMENU_VERSION}" || \
      test "$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)" = "${PLAMENU_VERSION}" && \
    PLAMENU_BUILD_CHANNEL="${PLAMENU_BUILD_CHANNEL}" \
    PLAMENU_BUILD_NUMBER="${PLAMENU_BUILD_NUMBER}" \
    PLAMENU_GIT_SHA="${PLAMENU_GIT_SHA}" \
    PLAMENU_GIT_DIRTY=false \
    CARGO_BUILD_JOBS="$PLAMENU_BUILD_JOBS" cargo build --locked --profile deploy -p plamenu && \
    install -Dm755 target/deploy/plamenu /out/plamenu && \
    strip --strip-all /out/plamenu

FROM alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce

ARG PLAMENU_VERSION
ARG PLAMENU_BUILD_CHANNEL=development
ARG PLAMENU_BUILD_NUMBER
ARG PLAMENU_GIT_SHA=unknown
ENV MIMALLOC_ALLOW_THP=0
LABEL org.opencontainers.image.title="Plamenu" \
      org.opencontainers.image.description="ActivityPub server with a Mastodon-compatible API" \
      org.opencontainers.image.url="https://codefloe.com/plamenu/plamenu" \
      org.opencontainers.image.source="https://codefloe.com/plamenu/plamenu" \
      org.opencontainers.image.version="${PLAMENU_VERSION}" \
      org.opencontainers.image.revision="${PLAMENU_GIT_SHA}" \
      org.plamenu.build.channel="${PLAMENU_BUILD_CHANNEL}" \
      org.plamenu.build.number="${PLAMENU_BUILD_NUMBER}" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

RUN apk add --no-cache ca-certificates ffmpeg tzdata && \
    addgroup -S -g 10001 plamenu && \
    adduser -S -D -H -u 10001 -G plamenu plamenu && \
    install -d -o plamenu -g plamenu /var/lib/plamenu/media /etc/plamenu

COPY --from=builder /out/plamenu /usr/local/bin/plamenu

USER 10001:10001
WORKDIR /var/lib/plamenu
VOLUME ["/var/lib/plamenu/media"]
EXPOSE 8420
# Readiness, not bare liveness: `/ready` runs a database round-trip so a
# process that is up but cannot reach its datastore is reported unhealthy
# (and dependents that wait on `service_healthy` keep waiting) instead of
# passing on a constant "OK". `/health` remains the cheap liveness probe.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=5 \
  CMD wget -q -O /dev/null http://127.0.0.1:8420/ready || exit 1
ENTRYPOINT ["plamenu", "--config", "/etc/plamenu/plamenu.toml"]
CMD ["serve"]
