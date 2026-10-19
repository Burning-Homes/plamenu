//! Read-only catalogue search: no implicit resolution or remote acquisition.
use super::policy::Use;
use crate::{AppState, error::ApiError};
use plamenu_db::discovery_store as db;

pub async fn search(
    state: &AppState,
    viewer: i64,
    term: &str,
    kind: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<db::Entry>, ApiError> {
    if term.trim().is_empty() || term.len() > 512 || term.contains('\0') || limit < 1 {
        return Ok(Vec::new());
    }
    let kinds: &[&str] = match kind {
        None => &["profile", "community", "post", "video"],
        Some("accounts") => &["profile", "community"],
        Some("statuses") => &["post", "video"],
        Some(_) => return Ok(Vec::new()),
    };
    let candidates: Vec<(i64,String)>=sqlx::query_as(
        "SELECT e.id,e.kind FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
         LEFT JOIN discovery_actor_labels a ON a.uri=e.actor_uri
         WHERE e.kind=ANY($1) AND e.state='active' AND e.valid_until>now() AND s.enabled
         AND (e.canonical_uri=$2 OR to_tsvector('simple',coalesce(e.title,'') || ' ' || coalesce(e.summary,'')) @@ plainto_tsquery('simple',$2)
           OR a.username ILIKE '%' || $2 || '%'
           OR a.username || '@' || split_part(split_part(a.uri,'://',2),'/',1)=ltrim($2,'@!')
           OR EXISTS(SELECT 1 FROM discovery_topics t WHERE t.entry_id=e.id AND t.topic=lower(ltrim($2,'#'))))
         ORDER BY (e.canonical_uri=$2) DESC,e.id DESC LIMIT 200")
        .bind(kinds).bind(term).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let filters = crate::filters::compiled_filters(&state.pool, viewer).await?;
    let mut results = Vec::new();
    let mut skipped = 0;
    for (id, kind) in candidates {
        let entry = match kind.as_str() {
            "post" => super::posts::eligible(state, viewer, id, Use::IndexPost).await?,
            "video" => super::videos::eligible(state, viewer, id, Use::IndexVideo).await?,
            _ => super::browse::eligible(state, viewer, id).await?,
        };
        if let Some(mut entry) = entry
            && super::posts::apply_filters_in_context(state, &filters, &mut entry, "search").await?
        {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            results.push(entry);
            if results.len() >= limit.min(80) as usize {
                break;
            }
        }
    }
    Ok(results)
}
