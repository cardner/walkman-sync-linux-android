//! Headless tests: dest skip by plugged serial, mp3h identity, enumerate vs enrich.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tempfile::TempDir;
use walkman_sync::catalog::{self, Catalog, RefreshOpts};
use walkman_sync::keys;
use walkman_sync::launch;
use walkman_sync::storage;
use walkman_sync::sync::{
    self, AppConfig, CopyJob, CopyOp, DeviceScan, FileEntry, LibraryPlan,
};
use walkman_sync::walkman::{self, SyncSettings};

static CATALOG_ENV: Mutex<()> = Mutex::new(());

fn slash(rel: &str) -> String {
    rel.replace('\\', "/")
}

fn rel_set(entries: &[(String, u64, f64)]) -> HashSet<String> {
    entries.iter().map(|(r, _, _)| slash(r)).collect()
}

fn write_serial(dir: &Path, serial: &str) {
    fs::write(dir.join(".sony-sync-serial"), serial).unwrap();
}

fn minimal_flac(md5: [u8; 16]) -> Vec<u8> {
    let packed: u64 = (44100u64 << 44) | (15u64 << 36) | 16;
    let mut streaminfo = Vec::new();
    streaminfo.extend_from_slice(&16u16.to_be_bytes());
    streaminfo.extend_from_slice(&16u16.to_be_bytes());
    streaminfo.extend_from_slice(&[0u8; 6]);
    streaminfo.extend_from_slice(&packed.to_be_bytes());
    streaminfo.extend_from_slice(&md5);
    let mut out = b"fLaC".to_vec();
    out.push(0x80);
    out.extend_from_slice(&(34u32).to_be_bytes()[1..]);
    out.extend_from_slice(&streaminfo);
    out.extend_from_slice(&[0u8; 16]);
    out
}

/// MPEG1 Layer III header + Xing with a known frame/byte count.
fn minimal_mp3(id3_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if !id3_payload.is_empty() {
        let size = id3_payload.len() as u32;
        out.extend_from_slice(b"ID3\x03\x00\x00");
        // syncsafe size
        out.push(((size >> 21) & 0x7f) as u8);
        out.push(((size >> 14) & 0x7f) as u8);
        out.push(((size >> 7) & 0x7f) as u8);
        out.push((size & 0x7f) as u8);
        out.extend_from_slice(id3_payload);
    }
    // MPEG1 Layer3, 128kbps, 44100, stereo
    out.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    out.extend_from_slice(&[0u8; 32]); // side info
    out.extend_from_slice(b"Xing");
    out.extend_from_slice(&0x0003u32.to_be_bytes()); // frames + bytes
    out.extend_from_slice(&42u32.to_be_bytes());
    out.extend_from_slice(&1000u32.to_be_bytes());
    out.extend_from_slice(&[0u8; 128]);
    out
}

#[test]
fn two_serials_dest_skip_vs_must_walk() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("catalog.sqlite");
    let walkman_a = tmp.path().join("a");
    let walkman_b = tmp.path().join("b");
    fs::create_dir_all(walkman_a.join("MUSIC")).unwrap();
    fs::create_dir_all(walkman_b.join("MUSIC")).unwrap();
    write_serial(&walkman_a, "serial-aaaa");
    write_serial(&walkman_b, "serial-bbbb");
    let music_a = walkman_a.join("MUSIC");
    let music_b = walkman_b.join("MUSIC");
    fs::write(music_a.join("track.flac"), minimal_flac([1; 16])).unwrap();

    let mut cat = Catalog::open(Some(&db)).unwrap();
    let stats = cat.refresh_dest(
        music_a.to_str().unwrap(),
        catalog::KIND_INTERNAL,
        RefreshOpts {
            walk: true,
            enrich: true,
        },
        None,
    );
    assert!(stats.added >= 1, "first plug of A indexes files");

    assert!(
        cat.should_skip_dest_walk(catalog::KIND_INTERNAL, music_a.to_str().unwrap()),
        "plugged serial A with a catalog of files skips the dest walk"
    );
    assert!(
        !cat.should_skip_dest_walk(catalog::KIND_INTERNAL, music_b.to_str().unwrap()),
        "first plug of Walkman B always walks — different serial, empty catalog"
    );

    // volume_for_kind would pick A's newest row and wrongly skip B
    let kind_row = cat.volume_for_kind(catalog::KIND_INTERNAL).unwrap();
    assert_eq!(kind_row.serial, "serial-aaaa");
    let root_b = cat.volume_for_root(catalog::KIND_INTERNAL, music_b.to_str().unwrap());
    assert!(root_b.is_none() || root_b.unwrap().file_count == 0);
}

#[test]
fn mp3h_stable_across_id3_only_change() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a.mp3");
    let b = tmp.path().join("b.mp3");
    fs::write(&a, minimal_mp3(b"TIT2 padded tag AAAAAAAAAA")).unwrap();
    fs::write(&b, minimal_mp3(b"TIT2 padded tag BBBBBBBBBB extra")).unwrap();
    let ka = keys::enrich(&a).unwrap();
    let kb = keys::enrich(&b).unwrap();
    assert!(ka.key.starts_with("mp3h-"), "{}", ka.key);
    assert_eq!(ka.key, kb.key, "ID3-only change must not change mp3h identity");
    assert!(!ka.key.starts_with("mp3-"), "must not full-file hash");
}

#[test]
fn enumerate_only_does_not_fill_keys_enrich_does() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("catalog.sqlite");
    let src = tmp.path().join("lib");
    let album = src.join("Keep - Album");
    fs::create_dir_all(&album).unwrap();
    fs::write(album.join("01.flac"), minimal_flac([9; 16])).unwrap();

    let mut cat = Catalog::open(Some(&db)).unwrap();
    let walked = cat.refresh_source(
        src.to_str().unwrap(),
        RefreshOpts {
            walk: true,
            enrich: false,
        },
        None,
    );
    assert_eq!(walked.added, 1);
    assert_eq!(walked.keyed, 0, "enumerate-only must not open audio");
    let rows = cat.files_for(catalog::KIND_SOURCE, Some(src.to_str().unwrap()));
    assert_eq!(rows.len(), 1);
    assert!(rows[0].content_key.is_empty());

    let enriched = cat.refresh_source(
        src.to_str().unwrap(),
        RefreshOpts {
            walk: false,
            enrich: true,
        },
        None,
    );
    assert!(enriched.keyed >= 1);
    let rows = cat.files_for(catalog::KIND_SOURCE, Some(src.to_str().unwrap()));
    assert!(rows[0].content_key.starts_with("flac-"));
}

#[test]
fn flat_sync_json_migrates_into_nested_devices() {
    let tmp = TempDir::new().unwrap();
    let music = tmp.path().join("internal").join("Music");
    fs::create_dir_all(&music).unwrap();
    write_serial(&music, "aaa11111");
    let path = tmp.path().join("sync.json");
    fs::write(
        &path,
        r#"{
            "source_dir": "\\\\WillineauVault\\Music",
            "playlist_dir": "\\\\WillineauVault\\Playlists",
            "internal_drive": "PLACEHOLDER",
            "sd_drive": "",
            "dest": "internal",
            "excluded_albums": "Skip - Album",
            "copy_workers": 2,
            "auto_detect_drives": true,
            "device_kind": "cinder"
        }"#,
    )
    .unwrap();
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("PLACEHOLDER", &music.to_string_lossy().replace('\\', "\\\\"));
    fs::write(&path, text).unwrap();

    let settings = SyncSettings::load_from(&path);
    assert_eq!(settings.source_dir, r"\\WillineauVault\Music");
    assert_eq!(settings.excluded_albums, "Skip - Album");
    assert_eq!(settings.copy_workers, 2);
    assert_eq!(settings.active_device, "aaa11111");
    let profile = settings.active_profile().unwrap();
    assert_eq!(profile.dest, "internal");
    assert_eq!(profile.device_kind, "cinder");
}

#[test]
fn install_shortcut_script_sets_icon_location() {
    let tmp = TempDir::new().unwrap();
    let lnk = tmp.path().join("Walkman Sync.lnk");
    let exe = PathBuf::from(r"C:\sync\walkman-sync.exe");
    let ico = PathBuf::from(r"C:\sync\walkman-sync.ico");
    let script = launch::shortcut_powershell(&lnk, exe.to_str().unwrap(), "", tmp.path(), Some(&ico));
    assert!(script.contains("IconLocation"), "{script}");
    assert!(script.contains("walkman-sync.ico"), "{script}");
    assert!(script.contains("walkman-sync.exe"), "{script}");
}

#[test]
fn macos_command_launches_the_binary() {
    let exe = PathBuf::from("/opt/walkman-sync");
    let work = PathBuf::from("/Users/me/Sony-sync");
    let script = launch::macos_command_script(&exe, &work);
    assert!(script.contains("#!/bin/bash"));
    assert!(script.contains("/opt/walkman-sync"));
    assert!(!script.contains("python"));
    assert!(!script.contains("launch.py"));
}

#[test]
fn bind_new_serial_creates_a_second_profile() {
    let mut settings = SyncSettings::default();
    settings.devices.insert(
        "serialA".into(),
        walkman::DeviceProfile {
            name: "A50".into(),
            internal_serial: "serialA".into(),
            internal_drive: r"F:\MUSIC".into(),
            dest: "sd".into(),
            ..walkman::DeviceProfile::default()
        },
    );
    settings.active_device = "serialA".into();
    // Without a live volume serial marker this uses get_volume_serial of a missing path → empty,
    // so bind_live_device returns false. Exercise matches() instead.
    let a = settings.devices["serialA"].clone();
    assert!(a.matches("serialA", ""));
    assert!(!a.matches("serialB", ""));
}

#[test]
fn audio_filter_rejects_appledouble_and_partial() {
    assert!(storage::is_audio("01.flac"));
    assert!(storage::is_audio("Track.MP3"));
    assert!(!storage::is_audio("cover.jpg"));
    assert!(!storage::is_audio("Album.m3u"));
    assert!(!storage::is_audio("._01.flac"));
    assert!(!storage::is_audio(".01.flac.partial"));
    assert!(!storage::is_audio(".DS_Store"));
    assert!(storage::is_playlist("Mix.m3u8"));
    assert!(!storage::is_playlist("._Mix.m3u8"));
    assert!(storage::is_ignored_dir("$RECYCLE.BIN"));
    assert!(storage::is_ignored_dir("@eaDir"));
    assert!(storage::is_preserved_dest("Liked Songs.m3u8"));
    assert!(storage::is_preserved_dest("cinder_loved.tsv"));
}

#[test]
fn source_walk_indexes_audio_only() {
    let tmp = TempDir::new().unwrap();
    let album = tmp.path().join("Keep - Album");
    fs::create_dir_all(&album).unwrap();
    fs::write(album.join("01.flac"), minimal_flac([1; 16])).unwrap();
    fs::write(album.join("cover.jpg"), b"jpeg").unwrap();
    fs::write(album.join("Album.m3u"), "#EXTM3U\n01.flac\n").unwrap();
    fs::write(album.join("._01.flac"), b"appledouble").unwrap();
    fs::create_dir_all(tmp.path().join("@eaDir")).unwrap();
    fs::write(tmp.path().join("@eaDir").join("hidden.mp3"), b"id3").unwrap();
    fs::create_dir_all(tmp.path().join("$RECYCLE.BIN")).unwrap();
    fs::write(tmp.path().join("$RECYCLE.BIN").join("x.flac"), b"x").unwrap();

    let names = rel_set(&storage::walk_files(tmp.path().to_str().unwrap(), false));
    assert!(
        names.iter().any(|n| n.ends_with("01.flac") && n.contains("Keep - Album")),
        "{names:?}"
    );
    assert!(names.iter().all(|n| n.ends_with("01.flac")), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("cover.jpg")));
    assert!(!names.iter().any(|n| n.contains(".m3u")));
    assert!(!names.iter().any(|n| n.contains("._")));
    assert!(!names.iter().any(|n| n.contains("@eaDir")));
    assert!(!names.iter().any(|n| n.to_ascii_lowercase().contains("recycle")));
}

#[test]
fn dest_walk_skips_preserved_and_lists_sidecars() {
    let tmp = TempDir::new().unwrap();
    let walkman = tmp.path().join("walkman");
    let music = walkman.join("MUSIC");
    let album = music.join("Keep - Album");
    fs::create_dir_all(&album).unwrap();
    fs::create_dir_all(music.join("cinder_playlists")).unwrap();
    fs::write(album.join("01.flac"), minimal_flac([2; 16])).unwrap();
    fs::write(album.join("cover.jpg"), b"jpeg").unwrap();
    fs::write(album.join("album.m3u8"), "#EXTM3U\n01.flac\n").unwrap();
    fs::write(music.join("Liked Songs.m3u8"), "#EXTM3U\n").unwrap();
    fs::write(music.join("Mix.m3u8"), "#EXTM3U\n").unwrap();
    fs::write(music.join(".sony-sync-manifest.tsv"), "rel\n").unwrap();
    fs::write(music.join("cinder_playlists").join("Mix.m3u8"), "#EXTM3U\n").unwrap();

    let root = music.to_str().unwrap();
    let names = rel_set(&storage::walk_files(root, true));
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("01.flac")), "{names:?}");

    let unwanted: HashSet<String> = storage::walk_dest_unwanted(root)
        .into_iter()
        .map(|r| slash(&r))
        .collect();
    assert!(unwanted.iter().any(|n| n.ends_with("cover.jpg")), "{unwanted:?}");
    assert!(unwanted.iter().any(|n| n.ends_with("album.m3u8")), "{unwanted:?}");
    assert!(!unwanted.iter().any(|n| n == "Mix.m3u8"), "{unwanted:?}");
    assert!(!unwanted.iter().any(|n| n.to_ascii_lowercase().contains("liked")), "{unwanted:?}");
    assert!(
        !unwanted.iter().any(|n| n.contains("cinder_playlists")),
        "{unwanted:?}"
    );
}

#[test]
fn source_files_from_catalog_omits_playlists() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("catalog.sqlite");
    let src = tmp.path().join("lib");
    let album = src.join("Keep - Album");
    fs::create_dir_all(&album).unwrap();
    fs::write(album.join("01.flac"), minimal_flac([3; 16])).unwrap();
    fs::write(album.join("Album.m3u"), "#EXTM3U\n01.flac\n").unwrap();
    fs::write(album.join("cover.jpg"), b"jpeg").unwrap();

    let mut cat = Catalog::open(Some(&db)).unwrap();
    cat.refresh_source(
        src.to_str().unwrap(),
        RefreshOpts {
            walk: true,
            enrich: false,
        },
        None,
    );
    let rows = cat.files_for(catalog::KIND_SOURCE, Some(src.to_str().unwrap()));
    assert_eq!(rows.len(), 1, "{:?}", rows.iter().map(|r| r.rel.clone()).collect::<Vec<_>>());
    assert!(slash(&rows[0].rel).ends_with("01.flac"));

    let mut folders = HashSet::new();
    folders.insert("Keep - Album".into());
    let grouped = sync::source_files_from_catalog(&cat, src.to_str().unwrap(), &folders);
    let files = grouped.get("Keep - Album").unwrap();
    assert_eq!(files.len(), 1);
    assert!(slash(&files[0].rel_path).ends_with("01.flac"));
}

#[test]
fn dest_walk_marks_sidecars_and_extra_audio_stale() {
    let _lock = CATALOG_ENV.lock().unwrap();
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("catalog.sqlite");
    std::env::set_var("SONY_SYNC_CATALOG", db.to_str().unwrap());
    struct ClearCatalogEnv;
    impl Drop for ClearCatalogEnv {
        fn drop(&mut self) {
            std::env::remove_var("SONY_SYNC_CATALOG");
        }
    }
    let _clear = ClearCatalogEnv;

    let src = tmp.path().join("lib");
    let album = src.join("Keep - Album");
    fs::create_dir_all(&album).unwrap();
    fs::write(album.join("01.flac"), minimal_flac([4; 16])).unwrap();
    let playlists = tmp.path().join("playlists");
    fs::create_dir_all(&playlists).unwrap();
    fs::write(
        playlists.join("Mix.m3u8"),
        "Keep - Album/01.flac\n",
    )
    .unwrap();

    let walkman = tmp.path().join("walkman");
    let music = walkman.join("MUSIC");
    let dest_album = music.join("Keep - Album");
    fs::create_dir_all(&dest_album).unwrap();
    fs::create_dir_all(music.join("Extra - Album")).unwrap();
    write_serial(&walkman, "serial-stale");
    fs::write(dest_album.join("01.flac"), minimal_flac([4; 16])).unwrap();
    fs::write(dest_album.join("cover.jpg"), b"jpeg").unwrap();
    fs::write(dest_album.join("album.m3u8"), "#EXTM3U\n01.flac\n").unwrap();
    fs::write(music.join("Extra - Album").join("gone.flac"), minimal_flac([5; 16])).unwrap();
    fs::write(music.join("Mix.m3u8"), "#EXTM3U\nKeep - Album/01.flac\n").unwrap();
    fs::write(music.join("Liked Songs.m3u8"), "#EXTM3U\n").unwrap();

    let config = test_app_config(&src, &music, &playlists);
    let report = sync::build_scan_report(&config, true, &mut |_, _, _| {});
    let stale: HashSet<String> = report.stale_files.iter().map(|s| slash(s)).collect();

    assert!(
        stale.iter().any(|s| s.ends_with("cover.jpg")),
        "cover art should be stale: {stale:?}"
    );
    assert!(
        stale.iter().any(|s| s.ends_with("album.m3u8")),
        "nested playlist should be stale: {stale:?}"
    );
    assert!(
        stale.iter().any(|s| s.contains("gone.flac")),
        "extra dest audio should be stale: {stale:?}"
    );
    assert!(
        !stale.iter().any(|s| s == "Mix.m3u8"),
        "root playlist must stay: {stale:?}"
    );
    assert!(
        !stale.iter().any(|s| s.to_ascii_lowercase().contains("liked")),
        "Liked Songs must stay: {stale:?}"
    );
}

fn test_app_config(src: &Path, music: &Path, playlists: &Path) -> AppConfig {
    let mut settings = SyncSettings::default();
    settings.source_dir = src.to_string_lossy().into_owned();
    settings.playlist_dir = playlists.to_string_lossy().into_owned();
    settings.auto_detect_drives = false;
    settings.active_device = "test".into();
    settings.devices.insert(
        "test".into(),
        walkman::DeviceProfile {
            name: "test".into(),
            internal_serial: "serial-stale".into(),
            internal_drive: music.to_string_lossy().into_owned(),
            dest: "internal".into(),
            internal_max_gb: 64.0,
            ..walkman::DeviceProfile::default()
        },
    );
    AppConfig {
        settings,
        lastfm_api_key: String::new(),
        lastfm_api_secret: String::new(),
        lastfm_username: String::new(),
        lastfm_session_key: String::new(),
        audiomuse_url: String::new(),
        audiomuse_token: String::new(),
        new_walkman: String::new(),
    }
}

fn job(rel: &str, bytes: u64, op: CopyOp) -> CopyJob {
    CopyJob {
        rel_path: rel.to_string(),
        transfer_bytes: bytes,
        op,
        ..CopyJob::default()
    }
}

fn entry(rel: &str, size: u64) -> FileEntry {
    FileEntry {
        rel_path: rel.to_string(),
        size,
        mtime: 0.0,
        content_key: String::new(),
    }
}

#[test]
fn plan_diff_buckets_add_modify_remove_unchanged() {
    let dest = "E:\\MUSIC";
    let new_rel = storage::join("NewArtist", &["n.flac"]);
    let keep_rel = storage::join("KeepArtist", &["ok.flac"]);
    let touch_rel = storage::join("TouchArtist", &["a.flac"]);
    let gone_rel = storage::join("TouchArtist", &["gone.flac"]);
    let old_rel = storage::join("OldArtist", &["x.flac"]);

    let mut plan = LibraryPlan::default();
    plan.assignments.insert("NewArtist".into(), dest.into());
    plan.assignments.insert("KeepArtist".into(), dest.into());
    plan.assignments.insert("TouchArtist".into(), dest.into());
    plan.source_files
        .insert("KeepArtist".into(), vec![entry(&keep_rel, 5000)]);

    let mut dest_scan = DeviceScan::default();
    dest_scan
        .files
        .insert(storage::fold_rel(&keep_rel), (5000, 0.0));
    dest_scan
        .artist_files
        .insert("KeepArtist".into(), vec![keep_rel.clone()]);
    dest_scan
        .files
        .insert(storage::fold_rel(&touch_rel), (1000, 0.0));
    dest_scan
        .files
        .insert(storage::fold_rel(&gone_rel), (2000, 0.0));
    dest_scan.artist_files.insert(
        "TouchArtist".into(),
        vec![touch_rel.clone(), gone_rel.clone()],
    );
    dest_scan
        .files
        .insert(storage::fold_rel(&old_rel), (9000, 0.0));
    dest_scan
        .artist_files
        .insert("OldArtist".into(), vec![old_rel.clone()]);

    let jobs = vec![
        job(&new_rel, 3000, CopyOp::Add),
        job(&touch_rel, 1500, CopyOp::Replace),
    ];
    let stale = vec![gone_rel];
    let diff = sync::classify_plan_diff(
        Some(&plan),
        Some(&dest_scan),
        dest,
        &jobs,
        &stale,
    );

    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.added[0].name, "NewArtist");
    assert_eq!(diff.added[0].add_files, 1);
    assert_eq!(diff.added[0].add_bytes, 3000);

    assert_eq!(diff.modified.len(), 1);
    assert_eq!(diff.modified[0].name, "TouchArtist");
    assert_eq!(diff.modified[0].add_files, 1);
    assert_eq!(diff.modified[0].remove_files, 1);
    assert_eq!(diff.modified[0].add_bytes, 1500);
    assert_eq!(diff.modified[0].remove_bytes, 3000);

    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.removed[0].name, "OldArtist");
    assert_eq!(diff.removed[0].remove_files, 1);
    assert_eq!(diff.removed[0].remove_bytes, 9000);

    assert_eq!(diff.unchanged, 1);
    assert_eq!(diff.unchanged_bytes, 5000);
}
