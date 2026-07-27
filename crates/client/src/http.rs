//! HTTP-backed [`Fetcher`] (feature `http`).

use crate::apply::{resolve_url, Fetcher};
use anyhow::{Context, Result};
use bonegrader_core::manifest::Manifest;
use std::io::Read;

/// Fetches blobs over HTTP(S) with ureq.
pub struct HttpFetcher {
    agent: ureq::Agent,
}

impl HttpFetcher {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(60))
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
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let mut buf = Vec::new();
        resp.into_reader()
            .read_to_end(&mut buf)
            .with_context(|| format!("reading body of {url}"))?;
        Ok(buf)
    }
}

/// Fetch and parse `<base_url>/manifest.json`.
pub fn fetch_manifest(base_url: &str) -> Result<Manifest> {
    let fetcher = HttpFetcher::new();
    let url = resolve_url(base_url, "manifest.json");
    let bytes = fetcher.get(&url)?;
    let text = String::from_utf8(bytes).context("manifest is not valid UTF-8")?;
    Manifest::from_json(&text).context("parsing manifest.json")
}
