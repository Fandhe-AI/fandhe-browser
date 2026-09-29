//! `Profile::delete` の公開 API を通した受入基準確認（TASK-53（53.2）・#188、
//! ビヘイビア `PROF-5`、MS-3）。
//!
//! `PROF-5` は「プロファイルを削除するとロックが解放され、ルートディレクトリ
//! ごと全データ（Cookie・KV・キャッシュ・履歴）が消える」ことを定める。
//! Windows では `Profile::open` が `Unsupported` を返すため Unix 専用とする
//! （Windows 側の契約は `src/profile.rs` の `#[cfg(windows)]` テストが担う）。

#![cfg(unix)]

use fandhe_browser_profile::{DataKind, Profile, ProfileError};
use std::ffi::OsStr;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（外部依存を増やさない）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS の `/var` symlink を `Profile::open` の厳格検証が弾くため正規化する。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-delete-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `path` が存在しない（symlink も含め辿らずに `NotFound`）ことを確認する。
fn assert_not_found(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Err(e) => assert_eq!(e.kind(), ErrorKind::NotFound, "{path:?}: {e}"),
        Ok(_) => panic!("{path:?} が残っている"),
    }
}

fn write_data(profile: &Profile, kind: DataKind, name: &str, body: &[u8]) {
    let mut f = profile
        .create_file_in(kind, OsStr::new(name))
        .expect("create_file_in");
    f.write_all(body).expect("write");
}

#[test]
fn prof_5_delete_removes_root_directory_with_all_data() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");
    let profile = Profile::open(&root).expect("open");
    write_data(&profile, DataKind::Cookies, "c.db", b"cookie");
    write_data(&profile, DataKind::Storage, "kv.db", b"kv");
    write_data(&profile, DataKind::Cache, "blob", b"cache");
    write_data(&profile, DataKind::History, "h.db", b"history");
    let nested = root.join("cache").join("a").join("b");
    std::fs::create_dir_all(&nested).expect("nested dir");
    std::fs::write(nested.join("deep.bin"), b"deep").expect("nested file");

    profile.delete().expect("delete");

    assert_not_found(&root);
}

#[test]
fn prof_5_delete_releases_lock_so_root_can_be_reopened() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");
    let profile = Profile::open(&root).expect("open");
    write_data(&profile, DataKind::Cookies, "c.db", b"cookie");

    profile.delete().expect("delete");

    let reopened = Profile::open(&root).expect("reopen after delete");
    for &kind in DataKind::ALL {
        let entries = std::fs::read_dir(reopened.data_dir(kind))
            .expect("read_dir")
            .count();
        assert_eq!(entries, 0, "{kind:?} は空で作り直される");
    }
}

#[test]
fn prof_5_delete_does_not_follow_symlink_inside_profile() {
    let tmp = TempDir::new();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::fs::write(outside.join("keep.txt"), b"precious").expect("outside file");

    let root = tmp.path().join("profile");
    let profile = Profile::open(&root).expect("open");
    std::os::unix::fs::symlink(&outside, root.join("cache").join("link")).expect("symlink");

    profile.delete().expect("delete");

    assert_not_found(&root);
    assert_eq!(
        std::fs::read(outside.join("keep.txt")).expect("outside file survives"),
        b"precious"
    );
}

#[test]
fn prof_5_delete_refuses_when_root_path_was_replaced() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");
    let moved = tmp.path().join("moved");
    let profile = Profile::open(&root).expect("open");
    write_data(&profile, DataKind::Cookies, "c.db", b"orig");

    std::fs::rename(&root, &moved).expect("rename");
    std::fs::create_dir_all(&root).expect("replacement dir");
    std::fs::write(root.join("other.txt"), b"other").expect("replacement file");

    let err = profile.delete().expect_err("must refuse");
    assert!(
        matches!(err, ProfileError::InvalidLayout { .. }),
        "got {err:?}"
    );
    assert_eq!(
        std::fs::read(moved.join("cookies").join("c.db")).expect("orig survives"),
        b"orig"
    );
    assert_eq!(
        std::fs::read(root.join("other.txt")).expect("replacement survives"),
        b"other"
    );
}

#[test]
fn prof_5_delete_leaves_sibling_profile_untouched() {
    let tmp = TempDir::new();
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    let a = Profile::open(&root_a).expect("open a");
    let b = Profile::open(&root_b).expect("open b");
    write_data(&a, DataKind::Cookies, "c.db", b"a-data");
    write_data(&b, DataKind::Cookies, "c.db", b"b-data");

    a.delete().expect("delete a");

    assert_not_found(&root_a);
    assert_eq!(
        std::fs::read(root_b.join("cookies").join("c.db")).expect("b survives"),
        b"b-data"
    );
    drop(b);
}
