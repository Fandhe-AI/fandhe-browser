//! Windows 長パス対応（`XOS-8`）が `LongPathsEnabled` レジストリ設定に依存しない
//! ことの結合テスト（TASK-61（61.2）・#206、MS-3）。
//!
//! `to_long_path`（TASK-61（61.1）・#205）と、それを `open_or_create` が呼ぶ経路を
//! 公開 API だけで確認する。`src/store.rs` の単体テストは変換規則の純関数テストと
//! 往復 1 件に留まるため、本ファイルは境界長（MAX_PATH 前後）・複数のファイル操作・
//! store API を 3 OS で固定する。
//!
//! # 「LongPathsEnabled 無効と同じ条件」の定義
//!
//! レジストリの切り替えは管理者権限を要するグローバル操作のため、テストでは行わない
//! （`windows-sys` 等の依存追加・`unsafe` も使わない）。Microsoft の
//! "Maximum Path Length Limitation" によると、`\\?\` なしで長パスを使うには
//! レジストリ `LongPathsEnabled` と、アプリケーションマニフェストの `longPathAware` の
//! 両方が必要である。`cargo test` のテストバイナリには `longPathAware` マニフェストが
//! ないため、ランナーのレジストリ値がどうであれ「無効と同じ」状態で動く。その状態で、
//! 最初から `\\?\` 付きの `to_long_path` の出力だけを使って 260 文字超のパスを
//! 操作できることを確認する。
//!
//! # 限界
//!
//! 「プレフィックスなしだと失敗する」対照実験は `std::fs` では観測できない（std が
//! 長い絶対パスを内部で verbatim 化するため）。本テストは std の暗黙変換に頼らず
//! 成立することだけを示し、それ以上は主張しない。レジストリ切替の E2E と
//! `longPathAware` マニフェスト比較は TASK-63（実機測定）の範囲。
//!
//! Windows では ACL 隔離が未実装（`XOS-7`）のため、`open_or_create` は変換成功後に
//! `Unsupported` を返す。成功扱いにはしない。

use fandhe_browser_profile::{ExplicitStore, LongPathKind, ProfileStore, to_long_path};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// パスの UTF-16 単位数（Windows の MAX_PATH 判定と同じ単位。3 OS 共通で測る）。
fn units(path: &Path) -> usize {
    path.as_os_str().to_string_lossy().encode_utf16().count()
}

/// 一意な一時ルート。drop 時に `to_long_path` 経由で再帰削除する（外部依存は使わない）。
struct Guard {
    root: PathBuf,
}

impl Guard {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS の `/var` symlink を避けるため canonicalize する。ただし Windows では
        // canonicalize が `\\?\` 付きを返し「無変換の入力」でなくなるため行わない。
        let base = std::env::temp_dir();
        #[cfg(not(windows))]
        let base = base.canonicalize().unwrap_or(base);
        Self {
            root: base.join(format!("fandhe-longpath-test-{}-{n}", std::process::id())),
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(long) = to_long_path(&self.root) {
            let _ = std::fs::remove_dir_all(long.path());
        }
    }
}

/// `base` の配下に、全体の UTF-16 単位数がちょうど `target` になる入れ子パスを組み立てる。
/// 各要素は 50 単位以下（NAME_MAX 255 を大きく下回る）。作成はしない。
fn path_with_units(base: &Path, target: usize) -> PathBuf {
    let mut path = base.to_path_buf();
    loop {
        let cur = units(&path);
        assert!(target > cur, "target {target} is unreachable from {cur}");
        let need = target - cur;
        assert!(need >= 2, "cannot add a component of {need} units");
        let mut chunk = (need - 1).min(50);
        // 残りが 1 単位になると次の要素を作れないため手前で調整する。
        if need - 1 - chunk == 1 {
            chunk -= 1;
        }
        path.push("d".repeat(chunk));
        if units(&path) == target {
            return path;
        }
    }
}

/// XOS-8: 260 文字超の絶対パスに対し、Unix では変換が no-op で入力がそのまま返る。
#[cfg(unix)]
#[test]
fn xos_8_unix_long_path_is_unchanged() {
    let guard = Guard::new();
    let long = path_with_units(&guard.root, 400);
    let converted = to_long_path(&long).expect("conversion must succeed");
    assert_eq!(converted.kind(), LongPathKind::Unchanged);
    assert_eq!(converted.path(), long.as_path());
}

/// XOS-8: Unix でも 260 文字超のルートで `open_or_create` が成功し、書き込んで読み戻せる。
#[cfg(unix)]
#[test]
fn xos_8_unix_open_or_create_with_long_root() {
    use fandhe_browser_profile::DataKind;
    use std::io::Write;

    let guard = Guard::new();
    let root = path_with_units(&guard.root, 400);
    assert_eq!(units(&root), 400);
    std::fs::create_dir_all(root.parent().expect("root has a parent")).expect("create parents");

    let store = ExplicitStore::new(&root).expect("explicit store");
    let profile = store.open_or_create().expect("open_or_create");
    assert!(root.is_dir(), "profile root must exist");

    let mut file = profile
        .create_file_in(DataKind::Cookies, "long.txt".as_ref())
        .expect("create file");
    file.write_all(b"long-path").expect("write");
    drop(file);
    let read = std::fs::read(profile.data_dir(DataKind::Cookies).join("long.txt")).expect("read");
    assert_eq!(read, b"long-path");
}

/// XOS-8: MAX_PATH 前後の境界長と 400 単位で、verbatim パス経由の一連のファイル操作が通る。
#[cfg(windows)]
#[test]
fn xos_8_windows_file_ops_at_boundary_lengths() {
    use std::collections::BTreeSet;

    // 247/248 は CreateDirectoryW の下限（MAX_PATH - 12）、259〜261 は MAX_PATH の境界。
    for target in [247usize, 248, 259, 260, 261, 400] {
        let guard = Guard::new();
        let dir = path_with_units(&guard.root, target);
        assert_eq!(units(&dir), target);

        let long = to_long_path(&dir).expect("to_long_path");
        assert!(matches!(
            long.kind(),
            LongPathKind::Disk | LongPathKind::Unc
        ));
        assert!(long.path().to_string_lossy().starts_with(r"\\?\"));
        let long_dir = long.path().to_path_buf();

        std::fs::create_dir_all(&long_dir).expect("create_dir_all");
        let file = long_dir.join("profile-data.bin");
        std::fs::write(&file, b"0123456789").expect("write");
        assert_eq!(std::fs::read(&file).expect("read"), b"0123456789");
        assert_eq!(std::fs::metadata(&file).expect("metadata").len(), 10);

        let names: BTreeSet<String> = std::fs::read_dir(&long_dir)
            .expect("read_dir")
            .map(|e| {
                let p = e.expect("entry").path();
                assert!(
                    p.to_string_lossy().starts_with(r"\\?\"),
                    "prefix lost: {p:?}"
                );
                p.file_name().expect("name").to_string_lossy().into_owned()
            })
            .collect();
        assert_eq!(names, BTreeSet::from(["profile-data.bin".to_string()]));

        let renamed = long_dir.join("renamed.bin");
        std::fs::rename(&file, &renamed).expect("rename");
        assert!(!file.exists(), "old path must be gone");
        assert!(renamed.exists(), "new path must exist");
        std::fs::remove_file(&renamed).expect("remove_file");
        std::fs::remove_dir_all(&long_dir).expect("remove_dir_all");
    }
}

/// XOS-8: Windows で 260 文字超のルートの `open_or_create` は、変換成功後の ACL 未実装
/// ゲート（`XOS-7`）で `Unsupported` になる。`InvalidLayout`・`Io` ではないことから変換
/// 自体は成功している。成功扱いには丸めない。
#[cfg(windows)]
#[test]
fn xos_8_windows_open_or_create_with_long_root_is_unsupported() {
    use fandhe_browser_profile::ProfileError;

    let guard = Guard::new();
    let root = path_with_units(&guard.root, 300);
    let long_parent = to_long_path(root.parent().expect("root has a parent")).expect("parent");
    std::fs::create_dir_all(long_parent.path()).expect("create parents");

    let store = ExplicitStore::new(&root).expect("explicit store");
    let result = store.open_or_create();
    assert!(
        matches!(result, Err(ProfileError::Unsupported { .. })),
        "unexpected result: {:?}",
        result.err()
    );
}
