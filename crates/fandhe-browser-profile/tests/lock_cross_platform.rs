//! プロセス間の advisory lock 競合検出を 3 OS（Linux・macOS・Windows）の
//! CI 上で確認する結合テスト（ビヘイビア `PROF-1`、TASK-54（54.3）・#193）。
//!
//! `src/lock.rs` の単体テストは同一プロセス内で 2 つの `File` ハンドルを
//! 使う検証にとどまり、**別プロセス**からの `flock`/`LockFileEx` 競合は
//! 未確認だった（親 Issue #189 が TASK-54 の範囲を縮小した後の残り）。本
//! ファイルはそれを埋める。
//!
//! ## Windows で `Profile::open` を使わない理由
//!
//! `Profile::open` は Windows では ACL 隔離（`XOS-7`〜`XOS-10`）が未実装の
//! ため常に `ProfileError::Unsupported` を返し（`src/profile.rs` の
//! `#[cfg(not(unix))]` 分岐）、公開 API 経由では Windows 上のロック競合を
//! 確認できない。また `lock::try_acquire`/`LOCK_FILE_NAME` は `pub(crate)`
//! であり、crate 境界の変更は本 Issue の範囲外（結合テストから直接呼べない）。
//! そのため、3 OS 共通の確認は std の `File::try_lock`（`lock.rs` が実際に
//! 使っているのと同じ API）を直接使う形で行い、`Profile::open` を経由した
//! 確認は Unix 限定のケースに分離する。
//!
//! ## 子プロセスの起動方式
//!
//! 親プロセス（このテストバイナリ自身）が [`std::env::current_exe`] を
//! `Command` で再実行し、環境変数でロックファイルのパスとモードを渡す。
//! 子として実行されたときのエントリポイントは [`lock_child_entry`] で、
//! 対応する環境変数が無い通常の `cargo test` 実行時は何もせず成功する。
//! `Command` は Unix では CLOEXEC 付き、Windows では継承不可のハンドルで
//! 子を作るため、子プロセスは親のロックハンドルを一切引き継がず、必ず
//! 自分で新たに開いたハンドルでロックを試す（このテストが確認したい
//! 「別プロセスからの競合検出」の前提）。
//!
//! 結果は stdout ではなく終了コードで返す（libtest 自体の出力と混ざらない
//! ようにするため）。`10` は取得成功、`11` は競合検出、`12` はその他の
//! エラー、`13` は環境変数の値が不正、を意味する。`TryLockError::Error`
//! （その他のエラー）を成功扱いにはしない（security.md「偽装・回避機能の
//! 禁止」: 未実装・失敗を成功に丸めない）。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 子プロセスへロックファイルのパスを渡す環境変数名。
const ENV_CHILD_PATH: &str = "FANDHE_PROFILE_LOCK_CHILD_PATH";
/// 子プロセスへ動作モード（`std` または `profile`）を渡す環境変数名。
const ENV_CHILD_MODE: &str = "FANDHE_PROFILE_LOCK_CHILD_MODE";

/// 子プロセスが `std::fs::File::try_lock` でロックを取得できた場合の終了コード。
const EXIT_ACQUIRED: i32 = 10;
/// 子プロセスがロック競合を検出した場合の終了コード
/// （std モードは `TryLockError::WouldBlock`、profile モードは
/// `ProfileError::Locked`）。
const EXIT_CONFLICT: i32 = 11;
/// 子プロセスがその他のエラー（I/O エラー・非対応プラットフォーム等）に
/// 遭遇した場合の終了コード。成功へ丸めず、失敗として区別する。
const EXIT_OTHER_ERROR: i32 = 12;
/// 環境変数の値が不正（想定外の `ENV_CHILD_MODE` 等）だった場合の終了コード。
const EXIT_BAD_ARGS: i32 = 13;

/// 子プロセスのエントリポイント。
///
/// `#[test]` として登録されているが、対応する環境変数
/// （[`ENV_CHILD_PATH`]・[`ENV_CHILD_MODE`]）が設定されていない通常の
/// `cargo test` 実行では何もせずに成功する。親テストが
/// `current_exe() -- lock_child_entry --exact --test-threads=1` を
/// 環境変数付きで再実行したときだけ、実際にロックを試して
/// [`std::process::exit`] で終了コードを返す（このテスト関数自身の
/// pass/fail は使わない）。
#[test]
fn lock_child_entry() {
    let (path, mode) = match (
        std::env::var_os(ENV_CHILD_PATH),
        std::env::var_os(ENV_CHILD_MODE),
    ) {
        (Some(path), Some(mode)) => (PathBuf::from(path), mode),
        _ => return,
    };

    let code = match mode.to_str() {
        Some("std") => run_std_mode(&path),
        Some("profile") => run_profile_mode(&path),
        _ => EXIT_BAD_ARGS,
    };
    std::process::exit(code);
}

/// std レベルのモード: `<path>` を読み書き用に開き、`try_lock` を 1 回だけ
/// 試す。`lock.rs::try_acquire` と同じ対応づけ（`Ok` → 取得、
/// `WouldBlock` → 競合、`Error` → その他のエラー）を使う。
fn run_std_mode(path: &Path) -> i32 {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return EXIT_OTHER_ERROR,
    };

    match file.try_lock() {
        Ok(()) => EXIT_ACQUIRED,
        Err(TryLockError::WouldBlock) => EXIT_CONFLICT,
        Err(TryLockError::Error(_)) => EXIT_OTHER_ERROR,
    }
}

/// `Profile::open` レベルのモード（Unix 限定）。`<path>` をプロファイル
/// ルートとして開き、crate 自体がプロセス間の二重 open を拒否することを
/// 確かめる。Windows は `Profile::open` が常に `Unsupported` を返すため
/// [`EXIT_OTHER_ERROR`] になる（呼び出し元は Unix 限定ケースでしか
/// このモードを使わない）。
#[cfg(unix)]
fn run_profile_mode(path: &Path) -> i32 {
    use fandhe_browser_profile::{Profile, ProfileError};

    match Profile::open(path) {
        Ok(_profile) => EXIT_ACQUIRED,
        Err(ProfileError::Locked { .. }) => EXIT_CONFLICT,
        Err(_) => EXIT_OTHER_ERROR,
    }
}

/// Windows では `Profile::open` を使わないため、モード自体を「その他の
/// エラー」として扱う（unused import を避けるため `#[cfg(unix)]` の
/// 関数と分離する）。
#[cfg(not(unix))]
fn run_profile_mode(_path: &Path) -> i32 {
    EXIT_OTHER_ERROR
}

/// テスト用の一時ディレクトリ。`std::fs` だけで完結させ、`tempfile` 等の
/// 外部依存は追加しない（dependency-policy.md）。
struct TempDir {
    path: PathBuf,
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-lock-xp-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("一時ディレクトリの作成");
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

/// 子プロセス（`lock_child_entry`）を指定モードで起動し、終了コードを返す。
///
/// 親プロセス自身の環境は変更せず（edition 2024 で `set_var` は unsafe で
/// あり、並列実行される他テストへ副作用が漏れるため）、`Command::env` で
/// 子プロセスにだけ値を渡す。
fn run_child(lock_path: &Path, mode: &str) -> std::process::Output {
    let exe = std::env::current_exe().expect("現在の結合テストバイナリのパス");
    Command::new(exe)
        .args(["lock_child_entry", "--exact", "--test-threads=1"])
        .env(ENV_CHILD_PATH, lock_path)
        .env(ENV_CHILD_MODE, mode)
        .output()
        .expect("子プロセスの起動")
}

fn assert_exit_code(output: &std::process::Output, expected: i32, context: &str) {
    let actual = output.status.code();
    assert_eq!(
        actual,
        Some(expected),
        "{context}: 終了コードが一致しない（stdout={:?}, stderr={:?}）",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn open_rw(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("ロックファイルを開く")
}

/// PROF-1（TASK-54（54.3）・#193）: 親プロセスがロックを保持している間、
/// 別プロセス（子）が同じロックファイルへ `try_lock` すると即座に競合
/// （`WouldBlock`）として検出される。親がロックを解放（drop）した後は、
/// 子が取得できる。3 OS 共通（cfg で絞らない）。
#[test]
fn prof_1_cross_process_lock_conflict_is_detected() {
    let tmp = TempDir::new();
    let lock_path = tmp.path().join("profile.lock");

    let parent_file = open_rw(&lock_path);
    parent_file
        .try_lock()
        .expect("親プロセスの 1 回目のロック取得は成功する");

    let conflict = run_child(&lock_path, "std");
    assert_exit_code(&conflict, EXIT_CONFLICT, "親がロック保持中の子プロセス");

    // Windows の `LockFileEx`/`UnlockFile` は、ハンドルを閉じるだけだと OS が
    // ロックを解放するタイミングが保証されない（Win32 API ドキュメント:
    // クローズ時の解放は "available system resources" 依存）。明示的に
    // `unlock()`（`UnlockFile`。同期的に解放される）してから `drop` することで、
    // 直後の子プロセスが確実に取得できる状態にする。
    parent_file.unlock().expect("親プロセスのアンロック");
    drop(parent_file);

    let acquired = run_child(&lock_path, "std");
    assert_exit_code(
        &acquired,
        EXIT_ACQUIRED,
        "親のロック解放後に起動した子プロセス",
    );
}

/// PROF-1（TASK-54（54.3）・#193）: 陰性対照。親がロックを持たない状態で
/// 子プロセスを起動すると取得に成功する。これにより、ケース 1 の競合
/// 検出（`EXIT_CONFLICT`）がパスの受け渡し不備等で空振りしていないことを
/// 確かめる。3 OS 共通。
#[test]
fn prof_1_cross_process_lock_acquire_without_holder_succeeds() {
    let tmp = TempDir::new();
    let lock_path = tmp.path().join("profile.lock");

    let acquired = run_child(&lock_path, "std");
    assert_exit_code(
        &acquired,
        EXIT_ACQUIRED,
        "保持者がいない状態で起動した子プロセス",
    );
}

/// PROF-1（TASK-54（54.3）・#193）: `Profile::open` 経由でも、別プロセスが
/// 既にロックを保持していれば `ProfileError::Locked` を返す。crate 自体が
/// プロセス間の二重 open を拒否することの確認（Unix 限定。Windows は
/// `Profile::open` 自体が ACL 未実装で `Unsupported` を返すため対象外。
/// `XOS-7`〜`XOS-10` 完了後の課題）。
#[cfg(unix)]
#[test]
fn prof_1_cross_process_profile_open_returns_locked() {
    use fandhe_browser_profile::Profile;

    let tmp = TempDir::new();
    let root = tmp.path().join("profile-root");

    let parent_profile = Profile::open(&root).expect("親プロセスの Profile::open は成功する");

    let conflict = run_child(&root, "profile");
    assert_exit_code(
        &conflict,
        EXIT_CONFLICT,
        "親が Profile を保持中の子プロセス（Profile::open 経由）",
    );

    drop(parent_profile);

    let acquired = run_child(&root, "profile");
    assert_exit_code(
        &acquired,
        EXIT_ACQUIRED,
        "親の Profile 解放後に起動した子プロセス（Profile::open 経由）",
    );
}
