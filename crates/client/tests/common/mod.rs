//! Shared helpers for the client integration tests: a filesystem-backed
//! "server" with failure injection, and manifest builders.
#![allow(dead_code)]

use anyhow::Result;
use bonegrader_client::fetch::{Fetcher, HttpStatus};
use bonegrader_core::hash::{sha1_bytes, sha256_bytes};
use bonegrader_core::manifest::{Category, FileEntry, Loader, Manifest, SeedEntry};
use bonegrader_core::scan::LocalFile;
use bonegrader_core::sign::SecretKey;
use bonegrader_core::state::ManagedEntry;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const BASE: &str = "https://test.invalid/main";

/// Serves `<root>/<rel>` for `BASE/<rel>`; counts requests and can fail
/// selected URLs a number of times (HTTP 503) or forever (404 when missing).
pub struct MockServer {
    pub root: tempfile::TempDir,
    calls: Mutex<HashMap<String, usize>>,
    failures: Mutex<HashMap<String, usize>>,
}

impl MockServer {
    pub fn new() -> Self {
        Self {
            root: tempfile::Builder::new()
                .prefix("bg-server-")
                .tempdir()
                .unwrap(),
            calls: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn put_blob(&self, content: &[u8]) {
        write(
            &self
                .root
                .path()
                .join(format!("files/by-hash/{}", sha1_bytes(content))),
            content,
        );
    }

    /// Publish `manifest` (optionally signed); returns the exact bytes.
    pub fn publish(&self, manifest: &Manifest, key: Option<&SecretKey>) -> Vec<u8> {
        let bytes = manifest.to_json_pretty().unwrap().into_bytes();
        write(&self.root.path().join("manifest.json"), &bytes);
        if let Some(k) = key {
            write(
                &self.root.path().join("manifest.json.sig"),
                k.sign(&bytes).as_bytes(),
            );
        }
        bytes
    }

    pub fn fail(&self, url: &str, times: usize) {
        self.failures.lock().unwrap().insert(url.to_string(), times);
    }

    pub fn calls(&self, url: &str) -> usize {
        self.calls.lock().unwrap().get(url).copied().unwrap_or(0)
    }

    pub fn blob_url(content: &[u8]) -> String {
        format!("{BASE}/files/by-hash/{}", sha1_bytes(content))
    }
}

impl Fetcher for MockServer {
    fn open(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        *self
            .calls
            .lock()
            .unwrap()
            .entry(url.to_string())
            .or_default() += 1;
        if let Some(n) = self.failures.lock().unwrap().get_mut(url) {
            if *n > 0 {
                *n -= 1;
                return Err(HttpStatus {
                    code: 503,
                    url: url.into(),
                }
                .into());
            }
        }
        let rel = url.strip_prefix(&format!("{BASE}/")).unwrap_or(url);
        match std::fs::read(self.root.path().join(rel)) {
            Ok(b) => Ok(Box::new(Cursor::new(b))),
            Err(_) => Err(HttpStatus {
                code: 404,
                url: url.into(),
            }
            .into()),
        }
    }
}

pub fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub fn instance() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("bg-instance-")
        .tempdir()
        .unwrap()
}

pub fn entry(cat: Category, path: &str, content: &[u8], mod_id: Option<&str>) -> FileEntry {
    let sha1 = sha1_bytes(content);
    FileEntry {
        category: cat,
        path: path.into(),
        file_name: path.rsplit('/').next().unwrap().into(),
        size: content.len() as u64,
        sha1: sha1.clone(),
        sha256: Some(sha256_bytes(content)),
        mod_id: mod_id.map(str::to_string),
        mod_version: None,
        url: format!("files/by-hash/{sha1}"),
    }
}

pub fn mod_entry(path: &str, content: &[u8], mod_id: &str) -> FileEntry {
    entry(Category::Mod, path, content, Some(mod_id))
}

pub fn seed(path: &str, content: &[u8]) -> SeedEntry {
    let sha1 = sha1_bytes(content);
    SeedEntry {
        path: path.into(),
        size: content.len() as u64,
        sha1: sha1.clone(),
        sha256: Some(sha256_bytes(content)),
        url: format!("files/by-hash/{sha1}"),
    }
}

pub fn manifest(files: Vec<FileEntry>) -> Manifest {
    Manifest {
        schema_version: 1,
        pack_name: "BonesAndBees".into(),
        channel: "main".into(),
        generated_at: "2026-07-26T20:00:00Z".into(),
        loader: Loader {
            loader_type: "neoforge".into(),
            mc_version: "1.21.1".into(),
            loader_version: "21.1.234".into(),
        },
        files,
        client: None,
        server: None,
        seeds: vec![],
    }
}

pub fn local(path: &str, content: &[u8], mod_ids: &[&str]) -> LocalFile {
    LocalFile {
        category: Category::Mod,
        path: path.into(),
        file_name: path.rsplit('/').next().unwrap().into(),
        size: content.len() as u64,
        sha1: sha1_bytes(content),
        mod_ids: mod_ids.iter().map(|s| s.to_string()).collect(),
        mod_version: None,
    }
}

pub fn managed(content: &[u8], mod_ids: &[&str]) -> ManagedEntry {
    ManagedEntry {
        sha1: sha1_bytes(content),
        mod_ids: mod_ids.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn read(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Sorted relative paths of all files below `dir` (helper for assertions).
pub fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}
