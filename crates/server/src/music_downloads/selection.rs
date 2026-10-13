//! Archive snapshots keep identities and proofs, never copied permissions.
use crate::{
    AppState,
    error::ApiError,
    music_collections::{Entry, Playable},
    music_entities::{self, Kind},
    music_queues::Source,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Deserialize, Serialize, sqlx::Type)]
#[sqlx(type_name = "music_download_parent")]
pub struct Parent {
    pub kind: String,
    pub id: i64,
    pub revision: i64,
}
#[derive(Clone, Deserialize, Serialize, sqlx::Type)]
#[sqlx(type_name = "music_download_item")]
pub struct Item {
    pub entry: Entry,
    pub proof: String,
}
#[derive(Clone, Deserialize, Serialize, sqlx::Type)]
#[sqlx(type_name = "music_download_selection")]
pub struct Selection {
    pub items: Vec<Item>,
    pub parent: Option<Parent>,
}
#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub sources: Vec<Source>,
    pub kind: Option<String>,
    pub id: Option<String>,
    pub label: Option<String>,
}
pub async fn parent(state: &AppState, owner: i64, scope: &Parent) -> Result<(), ApiError> {
    let (revision, _) = parent_details(state, owner, &scope.kind, scope.id).await?;
    if revision != scope.revision {
        return Err(ApiError::NotFound);
    }
    Ok(())
}
async fn parent_details(
    state: &AppState,
    owner: i64,
    kind: &str,
    id: i64,
) -> Result<(i64, String), ApiError> {
    match kind {
        "collection" => {
            let row = crate::music_collections::get(state, id, Some(owner)).await?;
            Ok((row.revision, row.name))
        }
        "artist" | "album" => {
            let row = music_entities::get(
                state,
                if kind == "artist" {
                    Kind::Artist
                } else {
                    Kind::Album
                },
                id,
                Some(owner),
            )
            .await?;
            Ok((row.revision, row.name))
        }
        _ => Err(ApiError::Unprocessable(
            "Invalid music download container".into(),
        )),
    }
}
async fn collection(state: &AppState, owner: i64, id: i64) -> Result<Vec<Entry>, ApiError> {
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let page = crate::music_collections::entry_page(state, id, after).await?;
        let visible = crate::music_collections::sources(state, &page.items, Some(owner))
            .await?
            .into_iter()
            .filter_map(|source| source.entry_id.parse::<i64>().ok())
            .collect::<std::collections::HashSet<_>>();
        for entry in page.items {
            if visible.contains(&entry.id) {
                entries.push(entry);
            }
            if entries.len() == state.config.music.max_download_items {
                return Ok(entries);
            }
        }
        match page.after {
            Some(next) => after = Some(next),
            None => return Ok(entries),
        }
    }
}
async fn container(state: &AppState, owner: i64, scope: &Parent) -> Result<Vec<Entry>, ApiError> {
    if scope.kind == "collection" {
        return collection(state, owner, scope.id).await;
    }
    let kind = if scope.kind == "artist" {
        Kind::Artist
    } else {
        Kind::Album
    };
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let page = music_entities::tracks(state, kind, scope.id, Some(owner), after).await?;
        for track in page.items {
            if entries.len() == state.config.music.max_download_items {
                return Ok(entries);
            }
            entries.push(Source::asset(track.id).entry(
                i64::try_from(entries.len() + 1).expect("bounded archive"),
                0,
                1,
            )?);
        }
        match page.after {
            Some(next) => after = Some(next),
            None => return Ok(entries),
        }
    }
}
pub async fn resolve(
    state: &AppState,
    owner: i64,
    input: &Request,
) -> Result<(Selection, String), ApiError> {
    if input.sources.len() > state.config.music.max_download_items
        || input.label.as_ref().is_some_and(|value| {
            value.trim().is_empty() || value.chars().count() > 256 || value.contains('\0')
        })
    {
        return Err(ApiError::Unprocessable(
            "Invalid music download selection".into(),
        ));
    }
    let mut entries = input
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            source.entry(i64::try_from(index + 1).expect("bounded selection"), 0, 1)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (scope, label) = match (&input.kind, &input.id) {
        (Some(kind), Some(id)) => {
            let id = id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or(ApiError::NotFound)?;
            let (revision, label) = parent_details(state, owner, kind, id).await?;
            let scope = Parent {
                kind: kind.clone(),
                id,
                revision,
            };
            entries.extend(container(state, owner, &scope).await?);
            (Some(scope), label)
        }
        (None, None) => (None, "Music download".into()),
        _ => {
            return Err(ApiError::Unprocessable(
                "Choose a complete music download container".into(),
            ));
        }
    };
    entries.truncate(state.config.music.max_download_items);
    crate::music_remote::normalize_sources(state, &mut entries, owner).await?;
    let mut items = Vec::new();
    for entry in entries {
        match super::content::proof(state, &entry, owner).await {
            Ok(proof) => items.push(Item { entry, proof }),
            Err(ApiError::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    if items.is_empty() {
        return Err(ApiError::Unprocessable(
            "No available music selected".into(),
        ));
    }
    let selection = Selection {
        items,
        parent: scope,
    };
    if let Some(scope) = &selection.parent {
        parent(state, owner, scope).await?;
    }
    Ok((selection, input.label.clone().unwrap_or(label)))
}
pub async fn playable(state: &AppState, owner: i64, item: &Item) -> Result<Playable, ApiError> {
    if super::content::proof(state, &item.entry, owner).await? != item.proof {
        return Err(ApiError::NotFound);
    }
    crate::music_collections::sources(state, std::slice::from_ref(&item.entry), Some(owner))
        .await?
        .pop()
        .ok_or(ApiError::NotFound)
}
