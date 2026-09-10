//! One-time seeding of the persistent `plamenu_bench` database.
//!
//! The dataset mirrors a real staging instance's database (surveyed
//! 2026-07-17) —
//! a real consumer instance's shape, where every render batch has real rows
//! to chew on instead of an empty table: ~61k remote accounts across ~4.5k
//! domains (Zipf-ish, one mastodon.social-like giant), ~450k statuses (28%
//! replies, 95/5 public/unlisted, staging's language mix), ~35% of statuses
//! tagged with ~4 tags each over ~87k distinct tags, ~28% carrying media,
//! ~25% preview cards, ~17k custom emojis, plus
//! quotes/edits/polls/reactions/dislikes and a conversation row per thread
//! root — all at staging's measured ratios, multiplied by [`SCALE`] for the
//! day production outgrows the survey. Deliberate divergence: 300 local
//! accounts / ~11k local statuses (staging has 15/103 — too few to keep the
//! local-timeline and web benches honest).
//!
//! On top of the content, four instance personas exercise as many read-path
//! predicates as possible at once:
//! - `bench_sparse` — 10 follows of prolific authors + one followed hashtag; the pathological
//!   slice-6 home shape. Kept minimal: it is the calibrated baseline persona.
//! - `bench_dense` — the "power user with everything on": 1,500 follows (some with per-follow
//!   `show_reblogs`/`notify`/`languages` settings, some pending), 200 blocks out / 300 blocks in,
//!   150 mutes (half muting notifications, some expiring), 40 personal domain blocks, 25 filters, 8
//!   lists (one 400-member, one exclusive), 15 followed tags, a filtering notification policy with
//!   pending requests, `chosen_languages`, thousands of bookmarks/favourites/notifications.
//! - `bench_popular` — the followed one: ~12k followers, 2k own statuses, pinned statuses, featured
//!   tags, profile fields; the target for followers/profile/AP-serving benches.
//! - `bench_writer` — 5k followers across >1.5k domains so POST /statuses pays a realistic delivery
//!   fan-out.
//!
//! Instance-level moderation is non-empty too: 60 domain blocks
//! (silence/suspend/`reject_media`) and ~1.8k silenced/suspended remote
//! accounts, so `instance_domain_allowed`/`account_hidden` filter real rows.
//!
//! Everything is derived from a fixed RNG seed. Domain-logic rows go through
//! the ordinary `plamenu_db` creation functions; a handful of pure-bulk side
//! tables (`status_tags`, tag/card usages, conversations, fetch failures) are
//! filled with set-based SQL that mirrors the corresponding creation fn —
//! each statement carries a comment pinning it to the fn it mirrors.
//!
//! A `bench_seed` marker table records the seed version: `cargo bench` reuses
//! the database when it matches and rebuilds it when [`SEED_VERSION`] was
//! bumped or `PLAMENU_BENCH_RESEED=1` is set.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    reason = "deterministic bench math over small in-range values"
)]

use std::sync::Arc;

use plamenu_ap::keys::{self, KeyPairPem};
use plamenu_db::account::{self, FieldPair, NewLocalAccount, ProfileUpdate, RemoteAccountData};
use plamenu_db::collection::{self, NewCollection, NewCollectionItem};
use plamenu_db::custom_emoji::{self, RemoteEmojiData};
use plamenu_db::group::{self, MembershipPolicy, NewLocalGroup, PostingPolicy};
use plamenu_db::instance_policy::{self, NewDomainBlock};
use plamenu_db::media::{NewRemoteMedia, NewRendition};
use plamenu_db::notification_policy::{self, Disposition, Policy};
use plamenu_db::poll::{self, NewPoll};
use plamenu_db::preview_card::{self, NewPreviewCard};
use plamenu_db::quote::{self, NewQuote};
use plamenu_db::reaction::{self, NewReaction};
use plamenu_db::status::{self, NewLocalStatus, NewRemoteStatus};
use plamenu_db::status_edit::{self, NewStatusEdit};
use plamenu_db::status_event::StatusEvent;
use plamenu_db::status_participation::State as RsvpState;
use plamenu_db::{
    PgPool, account_domain_block, block, bookmark, conversation, custom_filter, dislike, favourite,
    featured_tag, follow, id, instance_settings, list, marker, media, mention, mute, notification,
    notification_request, pin, preview_card_provider, relay, status_event, status_participation,
    tag, tagged_object, user,
};
use sqlx::{Connection, PgConnection};
use time::{Duration, OffsetDateTime};

/// Bump to invalidate existing `plamenu_bench` databases after changing the
/// dataset shape below.
pub const SEED_VERSION: &str = "4";

const BENCH_DB: &str = "plamenu_bench";
const RNG_SEED: u64 = 0x0050_6c61_6d65_6e75; // "Plamenu"

// --- staging-01 shape × SCALE (survey 2026-07-17, see module doc) ----------
/// Multiplier over the measured staging-01 row counts. Bump it (and reseed +
/// recalibrate every budget in one commit, like a hardware change) when the
/// production instance outgrows the survey — the shape, personas and
/// benchmarks stay fixed, only content volume scales.
const SCALE: u64 = 1;
const REMOTE_DOMAINS: u64 = 4_500 * SCALE;
const REMOTE_ACCOUNTS: u64 = 61_000 * SCALE;
/// Domains 0..[`EMOJI_DOMAINS`] carry custom emojis (staging: 1,367 of 4,541).
const EMOJI_DOMAINS: u64 = 1_400 * SCALE;
const LOCAL_ACCOUNTS: u64 = 300;
/// The first accounts are heavy posters — the sparse persona follows these,
/// reproducing the "few follows, deep per-author history" home-timeline shape.
/// Their volume tracks staging's busiest author (3,863 statuses).
const PROLIFIC_ACCOUNTS: u64 = 50;
const PROLIFIC_STATUSES_EACH: u64 = 1_300 * SCALE;
/// A mid tier of steady posters (staging p99 authors).
const MID_ACCOUNTS: u64 = 500;
const MID_STATUSES_EACH: u64 = 130 * SCALE;
/// The long tail, quadratically skewed across the remaining accounts.
const REGULAR_REMOTE_STATUSES: u64 = 320_000 * SCALE;
const LOCAL_STATUSES: u64 = 9_000;
const BULK_TAGS: u64 = 87_000 * SCALE;
const FAVOURITES: u64 = 70_000 * SCALE;
const BACKGROUND_DISLIKES: u64 = 1_500 * SCALE;
const LOCAL_BOOSTS: u64 = 3_000;
const REMOTE_BOOSTS: u64 = 1_800 * SCALE;
const QUOTES: u64 = 8_900 * SCALE;
const EDITED_STATUSES: u64 = 2_800 * SCALE;
const POLLS: u64 = 1_300 * SCALE;
const REACTIONS: u64 = 900 * SCALE;
/// Distinct news-site domains preview cards come from (staging providers).
const CARD_PROVIDERS: u64 = 6_900 * SCALE;
/// A hot slice of cards shared by many statuses (link storms).
const HOT_CARDS: u64 = 8_000 * SCALE;
const BACKGROUND_FOLLOWS: u64 = 20_000;
const POPULAR_FOLLOWERS: u64 = 11_700;
const WRITER_FOLLOWERS: u64 = 5_000;
const FETCH_FAILURES: i64 = 9_600 * SCALE as i64;
/// Posts in the bench group (the ranked-sort benches) — a busy
/// community's worth, each with a vote spread to rank. Real community
/// submissions are title + link, so these are typed `Page` and carry both.
const GROUP_POSTS: u64 = 2_000;
/// Every Nth group post is an `Event` with a sidecar. The group timelines are
/// the only benched pages dense enough in one topic for the event sidecar batch
/// (`entities.rs`'s `event_map`) to land on a *measured* page rather than merely
/// exist somewhere in the table — a community that posts events is also the
/// shape the E-track federates with.
const GROUP_EVENT_EVERY: u64 = 12;
/// Standalone federated events, so the type is present outside the group too.
const EVENT_STATUSES: u64 = 200;
/// One in this many of `bench_popular`'s posts is a long-form `Article`: the
/// followed persona is the one whose profile, outbox and AP objects are
/// benched, so it is where the title fold and the `<h1>` bake get measured.
const ARTICLE_EVERY: u64 = 7;
/// Replies whose parent never arrived (`in_reply_to_uri` set, `in_reply_to_id`
/// NULL) — the state `idx_statuses_orphan_reply_uri` and the repair path exist
/// for, and which no seeded row has ever been in.
const ORPHAN_REPLIES: u64 = 3_000;
/// Soft-deleted local originals kept as stubs because a reply still points at
/// them, so every `-- STUBFILTER` predicate filters real rows.
const STUB_STATUSES: i64 = 500;
/// Cached translations. Never read by a benchmark (no translation backend is
/// configured), but the table is swept by maintenance and capped by settings.
const TRANSLATED_STATUSES: i64 = 5_000;
/// Local custom emojis: `emoji_map` buckets shortcodes by author domain, and
/// the `domain IS NULL` arm has never been taken because only remote statuses
/// carried shortcodes.
const LOCAL_EMOJIS: u64 = 800;
const COLLECTIONS: u64 = 120;
/// Accepted relays. Every public activity's fan-out widens by exactly this
/// many, uniformly — which is why it stays small.
const RELAYS: u64 = 3;
/// Votes on the poll `bench_popular` owns. Deliberately not `bench_writer`:
/// the write benches delete that persona's statuses at the start of every run.
const POLL_VOTES: u64 = 400;
/// Tags whose usage is concentrated on the reseed day so they clear the trends
/// threshold (5 distinct accounts). One did before, so `api/trends_tags` read a
/// one-element array.
const HOT_TAGS: u64 = 45;
const WORKERS: u64 = 16;

const TOTAL_REMOTE_STATUSES: u64 = PROLIFIC_ACCOUNTS * PROLIFIC_STATUSES_EACH
    + MID_ACCOUNTS * MID_STATUSES_EACH
    + REGULAR_REMOTE_STATUSES;

/// The seeded fixture: the bench pool plus every id the scenarios target.
pub struct Seed {
    pub pool: PgPool,
    /// Persona following 10 prolific accounts (`bench_sparse`).
    pub sparse: i64,
    /// The everything-enabled persona (`bench_dense`, 1,500 follows).
    pub dense: i64,
    /// Persona the write benches post as (`bench_writer`), so their leftover
    /// rows never sit in the read personas' feeds. 5k followers.
    pub writer: i64,
    /// The many-followers persona (`bench_popular`).
    pub popular: i64,
    /// Root of the 100-reply thread (the context bench).
    pub thread_root: i64,
    /// A public remote status with media, card, tag and favourites.
    pub single_status: i64,
    /// A prolific remote account (1,300 statuses) for the account-statuses bench.
    pub prolific_account: i64,
    /// 20 account ids for the relationships bench (half followed by sparse).
    pub rel_ids: Vec<i64>,
    /// 20 status ids for the direct `render_statuses` bench.
    pub render_ids: Vec<i64>,
    /// The local group whose ranked timelines the benches read.
    pub group_account: i64,
    /// `bench_dense`'s 400-member list (the list-timeline bench).
    pub list_id: i64,
    /// A mid-tail remote acct ("user@domain") for the lookup bench.
    pub lookup_acct: String,
}

/// `SplitMix64`: a tiny deterministic RNG, avoiding a `rand` dependency the
/// workspace deliberately does not carry.
struct Rng(u64);

impl Rng {
    fn new(stream: u64) -> Self {
        Self(RNG_SEED ^ (stream.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const WORDS: [&str; 24] = [
    "meadow", "harbor", "signal", "quiet", "ember", "violet", "orbit", "linen", "cedar", "monsoon",
    "gallery", "lantern", "copper", "meridian", "sable", "thicket", "plume", "harvest", "cobalt",
    "drift", "saffron", "juniper", "tundra", "waltz",
];

fn sentence(rng: &mut Rng, marker: Option<&str>) -> String {
    let mut out = String::from("<p>");
    let words = 8 + rng.below(12);
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
    }
    if let Some(marker) = marker {
        out.push(' ');
        out.push_str(marker);
    }
    out.push_str("</p>");
    out
}

/// A title for a `Page` or an `Article`. Capped well under the 200-character
/// limit the composer enforces (`groups::TITLE_LENGTH_LIMIT`).
fn headline(rng: &mut Rng) -> String {
    let mut out = String::new();
    for i in 0..(3 + rng.below(5)) {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
    }
    out
}

/// A long-form body: several paragraphs, an order of magnitude past a Note.
/// The point is that the serializer, the sanitizer-free fold and the byte
/// budget see a body the size long-form actually produces — the Note bodies
/// average ~140 characters, and `max_characters_long_form` defaults to 50,000.
fn long_form(rng: &mut Rng, title: &str) -> String {
    let mut out = format!("<h1>{title}</h1>");
    for _ in 0..(6 + rng.below(6)) {
        out.push_str(&sentence(rng, None));
        out.push_str(&sentence(rng, None));
    }
    out
}

fn visibility(rng: &mut Rng) -> &'static str {
    match rng.below(1000) {
        0..=949 => "public",
        950..=996 => "unlisted",
        _ => "private",
    }
}

/// Staging's language mix: ~56% en, ~14% untagged, then de/ja/fr/es/sv/pl.
fn language(rng: &mut Rng) -> Option<&'static str> {
    match rng.below(100) {
        56..=69 => None,
        70..=78 => Some("de"),
        79..=82 => Some("ja"),
        83..=86 => Some("fr"),
        87..=88 => Some("es"),
        89..=90 => Some("sv"),
        91..=92 => Some("pl"),
        _ => Some("en"),
    }
}

fn past_timestamp(rng: &mut Rng) -> OffsetDateTime {
    OffsetDateTime::now_utc() - Duration::seconds(rng.below(180 * 86_400) as i64)
}

/// Zipf-ish domain assignment: one giant (9.3% of all accounts, the
/// mastodon.social analog), a handful of big domains, a mid tier, and a long
/// tail of ~4-account hosts — staging's measured distribution.
fn domain_index(account_index: u64) -> u64 {
    match account_index {
        i if i < 5_700 * SCALE => 0,
        i if i < 15_700 * SCALE => 1 + (i - 5_700 * SCALE) / (1_000 * SCALE),
        i if i < 29_000 * SCALE => 11 + (i - 15_700 * SCALE) / (133 * SCALE),
        i if i < 45_000 * SCALE => 111 + (i - 29_000 * SCALE) / (32 * SCALE),
        i => 611 + (i - 45_000 * SCALE) % (REMOTE_DOMAINS - 611),
    }
}

fn domain_of(account_index: u64) -> String {
    format!("d{}.bench.invalid", domain_index(account_index))
}

/// Opens (and if needed creates + seeds) the bench database.
pub async fn open() -> Seed {
    let base_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the dev Postgres");
    let bench_url = with_database(&base_url, BENCH_DB);
    let reseed = std::env::var("PLAMENU_BENCH_RESEED").is_ok_and(|v| v == "1");

    ensure_database(&base_url, reseed).await;
    // The bench database is a cache, so a schema it cannot be migrated onto —
    // a migration added, or amended before it was released — is a reason to
    // rebuild it, not to fail the run with sqlx's raw `VersionMismatch`.
    let mut pool = match plamenu_db::connect_and_migrate(&bench_url, 16).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("rebuilding {BENCH_DB}: {err}");
            ensure_database(&base_url, true).await;
            plamenu_db::connect_and_migrate(&bench_url, 16)
                .await
                .expect("migrate plamenu_bench")
        }
    };
    ensure_marker_table(&pool).await;

    let version = marker_value(&pool, "version").await;
    if version.as_deref() == Some(SEED_VERSION) {
        analyze_new_migrations(&pool).await;
    } else {
        if version.is_some() {
            // Stale dataset from an older seed shape: rebuild from scratch.
            pool.close().await;
            ensure_database(&base_url, true).await;
            pool = plamenu_db::connect_and_migrate(&bench_url, 16)
                .await
                .expect("migrate plamenu_bench");
            ensure_marker_table(&pool).await;
        }
        eprintln!("seeding {BENCH_DB} (version {SEED_VERSION}) — one-time, several minutes");
        let started = std::time::Instant::now();
        seed(&pool).await;
        eprintln!("seeded {BENCH_DB} in {:.0?}", started.elapsed());
    }
    load(pool).await
}

/// Re-`ANALYZE`s after migrations have been applied to an existing dataset.
///
/// The seed's own `ANALYZE` only runs on the path that builds the database.
/// Every other run takes the reuse path, where `connect_and_migrate` may have
/// just added columns, indexes or whole tables that no autoanalyze has visited
/// — and a bulk-loaded table with no `pg_stats` row gets absurd plans (measured
/// once at seed time: home timeline 2.8 s stale vs 65 ms analyzed). Comparing
/// the applied-migration count against a marker is enough to notice, and costs
/// one scalar query on the runs where nothing changed.
async fn analyze_new_migrations(pool: &PgPool) {
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .expect("count applied migrations");
    let known: i64 = marker_value(pool, "migrations")
        .await
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if applied <= known {
        return;
    }
    eprintln!("analyzing {BENCH_DB}: {} new migration(s)", applied - known);
    let started = std::time::Instant::now();
    sqlx::raw_sql("ANALYZE").execute(pool).await.unwrap();
    set_marker(pool, "migrations", &applied.to_string()).await;
    eprintln!("analyzed {BENCH_DB} in {:.0?}", started.elapsed());
}

/// Swaps the database name in a postgres URL.
fn with_database(base_url: &str, database: &str) -> String {
    let mut url = url::Url::parse(base_url).expect("DATABASE_URL parses");
    url.set_path(database);
    url.to_string()
}

async fn ensure_database(base_url: &str, drop_first: bool) {
    let mut conn = PgConnection::connect(base_url)
        .await
        .expect("connect to the dev Postgres (docker compose -f compose.dev.yml up -d)");
    if drop_first {
        sqlx::raw_sql("DROP DATABASE IF EXISTS plamenu_bench WITH (FORCE)")
            .execute(&mut conn)
            .await
            .expect("drop bench database");
    }
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(BENCH_DB)
        .fetch_optional(&mut conn)
        .await
        .expect("check bench database");
    if exists.is_none() {
        sqlx::raw_sql("CREATE DATABASE plamenu_bench")
            .execute(&mut conn)
            .await
            .expect("create bench database");
    }
    conn.close().await.ok();
}

async fn ensure_marker_table(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS bench_seed (key text PRIMARY KEY, value text NOT NULL)",
    )
    .execute(pool)
    .await
    .expect("create bench_seed marker table");
}

async fn marker_value(pool: &PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM bench_seed WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .expect("read bench_seed marker")
}

async fn set_marker(pool: &PgPool, key: &str, value: &str) {
    sqlx::query("INSERT INTO bench_seed (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await
        .expect("write bench_seed marker");
}

async fn marker_id(pool: &PgPool, key: &str) -> i64 {
    marker_value(pool, key)
        .await
        .unwrap_or_else(|| panic!("bench_seed marker {key} missing"))
        .parse()
        .expect("bench_seed marker is an id")
}

async fn marker_ids(pool: &PgPool, key: &str) -> Vec<i64> {
    marker_value(pool, key)
        .await
        .unwrap_or_else(|| panic!("bench_seed marker {key} missing"))
        .split(',')
        .map(|part| part.parse().expect("bench_seed marker is an id list"))
        .collect()
}

async fn load(pool: PgPool) -> Seed {
    Seed {
        sparse: marker_id(&pool, "sparse").await,
        dense: marker_id(&pool, "dense").await,
        writer: marker_id(&pool, "writer").await,
        popular: marker_id(&pool, "popular").await,
        thread_root: marker_id(&pool, "thread_root").await,
        single_status: marker_id(&pool, "single_status").await,
        prolific_account: marker_id(&pool, "prolific_account").await,
        rel_ids: marker_ids(&pool, "rel_ids").await,
        render_ids: marker_ids(&pool, "render_ids").await,
        group_account: marker_id(&pool, "group_account").await,
        list_id: marker_id(&pool, "list_id").await,
        lookup_acct: marker_value(&pool, "lookup_acct")
            .await
            .expect("bench_seed marker lookup_acct missing"),
        pool,
    }
}

/// Everything a benchmark run writes, deleted again.
///
/// `bench/README.md` has always claimed the dataset is identical at the start
/// of every run. It was not: the old cleanup deleted the two write personas'
/// statuses, favourites and notifications and nothing else, so three runs under
/// the wave-1 harness left 7,669 orphan `conversations`, 647 `link_crawl_jobs`,
/// 24 OAuth apps, 57 tokens and 244,279 `delivery_jobs` behind — monotonically,
/// forever. Both drivers call this, so there is one definition of what a run
/// writes rather than two that drift apart, and [`assert_dataset_unchanged`]
/// checks the claim instead of restating it.
pub async fn reset_run_residue(pool: &PgPool, writer: i64, popular: i64) {
    // The write benches post and boost as `writer`; deleting a status cascades
    // its mentions, media, edits and conversation membership.
    for sql in [
        "DELETE FROM statuses WHERE account_id = $1",
        "DELETE FROM favourites WHERE account_id = $1",
        "DELETE FROM notifications WHERE from_account_id = $1",
        // `write/post_delete` soft-deletes planted statuses through the real
        // endpoint, and a delete records a tombstone that deliberately
        // *outlives* the row it describes. The statuses go with the line
        // above; these do not, and 400 of them a run is drift.
        // SEED-SHAPE-UNCHANGED: run reset, not dataset.
        "DELETE FROM status_tombstones WHERE account_id = $1",
    ] {
        sqlx::query(sql)
            .bind(writer)
            .execute(pool)
            .await
            .expect("delete writer rows");
    }

    // The persistent seed predates normalized federation keys. The hot-path
    // driver provisions keys for the two local signing personas and the
    // instance actor so its measurements use the production path; those six
    // rows are run fixtures just like its OAuth app and queues. Keep this in
    // the shared reset so the contention driver can follow a hot-path run
    // without seeing dataset drift.
    sqlx::query(
        "DELETE FROM actor_keys
         WHERE owner_kind = 'instance' OR account_id = ANY($1)",
    )
    .bind(vec![writer, popular])
    .execute(pool)
    .await
    .expect("delete benchmark normalized keys");

    for sql in [
        // The ingest benches' remote peer, whose keypair is minted fresh every
        // process anyway. Deleting the account cascades its statuses (and with
        // them the 256 pre-ingested edit-bench notes), favourites,
        // notifications and mentions in one statement.
        "DELETE FROM accounts WHERE domain = 'bench-peer.invalid'",
        "DELETE FROM host_signature_prefs WHERE host = 'bench-peer.invalid'",
        "DELETE FROM host_reachability WHERE host = 'bench-peer.invalid'",
        // `db_jobs/purge_account` plants throwaway accounts and consumes them
        // one per iteration; a cancelled run leaves the rest, and their statuses
        // and usage rows go with them. SEED-SHAPE-UNCHANGED: this is the run
        // reset, not the dataset — nothing here seeds a row.
        "DELETE FROM accounts WHERE domain = 'bench-purge.invalid'",
        // The three-arm search corpus (`plant_search_corpus`): the accounts
        // take their statuses with them, the tags carry no usage rows and so
        // no dependants. SEED-SHAPE-UNCHANGED: run reset, not dataset.
        "DELETE FROM accounts WHERE domain = 'bench-search.invalid'",
        "DELETE FROM tags WHERE name LIKE 'benchsearch%'",
        // The throwaway local account `db_jobs/statuses_cleanup_sweep` deletes
        // for, which takes its planted statuses, its cleanup policy and its
        // tombstones with it.
        "DELETE FROM accounts WHERE username = 'bench_cleanup' AND domain IS NULL",
        // Reachability learned from the stubbed delivery drain.
        "DELETE FROM host_reachability WHERE host LIKE '%.bench-deliver.invalid'",
        "DELETE FROM host_signature_prefs WHERE host LIKE '%.bench-deliver.invalid'",
        // Queues. No worker runs during a bench, and the seed fills none of
        // these, so whatever is in them is this run's residue. `link_crawl_jobs`
        // grows on every post and every ingest because `enqueue_crawl` does not
        // check whether the content carries a link at all (C5).
        "DELETE FROM delivery_jobs",
        "DELETE FROM link_crawl_jobs",
        "DELETE FROM media_cleanup_jobs",
        "DELETE FROM quote_verify_jobs",
        "DELETE FROM reply_fetch_jobs",
        // Both drivers mint an OAuth app and tokens per run, and the web UI
        // creates its own built-in app the first time a page is rendered
        // (`ensure_web_app`) — which is a run artifact here even though it is
        // instance state in production, because the seed does not build it.
        "DELETE FROM oauth_tokens WHERE app_id IN \
         (SELECT id FROM oauth_apps
          WHERE name IN ('bench', 'contention-bench') OR client_id = 'plamenu-web-ui')",
        "DELETE FROM oauth_apps
         WHERE name IN ('bench', 'contention-bench') OR client_id = 'plamenu-web-ui'",
        // Ranked by the setup's trends refresh, not by the seed. The peak
        // columns on `tags` go with them: the ranker only writes a peak when
        // the score beats the stored one, so leaving them behind would make
        // every run after the first do measurably less work than the first.
        "DELETE FROM tag_trends",
        "DELETE FROM status_trends",
        "DELETE FROM preview_card_trends",
        "UPDATE tags SET max_score = NULL, max_score_at = NULL
         WHERE max_score IS NOT NULL OR max_score_at IS NOT NULL",
        // A day-keyed counter the write benches bump; a new day is a new row.
        "DELETE FROM daily_interactions",
        // The contention driver's credential-burst scenario signs in for real,
        // and every successful sign-in files a login activity. The seed files
        // none. SEED-SHAPE-UNCHANGED: run reset, not dataset.
        "DELETE FROM login_activities",
        // `/api/v3` aliases and post-read rows are durable production state,
        // but the direct DB fixtures first expose/mark seeded statuses on every
        // benchmark run. The seed itself contains neither relation.
        // SEED-SHAPE-UNCHANGED: run reset, not dataset.
        "DELETE FROM post_reads",
        "DELETE FROM lemmy_notification_read_overrides",
        "DELETE FROM lemmy_custom_emoji_metadata",
        "DELETE FROM lemmy_media_uploads",
        "DELETE FROM lemmy_id_aliases",
        // `conversations.root_status_id` is ON DELETE SET NULL with no index,
        // so every deleted thread root leaves a row behind — on the exact table
        // whose unindexed FK is a live product defect (C2). Last, so it also
        // collects the roots the deletes above just orphaned. The membership
        // test is what keeps it from eating the seeded *placeholder*
        // conversations, which are legitimately root-less: an orphan reply's
        // conversation has no root yet but does have the reply in it, while a
        // residue row has had its members cascaded away.
        "DELETE FROM conversations c
         WHERE c.root_status_id IS NULL
           AND NOT EXISTS (SELECT 1 FROM status_conversations sc
                           WHERE sc.conversation_id = c.id)",
    ] {
        sqlx::raw_sql(sql)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("cleanup `{sql}`: {err}"));
    }
}

/// The instant the trends refresh should treat as "now".
///
/// The day the seed concentrated tag usage on, at midday — not the wall clock.
/// The job scores usage inside one day, so against the wall clock it ranked
/// ~15,000 candidates on the day the dataset was built and exactly one on every
/// day after, while the same budget printed "ok" either way. Anchoring it to the
/// data is what lets the benchmark measure the same job whenever it runs.
pub async fn trends_reference(pool: &PgPool) -> OffsetDateTime {
    let day = match marker_value(pool, "trends_day").await {
        Some(value) => {
            time::Date::parse(&value, &time::format_description::well_known::Iso8601::DATE)
                .expect("bench_seed marker trends_day is a date")
        }
        // A database seeded before the marker existed: the newest usage is the
        // best available guess.
        None => sqlx::query_scalar::<_, Option<time::Date>>("SELECT max(day) FROM tag_usages")
            .fetch_one(pool)
            .await
            .unwrap()
            .unwrap_or_else(|| OffsetDateTime::now_utc().date()),
    };
    day.with_hms(12, 0, 0)
        .expect("midday is a valid time")
        .assume_utc()
}

/// Fails the run when the dataset is present but the wrong *shape*.
///
/// The pre-pass in `main.rs` already asserts that every read case returned a
/// plausible body; this asserts the rows a scenario depends on exist at the
/// volume it was calibrated against. The two are not the same check: a case can
/// return a fat body while measuring something else entirely — which is exactly
/// what happened when 80 `with_replies = false` follows were added to the dense
/// persona and `SEED_VERSION` was not bumped, so the live database carried 13
/// and the arm ran at 14% of its intended volume for thirteen hours (B1).
///
/// Each check names the benchmark that stops meaning anything without it.
pub async fn verify(pool: &PgPool) {
    let scalar = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|err| panic!("seed::verify `{sql}`: {err}"))
    };

    let mut problems: Vec<String> = Vec::new();
    let mut want = |name: &str, got: i64, floor: i64, why: &str| {
        if got < floor {
            problems.push(format!("{name}: {got}, want >= {floor} — {why}"));
        }
    };

    want(
        "dense follows with replies off",
        scalar(
            "SELECT count(*) FROM follows f
             JOIN accounts a ON a.id = f.account_id
             WHERE a.username = 'bench_dense' AND NOT f.with_replies",
        )
        .await,
        50,
        "api/home_dense stops exercising the per-row reply filter",
    );
    want(
        "events with a sidecar",
        scalar(
            "SELECT count(*) FROM statuses s
             JOIN status_events e ON e.status_id = s.id
             WHERE s.object_type = 'Event'",
        )
        .await,
        200,
        "entities::event_map short-circuits and the whole sidecar batch is unmeasured",
    );
    want(
        "events on the group's first page",
        scalar(
            "SELECT count(*) FROM (
                 SELECT COALESCE(b.reblog_of_id, b.id) AS id
                 FROM statuses b
                 JOIN accounts g ON g.id = b.account_id
                 WHERE g.username = 'bench_group'
                 ORDER BY b.sort_at DESC, b.id DESC LIMIT 20
             ) page
             JOIN statuses s ON s.id = page.id
             WHERE s.object_type = 'Event'",
        )
        .await,
        1,
        "api/group_* is the only benched page dense enough for the event batch \
         to be measured rather than merely present",
    );
    want(
        "trending tags",
        scalar("SELECT count(*) FROM tag_trends").await,
        20,
        "api/trends_tags renders a near-empty array (run the trends refresh first)",
    );
    want(
        "group posts carrying a preview card",
        scalar(
            "SELECT count(*) FROM preview_cards_statuses pcs
             JOIN status_mentions m ON m.status_id = pcs.status_id
             JOIN accounts g ON g.id = m.account_id
             WHERE g.username = 'bench_group'",
        )
        .await,
        100,
        "the three group timelines measure the cheapest possible serialization (B5)",
    );
    want(
        "groupable notifications in dense's newest 40",
        scalar(
            "SELECT count(*) FROM (
                 SELECT n.kind FROM notifications n
                 JOIN accounts a ON a.id = n.account_id
                 WHERE a.username = 'bench_dense' AND NOT n.filtered
                 ORDER BY n.id DESC LIMIT 40
             ) page
             WHERE kind IN ('favourite', 'reblog', 'follow')",
        )
        .await,
        30,
        "api/notifications_grouped measures 40 singleton groups (B8)",
    );
    want(
        "soft-deleted stubs",
        scalar("SELECT count(*) FROM statuses WHERE deleted_at IS NOT NULL").await,
        400,
        "every -- STUBFILTER predicate filters nothing",
    );
    want(
        "orphan replies",
        scalar(
            "SELECT count(*) FROM statuses
             WHERE in_reply_to_id IS NULL AND in_reply_to_uri IS NOT NULL",
        )
        .await,
        2_000,
        "status::unresolved_reply_parents returns empty on every page",
    );
    want(
        "attachments with a rendition ladder",
        scalar("SELECT count(DISTINCT media_id) FROM media_renditions").await,
        1_000,
        "media_json_hls' hls_native branch and the whole ladder are dead",
    );
    want(
        "cached attachments with a file",
        scalar(
            "SELECT count(*) FROM media_attachments
             WHERE processing = 'complete' AND file_name IS NOT NULL",
        )
        .await,
        300,
        "the serializer's local-file and preview arms are dead",
    );
    want(
        "accepted relays",
        scalar("SELECT count(*) FROM relays WHERE state = 'accepted'").await,
        1,
        "relay::enabled_inboxes returns nothing on every public fan-out",
    );
    want(
        "collections tagged onto statuses",
        scalar("SELECT count(*) FROM tagged_objects").await,
        50,
        "entities::tagged_collections_map never enters its per-collection loop",
    );
    want(
        "articles",
        scalar("SELECT count(*) FROM statuses WHERE object_type = 'Article'").await,
        200,
        "fold_typed_content early-returns on every row",
    );
    want(
        "poll votes",
        scalar("SELECT count(*) FROM poll_votes").await,
        300,
        "poll_map's own_votes is empty for every viewer",
    );
    want(
        "local custom emojis",
        scalar("SELECT count(*) FROM custom_emojis WHERE domain IS NULL").await,
        500,
        "emoji_map's domain IS NULL arm is never taken",
    );

    // The harness notes are re-ingested every run and used to be the newest
    // public rows in the database, so two of the 47 medians paged them instead
    // of the dataset (B6).
    //
    // Matched on `bench-%` rather than one named domain. The narrow version
    // missed it the moment a second harness domain appeared: `bench-purge`'s
    // planted statuses were public and stamped `now()`, 1,500 of them landed on
    // the first page, and the only symptom was `api/public_federated` quietly
    // shedding half its round trips. The dataset's own remote domains are
    // `d<N>.bench.invalid`, so the prefix separates the harness from the data.
    let harness_at_head: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT s.account_id FROM statuses s
             WHERE s.visibility = 'public' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
             ORDER BY s.sort_at DESC, s.id DESC LIMIT 20
         ) page
         JOIN accounts a ON a.id = page.account_id
         WHERE a.domain LIKE 'bench-%'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    if harness_at_head > 5 {
        problems.push(format!(
            "harness notes at the head of the public timeline: {harness_at_head} of the \
             newest 20 — api/public_federated{{,_anon}} are paging the bench's own \
             setup instead of the dataset (B6)"
        ));
    }

    // The same mistake one table over. A harness account's tag usage must sit
    // outside the window the trends refresh scores, or the planted rows decide
    // what is trending: the purge bench's first version dated 12,000 of them
    // today, which took eight real tags past the five-distinct-accounts
    // threshold and moved `api/trends_tags` by 2.2x with the endpoint untouched.
    let harness_recent_usage: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tag_usages tu
         JOIN accounts a ON a.id = tu.account_id
         WHERE a.domain LIKE 'bench-%' AND tu.day > current_date - 90",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    if harness_recent_usage > 0 {
        problems.push(format!(
            "{harness_recent_usage} harness tag_usages rows inside the trending window \
             — api/trends_tags and db_jobs/trends_refresh_tags are scoring the bench's \
             own filler; date planted usage well outside it"
        ));
    }

    assert!(
        problems.is_empty(),
        "the bench dataset is the wrong shape; reseed with PLAMENU_BENCH_RESEED=1 \
         after fixing seed.rs:\n  {}",
        problems.join("\n  ")
    );
}

/// Fails the run when the dataset it is about to measure is not the dataset
/// every earlier run measured.
///
/// Recorded once, when the database is built. Checked at the start of every run
/// *after* [`reset_run_residue`], so a run fails on the previous run's residue —
/// naming the table and the drift — instead of quietly measuring a different
/// database and reporting the difference as a code change.
pub async fn assert_dataset_unchanged(pool: &PgPool) {
    let Some(recorded) = marker_value(pool, "table_counts").await else {
        // A database seeded before this check existed. Recording now would
        // bless whatever residue it has accumulated, so say so and let the next
        // reseed establish the baseline honestly.
        eprintln!(
            "no dataset baseline recorded — drift checking starts at the next \
             PLAMENU_BENCH_RESEED=1"
        );
        return;
    };
    let counts = table_counts(pool).await;
    let baseline: std::collections::BTreeMap<&str, i64> = recorded
        .split(',')
        .filter_map(|entry| entry.split_once('='))
        .filter_map(|(table, count)| count.parse().ok().map(|count| (table, count)))
        .collect();
    // A table absent from the baseline was added by a migration since the
    // dataset was built, so its expected count is zero, not "unknown".
    let drift: Vec<String> = counts
        .iter()
        .map(|(table, count)| {
            (
                table,
                baseline.get(table.as_str()).copied().unwrap_or(0),
                count,
            )
        })
        .filter(|(_, was, count)| was != *count)
        .map(|(table, was, count)| format!("{table}: {was} -> {count}"))
        .collect();
    assert!(
        drift.is_empty(),
        "the bench dataset has drifted from the shape it was seeded with, so this \
         run is not comparable to any other:\n  {}\nEither a bench wrote rows \
         `reset_run_residue` does not delete (add them there), or a migration \
         changed the data (reseed with PLAMENU_BENCH_RESEED=1).",
        drift.join("\n  ")
    );
}

/// Records the row-count fingerprint [`assert_dataset_unchanged`] compares
/// against. Called at the end of seeding, where the dataset is pristine by
/// construction.
async fn record_dataset_baseline(pool: &PgPool) {
    let fingerprint = table_counts(pool)
        .await
        .iter()
        .map(|(table, count)| format!("{table}={count}"))
        .collect::<Vec<_>>()
        .join(",");
    set_marker(pool, "table_counts", &fingerprint).await;
}

/// Every table's exact row count, sorted by name.
///
/// `pg_stat_user_tables` supplies the table list and `query_to_xml` runs the
/// counts, so all 128 tables cost one round trip — 0.3 s over the whole bench
/// database, against ~4 minutes of run. `bench_seed` is excluded because this
/// check writes to it, and `_sqlx_migrations` because P14 already watches it.
async fn table_counts(pool: &PgPool) -> Vec<(String, i64)> {
    sqlx::query_as(
        r"
        SELECT relname::text,
               (xpath('/row/c/text()',
                      query_to_xml(format('SELECT count(*) AS c FROM %I.%I',
                                          schemaname, relname),
                                   false, true, '')))[1]::text::bigint
        FROM pg_stat_user_tables
        WHERE schemaname = 'public'
          AND relname NOT IN ('bench_seed', '_sqlx_migrations')
        ORDER BY relname
        ",
    )
    .fetch_all(pool)
    .await
    .expect("count the bench tables")
}

fn join_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Runs `make(worker_index)` for each worker concurrently, returning results
/// in worker order (so downstream selections stay deterministic).
async fn fan_out<T, F, Fut>(workers: u64, make: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(u64) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for worker in 0..workers {
        let task = make(worker);
        set.spawn(async move { (worker, task.await) });
    }
    let mut results: Vec<(u64, T)> = Vec::with_capacity(workers as usize);
    while let Some(joined) = set.join_next().await {
        results.push(joined.expect("seed worker panicked"));
    }
    results.sort_by_key(|(worker, _)| *worker);
    results.into_iter().map(|(_, value)| value).collect()
}

async fn seed(pool: &PgPool) {
    let step = |name: &'static str| {
        let t = std::time::Instant::now();
        eprintln!("  seeding: {name}");
        move || eprintln!("  seeded:  {name} in {:.0?}", t.elapsed())
    };

    configure_instance_settings(pool).await;

    // Real keys are irrelevant to read benches; reusing a small pool of
    // keypairs keeps RSA keygen from dominating the seed.
    let keys: Arc<Vec<KeyPairPem>> =
        Arc::new((0..10).map(|_| keys::generate_keypair().unwrap()).collect());

    let done = step("remote accounts");
    let remotes = Arc::new(seed_remote_accounts(pool, &keys).await);
    done();
    let done = step("custom emojis");
    seed_custom_emojis(pool).await;
    seed_local_emojis(pool).await;
    done();
    let done = step("local accounts");
    let locals = Arc::new(seed_local_accounts(pool, &keys).await);
    done();
    let personas = seed_personas(pool).await;

    // --- every status is created before any enrichment pass runs ------------
    // The group, the deep thread and the personas' own posts used to be seeded
    // *after* the media/card/tag passes, which walk `statuses.all` and the
    // `statuses` table. They therefore carried no media, no cards, no tags and
    // no titles, and `api/context_deep`, the three group timelines,
    // `web/web_profile` and `ap/outbox_page` all measured the cheapest possible
    // serialization (B5).
    let done = step("statuses");
    let mut statuses = seed_statuses(pool, &remotes, &locals, &personas).await;
    done();
    let done = step("persona statuses + group + thread");
    let own_statuses = seed_persona_statuses(pool, &personas, &mut statuses).await;
    let group = seed_group(pool, &remotes, &locals, &mut statuses).await;
    let thread_root = seed_thread(pool, &remotes, &locals, &mut statuses).await;
    seed_events(pool, &remotes, &locals, &personas, &mut statuses).await;
    seed_orphan_replies(pool, &remotes, &mut statuses).await;
    done();

    // Ordinals over `statuses.all`, so every set-based enrichment below keys on
    // a *stable* number instead of `hashint8(id)`. Snowflake ids are minted
    // from the wall clock by concurrent workers, so id-derived selections drew
    // a different slice on every reseed — tag membership most visibly, which
    // changed the media and card density of the tag timeline at no code change
    // (B4). `statuses.all` is built in worker order, so ordinal N is the same
    // logical row every time.
    let done = step("status ordinals");
    let statuses = Arc::new(statuses);
    build_status_ordinals(pool, &statuses).await;
    done();

    // Local statuses take `created_at`/`sort_at` from the column default, so
    // all ~15k of them landed inside one 68-second window at the head of every
    // timeline (B6). Backdating them over the same 180-day window the remote
    // ones already use has to happen here: `status_tags.sort_at`,
    // `tag_usages.day`, `preview_card_usages.day`, `conversations.created_at`
    // and `status_edits.created_at` are all derived from these columns below.
    let done = step("backdate local statuses");
    backdate_local_statuses(pool).await;
    backdate_thread(pool, thread_root).await;
    backdate_group_posts(pool, group.account).await;
    done();

    seed_popular_profile(pool, personas.popular, &own_statuses.popular).await;
    let done = step("tags");
    let benchtag = seed_tags(pool).await;
    done();
    let done = step("status_tags + usages (set-based)");
    seed_status_tags(pool, benchtag).await;
    done();
    let done = step("media");
    seed_media(pool, &statuses).await;
    done();
    let done = step("preview cards");
    seed_preview_cards(pool, &statuses).await;
    done();
    let done = step("polls/quotes/edits/reactions");
    let poll_ids = seed_polls(pool, &statuses).await;
    seed_poll_votes(pool, &remotes, &personas, &poll_ids).await;
    seed_quotes(pool).await;
    seed_edits(pool).await;
    seed_reactions(pool, &remotes, &statuses).await;
    done();
    let done = step("boosts");
    seed_boosts(pool, &remotes, &locals, &statuses).await;
    backdate_local_reblogs(pool).await;
    done();
    let done = step("engagement");
    let single_status = seed_engagement(pool, &remotes, &statuses).await;
    done();
    let done = step("translations + stubs + collections + relays");
    seed_translations(pool).await;
    seed_stubs(pool).await;
    seed_collections(pool, &remotes, &locals, &personas, group.account).await;
    seed_relays(pool).await;
    done();
    let done = step("conversations (set-based)");
    seed_conversations(pool).await;
    done();
    let done = step("graph + personas");
    let graph = seed_graph(pool, &remotes, &locals, &statuses, &personas).await;
    seed_moderation(pool, &remotes).await;
    done();
    let done = step("dms + notifications");
    let dm_ids = seed_dms(pool, &remotes, personas.sparse, personas.dense).await;
    seed_notifications(pool, &remotes, &statuses, &own_statuses, &personas, &graph).await;
    seed_markers(pool, &personas, &own_statuses).await;
    done();
    seed_fetch_failures(pool).await;
    drop_status_ordinals(pool).await;
    let group_account = group.account;
    let list_id = graph.list_id;

    let rel_ids: Vec<i64> = remotes[..10]
        .iter()
        .chain(&remotes[1_000..1_010])
        .copied()
        .collect();
    let mut render_ids: Vec<i64> = statuses
        .all
        .iter()
        .step_by(statuses.all.len() / 16)
        .map(|&(id, _)| id)
        .take(16)
        .collect();
    render_ids.push(thread_root);
    render_ids.push(single_status);
    render_ids.push(own_statuses.sparse[0]);
    render_ids.push(dm_ids[0]);

    // A mid-tail account for the webfinger-style lookup bench.
    let lookup_index: u64 = REMOTE_ACCOUNTS / 6 * 5;
    let lookup_acct = format!("u{lookup_index}@{}", domain_of(lookup_index));

    set_marker(pool, "sparse", &personas.sparse.to_string()).await;
    set_marker(pool, "dense", &personas.dense.to_string()).await;
    set_marker(pool, "writer", &personas.writer.to_string()).await;
    set_marker(pool, "popular", &personas.popular.to_string()).await;
    set_marker(pool, "thread_root", &thread_root.to_string()).await;
    set_marker(pool, "single_status", &single_status.to_string()).await;
    set_marker(pool, "prolific_account", &remotes[0].to_string()).await;
    set_marker(pool, "rel_ids", &join_ids(&rel_ids)).await;
    set_marker(pool, "render_ids", &join_ids(&render_ids)).await;
    set_marker(pool, "group_account", &group_account.to_string()).await;
    set_marker(pool, "list_id", &list_id.to_string()).await;
    set_marker(pool, "lookup_acct", &lookup_acct).await;

    // Fresh bulk-loaded tables have no planner statistics until autoanalyze
    // catches up — without this the first benches run on absurd plans
    // (measured: home timeline 2.8s stale vs 65ms analyzed).
    let done = step("analyze");
    sqlx::raw_sql("ANALYZE").execute(pool).await.unwrap();
    done();

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .unwrap();
    set_marker(pool, "migrations", &applied.to_string()).await;
    record_dataset_baseline(pool).await;
    set_marker(pool, "version", SEED_VERSION).await;
}

/// Benches hammer single tokens far past the API buckets; measurements must
/// not include 429s. Tags default to trendable so the trends refresh + API
/// benches rank real rows (the column default is false). Local/federated
/// timeline previews are enabled so the anonymous public-timeline benches
/// exercise the read path instead of 401ing on the anon-preview gate.
async fn configure_instance_settings(pool: &PgPool) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        instance_settings::SettingsUpdate {
            rate_limiting_enabled: false,
            trendable_by_default: true,
            timeline_preview_local: true,
            timeline_preview_federated: true,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
}

async fn seed_remote_accounts(pool: &PgPool, keys: &Arc<Vec<KeyPairPem>>) -> Vec<i64> {
    let chunk = REMOTE_ACCOUNTS.div_ceil(WORKERS);
    let ids = fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let keys = Arc::clone(keys);
        async move {
            let mut ids = Vec::new();
            let end = ((worker + 1) * chunk).min(REMOTE_ACCOUNTS);
            for i in (worker * chunk)..end {
                let mut rng = Rng::new(1_000 + i);
                let domain = domain_of(i);
                // A slice of accounts matches the search bench's query term.
                let username = if i % 240 == 0 {
                    format!("zephyr{i}")
                } else {
                    format!("u{i}")
                };
                let uri = format!("https://{domain}/users/{username}");
                let key = &keys[(i % keys.len() as u64) as usize];
                let note = sentence(&mut rng, None);
                // Profile fields on ~60% (staging: ~1.6 rows/account) and
                // avatars on most — the account render includes both.
                let fields = if rng.chance(60) {
                    (0..=rng.below(2))
                        .map(|f| FieldPair {
                            name: format!("field{f}"),
                            value: WORDS[rng.below(24) as usize].to_owned(),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let avatar = format!("https://{domain}/media/avatars/{i}.png");
                let header = format!("https://{domain}/media/headers/{i}.png");
                // Real account ages, so a Filter-new-accounts notification
                // policy doesn't classify the whole fleet as new.
                let created_at = OffsetDateTime::now_utc()
                    - Duration::seconds((30 + rng.below(1_000)) as i64 * 86_400);
                let account = account::upsert_remote(
                    &pool,
                    RemoteAccountData {
                        username: &username,
                        domain: &domain,
                        uri: &uri,
                        display_name: &format!("Bench {username}"),
                        note: &note,
                        inbox_url: &format!("{uri}/inbox"),
                        shared_inbox_url: &format!("https://{domain}/inbox"),
                        public_key_pem: &key.public_pem,
                        public_key_id: &format!("{uri}#main-key"),
                        avatar_remote_url: rng.chance(80).then_some(avatar.as_str()),
                        header_remote_url: rng.chance(40).then_some(header.as_str()),
                        avatar_description: "",
                        header_description: "",
                        created_at: Some(created_at),
                        fields,
                        featured_collection_url: None,
                        // A slice of locked accounts for pending-follow edges.
                        locked: i % 617 == 0,
                        also_known_as: &[],
                        moved_to_uri: None,
                        url: Some(&format!("https://{domain}/@{username}")),
                        discoverable: true,
                        feature_approval_policy: 0,
                        is_bot: i % 97 == 0,
                        indexable: true,
                        show_media: None,
                        show_media_replies: None,
                        show_featured: None,
                        memorial: false,
                        actor_type: Some("Person"),
                    },
                )
                .await
                .unwrap();
                ids.push(account.id);
            }
            ids
        }
    })
    .await;
    ids.into_iter().flatten().collect()
}

/// ~50k remote custom emojis over the first [`EMOJI_DOMAINS`] domains
/// (staging: 16.6k over 1,367 domains). Shortcodes repeat across domains —
/// `custom_emoji::lookup` is keyed (shortcode, domain), exactly like real
/// federated emoji.
async fn seed_custom_emojis(pool: &PgPool) {
    let chunk = EMOJI_DOMAINS.div_ceil(WORKERS);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        async move {
            let end = ((worker + 1) * chunk).min(EMOJI_DOMAINS);
            for d in (worker * chunk)..end {
                let domain = format!("d{d}.bench.invalid");
                let count = 4 + (d * 7) % 17;
                for j in 0..count {
                    custom_emoji::upsert_remote(
                        &pool,
                        RemoteEmojiData {
                            shortcode: &format!("bemoji{j}"),
                            domain: &domain,
                            uri: Some(&format!("https://{domain}/emojis/{j}")),
                            image_remote_url: &format!("https://{domain}/emojis/{j}.png"),
                            updated: None,
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        }
    })
    .await;
}

async fn seed_local_accounts(pool: &PgPool, keys: &Arc<Vec<KeyPairPem>>) -> Vec<i64> {
    let chunk = LOCAL_ACCOUNTS.div_ceil(WORKERS);
    let ids = fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let keys = Arc::clone(keys);
        async move {
            let mut ids = Vec::new();
            let end = ((worker + 1) * chunk).min(LOCAL_ACCOUNTS);
            for i in (worker * chunk)..end {
                let mut rng = Rng::new(2_000 + i);
                let key = &keys[(i % keys.len() as u64) as usize];
                let note = sentence(&mut rng, None);
                let account = account::create_local(
                    &pool,
                    NewLocalAccount {
                        username: &format!("bench_local{i}"),
                        display_name: &format!("Bench Local {i}"),
                        note: &note,
                        public_key_pem: &key.public_pem,
                    },
                )
                .await
                .unwrap();
                ids.push(account.id);
            }
            ids
        }
    })
    .await;
    ids.into_iter().flatten().collect()
}

struct Personas {
    sparse: i64,
    dense: i64,
    writer: i64,
    popular: i64,
}

async fn seed_personas(pool: &PgPool) -> Personas {
    let password_hash = plamenu::auth::hash_password("bench-password").unwrap();
    let mut ids = [0_i64; 4];
    for (slot, username) in [
        "bench_sparse",
        "bench_dense",
        "bench_writer",
        "bench_popular",
    ]
    .into_iter()
    .enumerate()
    {
        let keypair = keys::generate_keypair().unwrap();
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: username,
                note: "bench persona",
                public_key_pem: &keypair.public_pem,
            },
        )
        .await
        .unwrap();
        user::create(pool, account.id, None, &password_hash)
            .await
            .unwrap();
        ids[slot] = account.id;
    }
    let personas = Personas {
        sparse: ids[0],
        dense: ids[1],
        writer: ids[2],
        popular: ids[3],
    };

    // The dense persona reads with a language preference, like a real
    // multilingual-instance user (thins page fill, adds a predicate).
    let dense_user = user::find_by_account_id(pool, personas.dense)
        .await
        .unwrap()
        .unwrap();
    user::update_chosen_languages(
        pool,
        dense_user.id,
        Some(&["en".to_owned(), "de".to_owned()]),
    )
    .await
    .unwrap();

    // The popular persona has a filled-out profile (fields render on the
    // profile page and AP actor).
    account::update_local_profile(
        pool,
        personas.popular,
        ProfileUpdate {
            fields: Some(
                (0..4)
                    .map(|f| FieldPair {
                        name: format!("Link {f}"),
                        value: "https://example.com/".to_owned(),
                    })
                    .collect(),
            ),
            ..ProfileUpdate::default()
        },
    )
    .await
    .unwrap();
    personas
}

/// Every remote + local status, paired with its author, plus the ids of
/// public statuses that mention the read personas.
struct SeededStatuses {
    all: Vec<(i64, i64)>,
    mention_sparse: Vec<i64>,
    mention_dense: Vec<i64>,
}

async fn seed_statuses(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    personas: &Personas,
) -> SeededStatuses {
    let sparse = personas.sparse;
    let dense = personas.dense;
    let chunk = TOTAL_REMOTE_STATUSES.div_ceil(WORKERS);
    let regular_pool = REMOTE_ACCOUNTS - PROLIFIC_ACCOUNTS - MID_ACCOUNTS;
    let remote = fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let remotes = Arc::clone(remotes);
        async move {
            let mut rng = Rng::new(3_000 + worker);
            let mut created: Vec<(i64, i64)> = Vec::new();
            let mut mention_sparse = Vec::new();
            let mut mention_dense = Vec::new();
            let end = ((worker + 1) * chunk).min(TOTAL_REMOTE_STATUSES);
            for j in (worker * chunk)..end {
                // First block: prolific tier. Second: mid tier. Rest: the
                // long tail, quadratically skewed so a few tail authors are
                // busy and most have a handful of posts (staging: p50=2,
                // p90=15, p99=110 statuses per author).
                let author_index = if j < PROLIFIC_ACCOUNTS * PROLIFIC_STATUSES_EACH {
                    j / PROLIFIC_STATUSES_EACH
                } else if j < PROLIFIC_ACCOUNTS * PROLIFIC_STATUSES_EACH
                    + MID_ACCOUNTS * MID_STATUSES_EACH
                {
                    PROLIFIC_ACCOUNTS
                        + (j - PROLIFIC_ACCOUNTS * PROLIFIC_STATUSES_EACH) / MID_STATUSES_EACH
                } else {
                    let r = rng.below(regular_pool);
                    PROLIFIC_ACCOUNTS + MID_ACCOUNTS + r * r / regular_pool
                };
                let author = remotes[author_index as usize];
                let in_reply_to = (rng.chance(28) && !created.is_empty())
                    .then(|| created[rng.below(created.len() as u64) as usize].0);
                let marker = (j % 499 == 0).then_some("zephyrite benchmarks");
                let mut content = sentence(&mut rng, marker);
                // Custom-emoji shortcodes in ~8% of posts from emoji-bearing
                // domains — the render's emoji_map scan resolves these
                // against the 50k-row emoji table.
                let d = domain_index(author_index);
                if d < EMOJI_DOMAINS && rng.chance(8) {
                    let text = format!(" :bemoji{}:</p>", rng.below(4));
                    content = content.replace("</p>", &text);
                }
                let domain = domain_of(author_index);
                let created_at = past_timestamp(&mut rng);
                let sensitive = rng.chance(10);
                let spoiler = sensitive && rng.chance(30);
                let status = status::upsert_remote(
                    &pool,
                    NewRemoteStatus {
                        title: None,
                        object_type: None,
                        external_url: None,
                        uri: &format!("https://{domain}/statuses/bench-{j}"),
                        account_id: author,
                        content: &content,
                        created_at,
                        visibility: visibility(&mut rng),
                        in_reply_to_id: in_reply_to,
                        in_reply_to_uri: None,
                        spoiler_text: if spoiler { "bench cw" } else { "" },
                        sensitive,
                        language: language(&mut rng),
                        url: None,
                        quote_approval_policy: 0,
                    },
                )
                .await
                .unwrap();
                created.push((status.id, author));
                if j % 499 == 0 {
                    mention::attach(&pool, status.id, sparse, false)
                        .await
                        .unwrap();
                    mention_sparse.push(status.id);
                } else if j % 631 == 0 {
                    mention::attach(&pool, status.id, dense, false)
                        .await
                        .unwrap();
                    mention_dense.push(status.id);
                }
            }
            (created, mention_sparse, mention_dense)
        }
    })
    .await;

    let local_chunk = LOCAL_STATUSES.div_ceil(WORKERS);
    let local = fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let locals = Arc::clone(locals);
        async move {
            let mut rng = Rng::new(4_000 + worker);
            let mut created: Vec<(i64, i64)> = Vec::new();
            let end = ((worker + 1) * local_chunk).min(LOCAL_STATUSES);
            for _ in (worker * local_chunk)..end {
                let author = locals[rng.below(LOCAL_ACCOUNTS) as usize];
                let in_reply_to = (rng.chance(20) && !created.is_empty())
                    .then(|| created[rng.below(created.len() as u64) as usize].0);
                let mut content = sentence(&mut rng, None);
                // Local shortcodes on ~8% of local posts. `emoji_map` keys the
                // lookup on the author's domain, so only a *locally* authored
                // status ever takes the `domain IS NULL` arm.
                if rng.chance(8) {
                    let text = format!(" :lemoji{}:</p>", rng.below(LOCAL_EMOJIS));
                    content = content.replace("</p>", &text);
                }
                let status = status::create_local(
                    &pool,
                    NewLocalStatus {
                        language: Some("en"),
                        ..NewLocalStatus::new(author, &content, visibility(&mut rng), in_reply_to)
                    },
                )
                .await
                .unwrap();
                created.push((status.id, author));
            }
            created
        }
    })
    .await;

    let mut all = Vec::new();
    let mut mention_sparse = Vec::new();
    let mut mention_dense = Vec::new();
    for (created, sparse_ids, dense_ids) in remote {
        all.extend(created);
        mention_sparse.extend(sparse_ids);
        mention_dense.extend(dense_ids);
    }
    all.extend(local.into_iter().flatten());
    SeededStatuses {
        all,
        mention_sparse,
        mention_dense,
    }
}

struct OwnStatuses {
    sparse: Vec<i64>,
    dense: Vec<i64>,
    popular: Vec<i64>,
}

/// The personas' own posts (favourite/reblog notifications point at them;
/// popular's 2k-status history feeds the profile and AP outbox benches).
///
/// One in [`ARTICLE_EVERY`] of popular's posts is a long-form `Article`: a
/// title, an `object_type`, and a body an order of magnitude longer than a
/// Note. `bench_popular` is the persona whose profile page, AP actor and outbox
/// page are benched, so it is the only place the title fold
/// (`entities::fold_typed_content`), the `<h1>` bake and the typed AP object
/// are on a measured path at all.
async fn seed_persona_statuses(
    pool: &PgPool,
    personas: &Personas,
    statuses: &mut SeededStatuses,
) -> OwnStatuses {
    let mut rng = Rng::new(5_000);
    let mut own = OwnStatuses {
        sparse: Vec::new(),
        dense: Vec::new(),
        popular: Vec::new(),
    };
    for (author, count, bucket) in [
        (personas.sparse, 100_u64, 0_usize),
        (personas.dense, 50, 1),
        (personas.popular, 2_000, 2),
    ] {
        for n in 0..count {
            let article = bucket == 2 && n % ARTICLE_EVERY == 0;
            let title = article.then(|| headline(&mut rng));
            let content = if article {
                long_form(&mut rng, title.as_deref().unwrap_or_default())
            } else {
                sentence(&mut rng, None)
            };
            let visibility = if rng.chance(90) { "public" } else { "unlisted" };
            let status = status::create_local(
                pool,
                NewLocalStatus {
                    language: Some("en"),
                    title: title.as_deref(),
                    object_type: article.then_some("Article"),
                    ..NewLocalStatus::new(author, &content, visibility, None)
                },
            )
            .await
            .unwrap();
            statuses.all.push((status.id, author));
            match bucket {
                0 => own.sparse.push(status.id),
                1 => own.dense.push(status.id),
                _ => own.popular.push(status.id),
            }
        }
    }
    own
}

/// A stable ordinal per status, so the set-based enrichment passes can select a
/// slice without hashing the id.
///
/// Snowflake ids encode the wall clock and are minted by 16 concurrent workers,
/// so `hashint8(id)` drew a different 34.5% of statuses on every reseed, with a
/// different media and card density behind `benchtag` — the tag timeline
/// measured a different row set at no code change (B4). `statuses.all` is
/// assembled in worker order, so ordinal N names the same logical row on every
/// build.
async fn build_status_ordinals(pool: &PgPool, statuses: &Arc<SeededStatuses>) {
    let ids: Vec<i64> = statuses.all.iter().map(|&(id, _)| id).collect();
    sqlx::raw_sql("DROP TABLE IF EXISTS bench_status_ord")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE UNLOGGED TABLE bench_status_ord AS
         SELECT id AS status_id, (ord - 1)::bigint AS ord
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, ord)",
    )
    .bind(&ids)
    .execute(pool)
    .await
    .unwrap();
    sqlx::raw_sql("ALTER TABLE bench_status_ord ADD PRIMARY KEY (status_id)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::raw_sql("ANALYZE bench_status_ord")
        .execute(pool)
        .await
        .unwrap();
}

async fn drop_status_ordinals(pool: &PgPool) {
    sqlx::raw_sql("DROP TABLE IF EXISTS bench_status_ord")
        .execute(pool)
        .await
        .unwrap();
}

/// Spreads local statuses over the same 180-day window the remote ones use.
///
/// `status::create_local` takes no timestamp and `NewLocalStatus` has no field
/// for one, so every local status — the 9,000 bulk posts, the personas' 2,150,
/// the group's submissions and the deep thread — took `now()` from the column
/// default and landed in one 68-second window at the very top of every
/// `sort_at`-ordered feed. `api/home_sparse` and `web/web_home` paged the
/// persona's own posts; `api/public_local` paged the seed's last few minutes of
/// work (B6).
///
/// The offset is derived from the stable ordinal, not from the id, and
/// `sort_at` is clamped to `created_at` exactly the way `upsert_remote` does it
/// (`sort_at = LEAST(created_at, now())`). Ids keep encoding *ingest* time,
/// which is what they mean — remote statuses have had this shape all along.
async fn backdate_local_statuses(pool: &PgPool) {
    sqlx::raw_sql(
        r"
        UPDATE statuses s
           SET created_at = t.at, sort_at = t.at, updated_at = t.at
          FROM (
            SELECT o.status_id,
                   now() - make_interval(secs => ((o.ord * 2654435761) % 15552000)::double precision)
                     AS at
            FROM bench_status_ord o
          ) t
         WHERE t.status_id = s.id AND s.uri IS NULL AND s.reblog_of_id IS NULL
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Lays the 101-status thread out as one conversation in time.
///
/// Its remote replies were stamped `now()` at creation and its local ones took
/// the column default, so the whole thread sat on top of the federated timeline
/// and, after the generic backdate above, its local members scattered across
/// 180 days while its remote ones stayed at the head — replies before the posts
/// they answer. Forty days back, seven minutes per hop, is a thread.
async fn backdate_thread(pool: &PgPool, root: i64) {
    sqlx::query(
        r"
        WITH RECURSIVE thread AS (
            SELECT id, 0 AS depth FROM statuses WHERE id = $1
          UNION ALL
            SELECT s.id, t.depth + 1
            FROM statuses s JOIN thread t ON s.in_reply_to_id = t.id
        )
        UPDATE statuses s
           SET created_at = at.stamp, sort_at = at.stamp, updated_at = at.stamp
          FROM (
            SELECT id,
                   now() - interval '40 days' + make_interval(mins => depth * 7) AS stamp
            FROM thread
          ) at
         WHERE at.id = s.id
        ",
    )
    .bind(root)
    .execute(pool)
    .await
    .unwrap();
}

/// Lays the community's submissions out evenly over 90 days, in the order they
/// were written.
///
/// This is what makes the group benchmarks deterministic in the one way that
/// matters here: every [`GROUP_EVENT_EVERY`]th submission is an `Event`, so an
/// even spread puts a known one or two of them on the first page of
/// `api/group_{new,top,hot}` — which is the only benched surface where the
/// event sidecar batch is measured rather than merely present. Under the
/// generic random backdate the first page contained an event about five runs in
/// six, and the sixth would have frozen a query count that skipped the batch.
///
/// The ordinal is the submission's position in `statuses.all`, which for this
/// slice is its creation index: the workers cover contiguous, equal ranges and
/// are concatenated in worker order.
async fn backdate_group_posts(pool: &PgPool, group_account: i64) {
    sqlx::query(
        r"
        WITH ordered AS (
            SELECT s.id, row_number() OVER (ORDER BY o.ord) - 1 AS n,
                   count(*) OVER () AS total
            FROM statuses s
            JOIN bench_status_ord o ON o.status_id = s.id
            JOIN status_mentions m ON m.status_id = s.id AND m.account_id = $1
        )
        UPDATE statuses s
           SET created_at = at.stamp, sort_at = at.stamp, updated_at = at.stamp
          FROM (
            SELECT id,
                   now() - interval '90 days'
                     + make_interval(secs => (n * 7776000.0 / total)) AS stamp
            FROM ordered
          ) at
         WHERE at.id = s.id
        ",
    )
    .bind(group_account)
    .execute(pool)
    .await
    .unwrap();
}

/// Boost rows follow the thing they boosted.
///
/// Local reblogs are created after the backdate above (and by `seed_group`,
/// before it), so they would otherwise be the only rows left at `now()` — which
/// puts every boost in the dataset at the head of every feed. An hour after the
/// original is the shape a real timeline has.
async fn backdate_local_reblogs(pool: &PgPool) {
    sqlx::raw_sql(
        r"
        UPDATE statuses r
           SET created_at = LEAST(t.created_at + interval '1 hour', now()),
               sort_at    = LEAST(t.sort_at + interval '1 hour', now()),
               updated_at = LEAST(t.created_at + interval '1 hour', now())
          FROM statuses t
         WHERE t.id = r.reblog_of_id AND r.uri IS NULL AND r.reblog_of_id IS NOT NULL
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The bulk tag vocabulary (staging: 87.5k tags, x3 ≈ 262k) plus the hot
/// `benchtag` the tag-timeline bench reads. Names are word-prefixed so the
/// tag-search bench has realistic prefix matches.
async fn seed_tags(pool: &PgPool) -> i64 {
    let chunk = BULK_TAGS.div_ceil(WORKERS);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        async move {
            let end = ((worker + 1) * chunk).min(BULK_TAGS);
            for i in (worker * chunk)..end {
                tag::ensure(&pool, &format!("{}{i}", WORDS[(i % 24) as usize]))
                    .await
                    .unwrap();
            }
        }
    })
    .await;
    tag::ensure(pool, "benchtag").await.unwrap()
}

/// Tag memberships + daily usage rollups, set-based.
///
/// Mirrors `tag::attach` (`status_tags` carries the status' immutable
/// `sort_at`, migration 0131) and `tag::record_uses` (public, non-reblog,
/// usable tag; the silenced-author arm is intentionally omitted — moderation
/// flags are seeded later, so it would filter nothing here anyway).
/// 34.5% of statuses tagged with 1-8 tags (avg ~4.5) — staging's measured
/// share, ~1.9M membership rows; per-row round trips would dominate the seed.
async fn seed_status_tags(pool: &PgPool, benchtag: i64) {
    // The tag pick goes through an indexed helper table, NOT an in-query
    // array: subscripting a 262k-element (~2 MB) array datum re-detoasts
    // the whole array once per candidate row — ~10M rows made that a
    // multi-hour hang. A hash join against a real table costs seconds.
    //
    // Ordered by *name*, not by id: tag ids are wall-clock snowflakes minted
    // by concurrent workers, so ordering by id shuffled the whole index on
    // every reseed. Names are `{word}{i}`, which is deterministic.
    sqlx::raw_sql(
        "CREATE UNLOGGED TABLE bench_tag_pick AS
         SELECT (row_number() OVER (ORDER BY name) - 1)::int AS idx, id AS tag_id
         FROM tags WHERE name <> 'benchtag'",
    )
    .execute(pool)
    .await
    .unwrap();
    // The pick-count scalar subquery is uncorrelated — one InitPlan
    // evaluation, not per-row.
    sqlx::raw_sql(
        r"
        INSERT INTO status_tags (status_id, tag_id, sort_at)
        SELECT s.id, tp.tag_id, s.sort_at
        FROM statuses s
        JOIN bench_status_ord o ON o.status_id = s.id
        CROSS JOIN generate_series(0, 7) AS g(k)
        JOIN bench_tag_pick tp
          ON tp.idx = (((o.ord * 2246822519 + g.k::bigint * 668265263) % 2147483647)
                       % (SELECT count(*)::bigint FROM bench_tag_pick))::int
        WHERE s.reblog_of_id IS NULL
          AND ((o.ord * 40503) % 1000) < 345
          AND g.k <= ((o.ord * 2654435761) % 8)
        ON CONFLICT DO NOTHING
        ",
    )
    .execute(pool)
    .await
    .unwrap();

    // The hot tag the tag-timeline bench reads (~1 in 70 statuses).
    sqlx::query(
        r"
        INSERT INTO status_tags (status_id, tag_id, sort_at)
        SELECT s.id, $1, s.sort_at
        FROM statuses s
        JOIN bench_status_ord o ON o.status_id = s.id
        WHERE s.reblog_of_id IS NULL AND (o.ord % 70) = 0
        ON CONFLICT DO NOTHING
        ",
    )
    .bind(benchtag)
    .execute(pool)
    .await
    .unwrap();

    // A real trending set. `tag_trends` held exactly one row, because a tag
    // needs five *distinct accounts* using it on the scored day to clear the
    // threshold and 87,001 tags averaged 1.1 usages each — so `api/trends_tags`
    // rendered a one-element array and `db_jobs/trends_refresh_tags` scored one
    // candidate (B7). Concentrating one day's usage on the first [`HOT_TAGS`]
    // tags of the pick table gives the ranker a population to rank.
    //
    // Confined to a single calendar day on purpose: the score is
    // `(today - yesterday)² / yesterday` and collapses to zero as soon as
    // yesterday catches up, so a window straddling midnight would split the
    // concentration in half and rank nothing. The day is the newest *complete*
    // one — the newest day itself is only as full as the hour the seed happened
    // to run at, which at 00:30 would be fifty statuses across forty-five tags
    // and nothing would clear the threshold. It is recorded as a marker and
    // handed back to the refresh by [`trends_reference`], so the trending set
    // survives the dataset outliving its build day.
    let hot_day: time::Date =
        sqlx::query_scalar("SELECT max((created_at AT TIME ZONE 'UTC')::date) - 1 FROM statuses")
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        r"
        INSERT INTO status_tags (status_id, tag_id, sort_at)
        SELECT hot.id, tp.tag_id, hot.sort_at
        FROM (
            SELECT s.id, s.sort_at,
                   row_number() OVER (ORDER BY o.ord) - 1 AS n
            FROM statuses s
            JOIN bench_status_ord o ON o.status_id = s.id
            WHERE s.reblog_of_id IS NULL AND s.visibility = 'public'
              AND (s.created_at AT TIME ZONE 'UTC')::date = $2
        ) hot
        JOIN bench_tag_pick tp ON tp.idx = (hot.n % $1)::int
        ON CONFLICT DO NOTHING
        ",
    )
    .bind(HOT_TAGS as i64)
    .bind(hot_day)
    .execute(pool)
    .await
    .unwrap();
    set_marker(pool, "trends_day", &hot_day.to_string()).await;

    sqlx::raw_sql("DROP TABLE bench_tag_pick")
        .execute(pool)
        .await
        .unwrap();

    // Daily usage rollups for trends (mirrors tag::record_uses).
    sqlx::raw_sql(
        r"
        INSERT INTO tag_usages (tag_id, day, account_id, uses)
        SELECT st.tag_id, (s.created_at AT TIME ZONE 'UTC')::date, s.account_id, count(*)::int
        FROM status_tags st
        JOIN statuses s ON s.id = st.status_id
        JOIN tags t ON t.id = st.tag_id
        WHERE s.reblog_of_id IS NULL AND s.visibility = 'public' AND t.usable IS NOT FALSE
        GROUP BY 1, 2, 3
        ON CONFLICT (tag_id, day, account_id)
        DO UPDATE SET uses = tag_usages.uses + EXCLUDED.uses
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Remote media on ~28% of statuses, 1-2 attachments (staging: 28% of
/// statuses, 1.29 attachments each), across the shapes the serializer actually
/// branches on. Placeholder rows, never downloaded — the media worker is not
/// running during benches.
async fn seed_media(pool: &PgPool, statuses: &Arc<SeededStatuses>) {
    let chunk = statuses.all.len().div_ceil(WORKERS as usize);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let statuses = Arc::clone(statuses);
        async move {
            let mut rng = Rng::new(8_000 + worker);
            let start = worker as usize * chunk;
            let end = (start + chunk).min(statuses.all.len());
            for &(status_id, author) in &statuses.all[start..end] {
                if rng.chance(28) {
                    let count = if rng.chance(31) { 2 } else { 1 };
                    for slot in 0..count {
                        attach_remote_media(&pool, status_id, author, slot, &mut rng).await;
                    }
                }
            }
        }
    })
    .await;

    // A few hundred attachments that finished downloading: a real instance
    // serves most of its remote images from its own cache, and this is the only
    // shape that takes the `file_name` arm of the media serializer, carries a
    // preview and counts toward `cached_remote_bytes`.
    //
    // `file_name` is mandatory for these: `media::for_statuses` re-enqueues a
    // download for every `complete` row with no file, one UPDATE + one INSERT
    // per row, on the render path — a `complete` row without a file would turn
    // every timeline bench into a writer.
    sqlx::raw_sql(
        r"
        UPDATE media_attachments m
           SET processing = 'complete',
               kind = 'image',
               content_type = 'image/avif',
               file_name = m.id || '.avif',
               small_file_name = m.id || '.small.avif',
               small_width = 320,
               small_height = 180,
               file_size = 48000 + (m.id % 40000),
               thumbnail_file_size = 6000 + (m.id % 3000),
               cached_at = now() - make_interval(secs => (m.id % 604800)::double precision)
         WHERE m.id IN (
             SELECT id FROM media_attachments
             WHERE remote_url IS NOT NULL AND content_type LIKE 'image/%'
             ORDER BY id LIMIT 400
         )
        ",
    )
    .execute(pool)
    .await
    .unwrap();
    // Their processing jobs are done with.
    sqlx::raw_sql(
        "DELETE FROM media_processing_jobs j
         USING media_attachments m
         WHERE m.id = j.media_id AND m.processing = 'complete'",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The attachment shapes, at roughly the mix a general-purpose instance sees.
///
/// Every attachment used to be `image/jpeg` with `kind` NULL, no duration, no
/// renditions and no thumbnail, so `media_json_hls` took one branch for all
/// 128,471 of them: no video `meta.original` block, no HLS ladder, no poster,
/// no live state, and the web renderer's `gifv`/`video`/`audio` arms were dead
/// (B9).
#[derive(Clone, Copy)]
enum MediaShape {
    Image,
    Video,
    Hls,
    Audio,
    Gifv,
}

impl MediaShape {
    fn pick(rng: &mut Rng) -> Self {
        match rng.below(100) {
            0..=79 => Self::Image,
            80..=91 => Self::Video,
            92..=95 => Self::Hls,
            96..=98 => Self::Audio,
            _ => Self::Gifv,
        }
    }
}

/// Preview cards on ~25% of statuses (staging share). Most links are unique;
/// a fifth land on a shared hot-card pool (link storms), so cards ≈ 0.85x
/// attaches like staging's 85k cards / 112k attaches.
async fn seed_preview_cards(pool: &PgPool, statuses: &Arc<SeededStatuses>) {
    for k in 0..CARD_PROVIDERS {
        preview_card_provider::ensure(pool, &format!("news{k}.bench.invalid"))
            .await
            .unwrap();
    }

    let chunk = statuses.all.len().div_ceil(WORKERS as usize);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let statuses = Arc::clone(statuses);
        async move {
            let mut rng = Rng::new(14_000 + worker);
            let start = worker as usize * chunk;
            let end = (start + chunk).min(statuses.all.len());
            for (offset, &(status_id, _)) in statuses.all[start..end].iter().enumerate() {
                if !rng.chance(25) {
                    continue;
                }
                let card_index = if rng.chance(20) {
                    rng.below(HOT_CARDS)
                } else {
                    // Unique per position — collisions across workers are
                    // rare and harmless (upsert on url).
                    HOT_CARDS + (start + offset) as u64
                };
                let provider = card_index % CARD_PROVIDERS;
                let url = format!("https://news{provider}.bench.invalid/articles/{card_index}");
                let title = sentence(&mut rng, None);
                let card = preview_card::upsert(
                    &pool,
                    NewPreviewCard {
                        url: &url,
                        title: &title,
                        description: "bench article",
                        kind: "link",
                        author_name: "Bench Author",
                        author_url: "",
                        provider_name: "Bench News",
                        provider_url: &format!("https://news{provider}.bench.invalid"),
                        html: "",
                        width: 1_200,
                        height: 630,
                        image_url: Some(&format!(
                            "https://news{provider}.bench.invalid/images/{card_index}.jpg"
                        )),
                        image_description: "",
                        embed_url: "",
                        language: Some("en"),
                        published_at: None,
                        author_account_id: None,
                    },
                )
                .await
                .unwrap();
                preview_card::attach(&pool, status_id, card.id, &url)
                    .await
                    .unwrap();
            }
        }
    })
    .await;

    // Daily link-usage rollups for the trends worker (mirrors
    // preview_card_trend::record_use's public/original/no-CW arm; the
    // silenced-author check is moot — moderation is seeded later).
    sqlx::raw_sql(
        r"
        INSERT INTO preview_card_usages (preview_card_id, day, account_id, uses)
        SELECT pcs.preview_card_id, (s.created_at AT TIME ZONE 'UTC')::date, s.account_id,
               count(*)::int
        FROM preview_cards_statuses pcs
        JOIN statuses s ON s.id = pcs.status_id
        WHERE s.reblog_of_id IS NULL AND s.visibility = 'public'
          AND NOT s.sensitive AND s.spoiler_text = ''
        GROUP BY 1, 2, 3
        ON CONFLICT (preview_card_id, day, account_id)
        DO UPDATE SET uses = preview_card_usages.uses + EXCLUDED.uses
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Remote polls with cached tallies on a sample of statuses (staging: 1.3k),
/// plus one local poll owned by `bench_popular`. Returns the poll ids in
/// creation order, so the vote pass can pick a deterministic slice.
///
/// The local poll is deliberately *not* `bench_writer`'s: the write benches
/// delete that persona's statuses at the start of every run, which would take
/// the poll and its votes with them.
async fn seed_polls(pool: &PgPool, statuses: &Arc<SeededStatuses>) -> Vec<i64> {
    let mut rng = Rng::new(15_000);
    let mut ids = Vec::with_capacity(POLLS as usize);
    let stride = statuses.all.len() / POLLS as usize;
    for p in 0..POLLS as usize {
        let (status_id, author) = statuses.all[p * stride];
        let option_count = 2 + rng.below(3);
        let options: Vec<String> = (0..option_count).map(|o| format!("option {o}")).collect();
        let tallies: Vec<i64> = (0..option_count).map(|_| rng.below(500) as i64).collect();
        let voters = tallies.iter().sum::<i64>();
        let created = poll::create(
            pool,
            NewPoll {
                status_id,
                account_id: author,
                options: &options,
                cached_tallies: &tallies,
                multiple: rng.chance(20),
                hide_totals: false,
                voters_count: Some(voters),
                expires_at: Some(
                    OffsetDateTime::now_utc() - Duration::seconds(rng.below(30 * 86_400) as i64),
                ),
            },
        )
        .await
        .unwrap();
        ids.push(created.id);
    }
    ids
}

/// Poll votes.
///
/// `poll_votes` was empty, so every cached tally in the dataset was fiction
/// with no backing rows, and `poll_map`'s `own_votes` lookup — which runs on
/// every signed-in page carrying a poll — always returned nothing. That left
/// `voted` true only for a poll's own author, so the web renderer's results-bar
/// branch was unreachable and every signed-in viewer took the vote-form arm.
async fn seed_poll_votes(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    personas: &Personas,
    poll_ids: &[i64],
) {
    let mut rng = Rng::new(23_000);
    // The read personas have voted on a slice of the polls they can see.
    for (n, &poll_id) in poll_ids.iter().enumerate() {
        if n % 3 == 0 {
            poll::insert_vote(pool, poll_id, personas.dense, (n % 2) as i32, None)
                .await
                .unwrap();
        }
        if n % 5 == 0 {
            poll::insert_vote(pool, poll_id, personas.sparse, 0, None)
                .await
                .unwrap();
        }
    }

    // One local poll with a real electorate behind its tallies.
    let content = sentence(&mut rng, Some("bench poll"));
    let status = status::create_local(
        pool,
        NewLocalStatus {
            language: Some("en"),
            ..NewLocalStatus::new(personas.popular, &content, "public", None)
        },
    )
    .await
    .unwrap();
    // Created after the backdate pass, so it would otherwise be the single
    // newest local status in the database and sit at the head of every feed.
    sqlx::query(
        "UPDATE statuses SET created_at = now() - interval '3 days',
                             sort_at = now() - interval '3 days',
                             updated_at = now() - interval '3 days'
         WHERE id = $1",
    )
    .bind(status.id)
    .execute(pool)
    .await
    .unwrap();
    let options: Vec<String> = (0..4).map(|o| format!("option {o}")).collect();
    let local = poll::create(
        pool,
        NewPoll {
            status_id: status.id,
            account_id: personas.popular,
            options: &options,
            cached_tallies: &[0, 0, 0, 0],
            multiple: false,
            hide_totals: false,
            voters_count: Some(0),
            expires_at: Some(OffsetDateTime::now_utc() + Duration::seconds(7 * 86_400)),
        },
    )
    .await
    .unwrap();
    for v in 0..POLL_VOTES {
        // Distinct voters: the unique key is (poll, account, choice), and a
        // repeat account would silently insert nothing.
        let voter = remotes[((v * 149 + 11) % REMOTE_ACCOUNTS) as usize];
        poll::insert_vote(pool, local.id, voter, rng.below(4) as i32, None)
            .await
            .unwrap();
    }
    poll::insert_vote(pool, local.id, personas.dense, 2, None)
        .await
        .unwrap();
    // The tallies now describe the rows, the way the live vote path leaves them.
    poll::refresh_local_tallies(pool, local.id).await.unwrap();
}

/// Quote edges between existing statuses (staging x3: ~26.5k; 82% accepted,
/// 17% pending). Samples by id-modulo so no status list has to stay in
/// memory; `quotes` is keyed on the quoting status' uri.
async fn seed_quotes(pool: &PgPool) {
    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT s.id, s.uri, s.account_id FROM statuses s
         JOIN bench_status_ord o ON o.status_id = s.id
         WHERE s.uri IS NOT NULL AND s.reblog_of_id IS NULL AND o.ord % 20 = 7
         ORDER BY o.ord LIMIT $1",
    )
    .bind(QUOTES as i64 * 2)
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(rows.len() as u64 >= QUOTES * 2, "not enough quote samples");

    let rows = Arc::new(rows);
    let chunk = (QUOTES as usize).div_ceil(WORKERS as usize);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let rows = Arc::clone(&rows);
        async move {
            let mut rng = Rng::new(16_000 + worker);
            let start = worker as usize * chunk;
            let end = (start + chunk).min(QUOTES as usize);
            for q in start..end {
                let (status_id, status_uri, account_id) = &rows[q];
                let (quoted_id, quoted_uri, quoted_account) = &rows[QUOTES as usize + q];
                let state = match rng.below(100) {
                    0..=81 => "accepted",
                    82..=98 => "pending",
                    _ => "revoked",
                };
                quote::create(
                    &pool,
                    NewQuote {
                        quote_id: id::next(),
                        status_id: Some(*status_id),
                        status_uri,
                        account_id: *account_id,
                        quoted_status_id: Some(*quoted_id),
                        quoted_account_id: Some(*quoted_account),
                        state,
                        activity_uri: None,
                        approval_uri: None,
                        quoted_uri: Some(quoted_uri),
                        legacy: false,
                    },
                )
                .await
                .unwrap();
            }
        }
    })
    .await;
}

/// Edit history on a sample of statuses (staging x3: ~8.4k edited, ~2.2
/// snapshots each): a baseline snapshot at creation plus one edit, matching
/// the Mastodon-parity convention `status::apply_edit` follows.
async fn seed_edits(pool: &PgPool) {
    let rows: Vec<(i64, i64, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT s.id, s.account_id, s.content, s.created_at FROM statuses s
         JOIN bench_status_ord o ON o.status_id = s.id
         WHERE s.uri IS NOT NULL AND s.reblog_of_id IS NULL AND o.ord % 150 = 3
         ORDER BY o.ord LIMIT $1",
    )
    .bind(EDITED_STATUSES as i64)
    .fetch_all(pool)
    .await
    .unwrap();

    let rows = Arc::new(rows);
    let chunk = rows.len().div_ceil(WORKERS as usize);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let rows = Arc::clone(&rows);
        async move {
            let mut rng = Rng::new(18_000 + worker);
            let start = worker as usize * chunk;
            let end = (start + chunk).min(rows.len());
            for (status_id, account_id, content, created_at) in &rows[start..end] {
                let edited_at = *created_at + Duration::seconds(60 + rng.below(86_400) as i64);
                let new_content = format!("{content}<p>edited</p>");
                for (text, at) in [(content, *created_at), (&new_content, edited_at)] {
                    status_edit::snapshot(
                        &pool,
                        NewStatusEdit {
                            status_id: *status_id,
                            account_id: *account_id,
                            content: text,
                            text: "",
                            spoiler_text: "",
                            sensitive: false,
                            media_ids: &[],
                            created_at: at,
                        },
                    )
                    .await
                    .unwrap();
                }
                sqlx::query("UPDATE statuses SET edited_at = $2, content = $3 WHERE id = $1")
                    .bind(status_id)
                    .bind(edited_at)
                    .bind(&new_content)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
    })
    .await;
}

/// Emoji reactions from remote accounts (staging x3: ~2.7k).
async fn seed_reactions(pool: &PgPool, remotes: &Arc<Vec<i64>>, statuses: &Arc<SeededStatuses>) {
    let mut rng = Rng::new(19_000);
    for _ in 0..REACTIONS {
        let reactor = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
        let (target, _) = statuses.all[rng.below(statuses.all.len() as u64) as usize];
        reaction::create(
            pool,
            NewReaction {
                account_id: reactor,
                status_id: target,
                name: ["👍", "🎉", "😂", "❤️"][rng.below(4) as usize],
                custom_emoji_url: None,
                uri: None,
            },
        )
        .await
        .unwrap();
    }
}

/// Boosts: local accounts reblog into the shared feeds (renderer's
/// boost-target batch), plus remote Announces like staging ingests.
async fn seed_boosts(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    statuses: &Arc<SeededStatuses>,
) {
    let mut rng = Rng::new(13_000);
    for _ in 0..LOCAL_BOOSTS {
        let booster = locals[rng.below(LOCAL_ACCOUNTS) as usize];
        let (target, _) = statuses.all[rng.below(statuses.all.len() as u64) as usize];
        status::create_local_reblog(pool, booster, target)
            .await
            .unwrap();
    }
    for b in 0..REMOTE_BOOSTS {
        let idx = rng.below(REMOTE_ACCOUNTS);
        let booster = remotes[idx as usize];
        let (target, _) = statuses.all[rng.below(statuses.all.len() as u64) as usize];
        status::upsert_remote_reblog(
            pool,
            &format!("https://{}/activities/boost-{b}", domain_of(idx)),
            booster,
            target,
            Some(past_timestamp(&mut rng)),
        )
        .await
        .unwrap();
    }
}

/// What `seed_group` produced.
struct SeededGroup {
    account: i64,
}

/// The ranked-sort fixture: one local group with [`GROUP_POSTS`]
/// submitted posts — mention-row attribution plus the group's boost rows,
/// exactly what the hosting path creates — and a vote spread (favourites and
/// dislikes from the remote crowd) for Top/Hot to rank.
///
/// Submissions are `Page`s carrying a title, and two in five carry a link as
/// well: a real community's front page is title + link + card, and this fixture
/// was 600 bare `<p>` bodies. One in [`GROUP_EVENT_EVERY`] is an `Event` with a
/// sidecar, which is what puts the event batch on a *measured* page.
async fn seed_group(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    statuses: &mut SeededStatuses,
) -> SeededGroup {
    let keypair = keys::generate_keypair().unwrap();
    let (group_account, _) = group::create(
        pool,
        NewLocalGroup {
            account: NewLocalAccount {
                username: "bench_group",
                display_name: "Bench group",
                note: "",
                public_key_pem: &keypair.public_pem,
            },
            membership_policy: MembershipPolicy::Open,
            posting_policy: PostingPolicy::Members,
            created_by: locals[0],
        },
    )
    .await
    .unwrap();

    let chunk = GROUP_POSTS.div_ceil(WORKERS);
    let created = fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let remotes = Arc::clone(remotes);
        let locals = Arc::clone(locals);
        let group_id = group_account.id;
        async move {
            let mut rng = Rng::new(17_000 + worker);
            let mut created: Vec<(i64, i64)> = Vec::new();
            let end = ((worker + 1) * chunk).min(GROUP_POSTS);
            for n in (worker * chunk)..end {
                let author = locals[rng.below(LOCAL_ACCOUNTS) as usize];
                let event = n % GROUP_EVENT_EVERY == 0;
                let title = headline(&mut rng);
                // A link submission needs a title; the reverse is not true.
                let link = (!event && rng.chance(40))
                    .then(|| format!("https://news{}.bench.invalid/articles/group-{n}", n % 97));
                let content = sentence(&mut rng, None);
                let status = status::create_local(
                    &pool,
                    NewLocalStatus {
                        language: Some("en"),
                        title: Some(&title),
                        external_url: link.as_deref(),
                        object_type: Some(if event { "Event" } else { "Page" }),
                        ..NewLocalStatus::new(author, &content, "public", None)
                    },
                )
                .await
                .unwrap();
                if event {
                    // Locally authored, so `event_map` reads its attendance
                    // from `status_participations` rather than the sidecar's
                    // cached count.
                    attach_event(&pool, status.id, &mut rng, true).await;
                    let attendees = 2 + rng.below(9);
                    for a in 0..attendees {
                        let who = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
                        let state = if a % 5 == 0 {
                            RsvpState::Pending
                        } else {
                            RsvpState::Accepted
                        };
                        status_participation::upsert(
                            &pool,
                            status.id,
                            who,
                            state,
                            Some(&format!("https://bench.invalid/joins/{}-{a}", status.id)),
                            None,
                        )
                        .await
                        .unwrap();
                    }
                }
                mention::attach(&pool, status.id, group_id, true)
                    .await
                    .unwrap();
                status::create_local_reblog(&pool, group_id, status.id)
                    .await
                    .unwrap();
                created.push((status.id, author));
                // A long-tailed vote spread: most posts a handful, a few
                // dozens — enough differentiation for the rank orders.
                let upvotes = if rng.chance(10) {
                    20 + rng.below(60)
                } else {
                    rng.below(12)
                };
                let downvotes = rng.below(1 + upvotes / 3);
                for v in 0..(upvotes + downvotes) {
                    let voter = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
                    if v < upvotes {
                        favourite::create(&pool, voter, status.id, None)
                            .await
                            .unwrap();
                    } else {
                        dislike::create(&pool, voter, status.id, None)
                            .await
                            .unwrap();
                    }
                }
            }
            created
        }
    })
    .await;
    statuses.all.extend(created.into_iter().flatten());
    SeededGroup {
        account: group_account.id,
    }
}

/// Writes the `status_events` sidecar for a status already typed `Event`.
///
/// The invariant `entities.rs` documents is that the two always travel
/// together: the render batch's guard is on `statuses.object_type`, so a
/// sidecar without the column is never read, and the column without a sidecar
/// costs a query that returns nothing.
async fn attach_event(pool: &PgPool, status_id: i64, rng: &mut Rng, local: bool) {
    let start = OffsetDateTime::now_utc() + Duration::seconds(rng.below(90 * 86_400) as i64)
        - Duration::seconds(45 * 86_400);
    let mut event = StatusEvent::empty(status_id);
    event.start_time = Some(start);
    event.end_time = Some(start + Duration::seconds((3_600 + rng.below(6 * 3_600)) as i64));
    event.timezone = Some("Europe/Sofia".to_owned());
    event.is_online = Some(rng.chance(30));
    event.comments_enabled = Some(true);
    event.category = Some(WORDS[rng.below(WORDS.len() as u64) as usize].to_owned());
    event.event_status = Some(
        match rng.below(100) {
            0..=4 => "CANCELLED",
            5..=14 => "TENTATIVE",
            _ => "CONFIRMED",
        }
        .to_owned(),
    );
    event.join_mode = Some(
        match rng.below(100) {
            0..=59 => "free",
            60..=79 => "restricted",
            80..=91 => "invite",
            _ => "external",
        }
        .to_owned(),
    );
    if event.join_mode.as_deref() == Some("external") {
        event.external_participation_url =
            Some(format!("https://tickets.bench.invalid/{status_id}"));
    }
    if event.is_online != Some(true) {
        event.location_name = Some(format!(
            "{} hall",
            WORDS[rng.below(WORDS.len() as u64) as usize]
        ));
        event.location_locality = Some("Sofia".to_owned());
        event.location_country = Some("BG".to_owned());
        event.location_street = Some(format!(
            "{} street {}",
            WORDS[rng.below(24) as usize],
            1 + rng.below(90)
        ));
        event.location_postal_code = Some("1000".to_owned());
    }
    event.max_attendees = rng.chance(40).then(|| 20 + rng.below(200) as i32);
    if local {
        // A locally authored event counts its attendance from the
        // participation rows; only remote ones carry the origin's cached count.
        event.anonymous_participation = Some(rng.chance(20));
    } else {
        let count = rng.below(400) as i32;
        event.participant_count = Some(count);
        event.remaining_attendees = event.max_attendees.map(|max| (max - count).max(0));
    }
    status_event::upsert(pool, &event).await.unwrap();
}

/// Standalone federated events outside the group, so the type is present in the
/// general population too — authored by the prolific tier the read personas
/// follow, and by local accounts, so both arms of `event_map`'s
/// locally-authored split have rows.
async fn seed_events(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    personas: &Personas,
    statuses: &mut SeededStatuses,
) {
    let mut rng = Rng::new(21_000);
    for e in 0..EVENT_STATUSES {
        let title = headline(&mut rng);
        let content = sentence(&mut rng, None);
        let remote = e % 5 < 3;
        let (id, author) = if remote {
            let author_index = e % PROLIFIC_ACCOUNTS;
            let author = remotes[author_index as usize];
            let domain = domain_of(author_index);
            let status = status::upsert_remote(
                pool,
                NewRemoteStatus {
                    title: Some(&title),
                    object_type: Some("Event"),
                    external_url: None,
                    uri: &format!("https://{domain}/events/bench-{e}"),
                    account_id: author,
                    content: &content,
                    created_at: past_timestamp(&mut rng),
                    visibility: "public",
                    in_reply_to_id: None,
                    in_reply_to_uri: None,
                    spoiler_text: "",
                    sensitive: false,
                    language: Some("en"),
                    url: None,
                    quote_approval_policy: 0,
                },
            )
            .await
            .unwrap();
            (status.id, author)
        } else {
            let author = locals[(e * 7) as usize % LOCAL_ACCOUNTS as usize];
            let status = status::create_local(
                pool,
                NewLocalStatus {
                    language: Some("en"),
                    title: Some(&title),
                    object_type: Some("Event"),
                    ..NewLocalStatus::new(author, &content, "public", None)
                },
            )
            .await
            .unwrap();
            (status.id, author)
        };
        attach_event(pool, id, &mut rng, !remote).await;
        // The read personas RSVP to a slice, so `states_of` — the one sidecar
        // query that only runs for a signed-in viewer — has rows to return.
        if e % 4 == 0 {
            for persona in [personas.sparse, personas.dense] {
                status_participation::upsert(
                    pool,
                    id,
                    persona,
                    if e % 8 == 0 {
                        RsvpState::Accepted
                    } else {
                        RsvpState::Pending
                    },
                    Some(&format!("https://plamenu.test/joins/bench-{e}-{persona}")),
                    None,
                )
                .await
                .unwrap();
            }
        }
        if !remote {
            for a in 0..=rng.below(6) {
                let who = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
                status_participation::upsert(
                    pool,
                    id,
                    who,
                    RsvpState::Accepted,
                    Some(&format!("https://bench.invalid/joins/event-{e}-{a}")),
                    None,
                )
                .await
                .unwrap();
            }
        }
        statuses.all.push((id, author));
    }
}

/// Replies whose parent never arrived.
///
/// 27% of the dataset is replies and every one of them resolved, so
/// `status::unresolved_reply_parents` returned empty on every page,
/// `idx_statuses_orphan_reply_uri` was an empty partial index, and the web
/// thread page's "could not be fetched" arm was never rendered. The parent URIs
/// deliberately name objects that are not in the database — an orphan whose
/// parent *is* stored is a state the live code repairs at ingest.
async fn seed_orphan_replies(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    statuses: &mut SeededStatuses,
) {
    let chunk = ORPHAN_REPLIES.div_ceil(WORKERS);
    let created = fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let remotes = Arc::clone(remotes);
        async move {
            let mut rng = Rng::new(22_000 + worker);
            let mut created: Vec<(i64, i64)> = Vec::new();
            let end = ((worker + 1) * chunk).min(ORPHAN_REPLIES);
            for j in (worker * chunk)..end {
                let author_index = rng.below(REMOTE_ACCOUNTS);
                let author = remotes[author_index as usize];
                let domain = domain_of(author_index);
                let content = sentence(&mut rng, None);
                let status = status::upsert_remote(
                    &pool,
                    NewRemoteStatus {
                        title: None,
                        object_type: None,
                        external_url: None,
                        uri: &format!("https://{domain}/statuses/bench-orphan-{j}"),
                        account_id: author,
                        content: &content,
                        created_at: past_timestamp(&mut rng),
                        visibility: "public",
                        in_reply_to_id: None,
                        in_reply_to_uri: Some(&format!(
                            "https://gone{}.bench.invalid/objects/{j}",
                            j % 400
                        )),
                        spoiler_text: "",
                        sensitive: false,
                        language: Some("en"),
                        url: None,
                        quote_approval_policy: 0,
                    },
                )
                .await
                .unwrap();
                // The placeholder conversation the real ingest mints for a
                // reply it cannot fold into a thread: no owner, no root, and
                // absorbable if the parent ever turns up.
                conversation::ensure_for_status(
                    &pool,
                    &conversation::EnsureConversation {
                        status_id: status.id,
                        account_id: author,
                        in_reply_to_id: None,
                        is_reply: true,
                        refs: conversation::ContextRefs::default(),
                    },
                )
                .await
                .unwrap();
                created.push((status.id, author));
            }
            created
        }
    })
    .await;
    statuses.all.extend(created.into_iter().flatten());
}

/// Favourites (Zipf-skewed onto a hot slice, like staging's fetched-trending
/// engagement), background dislikes, and the media-heavy single status.
/// Returns the id of the single-status bench target.
async fn seed_engagement(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    statuses: &Arc<SeededStatuses>,
) -> i64 {
    let chunk = FAVOURITES.div_ceil(WORKERS);
    fan_out(WORKERS, |worker| {
        let pool = pool.clone();
        let remotes = Arc::clone(remotes);
        let statuses = Arc::clone(statuses);
        async move {
            let mut rng = Rng::new(7_000 + worker);
            let len = statuses.all.len() as u64;
            let end = ((worker + 1) * chunk).min(FAVOURITES);
            for _ in (worker * chunk)..end {
                let fan = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
                let x = rng.below(len);
                let (target, _) = statuses.all[(x * x / len) as usize];
                favourite::create(&pool, fan, target, None).await.unwrap();
            }
        }
    })
    .await;

    let mut rng = Rng::new(7_500);
    for _ in 0..BACKGROUND_DISLIKES {
        let hater = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
        let (target, _) = statuses.all[rng.below(statuses.all.len() as u64) as usize];
        dislike::create(pool, hater, target, None).await.unwrap();
    }

    // The single-status bench target: a public post with media, card, tag and
    // favourites (deterministic, unlike the mixed-visibility bulk statuses).
    let author = remotes[0];
    let content = sentence(&mut rng, Some("single status target"));
    let single = status::upsert_remote(
        pool,
        NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: &format!("https://{}/statuses/bench-single", domain_of(0)),
            account_id: author,
            content: &content,
            created_at: past_timestamp(&mut rng),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: Some("en"),
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap()
    .id;
    attach_remote_media(pool, single, author, 0, &mut rng).await;
    let tag_id = tag::ensure(pool, "benchtag").await.unwrap();
    tag::attach(pool, single, tag_id).await.unwrap();
    let card = preview_card::upsert(
        pool,
        NewPreviewCard {
            url: "https://news0.bench.invalid/articles/single",
            title: "single status card",
            description: "bench article",
            kind: "link",
            author_name: "Bench Author",
            author_url: "",
            provider_name: "Bench News",
            provider_url: "https://news0.bench.invalid",
            html: "",
            width: 1_200,
            height: 630,
            image_url: Some("https://news0.bench.invalid/images/single.jpg"),
            image_description: "",
            embed_url: "",
            language: Some("en"),
            published_at: None,
            author_account_id: None,
        },
    )
    .await
    .unwrap();
    preview_card::attach(
        pool,
        single,
        card.id,
        "https://news0.bench.invalid/articles/single",
    )
    .await
    .unwrap();
    for i in 0..30_usize {
        favourite::create(pool, remotes[500 + i], single, None)
            .await
            .unwrap();
    }
    single
}

/// A 100-reply thread under a local root (the context bench).
async fn seed_thread(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    statuses: &mut SeededStatuses,
) -> i64 {
    let mut rng = Rng::new(12_000);
    let content = sentence(&mut rng, Some("thread root"));
    let root = status::create_local(
        pool,
        NewLocalStatus {
            language: Some("en"),
            ..NewLocalStatus::new(locals[0], &content, "public", None)
        },
    )
    .await
    .unwrap();
    statuses.all.push((root.id, locals[0]));
    let mut parent = root.id;
    for i in 0..100_u64 {
        let content = sentence(&mut rng, None);
        let author = if i % 2 == 0 {
            remotes[(600 + i) as usize]
        } else {
            locals[(i % LOCAL_ACCOUNTS) as usize]
        };
        parent = if i % 2 == 0 {
            let domain = domain_of(600 + i);
            status::upsert_remote(
                pool,
                NewRemoteStatus {
                    title: None,
                    object_type: None,
                    external_url: None,
                    uri: &format!("https://{domain}/statuses/bench-thread-{i}"),
                    account_id: author,
                    content: &content,
                    created_at: OffsetDateTime::now_utc(),
                    visibility: "public",
                    in_reply_to_id: Some(parent),
                    in_reply_to_uri: None,
                    spoiler_text: "",
                    sensitive: false,
                    language: Some("en"),
                    url: None,
                    quote_approval_policy: 0,
                },
            )
            .await
            .unwrap()
            .id
        } else {
            status::create_local(
                pool,
                NewLocalStatus {
                    language: Some("en"),
                    ..NewLocalStatus::new(author, &content, "public", Some(parent))
                },
            )
            .await
            .unwrap()
            .id
        };
        statuses.all.push((parent, author));
    }
    root.id
}

/// Cached translations, set-based.
///
/// No benchmark reads these — the bench state configures no translation
/// backend, so `translate_status` refuses before it reaches the table — but the
/// cache is swept by the maintenance worker and capped by two instance
/// settings, and an empty table means neither has ever had a row to consider.
///
/// The `source_hash` is computed the way `translation::fragments_hash` does it
/// (SHA-256 over big-endian length-prefixed fragments) so these rows are real
/// cache entries rather than permanent misses. The slice is restricted to
/// statuses whose fragment list is exactly `[content]`: no title, no CW, no
/// poll options, no media descriptions.
async fn seed_translations(pool: &PgPool) {
    sqlx::query(
        r"
        INSERT INTO status_translations
            (status_id, target_language, source_hash, provider,
             detected_source_language, title, content, spoiler_text,
             created_at, last_used_at)
        SELECT s.id,
               CASE WHEN s.language = 'en' THEN 'de' ELSE 'en' END,
               sha256(int8send(octet_length(s.content)::bigint) || convert_to(s.content, 'UTF8')),
               'bench-mt',
               s.language,
               '',
               s.content,
               '',
               now() - make_interval(secs => (o.ord % 1209600)::double precision),
               now() - make_interval(secs => (o.ord % 43200)::double precision)
        FROM statuses s
        JOIN bench_status_ord o ON o.status_id = s.id
        WHERE s.reblog_of_id IS NULL
          AND s.language IS NOT NULL
          AND s.language <> 'sv'
          AND s.title IS NULL
          AND s.spoiler_text = ''
          AND s.content <> ''
          AND NOT EXISTS (SELECT 1 FROM media_attachments m WHERE m.status_id = s.id)
          AND NOT EXISTS (SELECT 1 FROM polls p WHERE p.status_id = s.id)
          AND o.ord % 91 = 5
        ORDER BY o.ord
        LIMIT $1
        ON CONFLICT (status_id, target_language) DO NOTHING
        ",
    )
    .bind(TRANSLATED_STATUSES)
    .execute(pool)
    .await
    .unwrap();
}

/// Soft-deleted local originals, kept as stubs because a reply still points at
/// them, plus the tombstones a hard delete leaves behind.
///
/// `deleted_at` was NULL on all 467,608 rows, so every `-- STUBFILTER`
/// predicate in the codebase — 30-odd of them, on every timeline, profile,
/// list, tag and thread query — filtered exactly nothing, `statuses_deleted_at_idx`
/// was an empty partial index, and the renderer's placeholder branch was
/// unreachable.
///
/// A real stub is blanked and has had every child row stripped, so this mirrors
/// `status::stub_local` rather than just stamping the column.
async fn seed_stubs(pool: &PgPool) {
    let stubbed: Vec<i64> = sqlx::query_scalar(
        r"
        WITH picked AS (
            SELECT s.id
            FROM statuses s
            JOIN bench_status_ord o ON o.status_id = s.id
            WHERE s.uri IS NULL AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
              AND EXISTS (SELECT 1 FROM statuses r
                          WHERE r.in_reply_to_id = s.id AND r.deleted_at IS NULL)
            ORDER BY o.ord
            LIMIT $1
        )
        UPDATE statuses s
           SET content = '', spoiler_text = '', text = '', title = NULL,
               sensitive = false, language = NULL,
               deleted_at = s.created_at + interval '1 day'
          FROM picked
         WHERE s.id = picked.id
        RETURNING s.id
        ",
    )
    .bind(STUB_STATUSES)
    .fetch_all(pool)
    .await
    .unwrap();

    // Everything `strip_stub_children` removes. A stub that kept its media,
    // tags and favourites is not the row the STUBFILTER benches should measure.
    for sql in [
        "DELETE FROM statuses WHERE reblog_of_id = ANY($1)",
        "DELETE FROM bookmarks WHERE status_id = ANY($1)",
        "DELETE FROM favourites WHERE status_id = ANY($1)",
        "DELETE FROM media_attachments WHERE status_id = ANY($1)",
        "DELETE FROM notifications WHERE status_id = ANY($1)",
        "DELETE FROM polls WHERE status_id = ANY($1)",
        "DELETE FROM preview_cards_statuses WHERE status_id = ANY($1)",
        "DELETE FROM status_dislikes WHERE status_id = ANY($1)",
        "DELETE FROM status_edits WHERE status_id = ANY($1)",
        "DELETE FROM status_events WHERE status_id = ANY($1)",
        "DELETE FROM status_pins WHERE status_id = ANY($1)",
        "DELETE FROM status_reactions WHERE status_id = ANY($1)",
        "DELETE FROM status_tags WHERE status_id = ANY($1)",
        "DELETE FROM tagged_objects WHERE status_id = ANY($1)",
        "DELETE FROM status_translations WHERE status_id = ANY($1)",
        "DELETE FROM quotes WHERE status_id = ANY($1)",
    ] {
        sqlx::query(sql)
            .bind(&stubbed)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("strip stub children `{sql}`: {err}"));
    }

    // Tombstones outlive the status they name, which is why the table has no
    // foreign key: these ids are gone.
    sqlx::query(
        r"
        INSERT INTO status_tombstones (status_id, account_id, uri, deleted_at)
        SELECT (SELECT max(id) FROM statuses) + g,
               (SELECT id FROM accounts WHERE domain IS NULL ORDER BY id LIMIT 1),
               'https://plamenu.test/users/bench_local0/statuses/tomb-' || g,
               now() - make_interval(days => (g % 20)::int)
        FROM generate_series(1, $1) g
        ON CONFLICT (status_id) DO NOTHING
        ",
    )
    .bind(STUB_STATUSES)
    .execute(pool)
    .await
    .unwrap();
}

/// FEP-7aa9 collections: featured sets owned by accounts, some of them tagged
/// onto statuses.
///
/// `collections`, `collection_items` and `tagged_objects` were all empty, so
/// the profile page's `owned_by` read returned nothing and
/// `entities::tagged_collections_map` — which runs its batch query on every
/// rendered page — never entered its per-collection loop.
///
/// The tagged slice is deliberately confined to the group: that loop is **not**
/// deduplicated and costs two to three queries per (status, collection) pair,
/// so it is seeded where the density is known and the cost lands on one
/// measured surface rather than scattered unpredictably across every timeline.
async fn seed_collections(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    personas: &Personas,
    group_account: i64,
) {
    let mut rng = Rng::new(24_000);
    let mut community: Vec<i64> = Vec::new();
    for c in 0..COLLECTIONS {
        // Popular first, so the benched profile page has a full set to render.
        let (owner, local) = match c {
            0..=5 => (personas.popular, true),
            6..=8 => (group_account, true),
            _ if c % 3 == 0 => (locals[(c * 13) as usize % LOCAL_ACCOUNTS as usize], true),
            _ => (remotes[((c * 977) % REMOTE_ACCOUNTS) as usize], false),
        };
        let tag_id = rng
            .chance(30)
            .then_some(())
            .map(|()| format!("{}{}", WORDS[(c % 24) as usize], c * 111));
        let tag_id = match tag_id {
            Some(name) => Some(tag::ensure(pool, &name).await.unwrap()),
            None => None,
        };
        let uri = (!local).then(|| format!("https://d{c}.bench.invalid/collections/{c}"));
        let created = collection::create(
            pool,
            NewCollection {
                account_id: owner,
                name: &headline(&mut rng),
                description: "bench collection",
                language: Some("en"),
                sensitive: false,
                discoverable: rng.chance(70),
                local,
                tag_id,
                uri: uri.as_deref(),
                url: uri.as_deref(),
                original_number_of_items: (!local).then(|| 5 + rng.below(40) as i32),
            },
        )
        .await
        .unwrap();
        // Distinct members: the arbiter is (collection, account) and a repeat
        // would silently insert nothing.
        for m in 0..(3 + rng.below(12)) {
            let member = remotes[((c * 7919 + m * 331) % REMOTE_ACCOUNTS) as usize];
            collection::add_item(
                pool,
                NewCollectionItem {
                    item_id: id::next(),
                    collection_id: created.id,
                    account_id: Some(member),
                    state: if m % 7 == 0 { "pending" } else { "accepted" },
                    uri: None,
                    object_uri: None,
                    activity_uri: None,
                    approval_uri: None,
                },
            )
            .await
            .unwrap();
        }
        if (6..=8).contains(&c) {
            community.push(created.id);
        }
        // Being added to someone's collection is a notification kind that had
        // no rows, so `render_notifications`' collection branch never ran.
        if c % 4 == 0 && owner != personas.dense {
            notification::create_for_collection(
                pool,
                personas.dense,
                owner,
                "added_to_collection",
                created.id,
            )
            .await
            .unwrap();
        }
    }

    // ~5% of the community's submissions are filed into one of its collections.
    let tagged: Vec<i64> = sqlx::query_scalar(
        "SELECT s.id FROM statuses s
         JOIN bench_status_ord o ON o.status_id = s.id
         JOIN status_mentions m ON m.status_id = s.id AND m.account_id = $1
         WHERE s.deleted_at IS NULL AND o.ord % 20 = 3
         ORDER BY o.ord",
    )
    .bind(group_account)
    .fetch_all(pool)
    .await
    .unwrap();
    for (n, status_id) in tagged.iter().enumerate() {
        let collection_id = community[n % community.len()];
        tagged_object::add(
            pool,
            *status_id,
            collection_id,
            &format!("https://plamenu.test/collections/{collection_id}/items/{status_id}"),
        )
        .await
        .unwrap();
    }
}

/// Accepted relays.
///
/// Every public activity asks `relay::enabled_inboxes` for its extra fan-out
/// targets and got an empty list, so the delivery widths the write benches
/// assert have never included a relay. Three is enough to widen every
/// `Create`/`Announce` group uniformly (which keeps the fan-out invariant
/// meaningful) without swamping the follower fan-out it is measuring.
async fn seed_relays(pool: &PgPool) {
    for r in 0..RELAYS {
        // Dedicated hosts: `fan_out_inboxes` de-duplicates against the follower
        // inboxes, so a relay sharing a host with a seeded follower would add
        // nothing at all.
        let inbox = format!("https://relay{r}.bench.invalid/inbox");
        let actor = format!("https://relay{r}.bench.invalid/actor");
        let relay = relay::create(pool, &inbox, Some(&actor))
            .await
            .unwrap()
            .expect("relay inbox is unique");
        let follow_id = format!("https://plamenu.test/payloads/relay-{r}");
        relay::mark_pending(pool, relay.id, &follow_id)
            .await
            .unwrap();
        relay::resolve_follow_response(pool, &follow_id, true)
            .await
            .unwrap();
        // A fortnight of forwarding history for the admin/landing stats.
        sqlx::query(
            "INSERT INTO relay_daily_activities (relay_id, day, count)
             SELECT $1, (now() AT TIME ZONE 'UTC')::date - g, 40 + (g * 17) % 300
             FROM generate_series(0, 13) g
             ON CONFLICT (relay_id, day) DO NOTHING",
        )
        .bind(relay.id)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// Local custom emojis, and the shortcodes that make them resolve.
///
/// `emoji_map` buckets shortcodes by the *author's* domain and looks local
/// authors up with a `domain IS NULL` key. The seed only ever put `:bemoji:`
/// codes in remote-authored statuses, so that arm — and `custom_emoji::listed`,
/// which backs `GET /api/v1/custom_emojis` — read an empty set.
async fn seed_local_emojis(pool: &PgPool) {
    for e in 0..LOCAL_EMOJIS {
        let category = (e % 5 != 0).then(|| format!("bench cat {}", e % 12));
        custom_emoji::create_local(
            pool,
            &format!("lemoji{e}"),
            &format!("{e}.png"),
            "image/png",
            4_096 + i64::from(u32::try_from(e % 20_000).unwrap_or_default()),
            category.as_deref(),
        )
        .await
        .unwrap();
    }
    // A slice hidden from the picker, so `listed`'s partial filter rejects
    // something.
    sqlx::raw_sql(
        "UPDATE custom_emojis SET visible_in_picker = false
         WHERE domain IS NULL AND (id % 11) = 0",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::raw_sql(
        "UPDATE custom_emojis SET disabled = true WHERE domain IS NULL AND (id % 37) = 0",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Conversation bookkeeping for the bulk statuses, set-based.
///
/// Mirrors what `conversations::ensure_conversation` does at ingest for
/// every stored status (staging: `status_conversations` ≈ statuses): one
/// conversation per thread root — the root status' id doubles as the
/// conversation id, which is collision-free (status ids are unique) and lets
/// the recursive walk map replies without a join back through ids — and a
/// `status_conversations` row for every non-reblog status. Direct statuses
/// are excluded: the persona DMs are seeded later through the real fns so
/// their `account_conversations` fan-out stays honest.
async fn seed_conversations(pool: &PgPool) {
    sqlx::raw_sql(
        r"
        INSERT INTO conversations (id, created_at, root_status_id)
        SELECT s.id, s.created_at, s.id
        FROM statuses s
        WHERE s.in_reply_to_id IS NULL AND s.reblog_of_id IS NULL AND s.visibility <> 'direct'
          AND NOT EXISTS (SELECT 1 FROM status_conversations sc WHERE sc.status_id = s.id)
        ",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::raw_sql(
        r"
        WITH RECURSIVE thread AS (
            SELECT s.id, s.id AS root FROM statuses s
            WHERE s.in_reply_to_id IS NULL AND s.reblog_of_id IS NULL
              AND s.visibility <> 'direct'
          UNION ALL
            SELECT s.id, t.root FROM statuses s
            JOIN thread t ON s.in_reply_to_id = t.id
            WHERE s.reblog_of_id IS NULL AND s.visibility <> 'direct'
        )
        INSERT INTO status_conversations (status_id, conversation_id)
        SELECT t.id, t.root
        FROM thread t
        JOIN conversations c ON c.id = t.root
        ON CONFLICT (status_id) DO NOTHING
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// What the graph pass produced that later passes need.
struct SeededGraph {
    /// `bench_dense`'s 400-member list (the list-timeline bench).
    list_id: i64,
    /// Remote accounts `bench_dense` follows and neither blocks nor mutes —
    /// the only senders whose notifications survive its filtering policy and
    /// `sender_filtered`, i.e. the only ones the v2 notification bench can see.
    notify_senders: Vec<i64>,
}

/// The whole social/moderation graph: background follows, the intricate
/// persona setups, popular's and writer's follower crowds.
async fn seed_graph(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    locals: &Arc<Vec<i64>>,
    statuses: &Arc<SeededStatuses>,
    personas: &Personas,
) -> SeededGraph {
    let mut rng = Rng::new(9_000);
    let benchtag = tag::ensure(pool, "benchtag").await.unwrap();

    // --- background graph -------------------------------------------------
    for _ in 0..BACKGROUND_FOLLOWS {
        let follower = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
        let target = if rng.chance(20) {
            locals[rng.below(LOCAL_ACCOUNTS) as usize]
        } else {
            remotes[rng.below(REMOTE_ACCOUNTS) as usize]
        };
        if follower != target {
            follow::create(pool, follower, target, None).await.unwrap();
        }
    }

    // --- bench_sparse: the calibrated baseline persona (kept minimal) -----
    let sparse = personas.sparse;
    for &target in &remotes[..10] {
        follow::create(pool, sparse, target, None).await.unwrap();
    }
    tag::follow(pool, sparse, benchtag).await.unwrap();
    for &target in &remotes[300..305] {
        block::create(pool, sparse, target, None).await.unwrap();
    }
    for &target in &remotes[305..310] {
        mute::upsert(pool, sparse, target, true, None)
            .await
            .unwrap();
    }
    for (title, keyword) in [("bench filter a", "saffron"), ("bench filter b", "tundra")] {
        custom_filter::create(
            pool,
            sparse,
            title,
            "warn",
            &["home".to_owned(), "public".to_owned()],
            None,
            &[custom_filter::NewKeyword {
                keyword: keyword.to_owned(),
                whole_word: true,
            }],
        )
        .await
        .unwrap();
    }

    // --- bench_dense: 1,500 follows and every read-path knob turned on ----
    let dense = personas.dense;
    let mut dense_follows: Vec<i64> = Vec::new();
    for &target in &remotes[..PROLIFIC_ACCOUNTS as usize] {
        dense_follows.push(target);
    }
    // 1,200 spread across the mid tier and long tail.
    for i in 0..1_200_u64 {
        let target = remotes[((PROLIFIC_ACCOUNTS + i * 149) % REMOTE_ACCOUNTS) as usize];
        if !dense_follows.contains(&target) {
            dense_follows.push(target);
        }
    }
    for &target in &locals[..250] {
        dense_follows.push(target);
    }
    dense_follows.push(personas.popular);
    for &target in &dense_follows {
        follow::create(pool, dense, target, None).await.unwrap();
    }
    // Per-follow settings on subsets (M32): hidden boosts, notify bells,
    // language-restricted follows — each is a filter arm in the home query.
    for &target in dense_follows.iter().step_by(15).take(100) {
        follow::update_settings(pool, dense, target, Some(false), None, None, None)
            .await
            .unwrap();
    }
    for &target in dense_follows.iter().skip(7).step_by(23).take(60) {
        follow::update_settings(pool, dense, target, None, None, Some(true), None)
            .await
            .unwrap();
    }
    let en = ["en".to_owned()];
    for &target in dense_follows.iter().skip(11).step_by(19).take(60) {
        follow::update_settings(pool, dense, target, None, None, None, Some(&en))
            .await
            .unwrap();
    }
    // Replies turned off on a slice of the follow set: the only arm of
    // the home filter that is per-row rather than per-author, so the dense
    // scenario has to carry some or the tripwire stops watching it.
    for &target in dense_follows.iter().skip(5).step_by(17).take(80) {
        follow::update_settings(pool, dense, target, None, Some(false), None, None)
            .await
            .unwrap();
    }
    // Outgoing pending follows toward locked accounts.
    for k in 0..20_u64 {
        let idx = (617 * (k + 1)) % REMOTE_ACCOUNTS;
        follow::create_outgoing(
            pool,
            dense,
            remotes[idx as usize],
            &format!("https://plamenu.test/follows/bench-pending-{k}"),
        )
        .await
        .unwrap();
    }
    // Blocks both ways, mutes (half silencing notifications, some expiring),
    // personal domain blocks.
    for &target in &remotes[2_000..2_200] {
        block::create(pool, dense, target, None).await.unwrap();
    }
    for &blocker in &remotes[2_200..2_500] {
        block::create(pool, blocker, dense, None).await.unwrap();
    }
    for (k, &target) in remotes[3_000..3_150].iter().enumerate() {
        let expires = (k % 5 == 0).then(|| OffsetDateTime::now_utc() + Duration::seconds(86_400));
        mute::upsert(pool, dense, target, k % 2 == 0, expires)
            .await
            .unwrap();
    }
    for d in 200..240_u64 {
        account_domain_block::create(pool, dense, &format!("d{d}.bench.invalid"))
            .await
            .unwrap();
    }
    // 25 keyword filters over home+public. Warn filters carry a real
    // content word (so filter matching does real work and `filtered` arrays
    // render); hide filters use never-matching terms — bare words would
    // silently drop ~half the feed and turn every page into a deep scan.
    for f in 0..25_u64 {
        let (action, keyword) = if f % 5 == 0 {
            ("hide", format!("hideterm{f}"))
        } else {
            ("warn", WORDS[(f % 24) as usize].to_owned())
        };
        custom_filter::create(
            pool,
            dense,
            &format!("dense filter {f}"),
            action,
            &["home".to_owned(), "public".to_owned()],
            None,
            &[
                custom_filter::NewKeyword {
                    keyword,
                    whole_word: true,
                },
                custom_filter::NewKeyword {
                    keyword: format!("filterterm{f}"),
                    whole_word: false,
                },
            ],
        )
        .await
        .unwrap();
    }
    // Lists: one big (the list-timeline bench), one exclusive, six small.
    let big_list = list::create(pool, dense, "bench big list", "list", false)
        .await
        .unwrap();
    let members: Vec<i64> = dense_follows.iter().copied().take(400).collect();
    list::add_members(pool, big_list.id, dense, &members)
        .await
        .unwrap()
        .unwrap();
    let exclusive = list::create(pool, dense, "bench exclusive", "list", true)
        .await
        .unwrap();
    let ex_members: Vec<i64> = dense_follows.iter().copied().skip(400).take(30).collect();
    list::add_members(pool, exclusive.id, dense, &ex_members)
        .await
        .unwrap()
        .unwrap();
    for l in 0..6_u64 {
        let small = list::create(pool, dense, &format!("bench list {l}"), "list", false)
            .await
            .unwrap();
        let start = 450 + (l as usize) * 40;
        let small_members: Vec<i64> = dense_follows.iter().copied().skip(start).take(25).collect();
        list::add_members(pool, small.id, dense, &small_members)
            .await
            .unwrap()
            .unwrap();
    }
    // 15 followed tags (arm B of the home query fetches per followed tag).
    // Names follow the bulk vocabulary so the follows point at tags that
    // actually carry statuses.
    for t in 0..14_u64 {
        let i = t * 111;
        let tag_id = tag::ensure(pool, &format!("{}{i}", WORDS[(i % 24) as usize]))
            .await
            .unwrap();
        tag::follow(pool, dense, tag_id).await.unwrap();
    }
    tag::follow(pool, dense, benchtag).await.unwrap();
    // Bookmarks and given favourites (the bookmarks/favourites page
    // benches). Strided sampling — no duplicate (account, status) pairs.
    for k in 0..2_000_usize {
        let (target, _) = statuses.all[(k * 613) % statuses.all.len()];
        bookmark::create(pool, dense, target).await.unwrap();
    }
    for k in 0..3_000_usize {
        let (target, _) = statuses.all[(k * 431 + 7) % statuses.all.len()];
        favourite::create(pool, dense, target, None).await.unwrap();
    }

    // --- bench_popular: the followed one -----------------------------------
    let popular = personas.popular;
    for i in 0..POPULAR_FOLLOWERS {
        let follower = remotes[((i * 15 + 3) % REMOTE_ACCOUNTS) as usize];
        follow::create(pool, follower, popular, None).await.unwrap();
    }
    for &follower in locals.iter() {
        follow::create(pool, follower, popular, None).await.unwrap();
    }
    for i in 0..300_u64 {
        follow::create(
            pool,
            popular,
            remotes[(i * 31) as usize % remotes.len()],
            None,
        )
        .await
        .unwrap();
    }

    // --- bench_writer: 5k followers across the whole domain spread --------
    for i in 0..WRITER_FOLLOWERS {
        let follower = remotes[((i * 36 + 1) % REMOTE_ACCOUNTS) as usize];
        follow::create(pool, follower, personas.writer, None)
            .await
            .unwrap();
    }

    // Senders whose notifications dense will actually see. Its policy filters
    // everyone it does not follow, and `sender_filtered` drops everyone it
    // blocks or mutes with notifications hidden — the seed drew notification
    // senders uniformly from all 61,000 remote accounts against 1,501 follows,
    // so ~97.5% of every filterable kind was stored `filtered = TRUE` and
    // invisible to `/api/v2/notifications`.
    let blocked = 2_000..2_500_usize;
    let muted = 3_000..3_150_usize;
    let excluded: std::collections::HashSet<i64> = remotes[blocked]
        .iter()
        .chain(&remotes[muted])
        .copied()
        .collect();
    let notify_senders: Vec<i64> = dense_follows
        .iter()
        .copied()
        .filter(|id| !excluded.contains(id) && *id != personas.dense)
        .collect();

    SeededGraph {
        list_id: big_list.id,
        notify_senders,
    }
}

/// Popular's profile furniture: pinned statuses and featured tags (the
/// profile/AP benches render them; `own` must be seeded first).
async fn seed_popular_profile(pool: &PgPool, popular: i64, own: &[i64]) {
    for &status_id in own.iter().rev().take(5) {
        pin::create(pool, popular, status_id).await.unwrap();
    }
    for t in 0..4_u64 {
        let i = t * 111;
        let tag_id = tag::ensure(pool, &format!("{}{i}", WORDS[(i % 24) as usize]))
            .await
            .unwrap();
        featured_tag::feature(pool, popular, tag_id).await.unwrap();
    }
}

/// Instance-level moderation: silenced/suspended remote accounts and domain
/// blocks, so `account_hidden`/`instance_domain_allowed`/`account_silenced`
/// filter real rows in every feed query. Runs AFTER the set-based usage
/// mirrors (which skip the silenced-author arm for exactly this reason).
async fn seed_moderation(pool: &PgPool, remotes: &Arc<Vec<i64>>) {
    // Mid-tail domains with real content: tier-3 hosts own ~32 accounts each.
    for d in 400..425_u64 {
        instance_policy::create_domain_block(
            pool,
            NewDomainBlock {
                domain: &format!("d{d}.bench.invalid"),
                severity: "silence",
                reject_media: false,
                reject_reports: false,
                private_comment: None,
                public_comment: Some("bench silence"),
                obfuscate: false,
            },
        )
        .await
        .unwrap();
    }
    for d in 425..450_u64 {
        instance_policy::create_domain_block(
            pool,
            NewDomainBlock {
                domain: &format!("d{d}.bench.invalid"),
                severity: "suspend",
                reject_media: false,
                reject_reports: false,
                private_comment: None,
                public_comment: Some("bench suspend"),
                obfuscate: false,
            },
        )
        .await
        .unwrap();
    }
    for d in 450..460_u64 {
        instance_policy::create_domain_block(
            pool,
            NewDomainBlock {
                domain: &format!("d{d}.bench.invalid"),
                severity: "noop",
                reject_media: true,
                reject_reports: false,
                private_comment: None,
                public_comment: None,
                obfuscate: false,
            },
        )
        .await
        .unwrap();
    }
    // Account-level moderation over the long tail (never the prolific tier —
    // the sparse persona's whole feed comes from it).
    for k in 0..1_200_u64 {
        account::silence(pool, remotes[(20_000 + k * 131) as usize % remotes.len()])
            .await
            .unwrap();
    }
    for k in 0..600_u64 {
        account::suspend(
            pool,
            remotes[(21_000 + k * 263) as usize % remotes.len()],
            "local",
        )
        .await
        .unwrap();
    }
}

/// Direct-message conversations toward the read personas (the conversations
/// bench). Returns the sparse persona's DM status ids.
async fn seed_dms(pool: &PgPool, remotes: &Arc<Vec<i64>>, sparse: i64, dense: i64) -> Vec<i64> {
    let mut rng = Rng::new(10_000);
    let mut sparse_ids = Vec::new();
    for (persona, base, count) in [(sparse, 400_u64, 30_u64), (dense, 460, 20)] {
        for k in 0..count {
            let sender = remotes[(base + k) as usize];
            let domain = domain_of(base + k);
            let mut previous = None;
            for m in 0..2_u64 {
                let content = sentence(&mut rng, Some("@persona")); // DM body
                let status = status::upsert_remote(
                    pool,
                    NewRemoteStatus {
                        title: None,
                        object_type: None,
                        external_url: None,
                        uri: &format!("https://{domain}/statuses/bench-dm-{persona}-{k}-{m}"),
                        account_id: sender,
                        content: &content,
                        created_at: past_timestamp(&mut rng),
                        visibility: "direct",
                        in_reply_to_id: previous,
                        in_reply_to_uri: None,
                        spoiler_text: "",
                        sensitive: false,
                        language: Some("en"),
                        url: None,
                        quote_approval_policy: 0,
                    },
                )
                .await
                .unwrap();
                mention::attach(pool, status.id, persona, false)
                    .await
                    .unwrap();
                let conversation_id = conversation::ensure_for_status(
                    pool,
                    &conversation::EnsureConversation {
                        status_id: status.id,
                        account_id: status.account_id,
                        in_reply_to_id: previous,
                        is_reply: previous.is_some(),
                        refs: conversation::ContextRefs::default(),
                    },
                )
                .await
                .unwrap();
                conversation::add_status(
                    pool,
                    conversation::AddStatus {
                        account_id: persona,
                        conversation_id,
                        participant_account_ids: &[sender],
                        status_id: status.id,
                        sender_id: sender,
                    },
                )
                .await
                .unwrap();
                if persona == sparse {
                    sparse_ids.push(status.id);
                }
                previous = Some(status.id);
            }
        }
    }
    sparse_ids
}

/// Notification inboxes: sparse keeps its calibrated 1,000; dense gets 5,000
/// under a filtering policy (so the v2 grouped family and the requests
/// machinery both have real rows), heavy on groupable kinds.
async fn seed_notifications(
    pool: &PgPool,
    remotes: &Arc<Vec<i64>>,
    statuses: &Arc<SeededStatuses>,
    own: &OwnStatuses,
    personas: &Personas,
    graph: &SeededGraph,
) {
    let mut rng = Rng::new(11_000);
    let mention_sparse = &statuses.mention_sparse;
    for i in 0..1_000_u64 {
        let from = remotes[rng.below(REMOTE_ACCOUNTS) as usize];
        let (kind, status_id) = match i % 4 {
            0 => (
                "mention",
                Some(mention_sparse[(i / 4) as usize % mention_sparse.len()]),
            ),
            1 => (
                "favourite",
                Some(own.sparse[(i / 4) as usize % own.sparse.len()]),
            ),
            2 => (
                "reblog",
                Some(own.sparse[(i / 4) as usize % own.sparse.len()]),
            ),
            _ => ("follow", None),
        };
        notification::create(pool, personas.sparse, from, kind, status_id)
            .await
            .unwrap();
    }

    // Dense: policy first, so notifications from strangers land filtered —
    // exactly what the real ingest path produces under this policy.
    let dense = personas.dense;
    notification_policy::upsert(
        pool,
        dense,
        Policy {
            for_not_following: Disposition::Filter,
            for_not_followers: Disposition::Accept,
            for_new_accounts: Disposition::Accept,
            for_private_mentions: Disposition::Accept,
            for_limited_accounts: Disposition::Filter,
            for_bots: Disposition::Accept,
        },
    )
    .await
    .unwrap();
    let mention_dense = &statuses.mention_dense;

    // The ungroupable kinds go FIRST now. `/api/v2/notifications` starts at the
    // newest row and walks until it has 40 distinct group keys, and the seed's
    // last writes used to be 200 emoji reactions and 30 stranger mentions —
    // neither of which is in `GROUPABLE_KINDS`, so the benched first page was
    // 40 singleton groups and the recursive walk's skip-ahead, the per-group
    // aggregates and the 8-sender sample hydration were never executed. The
    // dataset had 101 real groups averaging 34.65 members; they were all below
    // the fold (B8).
    for i in 0..200_u64 {
        let from = remotes[(6_000 + i) as usize];
        notification::create_reaction(
            pool,
            dense,
            from,
            own.dense[i as usize % own.dense.len()],
            "🎉",
        )
        .await
        .unwrap();
    }
    // Notification requests for stranger mention senders — deliberately from
    // accounts dense does *not* follow, so the policy files them.
    for k in 0..30_u64 {
        let from = remotes[(7_000 + k * 11) as usize];
        let status_id = mention_dense[k as usize % mention_dense.len()];
        notification::create(pool, dense, from, "mention", Some(status_id))
            .await
            .unwrap();
        notification_request::record_filtered(pool, dense, from, "mention", Some(status_id))
            .await
            .unwrap();
    }

    // The groupable bulk, last, and from accounts dense follows. Senders used
    // to be drawn from all 61,000 remote accounts while dense follows 1,501, so
    // the `Filter` policy stored ~97.5% of every filterable kind with
    // `filtered = TRUE` — invisible to the v2 endpoint, which does not ask for
    // filtered rows. What the bench saw instead was the `update` and
    // `pleroma:emoji_reaction` rows, the two kinds the policy does not filter.
    let senders = &graph.notify_senders;
    assert!(
        senders.len() >= 100,
        "dense has only {} unblocked followed senders; its notifications would \
         all be filtered",
        senders.len()
    );
    for i in 0..5_000_u64 {
        let from = senders[(i * 7) as usize % senders.len()];
        let (kind, status_id) = match i % 10 {
            0 | 1 => (
                "mention",
                Some(mention_dense[(i / 10) as usize % mention_dense.len()]),
            ),
            2..=5 => (
                "favourite",
                Some(own.dense[(i / 10) as usize % own.dense.len()]),
            ),
            6 | 7 => (
                "reblog",
                Some(own.dense[(i / 10) as usize % own.dense.len()]),
            ),
            8 => ("follow", None),
            _ => (
                "update",
                Some(statuses.all[(i * 37) as usize % statuses.all.len()].0),
            ),
        };
        notification::create(pool, dense, from, kind, status_id)
            .await
            .unwrap();
    }
    // A last handful of stranger favourites, so the filtered arm still has
    // rows and `include_filtered` reads are not measuring an empty predicate.
    for i in 0..120_u64 {
        let from = remotes[((8_000 + i * 37) % REMOTE_ACCOUNTS) as usize];
        notification::create(
            pool,
            dense,
            from,
            "favourite",
            Some(own.dense[i as usize % own.dense.len()]),
        )
        .await
        .unwrap();
    }
}

/// Read markers for the read personas (rendered by the web UI and API).
async fn seed_markers(pool: &PgPool, personas: &Personas, own: &OwnStatuses) {
    for (account_id, last) in [
        (personas.sparse, own.sparse[own.sparse.len() / 2]),
        (personas.dense, own.dense[own.dense.len() / 2]),
    ] {
        let user = user::find_by_account_id(pool, account_id)
            .await
            .unwrap()
            .unwrap();
        marker::upsert(pool, user.id, "home", Some(last))
            .await
            .unwrap();
        marker::upsert(pool, user.id, "notifications", Some(last))
            .await
            .unwrap();
    }
}

/// Fetch-failure backoff rows (staging x3: ~29k), set-based — the shape the
/// `remote_fetch_failure::should_attempt` probes run against in production.
async fn seed_fetch_failures(pool: &PgPool) {
    sqlx::query(
        r"
        INSERT INTO remote_fetch_failures (scope, failure_key, attempts, retry_at, last_error)
        SELECT CASE WHEN g % 20 = 0 THEN 'host' ELSE 'resource' END,
               CASE WHEN g % 20 = 0 THEN 'dead' || g || '.bench.invalid'
                    ELSE 'https://dead' || (g % 900) || '.bench.invalid/objects/' || g END,
               1 + (g % 7)::int,
               now() + (g % 48) * interval '1 hour',
               'bench seeded failure'
        FROM generate_series(1, $1) g
        ON CONFLICT DO NOTHING
        ",
    )
    .bind(FETCH_FAILURES)
    .execute(pool)
    .await
    .unwrap();
}

/// One attachment on `status_id`.
///
/// `slot` is part of the URL: `create_remote`'s conflict arbiter is
/// `(status_id, remote_url)`, and the old builder derived the URL from the
/// status id alone — so the "two attachments" case silently collapsed into one
/// on every multi-attachment status.
async fn attach_remote_media(pool: &PgPool, status_id: i64, author: i64, slot: u64, rng: &mut Rng) {
    let shape = MediaShape::pick(rng);
    let base = format!("https://media.bench.invalid/{status_id}-{slot}");
    let blurhash = "LEHV6nWB2yk8pyo0adR*.7kCMdnj";
    let video_url = format!("{base}.mp4");
    let thumb_url = format!("{base}.thumb.jpg");
    let new = match shape {
        MediaShape::Image => NewRemoteMedia {
            account_id: author,
            status_id,
            remote_url: &format!("{base}.jpg"),
            content_type: "image/jpeg",
            description: Some("bench image"),
            blurhash: Some(blurhash),
            width: Some(1_280),
            height: Some(720),
            ..Default::default()
        },
        MediaShape::Video | MediaShape::Gifv => {
            // A remote video is fetched only when someone presses play (the
            // lane), which is what `download_on_demand` means: the row is
            // `complete` with no file and is *not* a redownload candidate.
            let gifv = matches!(shape, MediaShape::Gifv);
            NewRemoteMedia {
                account_id: author,
                status_id,
                remote_url: &video_url,
                content_type: "video/mp4",
                description: Some("bench video"),
                blurhash: Some(blurhash),
                width: Some(1_280),
                height: Some(720),
                thumbnail_remote_url: Some(&thumb_url),
                duration: Some(if gifv {
                    2.0 + f64::from(u32::try_from(rng.below(4)).unwrap_or_default())
                } else {
                    30.0 + f64::from(u32::try_from(rng.below(600)).unwrap_or_default())
                }),
                download_on_demand: true,
                ..Default::default()
            }
        }
        MediaShape::Hls => {
            // A handful are live rather than recorded. Kept small on purpose:
            // a `waiting`/`live` attachment makes every render of its status
            // attempt a throttled origin refresh.
            let live = match rng.below(1000) {
                0..=2 => Some("live"),
                3..=5 => Some("waiting"),
                6..=19 => Some("ended"),
                _ => None,
            };
            let master = format!("{base}/master.m3u8");
            let urls: Vec<String> = LADDER
                .iter()
                .map(|(height, width, _)| match width {
                    Some(_) => format!("{base}-{height}.mp4"),
                    None => format!("{base}-audio.mp4"),
                })
                .collect();
            let ladder: Vec<NewRendition<'_>> = LADDER
                .iter()
                .zip(&urls)
                .map(|(&(height, width, size), url)| NewRendition {
                    height,
                    width,
                    frame_rate: width.map(|_| 30),
                    size_bytes: Some(size),
                    origin_url: url.as_str(),
                    is_audio: width.is_none(),
                })
                .collect();
            let new = NewRemoteMedia {
                account_id: author,
                status_id,
                remote_url: &video_url,
                content_type: "application/x-mpegURL",
                description: Some("bench stream"),
                blurhash: Some(blurhash),
                width: Some(1_920),
                height: Some(1_080),
                thumbnail_remote_url: Some(&thumb_url),
                duration: Some(
                    120.0 + f64::from(u32::try_from(rng.below(3_000)).unwrap_or_default()),
                ),
                download_on_demand: true,
                hls_master_url: Some(&master),
                renditions: ladder,
                live_state: live,
                live_permanent: live == Some("ended"),
                ..Default::default()
            };
            media::create_remote(pool, new).await.unwrap();
            return;
        }
        MediaShape::Audio => NewRemoteMedia {
            account_id: author,
            status_id,
            remote_url: &format!("{base}.mp3"),
            content_type: "audio/mpeg",
            description: Some("bench audio"),
            duration: Some(60.0 + f64::from(u32::try_from(rng.below(2_400)).unwrap_or_default())),
            download_on_demand: true,
            ..Default::default()
        },
    };
    media::create_remote(pool, new).await.unwrap();
}

/// The rendition ladder a `PeerTube`-shaped origin advertises: three video
/// heights plus the audio-only track (`height = 0`, `is_audio`).
/// SEED-SHAPE-UNCHANGED: only the documentation markup above changed.
const LADDER: [(i32, Option<i32>, i64); 4] = [
    (1_080, Some(1_920), 24_000_000),
    (720, Some(1_280), 11_000_000),
    (480, Some(854), 5_000_000),
    (0, None, 900_000),
];
