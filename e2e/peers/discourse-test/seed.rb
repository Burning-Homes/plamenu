# frozen_string_literal: true

domain = "discourse.local"
username = "diana"
email = "diana@#{domain}"
password = "discourse-diana-pass-123"

settings = {
  title: "Discourse ActivityPub interop test",
  force_hostname: domain,
  port: 443,
  force_https: true,
  login_required: false,
  must_approve_users: false,
  activity_pub_enabled: true,
  activity_pub_verbose_logging: true,
  activity_pub_object_logging: true,
  allowed_internal_hosts: %w[
    discourse.local
    mastodon.local
    plamenu.local
    plamenu2.local
    pleroma.local
    plup.local
    sharkey.local
    gotosocial.local
    mitra.local
    lemmy.local
    peertube.local
    mobilizon.local
  ].join("|"),
}

settings.each { |name, value| SiteSetting.set(name, value) }
SiteSetting.refresh!

admin = User.with_email(email).first
unless admin
  admin = User.new(email: email, username: username, password: password)
  admin.save!
end
admin.email_tokens.update_all(confirmed: true)
admin.activate
admin.grant_admin!
admin.change_trust_level!(TrustLevel[4]) if admin.trust_level < TrustLevel[4]

category = Category.find_by(slug: "federation")
category ||=
  Category.create!(
    name: "Federation",
    slug: "federation",
    color: "0088CC",
    text_color: "FFFFFF",
    user: admin,
  )

handler = DiscourseActivityPub::ActorHandler.new(model: category)
actor =
  handler.update_or_create_actor(
    username: "federation",
    name: "Federation (Discourse test)",
    enabled: true,
    default_visibility: "public",
    publication_type: "full_topic",
    post_object_type: "Article",
  )
raise handler.errors.map(&:message).join(", ") unless handler.success?

# Interrupted runs may leave generated followers after Plamenu's peer cache
# has been reset. Their redundant fan-out eventually exhausts this fixture's
# per-IP object-fetch limit. Reset only these disposable follower edges;
# preserve standing accounts and followers from other domains.
test_actors =
  DiscourseActivityPubActor.where(domain: "plamenu.local").where(
    "username ~ ?",
    "^e2e[0-9a-f]{8}$",
  )
removed =
  DiscourseActivityPubFollow.where(followed_id: actor.id, follower_id: test_actors.select(:id))
    .delete_all
puts "Removed #{removed} abandoned E2E followers"

fixture_title = "ActivityPub interoperability fixture"
unless Topic.exists?(title: fixture_title)
  PostCreator.create!(
    admin,
    title: fixture_title,
    raw: "A disposable topic published by the local Discourse ActivityPub test peer.",
    category: category.id,
  )
end

puts "Seeded admin #{username} and actor #{actor.handle} (#{actor.ap_id})"
