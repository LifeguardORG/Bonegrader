//! Tests of the shared update pipeline (`session::Updater`): manifest
//! signatures, stale plans, client version gates, the server list and seeds.

mod common;

use bonegrader_client::apply::ApplyOptions;
use bonegrader_client::exec::Decisions;
use bonegrader_client::fetch::Fetcher;
use bonegrader_client::servers;
use bonegrader_client::session::{
    load_state, ClientCompat, SignatureStatus, StaleManifest, Updater,
};
use bonegrader_core::manifest::{Category, ClientInfo, Manifest, ServerInfo};
use bonegrader_core::sign::{PublicKey, SecretKey};
use common::*;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn opts() -> ApplyOptions {
    ApplyOptions {
        retry_delay: Duration::ZERO,
        ..ApplyOptions::default()
    }
}

fn updater<'a>(inst: &'a Path, fetcher: &'a dyn Fetcher, keys: &'a [PublicKey]) -> Updater<'a> {
    Updater {
        instance: inst,
        base_url: BASE,
        fetcher,
        keys,
    }
}

fn pack(content: &[u8]) -> Manifest {
    manifest(vec![entry(
        Category::Resourcepack,
        "resourcepacks/pack.zip",
        content,
        None,
    )])
}

#[test]
fn installs_and_then_has_nothing_left_to_do() {
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"PACK");
    server.publish(&pack(b"PACK"), None);
    let up = updater(inst.path(), &server, &[]);

    let prepared = up.prepare().unwrap();
    assert_eq!(prepared.fetched.signature, SignatureStatus::NotRequired);
    assert!(!prepared.is_noop());
    assert_eq!(prepared.download_bytes(), 4);
    up.apply(&prepared, &Decisions::default(), &opts(), "ts", &|_| {})
        .unwrap();

    assert_eq!(read(inst.path().join("resourcepacks/pack.zip")), b"PACK");
    assert!(load_state(inst.path())
        .unwrap()
        .managed
        .contains_key("resourcepacks/pack.zip"));
    assert!(up.prepare().unwrap().is_noop());
}

#[test]
fn signed_manifests_verify_and_tampering_is_refused() {
    let key = SecretKey::generate().unwrap();
    let keys = vec![key.public()];
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"PACK");
    let bytes = server.publish(&pack(b"PACK"), Some(&key));
    let up = updater(inst.path(), &server, &keys);
    assert_eq!(
        up.prepare().unwrap().fetched.signature,
        SignatureStatus::Verified
    );

    // Swap in a different manifest without a matching signature.
    let mut evil = pack(b"PACK");
    evil.pack_name = "Evil".into();
    write(
        &server.root.path().join("manifest.json"),
        evil.to_json_pretty().unwrap().as_bytes(),
    );
    let err = up.prepare().unwrap_err();
    assert!(format!("{err:#}").contains("Signatur"), "{err:#}");

    // A manifest without signature is refused once keys are trusted.
    write(&server.root.path().join("manifest.json"), &bytes);
    std::fs::remove_file(server.root.path().join("manifest.json.sig")).unwrap();
    let err = up.prepare().unwrap_err();
    assert!(format!("{err:#}").contains("nicht signiert"), "{err:#}");

    // A signature by an untrusted key does not count either.
    let stranger = SecretKey::generate().unwrap();
    server.publish(&pack(b"PACK"), Some(&stranger));
    assert!(up.prepare().is_err());
}

/// Serves an old manifest on the first request and the new one afterwards,
/// with the new signature throughout — a deploy caught half-way.
struct MidDeploy {
    server: MockServer,
    old: Vec<u8>,
    manifest_calls: AtomicUsize,
}

impl Fetcher for MidDeploy {
    fn open(&self, url: &str) -> anyhow::Result<Box<dyn Read + Send>> {
        if url.ends_with("/manifest.json")
            && self.manifest_calls.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Ok(Box::new(std::io::Cursor::new(self.old.clone())));
        }
        self.server.open(url)
    }
}

#[test]
fn a_deploy_in_progress_is_retried_once() {
    let key = SecretKey::generate().unwrap();
    let keys = vec![key.public()];
    let inst = instance();
    let server = MockServer::new();
    let old = server.publish(&pack(b"OLD"), Some(&key));
    server.publish(&pack(b"NEW"), Some(&key));
    let f = MidDeploy {
        server,
        old,
        manifest_calls: AtomicUsize::new(0),
    };
    let prepared = updater(inst.path(), &f, &keys).prepare().unwrap();
    assert_eq!(prepared.fetched.signature, SignatureStatus::Verified);
    assert_eq!(f.manifest_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_plan_reviewed_against_an_older_manifest_is_not_applied() {
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"V1");
    server.put_blob(b"V2");
    server.publish(&pack(b"V1"), None);
    let up = updater(inst.path(), &server, &[]);
    let reviewed = up.prepare().unwrap().fetched.id;

    // The admin publishes while the player looks at the plan.
    server.publish(&pack(b"V2"), None);
    let err = up
        .apply_checked(
            Some(&reviewed),
            &Decisions::default(),
            &opts(),
            "ts",
            &|_| {},
        )
        .unwrap_err();
    assert!(err.downcast_ref::<StaleManifest>().is_some(), "{err:#}");
    assert!(
        !inst.path().join("resourcepacks").exists(),
        "nothing applied"
    );

    let fresh = up.prepare().unwrap().fetched.id;
    up.apply_checked(Some(&fresh), &Decisions::default(), &opts(), "ts", &|_| {})
        .unwrap();
    assert_eq!(read(inst.path().join("resourcepacks/pack.zip")), b"V2");
}

#[test]
fn an_outdated_client_is_told_to_update_and_changes_nothing() {
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"PACK");
    let mut m = pack(b"PACK");
    m.client = Some(ClientInfo {
        min_version: Some("99.0.0".into()),
        latest_version: Some("99.0.0".into()),
        download_url: Some("https://bonegrader.example/".into()),
    });
    server.publish(&m, None);
    let up = updater(inst.path(), &server, &[]);
    let prepared = up.prepare().unwrap();
    assert!(matches!(
        prepared.compat,
        ClientCompat::UpdateRequired { .. }
    ));
    let err = up
        .apply(&prepared, &Decisions::default(), &opts(), "ts", &|_| {})
        .unwrap_err();
    assert!(format!("{err:#}").contains("zu alt"), "{err:#}");
    assert!(!inst.path().join("resourcepacks").exists());
}

#[test]
fn a_newer_manifest_format_asks_for_an_update() {
    let inst = instance();
    let server = MockServer::new();
    write(
        &server.root.path().join("manifest.json"),
        br#"{"schemaVersion":2,"whatever":true}"#,
    );
    let err = updater(inst.path(), &server, &[]).prepare().unwrap_err();
    assert!(format!("{err:#}").contains("aktualisieren"), "{err:#}");
}

#[test]
fn the_server_is_added_once_and_a_removal_sticks() {
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"PACK");
    let mut m = pack(b"PACK");
    m.server = Some(ServerInfo {
        name: "BonesAndBees".into(),
        address: "play.bonesandbees.example".into(),
    });
    server.publish(&m, None);
    let up = updater(inst.path(), &server, &[]);

    let prepared = up.prepare().unwrap();
    assert!(prepared.server.is_some());
    let outcome = up
        .apply(&prepared, &Decisions::default(), &opts(), "ts", &|_| {})
        .unwrap();
    assert!(outcome.server_added);
    assert!(servers::has_server(inst.path(), "play.bonesandbees.example").unwrap());
    assert!(
        up.prepare().unwrap().is_noop(),
        "listed now: nothing left to do"
    );

    // The player deletes the entry; Bonegrader does not bring it back.
    std::fs::remove_file(inst.path().join(servers::SERVERS_FILE)).unwrap();
    assert!(up.prepare().unwrap().server.is_none());
}

#[test]
fn a_corrupt_server_list_is_left_alone_and_retried_later() {
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"PACK");
    let mut m = pack(b"PACK");
    m.server = Some(ServerInfo {
        name: "BonesAndBees".into(),
        address: "play.bonesandbees.example".into(),
    });
    server.publish(&m, None);
    write(&inst.path().join(servers::SERVERS_FILE), b"corrupt");
    let up = updater(inst.path(), &server, &[]);

    let prepared = up.prepare().unwrap();
    assert!(
        prepared.server.is_none(),
        "an unreadable list is never rewritten"
    );
    up.apply(&prepared, &Decisions::default(), &opts(), "ts", &|_| {})
        .unwrap();
    assert_eq!(read(inst.path().join(servers::SERVERS_FILE)), b"corrupt");
    assert_eq!(
        load_state(inst.path()).unwrap().server_added,
        None,
        "not marked as added"
    );

    // Once the player's list is readable again, the server is offered.
    std::fs::remove_file(inst.path().join(servers::SERVERS_FILE)).unwrap();
    assert!(up.prepare().unwrap().server.is_some());
}

#[test]
fn seeds_are_created_once_and_never_overwrite_the_players_files() {
    let inst = instance();
    let i = inst.path();
    write(&i.join("config/b.toml"), b"PLAYER-B");
    let server = MockServer::new();
    server.put_blob(b"PACK");
    server.put_blob(b"SEED-A");
    server.put_blob(b"SEED-B");
    let mut m = pack(b"PACK");
    m.seeds = vec![
        seed("config/a.toml", b"SEED-A"),
        seed("config/b.toml", b"SEED-B"),
    ];
    server.publish(&m, None);
    let up = updater(i, &server, &[]);

    let prepared = up.prepare().unwrap();
    let pending: Vec<&str> = prepared.seeds.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(pending, vec!["config/a.toml"]);
    let outcome = up
        .apply(&prepared, &Decisions::default(), &opts(), "ts", &|_| {})
        .unwrap();
    assert_eq!(outcome.report.seeded, 1);
    assert_eq!(read(i.join("config/a.toml")), b"SEED-A");
    assert_eq!(
        read(i.join("config/b.toml")),
        b"PLAYER-B",
        "player's file untouched"
    );
    let st = load_state(i).unwrap();
    assert!(st.seeded.contains("config/a.toml") && st.seeded.contains("config/b.toml"));

    // The player deletes the seeded file on purpose: it stays deleted.
    std::fs::remove_file(i.join("config/a.toml")).unwrap();
    assert!(up.prepare().unwrap().seeds.is_empty());
}

#[test]
fn the_plan_view_has_what_the_ui_needs() {
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(b"PACK");
    server.publish(&pack(b"PACK"), None);
    let prepared = updater(inst.path(), &server, &[]).prepare().unwrap();
    let json = serde_json::to_value(prepared.view()).unwrap();
    assert_eq!(json["manifestId"], prepared.fetched.id.as_str());
    assert_eq!(json["packName"], "BonesAndBees");
    assert_eq!(json["compat"]["state"], "current");
    assert_eq!(json["signature"], "notRequired");
    assert_eq!(json["assessment"]["suspicious"], false);
    assert_eq!(json["noop"], false);
    assert_eq!(json["downloadBytes"], 4);
    assert_eq!(
        json["plan"]["downloads"][0]["entry"]["path"],
        "resourcepacks/pack.zip"
    );
}
