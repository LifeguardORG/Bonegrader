//! Hashing. SHA-1 is the content identity throughout Bonegrader (blob names,
//! state, diff); it is also what CurseForge records in `minecraftinstance.json`,
//! so the two interoperate. SHA-256 is published alongside it and verified on
//! download, so integrity does not rest on SHA-1 alone.

use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Both digests of one stream, computed in a single pass.
#[derive(Default, Clone)]
pub struct Hasher {
    sha1: Sha1,
    sha256: Sha256,
    len: u64,
}

/// Lowercase hex digests plus the byte count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digests {
    pub sha1: String,
    pub sha256: String,
    pub len: u64,
}

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.sha1.update(bytes);
        self.sha256.update(bytes);
        self.len += bytes.len() as u64;
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finish(self) -> Digests {
        Digests {
            sha1: to_hex(&self.sha1.finalize()),
            sha256: to_hex(&self.sha256.finalize()),
            len: self.len,
        }
    }
}

/// Stream a file through SHA-1 and return the lowercase hex digest.
pub fn sha1_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// Stream a file through SHA-1 and SHA-256 at once.
pub fn digest_file(path: &Path) -> io::Result<Digests> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

/// SHA-1 of an in-memory buffer.
pub fn sha1_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// SHA-256 of an in-memory buffer.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode hex (either case). `None` on odd length or non-hex characters.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Well-known SHA-1 / SHA-256 test vectors.
        assert_eq!(sha1_bytes(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha1_bytes(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hasher_matches_one_shot_digests() {
        let mut h = Hasher::new();
        h.update(b"hello ");
        h.update(b"world");
        let d = h.finish();
        assert_eq!(d.sha1, sha1_bytes(b"hello world"));
        assert_eq!(d.sha256, sha256_bytes(b"hello world"));
        assert_eq!(d.len, 11);
    }

    #[test]
    fn file_matches_bytes() {
        let dir = std::env::temp_dir().join(format!("bonegrader-hash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::write(&p, b"hello world").unwrap();
        assert_eq!(sha1_file(&p).unwrap(), sha1_bytes(b"hello world"));
        let d = digest_file(&p).unwrap();
        assert_eq!(d.sha256, sha256_bytes(b"hello world"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(
            from_hex(&to_hex(&[0, 1, 0xab, 0xff])).unwrap(),
            vec![0, 1, 0xab, 0xff]
        );
        assert_eq!(from_hex("ABcd").unwrap(), vec![0xab, 0xcd]);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
