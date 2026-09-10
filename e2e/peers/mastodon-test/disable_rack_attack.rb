# frozen_string_literal: true

# The disposable federation/e2e stack intentionally drives one Mastodon test
# account through many API calls in a short window. Production throttling makes
# those runs stateful and flaky, so keep Rack::Attack disabled here by explicit
# environment opt-in instead of clearing Redis counters between tests.
if ENV['DISABLE_RACK_ATTACK'] == 'true'
  Rack::Attack.enabled = false
  Rails.logger.info('Rack::Attack disabled by DISABLE_RACK_ATTACK=true')
end
