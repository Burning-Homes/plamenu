//! Permanent mention browsing uses the durable addressed-status edges, not
//! dismissible notifications. Current status/audience/moderation still apply.
use crate::{AppState, entities, error::ApiError, webmentions};
use plamenu_db::{DbError, status};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
#[derive(Default, Deserialize)]
pub struct Query {
    pub before: Option<i64>,
    pub source: Option<String>,
    pub private: Option<bool>,
}
impl Query {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self
            .source
            .as_deref()
            .is_some_and(|s| !matches!(s, "" | "local" | "activitypub" | "webmention"))
        {
            return Err(ApiError::Unprocessable("Unknown mention source".into()));
        }
        Ok(())
    }
}
pub struct Page {
    pub items: Vec<Value>,
    pub websites: HashMap<i64, webmentions::Mention>,
    pub before: Option<i64>,
}
pub async fn page(state: &AppState, owner: i64, q: &Query) -> Result<Page, ApiError> {
    q.validate()?;
    let mut before = q.before;
    let source = q.source.as_deref().filter(|s| !s.is_empty());
    let private = q.private.unwrap_or(false);
    let mut page = Page {
        items: Vec::new(),
        websites: HashMap::new(),
        before: None,
    };
    for _ in 0..10 {
        let rows:Vec<(i64,String)>=sqlx::query_as(r"
            SELECT id,source FROM (
                SELECT s.id,CASE WHEN a.domain IS NULL OR a.portable THEN 'local' ELSE 'activitypub' END AS source
                FROM status_mentions m JOIN statuses s ON s.id=m.status_id JOIN accounts a ON a.id=s.account_id
                WHERE m.account_id=$1 AND s.deleted_at IS NULL -- STUBFILTER
                  AND ($2::bigint IS NULL OR s.id<$2)
                  AND (NOT $4 OR s.visibility<>'public')
                  AND NOT sender_filtered($1,s.account_id)
                UNION ALL
                SELECT id,'webmention' AS source FROM webmentions
                WHERE owner_id=$1 AND active AND entry IS NOT NULL
                  AND ($2::bigint IS NULL OR id<$2) AND NOT $4
            ) entries WHERE ($3::text IS NULL OR source=$3) ORDER BY id DESC LIMIT 51
        ").bind(owner).bind(before).bind(source).bind(private).fetch_all(&state.pool).await.map_err(DbError::from)?;
        let full = rows.len() == 51;
        let rows = rows.into_iter().take(50).collect::<Vec<_>>();
        let ids = rows
            .iter()
            .filter(|(_, s)| s != "webmention")
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let candidates = status::find_by_ids(&state.pool, &ids).await?;
        let viewable = entities::filter_viewable(&state.pool, &candidates, Some(owner)).await?;
        let rendered =
            entities::render_statuses(&state.pool, &state.config.domain, &viewable, Some(owner))
                .await?;
        let mut rendered: HashMap<i64, Value> = rendered
            .into_iter()
            .filter_map(|s| Some((s["id"].as_str()?.parse().ok()?, s)))
            .collect();
        let ids = rows
            .iter()
            .filter(|(_, s)| s == "webmention")
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let websites: Vec<webmentions::Mention> =
            sqlx::query_as("SELECT * FROM webmentions WHERE owner_id=$1 AND id=ANY($2) AND active")
                .bind(owner)
                .bind(&ids)
                .fetch_all(&state.pool)
                .await
                .map_err(DbError::from)?;
        let mut websites = websites
            .into_iter()
            .map(|r| (r.id, r))
            .collect::<HashMap<_, _>>();
        for (id, source) in rows {
            before = Some(id);
            if source == "webmention" {
                if let Some(row) = websites.remove(&id)
                    && webmentions::visible(state, &row).await?
                {
                    page.items
                        .push(json!({"id":id.to_string(),"source":source,"mention":row.entity()}));
                    page.websites.insert(id, row);
                }
            } else if let Some(item) = rendered.remove(&id) {
                page.items
                    .push(json!({"id":id.to_string(),"source":source,"status":item}));
            }
            if page.items.len() == 50 {
                page.before = before;
                return Ok(page);
            }
        }
        if !full {
            return Ok(page);
        }
    }
    page.before = before;
    Ok(page)
}
