//! Notification context belongs to the real event; recipients are people.
use std::collections::{BTreeMap, HashMap};

use crate::{DbError, PgConnection, PgPool, managed_actor};

pub const KINDS: &[&str] = &[
    "plamenu:actor.follow",
    "plamenu:actor.follow_request",
    "plamenu:actor.favourite",
    "plamenu:actor.reblog",
    "plamenu:actor.emoji_reaction",
    "plamenu:actor.comment",
    "plamenu:actor.mention",
    "plamenu:actor.quote",
    "plamenu:actor.status",
    "plamenu:actor.invitation",
    "plamenu:actor.transfer",
];

#[must_use]
pub fn base_kind(kind: &str) -> &str {
    match kind {
        "plamenu:actor.comment" | "plamenu:actor.mention" => "mention",
        "plamenu:actor.quote" => "quote",
        "plamenu:actor.status" => "status",
        "plamenu:actor.follow" => "follow",
        "plamenu:actor.follow_request" | "plamenu:actor.invitation" | "plamenu:actor.transfer" => {
            "follow_request"
        }
        "plamenu:actor.favourite" => "favourite",
        "plamenu:actor.reblog" => "reblog",
        "plamenu:actor.emoji_reaction" => "pleroma:emoji_reaction",
        _ => kind,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Event {
    Comment,
    Mention,
    Quote,
    Status,
    Follow,
    FollowRequest,
    Favourite,
    Reblog,
    Reaction,
    Invitation,
    Transfer,
}

impl Event {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Mention => "mention",
            Self::Quote => "quote",
            Self::Status => "status",
            Self::Follow => "follow",
            Self::FollowRequest => "follow_request",
            Self::Favourite => "favourite",
            Self::Reblog => "reblog",
            Self::Reaction => "emoji_reaction",
            Self::Invitation => "invitation",
            Self::Transfer => "transfer",
        }
    }
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Comment => "plamenu:actor.comment",
            Self::Mention => "plamenu:actor.mention",
            Self::Quote => "plamenu:actor.quote",
            Self::Status => "plamenu:actor.status",
            Self::Follow => "plamenu:actor.follow",
            Self::FollowRequest => "plamenu:actor.follow_request",
            Self::Favourite => "plamenu:actor.favourite",
            Self::Reblog => "plamenu:actor.reblog",
            Self::Reaction => "plamenu:actor.emoji_reaction",
            Self::Invitation => "plamenu:actor.invitation",
            Self::Transfer => "plamenu:actor.transfer",
        }
    }
    #[must_use]
    pub const fn policy_kind(self) -> &'static str {
        match self {
            Self::Comment | Self::Mention => "mention",
            Self::Quote => "quote",
            Self::Status => "status",
            Self::Follow => "follow",
            Self::FollowRequest | Self::Invitation | Self::Transfer => "follow_request",
            Self::Favourite => "favourite",
            Self::Reblog => "reblog",
            Self::Reaction => "pleroma:emoji_reaction",
        }
    }
    #[must_use]
    pub fn from_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "follow" => Self::Follow,
            "follow_request" => Self::FollowRequest,
            "favourite" => Self::Favourite,
            "reblog" => Self::Reblog,
            "pleroma:emoji_reaction" => Self::Reaction,
            "mention" => Self::Mention,
            "quote" => Self::Quote,
            "status" => Self::Status,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Context {
    pub actor_id: i64,
    /// The original account the interaction targeted, used to retract precisely
    /// that event even when it was delivered to several controllers.
    pub source_account_id: i64,
    pub event: Event,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredContext {
    pub notification_id: i64,
    pub actor_account_id: i64,
    pub source_account_id: i64,
    pub event: String,
    pub actor_kind: String,
}

/// A managed actor may legitimately have no eligible local inbox recipients.
/// `Some([])` must not fall back to storing an unreachable group notification.
pub async fn actor_recipients(pool: &PgPool, actor: i64) -> Result<Option<Vec<i64>>, DbError> {
    if managed_actor::kind(pool, actor).await?.is_none() {
        return Ok(None);
    }
    Ok(Some(
        managed_actor::notification_recipients(pool, actor).await?,
    ))
}

/// Only an actual hosted Video publication establishes channel context.
pub async fn publication(pool: &PgPool, status: i64) -> Result<Option<(i64, i64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT v.channel_id,s.account_id FROM video_publications v -- STUBFILTER: deleted publications never notify
         JOIN statuses s ON s.id=v.status_id JOIN accounts channel ON channel.id=v.channel_id
         JOIN video_channels c ON c.account_id=v.channel_id JOIN accounts owner ON owner.id=c.owner_account_id
         WHERE v.status_id=$1 AND s.deleted_at IS NULL AND s.visibility IN ('public','unlisted')
           AND channel.deleted_at IS NULL AND channel.suspended_at IS NULL
           AND owner.deleted_at IS NULL AND owner.suspended_at IS NULL",
    ).bind(status).fetch_optional(pool).await?)
}

/// Follow the stored reply graph, never an arbitrary audience or Mention tag.
/// Recipient visibility is checked separately before creating any notification.
pub async fn reply_publication(pool: &PgPool, status: i64) -> Result<Option<(i64, i64)>, DbError> {
    Ok(sqlx::query_as(
        "WITH RECURSIVE ancestors AS ( -- STUBFILTER: every reply ancestor excludes deleted rows
            SELECT s.id,s.in_reply_to_id,ARRAY[s.id] AS path FROM statuses s
            WHERE s.id=$1 AND s.deleted_at IS NULL
            UNION ALL
            SELECT p.id,p.in_reply_to_id,a.path||p.id FROM ancestors a JOIN statuses p ON p.id=a.in_reply_to_id
            WHERE cardinality(a.path)<128 AND NOT p.id=ANY(a.path)
              AND p.deleted_at IS NULL
         ) SELECT v.channel_id,s.account_id FROM ancestors a
         JOIN video_publications v ON v.status_id=a.id JOIN statuses s ON s.id=v.status_id
         JOIN accounts channel ON channel.id=v.channel_id
         JOIN video_channels c ON c.account_id=v.channel_id JOIN accounts owner ON owner.id=c.owner_account_id
         WHERE a.id<>$1 AND channel.deleted_at IS NULL AND channel.suspended_at IS NULL
           AND owner.deleted_at IS NULL AND owner.suspended_at IS NULL
         ORDER BY cardinality(a.path) LIMIT 1",
    ).bind(status).fetch_optional(pool).await?)
}

/// A managed role never broadens a post's audience. In particular, a direct
/// reply on a public video reaches only controllers explicitly addressed by
/// that reply; an unrelated editor cannot learn its text via notifications.
pub async fn readable_recipients(
    pool: &PgPool,
    status: i64,
    recipients: &[i64],
) -> Result<Vec<i64>, DbError> {
    if recipients.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "SELECT recipient FROM unnest($2::bigint[]) recipient JOIN statuses s ON s.id=$1 -- STUBFILTER: deleted posts never notify
         WHERE s.deleted_at IS NULL AND (
            s.account_id=recipient OR s.visibility IN ('public','unlisted','local')
            OR EXISTS(SELECT 1 FROM status_mentions m WHERE m.status_id=s.id AND m.account_id=recipient)
            OR (s.visibility='private' AND EXISTS(SELECT 1 FROM follows f
                WHERE f.account_id=recipient AND f.target_account_id=s.account_id AND NOT f.pending))
         )",
    ).bind(status).bind(recipients).fetch_all(pool).await?)
}

/// Route a single interaction. A collaborator who authored the video keeps
/// their ordinary recipient entitlement, with the same channel context.
pub async fn interaction_routes(
    pool: &PgPool,
    recipient: i64,
    kind: &str,
    status: Option<i64>,
) -> Result<Vec<(i64, Option<Context>)>, DbError> {
    let Some(event) = Event::from_kind(kind) else {
        return Ok(vec![(recipient, None)]);
    };
    if let Some(recipients) = actor_recipients(pool, recipient).await? {
        let context = Context {
            actor_id: recipient,
            source_account_id: recipient,
            event,
        };
        return Ok(recipients
            .into_iter()
            .map(|id| (id, Some(context)))
            .collect());
    }
    if matches!(event, Event::Favourite | Event::Reblog | Event::Reaction)
        && let Some(status) = status
        && let Some((channel, author)) = publication(pool, status).await?
        && recipient == author
    {
        let mut recipients = managed_actor::notification_recipients(pool, channel).await?;
        recipients.push(recipient);
        recipients.sort_unstable();
        recipients.dedup();
        let context = Context {
            actor_id: channel,
            source_account_id: recipient,
            event,
        };
        return Ok(recipients
            .into_iter()
            .map(|id| (id, Some(context)))
            .collect());
    }
    Ok(vec![(recipient, None)])
}

pub async fn attach(conn: &mut PgConnection, rows: &[(i64, Context)]) -> Result<(), DbError> {
    if rows.is_empty() {
        return Ok(());
    }
    let values = serde_json::Value::Array(
        rows.iter()
            .map(|(id, context)| {
                serde_json::json!({
                    "notification_id": id, "actor_account_id": context.actor_id,
                    "source_account_id": context.source_account_id, "event": context.event.name(),
                })
            })
            .collect(),
    );
    sqlx::query(
        "INSERT INTO notification_actor_contexts(notification_id,actor_account_id,source_account_id,event)
         SELECT notification_id,actor_account_id,source_account_id,event FROM jsonb_to_recordset($1)
           AS v(notification_id bigint,actor_account_id bigint,source_account_id bigint,event text)
         ON CONFLICT(notification_id,actor_account_id,event) DO NOTHING",
    ).bind(values).execute(conn).await?;
    Ok(())
}

pub async fn load(pool: &PgPool, ids: &[i64]) -> Result<HashMap<i64, Vec<StoredContext>>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, StoredContext>(
        "SELECT c.notification_id,c.actor_account_id,c.source_account_id,c.event,
                CASE WHEN channel.account_id IS NOT NULL THEN 'channel' ELSE 'group' END AS actor_kind
         FROM notification_actor_contexts c LEFT JOIN video_channels channel ON channel.account_id=c.actor_account_id
         WHERE c.notification_id=ANY($1)
         ORDER BY channel.account_id IS NULL,c.actor_account_id,c.event",
    ).bind(ids).fetch_all(pool).await?;
    let mut result: HashMap<i64, Vec<StoredContext>> = HashMap::new();
    for row in rows {
        result.entry(row.notification_id).or_default().push(row);
    }
    Ok(result)
}

pub async fn visible(
    pool: &PgPool,
    notification: i64,
    recipient: i64,
    report: Option<i64>,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT notification_visible($1,$2,$3)")
        .bind(recipient)
        .bind(notification)
        .bind(report)
        .fetch_one(pool)
        .await?)
}

pub type PostContexts = BTreeMap<i64, Vec<Context>>;

pub fn add_context(contexts: &mut PostContexts, recipient: i64, context: Context) {
    let rows = contexts.entry(recipient).or_default();
    if !rows.contains(&context) {
        rows.push(context);
    }
}
