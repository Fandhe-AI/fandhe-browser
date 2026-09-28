//! `Config::load`（TASK-91（91.1）・Issue #214）のファイル読み込みを含む
//! 結合テスト。ユニットテスト（`src/config.rs`）は文字列入力のみを扱うため、
//! 実ファイルの読み込み・パス解決・I/O エラー経路はここで確認する。
//!
//! 一時ディレクトリの作成・drop 時再帰削除は
//! `crates/fandhe-browser-profile/tests/profile_open.rs` の流儀
//! （`std::env::temp_dir()` 基点・`tempfile` 等の外部依存を追加しない。
//! dependency-policy.md）を踏襲する。

use fandhe_browser_core::{Config, ConfigError, Error, IsolationStrength};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// プロセス全体の CWD を変更するテストが互いに競合しないようにする
/// ロック（`std::env::set_current_dir` はプロセス単位の状態のため。
/// `cargo test` は既定で同一バイナリ内のテストを並列実行する）。
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// テスト用の一時ディレクトリ。drop 時に再帰削除する。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir();
        let path = base.join(format!(
            "fandhe-core-config-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("一時ディレクトリを作成できる");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write_config(&self, contents: &str) -> PathBuf {
        let config_path = self.path.join("fandhe-browser.toml");
        let mut file = std::fs::File::create(&config_path).expect("設定ファイルを作成できる");
        file.write_all(contents.as_bytes())
            .expect("設定ファイルへ書き込める");
        config_path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// TASK-91（91.1）: 一時ファイルに書いた TOML から `root`・`isolation` を
/// 読める（Issue #214 受入基準 2 の証跡）。
#[test]
fn task_91_1_load_reads_profile_root_and_isolation() {
    let dir = TempDir::new();
    let config_path =
        dir.write_config("[profile]\nroot = 'profiles/default'\nisolation = 'data-directory'\n");

    let config = Config::load(&config_path).expect("設定ファイルを読み込める");

    assert_eq!(
        config.profile().root(),
        Some(dir.path().join("profiles/default")).as_deref()
    );
    assert_eq!(
        config.profile().isolation(),
        IsolationStrength::DataDirectory
    );
}

/// TASK-91（91.1）: 相対 `root` は設定ファイルの親ディレクトリ基準で
/// `join` された `PathBuf` になる（CWD には依存しない）。
#[test]
fn task_91_1_relative_root_is_resolved_against_config_parent_dir() {
    let dir = TempDir::new();
    let config_path = dir.write_config("[profile]\nroot = 'a'\n");

    let config = Config::load(&config_path).expect("設定ファイルを読み込める");

    assert_eq!(
        config.profile().root(),
        Some(dir.path().join("a")).as_deref()
    );
}

/// TASK-91（91.1）: `path` 自体を相対パス（`fandhe-browser.toml` のような
/// ファイル名のみ）で `Config::load` へ渡した場合でも、解決後の
/// `profile.root` は絶対パスになる（Issue #538 P1 レビュー指摘の回帰防止。
/// 相対 `path` の親ディレクトリをそのまま `join` すると解決結果も相対の
/// ままになり、`Config::load` 完了後に呼び出し元が CWD を変更すると
/// `Profile::open` が別の場所を開いてしまう）。
#[test]
fn task_91_1_load_with_relative_path_resolves_root_to_absolute() {
    let _guard = CWD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = TempDir::new();
    dir.write_config("[profile]\nroot = 'profiles/default'\n");

    let original_cwd = std::env::current_dir().expect("CWD を取得できる");
    std::env::set_current_dir(dir.path()).expect("テスト用ディレクトリへ CWD を移せる");

    let load_result = std::panic::catch_unwind(|| {
        Config::load(Path::new("fandhe-browser.toml")).expect("相対パスの設定ファイルを読み込める")
    });

    std::env::set_current_dir(&original_cwd).expect("元の CWD へ戻せる");
    let config = load_result.expect("Config::load がパニックしない");

    // 期待する祖先ディレクトリ（設定ファイルの親＝一時ディレクトリ自体。
    // 実在するので `canonicalize()` できる）を、`dir.path()` を
    // `canonicalize()` した上で組み立てる（Bugbot 指摘の回帰防止）。
    // `"profiles/default"` サブディレクトリ自体は作成していないため
    // 実在せず、`root` 全体は `canonicalize()` できない。実装側は CWD
    // 基準の絶対化に `std::path::absolute`（内部で `current_dir()`／
    // `getcwd` を使う）を用いており、macOS では `set_current_dir` 後の
    // `getcwd` がシンボリックリンク解決済みパス（`/private/var/...`）を
    // 返す一方、`std::env::temp_dir()`（`dir.path()` の基点）は未解決の
    // まま（`/var/...`）となる。両者は同じディレクトリを指すが字句表現が
    // 異なるため、比較前に実在する祖先部分だけを `canonicalize()` して
    // 物理パスへ揃える（実装側は symlink 解決をファイルシステムアクセス
    // なしで行う設計方針〔モジュール doc「パス解決」参照〕のため変更
    // しない。あくまでテストの比較方法の修正）。
    let canonical_dir = dir
        .path()
        .canonicalize()
        .expect("一時ディレクトリを canonicalize できる");

    let assert_root_resolves_to = |root: &Path| {
        assert!(
            root.is_absolute(),
            "root は絶対パスに解決されるべき: {root:?}"
        );
        assert_eq!(
            root.file_name(),
            Some(std::ffi::OsStr::new("default")),
            "root の末尾要素は 'default' であるべき: {root:?}"
        );
        assert_eq!(
            root.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("profiles")),
            "root の親要素は 'profiles' であるべき: {root:?}"
        );
        let ancestor = root
            .parent()
            .and_then(Path::parent)
            .expect("root は <一時ディレクトリ>/profiles/default の形をしている");
        assert_eq!(
            ancestor
                .canonicalize()
                .expect("root の祖先（一時ディレクトリ自体）は実在し canonicalize できる"),
            canonical_dir,
            "root の祖先は設定ファイルの親ディレクトリと物理的に一致するべき: {root:?}"
        );
    };

    let root = config.profile().root().expect("root が解決されている");
    assert_root_resolves_to(root);

    // CWD を設定ファイル読み込み後に変更しても、解決済みの root は
    // 変わらない（CWD 非依存の契約）。
    let another_dir = TempDir::new();
    std::env::set_current_dir(another_dir.path()).expect("別ディレクトリへ CWD を移せる");
    let unaffected = config.profile().root().map(Path::to_path_buf);
    std::env::set_current_dir(&original_cwd).expect("元の CWD へ戻せる");
    assert_root_resolves_to(unaffected.as_deref().expect("root が保持されている"));
}

/// TASK-91（91.1）: 絶対 `root` はそのまま使われる（親ディレクトリで
/// 上書きしない）。
#[test]
fn task_91_1_absolute_root_is_used_as_is() {
    let dir = TempDir::new();
    let absolute_root = dir.path().join("elsewhere");
    let toml_source = format!("[profile]\nroot = {absolute_root:?}\n");
    let config_path = dir.write_config(&toml_source);

    let config = Config::load(&config_path).expect("設定ファイルを読み込める");

    assert_eq!(config.profile().root(), Some(absolute_root.as_path()));
}

/// TASK-91（91.1）: 絶対 `root` が境界検証を通過した場合、返される
/// `profile.root` は字句正規化済みのパスになる（Issue #538 P0 レビュー
/// 再指摘の回帰防止）。`<dir>/../outside/../<dir 名>/profiles` は字句上の
/// 最終到達点こそ `<dir>/profiles`（設定ディレクトリ配下）で境界検証は
/// 通過するが、未正規化のまま返すと
/// `fandhe-browser-profile::Profile::open`（TASK-50・#177）が要素ごとに
/// 辿った際、実在しない兄弟ディレクトリ `outside` を経由しようとして
/// 設定ディレクトリ外に作成しかねない。返り値が正規化済みで `outside` を
/// 含まないことを確認する。
#[test]
fn task_91_1_absolute_root_with_lexical_escape_is_normalized_before_return() {
    let dir = TempDir::new();
    let dir_name = dir
        .path()
        .file_name()
        .expect("一時ディレクトリ名を取得できる");
    let unnormalized_root = dir
        .path()
        .join("..")
        .join("outside")
        .join("..")
        .join(dir_name)
        .join("profiles");
    let toml_source = format!("[profile]\nroot = {unnormalized_root:?}\n");
    let config_path = dir.write_config(&toml_source);

    let config = Config::load(&config_path).expect("境界検証を通過して読み込める");

    let root = config.profile().root().expect("root が解決されている");
    assert_eq!(
        root,
        dir.path().join("profiles"),
        "root は字句正規化済みのパスであるべき（未正規化の 'outside' を含んではならない）"
    );
    assert!(
        !root.components().any(|c| c.as_os_str() == "outside"),
        "root に未正規化の中間要素 'outside' が残っている: {root:?}"
    );
}

/// TASK-91（91.1）: 存在しないファイルは `ConfigError::Io` になる。
#[test]
fn task_91_1_missing_file_is_io_error() {
    let dir = TempDir::new();
    let missing_path = dir.path().join("does-not-exist.toml");

    let err = Config::load(&missing_path).expect_err("存在しないファイルはエラーになる");

    assert!(matches!(err, Error::Config(ConfigError::Io { .. })));
}

/// TASK-91（91.1）: `MAX_CONFIG_BYTES` を超えるファイルは `TooLarge` になる
/// （ファイル全体を読み切る前に `Read::take` で打ち切る）。
#[test]
fn task_91_1_oversized_file_is_too_large_error() {
    let dir = TempDir::new();
    let oversized = "a".repeat(fandhe_browser_core::config::MAX_CONFIG_BYTES + 1);
    let config_path = dir.write_config(&oversized);

    let err = Config::load(&config_path).expect_err("上限超過ファイルはエラーになる");

    assert!(matches!(
        err,
        Error::Config(ConfigError::TooLarge {
            limit: fandhe_browser_core::config::MAX_CONFIG_BYTES
        })
    ));
}

/// TASK-91（91.1）・PROF-6: `root = "."` は設定ファイルの親ディレクトリ
/// 自体を指すため `Config::load` でも拒否される（Issue #538 レビュー指摘の
/// 回帰防止。`Profile::open`（TASK-50）が解決後のルートへ権限変更・子
/// ディレクトリ作成・ロック取得を行う契約のため、設定ファイルを置いた
/// ディレクトリへの意図しない副作用を防ぐ）。
#[test]
fn task_91_1_load_rejects_root_pointing_to_config_dir() {
    let dir = TempDir::new();
    let config_path = dir.write_config("[profile]\nroot = '.'\n");

    let err = Config::load(&config_path).expect_err("\".\" は Config::load でも拒否される");

    assert!(matches!(
        err,
        Error::Config(ConfigError::InvalidValue {
            key: "profile.root",
            ..
        })
    ));
}

/// TASK-91（91.1）・PROF-6: `root = "profiles/.."` は正規化すると設定
/// ファイルの親ディレクトリ自体を指すため `Config::load` でも拒否される
/// （Issue #538 レビュー指摘の回帰防止）。
#[test]
fn task_91_1_load_rejects_root_normalizing_to_config_dir() {
    let dir = TempDir::new();
    let config_path = dir.write_config("[profile]\nroot = 'profiles/..'\n");

    let err = Config::load(&config_path)
        .expect_err("\"profiles/..\" は正規化後に設定ファイルの親ディレクトリを指す");

    assert!(matches!(
        err,
        Error::Config(ConfigError::InvalidValue {
            key: "profile.root",
            ..
        })
    ));
}

/// TASK-91（91.1）・PROF-6: 設定ファイルの親ディレクトリ配下を指す絶対
/// `root` は受理される（`task_91_1_absolute_root_is_used_as_is` の境界検証
/// 追加後の回帰防止。Issue #538 P0 レビュー指摘対応）。
#[test]
fn task_91_1_absolute_root_under_config_dir_is_accepted() {
    let dir = TempDir::new();
    let absolute_root = dir.path().join("nested").join("profiles");
    let toml_source = format!("[profile]\nroot = {absolute_root:?}\n");
    let config_path = dir.write_config(&toml_source);

    let config = Config::load(&config_path).expect("設定ファイルの配下を指す絶対パスは受理される");

    assert_eq!(config.profile().root(), Some(absolute_root.as_path()));
}

/// TASK-91（91.1）・PROF-6: 設定ファイルのディレクトリ**外**を指す絶対 `root`
/// は拒否される（Issue #538 P0 レビュー指摘: `resolves_to_zero_depth`
/// 相当の検証は絶対パスを対象外にしていたため、`root = "/任意の既存
/// ディレクトリ"` が受理されてしまっていた）。
#[test]
fn task_91_1_absolute_root_outside_config_dir_is_rejected() {
    let dir = TempDir::new();
    let outside = TempDir::new();
    let toml_source = format!("[profile]\nroot = {:?}\n", outside.path());
    let config_path = dir.write_config(&toml_source);

    let err = Config::load(&config_path)
        .expect_err("設定ファイルのディレクトリ外を指す絶対パスは拒否される");

    assert!(matches!(
        err,
        Error::Config(ConfigError::InvalidValue {
            key: "profile.root",
            ..
        })
    ));
}

/// TASK-91（91.1）・PROF-6: 絶対 `root` が設定ファイルのディレクトリ自体と
/// 完全一致する場合も拒否される（`root = "."` の相対版に相当する絶対パスの
/// 抜け穴。Issue #538 P0 レビュー指摘対応）。
#[test]
fn task_91_1_absolute_root_equal_to_config_dir_is_rejected() {
    let dir = TempDir::new();
    let toml_source = format!("[profile]\nroot = {:?}\n", dir.path());
    let config_path = dir.write_config(&toml_source);

    let err = Config::load(&config_path)
        .expect_err("設定ファイル自身のディレクトリを指す絶対パスは拒否される");

    assert!(matches!(
        err,
        Error::Config(ConfigError::InvalidValue {
            key: "profile.root",
            ..
        })
    ));
}

/// TASK-91（91.1）: 非 UTF-8 バイト列は `InvalidUtf8` になる。
#[test]
fn task_91_1_non_utf8_file_is_invalid_utf8_error() {
    let dir = TempDir::new();
    let config_path = dir.path().join("fandhe-browser.toml");
    std::fs::write(&config_path, [0xff, 0xfe, 0xfd]).expect("バイト列を書き込める");

    let err = Config::load(&config_path).expect_err("非 UTF-8 はエラーになる");

    assert!(matches!(err, Error::Config(ConfigError::InvalidUtf8)));
}
