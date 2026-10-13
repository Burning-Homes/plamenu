//! Resolve the containing resource before considering any activity row.
use crate::{AppState, entities, error::ApiError, music_entities};
use plamenu_db::{DbError, status};
#[derive(Clone, Debug)]
pub struct Target {
    pub path: &'static str,
    pub id: i64,
    pub title: String,
    pub uri: String,
    pub aliases: Vec<String>,
    pub roots: Vec<i64>,
    pub owner: i64,
}
impl Target {
    #[must_use]
    pub fn page(&self) -> String {
        format!("/music/{}/{}", self.path, self.id)
    }
}
pub async fn resolve(
    state: &AppState,
    path: &str,
    id: i64,
    viewer: Option<i64>,
) -> Result<Target, ApiError> {
    let domain = &state.config.domain;
    let human = format!("https://{domain}/music/{path}/{id}");
    let mut aliases = vec![human.clone()];
    let mut roots = Vec::new();
    let (path, title, uri, owner) = match path {
        "artists" | "albums" => {
            let kind = if path == "artists" {
                music_entities::Kind::Artist
            } else {
                music_entities::Kind::Album
            };
            let row = music_entities::get(state, kind, id, viewer).await?;
            (
                kind.path(),
                row.name,
                music_entities::uri(domain, kind, id),
                row.owner_id,
            )
        }
        "assets" => {
            let row = crate::music_assets::get(state, id, viewer).await?;
            if let Some(post) =
                crate::music_publications::published(state, row.owner_id, id).await?
            {
                let author = plamenu_db::account::find_by_id(&state.pool, post.account_id)
                    .await?
                    .ok_or(ApiError::NotFound)?;
                let uri = entities::status_uri_for_account(domain, &post, &author);
                aliases.extend([uri.clone(), format!("{uri}/track")]);
                roots.push(post.id);
            }
            aliases.extend([format!("{human}/stream"), format!("{human}/download")]);
            ("assets", row.title, human.clone(), row.owner_id)
        }
        "collections" => {
            let row = crate::music_collections::get(state, id, viewer).await?;
            let uri = if row.kind == "library" {
                crate::music_library_federation::uri(domain, id)
            } else {
                human.clone()
            };
            ("collections", row.name, uri, row.owner_id)
        }
        "radios" => {
            let row = crate::music_radios::get(state, id, viewer).await?;
            ("radios", row.name, human.clone(), row.owner_id)
        }
        "remote" => {
            let row =
                crate::music_remote::get(state, id, viewer.ok_or(ApiError::NotFound)?).await?;
            aliases.push(row.uri.clone());
            if let Some(uri) = row.media_of {
                aliases.push(uri);
            }
            let posts: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM statuses WHERE uri=ANY($1) AND deleted_at IS NULL -- STUBFILTER",
            )
            .bind(&aliases)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)?;
            roots.extend(posts);
            ("remote", row.name, row.uri, row.controller_id)
        }
        _ => return Err(ApiError::NotFound),
    };
    aliases.push(uri.clone());
    aliases.sort();
    aliases.dedup();
    // A containing resource is not an alternative grant to its publication.
    let posts = status::find_by_ids(&state.pool, &roots).await?;
    roots = entities::filter_viewable(&state.pool, &posts, viewer)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();
    Ok(Target {
        path,
        id,
        title,
        uri,
        aliases,
        roots,
        owner,
    })
}
