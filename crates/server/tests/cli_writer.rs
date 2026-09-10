//! Real CLI processes must coexist with the server without sharing its IDs.
mod common;

use plamenu_db::{PgPool, account, single_writer};

async fn run_cli(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_plamenu"));
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .kill_on_drop(true);
    tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("CLI must complete without waiting on the server writer lock")
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn online_cli_has_disjoint_ids_and_refuses_a_second_cli(pool: PgPool) {
    let directory = tempfile::tempdir().unwrap();
    let mut database_url = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    database_url.set_path(pool.connect_options().get_database().unwrap());
    let config = directory.path().join("plamenu.toml");
    let settings = serde_json::json!({
        "domain": common::TEST_DOMAIN,
        "database_url": database_url.as_str(),
        "bind": "127.0.0.1:0",
        "media_dir": directory.path().join("media"),
        "encryption_secret": common::test_config().encryption_secret.unwrap().expose(),
    });
    std::fs::write(&config, toml::to_string(&settings).unwrap()).unwrap();
    let server = single_writer::acquire_single_writer_lock(database_url.as_str())
        .await
        .unwrap();
    let output = run_cli(
        &config,
        &[
            "account",
            "add",
            "alice",
            "--email",
            "alice@example.test",
            "--password",
            "Fixture-only-password-2026",
        ],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        alice.id & 1,
        1,
        "the CLI must select its lane before initialization"
    );
    let bob = common::create_local_account(&pool, "bob", "Bob").await;
    assert_eq!(
        bob.id & 1,
        0,
        "the serving library defaults to the server lane"
    );
    let cli = single_writer::acquire_cli_writer_lock(database_url.as_str())
        .await
        .unwrap();
    let output = run_cli(&config, &["role", "list"]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another Plamenu CLI command"));
    cli.release().await.unwrap();
    let output = run_cli(&config, &["role", "list"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.release().await.unwrap();
}
