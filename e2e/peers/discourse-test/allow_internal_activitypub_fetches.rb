# frozen_string_literal: true

# The ActivityPub plugin performs an additional raw-IP check after Discourse's
# SSRF detector has already accepted a hostname from allowed_internal_hosts.
# That makes local peers resolve successfully and then get rejected solely
# because Docker assigns them an RFC1918 address. Keep the bypass scoped to
# explicitly allowlisted hostnames in this disposable development instance.
module DiscourseTestAllowInternalActivityPubFetches
  private

  def disallowed_ip?(host)
    original_host = uri&.host

    if original_host.present? &&
         FinalDestination::SSRFDetector.host_bypasses_checks?(original_host)
      return false
    end

    super
  end
end
