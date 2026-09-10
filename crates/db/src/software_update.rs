//! Cached software-update-check results — Mastodon's `SoftwareUpdate`.
//!
//! The check worker fetches a release feed the operator points it at and calls
//! [`replace_with`] with the versions ahead of the running one; the admin
//! dashboard reads [`pending`] / [`urgent_count`] to show a banner. The table
//! only ever holds releases newer than the running version, so "list all" is
//! "list pending".

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// One advertised release newer than the running version.
#[derive(Debug, Clone)]
pub struct SoftwareUpdate {
    pub id: i64,
    pub version: String,
    /// A security release that should be applied promptly.
    pub urgent: bool,
    /// `patch` | `minor` | `major` — the semver bump from the running version.
    pub release_type: String,
    /// Link to the release notes ('' if the feed gave none).
    pub release_notes: String,
    pub created_at: OffsetDateTime,
}

/// A release the check discovered, before it is stored.
#[derive(Debug, Clone)]
pub struct NewSoftwareUpdate {
    pub version: String,
    pub urgent: bool,
    pub release_type: String,
    pub release_notes: String,
}

/// Replaces the stored update set with `updates`: upserts each row (preserving
/// `created_at` for versions already known, so "first seen" stays stable) and
/// deletes any stored version no longer advertised — Mastodon's
/// `SoftwareUpdate.upsert_all` followed by pruning the ones not returned. An
/// empty `updates` clears the table (the install is up to date).
pub async fn replace_with(pool: &PgPool, updates: &[NewSoftwareUpdate]) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let versions: Vec<String> = updates.iter().map(|u| u.version.clone()).collect();
    sqlx::query!(
        "DELETE FROM software_updates WHERE version <> ALL($1)",
        &versions,
    )
    .execute(&mut *tx)
    .await?;
    // The feed is remote-sourced and may repeat a version; keep the last
    // occurrence, as sequential upserts did — a duplicate would otherwise make
    // the single upsert touch one row twice, which ON CONFLICT rejects.
    let mut kept: Vec<&NewSoftwareUpdate> = Vec::new();
    for update in updates {
        if let Some(slot) = kept.iter().position(|u| u.version == update.version) {
            kept[slot] = update;
        } else {
            kept.push(update);
        }
    }
    if !kept.is_empty() {
        let update_versions: Vec<&str> = kept.iter().map(|u| u.version.as_str()).collect();
        let urgents: Vec<bool> = kept.iter().map(|u| u.urgent).collect();
        let release_types: Vec<&str> = kept.iter().map(|u| u.release_type.as_str()).collect();
        let release_notes: Vec<&str> = kept.iter().map(|u| u.release_notes.as_str()).collect();
        sqlx::query!(
            r#"
            INSERT INTO software_updates (version, urgent, release_type, release_notes)
            SELECT v.version, v.urgent, v.release_type, v.release_notes
            FROM unnest($1::text[], $2::bool[], $3::text[], $4::text[])
                 AS v(version, urgent, release_type, release_notes)
            ON CONFLICT (version) DO UPDATE SET
                urgent = EXCLUDED.urgent,
                release_type = EXCLUDED.release_type,
                release_notes = EXCLUDED.release_notes
            "#,
            &update_versions as &[&str],
            &urgents,
            &release_types as &[&str],
            &release_notes as &[&str],
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// All pending updates, most urgent then newest version first — the dashboard's
/// order. `version` sorts lexically, which is wrong across multi-digit
/// components, but the set is tiny and the worker already excludes anything not
/// strictly ahead of the running version, so the banner only needs the flag and
/// the highest entry.
pub async fn pending(pool: &PgPool) -> Result<Vec<SoftwareUpdate>, DbError> {
    let updates = sqlx::query_as!(
        SoftwareUpdate,
        r#"
        SELECT id, version, urgent, release_type, release_notes, created_at
        FROM software_updates
        ORDER BY urgent DESC, version DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(updates)
}

/// How many pending updates are security (`urgent`) releases — drives the
/// banner's severity styling.
pub async fn urgent_count(pool: &PgPool) -> Result<i64, DbError> {
    let count =
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM software_updates WHERE urgent"#,)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, urgent: bool) -> NewSoftwareUpdate {
        NewSoftwareUpdate {
            version: version.to_owned(),
            urgent,
            release_type: "patch".to_owned(),
            release_notes: String::new(),
        }
    }

    #[sqlx::test]
    async fn replace_upserts_and_prunes(pool: PgPool) {
        replace_with(&pool, &[entry("4.6.3", false), entry("4.6.4", true)])
            .await
            .unwrap();
        let rows = pending(&pool).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Urgent first, then newest version.
        assert_eq!(rows[0].version, "4.6.4");
        assert!(rows[0].urgent);
        assert_eq!(urgent_count(&pool).await.unwrap(), 1);

        // A re-check that drops 4.6.3 and de-escalates 4.6.4 removes the vanished
        // version and updates the surviving one in place.
        replace_with(&pool, &[entry("4.6.4", false)]).await.unwrap();
        let rows = pending(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].version, "4.6.4");
        assert!(!rows[0].urgent);
        assert_eq!(urgent_count(&pool).await.unwrap(), 0);

        // Empty set clears the table (install is current).
        replace_with(&pool, &[]).await.unwrap();
        assert!(pending(&pool).await.unwrap().is_empty());
    }
}
