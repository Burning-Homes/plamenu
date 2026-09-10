-- The implicit "Everyone" permission baseline is gone: permissions now come
-- solely from a user's assigned role, and the default "User" role starts with
-- none. Migration 0005 seeded it with invite_users (65536, mirroring
-- Mastodon's Everyone default); invites are now an explicit admin grant.
UPDATE user_roles
SET permissions = permissions & ~65536
WHERE id = 4;
