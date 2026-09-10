-- The built-in "User" role: an explicit default for every local user. Its
-- permissions mirror the implicit "Everyone" baseline (invite_users only), so
-- holders gain nothing beyond what a role-less user could already do — but the
-- default is now visible and editable in the roles console, and moderators
-- have a target to change accounts back to. Id 4 continues the baseline seed
-- (1 Moderator / 2 Admin / 3 Owner) and is `role::DEFAULT_ROLE_ID` in Rust;
-- runtime-created roles use snowflake ids and never collide.
INSERT INTO user_roles (id, name, color, "position", permissions, highlighted)
    VALUES (4, 'User', '', 0, 65536, FALSE);

UPDATE users SET role_id = 4 WHERE role_id IS NULL;
