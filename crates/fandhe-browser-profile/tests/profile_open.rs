//! `Profile::open` の公開 API を通した受入基準確認（TASK-50（50.1）・#176・
//! ビヘイビア `PROF-1`）。
//!
//! パーミッション・隔離の網羅的な確認は TASK-50（50.4）・#179 が本ファイルへ
//! 追加する前提のため、本ファイルはそのまま拡張できるよう別ファイルへ
//! 分割しない。
//!
//! Windows では ACL 隔離が未実装のため `Profile::open` は常に
//! `ProfileError::Unsupported` を返す（`XOS-7`〜`XOS-10`。PR #437 レビュー
//! 指摘への対応）。本ファイルの受入テストは実際にディレクトリが作られる
//! ことを検証する内容のため Unix 専用とし、Windows 側の契約は
//! `src/profile.rs` の `#[cfg(windows)]` テストで確認する。

#![cfg(unix)]

use fandhe_browser_profile::{DataKind, Profile};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（tempfile 等の外部
/// 依存は追加しない方針。dependency-policy.md）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // `std::env::temp_dir()` を `canonicalize` した基点から組み立てる。
        // macOS では `/var` が `/private/var` への OS 標準 symlink であり、
        // 正規化しないと `Profile::open` の厳格な symlink 検証（PR #437
        // レビュー指摘）が「意図しない既存の symlink」として弾いてしまう。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-open-test-{}-{n}",
            std::process::id()
        ));
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

/// 受入基準 1（PROF-1）: 存在しない入れ子のパスへ `open` すると、途中の
/// 階層を含めてルートディレクトリが作られる。
#[test]
fn prof_1_open_creates_nested_root_when_missing() {
    let tmp = TempDir::new();
    let root = tmp.path().join("a").join("b").join("profile");
    assert!(!root.exists());

    let profile = Profile::open(&root).expect("open は成功する");

    assert!(root.is_dir());
    assert_eq!(profile.root(), root.as_path());
}

/// 受入基準 2（PROF-1）: ルート直下に 4 つのデータ種別ディレクトリが
/// すべて作られ、いずれもディレクトリである。
#[test]
fn prof_1_open_creates_all_data_subdirectories() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    let profile = Profile::open(&root).expect("open は成功する");

    for kind in DataKind::ALL.iter().copied() {
        let dir = profile.data_dir(kind);
        assert!(
            dir.is_dir(),
            "{:?} 用ディレクトリが存在しない: {dir:?}",
            kind
        );
    }
}

/// PROF-1: 一度 `Profile` を drop してから同じルートへ再度 `open` しても
/// 成功する（#178 でロックが入っても、2 つのハンドルを同時に持たない本
/// テストの形は壊れない前提）。
#[test]
fn prof_1_reopen_after_drop_succeeds() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    let profile = Profile::open(&root).expect("初回 open は成功する");
    drop(profile);

    let reopened = Profile::open(&root).expect("再 open も成功する");
    assert_eq!(reopened.root(), root.as_path());
    for kind in DataKind::ALL.iter().copied() {
        assert!(reopened.data_dir(kind).is_dir());
    }
}
