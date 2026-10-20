use std::time::Duration;

use anyhow::Result;
use reqwest::{Client, StatusCode};
use tokio::time::timeout;

#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoveryResult {
    pub navidrome_ok: bool,
    pub invidious_ok: bool,
}

#[derive(Debug, Clone)]
pub struct Discoverer {
    client: Client,
    http_timeout: Duration,
}

impl Default for Discoverer {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .user_agent("syncpulse/0.1.0")
                .timeout(Duration::from_secs(5))
                .build()
                .expect("Failed to build reqwest client"),
            http_timeout: Duration::from_secs(5),
        }
    }
}

impl Discoverer {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn check_all(
        &self,
        navidrome_port: u16,
        invidious_port: u16,
        host: &str,
    ) -> Result<DiscoveryResult> {
        let (navidrome, invidious) = tokio::join!(
            self.check_navidrome_at(host, navidrome_port),
            self.check_invidious_at(host, invidious_port)
        );
        Ok(DiscoveryResult {
            navidrome_ok: navidrome,
            invidious_ok: invidious,
        })
    }

    pub async fn check_navidrome(&self, port: u16) -> bool {
        self.check_navidrome_at("127.0.0.1", port).await
    }

    pub async fn check_navidrome_at(&self, host: &str, port: u16) -> bool {
        self.get_ok(&format!("http://{host}:{port}/ping")).await
    }

    pub async fn check_invidious(&self, port: u16) -> bool {
        self.check_invidious_at("127.0.0.1", port).await
    }

    pub async fn check_invidious_at(&self, host: &str, port: u16) -> bool {
        let url = format!("http://{host}:{port}/api/v1/stats");
        let response = timeout(self.http_timeout, self.client.get(url).send()).await;
        let Ok(Ok(response)) = response else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        matches!(
            response.json::<serde_json::Value>().await,
            Ok(stats) if is_invidious_stats(&stats)
        )
    }

    async fn get_ok(&self, url: &str) -> bool {
        matches!(
            timeout(self.http_timeout, self.client.get(url).send()).await,
            Ok(Ok(response)) if is_healthy_status(response.status())
        )
    }
}

fn is_invidious_stats(value: &serde_json::Value) -> bool {
    value
        .get("software")
        .and_then(|software| software.get("name"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("invidious"))
}

fn is_healthy_status(status: StatusCode) -> bool {
    status.is_success() || status.is_redirection()
}

#[cfg(test)]
mod tests {
    use super::{is_healthy_status, is_invidious_stats, Discoverer};
    use reqwest::StatusCode;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            stream.write_all(response.as_bytes()).unwrap();
        });
        port
    }

    #[test]
    fn discovery_requires_a_success_or_redirect_response() {
        assert!(is_healthy_status(StatusCode::OK));
        assert!(is_healthy_status(StatusCode::FOUND));
        assert!(!is_healthy_status(StatusCode::NOT_FOUND));
        assert!(!is_healthy_status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn invidious_health_requires_its_documented_stats_identity() {
        assert!(is_invidious_stats(&json!({
            "software": { "name": "Invidious", "version": "1.0" }
        })));
        assert!(!is_invidious_stats(
            &json!({ "message": "other local web app" })
        ));
        assert!(!is_invidious_stats(&json!({
            "software": { "name": "something else" }
        })));
    }

    #[tokio::test]
    async fn service_checks_verify_each_product_specific_endpoint() {
        let discoverer = Discoverer::new();
        let navidrome_port =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n.");
        assert!(discoverer.check_navidrome(navidrome_port).await);

        let invidious_port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33\r\nConnection: close\r\n\r\n{\"software\":{\"name\":\"Invidious\"}}",
        );
        assert!(discoverer.check_invidious(invidious_port).await);

        let unrelated_port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}",
        );
        assert!(!discoverer.check_invidious(unrelated_port).await);
    }
}
