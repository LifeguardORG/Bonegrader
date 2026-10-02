//! Fetching bytes: the [`Fetcher`] abstraction, URL resolution and the
//! transport-security rule every URL must pass.

use anyhow::{bail, Context, Result};
use std::io::Read;

/// Source of file bytes. Real clients back this with HTTP; tests use the
/// filesystem. Implementors need not verify anything — callers check sizes and
/// hashes. Shared across download threads, hence `Send + Sync`.
pub trait Fetcher: Send + Sync {
    /// Open `url` for streaming.
    fn open(&self, url: &str) -> Result<Box<dyn Read + Send>>;

    /// Read a whole (small) body, refusing anything larger than `limit` bytes.
    fn get_limited(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.open(url)?
            .take(limit + 1)
            .read_to_end(&mut buf)
            .with_context(|| format!("{url} lesen"))?;
        if buf.len() as u64 > limit {
            bail!("{url}: Antwort ist größer als {limit} Bytes");
        }
        Ok(buf)
    }
}

/// An HTTP error status, so callers can tell "missing" from "broken".
#[derive(Debug, thiserror::Error)]
#[error("{url}: HTTP {code}")]
pub struct HttpStatus {
    pub code: u16,
    pub url: String,
}

/// True if `err` (or anything in its cause chain) is an HTTP 404/410.
pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<HttpStatus>()
            .is_some_and(|s| matches!(s.code, 404 | 410))
    })
}

/// True if retrying cannot help (the server answered "no" definitively).
pub fn is_permanent(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<HttpStatus>()
            .is_some_and(|s| matches!(s.code, 400..=499) && !matches!(s.code, 408 | 429))
            || e.downcast_ref::<InsecureUrl>().is_some()
    })
}

/// Join a possibly-relative manifest url onto the base url.
pub fn resolve_url(base_url: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") || base_url.is_empty() {
        url.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    }
}

/// Rejected because it is not HTTPS (plain HTTP is only allowed to loopback).
#[derive(Debug, thiserror::Error)]
#[error("unsichere Adresse – nur https:// ist erlaubt: {0}")]
pub struct InsecureUrl(pub String);

/// Everything Bonegrader downloads is executable code for the game, so it is
/// only fetched over HTTPS. Plain HTTP is allowed for loopback addresses
/// (local testing) only.
pub fn check_url(url: &str) -> Result<(), InsecureUrl> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        let host = match host_port.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => host_port.split(':').next().unwrap_or(""),
        };
        if matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return Ok(());
        }
    }
    Err(InsecureUrl(url.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_and_keeps_absolute_urls() {
        assert_eq!(
            resolve_url("https://x.de/main/", "/files/by-hash/ab"),
            "https://x.de/main/files/by-hash/ab"
        );
        assert_eq!(
            resolve_url("https://x.de/main", "https://cdn.de/f"),
            "https://cdn.de/f"
        );
        assert_eq!(resolve_url("", "files/x"), "files/x");
    }

    #[test]
    fn only_https_or_loopback_http() {
        assert!(check_url("https://bonegrader.example.com/main/manifest.json").is_ok());
        assert!(check_url("HTTPS://Example.com").is_ok());
        assert!(check_url("http://localhost:8080/main").is_ok());
        assert!(check_url("http://127.0.0.1/x").is_ok());
        assert!(check_url("http://[::1]:9000/x").is_ok());

        assert!(check_url("http://example.com/main").is_err());
        assert!(check_url("http://localhost.evil.com/x").is_err());
        assert!(check_url("http://127.0.0.1.evil.com/x").is_err());
        assert!(check_url("http://evil.com@example.com/").is_err());
        assert!(check_url("ftp://example.com/").is_err());
        assert!(check_url("bonegrader.example.com/main").is_err());
    }

    #[test]
    fn classifies_errors() {
        let nf = anyhow::Error::new(HttpStatus {
            code: 404,
            url: "u".into(),
        })
        .context("outer");
        assert!(is_not_found(&nf));
        assert!(is_permanent(&nf));
        let busy = anyhow::Error::new(HttpStatus {
            code: 503,
            url: "u".into(),
        });
        assert!(!is_not_found(&busy) && !is_permanent(&busy));
        let rate = anyhow::Error::new(HttpStatus {
            code: 429,
            url: "u".into(),
        });
        assert!(!is_permanent(&rate));
        assert!(is_permanent(&anyhow::Error::new(InsecureUrl(
            "http://x".into()
        ))));
    }
}
