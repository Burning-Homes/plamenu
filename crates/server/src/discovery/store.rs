//! Mapping between persisted discovery records and the pure policy. These
//! helpers are internal services, not administrator routes or fetch workers.

use plamenu_ap::discovery::DiscoverySignals;
use plamenu_db::discovery_store::{self as db, Entry, Settings, Source};
use plamenu_db::{DbError, PgPool};
use serde_json::json;
use time::OffsetDateTime;

use super::policy::{
    Access, Candidate, Decision, Kind, MissingSignalPolicy, Policy, Use, Verification, Visibility,
};

pub fn policy(settings: &Settings) -> Result<Policy, DbError> {
    let disabled_mechanisms = serde_json::from_value(json!(settings.disabled_mechanisms))
        .map_err(|error| DbError::Protocol(error.to_string()))?;
    let fallback = |allow| {
        if allow {
            MissingSignalPolicy::Allow
        } else {
            MissingSignalPolicy::Deny
        }
    };
    Ok(Policy {
        enabled: settings.enabled,
        missing_discoverable: fallback(settings.allow_missing_discoverable),
        missing_indexable: fallback(settings.allow_missing_indexable),
        disabled_mechanisms,
    })
}

/// Keep storage limits and pause state; reject a stale administrator edit.
pub async fn save_policy(
    pool: &PgPool,
    expected_revision: i64,
    policy: &Policy,
) -> Result<bool, DbError> {
    let mut settings = db::settings(pool).await?;
    if settings.revision != expected_revision {
        return Ok(false);
    }
    settings.enabled = policy.enabled;
    settings.allow_missing_discoverable = policy.missing_discoverable == MissingSignalPolicy::Allow;
    settings.allow_missing_indexable = policy.missing_indexable == MissingSignalPolicy::Allow;
    settings.disabled_mechanisms = serde_json::from_value(json!(policy.disabled_mechanisms))
        .map_err(|error| DbError::Protocol(error.to_string()))?;
    db::save_settings(pool, &settings).await
}

/// Reevaluate a raw cached entry with current settings and source facts. A pause
/// stops acquisition, not browsing; disablement hides results. The caller still
/// supplies current instance/viewer/remote restrictions before rendering.
pub fn evaluate_entry(
    settings: &Settings,
    source: &Source,
    entry: &Entry,
    usage: Use,
    access: Access,
    now: OffsetDateTime,
) -> Result<Decision, DbError> {
    if entry.source_id != source.id {
        return Err(DbError::Protocol("discovery source mismatch".into()));
    }
    let signals: DiscoverySignals = serde_json::from_value(json!({
        "discoverable": entry.discoverable, "indexable": entry.indexable,
    }))
    .map_err(|error| DbError::Protocol(error.to_string()))?;
    let kind = match entry.kind.as_str() {
        "source" => Kind::Source,
        "profile" | "community" => Kind::Profile,
        "post" => Kind::Post,
        "video" => Kind::Video,
        _ => return Err(DbError::Protocol("unknown discovery entry kind".into())),
    };
    let visibility = match entry.visibility.as_str() {
        "public" => Visibility::Public,
        "unlisted" => Visibility::Unlisted,
        "private" => Visibility::FollowersOnly,
        "direct" => Visibility::Direct,
        "local" => Visibility::LocalOnly,
        "members" => Visibility::MembersOnly,
        _ => Visibility::Unknown,
    };
    let verification = match (source.verified_at, source.valid_until) {
        (Some(_), Some(_)) => Verification::Canonical {
            checked_at: entry.verified_at,
            valid_until: entry.valid_until,
        },
        _ => Verification::Unverified,
    };
    let access = if access != Access::Allowed {
        access
    } else if entry.state == "withdrawn" {
        Access::Withdrawn
    } else if entry.state != "active" {
        Access::RemoteDenied
    } else if !source.enabled {
        Access::InstanceBlocked
    } else {
        Access::Allowed
    };
    Ok(policy(settings)?.evaluate(
        usage,
        &Candidate {
            kind,
            visibility,
            verification,
            signals,
        },
        access,
        now,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::policy::{Mechanism, Outcome};
    use super::*;
    use time::Duration;

    #[sqlx::test(migrations = "../db/migrations")]
    async fn persisted_policy_defaults_switches_and_cached_results(pool: PgPool) {
        let initial = db::settings(&pool).await.unwrap();
        assert_eq!(policy(&initial).unwrap(), Policy::default());
        let enabled = Policy {
            enabled: true,
            ..Policy::default()
        };
        assert!(
            save_policy(&pool, initial.revision, &enabled)
                .await
                .unwrap()
        );
        let db::Admission::Added(id) = db::add_source(&pool, "https://one.example/", None)
            .await
            .unwrap()
        else {
            panic!()
        };
        db::enqueue(
            &pool,
            id,
            "https://one.example/",
            db::Operation::InspectSource,
            None,
        )
        .await
        .unwrap();
        let lease = db::claim_due(&pool, "test").await.unwrap().remove(0);
        let now = OffsetDateTime::now_utc();
        db::finish_source(
            &pool,
            &lease,
            &db::VerifiedSource {
                canonical_uri: "https://one.example/actor",
                capabilities: &[],
                verified_at: now,
                valid_until: now + Duration::days(1),
            },
        )
        .await
        .unwrap();
        let actor_id = plamenu_db::id::next();
        sqlx::query("INSERT INTO discovery_entries(id,canonical_uri,source_id,kind,actor_uri,state,policy_revision,discoverable,indexable,visibility,title,summary,verified_at,valid_until) VALUES($1,'https://one.example/actor',$2,'profile','https://one.example/actor','active',1,'missing','missing','public','Actor','',now(),now()+interval '1 day')").bind(actor_id).bind(id).execute(&pool).await.unwrap();
        let uri = "https://one.example/post";
        db::enqueue(&pool, id, uri, db::Operation::RefreshEntry, None)
            .await
            .unwrap();
        let lease = db::claim_due(&pool, "test").await.unwrap().remove(0);
        db::finish_entry(
            &pool,
            &lease,
            &db::VerifiedEntry {
                outbox_uri: None,
                published_at: None,
                sensitive: false,
                author_revision: Some(1),
                kind: "post",
                actor_uri: "https://one.example/actor",
                account_id: None,
                discoverable: "missing",
                indexable: "missing",
                visibility: "public",
                title: "Public post",
                summary: "Text",
                language: None,
                verified_at: OffsetDateTime::now_utc(),
                valid_until: now + Duration::hours(1),
            },
        )
        .await
        .unwrap();
        let entry = db::entry(&pool, uri).await.unwrap().unwrap();
        let source = db::source(&pool, id).await.unwrap().unwrap();
        let current = db::settings(&pool).await.unwrap();
        let check = |settings: &Settings| {
            evaluate_entry(
                settings,
                &source,
                &entry,
                Use::IndexPost,
                Access::Allowed,
                OffsetDateTime::now_utc(),
            )
            .unwrap()
            .outcome
        };
        assert_eq!(check(&current), Outcome::Allow);
        let mut strict = enabled.clone();
        strict.missing_indexable = MissingSignalPolicy::Deny;
        strict.disabled_mechanisms.insert(Mechanism::Outbox);
        assert!(save_policy(&pool, current.revision, &strict).await.unwrap());
        assert!(
            !save_policy(&pool, current.revision, &enabled)
                .await
                .unwrap()
        );
        let current = db::settings(&pool).await.unwrap();
        assert_eq!(policy(&current).unwrap(), strict);
        assert_eq!(check(&current), Outcome::Deny);
        assert_eq!(entry.indexable, "missing");
        assert!(
            save_policy(&pool, current.revision, &enabled)
                .await
                .unwrap()
        );
        let mut current = db::settings(&pool).await.unwrap();
        current.paused = true;
        db::save_settings(&pool, &current).await.unwrap();
        let current = db::settings(&pool).await.unwrap();
        assert_eq!(check(&current), Outcome::Allow);
        assert!(db::claim_due(&pool, "paused").await.unwrap().is_empty());
        assert!(
            save_policy(&pool, current.revision, &Policy::default())
                .await
                .unwrap()
        );
        assert_eq!(check(&db::settings(&pool).await.unwrap()), Outcome::Deny);
    }
}
