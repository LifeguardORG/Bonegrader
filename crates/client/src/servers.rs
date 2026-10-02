//! The in-game multiplayer list (`servers.dat`, uncompressed NBT).
//!
//! Bonegrader adds the pack's server once, so players find it right away. The
//! file is read as generic NBT, so server icons and fields of newer Minecraft
//! versions survive untouched, and the previous file is kept as a backup.

use anyhow::{bail, Context, Result};
use fastnbt::Value;
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

pub const SERVERS_FILE: &str = "servers.dat";
const BACKUP_FILE: &str = "servers.dat.bonegrader-backup";

/// True if both addresses name the same server (case-insensitive, the default
/// port `25565` optional).
pub fn same_address(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

fn normalize(address: &str) -> String {
    let a = address.trim().to_ascii_lowercase();
    a.strip_suffix(":25565").map(str::to_string).unwrap_or(a)
}

/// Whether the instance's server list already contains `address`.
pub fn has_server(instance: &Path, address: &str) -> Result<bool> {
    let root = read(&instance.join(SERVERS_FILE))?;
    Ok(root
        .as_ref()
        .and_then(|r| r.get("servers"))
        .is_some_and(|list| contains(list, address)))
}

/// Append the server unless its address is already listed. Returns whether it
/// was added.
pub fn add_server(instance: &Path, name: &str, address: &str) -> Result<bool> {
    let path = instance.join(SERVERS_FILE);
    let mut root = read(&path)?.unwrap_or_default();
    let list = root
        .entry("servers".to_string())
        .or_insert_with(|| Value::List(Vec::new()));
    if contains(list, address) {
        return Ok(false);
    }
    let Value::List(items) = list else {
        bail!("{}: unerwartetes Format", path.display());
    };
    let mut entry = HashMap::new();
    entry.insert("name".to_string(), Value::String(name.to_string()));
    entry.insert("ip".to_string(), Value::String(address.to_string()));
    items.push(Value::Compound(entry));

    let bytes = fastnbt::to_bytes(&Value::Compound(root)).context("Serverliste kodieren")?;
    if path.exists() {
        fs::copy(&path, instance.join(BACKUP_FILE)).context("Sicherung der Serverliste anlegen")?;
    }
    let tmp = instance.join(format!("{SERVERS_FILE}.tmp"));
    fs::write(&tmp, bytes).with_context(|| format!("{} schreiben", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("{} ersetzen", path.display()))?;
    Ok(true)
}

fn read(path: &Path) -> Result<Option<HashMap<String, Value>>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("{} lesen", path.display())),
    };
    match fastnbt::from_bytes::<Value>(&bytes) {
        Ok(Value::Compound(root)) => Ok(Some(root)),
        Ok(_) => bail!("{}: unerwartetes Format", path.display()),
        Err(e) => bail!("{} ist beschädigt: {e}", path.display()),
    }
}

fn contains(list: &Value, address: &str) -> bool {
    let Value::List(items) = list else {
        return false;
    };
    items.iter().any(|item| match item {
        Value::Compound(c) => {
            matches!(c.get("ip"), Some(Value::String(ip)) if same_address(ip, address))
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("bg-servers-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn existing_list() -> Vec<u8> {
        let mut friend = HashMap::new();
        friend.insert("name".to_string(), Value::String("Freunde".into()));
        friend.insert(
            "ip".to_string(),
            Value::String("friends.example.org".into()),
        );
        friend.insert("icon".to_string(), Value::String("iVBORw0KGgo=".into()));
        friend.insert("acceptTextures".to_string(), Value::Byte(1));
        let mut root = HashMap::new();
        root.insert(
            "servers".to_string(),
            Value::List(vec![Value::Compound(friend)]),
        );
        fastnbt::to_bytes(&Value::Compound(root)).unwrap()
    }

    #[test]
    fn creates_the_list_when_missing() {
        let d = dir("new");
        assert!(!has_server(d.path(), "play.example.com").unwrap());
        assert!(add_server(d.path(), "BonesAndBees", "play.example.com").unwrap());
        assert!(has_server(d.path(), "PLAY.example.com:25565").unwrap());
        assert!(!d.path().join(BACKUP_FILE).exists(), "nothing to back up");
    }

    #[test]
    fn appends_and_keeps_everything_else() {
        let d = dir("append");
        let original = existing_list();
        std::fs::write(d.path().join(SERVERS_FILE), &original).unwrap();
        assert!(add_server(d.path(), "BonesAndBees", "play.example.com").unwrap());

        let root: Value =
            fastnbt::from_bytes(&std::fs::read(d.path().join(SERVERS_FILE)).unwrap()).unwrap();
        let Value::Compound(root) = root else {
            panic!()
        };
        let Some(Value::List(items)) = root.get("servers") else {
            panic!()
        };
        assert_eq!(items.len(), 2);
        let Value::Compound(first) = &items[0] else {
            panic!()
        };
        assert_eq!(
            first.get("icon"),
            Some(&Value::String("iVBORw0KGgo=".into())),
            "icon kept"
        );
        assert_eq!(
            first.get("acceptTextures"),
            Some(&Value::Byte(1)),
            "byte stays a byte"
        );
        let Value::Compound(ours) = &items[1] else {
            panic!()
        };
        assert_eq!(
            ours.get("ip"),
            Some(&Value::String("play.example.com".into()))
        );
        assert_eq!(std::fs::read(d.path().join(BACKUP_FILE)).unwrap(), original);
    }

    #[test]
    fn does_not_add_twice() {
        let d = dir("twice");
        assert!(add_server(d.path(), "B", "play.example.com").unwrap());
        assert!(!add_server(d.path(), "B", "Play.Example.com:25565").unwrap());
    }

    #[test]
    fn leaves_a_corrupt_list_alone() {
        let d = dir("corrupt");
        std::fs::write(d.path().join(SERVERS_FILE), b"not nbt").unwrap();
        assert!(has_server(d.path(), "x").is_err());
        assert!(add_server(d.path(), "B", "x").is_err());
        assert_eq!(
            std::fs::read(d.path().join(SERVERS_FILE)).unwrap(),
            b"not nbt"
        );
    }

    #[test]
    fn addresses_compare_loosely() {
        assert!(same_address("Play.Example.com", "play.example.com:25565"));
        assert!(!same_address("play.example.com:25566", "play.example.com"));
    }
}
