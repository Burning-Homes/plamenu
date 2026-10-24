mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
};
use common::{StubFederation, create_local_account, test_state_with_store};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    auth::hash_secret,
    federation::BoxFuture,
    storage::{MediaRead, MediaReader, MediaStore, MemoryStore},
};
use plamenu_db::{PgPool, id, oauth, user};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tower::ServiceExt;

struct HeldReader {
    inner: Box<dyn MediaReader>,
    hold: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    started: Arc<tokio::sync::Notify>,
    announced: bool,
}
impl AsyncRead for HeldReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if !this.announced {
            this.started.notify_one();
            this.announced = true;
        }
        if let Some(hold) = &mut this.hold {
            if hold.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.hold = None;
        }
        Pin::new(this.inner.as_mut()).poll_read(cx, buffer)
    }
}
impl AsyncSeek for HeldReader {
    fn start_seek(self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(self.get_mut().inner.as_mut()).start_seek(position)
    }
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(self.get_mut().inner.as_mut()).poll_complete(cx)
    }
}
struct HeldStore {
    inner: MemoryStore,
    gate: Arc<tokio::sync::Semaphore>,
    started: Arc<tokio::sync::Notify>,
}
impl Default for HeldStore {
    fn default() -> Self {
        Self {
            inner: MemoryStore::default(),
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
            started: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
impl MediaStore for HeldStore {
    fn put(&self, key: &str, bytes: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.put(key, bytes)
    }
    fn put_file(&self, key: &str, path: &std::path::Path) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        self.inner.get(key)
    }
    fn delete(&self, key: &str) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.delete(key)
    }
    fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
        self.inner.list()
    }
    fn open(&self, key: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
        let key = key.to_owned();
        Box::pin(async move {
            let source = self.inner.open(&key).await?;
            let gate = self.gate.clone();
            Ok(MediaRead {
                len: source.len,
                reader: Box::new(HeldReader {
                    inner: source.reader,
                    hold: Some(Box::pin(async move {
                        gate.acquire_owned().await.unwrap().forget();
                    })),
                    started: self.started.clone(),
                    announced: false,
                }),
            })
        })
    }
}
struct Fixture {
    state: AppState,
    store: Arc<HeldStore>,
    owner: i64,
    user: i64,
    asset: i64,
    token: String,
    token_id: i64,
    app: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let login = user::create(
        &pool,
        owner,
        Some("alice@example.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let store = Arc::new(HeldStore::default());
    let state = test_state_with_store(pool, StubFederation::with_actors([]), store.clone());
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Body fixture",
            website: None,
            client_id: "body-fixture",
            client_secret_hash: "hash",
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
            scopes: "read write",
        },
    )
    .await
    .unwrap()
    .id;
    let token = "body-token".to_owned();
    let credential = oauth::create_token(
        &state.pool,
        &hash_secret(&token),
        app,
        Some(login.id),
        "read",
    )
    .await
    .unwrap();
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,visibility,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) VALUES($1,$2,$3,'Track','public',$4,$5,'wav',2048,2048,$6,2)")
        .bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    for key in [format!("music{asset}.orig"), format!("music{asset}.mp3")] {
        store.inner.put(&key, vec![b'a'; 2048]).await.unwrap();
    }
    Fixture {
        state,
        store,
        owner,
        user: login.id,
        asset,
        token,
        token_id: credential.id,
        app,
    }
}
async fn get(f: &Fixture, path: String, token: Option<&str>) -> Response {
    let mut request = Request::get(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    plamenu::build_router(f.state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
async fn held(f: &Fixture, response: Response) -> tokio::task::JoinHandle<Result<(), axum::Error>> {
    assert!(response.status().is_success());
    let body = tokio::spawn(async move { response.into_body().collect().await.map(|_| ()) });
    f.store.started.notified().await;
    body
}
async fn release(
    f: &Fixture,
    body: tokio::task::JoinHandle<Result<(), axum::Error>>,
    allowed: bool,
) {
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    f.store.gate.add_permits(1);
    assert_eq!(body.await.unwrap().is_ok(), allowed);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_original_and_stream_bodies_recheck_current_credential_after_a_stalled_read(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    for suffix in ["download", "stream"] {
        let response = get(
            &f,
            format!("/music/assets/{}/{suffix}", f.asset),
            Some(&f.token),
        )
        .await;
        let body = held(&f, response).await;
        sqlx::query("UPDATE oauth_tokens SET revoked_at=now() WHERE id=$1")
            .bind(f.token_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        release(&f, body, false).await;
        sqlx::query("UPDATE oauth_tokens SET revoked_at=NULL WHERE id=$1")
            .bind(f.token_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn anonymous_embed_and_native_stream_stop_when_the_public_source_becomes_private(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    for path in [
        format!("/music/assets/{}/stream", f.asset),
        format!("/music/embed/assets/{}/audio", f.asset),
    ] {
        let response = get(&f, path, None).await;
        let body = held(&f, response).await;
        sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
            .bind(f.asset)
            .execute(&f.state.pool)
            .await
            .unwrap();
        release(&f, body, false).await;
        sqlx::query("UPDATE music_assets SET visibility='public',revision=revision+1 WHERE id=$1")
            .bind(f.asset)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn share_original_body_rechecks_revocation_after_headers_and_does_not_widen_to_public_access(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let link = plamenu::music_shares::create(
        &f.state,
        f.owner,
        &plamenu::music_shares::New {
            reference: plamenu::music_saved::Reference {
                kind: plamenu::music_saved::Kind::Track,
                id: f.asset.to_string(),
            },
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let response = get(
        &f,
        format!(
            "/music/share/assets/{}/download?token={}",
            f.asset, link.token
        ),
        None,
    )
    .await;
    let body = held(&f, response).await;
    plamenu::music_shares::revoke(&f.state, f.owner, link.id.parse().unwrap())
        .await
        .unwrap();
    release(&f, body, false).await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn scope_reduction_disabled_user_and_web_session_age_stop_bodies_but_foreign_token_age_does_not(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let response = get(
        &f,
        format!("/music/assets/{}/download", f.asset),
        Some(&f.token),
    )
    .await;
    let body = held(&f, response).await;
    sqlx::query("UPDATE oauth_tokens SET scopes='read:accounts' WHERE id=$1")
        .bind(f.token_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    release(&f, body, false).await;
    sqlx::query("UPDATE oauth_tokens SET scopes='read' WHERE id=$1")
        .bind(f.token_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let response = get(
        &f,
        format!("/music/assets/{}/download", f.asset),
        Some(&f.token),
    )
    .await;
    let body = held(&f, response).await;
    sqlx::query("UPDATE users SET disabled=true WHERE id=$1")
        .bind(f.user)
        .execute(&f.state.pool)
        .await
        .unwrap();
    release(&f, body, false).await;
    sqlx::query("UPDATE users SET disabled=false WHERE id=$1")
        .bind(f.user)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let web_app = f.state.web_app_id().await.unwrap();
    let web = oauth::create_token(
        &f.state.pool,
        &hash_secret("web-body-token"),
        web_app,
        Some(f.user),
        "read",
    )
    .await
    .unwrap();
    let response = get(
        &f,
        format!("/music/assets/{}/download", f.asset),
        Some("web-body-token"),
    )
    .await;
    let body = held(&f, response).await;
    sqlx::query("UPDATE oauth_tokens SET created_at=now()-interval '91 days' WHERE id=$1")
        .bind(web.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    release(&f, body, false).await;
    assert_ne!(web_app, f.app);
    sqlx::query("UPDATE oauth_tokens SET created_at=now()-interval '91 days' WHERE id=$1")
        .bind(f.token_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let response = get(
        &f,
        format!("/music/assets/{}/download", f.asset),
        Some(&f.token),
    )
    .await;
    let body = held(&f, response).await;
    release(&f, body, true).await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn head_and_ranges_preserve_headers_and_shutdown_drops_a_stalled_body_without_bytes(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::head(format!("/music/assets/{}/download", f.asset))
                .header("authorization", format!("Bearer {}", f.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "2048");
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    let response = app
        .oneshot(
            Request::get(format!("/music/assets/{}/download", f.asset))
                .header("range", "bytes=5-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 5-9/2048");
    f.store.gate.add_permits(1);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"aaaaa"
    );
    let response = get(&f, format!("/music/assets/{}/download", f.asset), None).await;
    let body = held(&f, response).await;
    f.state.shutdown.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), body)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn ready_rendition_bodies_keep_source_and_exact_authenticated_viewer_authority_after_headers(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let selection = plamenu::music_renditions::negotiate(&f.state, Some(f.owner), "mp3", Some(128))
        .await
        .unwrap();
    let row = plamenu::music_renditions::request(&f.state, f.asset, Some(f.owner), selection)
        .await
        .unwrap();
    let file = format!("musicrend{}.mp3", id::next());
    f.store.inner.put(&file, vec![b'a'; 2048]).await.unwrap();
    sqlx::query("UPDATE music_renditions SET state='ready',file_name=$2,size_bytes=2048,actual_bitrate=128,cleanup_job_id=$3 WHERE id=$1").bind(row.id).bind(file).bind(id::next()).execute(&f.state.pool).await.unwrap();
    let path = format!("/music/assets/{}/renditions/{}/stream", f.asset, row.id);
    let response = get(&f, path.clone(), Some(&f.token)).await;
    let body = held(&f, response).await;
    sqlx::query("UPDATE oauth_tokens SET revoked_at=now() WHERE id=$1")
        .bind(f.token_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    release(&f, body, false).await;
    sqlx::query("UPDATE oauth_tokens SET revoked_at=NULL WHERE id=$1")
        .bind(f.token_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let response = get(&f, path, None).await;
    let body = held(&f, response).await;
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    release(&f, body, false).await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_and_anonymous_embed_artwork_bodies_recheck_the_parent_permission_after_delayed_reads(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let art = id::next();
    let file = format!("musicart{art}.png");
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let bytes = png.into_inner();
    let size = i64::try_from(bytes.len()).unwrap();
    f.store.inner.put(&file, bytes).await.unwrap();
    sqlx::query("INSERT INTO music_artworks(id,owner_id,asset_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'image',$4,'image/png',$5,2,2,$6)").bind(art).bind(f.owner).bind(f.asset).bind(file).bind(size).bind(id::next()).execute(&f.state.pool).await.unwrap();
    for path in [
        format!("/music/artwork/{art}"),
        format!("/music/embed/artwork/{art}"),
    ] {
        let response = get(&f, path, None).await;
        let body = held(&f, response).await;
        sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
            .bind(f.asset)
            .execute(&f.state.pool)
            .await
            .unwrap();
        release(&f, body, false).await;
        sqlx::query("UPDATE music_assets SET visibility='public',revision=revision+1 WHERE id=$1")
            .bind(f.asset)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
}
