//! User roles and the permission bitmask (Mastodon's `UserRole`).
//!
//! A role bundles a `permissions` bitmask; `users.role_id` points at one (NULL
//! = an ordinary user with no elevated permissions). Bit positions match
//! Mastodon's `UserRole::FLAGS` so the serialized `role` object and the
//! `admin:*` scope gating stay wire-compatible. The `administrator` bit
//! short-circuits every permission check (`Role::can`).

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// Permission bits — positions match Mastodon's `UserRole::FLAGS`.
pub mod permission {
    pub const ADMINISTRATOR: i64 = 1 << 0;
    pub const VIEW_DEVOPS: i64 = 1 << 1;
    pub const VIEW_AUDIT_LOG: i64 = 1 << 2;
    pub const VIEW_DASHBOARD: i64 = 1 << 3;
    pub const MANAGE_REPORTS: i64 = 1 << 4;
    pub const MANAGE_FEDERATION: i64 = 1 << 5;
    pub const MANAGE_SETTINGS: i64 = 1 << 6;
    pub const MANAGE_BLOCKS: i64 = 1 << 7;
    pub const MANAGE_TAXONOMIES: i64 = 1 << 8;
    pub const MANAGE_APPEALS: i64 = 1 << 9;
    pub const MANAGE_USERS: i64 = 1 << 10;
    pub const MANAGE_INVITES: i64 = 1 << 11;
    pub const MANAGE_RULES: i64 = 1 << 12;
    pub const MANAGE_ANNOUNCEMENTS: i64 = 1 << 13;
    pub const MANAGE_CUSTOM_EMOJIS: i64 = 1 << 14;
    pub const MANAGE_WEBHOOKS: i64 = 1 << 15;
    pub const INVITE_USERS: i64 = 1 << 16;
    pub const MANAGE_ROLES: i64 = 1 << 17;
    pub const MANAGE_USER_ACCESS: i64 = 1 << 18;
    pub const DELETE_USER_DATA: i64 = 1 << 19;
    /// Staff access to the instance groups console (Plamenu extension beyond
    /// Mastodon's flag set — Mastodon has no groups, so this bit sits above
    /// its `FLAGS` range and never collides with a serialized Mastodon role).
    pub const MANAGE_GROUPS: i64 = 1 << 20;
    /// Upload or borrow personal custom emoji. This is intentionally a member
    /// capability and must not make its holder staff.
    pub const UPLOAD_CUSTOM_EMOJIS: i64 = 1 << 21;
    /// Create locally coordinated Webxdc sessions (member capability).
    pub const CREATE_WEBXDC: i64 = 1 << 22;
    /// Inspect and manage every stored Webxdc session on the instance.
    pub const MANAGE_WEBXDC: i64 = 1 << 23;
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Role {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub position: i32,
    pub permissions: i64,
    pub highlighted: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl Role {
    /// Whether this role grants `flag`. The `administrator` bit grants every
    /// permission, matching Mastodon's role-permission check.
    #[must_use]
    pub fn can(&self, flag: i64) -> bool {
        self.permissions & permission::ADMINISTRATOR != 0 || self.permissions & flag == flag
    }

    /// Whether this role grants any staff permission. Inviting users and
    /// adding personal emoji are member-tier bits; a role granting only those
    /// does not open the admin surfaces, exactly like holding no role at all.
    #[must_use]
    pub fn privileged(&self) -> bool {
        self.permissions
            & !(permission::INVITE_USERS
                | permission::UPLOAD_CUSTOM_EMOJIS
                | permission::CREATE_WEBXDC)
            != 0
    }
}

/// The seeded id of the built-in "User" role — the default every local user
/// receives at creation (migration 0005). Deleting the row degrades new users
/// to no role; the creation paths guard with a subselect so that stays safe.
pub const DEFAULT_ROLE_ID: i64 = 4;

/// Lists every role, highest-privilege (largest `position`) first.
pub async fn list(pool: &PgPool) -> Result<Vec<Role>, DbError> {
    let roles = sqlx::query_as!(
        Role,
        r#"
        SELECT id, name, color, position, permissions, highlighted,
               created_at, updated_at
        FROM user_roles
        ORDER BY position DESC, id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(roles)
}

pub async fn find_by_id(pool: &PgPool, role_id: i64) -> Result<Option<Role>, DbError> {
    let role = sqlx::query_as!(
        Role,
        r#"
        SELECT id, name, color, position, permissions, highlighted,
               created_at, updated_at
        FROM user_roles
        WHERE id = $1
        "#,
        role_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(role)
}

/// Looks a role up by name, case-insensitively (for the CLI).
pub async fn find_by_name(pool: &PgPool, name: &str) -> Result<Option<Role>, DbError> {
    let role = sqlx::query_as!(
        Role,
        r#"
        SELECT id, name, color, position, permissions, highlighted,
               created_at, updated_at
        FROM user_roles
        WHERE lower(name) = lower($1)
        "#,
        name,
    )
    .fetch_optional(pool)
    .await?;
    Ok(role)
}

pub async fn create(
    pool: &PgPool,
    name: &str,
    color: &str,
    position: i32,
    permissions: i64,
    highlighted: bool,
) -> Result<Role, DbError> {
    let role = sqlx::query_as!(
        Role,
        r#"
        INSERT INTO user_roles (id, name, color, position, permissions, highlighted)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, name, color, position, permissions, highlighted,
                  created_at, updated_at
        "#,
        id::next(),
        name,
        color,
        position,
        permissions,
        highlighted,
    )
    .fetch_one(pool)
    .await?;
    Ok(role)
}

/// Rewrites a role's fields. Returns `None` for an unknown id. Position and
/// permission elevation guards are the caller's concern (the web UI enforces
/// Mastodon's `UserRolePolicy` rules before calling this).
pub async fn update(
    pool: &PgPool,
    role_id: i64,
    name: &str,
    color: &str,
    position: i32,
    permissions: i64,
    highlighted: bool,
) -> Result<Option<Role>, DbError> {
    let role = sqlx::query_as!(
        Role,
        r#"
        UPDATE user_roles SET
            name        = $2,
            color       = $3,
            position    = $4,
            permissions = $5,
            highlighted = $6,
            updated_at  = now()
        WHERE id = $1
        RETURNING id, name, color, position, permissions, highlighted,
                  created_at, updated_at
        "#,
        role_id,
        name,
        color,
        position,
        permissions,
        highlighted,
    )
    .fetch_optional(pool)
    .await?;
    Ok(role)
}

/// Deletes a role. Users holding it fall back to no role (`ON DELETE SET
/// NULL`). Returns `false` when no row matched.
pub async fn delete(pool: &PgPool, role_id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query!(r#"DELETE FROM user_roles WHERE id = $1"#, role_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// How many users hold each role, for the roles admin page. Roles nobody
/// holds are absent from the map.
pub async fn member_counts(pool: &PgPool) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT role_id AS "role_id!", COUNT(*) AS "count!"
        FROM users
        WHERE role_id IS NOT NULL
        GROUP BY role_id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.role_id, r.count)).collect())
}

/// The role of the user behind a local account, if any (the admin extractor's
/// lookup). Returns `None` for accounts with no user row or no assigned role.
/// Account ids of local users whose role grants `flag` (or `administrator`,
/// which short-circuits every check) — Mastodon's `User.those_who_can`.
/// Backs the staff-facing side effects of sign-ups (`admin.sign_up`
/// notifications, pending-account mail).
pub async fn account_ids_who_can(pool: &PgPool, flag: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT u.account_id
        FROM users u
        JOIN user_roles r ON u.role_id = r.id
        WHERE r.permissions & ($1::bigint | $2::bigint) <> 0
        ORDER BY u.account_id
        "#,
        flag,
        permission::ADMINISTRATOR,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Counts local accounts that can currently administer the instance —
/// approved, enabled, unsuspended users whose role grants the `administrator`
/// permission — optionally excluding one account. Backs the "last
/// administrator" lock-out guard: disabling, suspending, or permanently
/// deleting the only remaining administrator would lock everyone out of the
/// admin surfaces. The `administrator` bit is checked literally
/// (`& administrator`), not through [`Role::can`], since a role that merely
/// carries `manage_users` is not itself an administrator.
pub async fn active_administrator_count(
    pool: &PgPool,
    exclude_account_id: Option<i64>,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM users u
        JOIN user_roles r ON u.role_id = r.id
        JOIN accounts a ON a.id = u.account_id
        WHERE r.permissions & $1::bigint <> 0
          AND u.approved AND NOT u.disabled
          AND a.suspended_at IS NULL
          AND ($2::bigint IS NULL OR u.account_id <> $2)
        "#,
        permission::ADMINISTRATOR,
        exclude_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// One `/staff` roster entry: a staff account and its role's display name.
pub struct StaffRosterRow {
    pub account_id: i64,
    pub role_name: String,
}

/// The server's public staff roster — local users whose role grants any
/// staff permission ([`Role::privileged`]), highest role first,
/// each with its role name so the page can group owners, admins and
/// moderators under their own headings. Backs the public `/staff` page, so it
/// hides accounts a visitor should not meet there: unapproved or disabled
/// users, suspended accounts, and anyone who unchecked "Suggest account to
/// others" (`discoverable`) — staff opt out of the listing the same way they
/// opt out of the profile directory.
pub async fn staff_roster(pool: &PgPool) -> Result<Vec<StaffRosterRow>, DbError> {
    let rows = sqlx::query_as!(
        StaffRosterRow,
        r#"
        SELECT u.account_id AS "account_id!", r.name AS "role_name!"
        FROM users u
        JOIN user_roles r ON u.role_id = r.id
        JOIN accounts a ON a.id = u.account_id
        WHERE r.permissions & ~$1::bigint <> 0
          AND u.approved AND u.confirmed_at IS NOT NULL
          AND NOT u.disabled AND a.suspended_at IS NULL
          AND a.discoverable IS NOT FALSE
        ORDER BY r.position DESC, u.account_id
        "#,
        permission::INVITE_USERS | permission::UPLOAD_CUSTOM_EMOJIS | permission::CREATE_WEBXDC,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn for_user(pool: &PgPool, user_id: i64) -> Result<Option<Role>, DbError> {
    let role = sqlx::query_as!(
        Role,
        r#"
        SELECT r.id, r.name, r.color, r.position, r.permissions, r.highlighted,
               r.created_at, r.updated_at
        FROM user_roles r
        JOIN users u ON u.role_id = r.id
        WHERE u.id = $1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(role)
}

/// The role of the user behind a local account, if any.
pub async fn for_account(pool: &PgPool, account_id: i64) -> Result<Option<Role>, DbError> {
    let role = sqlx::query_as!(
        Role,
        r#"
        SELECT r.id, r.name, r.color, r.position, r.permissions, r.highlighted,
               r.created_at, r.updated_at
        FROM user_roles r
        JOIN users u ON u.role_id = r.id
        WHERE u.account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(role)
}

/// Mastodon's implicit "Everyone" role (`UserRole::EVERYONE_ROLE_ID`, id
/// -99): what `CredentialAccountSerializer#role` serializes for a user with
/// no assigned role. Wire-compat shell only — it grants nothing here, since
/// permissions come solely from assigned roles. Not a stored row; the
/// timestamps are placeholders the serializer never reads.
#[must_use]
pub fn everyone() -> Role {
    Role {
        id: -99,
        name: String::new(),
        color: String::new(),
        position: 0,
        permissions: 0,
        highlighted: false,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

/// Assigns (or clears, with `None`) the role of the user behind a local
/// account. Returns `false` when the account has no user row.
pub async fn assign_to_account(
    pool: &PgPool,
    account_id: i64,
    role_id: Option<i64>,
) -> Result<bool, DbError> {
    let affected = sqlx::query!(
        r#"
        UPDATE users SET role_id = $2 WHERE account_id = $1
        "#,
        account_id,
        role_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::user;

    #[test]
    fn administrator_grants_every_permission() {
        let owner = Role {
            id: 3,
            name: "Owner".to_owned(),
            color: String::new(),
            position: 100,
            permissions: permission::ADMINISTRATOR,
            highlighted: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(owner.can(permission::MANAGE_REPORTS));
        assert!(owner.can(permission::DELETE_USER_DATA));
    }

    #[test]
    fn plain_role_only_grants_its_own_bits() {
        let moderator = Role {
            id: 1,
            name: "Moderator".to_owned(),
            color: String::new(),
            position: 10,
            permissions: permission::MANAGE_REPORTS | permission::MANAGE_USERS,
            highlighted: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(moderator.can(permission::MANAGE_REPORTS));
        assert!(moderator.can(permission::MANAGE_USERS));
        assert!(!moderator.can(permission::MANAGE_FEDERATION));
        assert!(!moderator.can(permission::ADMINISTRATOR));
    }

    #[sqlx::test]
    async fn seeded_default_roles_present(pool: PgPool) {
        let all = list(&pool).await.unwrap();
        let names: Vec<_> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Owner", "Admin", "Moderator", "User"]);

        let owner = find_by_name(&pool, "owner").await.unwrap().unwrap();
        assert!(owner.can(permission::ADMINISTRATOR));
        assert!(owner.privileged());
        let admin = find_by_name(&pool, "Admin").await.unwrap().unwrap();
        assert!(admin.can(permission::MANAGE_GROUPS));
        assert!(!admin.can(permission::ADMINISTRATOR));
        let moderator = find_by_name(&pool, "Moderator").await.unwrap().unwrap();
        assert!(moderator.can(permission::MANAGE_REPORTS));
        assert!(!moderator.can(permission::MANAGE_FEDERATION));
        assert!(moderator.privileged());

        // The default role grants member capabilities but no admin access.
        // Invites remain opt-in (migration 0015). A role granting
        // only `invite_users` stays non-staff too.
        let user = find_by_id(&pool, DEFAULT_ROLE_ID).await.unwrap().unwrap();
        assert_eq!(user.name, "User");
        assert!(!user.can(permission::INVITE_USERS));
        assert!(!user.privileged());
        assert!(user.can(permission::CREATE_WEBXDC));
        let inviter = Role {
            permissions: permission::INVITE_USERS,
            ..everyone()
        };
        assert!(inviter.can(permission::INVITE_USERS));
        assert!(!inviter.privileged());
        assert!(!everyone().privileged());
    }

    #[sqlx::test]
    async fn update_delete_and_member_counts(pool: PgPool) {
        let helper = create(&pool, "Helper", "", 5, permission::MANAGE_REPORTS, false)
            .await
            .unwrap();

        let updated = update(
            &pool,
            helper.id,
            "Support",
            "#123456",
            7,
            permission::MANAGE_REPORTS | permission::MANAGE_USERS,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.name, "Support");
        assert_eq!(updated.position, 7);
        assert!(updated.can(permission::MANAGE_USERS));

        let account = account::create_local(
            &pool,
            NewLocalAccount {
                username: "carol",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let member = user::create(&pool, account.id, None, "$argon2id$x")
            .await
            .unwrap();
        assign_to_account(&pool, account.id, Some(helper.id))
            .await
            .unwrap();
        assert_eq!(member_counts(&pool).await.unwrap(), vec![(helper.id, 1)]);

        // Deleting the role drops members back to no role.
        assert!(delete(&pool, helper.id).await.unwrap());
        assert!(!delete(&pool, helper.id).await.unwrap());
        assert!(for_user(&pool, member.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn active_administrator_count_tracks_only_usable_owners(pool: PgPool) {
        // Only the seeded Owner role carries the `administrator` bit; Admin and
        // Moderator manage users without being administrators themselves.
        let owner = find_by_name(&pool, "Owner").await.unwrap().unwrap();
        assert_ne!(owner.permissions & permission::ADMINISTRATOR, 0);
        let admin = find_by_name(&pool, "Admin").await.unwrap().unwrap();
        assert_eq!(admin.permissions & permission::ADMINISTRATOR, 0);

        let mk = |name: &'static str| {
            let pool = pool.clone();
            async move {
                let account = account::create_local(
                    &pool,
                    NewLocalAccount {
                        username: name,
                        display_name: "",
                        note: "",
                        public_key_pem: "pub",
                    },
                )
                .await
                .unwrap();
                user::create(&pool, account.id, None, "$argon2id$x")
                    .await
                    .unwrap();
                account
            }
        };

        // No administrators yet.
        assert_eq!(active_administrator_count(&pool, None).await.unwrap(), 0);

        let alice = mk("alice").await;
        assign_to_account(&pool, alice.id, Some(owner.id))
            .await
            .unwrap();
        let bob = mk("bob").await;
        assign_to_account(&pool, bob.id, Some(admin.id))
            .await
            .unwrap();

        // Bob is an Admin, not an administrator, so only Alice counts.
        assert_eq!(active_administrator_count(&pool, None).await.unwrap(), 1);
        // Excluding the only owner leaves none — the "last administrator".
        assert_eq!(
            active_administrator_count(&pool, Some(alice.id))
                .await
                .unwrap(),
            0
        );

        // A suspended or disabled owner cannot administer, so it is not counted.
        let carol = mk("carol").await;
        assign_to_account(&pool, carol.id, Some(owner.id))
            .await
            .unwrap();
        assert_eq!(active_administrator_count(&pool, None).await.unwrap(), 2);
        account::suspend(&pool, carol.id, "local").await.unwrap();
        assert_eq!(active_administrator_count(&pool, None).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn assign_and_resolve_role_for_user(pool: PgPool) {
        let account = account::create_local(
            &pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let created = user::create(&pool, account.id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();

        // Creation grants the default "User" role — visible, but not staff.
        let default = for_user(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(default.id, DEFAULT_ROLE_ID);
        assert!(default.can(permission::UPLOAD_CUSTOM_EMOJIS));
        assert!(!default.privileged());

        let admin = find_by_name(&pool, "Admin").await.unwrap().unwrap();
        assert!(
            assign_to_account(&pool, account.id, Some(admin.id))
                .await
                .unwrap()
        );
        let resolved = for_user(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(resolved.id, admin.id);
        assert!(resolved.can(permission::MANAGE_FEDERATION));

        // Clearing the role removes admin powers.
        assert!(assign_to_account(&pool, account.id, None).await.unwrap());
        assert!(for_user(&pool, created.id).await.unwrap().is_none());

        // Unknown account → no user row updated.
        assert!(
            !assign_to_account(&pool, 999_999, Some(admin.id))
                .await
                .unwrap()
        );
    }
}
