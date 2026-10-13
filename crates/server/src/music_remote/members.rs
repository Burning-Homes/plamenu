//! Collection pages retain reference edges independently of metadata containment.
use super::{
    document::{Document, Kind},
    live_proof, live_resource, readable_query,
};
use crate::{AppState, error::ApiError};
use plamenu_ap::activity::id_of;
use plamenu_db::DbError;
use serde_json::Value;
use std::collections::HashMap;
use time::OffsetDateTime;

#[derive(Default)]
pub(super) struct Pages {
    pub next: Option<String>,
    pub exhaustive: bool,
    pub items: Vec<(String, Vec<Value>)>,
}
pub(super) async fn store(
    conn: &mut sqlx::PgConnection,
    parent: &str,
    pages: &Pages,
    documents: &[Document],
    started: OffsetDateTime,
) -> Result<(), ApiError> {
    if pages.items.is_empty() {
        return Ok(());
    }
    let page_ids = pages
        .items
        .iter()
        .map(|(uri, _)| uri.clone())
        .collect::<Vec<_>>();
    let uri_docs = documents
        .iter()
        .map(|doc| (doc.uri.as_str(), doc))
        .collect::<HashMap<_, _>>();
    sqlx::query("DELETE FROM music_remote_members WHERE parent_uri=$1 AND (page_uri=ANY($2) OR $3) AND fetched_at<=$4")
        .bind(parent).bind(&page_ids).bind(pages.exhaustive).bind(started).execute(&mut *conn).await.map_err(DbError::from)?;
    for (page, items) in &pages.items {
        for item in items {
            let Some(reference) = id_of(item) else {
                continue;
            };
            let Some(doc) = uri_docs.get(reference).copied() else {
                continue;
            };
            let target = if doc.kind == Kind::Audio {
                doc.media_of
                    .as_deref()
                    .and_then(|uri| uri_docs.get(uri).copied())
                    .filter(|doc| doc.kind == Kind::Track)
                    .unwrap_or(doc)
            } else {
                doc
            };
            let source =
                (doc.kind == Kind::Audio && doc.uri != target.uri).then_some(doc.uri.as_str());
            sqlx::query("INSERT INTO music_remote_members(parent_uri,page_uri,reference_uri,resource_uri,source_uri,fetched_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(parent_uri,page_uri,reference_uri) DO UPDATE SET resource_uri=EXCLUDED.resource_uri,source_uri=EXCLUDED.source_uri,fetched_at=EXCLUDED.fetched_at WHERE music_remote_members.fetched_at<=EXCLUDED.fetched_at")
                .bind(parent).bind(page).bind(reference).bind(&target.uri).bind(source).bind(started).execute(&mut *conn).await.map_err(DbError::from)?;
        }
    }
    Ok(())
}
pub(super) async fn ids(
    state: &AppState,
    parent: &str,
    viewer: i64,
    before: Option<i64>,
) -> Result<Vec<i64>, ApiError> {
    Ok(sqlx::query_scalar(concat!("SELECT DISTINCT resource.id FROM (",readable_query!("r.uri IN(SELECT resource_uri FROM music_remote_members WHERE parent_uri=$1)",""),") resource JOIN music_remote_members m ON m.resource_uri=resource.uri WHERE m.parent_uri=$1 AND ($3::bigint IS NULL OR resource.id<$3) AND (m.source_uri IS NULL OR EXISTS(SELECT 1 FROM (",readable_query!("r.uri=m.source_uri",""),") source WHERE source.uri=m.source_uri AND (source.media_of=resource.uri OR source.uri=resource.uri))) ORDER BY resource.id DESC LIMIT 100"))
        .bind(parent).bind(Some(viewer)).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
