//! rel="me" profile-link verification — Mastodon's
//! `VerifyAccountLinksWorker`. When a local account saves its profile it is
//! enqueued in `link_verification_jobs` (remote actors likewise on ingest,
//! delayed — Mastodon's `check_links!`); this worker fetches every profile
//! field whose value is a URL and, if that page links back to the account
//! with `rel="me"`, stamps the field's `verified_at` row.
//!
//! Verification is re-run over all URL fields each pass, so a back-link that
//! is later removed clears the badge on the next save, matching Mastodon.
//! The result lives in `account_fields` and is not federated, so no
//! `Update(Actor)` is fanned out.

use std::cell::{Cell, RefCell};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::account::Account;
use plamenu_db::{account, link_verification};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use url::Url;

use crate::AppState;

/// How many accounts one pass verifies. Each involves outbound fetches, so the
/// batch is modest.
const BATCH_SIZE: i64 = 5;
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// The bare URL a profile field points at, if any: Plamenu only linkifies (and
/// so only verifies) a field whose whole value is an `https://` URL with no
/// whitespace, matching [`crate::entities::field_value_html`]. Returns the
/// value verbatim (not a re-serialized `Url`, which would normalize away a
/// bare host's missing trailing slash and miss the origin's page).
fn field_url(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    let looks_like_url = trimmed.starts_with("https://")
        && !trimmed.contains(char::is_whitespace)
        && Url::parse(trimmed).is_ok();
    looks_like_url.then_some(trimmed)
}

/// Whether `backlink` (an absolute URL a page declared with `rel="me"`) points
/// at one of the account's profile URLs — its `ActivityPub` id or its human
/// web URL — with or without a trailing slash.
fn matches_profile(backlink: &str, profile_urls: &[String]) -> bool {
    let trimmed = backlink.trim_end_matches('/');
    profile_urls
        .iter()
        .any(|url| trimmed == url.trim_end_matches('/'))
}

/// The URLs a back-link may use to name this profile: derived from the local
/// URL layout for local accounts, the published `id`/`url` for remote ones.
fn profile_urls(state: &AppState, account: &Account) -> Vec<String> {
    if account.is_local() {
        let urls = LocalUserUrls::for_account(
            &state.config.domain,
            &account.username,
            account.uri.as_deref(),
        );
        vec![urls.id, urls.web_url]
    } else {
        account
            .uri
            .iter()
            .chain(account.url.iter())
            .cloned()
            .collect()
    }
}

/// The verification URL of a remote field's sanitized-HTML value — Mastodon's
/// `Field#extract_url_from_html`: the value must be exactly one `<a>` whose
/// visible text (nested spans included) equals its `href`. Anything else can
/// display one URL while linking another, which must never earn a badge.
pub(crate) fn remote_field_url(value: &str) -> Option<String> {
    #[derive(Default)]
    struct AnchorScan {
        href: RefCell<Option<String>>,
        text: RefCell<String>,
        anchor_open: Cell<bool>,
        anchor_done: Cell<bool>,
        invalid: Cell<bool>,
    }
    impl TokenSink for AnchorScan {
        type Handle = ();

        fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
            match token {
                Token::TagToken(Tag {
                    kind: TagKind::StartTag,
                    name,
                    attrs,
                    ..
                }) => {
                    if &*name == "a" {
                        // Exactly one anchor; a second one invalidates.
                        if self.anchor_open.get() || self.anchor_done.get() {
                            self.invalid.set(true);
                        }
                        self.anchor_open.set(true);
                        *self.href.borrow_mut() = attrs
                            .iter()
                            .find(|a| &*a.name.local == "href")
                            .map(|a| a.value.to_string());
                    } else if !self.anchor_open.get() {
                        // Other markup is tolerated only inside the anchor
                        // (Mastodon's single-child rule).
                        self.invalid.set(true);
                    }
                }
                Token::TagToken(Tag {
                    kind: TagKind::EndTag,
                    name,
                    ..
                }) => {
                    if &*name == "a" {
                        self.anchor_open.set(false);
                        self.anchor_done.set(true);
                    }
                }
                Token::CharacterTokens(text) => {
                    if self.anchor_open.get() {
                        self.text.borrow_mut().push_str(&text);
                    } else if !text.trim().is_empty() {
                        self.invalid.set(true);
                    }
                }
                _ => {}
            }
            TokenSinkResult::Continue
        }
    }

    let tokenizer = Tokenizer::new(AnchorScan::default(), TokenizerOpts::default());
    let input = html5ever::buffer_queue::BufferQueue::default();
    input.push_back(StrTendril::from(value));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let scan = tokenizer.sink;
    if scan.invalid.get() || scan.anchor_open.get() {
        return None;
    }
    let href = scan.href.into_inner()?;
    (*scan.text.borrow() == href).then(|| field_url(&href).map(str::to_owned))?
}

/// Fetches `url` and reports whether it links back to one of `profile_urls`
/// with `rel="me"`. Any network/parse failure is a non-verification
/// (best-effort, like the link crawler), never an error.
async fn backlink_confirmed(state: &AppState, url: &str, profile_urls: &[String]) -> bool {
    let page = match state.federation.fetch_page(url, "text/html").await {
        Ok(page) => page,
        Err(error) => {
            tracing::debug!(error = %crate::error::ErrorChain(&error), url, "rel=me fetch failed");
            return false;
        }
    };
    // Resolve relative back-links against the page's final URL, falling back to
    // the requested URL when the response gave no usable one.
    let Some(base) = Url::parse(&page.final_url)
        .ok()
        .or_else(|| Url::parse(url).ok())
    else {
        return false;
    };
    crate::link_preview::rel_me_backlinks(&page.body, &base)
        .iter()
        .any(|backlink| matches_profile(backlink, profile_urls))
}

/// The URL a field would be verified against: local values are bare URLs,
/// remote values our sanitized HTML rendering of one.
fn candidate_url(field: &account::AccountField, local: bool) -> Option<String> {
    if local {
        field_url(&field.value).map(str::to_owned)
    } else {
        remote_field_url(&field.value)
    }
}

/// Verifies one account's URL-valued fields, stamping (or clearing) each
/// field's `verified_at` row as it goes. A no-op when the account is gone or
/// has no URL fields.
pub async fn verify_account(state: &AppState, account_id: i64) {
    let account = match account::find_by_id(&state.pool, account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return,
        Err(error) => {
            tracing::error!(%error, account = account_id, "link verify: account lookup failed");
            return;
        }
    };
    let fields = match account::fields(&state.pool, account.id).await {
        Ok(fields) => fields,
        Err(error) => {
            tracing::error!(%error, account = account.id, "link verify: field lookup failed");
            return;
        }
    };
    let profile_urls = profile_urls(state, &account);
    let now = OffsetDateTime::now_utc();

    for field in fields {
        let confirmed = match candidate_url(&field, account.is_local()) {
            Some(url) => backlink_confirmed(state, &url, &profile_urls).await,
            None => false,
        };
        let stamp = confirmed.then_some(now);
        if stamp.is_none() && field.verified_at.is_none() {
            continue;
        }
        // The update is keyed on the field's position and content: if the
        // profile was edited while we were fetching, it matches nothing and
        // the edit's own re-enqueued job verifies the fresh fields instead.
        if let Err(error) =
            account::set_field_verified_at(&state.pool, account.id, &field, stamp).await
        {
            tracing::error!(%error, account = account.id, "link verify: storing stamp failed");
        }
    }
}

/// Claims and verifies one batch of due jobs; returns how many were claimed.
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match link_verification::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim link verification jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        // The claim leased the job rather than deleting it, so a crash before
        // verification finishes lets the lease expire and it runs again (QC
        // audit #1). Verification is single-attempt, so complete the job either
        // way — dropping one that has been reclaimed past the cap.
        if job.exhausted() {
            tracing::warn!(
                account = job.account_id,
                attempts = job.attempts,
                "dropping link verification after too many crash reclaims"
            );
        } else {
            verify_account(state, job.account_id).await;
        }
        if let Err(error) = link_verification::complete(&state.pool, job.id).await {
            tracing::error!(%error, account = job.account_id, "failed to complete link verification job");
        }
    }
    claimed
}

/// Runs the verification loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("link verification worker started");
        loop {
            if run_due(&state).await == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls() -> Vec<String> {
        let urls = LocalUserUrls::new("plamenu.test", "alice");
        vec![urls.id, urls.web_url]
    }

    #[test]
    fn field_url_only_matches_bare_https() {
        assert!(field_url("https://example.com").is_some());
        assert!(field_url("  https://example.com/path  ").is_some());
        // Not a bare URL / not https / has text.
        assert!(field_url("http://example.com").is_none());
        assert!(field_url("My site: https://example.com").is_none());
        assert!(field_url("just text").is_none());
        assert!(field_url("").is_none());
    }

    #[test]
    fn profile_match_accepts_id_and_web_url() {
        let urls = urls();
        assert!(matches_profile("https://plamenu.test/users/alice", &urls));
        assert!(matches_profile("https://plamenu.test/@alice", &urls));
        // Trailing slash tolerated.
        assert!(matches_profile("https://plamenu.test/@alice/", &urls));
        // A different account or host does not match.
        assert!(!matches_profile("https://plamenu.test/@bob", &urls));
        assert!(!matches_profile("https://evil.test/@alice", &urls));
    }

    #[test]
    fn remote_field_url_requires_one_honest_anchor() {
        // The canonical remote shape: one anchor whose text is its href,
        // possibly split across (invisible/ellipsis) spans by the origin.
        assert_eq!(
            remote_field_url(r#"<a href="https://site.example/me">https://site.example/me</a>"#)
                .as_deref(),
            Some("https://site.example/me")
        );
        assert_eq!(
            remote_field_url(
                r#"<a href="https://site.example/me" rel="nofollow"><span>https://</span><span>site.example/me</span></a>"#
            )
            .as_deref(),
            Some("https://site.example/me")
        );
        // Deceptive or non-URL values earn nothing: text ≠ href, extra
        // content around the anchor, several anchors, plain text, http.
        assert_eq!(
            remote_field_url(r#"<a href="https://evil.example">https://site.example</a>"#),
            None
        );
        assert_eq!(
            remote_field_url(r#"see <a href="https://site.example">https://site.example</a>"#),
            None
        );
        assert_eq!(
            remote_field_url(
                r#"<a href="https://a.example">https://a.example</a><a href="https://b.example">https://b.example</a>"#
            ),
            None
        );
        assert_eq!(remote_field_url("just text"), None);
        assert_eq!(
            remote_field_url(r#"<a href="http://site.example">http://site.example</a>"#),
            None
        );
    }
}
