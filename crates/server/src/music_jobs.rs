//! Operator inspection and termination of native work, with no arbitrary PIDs.
use crate::{AppState, error::ApiError, music_assets};
use plamenu_db::{
    DbError,
    admin_action_log::{self, NewActionLog},
};
use serde::{Deserialize, Serialize};
#[derive(sqlx::FromRow, Serialize)]
pub struct Job {
    pub kind: String,
    #[serde(serialize_with = "string_id")]
    pub id: i64,
    #[serde(serialize_with = "string_id")]
    pub owner_id: i64,
    pub state: String,
}
#[derive(sqlx::FromRow, Serialize)]
pub struct Count {
    pub kind: String,
    pub state: String,
    pub count: i64,
}
#[derive(Default, Deserialize)]
pub struct Query {
    pub before: Option<i64>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub kind: String,
    pub id: String,
}
#[derive(Serialize)]
pub struct Page {
    pub counts: Vec<Count>,
    pub items: Vec<Job>,
    pub before: Option<String>,
}
pub async fn page(state: &AppState, q: &Query) -> Result<Page, ApiError> {
    let counts=sqlx::query_as("SELECT kind,state,count(*) AS count FROM (SELECT 'rendition'::text AS kind,state FROM music_renditions UNION ALL SELECT 'export',state FROM music_downloads UNION ALL SELECT 'output',transport FROM music_outputs UNION ALL SELECT 'folder',state FROM music_local_folders UNION ALL SELECT 'recovery',CASE WHEN applied THEN 'applied' ELSE 'pending' END FROM music_local_mutations) jobs GROUP BY kind,state ORDER BY kind,state").fetch_all(&state.pool).await.map_err(DbError::from)?;
    let mut items:Vec<Job>=sqlx::query_as("SELECT * FROM (SELECT 'rendition'::text AS kind,id,owner_id,state FROM music_renditions WHERE state IN('queued','running') UNION ALL SELECT 'export',id,owner_id,state FROM music_downloads WHERE state IN('queued','running') UNION ALL SELECT 'output',id,owner_id,transport FROM music_outputs WHERE enabled AND (transport='playing' OR worker_lease IS NOT NULL) UNION ALL SELECT 'folder',id,owner_id,state FROM music_local_folders WHERE state IN('queued','scanning','indexing') AND NOT EXISTS(SELECT 1 FROM music_local_mutations m WHERE m.folder_id=music_local_folders.id) UNION ALL SELECT 'recovery',id,owner_id,CASE WHEN applied THEN 'applied' ELSE 'pending' END FROM music_local_mutations) jobs WHERE ($1::bigint IS NULL OR id<$1) ORDER BY id DESC,kind LIMIT 101").bind(q.before).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let before = if items.len() > 100 {
        items.truncate(100);
        items.last().map(|r| r.id.to_string())
    } else {
        None
    };
    Ok(Page {
        counts,
        items,
        before,
    })
}
#[derive(sqlx::FromRow)]
struct Target {
    owner_id: i64,
}
/// Cancelling an active run preserves completed caches and saved output/folder
/// configuration. Durable recovery cannot be terminated halfway through repair.
pub async fn cancel(state: &AppState, actor: i64, r: &Reference) -> Result<(), ApiError> {
    let id =
        r.id.parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| ApiError::Unprocessable("Invalid task ID".into()))?;
    if !matches!(
        r.kind.as_str(),
        "rendition" | "export" | "output" | "folder"
    ) {
        return Err(ApiError::Unprocessable(
            "This task cannot be cancelled".into(),
        ));
    }
    let target:Option<Target>=sqlx::query_as("SELECT owner_id FROM (SELECT 'rendition'::text AS kind,id,owner_id FROM music_renditions UNION ALL SELECT 'export',id,owner_id FROM music_downloads UNION ALL SELECT 'output',id,owner_id FROM music_outputs UNION ALL SELECT 'folder',id,owner_id FROM music_local_folders) jobs WHERE kind=$1 AND id=$2").bind(&r.kind).bind(id).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    let target = target.ok_or(ApiError::NotFound)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, target.owner_id).await?;
    let changed=match r.kind.as_str(){
        "rendition"=>sqlx::query("DELETE FROM music_renditions WHERE id=$1 AND owner_id=$2 AND state IN('queued','running')"),
        "export"=>sqlx::query("DELETE FROM music_downloads WHERE id=$1 AND owner_id=$2 AND state IN('queued','running')"),
        "output"=>sqlx::query("UPDATE music_outputs SET transport='stopped',generation=generation+1,revision=revision+1,worker_lease=NULL,lease_until=NULL,position_seconds=0,last_error=NULL,updated_at=now() WHERE id=$1 AND owner_id=$2 AND enabled AND (transport='playing' OR worker_lease IS NOT NULL)"),
        "folder"=>sqlx::query("UPDATE music_local_folders SET state='idle',revision=revision+1,lease=NULL,lease_until=NULL,next_scan_at=now()+interval '60 seconds' WHERE id=$1 AND owner_id=$2 AND state IN('queued','scanning','indexing') AND NOT EXISTS(SELECT 1 FROM music_local_mutations m WHERE m.folder_id=$1)"),
        _=>unreachable!(),
    }.bind(id).bind(target.owner_id).execute(&mut *tx).await.map_err(DbError::from)?.rows_affected();
    if changed != 1 {
        return Err(ApiError::NotFound);
    }
    if r.kind == "output" {
        sqlx::query("DELETE FROM music_output_chunks WHERE output_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    admin_action_log::record_tx(
        &mut tx,
        NewActionLog {
            account_id: actor,
            action: "cancel",
            target_type: "MusicJob",
            target_id: id,
            human_identifier: &r.kind,
            permalink: Some("/admin/music/jobs"),
        },
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde field serializer callback"
)]
fn string_id<S: serde::Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Cancellation {
    One(Reference),
    Many(Batch),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub jobs: Vec<Reference>,
}
pub async fn cancel_many(
    state: &AppState,
    actor: i64,
    jobs: &[Reference],
) -> Result<Vec<Reference>, ApiError> {
    if jobs.is_empty() || jobs.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Select between 1 and 100 tasks".into(),
        ));
    }
    let mut unique = std::collections::HashSet::new();
    for r in jobs {
        if !matches!(
            r.kind.as_str(),
            "rendition" | "export" | "output" | "folder"
        ) || r.id.parse::<i64>().ok().is_none_or(|id| id <= 0)
            || !unique.insert((&r.kind, &r.id))
        {
            return Err(ApiError::Unprocessable("Invalid or duplicate task".into()));
        }
    }
    let mut cancelled = Vec::new();
    for r in jobs {
        match cancel(state, actor, r).await {
            Ok(()) => cancelled.push(Reference {
                kind: r.kind.clone(),
                id: r.id.clone(),
            }),
            Err(ApiError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(cancelled)
}
