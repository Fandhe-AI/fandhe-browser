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
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

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

/// TASK-91（91.1）: 非 UTF-8 バイト列は `InvalidUtf8` になる。
#[test]
fn task_91_1_non_utf8_file_is_invalid_utf8_error() {
    let dir = TempDir::new();
    let config_path = dir.path().join("fandhe-browser.toml");
    std::fs::write(&config_path, [0xff, 0xfe, 0xfd]).expect("バイト列を書き込める");

    let err = Config::load(&config_path).expect_err("非 UTF-8 はエラーになる");

    assert!(matches!(err, Error::Config(ConfigError::InvalidUtf8)));
}
