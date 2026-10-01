//! Bounded native HTTP/TLS listener; no proxy or companion process required.
use super::{AnyError, Tls};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};

fn acceptor(config: &Tls) -> Result<TlsAcceptor, AnyError> {
    let certs = CertificateDer::pem_file_iter(&config.cert)?.collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err("Private worker TLS certificate chain is empty".into());
    }
    let key = PrivateKeyDer::from_pem_file(&config.key)?;
    let mut tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(tls)))
}

pub(super) async fn serve(
    app: axum::Router,
    bind: std::net::SocketAddr,
    tls: Option<Tls>,
) -> Result<(), AnyError> {
    let mut tls_acceptor = tls.as_ref().map(acceptor).transpose()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(address = %listener.local_addr()?, tls = tls_acceptor.is_some(), "private native video worker listening; no database services");
    let connections = Arc::new(tokio::sync::Semaphore::new(32));
    let mut tasks = tokio::task::JoinSet::new();
    let shutdown = signal();
    tokio::pin!(shutdown);
    let mut reload = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            _ = reload.tick(), if tls.is_some() => {
                if let Some(config) = &tls {
                    if let Ok(next) = acceptor(config) { tls_acceptor = Some(next); }
                    else { tracing::warn!("private worker TLS reload failed; retaining last valid certificate"); }
                }
            }
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let Ok(permit) = connections.clone().try_acquire_owned() else { continue; };
                let app = app.clone();
                let tls = tls_acceptor.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Some(tls) = tls {
                        if let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(5), tls.accept(socket)).await { connection(app, stream).await; }
                    } else { connection(app, socket).await; }
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn connection<I>(app: axum::Router, stream: I)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    connection_with_idle(app, stream, Duration::from_secs(15)).await;
}

/// A stalled authenticated output reader must not pin a completed job's
/// admission for the entire absolute lifetime. Count actual transport progress,
/// not repeated body polling, so backpressure is subject to the idle deadline.
struct Traffic<I> {
    inner: I,
    began: Instant,
    progress: Arc<AtomicU64>,
}
impl<I: Unpin> Traffic<I> {
    fn advanced(&self) {
        self.progress.store(
            u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}
impl<I: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Traffic<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if buffer.filled().len() > before {
            self.advanced();
        }
        result
    }
}
impl<I: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Traffic<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(context, bytes);
        if matches!(result, Poll::Ready(Ok(written)) if written > 0) {
            self.advanced();
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

async fn connection_with_idle<I>(app: axum::Router, stream: I, idle: Duration)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let began = Instant::now();
    let progress = Arc::new(AtomicU64::new(0));
    let stream = Traffic {
        inner: stream,
        began,
        progress: progress.clone(),
    };
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_buf_size(64 * 1024)
        .keep_alive(false);
    let served = tokio::time::timeout(
        Duration::from_hours(4),
        http.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app)),
    );
    let stalled = async {
        loop {
            tokio::time::sleep(idle).await;
            let previous = Duration::from_millis(progress.load(Ordering::Relaxed));
            if began.elapsed().saturating_sub(previous) >= idle {
                return;
            }
        }
    };
    tokio::select! { _ = served => {}, () = stalled => {} }
}

async fn signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn undrained_output_and_incomplete_header_release_the_connection() {
        let app = axum::Router::new().route(
            "/large",
            axum::routing::get(|| async { vec![1_u8; 256 * 1024] }),
        );
        let (mut client, server) = tokio::io::duplex(64);
        let service = tokio::spawn(connection_with_idle(
            app.clone(),
            server,
            Duration::from_millis(100),
        ));
        client
            .write_all(b"GET /large HTTP/1.1\r\nHost: processor\r\n\r\n")
            .await
            .unwrap();
        let mut header = [0_u8; 16];
        client.read_exact(&mut header).await.unwrap();
        assert_eq!(&header[..12], b"HTTP/1.1 200");
        // Keep the reader alive without draining its tiny transport buffer.
        tokio::time::timeout(Duration::from_secs(2), service)
            .await
            .unwrap()
            .unwrap();
        let (mut client, server) = tokio::io::duplex(64);
        let service = tokio::spawn(connection_with_idle(
            app,
            server,
            Duration::from_millis(100),
        ));
        client.write_all(b"GET /large HTTP/1.1\r\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), service)
            .await
            .unwrap()
            .unwrap();
    }
}
