//! Instance rules and their per-language translations (Mastodon's `Rule` /
//! `RuleTranslation`).
//!
//! Rules are the numbered server policies surfaced at `GET /api/v1/instance/rules`
//! and embedded in the instance entity, and they're referenced by moderation
//! reports (`reports.rule_ids`). Mastodon soft-deletes them: [`list_ordered`]
//! returns the kept rows in display order, while [`find_by_ids`] resolves any
//! cited rule — including a discarded one — for a report's `rules` array
//! (Mastodon's `Rule.with_discarded`).

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Rule {
    pub id: i64,
    pub priority: i32,
    pub text: String,
    pub hint: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleTranslation {
    pub rule_id: i64,
    pub language: String,
    pub text: String,
    pub hint: String,
}

/// The live rules in display order (kept rows by `priority`, then `id`) —
/// Mastodon's `Rule.ordered`.
pub async fn list_ordered(pool: &PgPool) -> Result<Vec<Rule>, DbError> {
    let rules = sqlx::query_as!(
        Rule,
        r#"
        SELECT id, priority, text, hint, deleted_at, created_at, updated_at
        FROM rules
        WHERE deleted_at IS NULL
        ORDER BY priority, id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rules)
}

/// Resolves a set of rule ids, *including* soft-deleted ones, ordered for
/// stable output — Mastodon resolves a report's rules the same way.
pub async fn find_by_ids(pool: &PgPool, ids: &[i64]) -> Result<Vec<Rule>, DbError> {
    let rules = sqlx::query_as!(
        Rule,
        r#"
        SELECT id, priority, text, hint, deleted_at, created_at, updated_at
        FROM rules
        WHERE id = ANY($1)
        ORDER BY priority, id
        "#,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rules)
}

/// Loads the translations for a set of rules, sorted by language (Mastodon
/// orders `translations` by `language: :asc`).
pub async fn translations_for(
    pool: &PgPool,
    rule_ids: &[i64],
) -> Result<Vec<RuleTranslation>, DbError> {
    let translations = sqlx::query_as!(
        RuleTranslation,
        r#"
        SELECT rule_id, language, text, hint
        FROM rule_translations
        WHERE rule_id = ANY($1)
        ORDER BY rule_id, language
        "#,
        rule_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(translations)
}

/// Creates a rule. When `priority` is `None` it lands after the current rules
/// (Mastodon appends new rules at the end of the ordered list).
pub async fn create(
    pool: &PgPool,
    text: &str,
    hint: &str,
    priority: Option<i32>,
) -> Result<Rule, DbError> {
    let priority = match priority {
        Some(p) => p,
        None => next_priority(pool).await?,
    };
    let rule = sqlx::query_as!(
        Rule,
        r#"
        INSERT INTO rules (id, priority, text, hint)
        VALUES ($1, $2, $3, $4)
        RETURNING id, priority, text, hint, deleted_at, created_at, updated_at
        "#,
        id::next(),
        priority,
        text,
        hint,
    )
    .fetch_one(pool)
    .await?;
    Ok(rule)
}

/// Updates a rule's `text`, `hint` and/or `priority` in place; a `None`
/// argument leaves that field unchanged. Returns `None` for an unknown or
/// already-deleted id.
pub async fn update(
    pool: &PgPool,
    rule_id: i64,
    text: Option<&str>,
    hint: Option<&str>,
    priority: Option<i32>,
) -> Result<Option<Rule>, DbError> {
    let rule = sqlx::query_as!(
        Rule,
        r#"
        UPDATE rules SET
            text       = COALESCE($2, text),
            hint       = COALESCE($3, hint),
            priority   = COALESCE($4, priority),
            updated_at = now()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, priority, text, hint, deleted_at, created_at, updated_at
        "#,
        rule_id,
        text,
        hint,
        priority,
    )
    .fetch_optional(pool)
    .await?;
    Ok(rule)
}

/// Soft-deletes a rule (Mastodon's `Discard`). Returns `false` when no live
/// rule matched. The row is kept so reports that cite it still resolve.
pub async fn delete(pool: &PgPool, rule_id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query!(
        r#"
        UPDATE rules SET deleted_at = now()
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        rule_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// One past the current highest priority among live rules (0 when there are
/// none), so a freshly created rule sorts last.
async fn next_priority(pool: &PgPool) -> Result<i32, DbError> {
    let max = sqlx::query_scalar!(r#"SELECT MAX(priority) FROM rules WHERE deleted_at IS NULL"#)
        .fetch_one(pool)
        .await?;
    Ok(max.map_or(0, |p| p + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn create_lists_in_priority_order_and_appends(pool: PgPool) {
        let first = create(&pool, "No spam", "", None).await.unwrap();
        let second = create(&pool, "Be nice", "Hint", None).await.unwrap();
        assert_eq!(first.priority, 0);
        assert_eq!(second.priority, 1);

        let ordered = list_ordered(&pool).await.unwrap();
        let texts: Vec<_> = ordered.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["No spam", "Be nice"]);
    }

    #[sqlx::test]
    async fn update_changes_only_supplied_fields(pool: PgPool) {
        let rule = create(&pool, "Original", "old hint", None).await.unwrap();
        let updated = update(&pool, rule.id, Some("Revised"), None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.text, "Revised");
        assert_eq!(updated.hint, "old hint");
    }

    #[sqlx::test]
    async fn delete_hides_from_list_but_report_still_resolves(pool: PgPool) {
        let rule = create(&pool, "Doomed", "", None).await.unwrap();
        assert!(delete(&pool, rule.id).await.unwrap());
        assert!(!delete(&pool, rule.id).await.unwrap());

        // Gone from the public ordered list…
        assert!(list_ordered(&pool).await.unwrap().is_empty());
        // …but a report citing it still resolves the discarded rule.
        let cited = find_by_ids(&pool, &[rule.id]).await.unwrap();
        assert_eq!(cited.len(), 1);
        assert_eq!(cited[0].text, "Doomed");

        // Updating a deleted rule is a no-op.
        assert!(
            update(&pool, rule.id, Some("x"), None, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn translations_round_trip_sorted(pool: PgPool) {
        let rule = create(&pool, "Default text", "", None).await.unwrap();
        sqlx::query!(
            r#"INSERT INTO rule_translations (id, rule_id, language, text, hint)
               VALUES ($1, $2, 'fr', 'Texte', 'Indice'), ($3, $2, 'de', 'Text', '')"#,
            id::next(),
            rule.id,
            id::next(),
        )
        .execute(&pool)
        .await
        .unwrap();

        let translations = translations_for(&pool, &[rule.id]).await.unwrap();
        let langs: Vec<_> = translations.iter().map(|t| t.language.as_str()).collect();
        assert_eq!(langs, ["de", "fr"]);
    }
}
