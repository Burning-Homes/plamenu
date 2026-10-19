//! Avatars already known to normal resolution; no discovery media downloads.
use std::collections::HashMap;

use super::{ApiError, AppState, Markup, WebUser, db, html};

pub(super) struct Images(HashMap<String, String>, HashMap<String, String>);

impl Images {
    pub(super) async fn load(
        state: &AppState,
        user: &WebUser,
        entries: &[db::Entry],
        actors: &[String],
    ) -> Result<Self, ApiError> {
        let ids: Vec<_> = entries.iter().map(|entry| entry.id).collect();
        let rows = db::resolved_avatars(&state.pool, &ids, actors, user.current.account.id).await?;
        let allow_direct =
            crate::entities::allow_direct_media(&state.pool, Some(user.current.account.id)).await;
        let mut images = HashMap::new();
        for (id, uri, file, remote) in rows {
            let image = file
                .map(|file| format!("https://{}/media/{file}", state.config.domain))
                .or_else(|| {
                    remote.map(|_| {
                        crate::entities::media_proxy_url(
                            &state.config.domain,
                            "avatar",
                            id,
                            false,
                            allow_direct,
                        )
                    })
                });
            if let Some(image) = image {
                images.insert(uri, image);
            }
        }
        let uris: Vec<String> = sqlx::query_scalar("SELECT actor_uri FROM discovery_entries WHERE id=ANY($1) AND actor_uri IS NOT NULL UNION SELECT canonical_uri FROM discovery_entries WHERE id=ANY($1) AND kind IN ('profile','community') UNION SELECT channel_uri FROM discovery_video_details WHERE entry_id=ANY($1) UNION SELECT unnest($2::text[])")
            .bind(&ids).bind(actors).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
        let labels = db::actor_labels(&state.pool, &uris, user.current.account.id)
            .await?
            .into_iter()
            .map(|(uri, username, domain)| {
                let handle = format!(
                    "{username}@{}",
                    domain.unwrap_or_else(|| super::server_name(&uri))
                );
                (uri, handle)
            })
            .collect();
        Ok(Self(images, labels))
    }

    pub(super) fn handle(&self, actor: &str, community: bool) -> Markup {
        let Some((username, domain)) = self.1.get(actor).and_then(|h| h.rsplit_once('@')) else {
            // The original-profile link remains available while canonical metadata
            // is pending. A URL is not an account handle.
            return html! {};
        };
        html! {
            p.discovery-handle dir="ltr" {
                span.discovery-handle__part {(if community { "!" } else { "@" })(username)}
                wbr;
                span.discovery-handle__part {"@"(domain)}
            }
        }
    }
    pub(super) fn get(&self, actor: &str) -> Option<&str> {
        self.0.get(actor).map(String::as_str)
    }
}

pub(super) fn avatar(image: Option<&str>, fallback: &Markup) -> Markup {
    html! {
        span.discovery-avatar aria-hidden="true" {
            (fallback)
            @if let Some(image) = image {
                img src=(image) alt="" width="48" height="48" loading="lazy" decoding="async";
            }
        }
    }
}
