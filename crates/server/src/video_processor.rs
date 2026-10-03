//! Private processing mode of the same executable. No PostgreSQL, snowflake
//! lane, accounts, federation, public media route or publication permissions.
mod transport;

use crate::{
    config::{Config, NativeVideoConfig, SecretString},
    error::ApiError,
    media_transcode::{PackagedHls, PackagedStoryboard, ProcessedAv},
    sync::RecoverableMutex as _,
    video_offload::protocol::{self, Created, Manifest, Spec, Status},
    video_resources::{ProcessingPool, Reservation},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSettings {
    bind: std::net::SocketAddr,
    token: String,
    directory: PathBuf,
    #[serde(default = "ffmpeg")]
    ffmpeg_path: String,
    #[serde(default = "ffprobe")]
    ffprobe_path: String,
    #[serde(default)]
    native_video: NativeVideoConfig,
    tls: Option<Tls>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tls {
    cert: PathBuf,
    key: PathBuf,
}
fn ffmpeg() -> String {
    "ffmpeg".into()
}
fn ffprobe() -> String {
    "ffprobe".into()
}

/// Native optional setup, with a fresh 256-bit token in a private file rather
/// than process arguments, command output or an origin database credential.
pub fn generate(path: &Path, overwrite: bool) -> Result<(), AnyError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(overwrite);
    if !overwrite {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let token_secret: SecretString = crate::auth::generate_secret().into();
    let token = token_secret.expose();
    write!(
        file,
        "# Optional private processing mode of the installed native binary.\n# This file needs no database or federation encryption secret.\n# Copy this token only to approved origins' [[video_offload]] entries.\n# Plain HTTP is allowed only on literal loopback. Use native TLS remotely.\nbind = \"127.0.0.1:8423\"\ntoken = \"{token}\"\ndirectory = \"./video-worker\"\n# ffmpeg_path = \"ffmpeg\"\n# ffprobe_path = \"ffprobe\"\n# [native_video]\n# jobs = 2\n# max_upload_mib = 2048\n# min_free_gib = 1\n# [tls]\n# cert = \"/etc/letsencrypt/live/processor.example/fullchain.pem\"\n# key = \"/etc/letsencrypt/live/processor.example/privkey.pem\"\n"
    )?;
    file.sync_all()?;
    Ok(())
}

struct Bundle {
    processed: ProcessedAv,
    hls: Vec<PackagedHls>,
    board: PackagedStoryboard,
    poster_path: PathBuf,
    manifest: Manifest,
}
impl Bundle {
    fn path(&self, index: usize) -> Option<PathBuf> {
        match index {
            0 => Some(self.processed.file_path.clone()),
            1 => Some(self.poster_path.clone()),
            2 => Some(self.board.file_path.clone()),
            n if n == self.hls.len() + 3 && self.manifest.progressive.is_some() => self
                .hls
                .iter()
                .find_map(|r| r.progressive.as_ref().map(|p| p.file_path.clone())),
            n => self.hls.get(n - 3).map(|rung| rung.file_path.clone()),
        }
    }
}
#[derive(Clone, Default)]
enum Phase {
    Waiting,
    Uploading,
    Processing,
    Ready(Arc<Bundle>),
    #[default]
    Failed,
}
struct Job {
    spec: Spec,
    phase: Mutex<Phase>,
    lease: Mutex<Option<Instant>>,
    created: Instant,
    cancel: CancellationToken,
    source: tempfile::TempDir,
    _reservation: Reservation,
}
#[derive(Clone)]
struct Worker {
    config: Arc<Config>,
    token: SecretString,
    resources: ProcessingPool,
    jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>,
    completed: Arc<AtomicU64>,
}

/// Configuration has only local tools/budgets/transport and a private token.
/// Startup locks a dedicated scratch root before scavenging exact owned names.
pub async fn serve(path: &Path) -> Result<(), AnyError> {
    let settings: FileSettings = toml::from_str(&std::fs::read_to_string(path)?)
        .map_err(|_| "Invalid private video worker configuration")?;
    if !crate::video_offload::valid_token(&settings.token)
        || (!settings.bind.ip().is_loopback() && settings.tls.is_none())
    {
        return Err(
            "Private workers require a strong token and TLS outside literal loopback".into(),
        );
    }
    let config = Config::processor_tools(
        settings.directory,
        settings.ffmpeg_path,
        settings.ffprobe_path,
        settings.native_video,
    )?;
    let _lock = own_directory(&config.media_dir)?;
    crate::video_worker::scavenge_crash_scratch(&config.media_dir).await?;
    let resources = ProcessingPool::new(
        config.native_video.jobs,
        config.native_video.processing_budget(),
    );
    crate::media_gate::configure(config.native_video.jobs);
    let worker = Worker {
        config: Arc::new(config),
        token: settings.token.into(),
        resources,
        jobs: Arc::default(),
        completed: Arc::default(),
    };
    let stopped = CancellationToken::new();
    let reaper_worker = worker.clone();
    let reaper_stop = stopped.clone();
    let reaper = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = reaper_stop.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(1)) => reap(&reaper_worker),
            }
        }
        let jobs = std::mem::take(&mut *reaper_worker.jobs.lock_or_recover());
        for job in jobs.values() {
            job.cancel.cancel();
        }
    });
    let app = router(worker);
    let result = transport::serve(app, settings.bind, settings.tls).await;
    stopped.cancel();
    reaper.await?;
    result
}

fn own_directory(root: &Path) -> Result<std::fs::File, AnyError> {
    std::fs::create_dir_all(root)?;
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Worker root must be a dedicated real directory".into());
    }
    let marker = root.join(".plamenu-video-worker");
    let known = b"plamenu-private-video-processor-v1\n";
    if !marker.exists() && std::fs::read_dir(root)?.next().is_some() {
        return Err("First worker use requires an empty dedicated directory".into());
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut file) => {
            use std::io::Write;
            if std::fs::read_dir(root)?.count() != 1 {
                return Err("First worker use requires an empty dedicated directory".into());
            }
            file.write_all(known)?;
            file.sync_all()?;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(&marker)?.file_type().is_symlink()
                || std::fs::read(&marker)? != known
            {
                return Err("Worker ownership marker mismatch".into());
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&marker)?
        }
        Err(error) => return Err(Box::new(error)),
    };
    #[cfg(unix)]
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    #[cfg(not(unix))]
    return Err("Private worker requires an exclusive filesystem lock".into());
    match std::fs::symlink_metadata(root.join("tmp")) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err("Worker scratch must be a real owned directory".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Box::new(error)),
    }
    Ok(file)
}

fn router(worker: Worker) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/jobs", post(create))
        .route("/v1/jobs/{id}", get(status).post(renew).delete(cancel))
        .route("/v1/jobs/{id}/source", put(upload))
        .route("/v1/jobs/{id}/artifacts/{index}", get(artifact))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            worker.clone(),
            authorize,
        ))
        .with_state(worker)
}

async fn health(State(worker): State<Worker>) -> Json<serde_json::Value> {
    let (jobs, bytes) = worker.resources.usage();
    Json(
        serde_json::json!({"version":protocol::VERSION,"active_jobs":jobs,"reserved_bytes":bytes,"completed_jobs":worker.completed.load(Ordering::Relaxed)}),
    )
}

async fn authorize(
    State(worker): State<Worker>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use hmac::{Hmac, KeyInit, Mac};
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| crate::video_offload::valid_token(value))
        .is_some_and(|token| {
            let mut expected =
                Hmac::<sha2::Sha256>::new_from_slice(b"plamenu-private-processor-auth-v1")
                    .expect("HMAC key");
            expected.update(worker.token.expose().as_bytes());
            let mut provided =
                Hmac::<sha2::Sha256>::new_from_slice(b"plamenu-private-processor-auth-v1")
                    .expect("HMAC key");
            provided.update(token.as_bytes());
            provided
                .verify_slice(&expected.finalize().into_bytes())
                .is_ok()
        });
    let mut response = if authorized {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "private, no-store".parse().expect("static header"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static header"),
    );
    response
}

fn find(worker: &Worker, id: &str) -> Result<Arc<Job>, ApiError> {
    let job = worker
        .jobs
        .lock_or_recover()
        .get(id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    if job.cancel.is_cancelled()
        || job
            .lease
            .lock_or_recover()
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(protocol::LEASE_SECONDS))
        || job.created.elapsed() >= Duration::from_hours(4)
    {
        return Err(ApiError::NotFound);
    }
    Ok(job)
}
fn reap(worker: &Worker) {
    worker.jobs.lock_or_recover().retain(|_, job| {
        let keep = !job.cancel.is_cancelled()
            && job
                .lease
                .lock_or_recover()
                .is_some_and(|at| at.elapsed() < Duration::from_secs(protocol::LEASE_SECONDS))
            && job.created.elapsed() < Duration::from_hours(4);
        if !keep {
            job.cancel.cancel();
        }
        keep
    });
}
async fn create(
    State(worker): State<Worker>,
    Json(spec): Json<Spec>,
) -> Result<Json<Created>, ApiError> {
    let params = spec.params(&worker.config)?;
    let limit = params.file_size_limit.expect("validated limit");
    let reservation = worker
        .resources
        .try_reserve(limit, spec.source_bytes)
        .ok_or_else(|| ApiError::ServiceUnavailable("Private processor is busy".into()))?;
    let free =
        crate::storage::available_bytes(&worker.config.media_dir).map_err(protocol::io_error)?;
    if free
        < worker.config.native_video.min_free_gib * NativeVideoConfig::GIB
            + worker.resources.usage().1
    {
        return Err(ApiError::ServiceUnavailable(
            "Private processor disk budget is unavailable".into(),
        ));
    }
    let source = crate::media_transcode::scratch_dir(&worker.config).await?;
    let id = crate::media_processing::storage_stem(0);
    let job = Arc::new(Job {
        spec,
        phase: Mutex::new(Phase::Waiting),
        lease: Mutex::new(Some(Instant::now())),
        created: Instant::now(),
        cancel: CancellationToken::new(),
        source,
        _reservation: reservation,
    });
    worker.jobs.lock_or_recover().insert(id.clone(), job);
    Ok(Json(Created {
        id,
        lease_seconds: protocol::LEASE_SECONDS,
    }))
}
async fn renew(
    State(worker): State<Worker>,
    RoutePath(id): RoutePath<String>,
) -> Result<StatusCode, ApiError> {
    let job = find(&worker, &id)?;
    *job.lease.lock_or_recover() = Some(Instant::now());
    Ok(StatusCode::NO_CONTENT)
}
async fn cancel(State(worker): State<Worker>, RoutePath(id): RoutePath<String>) -> StatusCode {
    if let Some(job) = worker.jobs.lock_or_recover().remove(&id) {
        job.cancel.cancel();
    }
    StatusCode::NO_CONTENT
}
async fn status(
    State(worker): State<Worker>,
    RoutePath(id): RoutePath<String>,
) -> Result<Json<Status>, ApiError> {
    let job = find(&worker, &id)?;
    let phase = job.phase.lock_or_recover();
    let (state, manifest) = match &*phase {
        Phase::Waiting => ("waiting", None),
        Phase::Uploading => ("uploading", None),
        Phase::Processing => ("processing", None),
        Phase::Failed => ("failed", None),
        Phase::Ready(bundle) => ("ready", Some(bundle.manifest.clone())),
    };
    Ok(Json(Status {
        state: state.into(),
        manifest,
    }))
}
async fn upload(
    State(worker): State<Worker>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let job = find(&worker, &id)?;
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        != Some(job.spec.source_bytes)
    {
        return Err(protocol::invalid());
    }
    {
        let mut phase = job.phase.lock_or_recover();
        if !matches!(*phase, Phase::Waiting) {
            return Err(ApiError::Conflict(
                "Private source is already submitted".into(),
            ));
        }
        *phase = Phase::Uploading;
    }
    // A dropped HTTP caller cannot prematurely free this write's resources.
    tokio::spawn(async move {
        let received = tokio::select! { () = job.cancel.cancelled() => Err(ApiError::NotFound), result = receive(&job, body) => result };
        if let Err(error) = received { *job.phase.lock_or_recover() = Phase::Failed; return Err(error); }
        *job.phase.lock_or_recover() = Phase::Processing;
        tokio::spawn(async move {
            let result = tokio::select! { () = job.cancel.cancelled() => Err(ApiError::NotFound), result = encode(&worker, &job) => result };
            *job.phase.lock_or_recover() = match result {
                Ok(bundle) if !job.cancel.is_cancelled() => {
                    worker.completed.fetch_add(1,Ordering::Relaxed);
                    Phase::Ready(Arc::new(bundle))
                },
                _ => Phase::Failed,
            };
        });
        Ok(StatusCode::ACCEPTED)
    }).await.map_err(|error| ApiError::Internal(Box::new(error)))?
}
async fn receive(job: &Job, body: Body) -> Result<(), ApiError> {
    let mut file = tokio::fs::File::create(job.source.path().join("source.bin"))
        .await
        .map_err(protocol::io_error)?;
    let mut stream = body.into_data_stream();
    let mut received = 0_u64;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .map_err(|_| protocol::invalid())?
    {
        let chunk = chunk.map_err(|_| protocol::invalid())?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(protocol::invalid)?;
        if received > job.spec.source_bytes || chunk.len() > 1024 * 1024 {
            return Err(ApiError::PayloadTooLarge);
        }
        file.write_all(&chunk).await.map_err(protocol::io_error)?;
    }
    if received != job.spec.source_bytes {
        return Err(protocol::invalid());
    }
    file.flush().await.map_err(protocol::io_error)
}
async fn encode(worker: &Worker, job: &Job) -> Result<Bundle, ApiError> {
    let params = job.spec.params(&worker.config)?;
    let processed = crate::media_transcode::process_native_video_file(
        &worker.config,
        &job.source.path().join("source.bin"),
        &params,
        job.spec.poster_seconds,
    )
    .await?;
    let poster = processed.small.as_ref().ok_or_else(protocol::invalid)?;
    if processed.content_type != "video/mp4" {
        return Err(protocol::invalid());
    }
    let hls = crate::media_transcode::package_native_hls(
        &worker.config,
        &processed,
        &job.spec.stem,
        &params,
    )
    .await?;
    let board = crate::media_transcode::package_native_storyboard(
        &worker.config,
        &processed,
        &job.spec.stem,
        &params,
    )
    .await?;
    let poster_path = processed.scratch.path().join("worker-poster.avif");
    tokio::fs::write(&poster_path, &poster.bytes)
        .await
        .map_err(protocol::io_error)?;
    let mut artifacts = Vec::new();
    for path in [&processed.file_path, &poster_path, &board.file_path]
        .into_iter()
        .chain(hls.iter().map(|rung| &rung.file_path))
    {
        artifacts.push(protocol::digest(path).await?);
    }
    let progressive = (job.spec.version == protocol::VERSION)
        .then(|| hls.iter().find_map(|r| r.progressive.as_ref()))
        .flatten();
    if let Some(p) = progressive {
        artifacts.push(protocol::digest(&p.file_path).await?);
    }
    let manifest = Manifest {
        version: job.spec.version,
        artifacts,
        renditions: hls
            .iter()
            .map(|rung| protocol::Rendition {
                file_name: rung.file_name.clone(),
                width: rung.width,
                height: rung.height,
                bandwidth: rung.bandwidth,
                playlist: rung.playlist.clone(),
                segment_sha256: rung.segment_sha256.clone(),
            })
            .collect(),
        board: protocol::Board {
            columns: board.columns,
            rows: board.rows,
            count: board.count,
            interval_seconds: board.interval_seconds,
        },
        blurhash: processed.blurhash.clone(),
        progressive: progressive.map(|p| p.metadata.clone()),
    };
    if serde_json::to_vec(&manifest)
        .map_err(|_| protocol::invalid())?
        .len()
        > protocol::MAX_MANIFEST
    {
        return Err(protocol::invalid());
    }
    Ok(Bundle {
        processed,
        hls,
        board,
        poster_path,
        manifest,
    })
}
async fn artifact(
    State(worker): State<Worker>,
    RoutePath((id, index)): RoutePath<(String, usize)>,
) -> Result<Response, ApiError> {
    let job = find(&worker, &id)?;
    let bundle = match &*job.phase.lock_or_recover() {
        Phase::Ready(bundle) => bundle.clone(),
        _ => return Err(ApiError::NotFound),
    };
    let path = bundle.path(index).ok_or(ApiError::NotFound)?;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(protocol::io_error)?;
    let length = bundle
        .manifest
        .artifacts
        .get(index)
        .ok_or(ApiError::NotFound)?
        .bytes;
    // Pins both scratch and reservation until this response actually finishes.
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024).map(move |chunk| {
        let _owner = (&bundle, &job);
        chunk
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".into()),
            (header::CONTENT_LENGTH, length.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn fixture() -> (tempfile::TempDir, Worker) {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::processor_tools(
            directory.path().into(),
            ffmpeg(),
            ffprobe(),
            NativeVideoConfig::default(),
        )
        .unwrap();
        let worker = Worker {
            config: Arc::new(config),
            token: "a".repeat(64).into(),
            resources: ProcessingPool::new(1, 220),
            jobs: Arc::default(),
            completed: Arc::default(),
        };
        (directory, worker)
    }
    fn spec() -> Spec {
        Spec {
            version: 1,
            stem: crate::media_processing::storage_stem(1),
            source_bytes: 10,
            output_bytes: 10,
            preset: "veryfast".into(),
            crf: Some(23),
            audio_kbps: 192,
            force_reencode: false,
            window_ms: None,
            poster_seconds: 0.0,
            avif_quality: 70,
            avif_speed: 6,
        }
    }
    async fn request(
        worker: &Worker,
        method: &str,
        uri: &str,
        body: Vec<u8>,
        authorized: bool,
    ) -> Response {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, body.len());
        if authorized {
            request = request.header(
                header::AUTHORIZATION,
                format!("Bearer {}", worker.token.expose()),
            );
        }
        router(worker.clone())
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }
    async fn admitted(worker: &Worker) -> String {
        let response = request(
            worker,
            "POST",
            "/v1/jobs",
            serde_json::to_vec(&spec()).unwrap(),
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Created>(&bytes).unwrap().id
    }
    #[tokio::test]
    async fn unauthenticated_invalid_and_full_admission_never_allocate_jobs() {
        let (_directory, worker) = fixture();
        let response = request(&worker, "POST", "/v1/jobs", vec![0; 32 * 1024], false).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let response = request(&worker, "POST", "/v1/jobs", vec![0; 32 * 1024], true).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let mut invalid = serde_json::to_value(spec()).unwrap();
        invalid["source_url"] = "file:///private".into();
        assert_eq!(
            request(
                &worker,
                "POST",
                "/v1/jobs",
                serde_json::to_vec(&invalid).unwrap(),
                true
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(worker.resources.usage(), (0, 0));
        let id = admitted(&worker).await;
        assert_eq!(worker.resources.usage(), (1, 220));
        assert_eq!(
            request(
                &worker,
                "POST",
                "/v1/jobs",
                serde_json::to_vec(&spec()).unwrap(),
                true
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            request(
                &worker,
                "GET",
                &format!("/v1/jobs/{id}/artifacts/0"),
                vec![],
                true
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        request(&worker, "DELETE", &format!("/v1/jobs/{id}"), vec![], true).await;
        assert_eq!(worker.resources.usage(), (0, 0));
    }
    #[tokio::test]
    async fn expired_jobs_cannot_renew_or_serve_and_release_only_owned_scratch() {
        let (_directory, worker) = fixture();
        let id = admitted(&worker).await;
        let job = find(&worker, &id).unwrap();
        let source = job.source.path().to_owned();
        std::fs::write(source.join("source.bin"), b"private").unwrap();
        *job.lease.lock_or_recover() = Instant::now().checked_sub(Duration::from_secs(31));
        drop(job);
        for method in ["GET", "POST"] {
            assert_eq!(
                request(&worker, method, &format!("/v1/jobs/{id}"), vec![], true)
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        reap(&worker);
        assert_eq!(worker.resources.usage(), (0, 0));
        assert!(!source.exists());
    }
    #[tokio::test]
    async fn short_upload_is_failed_private_and_not_replayed() {
        let (_directory, worker) = fixture();
        let id = admitted(&worker).await;
        let submission = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/v1/jobs/{id}/source"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", worker.token.expose()),
            )
            .header(header::CONTENT_LENGTH, 10)
            .body(Body::from(vec![1_u8; 5]))
            .unwrap();
        assert_eq!(
            router(worker.clone())
                .oneshot(submission)
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let job = find(&worker, &id).unwrap();
        assert!(matches!(*job.phase.lock_or_recover(), Phase::Failed));
        assert_eq!(worker.resources.usage(), (1, 220));
        drop(job);
        request(&worker, "DELETE", &format!("/v1/jobs/{id}"), vec![], true).await;
        assert_eq!(worker.resources.usage(), (0, 0));
    }
    #[test]
    fn generated_config_is_private_unique_and_needs_no_database() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a.toml");
        let b = root.path().join("b.toml");
        generate(&a, false).unwrap();
        generate(&b, false).unwrap();
        let first = std::fs::read_to_string(&a).unwrap();
        let one: FileSettings = toml::from_str(&first).unwrap();
        let two: FileSettings = toml::from_str(&std::fs::read_to_string(b).unwrap()).unwrap();
        assert!(crate::video_offload::valid_token(&one.token));
        assert_ne!(one.token, two.token);
        assert!(!first.contains("database_url"));
        assert!(generate(&a, false).is_err());
        assert_eq!(first, std::fs::read_to_string(&a).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(a).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn dedicated_worker_directory_is_locked_and_refuses_foreign_files() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("worker");
        let first = own_directory(&root).unwrap();
        assert!(own_directory(&root).is_err());
        drop(first);
        assert!(own_directory(&root).is_ok());
        let foreign = parent.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("operator.txt"), b"keep").unwrap();
        assert!(own_directory(&foreign).is_err());
        assert!(!foreign.join(".plamenu-video-worker").exists());
        assert_eq!(
            std::fs::read(foreign.join("operator.txt")).unwrap(),
            b"keep"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&foreign, root.join("tmp")).unwrap();
            assert!(own_directory(&root).is_err());
        }
    }
}
