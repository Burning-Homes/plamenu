//! Unsigned Webmention transport. Bodies must be complete before verification.
use super::{FederationClient, FederationError, PAGE_BODY_LIMIT, PAGE_MAX_REDIRECTS};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};

#[derive(Debug, Clone)]
pub struct MentionPage {
    pub final_url: String,
    pub content_type: String,
    pub links: Vec<String>,
    pub body: String,
}

impl FederationClient {
    pub async fn fetch_mention_page(&self, raw_url: &str) -> Result<MentionPage, FederationError> {
        self.fetch_mention_page_with_guard(raw_url, |_| async { Ok(true) })
            .await
    }
    pub async fn fetch_mention_page_with_guard<F, Fut>(
        &self,
        raw_url: &str,
        mut allow: F,
    ) -> Result<MentionPage, FederationError>
    where
        F: FnMut(&str) -> Fut,
        Fut: std::future::Future<Output = Result<bool, FederationError>>,
    {
        let mut url = self.checked_feed(raw_url)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            if !allow(url.as_str()).await? {
                return Err(FederationError::Admission(
                    "Website domain policy refused request".into(),
                ));
            }
            let mut response = self
                .http_no_redirect_for(&url)
                .get(url.clone())
                .header(ACCEPT, "text/html, application/xhtml+xml")
                .send()
                .await?;
            if response.status().is_redirection() {
                let next = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                url = self.checked_feed(
                    url.join(next)
                        .map_err(|_| FederationError::InvalidUrl(next.into()))?
                        .as_str(),
                )?;
                continue;
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let links = response
                .headers()
                .get_all(reqwest::header::LINK)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_owned))
                .collect();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if chunk.len() > PAGE_BODY_LIMIT.saturating_sub(body.len()) {
                    return Err(FederationError::TooLarge(url.to_string()));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(MentionPage {
                final_url: url.into(),
                content_type,
                links,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Err(FederationError::InvalidUrl(format!(
            "{raw_url}: too many redirects"
        )))
    }

    /// Every POST destination is checked; redirects retain only public form fields.
    pub async fn send_webmention(
        &self,
        endpoint: &str,
        source: &str,
        target: &str,
    ) -> Result<(), FederationError> {
        self.send_webmention_with_guard(endpoint, source, target, |_| async { Ok(true) })
            .await
    }
    pub async fn send_webmention_with_guard<F, Fut>(
        &self,
        endpoint: &str,
        source: &str,
        target: &str,
        mut allow: F,
    ) -> Result<(), FederationError>
    where
        F: FnMut(&str) -> Fut,
        Fut: std::future::Future<Output = Result<bool, FederationError>>,
    {
        let mut url = self.checked_feed(endpoint)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            if !allow(url.as_str()).await? {
                return Err(FederationError::Admission(
                    "Website domain policy refused request".into(),
                ));
            }
            let response = self
                .http_no_redirect_for(&url)
                .post(url.clone())
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(encode_form(source, target))
                .send()
                .await?;
            if response.status().is_redirection() {
                let next = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                url = self.checked_feed(
                    url.join(next)
                        .map_err(|_| FederationError::InvalidUrl(next.into()))?
                        .as_str(),
                )?;
                continue;
            }
            return if response.status().is_success() {
                Ok(())
            } else {
                Err(FederationError::Status(response.status().as_u16()))
            };
        }
        Err(FederationError::InvalidUrl(format!(
            "{endpoint}: too many redirects"
        )))
    }
}

fn encode_form(source: &str, target: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("source", source)
        .append_pair("target", target)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn unsigned_mentions_refuse_private_addresses_before_connecting() {
        let client = FederationClient::new("test", false).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mention", listener.local_addr().unwrap());
        assert!(matches!(
            client.fetch_mention_page(&url).await,
            Err(FederationError::PrivateAddress(_))
        ));
        assert!(matches!(
            client
                .send_webmention(&url, "https://a.example/post", "https://b.example/track")
                .await,
            Err(FederationError::PrivateAddress(_))
        ));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[tokio::test]
    async fn website_policy_is_checked_before_each_redirected_get_and_post() {
        for post in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/start", listener.local_addr().unwrap());
            let blocked = format!("http://{}/blocked", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0; 8192];
                let _ = socket.read(&mut buf).await.unwrap();
                socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {blocked}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                drop(socket);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                        .await
                        .is_err()
                );
            });
            let client = FederationClient::new("test", true).unwrap();
            let allow = |next: &str| {
                let allowed = !next.ends_with("/blocked");
                async move { Ok(allowed) }
            };
            if post {
                assert!(matches!(
                    client
                        .send_webmention_with_guard(
                            &url,
                            "https://a.example/post",
                            "https://b.example/track",
                            allow
                        )
                        .await,
                    Err(FederationError::Admission(_))
                ));
            } else {
                assert!(matches!(
                    client.fetch_mention_page_with_guard(&url, allow).await,
                    Err(FederationError::Admission(_))
                ));
            }
            server.await.unwrap();
        }
    }
    async fn origin(body: Vec<u8>) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/page", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let read = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..read]);
                if read == 0 || request.windows(4).any(|s| s == b"\r\n\r\n") {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nLink: </mention>; rel=\"webmention\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
            request
        });
        (url, task)
    }
    #[tokio::test]
    async fn verification_fetch_preserves_headers_and_never_accepts_a_truncated_body() {
        let client = FederationClient::new("test", true).unwrap();
        let (url, task) = origin(b"<p>Complete</p>".to_vec()).await;
        let page = client.fetch_mention_page(&url).await.unwrap();
        assert_eq!(page.body, "<p>Complete</p>");
        assert_eq!(page.links, vec!["</mention>; rel=\"webmention\""]);
        task.await.unwrap();
        let (url, task) = origin(vec![b'x'; PAGE_BODY_LIMIT + 1]).await;
        assert!(matches!(
            client.fetch_mention_page(&url).await,
            Err(FederationError::TooLarge(_))
        ));
        task.await.unwrap();
        let encoded = encode_form("https://a.example/post?a=b&c=d", "https://b.example/song");
        let pairs: Vec<_> = url::form_urlencoded::parse(encoded.as_bytes()).collect();
        assert_eq!(pairs[0].1, "https://a.example/post?a=b&c=d");
    }
}
