//! Detached Ed25519 signatures for `manifest.json`.
//!
//! The publisher signs the exact manifest bytes with a secret key that never
//! leaves the admin's machine and uploads the signature as
//! [`SIGNATURE_FILE`] next to the manifest. Clients carry the matching public
//! key(s) and refuse manifests that do not verify — so taking over the web
//! server is no longer enough to push code to the players.
//!
//! Keys and signatures are plain hex text: a 32-byte secret seed, a 32-byte
//! public key, a 64-byte signature.

use crate::hash::{from_hex, to_hex};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Name of the detached signature next to `manifest.json`.
pub const SIGNATURE_FILE: &str = "manifest.json.sig";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignError {
    #[error("ungültiger Schlüssel: {0}")]
    BadKey(String),
    #[error("Signaturdatei ist beschädigt")]
    BadSignatureEncoding,
    #[error("Signatur passt zu keinem vertrauenswürdigen Schlüssel")]
    NoMatch,
    #[error("Zufallsgenerator nicht verfügbar: {0}")]
    Rng(String),
}

/// A signing key (keep secret).
pub struct SecretKey(SigningKey);

/// A verifying key (safe to publish and to compile into the client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl SecretKey {
    /// Generate a fresh key from the OS random number generator.
    pub fn generate() -> Result<Self, SignError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| SignError::Rng(e.to_string()))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// Parse a secret key file: the hex seed, optionally preceded by `#`
    /// comment lines.
    pub fn from_text(text: &str) -> Result<Self, SignError> {
        let line = key_lines(text)
            .next()
            .ok_or_else(|| SignError::BadKey("leere Schlüsseldatei".into()))?;
        let seed: [u8; 32] = decode_fixed(line)
            .ok_or_else(|| SignError::BadKey("erwartet 64 Hex-Zeichen".into()))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// The key file content written by `keygen`.
    pub fn to_text(&self) -> String {
        format!(
            "# Bonegrader manifest signing key — GEHEIM halten, niemals committen.\n{}\n",
            to_hex(&self.0.to_bytes())
        )
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Sign `msg`; returns the signature file content (hex + newline).
    pub fn sign(&self, msg: &[u8]) -> String {
        format!("{}\n", to_hex(&self.0.sign(msg).to_bytes()))
    }
}

impl PublicKey {
    pub fn from_hex(s: &str) -> Result<Self, SignError> {
        let bytes: [u8; 32] = decode_fixed(s)
            .ok_or_else(|| SignError::BadKey(format!("erwartet 64 Hex-Zeichen: {s}")))?;
        VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|e| SignError::BadKey(e.to_string()))
    }

    pub fn to_hex(&self) -> String {
        to_hex(self.0.as_bytes())
    }

    /// Strict verification (rejects malleable/non-canonical signatures).
    pub fn verify(&self, msg: &[u8], sig_text: &str) -> Result<bool, SignError> {
        let bytes: [u8; 64] =
            decode_fixed(sig_text.trim()).ok_or(SignError::BadSignatureEncoding)?;
        let sig = Signature::from_bytes(&bytes);
        Ok(self.0.verify_strict(msg, &sig).is_ok())
    }
}

/// Parse a trusted-keys file: one hex public key per line, `#` comments and
/// blank lines ignored. An empty result means "no keys configured".
pub fn parse_public_keys(text: &str) -> Result<Vec<PublicKey>, SignError> {
    key_lines(text).map(PublicKey::from_hex).collect()
}

/// Verify `sig_text` over `msg` against any of `keys`; returns the index of
/// the key that matched.
pub fn verify_any(keys: &[PublicKey], msg: &[u8], sig_text: &str) -> Result<usize, SignError> {
    for (i, k) in keys.iter().enumerate() {
        if k.verify(msg, sig_text)? {
            return Ok(i);
        }
    }
    Err(SignError::NoMatch)
}

fn key_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

fn decode_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    from_hex(s)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let sk = SecretKey::generate().unwrap();
        let pk = sk.public();
        let sig = sk.sign(b"manifest bytes");
        assert!(pk.verify(b"manifest bytes", &sig).unwrap());
        assert!(
            !pk.verify(b"manifest bytes!", &sig).unwrap(),
            "tampered message"
        );
    }

    #[test]
    fn key_files_roundtrip() {
        let sk = SecretKey::generate().unwrap();
        let again = SecretKey::from_text(&sk.to_text()).unwrap();
        assert_eq!(sk.public(), again.public());

        let keys_file = format!("# trusted keys\n\n{}\n", sk.public().to_hex());
        let keys = parse_public_keys(&keys_file).unwrap();
        assert_eq!(keys, vec![sk.public()]);
        assert!(parse_public_keys("# nothing here\n").unwrap().is_empty());
        assert!(parse_public_keys("nothex").is_err());
    }

    #[test]
    fn verify_any_picks_the_matching_key() {
        let old = SecretKey::generate().unwrap();
        let new = SecretKey::generate().unwrap();
        let keys = vec![old.public(), new.public()];
        assert_eq!(verify_any(&keys, b"m", &new.sign(b"m")), Ok(1));
        let stranger = SecretKey::generate().unwrap();
        assert_eq!(
            verify_any(&keys, b"m", &stranger.sign(b"m")),
            Err(SignError::NoMatch)
        );
        assert_eq!(
            verify_any(&keys, b"m", "garbage"),
            Err(SignError::BadSignatureEncoding)
        );
    }

    #[test]
    fn deterministic_known_answer() {
        // RFC 8032 test 1: empty message, secret seed 9d61b1...
        let sk = SecretKey::from_text(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        )
        .unwrap();
        assert_eq!(
            sk.public().to_hex(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(
            sk.sign(b"").trim(),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }
}
