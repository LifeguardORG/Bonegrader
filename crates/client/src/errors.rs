//! Turn an error chain into something a player can act on: a [`Kind`] that
//! front-ends map to a plain-language title and hint, and a detail text
//! without the repetitions nested errors tend to produce.

use crate::apply::{BadDownload, Cancelled, FileInUse};
use crate::fetch::{HttpStatus, InsecureUrl, Unreachable};
use crate::session::{ClientTooOld, InstanceMissing, NotAManifest, StaleManifest, TrustError};
use bonegrader_core::manifest::ManifestError;
use serde::Serialize;
use std::error::Error as StdError;
use std::io;

/// The situations a front-end explains differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// No connection to the update server (offline, DNS, refused, timeout).
    Offline,
    /// The server answered, but the channel or a file does not exist.
    NotFound,
    /// The server answered with an error (5xx, 403, 429 …).
    Server,
    /// The address is not allowed (plain HTTP to a remote host).
    BadUrl,
    /// The address does not serve a valid Bonegrader channel.
    Invalid,
    /// The manifest's signature is missing or wrong.
    Signature,
    /// This Bonegrader is too old for the channel.
    TooOld,
    /// The channel changed after the player's check.
    Stale,
    /// A file is held open by another program (Minecraft still running?).
    Locked,
    /// The disk is full.
    DiskFull,
    /// No permission to write in the instance folder.
    Permission,
    /// A download arrived damaged or incomplete.
    Corrupt,
    /// The player cancelled; nothing was changed.
    Cancelled,
    /// The instance folder does not exist (any more).
    Instance,
    Other,
}

/// Classify `err` by the most specific error anywhere in its chain.
pub fn classify(err: &anyhow::Error) -> Kind {
    let mut from_io = None;
    for e in err.chain() {
        if let Some(kind) = classify_one(e) {
            return kind;
        }
        if from_io.is_none() {
            from_io = e.downcast_ref::<io::Error>().and_then(classify_io);
        }
    }
    from_io.unwrap_or(Kind::Other)
}

fn classify_one(e: &(dyn StdError + 'static)) -> Option<Kind> {
    let kind = if e.is::<Cancelled>() {
        Kind::Cancelled
    } else if e.is::<StaleManifest>() {
        Kind::Stale
    } else if e.is::<ClientTooOld>() {
        Kind::TooOld
    } else if let Some(m) = e.downcast_ref::<ManifestError>() {
        match m {
            ManifestError::UnsupportedSchema { .. } => Kind::TooOld,
            _ => Kind::Invalid,
        }
    } else if e.is::<NotAManifest>() {
        Kind::Invalid
    } else if e.is::<TrustError>() {
        Kind::Signature
    } else if e.is::<InstanceMissing>() {
        Kind::Instance
    } else if e.is::<FileInUse>() {
        Kind::Locked
    } else if e.is::<BadDownload>() {
        Kind::Corrupt
    } else if e.is::<InsecureUrl>() {
        Kind::BadUrl
    } else if e.is::<Unreachable>() {
        Kind::Offline
    } else {
        match e.downcast_ref::<HttpStatus>()?.code {
            404 | 410 => Kind::NotFound,
            _ => Kind::Server,
        }
    };
    Some(kind)
}

fn classify_io(e: &io::Error) -> Option<Kind> {
    use io::ErrorKind as K;
    match e.raw_os_error() {
        // ENOSPC / EDQUOT
        Some(28 | 122) if cfg!(target_os = "linux") => return Some(Kind::DiskFull),
        Some(28 | 69) if cfg!(target_os = "macos") => return Some(Kind::DiskFull),
        // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL
        Some(39 | 112) if cfg!(windows) => return Some(Kind::DiskFull),
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
        Some(32 | 33) if cfg!(windows) => return Some(Kind::Locked),
        _ => {}
    }
    Some(match e.kind() {
        K::StorageFull => Kind::DiskFull,
        K::PermissionDenied | K::ReadOnlyFilesystem => Kind::Permission,
        K::ResourceBusy | K::ExecutableFileBusy => Kind::Locked,
        K::TimedOut
        | K::ConnectionRefused
        | K::ConnectionReset
        | K::ConnectionAborted
        | K::NotConnected
        | K::BrokenPipe
        | K::HostUnreachable
        | K::NetworkUnreachable
        | K::NetworkDown => Kind::Offline,
        _ => return None,
    })
}

/// The whole chain as one line ("context: cause: cause"), skipping a cause
/// whose text an outer message already contains.
pub fn describe(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for e in err.chain() {
        let text = e.to_string();
        let text = text.trim();
        if text.is_empty() || parts.iter().any(|p| p.contains(text)) {
            continue;
        }
        parts.push(text.to_string());
    }
    parts.join(": ")
}

/// What front-ends receive for a failed action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub kind: Kind,
    /// The outermost message (what was being done).
    pub message: String,
    /// The full, de-duplicated chain (for "Details" and bug reports).
    pub detail: String,
}

impl From<&anyhow::Error> for Report {
    fn from(err: &anyhow::Error) -> Self {
        Self {
            kind: classify(err),
            message: err.to_string(),
            detail: describe(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Context};

    fn io(kind: io::ErrorKind) -> anyhow::Error {
        anyhow::Error::new(io::Error::new(kind, "x"))
    }

    #[test]
    fn typed_errors_win_wherever_they_are_in_the_chain() {
        let e = anyhow::Error::new(Unreachable::new("https://h/main", "refused"))
            .context("Server-Stand laden");
        assert_eq!(classify(&e), Kind::Offline);
        let e = anyhow::Error::new(HttpStatus {
            code: 404,
            url: "u".into(),
        })
        .context("laden");
        assert_eq!(classify(&e), Kind::NotFound);
        let e = anyhow::Error::new(HttpStatus {
            code: 503,
            url: "u".into(),
        });
        assert_eq!(classify(&e), Kind::Server);
        assert_eq!(
            classify(&anyhow::Error::new(TrustError::Unsigned("u".into()))),
            Kind::Signature
        );
        assert_eq!(
            classify(&anyhow::Error::new(Cancelled).context("Update")),
            Kind::Cancelled
        );
        assert_eq!(classify(&anyhow::Error::new(StaleManifest)), Kind::Stale);
        assert_eq!(
            classify(&anyhow::Error::new(ManifestError::UnsupportedSchema {
                found: 2,
                supported: 1
            })),
            Kind::TooOld
        );
        assert_eq!(
            classify(&anyhow::Error::new(FileInUse).context("mods/a.jar ist gesperrt")),
            Kind::Locked
        );
        assert_eq!(
            classify(
                &anyhow::Error::new(BadDownload {
                    file: "a".into(),
                    problem: "kaputt".into()
                })
                .context("Download von a fehlgeschlagen")
            ),
            Kind::Corrupt
        );
        assert_eq!(classify(&anyhow!("irgendwas")), Kind::Other);
    }

    #[test]
    fn io_errors_are_sorted_by_what_the_player_can_do() {
        assert_eq!(
            classify(&io(io::ErrorKind::StorageFull).context("schreiben")),
            Kind::DiskFull
        );
        assert_eq!(
            classify(&io(io::ErrorKind::PermissionDenied)),
            Kind::Permission
        );
        assert_eq!(
            classify(&io(io::ErrorKind::ConnectionReset).context("Lesefehler")),
            Kind::Offline
        );
        assert_eq!(classify(&io(io::ErrorKind::NotFound)), Kind::Other);
        if cfg!(target_os = "linux") {
            let full = anyhow::Error::new(io::Error::from_raw_os_error(28));
            assert_eq!(classify(&full), Kind::DiskFull);
        }
    }

    #[test]
    fn describe_drops_repeated_causes() {
        let inner = io::Error::other("Connection refused (os error 111)");
        let e = anyhow::Error::new(inner)
            .context("http://x/m: Connection Failed: Connection refused (os error 111)")
            .context("Server-Stand von http://x/m laden");
        assert_eq!(
            describe(&e),
            "Server-Stand von http://x/m laden: http://x/m: Connection Failed: Connection refused (os error 111)"
        );
        let report = Report::from(&e);
        assert_eq!(report.message, "Server-Stand von http://x/m laden");
        assert_eq!(report.kind, Kind::Other);
    }

    #[test]
    fn reports_serialize_for_the_ui() {
        let e: anyhow::Error = Err::<(), _>(Cancelled).context("Update").unwrap_err();
        let json = serde_json::to_value(Report::from(&e)).unwrap();
        assert_eq!(json["kind"], "cancelled");
        assert_eq!(json["message"], "Update");
        assert!(json["detail"].as_str().unwrap().contains("Abgebrochen"));
    }
}
