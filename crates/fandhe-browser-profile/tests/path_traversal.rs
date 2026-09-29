//! `PROF-4`（TASK-53（53.1）・#187・MS-3）の公開 API 結合テスト。
//!
//! パストラバーサルを狙う入力を `fandhe_browser_profile` の公開 API
//! （`sanitize_component`・`assert_within_root`・`Profile::create_file_in`）へ
//! 渡し、二重防御（1 層目 = コンポーネント検証、2 層目 = ルート配下の字句検証）
//! ですべて拒否され、プロファイルルート外への書き込みが 0 件であることを確かめる。
//!
//! ## `src/profile.rs` のユニットテストとの分担
//!
//! あちらは実装詳細寄りの `prof_4_*`。本ファイルは公開 API のみを使い、
//! 全 `DataKind` を対象にし、着地しうる場所の名前集合を完全一致で比べる。
//!
//! ## ケース ID
//!
//! - `P4-01`〜`P4-13`: 字句パターン（`traversal_cases`）。PoC-7 の 9 入力は
//!   `..` が重複しており異なる文字列は 8 種のため、5 種を加えて 13 種にしている
//! - `P4-14`・`P4-15`: ファイルシステム層のケース（既存ファイルへの symlink・
//!   dangling symlink）。名前自体は正当で 1 層目を通過し、`openat` の
//!   `NOFOLLOW` が `InvalidLayout` で拒否する
//!
//! ## cfg 方針
//!
//! `Profile::open` は Windows では `Unsupported` を返し `create_file_in` は unix
//! 限定のため、ファイルシステムを使うテストと補助コードだけ `#[cfg(unix)]` とする。
//! 字句検証のテストは 3 OS で実行する（XOS 系）。そのため兄弟ファイルと違い
//! ファイル全体には `#![cfg(unix)]` を置かない。
//!
//! 削除機能（`PROF-5`）は #188（TASK-53.2）の範囲であり、本ファイルでは扱わない。

use fandhe_browser_profile::{ProfileError, assert_within_root, sanitize_component};
use std::collections::BTreeSet;
use std::ffi::OsString;

#[cfg(unix)]
use fandhe_browser_profile::{DataKind, Profile};
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// `PROF-4` が要求する入力パターン数の下限。
const PROF_4_MIN_CASES: usize = 9;

/// 字句パターン表の件数の固定値（表の編集ミスを検出する）。
const EXPECTED_CASE_COUNT: usize = 13;

/// PoC-7 の 9 入力（`..` の重複もそのまま持つ）。
const POC7_INPUTS: [&str; 9] = [
    "../../../etc",
    "..",
    "a/../../b",
    "/etc/passwd",
    "sub/dir",
    "../escape",
    "a/b",
    "..",
    "",
];

/// 1 件の字句パターン。
struct TraversalCase {
    id: &'static str,
    input: OsString,
    category: &'static str,
    /// `root/cookies/<input>` が字句上ルートの外を指す（2 層目の単独検証対象）。
    lexically_escapes: bool,
    from_poc7: bool,
}

fn case(
    id: &'static str,
    input: impl Into<OsString>,
    category: &'static str,
    lexically_escapes: bool,
    from_poc7: bool,
) -> TraversalCase {
    TraversalCase {
        id,
        input: input.into(),
        category,
        lexically_escapes,
        from_poc7,
    }
}

/// 字句パターン表（3 OS 共通）。P4-10 は `\` が unix では区切り文字でないため
/// OS 依存になり、2 層目の対象から外している（1 層目は 3 OS とも拒否する）。
fn traversal_cases() -> Vec<TraversalCase> {
    vec![
        case("P4-01", "..", "parent dir", true, true),
        case("P4-02", "../../../etc", "multi-level parent", true, true),
        case("P4-03", "../escape", "parent plus name", true, true),
        case("P4-04", "a/../../b", "embedded parent", true, true),
        case("P4-05", "/etc/passwd", "absolute path", true, true),
        case("P4-06", "sub/dir", "slash separated", false, true),
        case("P4-07", "a/b", "slash separated (key)", false, true),
        case("P4-08", "", "empty", false, true),
        case("P4-09", ".", "current dir", false, false),
        case("P4-10", "..\\..\\escape", "backslash parent", false, false),
        case("P4-11", "a\0b", "NUL byte", false, false),
        case("P4-12", "a".repeat(256), "over NAME_MAX", false, false),
        case("P4-13", "a..b", "embedded dots", false, false),
    ]
}

#[test]
fn prof_4_case_table_has_at_least_9_distinct_patterns() {
    let cases = traversal_cases();
    assert_eq!(cases.len(), EXPECTED_CASE_COUNT);
    assert!(cases.len() >= PROF_4_MIN_CASES);
    assert!(cases.iter().all(|c| !c.category.is_empty()));
}

#[test]
fn prof_4_case_ids_and_inputs_are_unique() {
    let cases = traversal_cases();
    let ids: BTreeSet<_> = cases.iter().map(|c| c.id).collect();
    let inputs: BTreeSet<_> = cases.iter().map(|c| c.input.clone()).collect();
    assert_eq!(ids.len(), cases.len(), "ID が重複している");
    assert_eq!(inputs.len(), cases.len(), "入力が重複している");
}

#[test]
fn prof_4_case_table_covers_all_poc7_inputs() {
    let cases = traversal_cases();
    let inputs: BTreeSet<_> = cases.iter().map(|c| c.input.clone()).collect();
    for poc in POC7_INPUTS {
        assert!(
            inputs.contains(&OsString::from(poc)),
            "PoC-7 入力 {poc:?} が表にない"
        );
    }
    // `..` の重複を除いた異なる文字列の数。
    assert_eq!(cases.iter().filter(|c| c.from_poc7).count(), 8);
}

#[test]
fn prof_4_sanitize_component_rejects_every_traversal_case() {
    let failures: Vec<_> = traversal_cases()
        .iter()
        .filter(|c| {
            !matches!(
                sanitize_component(&c.input),
                Err(ProfileError::InvalidComponent { .. })
            )
        })
        .map(|c| c.id)
        .collect();
    assert!(
        failures.is_empty(),
        "1 層目が拒否しなかったケース: {failures:?}"
    );
}

#[test]
fn prof_4_assert_within_root_rejects_lexically_escaping_cases() {
    let root = std::env::temp_dir().join("fandhe-prof4-traversal-root");
    let targets: Vec<_> = traversal_cases()
        .into_iter()
        .filter(|c| c.lexically_escapes)
        .collect();
    assert_eq!(targets.len(), 5);
    let failures: Vec<_> = targets
        .iter()
        .filter(|c| {
            let candidate = root.join("cookies").join(&c.input);
            !matches!(
                assert_within_root(&root, &candidate),
                Err(ProfileError::OutsideRoot { .. })
            )
        })
        .map(|c| c.id)
        .collect();
    assert!(
        failures.is_empty(),
        "2 層目が拒否しなかったケース: {failures:?}"
    );
}

#[cfg(windows)]
#[test]
fn prof_4_sanitize_component_rejects_windows_prefixed_inputs() {
    for raw in ["C:foo", "C:\\Windows", "\\\\server\\share"] {
        assert!(
            matches!(
                sanitize_component(std::ffi::OsStr::new(raw)),
                Err(ProfileError::InvalidComponent { .. })
            ),
            "{raw:?} が拒否されなかった"
        );
    }
}

#[cfg(unix)]
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一意な一時ディレクトリ（drop 時に削除）。作成は各テストが明示的に行う。
#[cfg(unix)]
struct TempDir {
    path: PathBuf,
}

#[cfg(unix)]
impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS の `/var` -> `/private/var` を `Profile::open` が弾くため canonicalize する。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-traversal-test-{}-{n}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `dir` 直下のエントリ名集合。symlink を見つけたら panic する（fail-closed）。
#[cfg(unix)]
fn entry_names(dir: &Path) -> BTreeSet<OsString> {
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir 失敗: {e}")) {
        let entry = entry.unwrap_or_else(|e| panic!("エントリ取得失敗: {e}"));
        let ty = entry
            .file_type()
            .unwrap_or_else(|e| panic!("file_type 失敗: {e}"));
        assert!(!ty.is_symlink(), "想定外の symlink: {:?}", entry.path());
        names.insert(entry.file_name());
    }
    names
}

#[cfg(unix)]
fn names(list: &[&str]) -> BTreeSet<OsString> {
    list.iter().map(OsString::from).collect()
}

#[cfg(unix)]
#[test]
fn prof_4_create_file_in_rejects_every_traversal_case_for_all_kinds_and_writes_nothing_outside() {
    let tmp = TempDir::new();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap_or_else(|e| panic!("outside 作成失敗: {e}"));
    let profile_root = tmp.path().join("profile");
    let profile =
        Profile::open(&profile_root).unwrap_or_else(|e| panic!("Profile::open 失敗: {e}"));

    let mut failures = Vec::new();
    for c in traversal_cases() {
        for &kind in DataKind::ALL {
            if !matches!(
                profile.create_file_in(kind, &c.input),
                Err(ProfileError::InvalidComponent { .. })
            ) {
                failures.push((c.id, kind));
            }
        }
    }
    assert!(failures.is_empty(), "拒否されなかった組: {failures:?}");

    // 着地しうる場所の名前集合を完全一致で確認する。
    assert_eq!(entry_names(tmp.path()), names(&["profile", "outside"]));
    assert_eq!(
        entry_names(&profile_root),
        names(&["profile.lock", "cookies", "storage", "cache", "history"])
    );
    for &kind in DataKind::ALL {
        assert!(
            entry_names(&profile.data_dir(kind)).is_empty(),
            "{kind:?} が非空"
        );
    }
    assert!(entry_names(&outside).is_empty(), "outside が非空");

    // 陰性対照: 正当な名前は成功する（プロファイル自体は壊れていない）。
    profile
        .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("example.com"))
        .unwrap_or_else(|e| panic!("正当な名前が拒否された: {e}"));
    assert_eq!(
        entry_names(&profile.data_dir(DataKind::Cookies)),
        names(&["example.com"])
    );
}

#[cfg(unix)]
#[test]
fn prof_4_create_file_in_refuses_symlink_escaping_profile_root() {
    // P4-14: 境界外の既存ファイルへの symlink / P4-15: dangling symlink。
    let tmp = TempDir::new();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap_or_else(|e| panic!("outside 作成失敗: {e}"));
    let secret = outside.join("secret");
    std::fs::write(&secret, b"original").unwrap_or_else(|e| panic!("secret 作成失敗: {e}"));
    let profile = Profile::open(tmp.path().join("profile"))
        .unwrap_or_else(|e| panic!("Profile::open 失敗: {e}"));

    for &kind in DataKind::ALL {
        let dir = profile.data_dir(kind);
        let dangling_target = outside.join(format!("never-created-{}", kind.dir_name()));
        symlink(&secret, dir.join("session")).unwrap_or_else(|e| panic!("symlink 失敗: {e}"));
        symlink(&dangling_target, dir.join("dangling"))
            .unwrap_or_else(|e| panic!("symlink 失敗: {e}"));

        for (id, name) in [("P4-14", "session"), ("P4-15", "dangling")] {
            assert!(
                matches!(
                    profile.create_file_in(kind, std::ffi::OsStr::new(name)),
                    Err(ProfileError::InvalidLayout { .. })
                ),
                "{id} {kind:?} が InvalidLayout で拒否されなかった"
            );
        }
        assert_eq!(
            std::fs::symlink_metadata(&dangling_target)
                .err()
                .map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound),
            "dangling のリンク先が作られた: {kind:?}"
        );
    }
    let content = std::fs::read(&secret).unwrap_or_else(|e| panic!("secret 読み込み失敗: {e}"));
    assert_eq!(content, b"original");
}
