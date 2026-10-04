//! Profile extensions needed together when publishing a local actor document.

use crate::DbError;

#[derive(Default)]
pub struct Sidecars {
    pub attribution_domains: Vec<String>,
    pub featured_names: Vec<String>,
    pub identity_documents: Option<serde_json::Value>,
}

/// Load one profile snapshot on the caller's connection, including uncommitted
/// edits in an enclosing profile/identity mutation. Tag order matches the REST
/// featured-tag listing: this author's distributable usage count, then row id.
pub async fn load<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    account_id: i64,
) -> Result<Sidecars, DbError> {
    let row = sqlx::query_as!(
        Sidecars,
        r#"
        SELECT a.attribution_domains AS "attribution_domains!",
               ARRAY(
                   SELECT t.name
                   FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id
                   LEFT JOIN status_tags st ON st.tag_id = ft.tag_id
                   LEFT JOIN statuses s -- STUBKEEP: own-tag aggregate; a stub still witnesses the tag was used
                       ON s.id = st.status_id AND s.account_id = ft.account_id
                      AND s.visibility IN ('public', 'unlisted')
                   WHERE ft.account_id = a.id
                   GROUP BY ft.id, t.name
                   ORDER BY count(s.id) DESC, ft.id DESC
               ) AS "featured_names!",
               p.documents AS "identity_documents?"
        FROM accounts a
        LEFT JOIN account_identity_proofs p ON p.account_id = a.id
        WHERE a.id = $1
        "#,
        account_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(row.unwrap_or_default())
}
