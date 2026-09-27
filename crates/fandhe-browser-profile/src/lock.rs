//! プロファイルルートの二重 open を防ぐ advisory lock を担うモジュール。
//!
//! `profile.rs` の `Profile::open`（unix）から呼ばれ、プロファイルルート
//! 直下の `profile.lock`（[`LOCK_FILE_NAME`]）に対して排他ロックを取る
//! （ビヘイビア `PROF-1`、TASK-50（50.3）・#178）。
//!
//! 新規依存は追加せず、std の `File::try_lock`/`File::unlock`
//! （Rust 1.89 で安定化。unix は `flock(LOCK_EX|LOCK_NB)`、Windows は
//! `LockFileEx` を使う）だけで実装する。`fs2`/`fs4`/`fd-lock` は導入しない
//! （#175・#190 の決定）。std の実装が既に 3 OS をカバーするため、本モジュール
//! は `cfg(unix)` 等の分岐を持たず、そのまま 3 OS でコンパイル・テストできる
//! （TASK-54 が求めていたクロスプラットフォーム置換は、この std 実装により
//! 不要になった）。
//!
//! ## advisory lock の限界
//!
//! `flock`/`LockFileEx` は協調的なプロセス同士の二重 open を防ぐための
//! 仕組みであり、ロックを無視して直接読み書きする敵対的なプロセスを防ぐ
//! ものではない（security.md「偽装・回避機能の禁止」が求める範囲外。敵対的な
//! アクセスからの隔離は `profile.rs` が設定する `0o700`/`0o600` パーミッション
//! が担う）。また POSIX の advisory lock は "per open file description"
//! （fd 単位ではなくファイルディスクリプタの元になった open 呼び出し単位）の
//! 意味論を持つため、同一プロセス内でも別の `File::open` 呼び出しで得た
//! ハンドルは互いに競合する。NFS 等、一部のネットワークファイルシステムでは
//! ロックが保証されないことにも注意する。
//!
//! ロックファイル自体は `ProfileLock` の drop 時に unlink しない。unlink
//! すると、別プロセスが旧 inode のハンドルでロックを保持したまま、新しい
//! プロセスが新規作成した別 inode に対してロックを取得できてしまい、
//! 二重 open の検出という目的そのものが壊れるため。
#![cfg_attr(not(unix), allow(dead_code))]
// Windows の ACL 隔離（`XOS-7`〜`XOS-10`）が入るまで `Profile::open` が
// 早期に `Unsupported` を返すため、非 unix ビルドでは本モジュールの型・関数が
// 未使用（dead_code）になる。テストビルドでは単体テストが使うため許可は不要
// だが、非テストの Windows ビルドで `-D warnings` に引っかからないよう
// 明示的に許可する。

use std::fs::{File, TryLockError};
use std::path::Path;

use crate::profile::ProfileError;

/// プロファイルルート直下に置く advisory lock ファイルの名前。
pub(crate) const LOCK_FILE_NAME: &str = "profile.lock";

/// 取得済みの advisory lock を表す。
///
/// `Profile` が最後まで保持し、`Drop` でアンロックする。ロックの実体は
/// 保持する `File` ハンドルに紐づく（per open file description）ため、この
/// 値が生存している間だけロックが有効であることが保証される。
#[derive(Debug)]
pub(crate) struct ProfileLock {
    file: File,
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // アンロックに失敗しても、直後にハンドルを閉じれば OS 側でロックは
        // 解放される（`File` の drop がハンドルを閉じる）。`unlock` はここでは
        // 明示的な意図表明であり、失敗時に panic や再試行をする実利がないため
        // エラーを無視する。
        let _ = self.file.unlock();
    }
}

/// `file`（`profile.lock` を指す、検証済みのハンドル）に対して非ブロッキングで
/// 排他ロックを取得する。
///
/// 既に他の `Profile`（同一プロセスの別インスタンス、または別プロセス）が
/// ロックを保持している場合は、待たずに即座に
/// [`ProfileError::Locked`](crate::profile::ProfileError::Locked) を返す
/// （`Profile::open` の呼び出し元をハングさせない）。`display_path` はエラーに
/// 含める表示用パスで、実際のロック対象の特定には使わない。
pub(crate) fn try_acquire(file: File, display_path: &Path) -> Result<ProfileLock, ProfileError> {
    match file.try_lock() {
        Ok(()) => Ok(ProfileLock { file }),
        Err(TryLockError::WouldBlock) => Err(ProfileError::Locked {
            path: display_path.to_path_buf(),
        }),
        // ロック非対応のファイルシステム・プラットフォーム等。「未実装の
        // 機能で成功を一律に返すフォールバックを禁止する」方針（security.md）
        // に従い、成功を装わず入出力エラーとして扱う。
        Err(TryLockError::Error(source)) => Err(ProfileError::Io(source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// テスト用の一時ディレクトリ。`File` より先に宣言し、後に生成した値から
    /// 先に drop される Rust の規則により、開いたままの `File`（Windows では
    /// 開いたままのファイルを削除できない）より後に一時ディレクトリの削除が
    /// 走らないようにする（`std::fs` だけで完結させ、`tempfile` 等の外部依存は
    /// 追加しない。dependency-policy.md）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fandhe-profile-lock-test-{}-{n}",
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

    fn open_rw(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("ロックファイルを開く")
    }

    /// PROF-1: 同じロックファイルを 2 つの別々の `File` ハンドルで開いて
    /// `try_acquire` すると、1 回目は成功し、2 回目は
    /// `ProfileError::Locked { path }` で失敗する（`path` は渡した表示用パスと
    /// 一致する）。
    #[test]
    fn prof_1_lock_second_acquire_on_same_file_fails_with_locked() {
        let tmp = TempDir::new();
        let lock_path = tmp.path().join(LOCK_FILE_NAME);

        let first_file = open_rw(&lock_path);
        let first = try_acquire(first_file, &lock_path).expect("1 回目のロック取得は成功する");

        let second_file = open_rw(&lock_path);
        let err = try_acquire(second_file, &lock_path).expect_err("2 回目のロック取得は失敗する");
        assert!(matches!(
            err,
            ProfileError::Locked { path } if path == lock_path
        ));

        drop(first);
    }

    /// PROF-1: 1 つ目のロックを drop（アンロック）した後なら、2 つ目の
    /// `try_acquire` は成功する。
    #[test]
    fn prof_1_lock_reacquire_after_drop_succeeds() {
        let tmp = TempDir::new();
        let lock_path = tmp.path().join(LOCK_FILE_NAME);

        let first_file = open_rw(&lock_path);
        let first = try_acquire(first_file, &lock_path).expect("1 回目のロック取得は成功する");
        drop(first);

        let second_file = open_rw(&lock_path);
        let second =
            try_acquire(second_file, &lock_path).expect("drop 後の 2 回目のロック取得は成功する");
        drop(second);
    }
}
