#![cfg(unix)]
mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    auth::hash_password,
    music_local::{
        config::RootConfig,
        files::{self, Mutation},
        folders, journal, worker,
    },
};
use plamenu_db::{PgPool, id, user};
use serde_json::{Value, json};
use sqlx::ConnectOptions;
use std::{io::Write, path::Path, sync::Arc};
use tokio::io::{AsyncBufReadExt, BufReader};
struct Fixture {
    state: AppState,
    owner: i64,
    folder: folders::Folder,
    file: i64,
    container: tempfile::TempDir,
}
fn configure(state: &mut AppState, mount: &Path) {
    Arc::make_mut(&mut state.config)
        .music_local_roots
        .push(RootConfig {
            id: "archive".into(),
            label: "Mutation test".into(),
            path: mount.into(),
            allow_tag_writeback: true,
            allow_rename: true,
            allow_delete: true,
        });
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, &format!("m{}", id::next()), "Owner")
        .await
        .id;
    user::create(
        &pool,
        owner,
        Some(&format!("{owner}@example.test")),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let mut state = test_state_with(pool, StubFederation::with_actors([]));
    let container = tempfile::tempdir().unwrap();
    let mount = container.path().join("mount");
    std::fs::create_dir(&mount).unwrap();
    configure(&mut state, &mount);
    let result = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.2",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(mount.join("source.wav"))
        .status()
        .await
        .unwrap();
    assert!(result.success());
    let folder = folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Mutation library".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let claimed = worker::claim(&state).await.unwrap().unwrap();
    worker::process(&state, &claimed).await.unwrap();
    let folder = folders::get(&state, owner, folder.id).await.unwrap();
    let file = files::list(&state, owner, folder.id, None).await.unwrap()[0].id;
    Fixture {
        state,
        owner,
        folder,
        file,
        container,
    }
}
fn input(operation: &str, revision: i64) -> Mutation {
    match operation {
        "rename" => Mutation::Rename {
            revision,
            name: "renamed.wav".into(),
        },
        "delete" => Mutation::Delete {
            revision,
            confirm: true,
        },
        "tags" => Mutation::WriteTags { revision },
        _ => panic!("unknown operation"),
    }
}
async fn reject_file_updates(pool: &PgPool) {
    sqlx::query("CREATE FUNCTION reject_mutation_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture commit failure'; END $$").execute(pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_mutation_update BEFORE UPDATE ON music_local_files FOR EACH ROW EXECUTE FUNCTION reject_mutation_update()").execute(pool).await.unwrap();
}
async fn allow_file_updates(pool: &PgPool) {
    sqlx::query("DROP TRIGGER reject_mutation_update ON music_local_files")
        .execute(pool)
        .await
        .unwrap();
}
async fn pending(f: &Fixture) -> (i64, Value) {
    sqlx::query_as(
        "SELECT id,to_jsonb(intent) AS intent FROM music_local_mutations WHERE folder_id=$1",
    )
    .bind(f.folder.id)
    .fetch_one(&f.state.pool)
    .await
    .unwrap()
}
#[test]
fn mutation_process_child() {
    let Ok(raw) = std::env::var("PLAMENU_MUTATION_TEST") else {
        return;
    };
    let data: Value = serde_json::from_str(&raw).unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let pool = PgPool::connect(data["database"].as_str().unwrap())
            .await
            .unwrap();
        let mut state = test_state_with(pool, StubFederation::with_actors([]));
        configure(&mut state, Path::new(data["mount"].as_str().unwrap()));
        let result = files::mutate_with_launcher(
            &state,
            data["owner"].as_i64().unwrap(),
            data["folder"].as_i64().unwrap(),
            data["file"].as_i64().unwrap(),
            &input(
                data["operation"].as_str().unwrap(),
                data["revision"].as_i64().unwrap(),
            ),
            Path::new(env!("CARGO_BIN_EXE_plamenu")),
        )
        .await;
        assert!(result.is_err());
    });
    println!("filesystem-ready");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}
async fn killed_after_filesystem(f: &Fixture, operation: &str) {
    let data = json!({"database":f.state.pool.connect_options().to_url_lossy().to_string(),"mount":f.container.path().join("mount"),"owner":f.owner,"folder":f.folder.id,"file":f.file,"revision":f.folder.revision,"operation":operation});
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mutation_process_child", "--nocapture"])
        .env("PLAMENU_MUTATION_TEST", data.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line == "filesystem-ready" {
                return true;
            }
        }
        false
    })
    .await;
    child.start_kill().unwrap();
    child.wait().await.unwrap();
    assert!(
        ready.unwrap_or(false),
        "mutation child failed before durable filesystem boundary"
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn process_death_and_database_failure_recover_rename_delete_and_tag_exchange(pool: PgPool) {
    for operation in ["rename", "delete", "tags"] {
        let f = fixture(pool.clone()).await;
        let mount = f.container.path().join("mount");
        let before = std::fs::read(mount.join("source.wav")).unwrap();
        reject_file_updates(&f.state.pool).await;
        killed_after_filesystem(&f, operation).await;
        let (mid, proof) = pending(&f).await;
        assert!(worker::claim(&f.state).await.unwrap().is_none());
        assert!(
            folders::refresh(
                &f.state,
                f.owner,
                f.folder.id,
                f.folder.revision + 1,
                false,
                false
            )
            .await
            .is_err()
        );
        allow_file_updates(&f.state.pool).await;
        journal::recover(&f.state, mid).await.unwrap();
        journal::recover(&f.state, mid).await.unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM music_local_mutations WHERE folder_id=$1")
                .bind(f.folder.id)
                .fetch_one(&f.state.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
        let row = files::list(&f.state, f.owner, f.folder.id, None)
            .await
            .unwrap()
            .remove(0);
        match operation {
            "rename" => {
                assert_eq!(row.relative_path, "renamed.wav");
                assert_eq!(std::fs::read(mount.join("renamed.wav")).unwrap(), before);
            }
            "delete" => {
                assert!(!row.available);
                assert!(!mount.join("source.wav").exists());
            }
            "tags" => {
                assert!(row.available);
                assert!(mount.join("source.wav").exists());
                assert!(!mount.join(proof["target"].as_str().unwrap()).exists());
            }
            _ => unreachable!(),
        }
        // Retire this fixture's watcher before testing the next independent mount.
        let current = folders::get(&f.state, f.owner, f.folder.id).await.unwrap();
        folders::refresh(
            &f.state,
            f.owner,
            current.id,
            current.revision,
            false,
            false,
        )
        .await
        .unwrap();
        sqlx::query("DROP FUNCTION reject_mutation_update()")
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
}
#[sqlx::test(migrations = "../db/migrations")]
async fn an_uncommitted_intent_never_changes_the_source(pool: PgPool) {
    let f = fixture(pool).await;
    let mount = f.container.path().join("mount");
    let bytes = std::fs::read(mount.join("source.wav")).unwrap();
    sqlx::query("CREATE FUNCTION reject_intent() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture intent failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_intent BEFORE INSERT ON music_local_mutations FOR EACH ROW EXECUTE FUNCTION reject_intent()").execute(&f.state.pool).await.unwrap();
    for operation in ["rename", "delete", "tags"] {
        assert!(
            files::mutate_with_launcher(
                &f.state,
                f.owner,
                f.folder.id,
                f.file,
                &input(operation, f.folder.revision),
                Path::new(env!("CARGO_BIN_EXE_plamenu"))
            )
            .await
            .is_err()
        );
        assert!(!std::fs::read_dir(&mount).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".plamenu-")
        }));
    }
    assert_eq!(std::fs::read(mount.join("source.wav")).unwrap(), bytes);
    assert!(!mount.join("renamed.wav").exists());
    assert_eq!(
        folders::get(&f.state, f.owner, f.folder.id)
            .await
            .unwrap()
            .revision,
        f.folder.revision
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn recovery_refuses_a_replaced_mount_and_does_not_touch_substitute_files(pool: PgPool) {
    let f = fixture(pool).await;
    reject_file_updates(&f.state.pool).await;
    killed_after_filesystem(&f, "rename").await;
    allow_file_updates(&f.state.pool).await;
    let (mid, _) = pending(&f).await;
    let mount = f.container.path().join("mount");
    let old = f.container.path().join("old-mount");
    std::fs::rename(&mount, &old).unwrap();
    std::fs::create_dir(&mount).unwrap();
    std::fs::write(mount.join("source.wav"), b"unrelated source").unwrap();
    std::fs::write(mount.join("renamed.wav"), b"unrelated target").unwrap();
    assert!(journal::recover(&f.state, mid).await.is_err());
    journal::run_due(&f.state).await.unwrap();
    let failed = folders::get(&f.state, f.owner, f.folder.id).await.unwrap();
    assert_eq!(failed.state, "failed");
    assert!(failed.last_error.unwrap().contains("awaits recovery"));
    assert_eq!(
        std::fs::read(mount.join("source.wav")).unwrap(),
        b"unrelated source"
    );
    assert_eq!(
        std::fs::read(mount.join("renamed.wav")).unwrap(),
        b"unrelated target"
    );
    assert!(worker::claim(&f.state).await.unwrap().is_none());
    std::fs::rename(&mount, f.container.path().join("substitute")).unwrap();
    std::fs::rename(&old, &mount).unwrap();
    journal::recover(&f.state, mid).await.unwrap();
}
#[sqlx::test(migrations = "../db/migrations")]
async fn removing_the_index_rolls_back_a_pending_physical_delete_instead_of_erasing_operator_audio(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let mount = f.container.path().join("mount");
    let bytes = std::fs::read(mount.join("source.wav")).unwrap();
    reject_file_updates(&f.state.pool).await;
    killed_after_filesystem(&f, "delete").await;
    allow_file_updates(&f.state.pool).await;
    let (mid, _) = pending(&f).await;
    folders::disconnect(&f.state, f.owner, f.folder.id, f.folder.revision + 1)
        .await
        .unwrap();
    journal::recover(&f.state, mid).await.unwrap();
    assert_eq!(std::fs::read(mount.join("source.wav")).unwrap(), bytes);
}
