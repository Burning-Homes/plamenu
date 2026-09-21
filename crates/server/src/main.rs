use std::sync::Arc;

use clap::{Parser, Subcommand};
use plamenu::federation::HttpFederation;
use plamenu::{AppState, Config, actions, build_router, delivery, migration, poll_expiry};
use plamenu_ap::acct::Acct;
use plamenu_ap::keys;
use plamenu_db::account::{self, NewLocalAccount};
use plamenu_federation::{FederationClient, RequestSigner};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "plamenu",
    version = plamenu::VERSION,
    about = "A fast, small ActivityPub server"
)]
struct Cli {
    /// TOML configuration file used by every command except `config generate`.
    #[arg(long, global = true, default_value = "plamenu.toml")]
    config: std::path::PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create Plamenu configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Run the HTTP server (includes the delivery worker).
    Serve,
    /// Manage local accounts.
    #[command(subcommand)]
    Account(AccountCommand),
    /// Manage local groups.
    #[command(subcommand)]
    Group(GroupCommand),
    /// Post a status as a local account (enqueued for follower delivery).
    Post { username: String, text: String },
    /// Follow a remote account (`user@domain`) as a local account.
    Follow { username: String, target: String },
    /// Manage this instance's custom emoji.
    #[command(subcommand)]
    Emoji(EmojiCommand),
    /// Inspect moderation roles.
    #[command(subcommand)]
    Role(RoleCommand),
    /// Manage this instance's published rules (server policies).
    #[command(subcommand)]
    Rule(RuleCommand),
    /// Manage server announcements (shown to logged-in users).
    #[command(subcommand)]
    Announcement(AnnouncementCommand),
    /// Maintenance tasks for stored media.
    #[command(subcommand)]
    Media(MediaCommand),
    /// Read-only federation diagnostics (fetch objects, resolve handles,
    /// inspect the delivery queue and the reachability breaker).
    #[command(subcommand)]
    Federation(FederationCommand),
    /// Erase the server from the federation (Mastodon's `tootctl
    /// self-destruct`): broadcast account deletion notices to every known
    /// server, and serve 410 Gone while they go out. Irreversible; always
    /// asks for confirmation. Re-run to see the wind-down's progress.
    SelfDestruct,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Write a documented starter TOML file to the --config path.
    Generate {
        /// Replace an existing file instead of preserving it.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum MediaCommand {
    /// Fill in missing stored byte sizes for media, avatars, headers and
    /// emoji (backing the admin storage metrics). Stats each file on the
    /// media store; skips any whose file is missing.
    BackfillSizes,
    /// Find stored files no database row references — orphans left by deletions
    /// that ran before the durable cleanup queue existed — and (with `--delete`)
    /// schedule them for removal. Reports counts only by default. Run during low
    /// upload activity: a brand-new upload whose row is not yet inserted would
    /// otherwise look orphaned.
    Reconcile {
        /// Actually enqueue the orphans for deletion (default: report only).
        #[arg(long)]
        delete: bool,
    },
}

#[derive(Subcommand)]
enum RoleCommand {
    /// List the available roles and their permission bitmasks.
    List,
}

/// Read-only federation diagnostics — every subcommand only reads (signed GETs
/// or DB queries) and persists nothing, safe to run against a live server.
#[derive(Subcommand)]
enum FederationCommand {
    /// Fetch a remote `ActivityPub` object and pretty-print it. Signed GET,
    /// following a permalink to the canonical `id` (paste a status URL or an
    /// actor URL). Nothing is stored.
    Fetch { url: String },
    /// Resolve a `user@domain` handle via `WebFinger` and print every
    /// `ActivityPub` actor it advertises. Nothing is stored (unlike a real
    /// follow, no remote account row is created).
    Webfinger { acct: String },
    /// Inspect the outbound delivery queue.
    #[command(subcommand)]
    Queue(FederationQueueCommand),
    /// List the hosts the delivery breaker currently considers unreachable,
    /// with their failure streak and last error.
    Reachability,
    /// Inspect, rotate, revoke, or rewrap normalized federation keys.
    #[command(subcommand)]
    Keys(FederationKeysCommand),
}

#[derive(Subcommand)]
enum FederationKeysCommand {
    /// Fail unless all private rows are encrypted, decryptable, and match
    /// their public halves.
    Audit,
    /// Re-encrypt rows from configured previous secrets onto the primary
    /// encryption-secret version. Safe to resume.
    Rewrap {
        /// Process at most this many stale rows, for controlled rolling
        /// rehearsals. Omit to drain every bounded batch.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Permanently drop the verified-empty legacy plaintext columns after all
    /// processes have been upgraded and the rollback window has ended.
    Contract,
    /// Rotate one local account signing algorithm with a bounded overlap.
    RotateAccount {
        username: String,
        #[arg(long, value_parser = ["rsa", "ed25519"])]
        algorithm: String,
        #[arg(long, default_value_t = 168)]
        overlap_hours: i64,
        /// Publish the replacement before using it to sign, giving peers time
        /// to process the old-key-signed Actor Update.
        #[arg(long, default_value_t = 300)]
        activation_delay_seconds: i64,
    },
    /// Rotate an instance-actor signing algorithm with a bounded overlap.
    RotateInstance {
        #[arg(long, value_parser = ["rsa", "ed25519"])]
        algorithm: String,
        #[arg(long, default_value_t = 168)]
        overlap_hours: i64,
        #[arg(long, default_value_t = 300)]
        activation_delay_seconds: i64,
    },
    /// Immediately revoke a key URI; it can no longer sign or verify.
    Revoke { key_uri: String },
    /// Retire a key URI after its overlap/grace use is complete.
    Retire { key_uri: String },
}

#[derive(Subcommand)]
enum FederationQueueCommand {
    /// Summarize the queue: total/due counts, when the next job fires, and the
    /// worst per-host backlogs.
    Inspect,
}

#[derive(Subcommand)]
enum RuleCommand {
    /// List the published rules in display order.
    List,
    /// Add a rule. New rules are appended after the existing ones.
    Add {
        /// The rule text shown to users (max 300 characters).
        text: String,
        /// Optional longer explanation shown beneath the rule.
        #[arg(long, default_value = "")]
        hint: String,
    },
    /// Edit an existing rule's text and/or hint by id.
    Edit {
        id: i64,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        hint: Option<String>,
        /// Reposition the rule in the ordered list.
        #[arg(long)]
        priority: Option<i32>,
    },
    /// Remove a rule by id (soft-deleted; reports that cite it still resolve).
    Remove { id: i64 },
}

#[derive(Subcommand)]
enum AnnouncementCommand {
    /// List all announcements (published and not), newest first.
    List,
    /// Add an announcement. It publishes immediately unless `--scheduled-at`
    /// is set to a future RFC 3339 timestamp.
    Add {
        /// The announcement text (linkified; supports @mentions and #hashtags).
        text: String,
        /// Hold the announcement unpublished until this time (RFC 3339).
        #[arg(long)]
        scheduled_at: Option<String>,
    },
    /// Publish an announcement by id.
    Publish { id: i64 },
    /// Unpublish an announcement by id.
    Unpublish { id: i64 },
    /// Remove an announcement by id (also clears its reactions and dismissals).
    Remove { id: i64 },
}

#[derive(Subcommand)]
enum EmojiCommand {
    /// Add a local custom emoji from an image file (PNG, GIF or WebP,
    /// at most 256 KB).
    Add {
        /// The `:shortcode:` (without colons): 2-128 letters, digits or `_`.
        shortcode: String,
        /// Path to the image file.
        file: std::path::PathBuf,
    },
    /// List local custom emoji.
    List,
    /// Remove a local custom emoji by shortcode.
    Remove { shortcode: String },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Create a local account, optionally with login credentials.
    Add {
        username: String,
        #[arg(long, default_value = "")]
        display_name: String,
        /// Create a pre-immutable-ID actor for upgrade/interoperability
        /// rehearsals. Normal production account creation must not use this.
        #[arg(long, hide = true)]
        legacy_actor_uri: bool,
        /// Optional e-mail: an alternate login identifier that also enables
        /// password reset (requires --password).
        #[arg(long, requires = "password")]
        email: Option<String>,
        /// Password for signing in (via username, or --email when given).
        #[arg(long)]
        password: Option<String>,
    },
    /// Set (or replace) the login credentials of an existing account.
    Passwd {
        username: String,
        /// Optional e-mail; omitting it keeps any address already stored.
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password: String,
    },
    /// Grant a moderation role to an account (by role name, e.g. `Owner`), or
    /// clear it with `--clear`. Use this to bootstrap the first administrator.
    SetRole {
        username: String,
        /// Role name (case-insensitive); omit with `--clear`.
        role: Option<String>,
        /// Remove any assigned role instead of setting one.
        #[arg(long, conflicts_with = "role")]
        clear: bool,
    },
    /// Change the human/discovery handle of an immutable-ID account. The old
    /// profile URL remains reserved as a redirect; the AP actor ID is stable.
    Rename {
        username: String,
        new_username: String,
    },
    /// Manage an account's aliases (`alsoKnownAs`) — declare an alias so
    /// another account is allowed to migrate its followers here.
    #[command(subcommand)]
    Alias(AliasCommand),
    /// Migrate this account to another (remote) account, re-pointing local
    /// followers and telling remote followers via `Move`. The destination
    /// must already list this account in its aliases.
    Migrate {
        username: String,
        /// Destination `user@domain` acct or actor URI.
        target: String,
    },
}

#[derive(Subcommand)]
enum GroupCommand {
    /// Create a local group owned by an existing local account.
    Add {
        /// The group's name — its `preferredUsername`, sharing the local
        /// account namespace (`!name@domain` / `@name@domain`).
        name: String,
        /// Local username of the owner.
        #[arg(long)]
        owner: String,
        #[arg(long, default_value = "")]
        display_name: String,
        /// Hold join requests for moderator approval instead of
        /// auto-accepting followers.
        #[arg(long)]
        approval: bool,
    },
    /// List local groups.
    List,
    /// Lock a thread in a group — no new comments (moderator action).
    Lock {
        /// The group's name.
        group: String,
        /// The thread root (or any post in it) as a local status id.
        status_id: i64,
    },
    /// Reopen a locked thread.
    Unlock { group: String, status_id: i64 },
    /// Remove a post or comment from a group (moderator removal; the status
    /// itself survives).
    Remove {
        group: String,
        status_id: i64,
        #[arg(long, default_value = "Removed by moderator")]
        reason: String,
    },
    /// Ban an account from a group (outcast). `target` is a local username or a
    /// known `user@domain` handle.
    Ban { group: String, target: String },
    /// Lift a group ban.
    Unban { group: String, target: String },
    /// Rename a group (change its display name), preserving every other
    /// setting. Federates the profile Update.
    Rename {
        /// The group's name.
        group: String,
        #[arg(long)]
        display_name: String,
    },
    /// Transfer ownership of a group to another local member. The previous
    /// owner is demoted to moderator.
    Transfer {
        /// The group's name.
        group: String,
        /// Local username (or `@user`) of the new owner — must be a member.
        #[arg(long)]
        to: String,
    },
    /// Delete a group: tombstones the actor (`410 Gone`), federates
    /// `Delete(Group)` (Lemmy) plus `Delete(Actor)` (Mastodon), and purges its
    /// content. Irreversible.
    Delete { group: String },
}

#[derive(Subcommand)]
enum AliasCommand {
    /// Declare an alias (a `user@domain` acct or actor URI) on an account.
    Add { username: String, alias: String },
    /// List an account's declared aliases.
    List { username: String },
    /// Remove a declared alias (by its stored URI).
    Remove { username: String, alias: String },
}

// The staging deploy is a static musl binary, and musl's own malloc is slow
// under multithreaded load — always run on mimalloc instead.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), AnyError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    if let Command::Config(ConfigCommand::Generate { force }) = &cli.command {
        Config::generate(&cli.config, *force)?;
        println!("generated {}", cli.config.display());
        return Ok(());
    }
    let config = Config::from_file(&cli.config)?;
    // Acquire before startup migrations/key backfills, and monitor through
    // startup, serving/CLI execution, and graceful worker drainage. Returning an error from
    // this branch reaches main's process::exit(1), stopping detached tasks too.
    let mut writer = if matches!(&cli.command, Command::Serve) {
        plamenu_db::single_writer::acquire_single_writer_lock(&config.database_url).await?
    } else {
        let writer =
            plamenu_db::single_writer::acquire_cli_writer_lock(&config.database_url).await?;
        plamenu_db::id::initialize_cli_lane()?;
        writer
    };
    let operation = async {
        let state = initialize_state(config).await?;
        dispatch(cli.command, state).await
    };
    tokio::select! {
        result = operation => {
            if let Err(error) = writer.release().await {
                tracing::warn!(%error, "failed to release the writer lock cleanly");
            }
            result
        }
        result = writer.monitor() => {
            result?;
            Err("writer lock monitor unexpectedly exited".into())
        }
    }
}

/// Initializes shared services under the server or CLI writer-session monitor.
async fn initialize_state(config: Config) -> Result<AppState, AnyError> {
    let pool = plamenu_db::connect_and_migrate(&config.database_url, config.db_pool_size).await?;
    let legacy_actor_uris =
        account::backfill_legacy_local_actor_uris(&pool, &config.domain).await?;
    if legacy_actor_uris > 0 {
        tracing::info!(
            count = legacy_actor_uris,
            "persisted legacy local ActivityPub actor URIs"
        );
    }
    let encrypted_keys = plamenu::key_store::backfill_and_preflight(
        &pool,
        &config,
        plamenu::key_store::LegacyPrivateDisposition::PreserveForRollback,
    )
    .await?;
    if encrypted_keys > 0 {
        tracing::info!(
            count = encrypted_keys,
            "encrypted and verified legacy federation signing keys"
        );
    }
    let federation_keyring = Arc::new(plamenu::crypto::FederationKeyring::from_config(&config)?);
    plamenu::key_store::ensure_instance(&pool, &federation_keyring, &config.domain).await?;
    // Lead with the Mastodon-compat token (Googlebot-style `compatible;`
    // form, still truthfully self-identifying as plamenu). Some instances
    // sit behind a Cloudflare WAF that allowlists federation fetches by a
    // leading User-Agent token — e.g. aussie.zone passes UAs starting with
    // `Mastodon`/`Lemmy`/`PieFed` and 403s everything else, which black-holed
    // our up-thread pulls. Leading with the compat marker restores those hosts
    // and, verified against 80 live remotes, degrades none.
    let user_agent = format!(
        "Mastodon/{} (compatible; plamenu/{}; +https://{})",
        plamenu::MASTODON_COMPAT_VERSION,
        plamenu::VERSION,
        config.domain
    );
    // Link-preview fetches carry the Mastodon-compat marker and a `Bot`
    // suffix like Mastodon's `FetchLinkCardService`: crawler-gating sites
    // (YouTube among them) only serve their OpenGraph/oEmbed metadata to
    // fetchers they recognize and hand everyone else a metadata-less shell.
    let page_user_agent = format!(
        "plamenu/{} (compatible; Mastodon/{}; +https://{}) Bot",
        plamenu::VERSION,
        plamenu::MASTODON_COMPAT_VERSION,
        config.domain
    );
    // Every outbound fetch is signed with the instance actor's key, so
    // remotes running authorized fetch answer us.
    let instance_records = plamenu_db::actor_key::usable_for_instance(&pool).await?;
    let rsa_record = instance_records
        .iter()
        .find(|key| key.algorithm == "rsa")
        .cloned()
        .ok_or("instance RSA signing key is missing")?;
    let rsa = plamenu::key_store::decrypt_record(&federation_keyring, rsa_record)?;
    let mut fetch_signer =
        RequestSigner::from_pkcs8_pem(rsa.private.expose_str()?, rsa.record.key_uri)?;
    // …and the Ed25519 key too, so a peer that cannot resolve the RSA keyId
    // gets an RFC 9421 second knock instead of a dead fetch.
    if let Some(ed_record) = instance_records
        .iter()
        .find(|key| key.algorithm == "ed25519")
        .cloned()
    {
        let ed = plamenu::key_store::decrypt_record(&federation_keyring, ed_record)?;
        fetch_signer = fetch_signer.with_ed25519(ed.record.key_uri, ed.private.expose_str()?);
    }
    let federation = Arc::new(HttpFederation::new(
        FederationClient::new(&user_agent, config.allow_private_fetch)?
            .with_proxies(&plamenu_federation::ProxyConfig {
                proxy_url: config.federation.proxy_url.clone(),
                onion_proxy_url: config.federation.onion_proxy_url.clone(),
                i2p_proxy_url: config.federation.i2p_proxy_url.clone(),
                no_proxy: config.federation.no_proxy.clone(),
            })?
            .with_fetch_signer(fetch_signer)
            .with_page_user_agent(page_user_agent),
        pool.clone(),
        config.domain.clone(),
        Arc::clone(&federation_keyring),
    ));
    let media = Arc::new(plamenu::storage::LocalDiskStore::new(
        config.media_dir.clone(),
    )?);
    Ok(AppState::new(pool, config, federation, media)?)
}

/// Runs the parsed subcommand against a ready [`AppState`].
#[allow(clippy::too_many_lines)] // a flat match over every CLI subcommand
async fn dispatch(command: Command, state: AppState) -> Result<(), AnyError> {
    match command {
        Command::Config(_) => unreachable!("config generation is handled before app startup"),
        Command::Serve => serve(state).await,
        Command::Account(AccountCommand::Add {
            username,
            display_name,
            legacy_actor_uri,
            email,
            password,
        }) => {
            account_add(
                &state,
                &username,
                &display_name,
                legacy_actor_uri,
                email.as_deref(),
                password.as_deref(),
            )
            .await
        }
        Command::Account(AccountCommand::Passwd {
            username,
            email,
            password,
        }) => account_passwd(&state, &username, email.as_deref(), &password).await,
        Command::Account(AccountCommand::SetRole {
            username,
            role,
            clear,
        }) => account_set_role(&state, &username, role.as_deref(), clear).await,
        Command::Account(AccountCommand::Rename {
            username,
            new_username,
        }) => account_rename(&state, &username, &new_username).await,
        Command::Account(AccountCommand::Alias(cmd)) => account_alias(&state, cmd).await,
        Command::Group(GroupCommand::Add {
            name,
            owner,
            display_name,
            approval,
        }) => group_add(&state, &name, &owner, &display_name, approval).await,
        Command::Group(GroupCommand::List) => group_list(&state).await,
        Command::Group(GroupCommand::Lock { group, status_id }) => {
            group_lock_cli(&state, &group, status_id, true).await
        }
        Command::Group(GroupCommand::Unlock { group, status_id }) => {
            group_lock_cli(&state, &group, status_id, false).await
        }
        Command::Group(GroupCommand::Remove {
            group,
            status_id,
            reason,
        }) => group_remove_cli(&state, &group, status_id, &reason).await,
        Command::Group(GroupCommand::Ban { group, target }) => {
            group_ban_cli(&state, &group, &target, true).await
        }
        Command::Group(GroupCommand::Unban { group, target }) => {
            group_ban_cli(&state, &group, &target, false).await
        }
        Command::Group(GroupCommand::Rename {
            group,
            display_name,
        }) => group_rename_cli(&state, &group, &display_name).await,
        Command::Group(GroupCommand::Transfer { group, to }) => {
            group_transfer_cli(&state, &group, &to).await
        }
        Command::Group(GroupCommand::Delete { group }) => group_delete_cli(&state, &group).await,
        Command::Account(AccountCommand::Migrate { username, target }) => {
            let outcome = migration::migrate_local_account(&state, &username, &target)
                .await
                .map_err(plamenu::error::ApiError::from)?;
            println!(
                "migrated @{username} to {} ({} remote follower inboxes notified)",
                outcome.target_uri, outcome.followers_notified
            );
            Ok(())
        }
        Command::Post { username, text } => {
            let (status, deliveries) = actions::post_status(
                &state,
                actions::PostParams {
                    username: &username,
                    text: &text,
                    visibility: "public",
                    in_reply_to_id: None,
                    media_ids: &[],
                    quoted_status_id: None,
                    ..Default::default()
                },
            )
            .await?;
            let uri = status.uri.clone().unwrap_or_else(|| {
                format!(
                    "https://{}/users/{username}/statuses/{}",
                    state.config.domain, status.id
                )
            });
            println!("posted {uri} ({deliveries} deliveries queued)");
            Ok(())
        }
        Command::Follow { username, target } => {
            let outcome = actions::follow_remote(&state, &username, &target).await?;
            println!(
                "follow of {} queued for delivery to {} (pending until accepted)",
                outcome.target_uri, outcome.target_inbox
            );
            Ok(())
        }
        Command::Emoji(EmojiCommand::Add { shortcode, file }) => {
            emoji_add(&state, &shortcode, &file).await
        }
        Command::Emoji(EmojiCommand::List) => emoji_list(&state).await,
        Command::Emoji(EmojiCommand::Remove { shortcode }) => {
            if plamenu_db::custom_emoji::delete_local(&state.pool, &shortcode).await? {
                println!("removed :{shortcode}:");
                Ok(())
            } else {
                Err(format!("no local emoji :{shortcode}:").into())
            }
        }
        Command::Role(RoleCommand::List) => role_list(&state).await,
        Command::Rule(cmd) => rule_command(&state, cmd).await,
        Command::Announcement(cmd) => announcement_command(&state, cmd).await,
        Command::Media(MediaCommand::BackfillSizes) => media_backfill_sizes(&state).await,
        Command::Media(MediaCommand::Reconcile { delete }) => media_reconcile(&state, delete).await,
        Command::Federation(cmd) => federation_command(&state, cmd).await,
        Command::SelfDestruct => self_destruct(&state).await,
    }
}

/// `plamenu self-destruct` — Mastodon's CLI flow: when already armed it
/// reports progress; otherwise it demands the domain be typed back plus an
/// explicit yes, then arms the mode. The running server notices the flag
/// within seconds (no restart needed) and starts broadcasting.
async fn self_destruct(state: &AppState) -> Result<(), AnyError> {
    use std::io::{BufRead, Write};

    if plamenu_db::instance_settings::get(&state.pool)
        .await?
        .is_self_destructing()
    {
        println!("Self-destruct mode is already enabled for this server");
        let progress = plamenu::self_destruct::progress(state).await?;
        if progress.pending_accounts > 0 {
            println!(
                "{} account(s) are still pending deletion.",
                progress.pending_accounts
            );
        } else if progress.pending_deliveries > 0 {
            println!(
                "Deletion notices are still being delivered ({} deliveries queued or retrying).",
                progress.pending_deliveries
            );
        } else {
            println!(
                "Every deletion notice has been sent! \
                 You can safely delete all data and decommission your servers!"
            );
        }
        return Ok(());
    }

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ask = |prompt: &str| -> Result<String, AnyError> {
        print!("{prompt} ");
        std::io::stdout().flush()?;
        Ok(lines
            .next()
            .transpose()?
            .unwrap_or_default()
            .trim()
            .to_owned())
    };

    if ask("Type in the domain of the server to confirm:")? != state.config.domain {
        return Err("Domains do not match. Stopping self-destruct initiation.".into());
    }
    println!(
        "\nThis operation WILL NOT be reversible.\n\
         While no data is erased locally, the server will be in a BROKEN STATE afterwards:\n\
         other servers will forget this server's accounts, but local state will not change.\n\
         The server must keep running until every deletion notice is delivered\n\
         (re-run this command to watch the progress).\n"
    );
    if !matches!(
        ask("Are you sure you want to proceed? (yes/no)")?
            .to_lowercase()
            .as_str(),
        "y" | "yes"
    ) {
        return Err("Operation cancelled. Self-destruct will not begin.".into());
    }

    plamenu_db::instance_settings::begin_self_destruct(&state.pool).await?;
    println!(
        "\nSelf-destruct enabled. The running server begins broadcasting deletion\n\
         notices within seconds and now answers 410 Gone (sign-in and data export\n\
         remain available). Re-run this command to see the wind-down's progress."
    );
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "a flat list of worker supervisor spawns and their matching aborts"
)]
async fn serve(state: AppState) -> Result<(), AnyError> {
    let bind = state.config.bind;
    // Loud warning when an off-host bind trusts a broad private range as a
    // proxy — that combination lets any host on the range forge client IPs.
    if let Some(warning) = state.config.insecure_proxy_trust_warning() {
        tracing::warn!("{warning}");
    }
    if let Some(warning) = state.config.proxy_destination_filtering_warning() {
        tracing::warn!("{warning}");
    }
    let mut supervisors = vec![
        supervise("delivery", state.clone(), delivery::spawn),
        supervise("poll expiry", state.clone(), poll_expiry::spawn),
        supervise(
            "scheduled status publisher",
            state.clone(),
            plamenu::scheduled_status_publish::spawn,
        ),
        supervise(
            "status cleanup",
            state.clone(),
            plamenu::statuses_cleanup::spawn,
        ),
        supervise("streaming", state.clone(), plamenu::streaming::spawn),
        supervise("web push", state.clone(), plamenu::web_push::spawn),
        supervise("link crawler", state.clone(), plamenu::link_preview::spawn),
        supervise("link verifier", state.clone(), plamenu::link_verify::spawn),
        supervise("reply fetcher", state.clone(), plamenu::reply_fetch::spawn),
        supervise(
            "remote history",
            state.clone(),
            plamenu::remote_history::spawn,
        ),
        supervise(
            "quote verifier",
            state.clone(),
            plamenu::quote_verify::spawn,
        ),
        supervise("account move", state.clone(), plamenu::migration::spawn),
        supervise("domain severance", state.clone(), plamenu::severance::spawn),
    ];
    // Size the heavy-media gate from the admin setting before any media work
    // runs (0 = auto: half the cores). A changed setting applies on restart.
    match state.settings_cache.get(&state.pool).await {
        Ok(settings) => plamenu::media_gate::configure(
            usize::try_from(settings.media_processing_concurrency.clamp(0, 64)).unwrap_or(0),
        ),
        Err(error) => {
            tracing::warn!(%error, "settings unavailable; media gate uses the auto limit");
            plamenu::media_gate::configure(0);
        }
    }
    supervisors.extend([
        supervise("media", state.clone(), plamenu::media_worker::spawn),
        supervise("media A/V", state.clone(), plamenu::media_worker::spawn_av),
        supervise(
            "media retention",
            state.clone(),
            plamenu::media_worker::spawn_retention,
        ),
        supervise(
            "media cleanup",
            state.clone(),
            plamenu::media_cleanup_worker::spawn,
        ),
        supervise("webhook", state.clone(), plamenu::webhooks::spawn),
        supervise("import", state.clone(), plamenu::import_worker::spawn),
        supervise("archive", state.clone(), plamenu::archive_worker::spawn),
        supervise("trends", state.clone(), plamenu::trends::spawn),
        supervise("maintenance", state.clone(), plamenu::maintenance::spawn),
        supervise(
            "self destruct",
            state.clone(),
            plamenu::self_destruct::spawn,
        ),
    ]);
    // No SMTP relay configured → no worker; the mail-needing flows refuse
    // upfront (`mailer::enabled`), so the queue stays empty.
    if let Some(smtp_config) = &state.config.smtp {
        supervisors.push(supervise_mailer(
            state.clone(),
            plamenu::mailer::Smtp::from_config(smtp_config)?,
        ));
    }
    let shutdown = state.shutdown.clone();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "plamenu listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    // Graceful worker shutdown: signal cancellation so every
    // worker loop stops claiming new work at its next pause point, then let
    // each supervisor drain its child within the bounded window and abort only
    // the stragglers. The supervisors all observe the signal concurrently, so
    // total shutdown time is one drain window, not one per worker.
    tracing::info!("HTTP drained; stopping background workers");
    shutdown.cancel();
    for supervisor in supervisors {
        let _ = supervisor.await;
    }
    Ok(())
}

// The supervisor below recovers a panicked worker by observing its erroring
// `JoinHandle` and respawning it. That path only exists when
// panics unwind; `panic = "abort"` would abort the whole process on any worker
// panic, silently defeating every supervised restart. Fail any build that
// selects abort — the test and bench profiles always unwind, so this never
// fires under `cargo test`; it guards real (release) builds, which the
// `./dev release` builds through release/Dockerfile.build.
#[cfg(panic = "abort")]
compile_error!(
    "plamenu's worker supervisor requires panic = \"unwind\"; the release profile \
     in Cargo.toml must not set panic = \"abort\""
);

/// How long a supervisor waits for its worker to finish its current job and
/// observe the shutdown signal before aborting it as a straggler. Workers pause
/// between jobs via `workers::pause`, so a cooperative
/// exit normally takes milliseconds; the window exists for a job already in
/// flight (an ffmpeg run, a slow remote delivery). Leased/queued work an
/// aborted straggler leaves behind is reclaimed on the next startup by the
/// queues' own lease expiry.
const WORKER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Restarts a background subsystem if it panics or returns unexpectedly. The
/// HTTP server must never remain superficially healthy with a permanently dead
/// queue worker.
///
/// Recovery on panic depends on unwinding: a panicking `spawn()`ed task resolves
/// its `JoinHandle` to `Err(JoinError::is_panic)`, which this loop logs and
/// restarts. Under `panic = "abort"` the process would abort before the handle
/// ever resolves, so the release profile pins `panic = "unwind"` (guarded by the
/// `compile_error!` above and the `release_panic_strategy` integration test).
fn supervise(
    name: &'static str,
    state: AppState,
    spawn: fn(AppState) -> tokio::task::JoinHandle<()>,
) -> tokio::task::JoinHandle<()> {
    let registry = state.workers.clone();
    let shutdown = state.shutdown.clone();
    tokio::spawn(supervise_with(name, registry, shutdown, move || {
        spawn(state.clone())
    }))
}

fn supervise_mailer(state: AppState, smtp: plamenu::mailer::Smtp) -> tokio::task::JoinHandle<()> {
    let registry = state.workers.clone();
    let shutdown = state.shutdown.clone();
    tokio::spawn(supervise_with("mailer", registry, shutdown, move || {
        plamenu::mailer::spawn(state.clone(), smtp.clone())
    }))
}

/// The supervisor loop. It owns the current child handle at all times, so
/// shutdown can drain and (only if necessary) abort the *worker*, not just the
/// supervisor — aborting a supervisor would detach its child to run until
/// runtime teardown. Each unexpected exit is recorded in the
/// worker registry so the readiness probe can see a restart-looping subsystem.
async fn supervise_with(
    name: &'static str,
    registry: std::sync::Arc<plamenu::workers::WorkerRegistry>,
    shutdown: tokio_util::sync::CancellationToken,
    mut spawn: impl FnMut() -> tokio::task::JoinHandle<()>,
) {
    loop {
        let mut child = spawn();
        tokio::select! {
            result = &mut child => {
                match result {
                    Ok(()) if shutdown.is_cancelled() => return,
                    Ok(()) => {
                        tracing::error!(worker = name, "worker stopped unexpectedly; restarting");
                    }
                    Err(error) => {
                        tracing::error!(worker = name, %error, "worker crashed; restarting");
                    }
                }
                registry.record_exit(name);
                tokio::select! {
                    () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                    () = shutdown.cancelled() => return,
                }
            }
            () = shutdown.cancelled() => {
                // Bounded drain: give the worker time to notice the signal (or
                // finish its in-flight job), then abort only a straggler.
                if tokio::time::timeout(WORKER_DRAIN_TIMEOUT, &mut child)
                    .await
                    .is_err()
                {
                    tracing::warn!(worker = name, "worker did not drain in time; aborting");
                    child.abort();
                    let _ = child.await;
                }
                return;
            }
        }
    }
}

#[allow(
    clippy::explicit_auto_deref,
    reason = "sqlx 0.9 generic executors require dereferencing a transaction to its connection"
)]
async fn account_add(
    state: &AppState,
    username: &str,
    display_name: &str,
    legacy_actor_uri: bool,
    email: Option<&str>,
    password: Option<&str>,
) -> Result<(), AnyError> {
    // Validates the username with the same rules webfinger lookups use.
    let acct = Acct::new(username, &state.config.account_domain)?;
    let keypair = keys::generate_keypair()?;
    let ed25519 = keys::generate_ed25519_keypair();
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(plamenu::crypto::KeyEncryptionError::MissingConfiguration)?;
    let mut tx = state.pool.begin().await?;
    let new_account = NewLocalAccount {
        username,
        display_name,
        note: "",
        public_key_pem: &keypair.public_pem,
    };
    let created = if legacy_actor_uri {
        account::create_local_normalized_legacy(&mut *tx, new_account).await?
    } else {
        account::create_local_immutable(&mut *tx, new_account, &state.config.domain).await?
    };
    plamenu::key_store::provision_account_tx(
        &mut *tx,
        keyring,
        &state.config.domain,
        &created,
        &keypair,
        &ed25519,
    )
    .await?;
    tx.commit().await?;
    println!(
        "created @{acct} (id {}) — https://{}/users/{}",
        created.id, state.config.domain, created.username
    );
    if let Some(password) = password {
        // Same length policy as every other credential surface.
        plamenu::auth::validate_password(password)
            .map_err(plamenu::auth::PasswordPolicy::message)?;
        let hash = plamenu::auth::hash_password(password)?;
        plamenu_db::user::create(&state.pool, created.id, email, &hash).await?;
        println!("login enabled for {}", email.unwrap_or(&acct.to_string()));
    }
    // The running server's worker delivers the queued rows.
    plamenu::webhooks::account_event(state, plamenu_db::webhook::ACCOUNT_CREATED, created.id).await;
    Ok(())
}

async fn group_add(
    state: &AppState,
    name: &str,
    owner: &str,
    display_name: &str,
    approval: bool,
) -> Result<(), AnyError> {
    let owner_account = account::find_local_by_username(&state.pool, owner)
        .await?
        .ok_or_else(|| format!("no local account @{owner}"))?;
    let (created, group) = plamenu::groups::create_group(
        state,
        plamenu::groups::CreateGroupParams {
            name,
            display_name,
            membership_policy: if approval {
                plamenu_db::group::MembershipPolicy::Approval
            } else {
                plamenu_db::group::MembershipPolicy::Open
            },
            // Operators tune the posting policy from the web/admin console; the
            // default matches the DB (members-only).
            posting_policy: plamenu_db::group::PostingPolicy::Members,
            created_by: owner_account.id,
            // Like `account add`: the operator speaking overrides the blocklist
            // and the per-account group quota.
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    // The typed refusal folds back into the ApiError wording the CLI has
    // always printed.
    .map_err(plamenu::error::ApiError::from)?;
    println!(
        "created group !{}@{} ({} membership, owner @{owner}) — https://{}/users/{}",
        created.username,
        state.config.account_domain,
        group.membership_policy().as_str(),
        state.config.domain,
        created.username
    );
    Ok(())
}

async fn group_list(state: &AppState) -> Result<(), AnyError> {
    let ids = plamenu_db::group::local_group_ids(&state.pool, true, i64::MAX, 0).await?;
    let accounts = account::find_by_ids(&state.pool, &ids).await?;
    for id in ids {
        if let Some(group_account) = accounts.iter().find(|a| a.id == id) {
            println!(
                "!{}@{}",
                group_account.username, state.config.account_domain
            );
        }
    }
    Ok(())
}

/// Resolves a local group account by name (the operator's shorthand for the
/// moderation subcommands).
async fn resolve_group(state: &AppState, name: &str) -> Result<account::Account, AnyError> {
    account::find_local_by_username(&state.pool, name.trim_start_matches('!'))
        .await?
        .filter(account::Account::is_group)
        .ok_or_else(|| format!("no local group !{name}").into())
}

/// The group's owner — the moderator these CLI actions act as.
async fn group_owner(state: &AppState, group_id: i64) -> Result<account::Account, AnyError> {
    let elevated = plamenu_db::group::elevated(&state.pool, group_id).await?;
    let owner_id = elevated
        .iter()
        .find(|entry| entry.affiliation == "owner")
        .map(|entry| entry.account_id)
        .ok_or("group has no owner")?;
    account::find_by_id(&state.pool, owner_id)
        .await?
        .ok_or_else(|| "owner account missing".into())
}

/// Resolves a `@user` / `user@domain` handle to a *known* account (no fetch).
async fn resolve_group_target(
    state: &AppState,
    handle: &str,
) -> Result<account::Account, AnyError> {
    let stripped = handle.trim_start_matches('@');
    let account = match stripped.split_once('@') {
        None => account::find_local_by_username(&state.pool, stripped).await?,
        Some((user, domain)) if state.config.is_local_domain(domain) => {
            account::find_local_by_username(&state.pool, user).await?
        }
        // Group-moderation CLI targets are person-like (a user to promote/ban);
        // a group itself is named by its own `resolve_group` path.
        Some((user, domain)) => {
            account::find_remote_person_by_acct(&state.pool, user, domain).await?
        }
    };
    account.ok_or_else(|| format!("no known account @{stripped}").into())
}

async fn group_lock_cli(
    state: &AppState,
    group_name: &str,
    status_id: i64,
    lock: bool,
) -> Result<(), AnyError> {
    let group_account = resolve_group(state, group_name).await?;
    let owner = group_owner(state, group_account.id).await?;
    let root_id = plamenu_db::status::thread_root(&state.pool, status_id).await?;
    let root = plamenu_db::status::find_by_id(&state.pool, root_id)
        .await?
        .ok_or_else(|| format!("no status {status_id}"))?;
    plamenu::groups::set_thread_lock(state, &group_account, &owner, &root, lock).await?;
    println!(
        "{} thread {root_id} in !{}",
        if lock { "locked" } else { "unlocked" },
        group_account.username
    );
    Ok(())
}

async fn group_remove_cli(
    state: &AppState,
    group_name: &str,
    status_id: i64,
    reason: &str,
) -> Result<(), AnyError> {
    let group_account = resolve_group(state, group_name).await?;
    let owner = group_owner(state, group_account.id).await?;
    let status = plamenu_db::status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or_else(|| format!("no status {status_id}"))?;
    plamenu::groups::remove_from_group(state, &group_account, &owner, &status, reason).await?;
    println!(
        "removed status {status_id} from !{}",
        group_account.username
    );
    Ok(())
}

async fn group_rename_cli(
    state: &AppState,
    group_name: &str,
    display_name: &str,
) -> Result<(), AnyError> {
    let group_account = resolve_group(state, group_name).await?;
    let group = plamenu_db::group::find(&state.pool, group_account.id)
        .await?
        .ok_or_else(|| format!("no group !{group_name}"))?;
    // Preserve every other setting; only the display name changes.
    let settings = plamenu::groups::GroupSettings {
        display_name,
        note_html: &group_account.note,
        note_source: &group_account.note_source,
        policy: group.membership_policy(),
        sensitive: group.sensitive,
        posting_policy: group.posting_policy(),
        discoverable: group_account.discoverable.unwrap_or(true),
        profile: plamenu::groups::GroupProfileEdit::default(),
    };
    plamenu::groups::update_settings(state, &group_account, settings).await?;
    println!("renamed !{} to {display_name}", group_account.username);
    Ok(())
}

async fn group_transfer_cli(state: &AppState, group_name: &str, to: &str) -> Result<(), AnyError> {
    let group_account = resolve_group(state, group_name).await?;
    let new_owner = resolve_group_target(state, to).await?;
    plamenu::groups::transfer_owner(state, &group_account, &new_owner).await?;
    println!(
        "transferred !{} to @{}",
        group_account.username, new_owner.username
    );
    Ok(())
}

async fn group_delete_cli(state: &AppState, group_name: &str) -> Result<(), AnyError> {
    let group_account = resolve_group(state, group_name).await?;
    plamenu::groups::delete_group(state, &group_account).await?;
    println!("deleted !{}", group_account.username);
    Ok(())
}

async fn group_ban_cli(
    state: &AppState,
    group_name: &str,
    target: &str,
    ban: bool,
) -> Result<(), AnyError> {
    let group_account = resolve_group(state, group_name).await?;
    let owner = group_owner(state, group_account.id).await?;
    let target_account = resolve_group_target(state, target).await?;
    if ban {
        plamenu::groups::ban_member(state, &group_account, &owner, &target_account, None, None)
            .await?;
        println!("banned {target} from !{}", group_account.username);
    } else {
        plamenu::groups::unban_member(state, &group_account, &owner, &target_account).await?;
        println!("unbanned {target} from !{}", group_account.username);
    }
    Ok(())
}

async fn emoji_add(
    state: &AppState,
    shortcode: &str,
    file: &std::path::Path,
) -> Result<(), AnyError> {
    if !plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        return Err("shortcode must be 2-128 characters of [a-zA-Z0-9_]".into());
    }
    let bytes = std::fs::read(file)?;
    let max_bytes = plamenu_db::custom_emoji::settings(&state.pool)
        .await?
        .max_file_size_bytes();
    let (content_type, extension) =
        plamenu::media_processing::validate_emoji_image(&bytes, max_bytes)?;
    let file_name = format!("{}.{extension}", plamenu_db::id::next());
    let file_size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    state.media.put(&file_name, bytes).await?;
    let created = plamenu_db::custom_emoji::create_local(
        &state.pool,
        shortcode,
        &file_name,
        content_type,
        file_size,
        None,
    )
    .await?
    .ok_or_else(|| format!("a local emoji :{shortcode}: already exists"))?;
    println!(
        "added :{shortcode}: — https://{}/media/{file_name} (id {})",
        state.config.domain, created.id
    );
    Ok(())
}

/// Stats each stored file whose byte size is still unknown and records it, so
/// the admin storage metrics (`space_usage`, `instance_media_attachments`)
/// account for media uploaded before sizes were tracked. The sweep itself is
/// shared with the admin console (`maintenance::backfill_stored_sizes`);
/// unreadable files land in the log.
async fn media_backfill_sizes(state: &AppState) -> Result<(), AnyError> {
    let (filled, skipped) = plamenu::maintenance::backfill_stored_sizes(state).await?;
    println!("backfilled {filled} stored file size(s)");
    if skipped > 0 {
        println!("skipped {skipped} unreadable file(s) — see the log");
    }
    Ok(())
}

async fn media_reconcile(state: &AppState, delete: bool) -> Result<(), AnyError> {
    let report = plamenu::media_cleanup_worker::reconcile(state, delete).await?;
    println!(
        "scanned {} stored file(s); {} referenced; {} orphan(s)",
        report.scanned, report.referenced, report.orphans
    );
    if delete {
        println!(
            "enqueued {} orphan(s) for deletion — the cleanup worker removes them",
            report.enqueued
        );
    } else if report.orphans > 0 {
        println!("dry run: re-run with --delete to schedule the orphans for removal");
    }
    Ok(())
}

async fn emoji_list(state: &AppState) -> Result<(), AnyError> {
    let all = plamenu_db::custom_emoji::list_local(&state.pool).await?;
    if all.is_empty() {
        println!("no local emoji");
        return Ok(());
    }
    for emoji in all {
        let mut flags = String::new();
        if emoji.disabled {
            flags.push_str(" [disabled]");
        }
        if !emoji.visible_in_picker {
            flags.push_str(" [hidden from picker]");
        }
        println!(
            ":{}: {}{flags}",
            emoji.shortcode,
            emoji.image_file_name.as_deref().unwrap_or(""),
        );
    }
    Ok(())
}

async fn account_alias(state: &AppState, cmd: AliasCommand) -> Result<(), AnyError> {
    match cmd {
        AliasCommand::Add { username, alias } => {
            let uri = migration::add_local_alias(state, &username, &alias)
                .await
                .map_err(plamenu::error::ApiError::from)?;
            println!("alias added to @{username}: {uri}");
        }
        AliasCommand::List { username } => {
            let aliases = migration::list_local_aliases(state, &username).await?;
            if aliases.is_empty() {
                println!("@{username} has no declared aliases");
            }
            for alias in aliases {
                println!("{alias}");
            }
        }
        AliasCommand::Remove { username, alias } => {
            if migration::remove_local_alias(state, &username, &alias)
                .await
                .map_err(plamenu::error::ApiError::from)?
            {
                println!("alias removed from @{username}: {alias}");
            } else {
                return Err(format!("@{username} has no alias {alias}").into());
            }
        }
    }
    Ok(())
}

async fn account_passwd(
    state: &AppState,
    username: &str,
    email: Option<&str>,
    password: &str,
) -> Result<(), AnyError> {
    let account = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or_else(|| format!("no local account @{username}"))?;
    // Same length policy as every other credential surface.
    plamenu::auth::validate_password(password).map_err(plamenu::auth::PasswordPolicy::message)?;
    let hash = plamenu::auth::hash_password(password)?;
    // Set the credential and sign the account out everywhere in one transaction
    // An operator password reset must not leave a copied token
    // live, and — like every other credential surface — the two writes commit
    // together rather than as independent statements.
    plamenu_db::user::set_credentials_and_revoke(&state.pool, account.id, email, &hash, None)
        .await?;
    match email {
        Some(email) => println!("login credentials set for @{username} ({email})"),
        None => println!("login credentials set for @{username}"),
    }
    Ok(())
}

async fn account_rename(
    state: &AppState,
    username: &str,
    new_username: &str,
) -> Result<(), AnyError> {
    let account = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or_else(|| format!("no local account @{username}"))?;
    let actor_id = account
        .uri
        .clone()
        .ok_or("account has no persisted ActivityPub actor ID")?;
    if !account::rename_local(&state.pool, account.id, new_username).await? {
        return Err(format!("no local account @{username}").into());
    }
    println!("renamed @{username} to @{new_username}; actor remains {actor_id}");
    Ok(())
}

async fn account_set_role(
    state: &AppState,
    username: &str,
    role: Option<&str>,
    clear: bool,
) -> Result<(), AnyError> {
    let account = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or_else(|| format!("no local account @{username}"))?;
    let role_id = if clear {
        None
    } else {
        let name = role.ok_or("a role name is required (or pass --clear)")?;
        let role = plamenu_db::role::find_by_name(&state.pool, name)
            .await?
            .ok_or_else(|| format!("no role named {name:?} (try `plamenu role list`)"))?;
        Some(role.id)
    };
    if !plamenu_db::role::assign_to_account(&state.pool, account.id, role_id).await? {
        return Err(format!("@{username} has no login user to assign a role to").into());
    }
    match role {
        Some(name) if !clear => println!("granted role {name} to @{username}"),
        _ => println!("cleared the role of @{username}"),
    }
    Ok(())
}

async fn role_list(state: &AppState) -> Result<(), AnyError> {
    for role in plamenu_db::role::list(&state.pool).await? {
        println!(
            "{} (id {}, position {}, permissions {})",
            role.name, role.id, role.position, role.permissions
        );
    }
    Ok(())
}

async fn federation_command(state: &AppState, cmd: FederationCommand) -> Result<(), AnyError> {
    match cmd {
        FederationCommand::Fetch { url } => federation_fetch(state, &url).await,
        FederationCommand::Webfinger { acct } => federation_webfinger(state, &acct).await,
        FederationCommand::Queue(FederationQueueCommand::Inspect) => {
            federation_queue_inspect(state).await
        }
        FederationCommand::Reachability => federation_reachability(state).await,
        FederationCommand::Keys(command) => federation_keys(state, command).await,
    }
}

async fn federation_keys(state: &AppState, command: FederationKeysCommand) -> Result<(), AnyError> {
    match command {
        FederationKeysCommand::Audit => {
            let count = plamenu::key_store::audit_private_storage(state).await?;
            println!("verified {count} encrypted private federation keys; plaintext audit clean");
        }
        FederationKeysCommand::Rewrap { limit } => {
            let count = if let Some(limit) = limit {
                if limit == 0 {
                    return Err("--limit must be positive".into());
                }
                plamenu::key_store::rewrap_batch(state, limit).await?
            } else {
                plamenu::key_store::rewrap_all(state).await?
            };
            println!("rewrapped {count} private federation keys onto the primary version");
        }
        FederationKeysCommand::Contract => {
            if plamenu::key_store::contract_legacy_private_storage(state).await? {
                println!("dropped verified-empty legacy private-key columns");
            } else {
                println!("legacy private-key columns were already absent");
            }
        }
        FederationKeysCommand::RotateAccount {
            username,
            algorithm,
            overlap_hours,
            activation_delay_seconds,
        } => {
            if overlap_hours <= 0 {
                return Err("--overlap-hours must be positive".into());
            }
            if activation_delay_seconds <= 0 {
                return Err("--activation-delay-seconds must be positive".into());
            }
            let account = account::find_local_by_username(&state.pool, &username)
                .await?
                .ok_or_else(|| format!("no local account @{username}"))?;
            let key = plamenu::key_store::rotate_account_key(
                state,
                &account,
                &algorithm,
                time::Duration::hours(overlap_hours),
                time::Duration::seconds(activation_delay_seconds),
            )
            .await?;
            println!(
                "published @{username} {algorithm} rotation: {}; activates in {activation_delay_seconds}s",
                key.key_uri
            );
        }
        FederationKeysCommand::RotateInstance {
            algorithm,
            overlap_hours,
            activation_delay_seconds,
        } => {
            if overlap_hours <= 0 {
                return Err("--overlap-hours must be positive".into());
            }
            if activation_delay_seconds <= 0 {
                return Err("--activation-delay-seconds must be positive".into());
            }
            let key = plamenu::key_store::rotate_instance_key(
                state,
                &algorithm,
                time::Duration::hours(overlap_hours),
                time::Duration::seconds(activation_delay_seconds),
            )
            .await?;
            println!(
                "published instance {algorithm} rotation: {}; activates in {activation_delay_seconds}s; queued update to known remote inboxes",
                key.key_uri
            );
        }
        FederationKeysCommand::Revoke { key_uri } => {
            if !plamenu_db::actor_key::revoke(&state.pool, &key_uri).await? {
                return Err(format!("no active key {key_uri}").into());
            }
            println!("revoked {key_uri}");
        }
        FederationKeysCommand::Retire { key_uri } => {
            if !plamenu_db::actor_key::retire(&state.pool, &key_uri).await? {
                return Err(format!("no unretired key {key_uri}").into());
            }
            println!("retired {key_uri}");
        }
    }
    Ok(())
}

/// `plamenu federation fetch <url>` — signed GET of a remote AP object, dumped
/// as pretty JSON. Uses the permalink-following fetch so an operator can paste
/// a human status/actor URL, not just a canonical id.
async fn federation_fetch(state: &AppState, url: &str) -> Result<(), AnyError> {
    let object = match state.federation.fetch_object_following(url).await {
        Ok(object) => object,
        // A diagnostics command needs the underlying error, not the pre-flight
        // refusal — probe past the budget like the federation-debug page does
        // (a success clears the suppression).
        Err(plamenu_federation::FederationError::FetchSuppressed(key)) => {
            eprintln!("note: suppressed by the finite failure budget for {key}; probing past it");
            state
                .federation
                .fetch_object_following_ignoring_budget(url)
                .await?
        }
        Err(error) => return Err(error.into()),
    };
    println!("{}", serde_json::to_string_pretty(&object)?);
    Ok(())
}

/// `plamenu federation webfinger <acct>` — resolve a handle to its advertised
/// actor(s) without touching the database (a read-only alternative to the
/// account-creating resolve path used during a follow).
async fn federation_webfinger(state: &AppState, acct: &str) -> Result<(), AnyError> {
    let parsed: Acct = acct.parse()?;
    let resolved = state.federation.resolve_acct(&parsed).await?;
    println!("acct:    {}", resolved.acct);
    println!("actor:   {}", resolved.actor_uri);
    if resolved.candidates.len() > 1
        || resolved
            .candidates
            .first()
            .is_some_and(|c| c.advertised_type.is_some())
    {
        println!("candidates:");
        for candidate in &resolved.candidates {
            let kind = candidate
                .advertised_type
                .as_deref()
                .unwrap_or("(unspecified)");
            println!("  {} [{kind}]", candidate.actor_uri);
        }
    }
    Ok(())
}

/// `plamenu federation queue inspect` — a read-only snapshot of the outbound
/// delivery queue: totals, the next fire time, and the worst per-host backlogs
/// (pairs with `federation reachability` to explain a stuck host).
async fn federation_queue_inspect(state: &AppState) -> Result<(), AnyError> {
    let pending = plamenu_db::job::pending_count(&state.pool).await?;
    let due = plamenu_db::job::due_count(&state.pool).await?;
    println!("delivery queue: {pending} queued, {due} due now");
    match plamenu_db::job::next_due_delay(&state.pool).await? {
        None => println!("next job:       (queue empty)"),
        Some(delay) if delay.is_zero() => println!("next job:       due now"),
        Some(delay) => println!("next job:       due in {}", human_secs(delay.as_secs_f64())),
    }
    let hosts = plamenu_db::job::queue_by_host(&state.pool, 20).await?;
    if !hosts.is_empty() {
        println!("\ntop {} host(s) by backlog:", hosts.len());
        for host in hosts {
            println!(
                "  {:<40} {:>5} job(s), max {} attempt(s), next in {}",
                host.host,
                host.jobs,
                host.max_attempts,
                human_secs(host.next_due_seconds.unwrap_or(0.0)),
            );
        }
    }
    Ok(())
}

/// `plamenu federation reachability` — the circuit breaker's open hosts:
/// those with a failure streak long enough that low-value fan-out is being
/// skipped, newest-broken last.
async fn federation_reachability(state: &AppState) -> Result<(), AnyError> {
    let hosts = plamenu_db::reachability::unreachable_hosts(&state.pool).await?;
    if hosts.is_empty() {
        println!("no hosts are currently marked unreachable");
        return Ok(());
    }
    println!("{} host(s) marked unreachable:", hosts.len());
    for host in hosts {
        let state_note = if host.abandoned_at.is_some() {
            " (abandoned)"
        } else {
            ""
        };
        let class = host.last_failure_class.as_deref().unwrap_or("?");
        print!(
            "  {}{state_note}: {} consecutive failures, last {class}",
            host.host, host.consecutive_failures
        );
        if let Some(error) = &host.last_error {
            print!(" — {error}");
        }
        println!();
    }
    Ok(())
}

/// A compact human duration for the queue/reachability CLI output.
fn human_secs(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

/// Mastodon's `Rule::TEXT_SIZE_LIMIT`.
const RULE_TEXT_LIMIT: usize = 300;

async fn rule_command(state: &AppState, cmd: RuleCommand) -> Result<(), AnyError> {
    match cmd {
        RuleCommand::List => rule_list(state).await,
        RuleCommand::Add { text, hint } => rule_add(state, &text, &hint).await,
        RuleCommand::Edit {
            id,
            text,
            hint,
            priority,
        } => rule_edit(state, id, text.as_deref(), hint.as_deref(), priority).await,
        RuleCommand::Remove { id } => rule_remove(state, id).await,
    }
}

async fn rule_list(state: &AppState) -> Result<(), AnyError> {
    let rules = plamenu_db::rule::list_ordered(&state.pool).await?;
    if rules.is_empty() {
        println!("(no rules)");
    }
    for rule in rules {
        println!("id {} (priority {}): {}", rule.id, rule.priority, rule.text);
        if !rule.hint.is_empty() {
            println!("    {}", rule.hint);
        }
    }
    Ok(())
}

async fn rule_add(state: &AppState, text: &str, hint: &str) -> Result<(), AnyError> {
    if text.trim().is_empty() {
        return Err("rule text must not be empty".into());
    }
    if text.chars().count() > RULE_TEXT_LIMIT {
        return Err(format!("rule text exceeds {RULE_TEXT_LIMIT} characters").into());
    }
    let rule = plamenu_db::rule::create(&state.pool, text, hint, None).await?;
    println!("created rule {} (priority {})", rule.id, rule.priority);
    Ok(())
}

async fn rule_edit(
    state: &AppState,
    id: i64,
    text: Option<&str>,
    hint: Option<&str>,
    priority: Option<i32>,
) -> Result<(), AnyError> {
    if let Some(text) = text {
        if text.trim().is_empty() {
            return Err("rule text must not be empty".into());
        }
        if text.chars().count() > RULE_TEXT_LIMIT {
            return Err(format!("rule text exceeds {RULE_TEXT_LIMIT} characters").into());
        }
    }
    match plamenu_db::rule::update(&state.pool, id, text, hint, priority).await? {
        Some(rule) => println!("updated rule {}", rule.id),
        None => return Err(format!("no live rule with id {id}").into()),
    }
    Ok(())
}

async fn rule_remove(state: &AppState, id: i64) -> Result<(), AnyError> {
    if plamenu_db::rule::delete(&state.pool, id).await? {
        println!("removed rule {id}");
    } else {
        return Err(format!("no live rule with id {id}").into());
    }
    Ok(())
}

async fn announcement_command(state: &AppState, cmd: AnnouncementCommand) -> Result<(), AnyError> {
    use plamenu_db::announcement::{self, NewAnnouncement};

    match cmd {
        AnnouncementCommand::List => {
            let announcements = announcement::list_all(&state.pool).await?;
            if announcements.is_empty() {
                println!("(no announcements)");
            }
            for ann in announcements {
                let state_label = if ann.published {
                    "published".to_owned()
                } else {
                    match ann.scheduled_at {
                        Some(at) => format!("scheduled for {at}"),
                        None => "unpublished".to_owned(),
                    }
                };
                println!("id {} ({state_label}): {}", ann.id, ann.text);
            }
        }
        AnnouncementCommand::Add { text, scheduled_at } => {
            if text.trim().is_empty() {
                return Err("announcement text must not be empty".into());
            }
            let scheduled_at = scheduled_at
                .map(|raw| {
                    time::OffsetDateTime::parse(
                        &raw,
                        &time::format_description::well_known::Rfc3339,
                    )
                })
                .transpose()
                .map_err(|e| format!("invalid --scheduled-at (expected RFC 3339): {e}"))?;
            let ann = announcement::create(
                &state.pool,
                NewAnnouncement {
                    text: &text,
                    scheduled_at,
                    ..Default::default()
                },
            )
            .await?;
            let state_label = if ann.published {
                "published"
            } else {
                "scheduled"
            };
            if ann.published {
                plamenu::streaming::announcement_published(state, ann.id).await;
            }
            println!("created announcement {} ({state_label})", ann.id);
        }
        AnnouncementCommand::Publish { id } => {
            match announcement::publish(&state.pool, id).await? {
                Some(ann) => {
                    plamenu::streaming::announcement_published(state, ann.id).await;
                    println!("published announcement {}", ann.id);
                }
                None => return Err(format!("no announcement with id {id}").into()),
            }
        }
        AnnouncementCommand::Unpublish { id } => {
            match announcement::unpublish(&state.pool, id).await? {
                Some(ann) => {
                    plamenu::streaming::announcement_deleted(state, ann.id).await;
                    println!("unpublished announcement {}", ann.id);
                }
                None => return Err(format!("no announcement with id {id}").into()),
            }
        }
        AnnouncementCommand::Remove { id } => {
            if announcement::delete(&state.pool, id).await? {
                plamenu::streaming::announcement_deleted(state, id).await;
                println!("removed announcement {id}");
            } else {
                return Err(format!("no announcement with id {id}").into());
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod cli_documentation_tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn documented_operator_commands_parse() {
        let examples = [
            vec!["plamenu", "--config", "plamenu.toml", "config", "generate"],
            vec![
                "plamenu",
                "--config",
                "/etc/plamenu/plamenu.toml",
                "account",
                "add",
                "alice",
                "--email",
                "alice@example.com",
                "--password",
                "a-long-unique-password",
            ],
            vec!["plamenu", "account", "set-role", "alice", "Owner"],
            vec![
                "plamenu",
                "account",
                "passwd",
                "alice",
                "--password",
                "new-password",
            ],
            vec!["plamenu", "role", "list"],
            vec!["plamenu", "federation", "webfinger", "alice@example.com"],
            vec![
                "plamenu",
                "federation",
                "fetch",
                "https://example.com/@alice",
            ],
            vec!["plamenu", "federation", "queue", "inspect"],
            vec!["plamenu", "federation", "reachability"],
            vec!["plamenu", "federation", "keys", "audit"],
            vec!["plamenu", "media", "reconcile", "--delete"],
        ];

        for argv in examples {
            Cli::try_parse_from(&argv).unwrap_or_else(|error| {
                panic!("documented command failed to parse: {argv:?}: {error}")
            });
        }
    }
}

#[cfg(test)]
mod supervisor_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use plamenu::workers::WorkerRegistry;
    use tokio_util::sync::CancellationToken;

    use super::supervise_with;

    #[tokio::test]
    async fn panicked_worker_is_restarted() {
        // This exercises the unwind-dependent recovery path: the panicking task
        // resolves its `JoinHandle` to an error the supervisor observes. The test
        // and bench profiles always unwind, so this path is reachable here; the
        // *release* profile is separately pinned to unwind by the `compile_error!`
        // guard above and the `release_panic_strategy` integration test (#19).
        let registry = Arc::new(WorkerRegistry::default());
        let starts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&starts);
        let supervisor = tokio::spawn(supervise_with(
            "test",
            Arc::clone(&registry),
            CancellationToken::new(),
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async { panic!("deliberate worker panic") })
            },
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            while starts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervisor restarted the panicked worker");
        supervisor.abort();
        // Each exit was recorded, so the crash loop is visible to the
        // readiness probe (#23).
        assert!(!registry.restart_looping().is_empty() || starts.load(Ordering::SeqCst) < 3);
    }

    /// On shutdown the supervisor awaits a worker that observes
    /// the cancellation signal — it neither aborts a cooperative worker nor
    /// detaches it to run past shutdown.
    #[tokio::test]
    async fn shutdown_drains_a_cooperative_worker() {
        let shutdown = CancellationToken::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let worker_token = shutdown.clone();
        let worker_finished = Arc::clone(&finished);
        let supervisor = tokio::spawn(supervise_with(
            "test",
            Arc::new(WorkerRegistry::default()),
            shutdown.clone(),
            move || {
                let token = worker_token.clone();
                let finished = Arc::clone(&worker_finished);
                tokio::spawn(async move {
                    token.cancelled().await;
                    // The cooperative exit path: finish cleanly, no abort.
                    finished.fetch_add(1, Ordering::SeqCst);
                })
            },
        ));
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), supervisor)
            .await
            .expect("supervisor drained and returned")
            .expect("supervisor did not panic");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "the worker ran to completion instead of being aborted"
        );
    }

    /// A worker that exits *during* shutdown is not respawned and not counted
    /// as a failure loop.
    #[tokio::test]
    async fn shutdown_stops_respawning() {
        let shutdown = CancellationToken::new();
        let starts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&starts);
        shutdown.cancel();
        let supervisor = tokio::spawn(supervise_with(
            "test",
            Arc::new(WorkerRegistry::default()),
            shutdown.clone(),
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async {})
            },
        ));
        tokio::time::timeout(Duration::from_secs(3), supervisor)
            .await
            .expect("supervisor returned")
            .expect("supervisor did not panic");
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "no respawn after shutdown"
        );
    }
}
