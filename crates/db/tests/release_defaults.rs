use plamenu_db::{
    MIGRATOR, PgPool, account, custom_emoji, instance_settings, remote_history, role, user,
};

#[sqlx::test(migrations = false)]
async fn upgrade_preserves_custom_settings_and_existing_user_preferences(pool: PgPool) {
    MIGRATOR.run_to(71, &pool).await.unwrap();
    let account = account::create_local(
        &pool,
        account::NewLocalAccount {
            username: "alice",
            display_name: "",
            note: "",
            public_key_pem: "pub",
        },
    )
    .await
    .unwrap();
    let existing = user::create(&pool, account.id, Some("alice@example.com"), "$argon2id$x")
        .await
        .unwrap();
    sqlx::raw_sql(
        "UPDATE instance_settings SET max_characters = 8192,
             media_remote_full_processing = 'jpeg', media_remote_gif_handling = 'gifv',
             media_avif_quality = 85;
         UPDATE custom_emoji_settings SET max_file_size_kb = 1024;
         UPDATE remote_history_settings SET retention_days = 45;
         UPDATE user_roles SET permissions = 0 WHERE id = 2;",
    )
    .execute(&pool)
    .await
    .unwrap();

    MIGRATOR.run(&pool).await.unwrap();
    let settings = instance_settings::get(&pool).await.unwrap();
    assert_eq!(settings.max_characters, 8192);
    assert_eq!(settings.media_remote_full_processing, "jpeg");
    assert_eq!(settings.media_remote_gif_handling, "gifv");
    assert_eq!(settings.media_avif_quality, 85);
    assert_eq!(
        custom_emoji::settings(&pool)
            .await
            .unwrap()
            .max_file_size_kb,
        1024
    );
    let history = remote_history::settings(&pool).await.unwrap();
    assert!(history.enabled);
    assert!(history.bare_iri_enabled);
    assert_eq!(history.retention_days, 45);
    assert_eq!(
        role::find_by_name(&pool, "Admin")
            .await
            .unwrap()
            .unwrap()
            .permissions,
        0
    );
    assert!(
        !user::settings_by_user_id(&pool, existing.id)
            .await
            .unwrap()
            .unwrap()
            .reading_allow_direct_remote_media
    );
    // Startup may run the migrator again without checksum or replay errors.
    MIGRATOR.run(&pool).await.unwrap();
}
