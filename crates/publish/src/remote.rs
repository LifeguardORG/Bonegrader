//! The channel host, reached over SSH: upload a built channel, check the
//! connection, list the published manifests and roll back to an earlier one.
//!
//! Every command runs with `BatchMode`: the admin app has no terminal, so a
//! missing key or an unconfirmed host key fails at once with ssh's own message
//! (which the error carries) instead of waiting for input that never comes.
//! Remote paths and the host are restricted to characters no shell
//! interprets, so they can be placed into remote commands as they are.

use anyhow::{bail, Context, Result};
use bonegrader_core::paths::is_safe_component;
use bonegrader_core::sign::SIGNATURE_FILE;
use serde::Serialize;
use std::borrow::BorrowMut;
use std::path::Path;
use std::process::Command;
use time::OffsetDateTime;

const SSH_OPTS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"];

/// Characters allowed in the SSH target and remote folder: enough for
/// `deploy@host`, `/srv/bonegrader`, `~/sites/x` — and nothing a shell or
/// rsync would interpret.
pub fn is_plain_arg(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._/~:-".contains(c))
}

/// Where the channels live: `deploy@host` and the folder holding them.
#[derive(Debug, Clone)]
pub struct Remote {
    host: String,
    base: String,
    /// The ssh program (tests use a stand-in that runs commands locally).
    pub ssh: String,
}

impl Remote {
    pub fn new(host: &str, base: &str) -> Result<Self> {
        let (host, base) = (host.trim(), base.trim());
        let base = if base.len() > 1 {
            base.trim_end_matches('/')
        } else {
            base
        };
        if !is_plain_arg(host) {
            bail!("SSH-Ziel {host:?} enthält unzulässige Zeichen (erlaubt: Buchstaben, Ziffern, @ . _ ~ : -)");
        }
        if !is_plain_arg(base) {
            bail!("Remote-Ordner {base:?} enthält unzulässige Zeichen (erlaubt: Buchstaben, Ziffern, / @ . _ ~ : -)");
        }
        Ok(Self {
            host: host.into(),
            base: base.into(),
            ssh: "ssh".into(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// `<base>/<channel>` on the host.
    pub fn dest(&self, channel: &str) -> Result<String> {
        if !is_safe_component(channel) {
            bail!("ungültiger Channel-Name: {channel:?}");
        }
        Ok(format!("{}/{channel}", self.base))
    }

    fn ssh(&self, script: &str) -> Command {
        let mut c = Command::new(&self.ssh);
        c.args(SSH_OPTS).arg(&self.host).arg(script);
        c
    }

    fn rsync(&self) -> Command {
        let mut c = Command::new("rsync");
        c.arg("-e")
            .arg(format!("{} {}", self.ssh, SSH_OPTS.join(" ")));
        c
    }
}

/// Run a command; on failure the error carries the last lines it printed
/// (e.g. "Permission denied (publickey)").
fn run(mut cmd: impl BorrowMut<Command>) -> Result<String> {
    let cmd = cmd.borrow_mut();
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd
        .output()
        .with_context(|| format!("{program} starten – ist es installiert?"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let tail = lines[lines.len().saturating_sub(3)..].join(" / ");
        let code = out
            .status
            .code()
            .map_or_else(|| "abgebrochen".to_string(), |c| format!("Exit {c}"));
        if tail.is_empty() {
            bail!("{program} fehlgeschlagen ({code})");
        }
        bail!("{program} fehlgeschlagen ({code}): {tail}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// UTC stamp for history file names (no ':' — Windows-safe if copied).
fn stamp() -> String {
    let n = OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        n.year(),
        u8::from(n.month()),
        n.day(),
        n.hour(),
        n.minute(),
        n.second()
    )
}

/// rsync a built channel dir to the host: blobs first (additive, immutable),
/// then signature and manifest, switched with one remote command (signature
/// first — a client caught in between sees a mismatched pair once and simply
/// retries). The previous manifest is kept in `history/` and its name
/// returned (`None` on the very first upload). Requires `rsync` on both ends.
pub fn upload_channel(out: &Path, remote: &Remote, channel: &str) -> Result<Option<String>> {
    let dest = remote.dest(channel)?;
    let host = remote.host();
    let signed = out.join(SIGNATURE_FILE).exists();
    let saved = format!("manifest-{}.json", stamp());

    run(remote.ssh(&format!("mkdir -p {dest}/files/by-hash {dest}/history")))
        .context("Ordner auf dem Server anlegen")?;
    run(remote
        .rsync()
        .args(["-a", "--ignore-existing"])
        .arg(format!("{}/", out.join("files").display()))
        .arg(format!("{host}:{dest}/files/")))
    .context("Dateien hochladen")?;
    run(remote
        .rsync()
        .arg("-a")
        .arg(out.join("manifest.json"))
        .arg(format!("{host}:{dest}/manifest.json.tmp")))
    .context("Manifest hochladen")?;
    let mut switch = format!(
        "had=no; if [ -f {dest}/manifest.json ]; then cp -p {dest}/manifest.json {dest}/history/{saved}; had=yes; fi; \
         if [ -f {dest}/{SIGNATURE_FILE} ]; then cp -p {dest}/{SIGNATURE_FILE} {dest}/history/{saved}.sig; fi; "
    );
    if signed {
        run(remote
            .rsync()
            .arg("-a")
            .arg(out.join(SIGNATURE_FILE))
            .arg(format!("{host}:{dest}/{SIGNATURE_FILE}.tmp")))
        .context("Signatur hochladen")?;
        switch.push_str(&format!(
            "mv -f {dest}/{SIGNATURE_FILE}.tmp {dest}/{SIGNATURE_FILE} && "
        ));
    } else {
        // An old signature would not match the new manifest.
        switch.push_str(&format!("rm -f {dest}/{SIGNATURE_FILE} && "));
    }
    switch.push_str(&format!(
        "mv -f {dest}/manifest.json.tmp {dest}/manifest.json && echo $had"
    ));
    let had = run(remote.ssh(&switch)).context("Neuen Stand aktivieren")?;
    Ok((had.trim() == "yes").then_some(saved))
}

/// What [`test_connection`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// The remote base folder exists.
    pub exists: bool,
    /// Uploads can write there (or create it, if it is missing).
    pub writable: bool,
    /// rsync is installed on the host.
    pub remote_rsync: bool,
    /// rsync is installed here.
    pub local_rsync: bool,
}

/// Log in (without prompting) and check the folder and tools an upload needs.
pub fn test_connection(remote: &Remote) -> Result<Connection> {
    let base = &remote.base;
    let out = run(remote.ssh(&format!(
        "if [ -d {base} ]; then echo exists; [ -w {base} ] && echo writable; \
         else p=$(dirname {base}); [ -w \"$p\" ] && echo writable; fi; \
         command -v rsync >/dev/null 2>&1 && echo rsync; echo ok"
    )))
    .with_context(|| format!("Anmelden bei {}", remote.host))?;
    let has = |w: &str| out.lines().any(|l| l.trim() == w);
    if !has("ok") {
        bail!("unerwartete Antwort vom Server: {}", out.trim());
    }
    let local_rsync = Command::new("rsync")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    Ok(Connection {
        exists: has("exists"),
        writable: has("writable"),
        remote_rsync: has("rsync"),
        local_rsync,
    })
}

/// One published manifest of a channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// `manifest-<stamp>.json` in `history/`; empty for the live manifest.
    pub name: String,
    pub live: bool,
    /// When that manifest was built.
    pub generated_at: Option<String>,
    /// When it was replaced (the stamp in its file name, UTC).
    pub replaced_at: Option<String>,
    /// Managed files (mods, resource and shader packs).
    pub files: usize,
    pub signed: bool,
}

/// The live manifest and the earlier ones in `history/`, newest first.
pub fn list_history(remote: &Remote, channel: &str) -> Result<Vec<HistoryEntry>> {
    let dest = remote.dest(channel)?;
    let out = run(remote.ssh(&format!(
        "cd {dest} 2>/dev/null || exit 0; \
         for f in manifest.json history/manifest-*.json; do \
           [ -f \"$f\" ] || continue; \
           if [ -f \"$f.sig\" ]; then s=signed; else s=unsigned; fi; \
           printf '%s\\t%s\\t%s\\t' \"$f\" \"$s\" \"$(grep -c '\"category\"' \"$f\")\"; \
           grep -m1 '\"generatedAt\"' \"$f\" || echo; \
         done"
    )))
    .with_context(|| format!("Verlauf von {dest} lesen"))?;
    Ok(parse_history(&out))
}

fn parse_history(out: &str) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = out
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let path = parts.next()?;
            let signed = parts.next()? == "signed";
            let files = parts.next()?.trim().parse().unwrap_or(0);
            let generated_at = parts.next().and_then(|l| json_string(l, "generatedAt"));
            if path == "manifest.json" {
                return Some(HistoryEntry {
                    name: String::new(),
                    live: true,
                    generated_at,
                    replaced_at: None,
                    files,
                    signed,
                });
            }
            let name = path.strip_prefix("history/")?;
            is_history_name(name).then(|| HistoryEntry {
                name: name.to_string(),
                live: false,
                generated_at,
                replaced_at: name
                    .strip_prefix("manifest-")
                    .and_then(|s| s.strip_suffix(".json"))
                    .map(str::to_string),
                files,
                signed,
            })
        })
        .collect();
    // Live first, then the most recently replaced.
    entries.sort_by(|a, b| b.live.cmp(&a.live).then_with(|| b.name.cmp(&a.name)));
    entries
}

/// `manifest-20261002-164403.json` — the only names a rollback accepts.
pub fn is_history_name(name: &str) -> bool {
    name.strip_prefix("manifest-")
        .and_then(|s| s.strip_suffix(".json"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
}

/// The string value of `"key": "…"` in a line of pretty-printed JSON.
fn json_string(line: &str, key: &str) -> Option<String> {
    let rest = &line[line.find(&format!("\"{key}\""))? + key.len() + 2..];
    let rest = rest
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let value = &rest[..rest.find('"')?];
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-:.+".contains(c))
        .then(|| value.to_string())
}

/// Switch the channel back to `history/<name>`. The manifest it replaces is
/// itself saved to `history/` first (so a rollback can be rolled back); its
/// name is returned.
pub fn rollback(remote: &Remote, channel: &str, name: &str) -> Result<String> {
    if !is_history_name(name) {
        bail!("ungültiger Verlaufseintrag: {name:?}");
    }
    let dest = remote.dest(channel)?;
    let saved = format!("manifest-{}.json", stamp());
    run(remote.ssh(&format!(
        "set -e; cd {dest}; \
         [ -f history/{name} ] || {{ echo 'Verlaufseintrag {name} fehlt' >&2; exit 1; }}; \
         cp -p manifest.json history/{saved}; \
         if [ -f {SIGNATURE_FILE} ]; then cp -p {SIGNATURE_FILE} history/{saved}.sig; fi; \
         cp -p history/{name} manifest.json.tmp; \
         if [ -f history/{name}.sig ]; then cp -p history/{name}.sig {SIGNATURE_FILE}.tmp && mv -f {SIGNATURE_FILE}.tmp {SIGNATURE_FILE}; \
         else rm -f {SIGNATURE_FILE}; fi; \
         mv -f manifest.json.tmp manifest.json"
    )))
    .with_context(|| format!("Rollback von {channel} auf {name}"))?;
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_checked() {
        assert!(is_plain_arg("deploy@bonegrader.example.com"));
        assert!(is_plain_arg("/srv/bonegrader"));
        assert!(!is_plain_arg("host; rm -rf /"));
        assert!(!is_plain_arg("/srv/x'y"));
        assert!(!is_plain_arg("-oProxyCommand=evil"));
        assert!(Remote::new("h", "/srv/$(evil)").is_err());
        assert!(Remote::new("-oProxyCommand=x", "/srv").is_err());
        let r = Remote::new(" deploy@h ", "/srv/bonegrader/").unwrap();
        assert_eq!(r.dest("main").unwrap(), "/srv/bonegrader/main");
        assert!(r.dest("../main").is_err());
        assert!(rollback(&r, "main", "manifest.json; rm -rf /").is_err());
    }

    #[test]
    fn history_names_and_lines_are_parsed_strictly() {
        assert!(is_history_name("manifest-20261002-164403.json"));
        assert!(!is_history_name("manifest-.json"));
        assert!(!is_history_name("manifest-2026;x.json"));
        assert!(!is_history_name("../manifest-1.json"));
        let out = "manifest.json\tsigned\t142\t  \"generatedAt\": \"2026-10-02T16:44:03Z\",\n\
                   history/manifest-20261001-120000.json\tunsigned\t140\t  \"generatedAt\": \"2026-10-01T11:58:00Z\",\n\
                   history/manifest-20261002-164403.json\tsigned\t141\t\n\
                   history/evil;name.json\tsigned\t1\t\n";
        let h = parse_history(out);
        assert_eq!(h.len(), 3, "{h:?}");
        assert!(h[0].live && h[0].signed && h[0].files == 142);
        assert_eq!(h[0].generated_at.as_deref(), Some("2026-10-02T16:44:03Z"));
        assert_eq!(h[1].name, "manifest-20261002-164403.json");
        assert_eq!(h[1].replaced_at.as_deref(), Some("20261002-164403"));
        assert_eq!(h[1].generated_at, None);
        assert_eq!(h[2].name, "manifest-20261001-120000.json");
        assert!(!h[2].signed);
        assert_eq!(
            json_string(r#"  "generatedAt": "<script>","#, "generatedAt"),
            None
        );
    }

    /// End to end against a local "server": an ssh stand-in that runs the
    /// remote command here, and the real rsync (Linux only: macOS ships a
    /// different rsync implementation).
    #[cfg(target_os = "linux")]
    #[test]
    fn upload_history_and_rollback_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("rsync").arg("--version").output().is_err() {
            eprintln!("rsync not installed – skipped");
            return;
        }
        let tmp = tempfile::Builder::new()
            .prefix("bg-remote-")
            .tempdir()
            .unwrap();
        let fake_ssh = tmp.path().join("ssh");
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\n# ssh stand-in: skip options and host, run the command here\n\
             while [ \"$1\" = \"-o\" ] || [ \"$1\" = \"-l\" ]; do shift 2; done\nshift\nexec sh -c \"$*\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let server = tmp.path().join("srv");
        let mut remote = Remote::new("deploy@localhost", server.to_str().unwrap()).unwrap();
        remote.ssh = fake_ssh.display().to_string();

        let conn = test_connection(&remote).unwrap();
        assert!(
            !conn.exists && conn.writable && conn.remote_rsync && conn.local_rsync,
            "{conn:?}"
        );

        let build = |v: &str, signed: bool| {
            let out = tmp.path().join(format!("out-{v}"));
            std::fs::create_dir_all(out.join("files/by-hash")).unwrap();
            std::fs::write(out.join(format!("files/by-hash/blob{v}")), v).unwrap();
            std::fs::write(
                out.join("manifest.json"),
                format!("{{\n  \"generatedAt\": \"2026-10-0{v}T10:00:00Z\",\n  \"files\": [{{\n    \"category\": \"mod\"\n  }}]\n}}\n"),
            )
            .unwrap();
            if signed {
                std::fs::write(out.join(SIGNATURE_FILE), format!("sig{v}")).unwrap();
            }
            out
        };
        assert_eq!(
            upload_channel(&build("1", true), &remote, "main").unwrap(),
            None
        );
        std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct stamps
        let saved = upload_channel(&build("2", false), &remote, "main")
            .unwrap()
            .unwrap();
        let live = server.join("main");
        assert!(live.join("files/by-hash/blob1").is_file(), "blobs are kept");
        assert!(
            !live.join(SIGNATURE_FILE).exists(),
            "stale signature removed"
        );
        assert!(live.join("history").join(format!("{saved}.sig")).is_file());

        let h = list_history(&remote, "main").unwrap();
        assert_eq!(h.len(), 2, "{h:?}");
        assert!(h[0].live && !h[0].signed);
        assert_eq!(h[0].generated_at.as_deref(), Some("2026-10-02T10:00:00Z"));
        assert_eq!(h[1].name, saved);
        assert!(h[1].signed && h[1].files == 1);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let backup = rollback(&remote, "main", &saved).unwrap();
        let text = std::fs::read_to_string(live.join("manifest.json")).unwrap();
        assert!(text.contains("2026-10-01"), "{text}");
        assert_eq!(
            std::fs::read_to_string(live.join(SIGNATURE_FILE)).unwrap(),
            "sig1"
        );
        assert!(
            live.join("history").join(&backup).is_file(),
            "rollback is undoable"
        );
        assert!(rollback(&remote, "main", "manifest-19990101-000000.json").is_err());
        assert!(
            list_history(&remote, "beta").unwrap().is_empty(),
            "no channel yet"
        );
    }
}
