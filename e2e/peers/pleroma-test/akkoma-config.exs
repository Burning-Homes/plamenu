import Config

# Runtime config for the disposable Akkoma instance, read at boot by
# Pleroma.Config.ReleaseRuntimeProvider (AKKOMA_CONFIG_PATH). Based on Akkoma's
# shipped config/docker.exs, with dev/e2e federation overrides folded in. Kept
# on the pleroma.local domain so the rest of the stack is unchanged.

config :pleroma, Pleroma.Web.Endpoint,
  url: [host: System.get_env("DOMAIN", "localhost"), scheme: "https", port: 443],
  http: [ip: {0, 0, 0, 0}, port: 4000]

config :pleroma, :instance,
  name: System.get_env("INSTANCE_NAME", "Pleroma"),
  email: System.get_env("ADMIN_EMAIL"),
  notify_email: System.get_env("NOTIFY_EMAIL"),
  limit: 5000,
  # Single-process, low-noise disposable box: federation on, registrations
  # closed (users are made via pleroma_ctl), no activation gate.
  federating: true,
  account_activation_required: false,
  registrations_open: false,
  healthcheck: true

config :pleroma, Pleroma.Repo,
  adapter: Ecto.Adapters.Postgres,
  username: System.get_env("DB_USER", "pleroma"),
  password: System.fetch_env!("DB_PASS"),
  database: System.get_env("DB_NAME", "pleroma"),
  hostname: System.get_env("DB_HOST", "db"),
  pool_size: 10

# Trust the Caddy "tls internal" CA the same way the Mastodon test box does:
# this instance is disposable, so we simply skip outbound TLS verification
# instead of threading the local CA into the cert bundle. Akkoma federates over
# a Finch/Mint pool, so the knob lives at pools.default.conn_opts.transport_opts
# (the old hackney `ssl_options` form is ignored) — this merges into the pool
# spec built by Pleroma.HTTP.AdapterHelper.
config :pleroma, :http,
  adapter: [
    pools: %{
      default: [
        conn_opts: [transport_opts: [verify: :verify_none]]
      ]
    }
  ]

# No rich-media crawling surprises; keep the outgoing federator prompt.
config :pleroma, :rich_media, enabled: false
config :pleroma, :workers, retries: [federator_outgoing: 1]

config :pleroma, :media_proxy,
  enabled: false,
  redirect_on_failure: true

config :web_push_encryption, :vapid_details, subject: "mailto:#{System.get_env("NOTIFY_EMAIL")}"

config :pleroma, :database, rum_enabled: false
config :pleroma, :instance, static_dir: "/var/lib/akkoma/static"
config :pleroma, Pleroma.Uploaders.Local, uploads: "/var/lib/akkoma/uploads"

# Akkoma (unlike upstream Pleroma) hard-requires an upload base_url at boot.
config :pleroma, Pleroma.Upload,
  base_url: "https://#{System.get_env("DOMAIN", "localhost")}/media/"

# Quieter logs, but keep enough to see signature/federation failures.
config :logger, level: :info

# Secrets can't live in the image; generate once into the persistent volume.
if not File.exists?("/var/lib/akkoma/secret.exs") do
  secret = :crypto.strong_rand_bytes(64) |> Base.encode64() |> binary_part(0, 64)
  signing_salt = :crypto.strong_rand_bytes(8) |> Base.encode64() |> binary_part(0, 8)
  {web_push_public_key, web_push_private_key} = :crypto.generate_key(:ecdh, :prime256v1)

  secret_file =
    EEx.eval_string(
      """
      import Config

      config :pleroma, Pleroma.Web.Endpoint,
        secret_key_base: "<%= secret %>",
        signing_salt: "<%= signing_salt %>"

      config :web_push_encryption, :vapid_details,
        public_key: "<%= web_push_public_key %>",
        private_key: "<%= web_push_private_key %>"
      """,
      secret: secret,
      signing_salt: signing_salt,
      web_push_public_key: Base.url_encode64(web_push_public_key, padding: false),
      web_push_private_key: Base.url_encode64(web_push_private_key, padding: false)
    )

  File.write("/var/lib/akkoma/secret.exs", secret_file)
end

import_config("/var/lib/akkoma/secret.exs")
