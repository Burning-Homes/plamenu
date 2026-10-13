//! Entity activity browsing reuses addressed posts and authoritative native
//! conversation/quote relations. It does not aggregate a resource's children.
mod target;
use crate::{AppState, entities, error::ApiError, webmentions};
use plamenu_db::{DbError, status};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
pub use target::{Target, resolve};
#[derive(Default, Deserialize)]
pub struct Query {
    pub before: Option<i64>,
    pub source: Option<String>,
    pub kind: Option<String>,
}
impl Query {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self
            .source
            .as_deref()
            .is_some_and(|s| !matches!(s, "" | "local" | "remote" | "webmention"))
            || self.kind.as_deref().is_some_and(|s| {
                !matches!(
                    s,
                    "" | "create" | "reply" | "announce" | "like" | "quote" | "webmention"
                )
            })
        {
            return Err(ApiError::Unprocessable("Unknown activity filter".into()));
        }
        Ok(())
    }
}
#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    status_id: Option<i64>,
    actor_id: Option<i64>,
    kind: String,
    source: String,
}
pub struct Page {
    pub items: Vec<Value>,
    pub websites: HashMap<i64, webmentions::Mention>,
    pub before: Option<i64>,
}
pub async fn page(
    state: &AppState,
    target: &Target,
    viewer: Option<i64>,
    q: &Query,
) -> Result<Page, ApiError> {
    q.validate()?;
    let mut before = q.before;
    let mut result = Page {
        items: Vec::new(),
        websites: HashMap::new(),
        before: None,
    };
    for _ in 0..10 {
        let rows = read_rows(state, target, viewer, q, before).await?;
        let full = rows.len() == 51;
        let rows = rows.into_iter().take(50).collect::<Vec<_>>();
        let ids = rows.iter().filter_map(|r| r.status_id).collect::<Vec<_>>();
        let posts = status::find_by_ids(&state.pool, &ids).await?;
        let posts = entities::filter_viewable(&state.pool, &posts, viewer).await?;
        let values =
            entities::render_statuses(&state.pool, &state.config.domain, &posts, viewer).await?;
        let values: HashMap<i64, Value> = values
            .into_iter()
            .filter_map(|s| Some((s["id"].as_str()?.parse().ok()?, s)))
            .collect();
        let actor_ids = rows.iter().filter_map(|r| r.actor_id).collect::<Vec<_>>();
        let actors =
            entities::render_accounts_by_ids(&state.pool, &state.config.domain, &actor_ids, viewer)
                .await?;
        let actors: HashMap<i64, Value> = actors
            .into_iter()
            .filter_map(|a| Some((a["id"].as_str()?.parse().ok()?, a)))
            .collect();
        let ids = rows
            .iter()
            .filter(|r| r.source == "webmention")
            .map(|r| r.id)
            .collect::<Vec<_>>();
        let websites: Vec<webmentions::Mention> = sqlx::query_as(
            "SELECT * FROM webmentions WHERE id=ANY($1) AND active AND target=ANY($2)",
        )
        .bind(ids)
        .bind(&target.aliases)
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?;
        let mut websites = websites
            .into_iter()
            .map(|m| (m.id, m))
            .collect::<HashMap<_, _>>();
        for row in rows {
            before = Some(row.id);
            if let Some(sid) = row.status_id {
                if let Some(post) = values.get(&sid) {
                    result.items.push(json!({"id":row.id.to_string(),"kind":row.kind,"source":row.source,"account":row.actor_id.and_then(|id|actors.get(&id)),"status":post}));
                }
            } else if let Some(mention) = websites.remove(&row.id)
                && webmentions::visible(state, &mention).await?
            {
                result.items.push(json!({"id":row.id.to_string(),"kind":"webmention","source":"webmention","mention":mention.entity()}));
                result.websites.insert(row.id, mention);
            }
            if result.items.len() == 50 {
                result.before = before;
                return Ok(result);
            }
        }
        if !full {
            return Ok(result);
        }
    }
    result.before = before;
    Ok(result)
}

async fn read_rows(
    state: &AppState,
    target: &Target,
    viewer: Option<i64>,
    q: &Query,
    before: Option<i64>,
) -> Result<Vec<Row>, ApiError> {
    Ok(sqlx::query_as(r"
            WITH RECURSIVE seed_statuses AS (
                SELECT s.id FROM statuses s WHERE s.deleted_at IS NULL -- STUBFILTER
                  AND (s.id=ANY($1) OR s.in_reply_to_uri=ANY($2)
                       OR EXISTS(SELECT 1 FROM webmention_interactions i WHERE i.status_id=s.id AND i.target=ANY($2)))
            ), threads(id) AS (
                SELECT DISTINCT sc.conversation_id FROM seed_statuses seed
                JOIN status_conversations sc ON sc.status_id=seed.id
                UNION
                SELECT sc.conversation_id FROM threads t
                JOIN status_conversations parent ON parent.conversation_id=t.id
                JOIN quotes q ON q.quoted_status_id=parent.status_id AND q.state='accepted'
                JOIN status_conversations sc ON sc.status_id=q.status_id
            ), posts AS (
                SELECT s.id,s.account_id,s.reblog_of_id,
                       CASE WHEN i.kind='repost' OR s.reblog_of_id IS NOT NULL THEN 'announce'
                            WHEN i.kind='like' THEN 'like'
                            WHEN i.kind='quote' OR EXISTS(SELECT 1 FROM quotes q WHERE q.status_id=s.id AND q.state='accepted') THEN 'quote'
                            WHEN i.kind='reply' OR s.in_reply_to_id IS NOT NULL OR s.in_reply_to_uri IS NOT NULL THEN 'reply'
                            ELSE 'create' END AS kind
                FROM statuses s LEFT JOIN webmention_interactions i ON i.status_id=s.id
                WHERE s.deleted_at IS NULL -- STUBFILTER
                  AND (s.id IN (SELECT id FROM seed_statuses)
                    OR EXISTS(SELECT 1 FROM status_conversations sc JOIN threads t ON t.id=sc.conversation_id WHERE sc.status_id=s.id)
                    OR s.reblog_of_id IN (SELECT id FROM seed_statuses)
                    OR EXISTS(SELECT 1 FROM status_conversations sc JOIN threads t ON t.id=sc.conversation_id WHERE sc.status_id=s.reblog_of_id))
            ), entries AS (
                SELECT p.id,p.id AS status_id,p.account_id AS actor_id,p.kind,
                       CASE WHEN a.domain IS NULL OR a.portable THEN 'local' ELSE 'remote' END AS source
                FROM posts p JOIN accounts a ON a.id=p.account_id
                WHERE NOT sender_filtered($3,p.account_id) AND ($4::bigint IS NULL OR NOT sender_filtered($4,p.account_id))
                UNION ALL
                SELECT f.id,f.status_id,f.account_id,'like',CASE WHEN a.domain IS NULL OR a.portable THEN 'local' ELSE 'remote' END
                FROM favourites f JOIN posts p ON p.id=f.status_id JOIN accounts a ON a.id=f.account_id
                WHERE NOT sender_filtered($3,f.account_id) AND ($4::bigint IS NULL OR NOT sender_filtered($4,f.account_id))
                UNION ALL
                SELECT id,NULL::bigint,NULL::bigint,'webmention','webmention' FROM webmentions
                WHERE active AND entry IS NOT NULL AND target=ANY($2)
            )
            SELECT * FROM entries WHERE ($5::bigint IS NULL OR id<$5)
              AND ($6::text IS NULL OR source=$6) AND ($7::text IS NULL OR kind=$7)
            ORDER BY id DESC LIMIT 51
        ").bind(&target.roots).bind(&target.aliases).bind(target.owner).bind(viewer).bind(before)
          .bind(q.source.as_deref().filter(|s|!s.is_empty())).bind(q.kind.as_deref().filter(|s|!s.is_empty()))
          .fetch_all(&state.pool).await.map_err(DbError::from)?)
}
