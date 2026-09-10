//! Moderation reports: local reports (federated out as `Flag`) and inbound
//! remote reports received as `Flag` activities.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// A report to insert.
#[derive(Debug)]
pub struct NewReport<'a> {
    pub account_id: i64,
    pub target_account_id: i64,
    pub status_ids: &'a [i64],
    pub comment: &'a str,
    /// One of `other` / `spam` / `legal` / `violation`.
    pub category: &'a str,
    /// Whether the report was forwarded to the target's origin (`None` for
    /// inbound reports, which were never re-forwarded).
    pub forwarded: Option<bool>,
    pub rule_ids: Option<&'a [i64]>,
    pub uri: Option<&'a str>,
}

/// A stored report, the source for the REST `Report` entity.
#[derive(Debug, Clone)]
pub struct Report {
    pub id: i64,
    pub account_id: i64,
    pub target_account_id: i64,
    pub status_ids: Vec<i64>,
    pub comment: String,
    pub category: String,
    pub forwarded: Option<bool>,
    pub rule_ids: Option<Vec<i64>>,
    pub uri: Option<String>,
    pub action_taken_at: Option<OffsetDateTime>,
    /// The moderator handling the report (Mastodon's `assigned_account`).
    pub assigned_account_id: Option<i64>,
    /// The moderator who resolved it (`action_taken_by_account`).
    pub action_taken_by_account_id: Option<i64>,
    /// The group this report is scoped to, or `None` for an ordinary
    /// instance-staff report.
    pub group_account_id: Option<i64>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Files a report, returning the stored row.
pub async fn create(pool: &PgPool, report: NewReport<'_>) -> Result<Report, DbError> {
    create_scoped(pool, report, None).await
}

/// Files a report scoped to a local group: `group_account_id` routes it
/// to the group's moderators instead of (or, for a local target, in addition
/// to) instance staff. `create` is this with `None`.
pub async fn create_scoped<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    report: NewReport<'_>,
    group_account_id: Option<i64>,
) -> Result<Report, DbError> {
    let row = sqlx::query_as!(
        Report,
        r#"
        INSERT INTO reports
            (id, account_id, target_account_id, status_ids, comment, category,
             forwarded, rule_ids, uri, group_account_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING id, account_id, target_account_id,
                  status_ids AS "status_ids!", comment, category, forwarded,
                  rule_ids, uri, action_taken_at, assigned_account_id,
                  action_taken_by_account_id, group_account_id, created_at,
                  updated_at
        "#,
        id::next(),
        report.account_id,
        report.target_account_id,
        report.status_ids,
        report.comment,
        report.category,
        report.forwarded,
        report.rule_ids,
        report.uri,
        group_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Whether an inbound report with this `Flag` activity uri is already stored,
/// so a redelivered `Flag` doesn't file a second report.
pub async fn exists_by_uri(pool: &PgPool, uri: &str) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM reports WHERE uri = $1) AS "found!""#,
        uri,
    )
    .fetch_one(pool)
    .await?;
    Ok(found)
}

/// One report by id.
pub async fn find_by_id(pool: &PgPool, report_id: i64) -> Result<Option<Report>, DbError> {
    let row = sqlx::query_as!(
        Report,
        r#"
        SELECT id, account_id, target_account_id, status_ids AS "status_ids!",
               comment, category, forwarded, rule_ids, uri, action_taken_at,
               assigned_account_id, action_taken_by_account_id,
               group_account_id, created_at, updated_at
        FROM reports
        WHERE id = $1
        "#,
        report_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Every report filed by `account_id`, newest first (diagnostics / tests).
pub async fn list_by_reporter(pool: &PgPool, account_id: i64) -> Result<Vec<Report>, DbError> {
    let rows = sqlx::query_as!(
        Report,
        r#"
        SELECT id, account_id, target_account_id, status_ids AS "status_ids!",
               comment, category, forwarded, rule_ids, uri, action_taken_at,
               assigned_account_id, action_taken_by_account_id,
               group_account_id, created_at, updated_at
        FROM reports
        WHERE account_id = $1
        ORDER BY id DESC
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Filters for [`list_for_admin`] — Mastodon's `ReportFilter` keys exposed by
/// the admin reports controller. `resolved`/`unresolved` follow Mastodon's
/// `status_scope`: neither set → unresolved only; `resolved` alone → resolved
/// only; both → every report.
#[derive(Debug, Default)]
pub struct AdminReportFilter {
    pub resolved: bool,
    pub unresolved: bool,
    pub account_id: Option<i64>,
    pub target_account_id: Option<i64>,
    /// Restrict to reports scoped to this group (the group moderator queue);
    /// `None` in the instance-staff console lists every report, group or not.
    pub group_account_id: Option<i64>,
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: i64,
}

/// Lists reports matching `filter`, newest id first (oldest first when `min_id`
/// is set), for the admin moderation surface.
pub async fn list_for_admin(
    pool: &PgPool,
    filter: &AdminReportFilter,
) -> Result<Vec<Report>, DbError> {
    let ascending = filter.min_id.is_some();
    // Mastodon's `status_scope`: both flags → all; `resolved` alone → resolved;
    // otherwise the default unresolved-only view.
    let all = filter.resolved && filter.unresolved;
    let resolved_only = filter.resolved && !filter.unresolved;
    let rows = sqlx::query_as!(
        Report,
        r#"
        SELECT id, account_id, target_account_id, status_ids AS "status_ids!",
               comment, category, forwarded, rule_ids, uri, action_taken_at,
               assigned_account_id, action_taken_by_account_id,
               group_account_id, created_at, updated_at
        FROM reports
        WHERE ($1::bool
                OR ($2::bool AND action_taken_at IS NOT NULL)
                OR (NOT $2::bool AND action_taken_at IS NULL))
          AND ($3::bigint IS NULL OR account_id = $3)
          AND ($4::bigint IS NULL OR target_account_id = $4)
          AND ($5::bigint IS NULL OR group_account_id = $5)
          AND ($6::bigint IS NULL OR id < $6)
          AND ($7::bigint IS NULL OR id > $7)
          AND ($8::bigint IS NULL OR id > $8)
        ORDER BY id * (CASE WHEN $9 THEN 1 ELSE -1 END)
        LIMIT $10
        "#,
        all,
        resolved_only,
        filter.account_id,
        filter.target_account_id,
        filter.group_account_id,
        filter.max_id,
        filter.since_id,
        filter.min_id,
        ascending,
        filter.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Counts the open (un-actioned) reports — `action_taken_at IS NULL`, the same
/// predicate as the default unresolved admin view. Used for the dashboard's
/// at-a-glance "open reports" figure.
pub async fn count_unresolved(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM reports WHERE action_taken_at IS NULL"#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Open (un-actioned) reports scoped to `group_account_id` — the badge on the
/// group moderation page.
pub async fn count_unresolved_for_group(
    pool: &PgPool,
    group_account_id: i64,
) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM reports
           WHERE group_account_id = $1 AND action_taken_at IS NULL"#,
        group_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Updates a report's `category` and/or `rule_ids` (Mastodon's `PUT`
/// `report_params`). A `None` leaves the field unchanged; `rule_ids` is
/// addressed separately so an explicit empty array can clear it.
pub async fn update_category(
    pool: &PgPool,
    report_id: i64,
    category: Option<&str>,
    rule_ids: Option<&[i64]>,
) -> Result<Option<Report>, DbError> {
    let set_rule_ids = rule_ids.is_some();
    let row = sqlx::query_as!(
        Report,
        r#"
        UPDATE reports
        SET category   = COALESCE($2, category),
            rule_ids   = CASE WHEN $3 THEN $4 ELSE rule_ids END,
            updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, target_account_id,
                  status_ids AS "status_ids!", comment, category, forwarded,
                  rule_ids, uri, action_taken_at, assigned_account_id,
                  action_taken_by_account_id, group_account_id, created_at,
                  updated_at
        "#,
        report_id,
        category,
        set_rule_ids,
        rule_ids,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Sets (or clears, with `None`) the assigned moderator
/// (`assign_to_self`/`unassign`).
pub async fn assign(
    pool: &PgPool,
    report_id: i64,
    assigned_account_id: Option<i64>,
) -> Result<Option<Report>, DbError> {
    let row = sqlx::query_as!(
        Report,
        r#"
        UPDATE reports
        SET assigned_account_id = $2, updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, target_account_id,
                  status_ids AS "status_ids!", comment, category, forwarded,
                  rule_ids, uri, action_taken_at, assigned_account_id,
                  action_taken_by_account_id, group_account_id, created_at,
                  updated_at
        "#,
        report_id,
        assigned_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Marks a report resolved (Mastodon's `resolve!`): stamps `action_taken_at`
/// and records the acting moderator.
pub async fn resolve(
    pool: &PgPool,
    report_id: i64,
    acting_account_id: i64,
) -> Result<Option<Report>, DbError> {
    let row = sqlx::query_as!(
        Report,
        r#"
        UPDATE reports
        SET action_taken_at = now(), action_taken_by_account_id = $2,
            updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, target_account_id,
                  status_ids AS "status_ids!", comment, category, forwarded,
                  rule_ids, uri, action_taken_at, assigned_account_id,
                  action_taken_by_account_id, group_account_id, created_at,
                  updated_at
        "#,
        report_id,
        acting_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Reopens a resolved report (Mastodon's `unresolve!`): clears
/// `action_taken_at` and the acting moderator.
pub async fn unresolve(pool: &PgPool, report_id: i64) -> Result<Option<Report>, DbError> {
    let row = sqlx::query_as!(
        Report,
        r#"
        UPDATE reports
        SET action_taken_at = NULL, action_taken_by_account_id = NULL,
            updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, target_account_id,
                  status_ids AS "status_ids!", comment, category, forwarded,
                  rule_ids, uri, action_taken_at, assigned_account_id,
                  action_taken_by_account_id, group_account_id, created_at,
                  updated_at
        "#,
        report_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn create_and_read_back(pool: PgPool) {
        let reporter = local(&pool, "alice").await;
        let target = local(&pool, "carol").await;

        let stored = create(
            &pool,
            NewReport {
                account_id: reporter,
                target_account_id: target,
                status_ids: &[10, 20],
                comment: "spammy",
                category: "spam",
                forwarded: Some(true),
                rule_ids: None,
                uri: Some("https://plamenu.test/reports/1"),
            },
        )
        .await
        .unwrap();

        assert_eq!(stored.account_id, reporter);
        assert_eq!(stored.target_account_id, target);
        assert_eq!(stored.status_ids, vec![10, 20]);
        assert_eq!(stored.comment, "spammy");
        assert_eq!(stored.category, "spam");
        assert_eq!(stored.forwarded, Some(true));
        assert_eq!(stored.rule_ids, None);
        assert!(stored.action_taken_at.is_none());

        let fetched = find_by_id(&pool, stored.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, stored.id);
        assert_eq!(
            fetched.uri.as_deref(),
            Some("https://plamenu.test/reports/1")
        );
    }

    #[sqlx::test]
    async fn defaults_and_listing(pool: PgPool) {
        let reporter = local(&pool, "alice").await;
        let target = local(&pool, "carol").await;
        let other = local(&pool, "dave").await;

        // An inbound-style report: no statuses, no forwarded flag, rule ids.
        let first = create(
            &pool,
            NewReport {
                account_id: reporter,
                target_account_id: target,
                status_ids: &[],
                comment: "",
                category: "violation",
                forwarded: None,
                rule_ids: Some(&[3, 7]),
                uri: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(first.status_ids, Vec::<i64>::new());
        assert_eq!(first.forwarded, None);
        assert_eq!(first.rule_ids, Some(vec![3, 7]));

        let second = create(
            &pool,
            NewReport {
                account_id: reporter,
                target_account_id: other,
                status_ids: &[],
                comment: "",
                category: "other",
                forwarded: Some(false),
                rule_ids: None,
                uri: None,
            },
        )
        .await
        .unwrap();

        let mine = list_by_reporter(&pool, reporter).await.unwrap();
        assert_eq!(
            mine.iter().map(|r| r.id).collect::<Vec<_>>(),
            [second.id, first.id],
            "newest first"
        );
        assert!(list_by_reporter(&pool, target).await.unwrap().is_empty());
    }

    fn filter(resolved: bool, unresolved: bool) -> AdminReportFilter {
        AdminReportFilter {
            resolved,
            unresolved,
            limit: 100,
            ..AdminReportFilter::default()
        }
    }

    #[sqlx::test]
    async fn admin_lifecycle_and_status_filter(pool: PgPool) {
        let reporter = local(&pool, "alice").await;
        let target = local(&pool, "carol").await;
        let mod_account = local(&pool, "mod").await;

        let make = |cat: &'static str| {
            let pool = pool.clone();
            async move {
                create(
                    &pool,
                    NewReport {
                        account_id: reporter,
                        target_account_id: target,
                        status_ids: &[],
                        comment: "",
                        category: cat,
                        forwarded: None,
                        rule_ids: None,
                        uri: None,
                    },
                )
                .await
                .unwrap()
            }
        };
        let first = make("spam").await;
        let second = make("other").await;

        // Default view is unresolved-only, newest first.
        let unresolved = list_for_admin(&pool, &filter(false, false)).await.unwrap();
        assert_eq!(
            unresolved.iter().map(|r| r.id).collect::<Vec<_>>(),
            [second.id, first.id]
        );

        // Resolve one; it leaves the default view and joins the resolved view.
        let resolved = resolve(&pool, first.id, mod_account)
            .await
            .unwrap()
            .unwrap();
        assert!(resolved.action_taken_at.is_some());
        assert_eq!(resolved.action_taken_by_account_id, Some(mod_account));

        let still_unresolved = list_for_admin(&pool, &filter(false, false)).await.unwrap();
        assert_eq!(
            still_unresolved.iter().map(|r| r.id).collect::<Vec<_>>(),
            [second.id]
        );
        let resolved_only = list_for_admin(&pool, &filter(true, false)).await.unwrap();
        assert_eq!(
            resolved_only.iter().map(|r| r.id).collect::<Vec<_>>(),
            [first.id]
        );
        // Both flags → every report.
        let all = list_for_admin(&pool, &filter(true, true)).await.unwrap();
        assert_eq!(all.len(), 2);

        // Reopen restores it to the unresolved view.
        let reopened = unresolve(&pool, first.id).await.unwrap().unwrap();
        assert!(reopened.action_taken_at.is_none());
        assert_eq!(reopened.action_taken_by_account_id, None);
        assert_eq!(
            list_for_admin(&pool, &filter(false, false))
                .await
                .unwrap()
                .len(),
            2
        );

        // Assign / unassign.
        let assigned = assign(&pool, second.id, Some(mod_account))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(assigned.assigned_account_id, Some(mod_account));
        let unassigned = assign(&pool, second.id, None).await.unwrap().unwrap();
        assert_eq!(unassigned.assigned_account_id, None);

        // Update category and rule_ids independently.
        let recat = update_category(&pool, second.id, Some("violation"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recat.category, "violation");
        assert_eq!(recat.rule_ids, None);
        let ruled = update_category(&pool, second.id, None, Some(&[5, 9]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ruled.category, "violation", "category preserved");
        assert_eq!(ruled.rule_ids, Some(vec![5, 9]));
    }

    #[sqlx::test]
    async fn group_scoped_reports_route_to_the_group_queue(pool: PgPool) {
        let reporter = local(&pool, "alice").await;
        let target = local(&pool, "carl").await;
        let group = local(&pool, "hiking").await;

        // A community report is scoped to the group.
        let scoped = create_scoped(
            &pool,
            NewReport {
                account_id: reporter,
                target_account_id: target,
                status_ids: &[],
                comment: "spam",
                category: "other",
                forwarded: None,
                rule_ids: None,
                uri: None,
            },
            Some(group),
        )
        .await
        .unwrap();
        assert_eq!(scoped.group_account_id, Some(group));

        // An ordinary report carries no group scope.
        let plain = create(
            &pool,
            NewReport {
                account_id: reporter,
                target_account_id: target,
                status_ids: &[],
                comment: "",
                category: "other",
                forwarded: None,
                rule_ids: None,
                uri: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(plain.group_account_id, None);

        // The group queue sees only its own report.
        let group_filter = AdminReportFilter {
            unresolved: true,
            group_account_id: Some(group),
            limit: 50,
            ..AdminReportFilter::default()
        };
        let group_reports = list_for_admin(&pool, &group_filter).await.unwrap();
        assert_eq!(
            group_reports.iter().map(|r| r.id).collect::<Vec<_>>(),
            [scoped.id]
        );
        assert_eq!(count_unresolved_for_group(&pool, group).await.unwrap(), 1);
        // The instance-staff console (no group filter) still sees both.
        assert_eq!(count_unresolved(&pool).await.unwrap(), 2);
    }

    #[sqlx::test]
    async fn admin_filter_by_account(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let target = local(&pool, "carol").await;
        let other_target = local(&pool, "dave").await;

        let new = |reporter: i64, tgt: i64| NewReport {
            account_id: reporter,
            target_account_id: tgt,
            status_ids: &[],
            comment: "",
            category: "other",
            forwarded: None,
            rule_ids: None,
            uri: None,
        };
        let by_alice = create(&pool, new(alice, target)).await.unwrap();
        let _by_bob = create(&pool, new(bob, target)).await.unwrap();
        let _other = create(&pool, new(alice, other_target)).await.unwrap();

        let from_alice = list_for_admin(
            &pool,
            &AdminReportFilter {
                unresolved: true,
                resolved: true,
                account_id: Some(alice),
                target_account_id: Some(target),
                limit: 100,
                ..AdminReportFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            from_alice.iter().map(|r| r.id).collect::<Vec<_>>(),
            [by_alice.id]
        );
    }
}
