# Disposable Pleroma-family interop peer for federation / e2e testing against
# Plamenu. Now builds **Akkoma** (a Pleroma hard-fork) rather than upstream
# Pleroma: the real-world servers we interop with run Akkoma, which — unlike
# upstream Pleroma — mirrors a status' emoji_reactions at the TOP LEVEL of the
# Mastodon-API entity (not only under `pleroma`), which is what Phanpy and other
# Pleroma-aware clients read. Kept on the pleroma.local domain / pleroma-web
# alias so nothing else in the stack changes.
#
# Assembles a prod release in-image (self-contained, nothing bind-mounted);
# Akkoma's release runtime provider reads our config at boot from
# $AKKOMA_CONFIG_PATH.
FROM hexpm/elixir:1.15.4-erlang-26.0.2-alpine-3.18.2

ENV MIX_ENV=prod
# epmd on loopback only — single-node box.
ENV ERL_EPMD_ADDRESS=127.0.0.1

# Akkoma's build deps (no libvips: Akkoma uses exiftool/imagemagick, not vix).
RUN apk add --no-cache git gcc g++ musl-dev make cmake file-dev exiftool \
    ffmpeg imagemagick libmagic ncurses postgresql-client openssl

ARG UID=911
ARG GID=911
RUN addgroup -g ${GID} akkoma \
    && adduser -u ${UID} -G akkoma -D -h /opt/akkoma akkoma \
    && mkdir -p /etc/akkoma /var/lib/akkoma/uploads /var/lib/akkoma/static \
    && chown -R akkoma /etc/akkoma /var/lib/akkoma

USER akkoma
WORKDIR /opt/akkoma

# Pin the exact source revision used by the interoperability matrix.
ARG AKKOMA_REF=4ceb81aa7f86af7b093d3d683940933e96e5dfcb
RUN git init /opt/akkoma \
    && git -C /opt/akkoma remote add origin https://akkoma.dev/AkkomaGang/akkoma.git \
    && git -C /opt/akkoma fetch --depth 1 origin ${AKKOMA_REF} \
    && git -C /opt/akkoma checkout --detach FETCH_HEAD

# Empty compile-time secret so `mix release` doesn't evaluate env-driven runtime
# config at build (the ReleaseRuntimeProvider reads it at boot instead).
RUN echo "import Config" > config/prod.secret.exs \
    && mix local.hex --force \
    && mix local.rebar --force \
    && mix deps.get --only prod \
    && mix release --path /opt/akkoma

# Env-driven runtime config, read by Pleroma.Config.ReleaseRuntimeProvider.
# COPY as root (classic builder has no BuildKit --chown/--chmod), then hand the
# config to akkoma at mode 640 — the provider rejects world/group-write, and the
# file must be owner-readable by the akkoma user that boots the release.
USER root
COPY ./akkoma-config.exs /etc/akkoma/config.exs
COPY ./docker-entrypoint.sh /docker-entrypoint.sh
RUN chown akkoma:akkoma /etc/akkoma/config.exs \
    && chmod 640 /etc/akkoma/config.exs \
    && chmod 755 /docker-entrypoint.sh
ENV AKKOMA_CONFIG_PATH=/etc/akkoma/config.exs

USER akkoma
EXPOSE 4000
ENTRYPOINT ["/docker-entrypoint.sh"]
