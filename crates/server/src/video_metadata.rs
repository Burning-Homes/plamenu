//! PeerTube-compatible Video metadata, without inventing reuse permissions.
use crate::{AppState, error::ApiError};
use plamenu_db::{
    account, status, video_captions,
    video_metadata::{self, Metadata},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub const CATEGORIES: [&str; 18] = [
    "Music",
    "Films",
    "Vehicles",
    "Art",
    "Sports",
    "Travels",
    "Gaming",
    "People",
    "Comedy",
    "Entertainment",
    "News & Politics",
    "How To",
    "Education",
    "Activism",
    "Science & Technology",
    "Animals",
    "Kids",
    "Food",
];
pub const LICENCES: [&str; 9] = [
    "CC BY 4.0",
    "CC BY-SA 4.0",
    "CC BY-ND 4.0",
    "CC BY-NC 4.0",
    "CC BY-NC-SA 4.0",
    "CC BY-NC-ND 4.0",
    "CC0 1.0",
    "Public Domain Mark 1.0",
    "All rights reserved",
];
#[must_use]
pub fn licence_url(id: i32) -> Option<&'static str> {
    match id {
        1 => Some("https://creativecommons.org/licenses/by/4.0/"),
        2 => Some("https://creativecommons.org/licenses/by-sa/4.0/"),
        3 => Some("https://creativecommons.org/licenses/by-nd/4.0/"),
        4 => Some("https://creativecommons.org/licenses/by-nc/4.0/"),
        5 => Some("https://creativecommons.org/licenses/by-nc-sa/4.0/"),
        6 => Some("https://creativecommons.org/licenses/by-nc-nd/4.0/"),
        7 => Some("https://creativecommons.org/publicdomain/zero/1.0/"),
        8 => Some("https://creativecommons.org/publicdomain/mark/1.0/"),
        _ => None,
    }
}

#[derive(Default, Deserialize)]
pub struct Input {
    pub category: Option<Value>,
    pub licence: Option<Value>,
    pub spoiler_text: Option<String>,
    pub sensitive: Option<Value>,
}

pub fn parse_id(value: &Value, max: i32) -> Result<Option<i32>, ApiError> {
    let parsed = match value {
        Value::String(raw) if raw.is_empty() => Some(0),
        Value::String(raw) => raw.parse::<i32>().ok(),
        Value::Number(number) => number.as_i64().and_then(|id| i32::try_from(id).ok()),
        _ => None,
    }
    .filter(|id| (0..=max).contains(id))
    .ok_or_else(|| {
        ApiError::Unprocessable("Choose a supported Video category or licence".into())
    })?;
    Ok((parsed != 0).then_some(parsed))
}

fn merge(mut current: Metadata, input: &Input) -> Result<Metadata, ApiError> {
    if let Some(value) = &input.category {
        current.category = parse_id(value, 18)?;
    }
    if let Some(value) = &input.licence {
        current.licence = parse_id(value, 9)?;
        current.licence_version_known = current.licence.is_some();
    }
    if let Some(value) = &input.spoiler_text {
        if value.chars().count() > 500 {
            return Err(ApiError::Unprocessable(
                "Video content warning must be at most 500 characters".into(),
            ));
        }
        value.trim().clone_into(&mut current.spoiler_text);
    }
    if let Some(value) = &input.sensitive {
        current.sensitive = value.as_bool().unwrap_or_else(|| {
            value
                .as_str()
                .is_some_and(|raw| crate::routes::params::truthy(Some(raw)))
        });
    }
    current.sensitive |= !current.spoiler_text.is_empty();
    Ok(current)
}

pub async fn draft(
    state: &AppState,
    owner: i64,
    media_id: i64,
    input: &Input,
) -> Result<Metadata, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !video_metadata::lock_owned_draft(&mut tx, owner, media_id).await? {
        return Err(ApiError::Unprocessable(
            "This Video is already published or scheduled".into(),
        ));
    }
    let before = video_metadata::for_media(&mut *tx, &[media_id])
        .await?
        .remove(&media_id)
        .unwrap_or(Metadata {
            media_id,
            ..Default::default()
        });
    let next = merge(before, input)?;
    if input.category.is_some()
        || input.licence.is_some()
        || input.spoiler_text.is_some()
        || input.sensitive.is_some()
    {
        video_metadata::upsert(&mut *tx, &next).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(next)
}

#[must_use]
pub fn entity(value: &Metadata) -> Value {
    json!({
        "category": value.category.map(|id| json!({"id":id, "name":CATEGORIES[usize::try_from(id - 1).unwrap_or(0)]})),
        "licence": value.licence.map(|id| json!({"id":id, "name":licence_name(value, id), "url":value.licence_version_known.then(|| licence_url(id)).flatten()}))
    })
}

fn licence_name(value: &Metadata, id: i32) -> &str {
    let name = LICENCES[usize::try_from(id - 1).unwrap_or(0)];
    if value.licence_version_known {
        name
    } else {
        name.trim_end_matches(" 4.0").trim_end_matches(" 1.0")
    }
}

pub fn enrich_object(object: &mut Value, value: &Metadata) {
    if let Some(id) = value.category {
        object["category"] = json!({"identifier":id.to_string(), "name":CATEGORIES[usize::try_from(id - 1).unwrap_or(0)]});
    } else {
        // PeerTube ignores omitted/null fields during Update; its validated
        // nonnegative identifier 0 explicitly means no recognized category.
        object["category"] = json!({"identifier":"0", "name":"Unspecified"});
    }
    if let Some(id) = value.licence {
        object["licence"] = json!({"identifier":id.to_string(), "name":licence_name(value, id), "url":value.licence_version_known.then(|| licence_url(id)).flatten()});
    } else {
        // Never leave a stale reuse grant at a peer after an explicit clear.
        object["licence"] = json!({"identifier":"0", "name":"Unspecified"});
    }
}

/// Never trust remote labels or links; only recognized IDs acquire semantics.
pub async fn ingest(state: &AppState, status_id: i64, object: &Value) -> Result<bool, ApiError> {
    if !crate::ingest::object_type_in(object, &["Video"]) {
        return Ok(false);
    }
    let files = plamenu_db::media::for_statuses(&state.pool, &[status_id]).await?;
    if let Some(primary) = files
        .get(&status_id)
        .and_then(|items| items.iter().find(|item| item.kind_or_derived() == "video"))
    {
        let parsed = |key: &str, max| {
            object
                .get(key)
                .and_then(|v| v.get("identifier"))
                .and_then(|value| parse_id(value, max).ok())
                .flatten()
        };
        let licence = parsed("licence", 9);
        let licence_version_known = licence.and_then(licence_url).is_some_and(|url| {
            object
                .get("licence")
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
                == Some(url)
        });
        let next = Metadata {
            media_id: primary.id,
            category: parsed("category", 18),
            licence,
            licence_version_known,
            ..Default::default()
        };
        let before = video_metadata::for_media(&state.pool, &[primary.id])
            .await?
            .remove(&primary.id);
        let changed = before.as_ref().is_some_and(|value| value != &next)
            || (before.is_none() && (next.category.is_some() || next.licence.is_some()));
        if before.as_ref() != Some(&next) {
            video_metadata::upsert(&state.pool, &next).await?;
        }
        return Ok(changed);
    }
    Ok(false)
}

/// The metadata, edited timestamp and signed Update share one transaction.
pub async fn update(
    state: &AppState,
    owner: i64,
    media_id: i64,
    input: &Input,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let publication = video_captions::lock_owned_publication(&mut tx, media_id, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let before = video_metadata::for_media(&mut *tx, &[media_id])
        .await?
        .remove(&media_id)
        .unwrap_or(Metadata {
            media_id,
            ..Default::default()
        });
    // Warnings remain status-level edits: this endpoint edits just these IDs.
    if input.sensitive.is_some() || input.spoiler_text.is_some() {
        return Err(ApiError::Unprocessable(
            "Edit content warnings through the status editor".into(),
        ));
    }
    let next = merge(before.clone(), input)?;
    if next == before {
        return Ok(());
    }
    video_metadata::upsert(&mut *tx, &next).await?;
    let edited_at = time::OffsetDateTime::now_utc();
    sqlx::query("UPDATE statuses SET edited_at=$2, updated_at=$2 WHERE id=$1")
        .bind(publication.status_id)
        .bind(edited_at)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let updated = status::find_by_id(&mut *tx, publication.status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let actor = account::find_by_id(&mut *tx, publication.actor_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::actions::federate_note_update_conn(
        state,
        &mut tx,
        &actor,
        &updated,
        edited_at,
        Vec::new(),
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    crate::streaming::status_edited(state, updated.id).await;
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_UPDATED, &updated).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_ids_are_bounded_and_unspecified_never_grants_a_licence() {
        let empty = Metadata::default();
        assert!(entity(&empty)["licence"].is_null());
        assert_eq!(parse_id(&json!("0"), 9).unwrap(), None);
        for bad in [
            json!(10),
            json!(-1),
            json!(1.5),
            json!(true),
            json!("2junk"),
            json!("999999999999999999999"),
        ] {
            assert!(parse_id(&bad, 9).is_err(), "{bad}");
        }
        let local = merge(
            empty,
            &Input {
                licence: Some(json!(2)),
                spoiler_text: Some("Flashing imagery".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(local.sensitive);
        assert_eq!(entity(&local)["licence"]["url"], licence_url(2).unwrap());
        let unknown_version = Metadata {
            licence: Some(2),
            ..Default::default()
        };
        assert!(entity(&unknown_version)["licence"]["url"].is_null());
        assert_eq!(entity(&unknown_version)["licence"]["name"], "CC BY-SA");
        assert!(
            merge(
                local,
                &Input {
                    spoiler_text: Some("x".repeat(501)),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
