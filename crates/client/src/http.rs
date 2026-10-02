//! HTTP-backed [`Fetcher`] (feature `http`).

use crate::fetch::{check_url, Fetcher, HttpStatus};
use anyhow::{Context, Result};
use std::io::Read;
use std::time::Duration;

/// Fetches over HTTPS with ureq.
///
/// Timeouts limit *silence*, not total duration: connecting may take 15 s and
/// each read may wait 30 s for data, but a large file on a slow line may take
/// as long as it needs while bytes keep flowing.
pub struct HttpFetcher {
    agent: ureq::Agent,
}

impl HttpFetcher {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(30))
                .timeout_write(Duration::from_secs(30))
                .user_agent(concat!("bonegrader/", env!("CARGO_PKG_VERSION")))
                .build(),
        }
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher for HttpFetcher {
    fn open(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        check_url(url)?;
        let resp = match self.agent.get(url).call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) => {
                return Err(HttpStatus {
                    code,
                    url: url.to_string(),
                }
                .into())
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("GET {url}"))),
        };
        // Redirects must not downgrade to plain HTTP either.
        check_url(resp.get_url()).context("Weiterleitung auf eine unsichere Adresse")?;
        Ok(Box::new(resp.into_reader()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    /// A body that trickles in for longer than any single timeout must still
    /// arrive: only silence may time out, not a slow transfer.
    #[test]
    fn slow_but_steady_download_is_not_cut_off() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut s, &mut buf);
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            for _ in 0..8 {
                s.write_all(b"x").unwrap();
                s.flush().unwrap();
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        let fetcher = HttpFetcher {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout_read(Duration::from_secs(1))
                .build(),
        };
        let t = Instant::now();
        let body = fetcher
            .get_limited(&format!("http://{addr}/f"), 1024)
            .unwrap();
        assert_eq!(body, b"xxxxxxxx");
        assert!(
            t.elapsed() > Duration::from_secs(1),
            "transfer outlasted the read timeout"
        );
        server.join().unwrap();
    }

    #[test]
    fn status_errors_are_typed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut s, &mut buf);
            write!(
                s,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let err = HttpFetcher::new()
            .open(&format!("http://{addr}/missing"))
            .err()
            .unwrap();
        assert!(crate::fetch::is_not_found(&err), "{err:#}");
        server.join().unwrap();
    }

    #[test]
    fn refuses_plain_http_to_remote_hosts() {
        let err = HttpFetcher::new()
            .open("http://example.com/main/manifest.json")
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("https"), "{err:#}");
    }
}
