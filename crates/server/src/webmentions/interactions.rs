use super::Mention;
use crate::{
    AppState,
    actions::{self, PostParams},
    error::ApiError,
};
use plamenu_db::{DbError, account::Account, status::Status};

pub async fn publish(
    state: &AppState,
    author: &Account,
    mention: i64,
    kind: &str,
    text: &str,
) -> Result<Status, ApiError> {
    if !matches!(kind, "reply" | "like" | "repost" | "quote") || text.chars().count() > 4000 {
        return Err(ApiError::Unprocessable(
            "Invalid website interaction".into(),
        ));
    }
    let row: Mention = sqlx::query_as("SELECT * FROM webmentions WHERE id=$1 AND active")
        .bind(mention)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)?;
    if !super::visible(state, &row).await? {
        return Err(ApiError::NotFound);
    }
    let body = if text.trim().is_empty() {
        format!("{kind}: {}", row.source)
    } else {
        format!("{}\n\n{}", text.trim(), row.source)
    };
    let (item, _) = actions::post_website_interaction(
        state,
        PostParams {
            username: &author.username,
            text: &body,
            visibility: "public",
            ..Default::default()
        },
        &row.source,
        kind,
        &row.target,
    )
    .await?;
    Ok(item)
}
pub(crate) async fn attach(
    conn: &mut sqlx::PgConnection,
    status: i64,
    source: &str,
    kind: &str,
    target: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO webmention_interactions(status_id,source,kind,target) VALUES($1,$2,$3,$4)",
    )
    .bind(status)
    .bind(source)
    .bind(kind)
    .bind(target)
    .execute(conn)
    .await
    .map_err(DbError::from)?;
    Ok(())
}
pub async fn get(state: &AppState, status: i64) -> Result<Option<(String, String)>, ApiError> {
    Ok(
        sqlx::query_as("SELECT source,kind FROM webmention_interactions WHERE status_id=$1")
            .bind(status)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?,
    )
}
#[must_use]
pub fn marker(kind: &str) -> &'static str {
    match kind {
        "reply" => "u-in-reply-to",
        "like" => "u-like-of",
        "repost" => "u-repost-of",
        "quote" => "u-quotation-of",
        _ => "u-url",
    }
}
