//! End-to-end test of the client update loop against a real (temp) instance and
//! a filesystem-backed "server", exercising compute_plan -> finalize -> apply.

use std::path::{Path, PathBuf};

use bonegrader_client::apply::{apply, apply_with_progress, Fetcher, Progress};
use bonegrader_client::exec::{finalize, Decisions};
use bonegrader_core::diff::compute_plan;
use bonegrader_core::hash::sha1_bytes;
use bonegrader_core::manifest::{Category, FileEntry, Loader, Manifest};
use bonegrader_core::scan::LocalFile;
use bonegrader_core::state::{ClientState, ManagedEntry};

/// Reads blobs from a local channel directory.
struct FileFetcher {
    root: PathBuf,
}
impl Fetcher for FileFetcher {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.root.join(url))?)
    }
}

fn unique_tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("bonegrader-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn mod_entry(path: &str, content: &[u8], mod_id: &str) -> FileEntry {
    let sha1 = sha1_bytes(content);
    FileEntry {
        category: Category::Mod,
        path: path.into(),
        file_name: path.rsplit('/').next().unwrap().into(),
        size: content.len() as u64,
        sha1: sha1.clone(),
        mod_id: Some(mod_id.into()),
        mod_version: None,
        url: format!("files/by-hash/{sha1}"),
    }
}

fn local(path: &str, content: &[u8], mod_id: &str) -> LocalFile {
    LocalFile {
        category: Category::Mod,
        path: path.into(),
        file_name: path.rsplit('/').next().unwrap().into(),
        size: content.len() as u64,
        sha1: sha1_bytes(content),
        mod_ids: vec![mod_id.into()],
        mod_version: None,
    }
}

fn managed(content: &[u8], mod_id: &str) -> ManagedEntry {
    ManagedEntry {
        sha1: sha1_bytes(content),
        mod_ids: vec![mod_id.into()],
    }
}

#[test]
fn full_update_cycle_preserves_user_mod_and_backs_up_replacements() {
    // ---- contents ----
    let a = b"AAA"; // up-to-date managed mod
    let create_old = b"CREATE-OLD"; // managed, will be version-bumped
    let create_new = b"CREATE-NEW-6.0.11";
    let personal = b"MY-PERSONAL-CLIENT-MOD"; // user's own -> must survive
    let stale = b"STALE"; // managed, dropped server-side -> removed
    let newmod = b"BRAND-NEW"; // brand-new managed mod

    // ---- lay out the instance on disk ----
    let inst = unique_tmp("inst");
    write(&inst.join("mods/a.jar"), a);
    write(&inst.join("mods/create-6.0.10.jar"), create_old);
    write(&inst.join("mods/personal.jar"), personal);
    write(&inst.join("mods/stale.jar"), stale);

    // ---- lay out the "server" channel (blobs for downloads only) ----
    let chan = unique_tmp("chan");
    for c in [create_new.as_slice(), newmod.as_slice()] {
        write(&chan.join(format!("files/by-hash/{}", sha1_bytes(c))), c);
    }
    let manifest = Manifest {
        schema_version: 1,
        pack_name: "BonesAndBees".into(),
        channel: "main".into(),
        generated_at: "2026-07-26T20:00:00Z".into(),
        loader: Loader {
            loader_type: "neoforge".into(),
            mc_version: "1.21.1".into(),
            loader_version: "21.1.234".into(),
        },
        files: vec![
            mod_entry("mods/a.jar", a, "a"),
            mod_entry("mods/create-6.0.11.jar", create_new, "create"),
            mod_entry("mods/newmod.jar", newmod, "newmod"),
        ],
    };

    // ---- prior state: a, old-create and stale are managed; personal is not ----
    let mut state = ClientState {
        instance_path: inst.to_string_lossy().into(),
        launcher_type: "manual".into(),
        channel: "main".into(),
        last_manifest_generated_at: Some("2026-07-01T00:00:00Z".into()),
        ..Default::default()
    };
    state.managed.insert("mods/a.jar".into(), managed(a, "a"));
    state.managed.insert(
        "mods/create-6.0.10.jar".into(),
        managed(create_old, "create"),
    );
    state
        .managed
        .insert("mods/stale.jar".into(), managed(stale, "stale"));

    // ---- what the client scanned on disk right now ----
    let scanned = vec![
        local("mods/a.jar", a, "a"),
        local("mods/create-6.0.10.jar", create_old, "create"),
        local("mods/personal.jar", personal, "personal"),
        local("mods/stale.jar", stale, "stale"),
    ];

    // ---- plan ----
    let plan = compute_plan(&manifest, &scanned, &state);
    let dl_paths: Vec<&str> = plan
        .downloads
        .iter()
        .map(|d| d.entry.path.as_str())
        .collect();
    assert!(dl_paths.contains(&"mods/create-6.0.11.jar"));
    assert!(dl_paths.contains(&"mods/newmod.jar"));
    let create_dl = plan
        .downloads
        .iter()
        .find(|d| d.entry.path == "mods/create-6.0.11.jar")
        .unwrap();
    assert_eq!(
        create_dl.replaces.as_deref(),
        Some("mods/create-6.0.10.jar")
    );
    assert_eq!(plan.removals, vec!["mods/stale.jar".to_string()]);
    assert_eq!(plan.user_extras, vec!["mods/personal.jar".to_string()]);
    assert!(plan.collisions.is_empty());

    // ---- finalize (default: keep the personal mod) + apply ----
    let exec = finalize(&plan, &Decisions::default());
    let fetcher = FileFetcher { root: chan.clone() };
    let report = apply(&inst, &manifest, &exec, &fetcher, "", &state, "test-ts").unwrap();

    // ---- filesystem assertions ----
    assert_eq!(
        std::fs::read(inst.join("mods/a.jar")).unwrap(),
        a,
        "a untouched"
    );
    assert_eq!(
        std::fs::read(inst.join("mods/create-6.0.11.jar")).unwrap(),
        create_new,
        "new create installed"
    );
    assert!(
        !inst.join("mods/create-6.0.10.jar").exists(),
        "old create removed"
    );
    assert_eq!(
        std::fs::read(inst.join("mods/newmod.jar")).unwrap(),
        newmod,
        "new mod installed"
    );
    assert_eq!(
        std::fs::read(inst.join("mods/personal.jar")).unwrap(),
        personal,
        "USER MOD MUST BE UNTOUCHED"
    );
    assert!(!inst.join("mods/stale.jar").exists(), "stale removed");

    // ---- backup contains what we removed/replaced ----
    let backup = inst.join(".bonegrader-backup/test-ts");
    assert_eq!(
        std::fs::read(backup.join("mods/create-6.0.10.jar")).unwrap(),
        create_old
    );
    assert_eq!(std::fs::read(backup.join("mods/stale.jar")).unwrap(), stale);

    // ---- report + new state ----
    assert_eq!(report.downloaded, 2);
    assert_eq!(report.deleted, 1);
    let keys: Vec<&String> = report.new_state.managed.keys().collect();
    assert_eq!(
        keys,
        vec![
            &"mods/a.jar".to_string(),
            &"mods/create-6.0.11.jar".to_string(),
            &"mods/newmod.jar".to_string(),
        ],
        "managed set = manifest files present; personal stays unmanaged"
    );
    assert_eq!(
        report.new_state.last_manifest_generated_at.as_deref(),
        Some("2026-07-26T20:00:00Z")
    );

    // ---- second run is a clean no-op ----
    let scanned2 = vec![
        local("mods/a.jar", a, "a"),
        local("mods/create-6.0.11.jar", create_new, "create"),
        local("mods/newmod.jar", newmod, "newmod"),
        local("mods/personal.jar", personal, "personal"),
    ];
    let plan2 = compute_plan(&manifest, &scanned2, &report.new_state);
    assert!(plan2.is_noop(), "second run should be a no-op: {plan2:?}");
    assert_eq!(
        plan2.user_extras,
        vec!["mods/personal.jar".to_string()],
        "personal still recognised as a kept extra"
    );

    // cleanup
    let _ = std::fs::remove_dir_all(&inst);
    let _ = std::fs::remove_dir_all(&chan);
}

#[test]
fn progress_events_are_emitted_in_order() {
    let a = b"AAA";
    let b = b"BBBB";
    let inst = unique_tmp("inst-prog");
    std::fs::create_dir_all(inst.join("mods")).unwrap();
    let chan = unique_tmp("chan-prog");
    for c in [a.as_slice(), b.as_slice()] {
        write(&chan.join(format!("files/by-hash/{}", sha1_bytes(c))), c);
    }
    let manifest = Manifest {
        schema_version: 1,
        pack_name: "T".into(),
        channel: "main".into(),
        generated_at: "t".into(),
        loader: Loader {
            loader_type: "neoforge".into(),
            mc_version: "1.21.1".into(),
            loader_version: "21.1.234".into(),
        },
        files: vec![
            mod_entry("mods/a.jar", a, "a"),
            mod_entry("mods/b.jar", b, "b"),
        ],
    };
    let state = ClientState::default(); // empty instance -> two fresh installs
    let plan = compute_plan(&manifest, &[], &state);
    let exec = finalize(&plan, &Decisions::default());

    let events = std::cell::RefCell::new(Vec::<Progress>::new());
    let fetcher = FileFetcher { root: chan.clone() };
    let report = apply_with_progress(&inst, &manifest, &exec, &fetcher, "", &state, "ts", &|p| {
        events.borrow_mut().push(p.clone())
    })
    .unwrap();

    let ev = events.into_inner();
    assert_eq!(ev.first().unwrap().phase, "download");
    assert!(ev.iter().any(|e| e.phase == "apply"));
    let last = ev.last().unwrap();
    assert_eq!(last.phase, "done");
    assert_eq!(last.done_files, 2);
    assert_eq!(last.total_files, 2);
    assert_eq!(last.total_bytes, (a.len() + b.len()) as u64);
    assert_eq!(last.done_bytes, last.total_bytes);
    // done_bytes must be monotonically non-decreasing across the run.
    let mut prev = 0;
    for e in &ev {
        assert!(e.done_bytes >= prev);
        prev = e.done_bytes;
    }
    assert_eq!(report.downloaded, 2);

    let _ = std::fs::remove_dir_all(&inst);
    let _ = std::fs::remove_dir_all(&chan);
}
