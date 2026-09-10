# frozen_string_literal: true

require "/opt/discourse-test/allow_internal_activitypub_fetches"

# `RUBYOPT` loads this before Rails and the ActivityPub plugin. Keep a small
# trace active so development class reloads also receive the harness patches.
DISCOURSE_TEST_ACTIVITY_PUB_PATCHER =
  TracePoint.new(:end) do
    next unless defined?(DiscourseActivityPub::Request)

    request_class = DiscourseActivityPub::Request
    private_fetch_patch = DiscourseTestAllowInternalActivityPubFetches

    request_class.prepend(private_fetch_patch) unless request_class < private_fetch_patch
  end
DISCOURSE_TEST_ACTIVITY_PUB_PATCHER.enable
