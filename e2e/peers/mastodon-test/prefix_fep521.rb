# Reproduces the Mastodon 4.7 window in which `assertionMethod` REPLACED the
# legacy `publicKey` instead of being merged with it — the behaviour live on
# infosec.space, which answers our signed GETs with
#   401 {"error":"Public key not found for key https://…/actor#main-key"}
#
# Introduced by bea833274d (2026-06-19, "Add inbound support for FEP-521a"),
# fixed by ee02364e4b (2026-07-06, mastodon#39725). Mount this initializer into
# web+sidekiq to put the local 4.7 instance back into the broken window:
#
#   docker compose -f docker-compose.yml -f docker-compose.override.yml \
#     -f docker-compose.prefix.yml up -d web sidekiq
#
# Remove the extra file (and `up -d` again) to return to stock 4.7 behaviour.
Rails.application.config.to_prepare do
  ActivityPub::ProcessAccountService.class_eval do
    def public_keys
      @public_keys ||= fep_521a_public_keys.presence || legacy_public_keys
    end
  end
end
