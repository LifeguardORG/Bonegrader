//! End-to-end tests of the update loop against real (temp) instances and a
//! filesystem-backed "server": compute_plan -> finalize -> apply, including
//! failures, rollback, resume and undo.

mod common;

use bonegrader_client::apply::{
    apply_with_progress, ApplyOptions, ApplyReport, Cancelled, Progress, BACKUP_DIR, TMP_DIR,
};
use bonegrader_client::exec::{finalize, Decisions};
use bonegrader_client::session::undo_last;
use bonegrader_core::diff::compute_plan;
use bonegrader_core::hash::sha1_bytes;
use bonegrader_core::manifest::{Category, Manifest};
use bonegrader_core::scan::LocalFile;
use bonegrader_core::state::ClientState;
use common::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn opts() -> ApplyOptions {
    ApplyOptions {
        retry_delay: Duration::ZERO,
        ..ApplyOptions::default()
    }
}

/// Plan against `scanned` + `state`, finalize with `decisions`, apply.
fn run(
    inst: &Path,
    man: &Manifest,
    scanned: &[LocalFile],
    state: &ClientState,
    decisions: &Decisions,
    server: &MockServer,
    ts: &str,
) -> anyhow::Result<ApplyReport> {
    let plan = compute_plan(man, scanned, state);
    let exec = finalize(&plan, decisions);
    apply_with_progress(inst, man, &exec, server, BASE, state, ts, &opts(), &|_| {})
}

fn state_with(entries: &[(&str, &[u8], &[&str])]) -> ClientState {
    let mut st = ClientState {
        instance_path: "/x".into(),
        launcher_type: "manual".into(),
        channel: "main".into(),
        last_manifest_generated_at: Some("2026-07-01T00:00:00Z".into()),
        ..Default::default()
    };
    for (p, c, ids) in entries {
        st.managed.insert(p.to_string(), managed(c, ids));
    }
    st
}

#[test]
fn full_update_cycle_preserves_user_mod_and_backs_up_replacements() {
    let a = b"AAA"; // up-to-date managed mod
    let create_old = b"CREATE-OLD"; // managed, will be version-bumped
    let create_new = b"CREATE-NEW-6.0.11";
    let personal = b"MY-PERSONAL-CLIENT-MOD"; // user's own -> must survive
    let stale = b"STALE"; // managed, dropped server-side -> removed
    let newmod = b"BRAND-NEW"; // brand-new managed mod

    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/a.jar"), a);
    write(&i.join("mods/create-6.0.10.jar"), create_old);
    write(&i.join("mods/personal.jar"), personal);
    write(&i.join("mods/stale.jar"), stale);

    let server = MockServer::new();
    server.put_blob(create_new);
    server.put_blob(newmod);
    let man = manifest(vec![
        mod_entry("mods/a.jar", a, "a"),
        mod_entry("mods/create-6.0.11.jar", create_new, "create"),
        mod_entry("mods/newmod.jar", newmod, "newmod"),
    ]);
    let state = state_with(&[
        ("mods/a.jar", a, &["a"]),
        ("mods/create-6.0.10.jar", create_old, &["create"]),
        ("mods/stale.jar", stale, &["stale"]),
    ]);
    let scanned = vec![
        local("mods/a.jar", a, &["a"]),
        local("mods/create-6.0.10.jar", create_old, &["create"]),
        local("mods/personal.jar", personal, &["personal"]),
        local("mods/stale.jar", stale, &["stale"]),
    ];

    let plan = compute_plan(&man, &scanned, &state);
    assert_eq!(plan.removals, vec!["mods/stale.jar".to_string()]);
    assert_eq!(plan.user_extras, vec!["mods/personal.jar".to_string()]);
    let report = run(
        i,
        &man,
        &scanned,
        &state,
        &Decisions::default(),
        &server,
        "test-ts",
    )
    .unwrap();

    assert_eq!(read(i.join("mods/a.jar")), a, "a untouched");
    assert_eq!(read(i.join("mods/create-6.0.11.jar")), create_new);
    assert!(
        !i.join("mods/create-6.0.10.jar").exists(),
        "old create removed"
    );
    assert_eq!(read(i.join("mods/newmod.jar")), newmod);
    assert_eq!(
        read(i.join("mods/personal.jar")),
        personal,
        "USER MOD MUST BE UNTOUCHED"
    );
    assert!(!i.join("mods/stale.jar").exists(), "stale removed");
    assert!(!i.join(TMP_DIR).exists(), "staging cleaned up");

    let backup = i.join(BACKUP_DIR).join("test-ts");
    assert_eq!(read(backup.join("mods/create-6.0.10.jar")), create_old);
    assert_eq!(read(backup.join("mods/stale.jar")), stale);
    assert!(backup.join("update.json").exists(), "undo record written");

    assert_eq!((report.downloaded, report.reused), (2, 0));
    assert_eq!(
        (report.installed, report.deleted, report.replaced),
        (2, 1, 1)
    );
    let keys: Vec<&String> = report.new_state.managed.keys().collect();
    assert_eq!(
        keys,
        vec!["mods/a.jar", "mods/create-6.0.11.jar", "mods/newmod.jar"]
    );
    assert_eq!(report.new_state.pack_name.as_deref(), Some("BonesAndBees"));

    // Second run is a clean no-op; the personal mod is still a kept extra.
    let scanned2 = vec![
        local("mods/a.jar", a, &["a"]),
        local("mods/create-6.0.11.jar", create_new, &["create"]),
        local("mods/newmod.jar", newmod, &["newmod"]),
        local("mods/personal.jar", personal, &["personal"]),
    ];
    let plan2 = compute_plan(&man, &scanned2, &report.new_state);
    assert!(plan2.is_noop(), "{plan2:?}");
    assert_eq!(plan2.user_extras, vec!["mods/personal.jar".to_string()]);
}

#[test]
fn progress_events_are_emitted_in_order() {
    let (a, b) = (b"AAA".as_slice(), b"BBBB".as_slice());
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(a);
    server.put_blob(b);
    let man = manifest(vec![
        mod_entry("mods/a.jar", a, "a"),
        mod_entry("mods/b.jar", b, "b"),
    ]);
    let state = ClientState::default();
    let exec = finalize(&compute_plan(&man, &[], &state), &Decisions::default());

    let events = Mutex::new(Vec::<Progress>::new());
    let report = apply_with_progress(
        inst.path(),
        &man,
        &exec,
        &server,
        BASE,
        &state,
        "ts",
        &opts(),
        &|p| events.lock().unwrap().push(p.clone()),
    )
    .unwrap();

    let ev = events.into_inner().unwrap();
    assert_eq!(ev.first().unwrap().phase, "download");
    assert!(ev.iter().any(|e| e.phase == "apply"));
    let last = ev.last().unwrap();
    assert_eq!(last.phase, "done");
    assert_eq!((last.done_files, last.total_files), (2, 2));
    assert_eq!(last.total_bytes, (a.len() + b.len()) as u64);
    assert_eq!(last.done_bytes, last.total_bytes);
    let mut prev = 0;
    for e in &ev {
        assert!(e.done_bytes >= prev, "monotonic without retries");
        prev = e.done_bytes;
    }
    assert_eq!(report.installed, 2);
}

#[test]
fn identical_content_twice_in_the_manifest_installs_both() {
    // Same resource pack under two names. Before: the shared staging file was
    // moved away by the first install, the second failed mid-apply.
    let pack = b"SAME-PACK-CONTENT";
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(pack);
    let man = manifest(vec![
        entry(Category::Resourcepack, "resourcepacks/a.zip", pack, None),
        entry(Category::Resourcepack, "resourcepacks/b.zip", pack, None),
    ]);
    let report = run(
        inst.path(),
        &man,
        &[],
        &ClientState::default(),
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap();
    assert_eq!(read(inst.path().join("resourcepacks/a.zip")), pack);
    assert_eq!(read(inst.path().join("resourcepacks/b.zip")), pack);
    assert_eq!(
        report.downloaded, 1,
        "fetched once, copied for the second target"
    );
    assert_eq!(server.calls(&MockServer::blob_url(pack)), 1);
}

#[test]
fn splitting_a_jar_keeps_both_mods_and_the_old_backup() {
    // a.jar declared [[mods]] a+b; the pack now ships a.jar (same name, new
    // content) and b.jar. Before: a.jar ended up missing and its backup was
    // overwritten with the new version.
    let (old, new_a, b) = (
        b"A-OLD-a-and-b".as_slice(),
        b"A-NEW".as_slice(),
        b"B-ALONE".as_slice(),
    );
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/a.jar"), old);
    let server = MockServer::new();
    server.put_blob(new_a);
    server.put_blob(b);
    let man = manifest(vec![
        mod_entry("mods/a.jar", new_a, "a"),
        mod_entry("mods/b.jar", b, "b"),
    ]);
    let state = state_with(&[("mods/a.jar", old, &["a", "b"])]);
    let scanned = vec![local("mods/a.jar", old, &["a", "b"])];

    run(
        i,
        &man,
        &scanned,
        &state,
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap();
    assert_eq!(read(i.join("mods/a.jar")), new_a);
    assert_eq!(read(i.join("mods/b.jar")), b);
    assert_eq!(
        read(i.join(BACKUP_DIR).join("ts/mods/a.jar")),
        old,
        "backup keeps the old version"
    );
}

#[test]
fn identical_duplicate_is_removed_and_the_next_run_is_clean() {
    let jei = b"JEI-19.21";
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/jei.jar"), jei);
    write(&i.join("mods/jei (1).jar"), jei);
    let server = MockServer::new();
    let man = manifest(vec![mod_entry("mods/jei.jar", jei, "jei")]);
    let state = state_with(&[("mods/jei.jar", jei, &["jei"])]);
    let scanned = vec![
        local("mods/jei.jar", jei, &["jei"]),
        local("mods/jei (1).jar", jei, &["jei"]),
    ];

    let report = run(
        i,
        &man,
        &scanned,
        &state,
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!i.join("mods/jei (1).jar").exists());
    assert_eq!(read(i.join(BACKUP_DIR).join("ts/mods/jei (1).jar")), jei);
    let plan2 = compute_plan(
        &man,
        &[local("mods/jei.jar", jei, &["jei"])],
        &report.new_state,
    );
    assert!(plan2.is_noop());
}

#[test]
fn a_renamed_local_copy_is_used_instead_of_downloading() {
    let jei = b"JEI-19.21";
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/jei (1).jar"), jei);
    let server = MockServer::new(); // has no blobs at all
    let man = manifest(vec![mod_entry("mods/jei.jar", jei, "jei")]);
    let state = state_with(&[("mods/other.jar", b"o", &["other"])]);
    let scanned = vec![local("mods/jei (1).jar", jei, &["jei"])];

    let report = run(
        i,
        &man,
        &scanned,
        &state,
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap();
    assert_eq!((report.downloaded, report.reused), (0, 1));
    assert_eq!(read(i.join("mods/jei.jar")), jei);
    assert!(!i.join("mods/jei (1).jar").exists());
}

#[test]
fn failed_download_leaves_the_instance_untouched_and_resumes() {
    // Jobs run in SHA-1 order; with one worker, pick contents so the blob
    // missing on the server comes last and the good one is staged before.
    let good = b"GOOD".to_vec();
    let bad = (0..)
        .map(|n| format!("BAD{n}").into_bytes())
        .find(|c| sha1_bytes(c) > sha1_bytes(&good))
        .unwrap();
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/old.jar"), b"OLD");
    let server = MockServer::new();
    server.put_blob(&good); // `bad` is missing: 404
    let man = manifest(vec![
        mod_entry("mods/good.jar", &good, "good"),
        mod_entry("mods/bad.jar", &bad, "bad"),
    ]);
    let state = state_with(&[("mods/old.jar", b"OLD", &["old"])]);
    let scanned = vec![local("mods/old.jar", b"OLD", &["old"])];
    let serial = ApplyOptions {
        parallel: 1,
        ..opts()
    };
    let exec = finalize(&compute_plan(&man, &scanned, &state), &Decisions::default());
    let before = tree(i);

    let err = apply_with_progress(
        i,
        &man,
        &exec,
        &server,
        BASE,
        &state,
        "ts1",
        &serial,
        &|_| {},
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("mods/bad.jar"), "{err:#}");
    let after: Vec<String> = tree(i)
        .into_iter()
        .filter(|p| !p.starts_with(TMP_DIR))
        .collect();
    assert_eq!(after, before, "instance untouched");
    assert!(!i.join(BACKUP_DIR).exists());
    assert!(
        i.join(TMP_DIR).join(sha1_bytes(&good)).is_file(),
        "verified blob kept for the retry"
    );

    // The blob appears; the retry only fetches what is still missing.
    server.put_blob(&bad);
    let report = apply_with_progress(
        i,
        &man,
        &exec,
        &server,
        BASE,
        &state,
        "ts2",
        &serial,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        server.calls(&MockServer::blob_url(&good)),
        1,
        "not downloaded twice"
    );
    assert_eq!((report.downloaded, report.reused), (1, 1));
    assert_eq!(read(i.join("mods/bad.jar")), bad);
    assert!(!i.join("mods/old.jar").exists());
}

#[test]
fn cancelling_a_download_changes_nothing_and_keeps_finished_files() {
    let first = b"FIRST".to_vec();
    let second = (0..)
        .map(|n| format!("SECOND{n}").into_bytes())
        .find(|c| sha1_bytes(c) > sha1_bytes(&first))
        .unwrap();
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/old.jar"), b"OLD");
    let server = MockServer::new();
    server.put_blob(&first);
    server.put_blob(&second);
    let man = manifest(vec![
        mod_entry("mods/first.jar", &first, "first"),
        mod_entry("mods/second.jar", &second, "second"),
    ]);
    let state = state_with(&[("mods/old.jar", b"OLD", &["old"])]);
    let scanned = vec![local("mods/old.jar", b"OLD", &["old"])];
    let exec = finalize(&compute_plan(&man, &scanned, &state), &Decisions::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let o = ApplyOptions {
        parallel: 1,
        cancel: Some(cancel.clone()),
        ..opts()
    };
    let before = tree(i);

    // The player cancels once the first file is in.
    let progress = |p: &Progress| {
        if p.done_files >= 1 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let err = apply_with_progress(i, &man, &exec, &server, BASE, &state, "ts1", &o, &progress)
        .unwrap_err();
    assert!(err.is::<Cancelled>(), "{err:#}");
    let after: Vec<String> = tree(i)
        .into_iter()
        .filter(|p| !p.starts_with(TMP_DIR))
        .collect();
    assert_eq!(after, before, "instance untouched");
    assert!(!i.join(BACKUP_DIR).exists());
    assert_eq!(
        server.calls(&MockServer::blob_url(&second)),
        0,
        "no new download started"
    );

    // Next attempt: the finished file is reused.
    cancel.store(false, Ordering::SeqCst);
    let report =
        apply_with_progress(i, &man, &exec, &server, BASE, &state, "ts2", &o, &|_| {}).unwrap();
    assert_eq!((report.downloaded, report.reused), (1, 1));
    assert_eq!(server.calls(&MockServer::blob_url(&first)), 1);
    assert_eq!(read(i.join("mods/second.jar")), second);
}

#[test]
fn running_downloads_finish_when_another_one_fails() {
    // Parallel workers: a hard failure stops new downloads, but transfers
    // already running complete and stay staged.
    let inst = instance();
    let server = MockServer::new();
    let blobs: Vec<Vec<u8>> = (0..6).map(|n| format!("BLOB{n}").into_bytes()).collect();
    for b in &blobs[1..] {
        server.put_blob(b);
    }
    let files = blobs
        .iter()
        .enumerate()
        .map(|(n, b)| mod_entry(&format!("mods/m{n}.jar"), b, &format!("m{n}")))
        .collect();
    let man = manifest(files);
    let exec = finalize(
        &compute_plan(&man, &[], &ClientState::default()),
        &Decisions::default(),
    );
    let err = apply_with_progress(
        inst.path(),
        &man,
        &exec,
        &server,
        BASE,
        &ClientState::default(),
        "ts",
        &opts(),
        &|_| {},
    );
    assert!(err.is_err());
    // Whatever was fetched is complete and verified (no torn files).
    for e in std::fs::read_dir(inst.path().join(TMP_DIR)).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.ends_with(".part"), "no partial files left: {name}");
        assert_eq!(sha1_bytes(&read(p)), name);
    }
}

#[test]
fn transient_failures_are_retried() {
    let a = b"AAAA";
    let inst = instance();
    let server = MockServer::new();
    server.put_blob(a);
    server.fail(&MockServer::blob_url(a), 2); // two 503s, then success
    let man = manifest(vec![mod_entry("mods/a.jar", a, "a")]);
    let report = run(
        inst.path(),
        &man,
        &[],
        &ClientState::default(),
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap();
    assert_eq!(report.downloaded, 1);
    assert_eq!(server.calls(&MockServer::blob_url(a)), 3);
}

#[test]
fn corrupt_downloads_are_rejected() {
    let a = b"AAAA";
    let inst = instance();
    let server = MockServer::new();
    // The server returns other bytes under a's hash.
    write(
        &server
            .root
            .path()
            .join(format!("files/by-hash/{}", sha1_bytes(a))),
        b"EVIL",
    );
    let man = manifest(vec![mod_entry("mods/a.jar", a, "a")]);
    let err = run(
        inst.path(),
        &man,
        &[],
        &ClientState::default(),
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("Prüfsumme") || format!("{err:#}").contains("unvollständig"),
        "{err:#}"
    );
    assert!(!inst.path().join("mods/a.jar").exists());
}

#[test]
fn a_failure_while_committing_rolls_everything_back() {
    let (old, new) = (b"MOD-OLD".as_slice(), b"MOD-NEW".as_slice());
    let pack = b"PACK";
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/m-1.jar"), old);
    write(&i.join("mods/gone.jar"), b"GONE");
    // `resourcepacks` is a *file*: installing into it fails after the mods
    // steps already happened (they sort first).
    write(&i.join("resourcepacks"), b"not a folder");
    let server = MockServer::new();
    server.put_blob(new);
    server.put_blob(pack);
    let man = manifest(vec![
        mod_entry("mods/m-2.jar", new, "m"),
        entry(Category::Resourcepack, "resourcepacks/p.zip", pack, None),
    ]);
    let state = state_with(&[
        ("mods/m-1.jar", old, &["m"]),
        ("mods/gone.jar", b"GONE", &["gone"]),
    ]);
    let scanned = vec![
        local("mods/m-1.jar", old, &["m"]),
        local("mods/gone.jar", b"GONE", &["gone"]),
    ];
    let before = tree(i);

    let err = run(
        i,
        &man,
        &scanned,
        &state,
        &Decisions::default(),
        &server,
        "ts",
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("zurückgenommen"), "{err:#}");
    let after: Vec<String> = tree(i)
        .into_iter()
        .filter(|p| !p.starts_with(TMP_DIR))
        .collect();
    assert_eq!(after, before, "every move undone");
    assert_eq!(read(i.join("mods/m-1.jar")), old);
    assert_eq!(read(i.join("mods/gone.jar")), b"GONE");
    assert!(
        !i.join(BACKUP_DIR).join("ts").exists(),
        "no half backup left behind"
    );
}

#[test]
fn undo_restores_the_files_and_state_from_before() {
    let (v1, v2) = (b"MOD-V1".as_slice(), b"MOD-V2".as_slice());
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/m-1.jar"), v1);
    write(&i.join("mods/gone.jar"), b"GONE");
    write(&i.join("mods/mine.jar"), b"MINE");
    let server = MockServer::new();
    server.put_blob(v2);
    server.put_blob(b"FRESH");
    let man = manifest(vec![
        mod_entry("mods/m-2.jar", v2, "m"),
        mod_entry("mods/fresh.jar", b"FRESH", "fresh"),
    ]);
    let state = state_with(&[
        ("mods/m-1.jar", v1, &["m"]),
        ("mods/gone.jar", b"GONE", &["gone"]),
    ]);
    let scanned = vec![
        local("mods/m-1.jar", v1, &["m"]),
        local("mods/gone.jar", b"GONE", &["gone"]),
        local("mods/mine.jar", b"MINE", &["mine"]),
    ];
    bonegrader_client::session::save_state(i, &state).unwrap();
    let before = tree(i);

    let report = run(
        i,
        &man,
        &scanned,
        &state,
        &Decisions::default(),
        &server,
        "20260801-120000",
    )
    .unwrap();
    bonegrader_client::session::save_state(i, &report.new_state).unwrap();
    assert!(i.join("mods/m-2.jar").exists() && !i.join("mods/m-1.jar").exists());

    let undo = undo_last(i, "20260801-130000").unwrap();
    assert_eq!((undo.restored, undo.removed), (2, 2));
    let now: Vec<String> = tree(i)
        .into_iter()
        .filter(|p| !p.starts_with(BACKUP_DIR))
        .collect();
    assert_eq!(now, before, "files as before the update");
    assert_eq!(read(i.join("mods/m-1.jar")), v1);
    assert_eq!(read(i.join("mods/mine.jar")), b"MINE");
    assert_eq!(
        bonegrader_client::session::load_state(i).unwrap(),
        state,
        "state restored"
    );
    assert!(
        bonegrader_client::session::restorable(i).is_none(),
        "nothing older to undo"
    );
    assert!(undo_last(i, "x").is_err());
}

#[test]
fn old_backups_are_pruned() {
    let inst = instance();
    let i = inst.path();
    let server = MockServer::new();
    let mut state = ClientState::default();
    for (n, content) in [b"V1".as_slice(), b"V2", b"V3", b"V4"].iter().enumerate() {
        server.put_blob(content);
        let man = manifest(vec![mod_entry("mods/m.jar", content, "m")]);
        let scanned: Vec<LocalFile> = if n == 0 {
            vec![]
        } else {
            vec![local("mods/m.jar", &read(i.join("mods/m.jar")), &["m"])]
        };
        let plan = compute_plan(&man, &scanned, &state);
        let exec = finalize(&plan, &Decisions::default());
        let o = ApplyOptions {
            keep_backups: 2,
            ..opts()
        };
        let report = apply_with_progress(
            i,
            &man,
            &exec,
            &server,
            BASE,
            &state,
            &format!("2026080{n}-120000"),
            &o,
            &|_| {},
        )
        .unwrap();
        state = report.new_state;
    }
    let dirs: Vec<String> = std::fs::read_dir(i.join(BACKUP_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(dirs, vec!["20260802-120000", "20260803-120000"]);
}

#[test]
fn keeping_a_collision_retires_the_old_pack_copy() {
    // The pack moves JEI 19.0 -> 19.5; the player has their own JEI and keeps it.
    let (jei_old, jei_new, mine) = (
        b"JEI-19.0".as_slice(),
        b"JEI-19.5".as_slice(),
        b"JEI-MINE".as_slice(),
    );
    let inst = instance();
    let i = inst.path();
    write(&i.join("mods/jei-19.0.jar"), jei_old);
    write(&i.join("mods/my-jei.jar"), mine);
    let server = MockServer::new();
    server.put_blob(jei_new);
    let man = manifest(vec![mod_entry("mods/jei-19.5.jar", jei_new, "jei")]);
    let state = state_with(&[("mods/jei-19.0.jar", jei_old, &["jei"])]);
    let scanned = vec![
        local("mods/jei-19.0.jar", jei_old, &["jei"]),
        local("mods/my-jei.jar", mine, &["jei"]),
    ];
    let mut d = Decisions::default();
    d.keep_collision_local.insert("mods/my-jei.jar".into());

    run(i, &man, &scanned, &state, &d, &server, "ts").unwrap();
    assert_eq!(
        tree(&i.join("mods")),
        vec!["my-jei.jar"],
        "exactly one JEI left"
    );
}
