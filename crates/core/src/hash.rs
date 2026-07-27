//! SHA-1 hashing. SHA-1 is the discrepancy check throughout Bonegrader; it is
//! also what CurseForge already records in `minecraftinstance.json`, so the two
//! interoperate. SHA-1 is used purely for content-identity here, not security.

use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Stream a file through SHA-1 and return the lowercase hex digest.
pub fn sha1_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// SHA-1 of an in-memory buffer.
pub fn sha1_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Well-known SHA-1 test vectors.
        assert_eq!(sha1_bytes(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_bytes(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn file_matches_bytes() {
        let dir = std::env::temp_dir().join("bonegrader-hash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::write(&p, b"hello world").unwrap();
        assert_eq!(sha1_file(&p).unwrap(), sha1_bytes(b"hello world"));
        let _ = std::fs::remove_file(&p);
    }
}
