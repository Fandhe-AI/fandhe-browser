//! 他 crate のテスト・bench が `Profile::open` 用の一時ルートの基点を得るための補助
//! （`#[doc(hidden)]`。製品コードからは使わない。PROF-1・XOS-10・Issue #955）。
//!
//! Windows の `Profile::open` は `%LOCALAPPDATA%` 配下のルートだけを受け付ける。
//! `std::env::temp_dir()`（`TEMP`）は `C:/Temp` のように配下外を指し得るため、
//! Windows では `%LOCALAPPDATA%` 配下の専用ディレクトリを基点にする。

use std::path::PathBuf;

/// `Profile::open` が受け付ける一時ルートの基点（正規化済みの既存ディレクトリ）を返す。
///
/// - unix: `temp_dir()` を `canonicalize` したもの（macOS の `/var` symlink を避ける）。
/// - Windows: `%LOCALAPPDATA%\fandhe-test-tmp` を作成して `canonicalize` したもの。
///   `LOCALAPPDATA` が未設定・作成不能のときは `temp_dir()` へフォールバックする
///   （その場合 `Profile::open` が拒否することがあり、テストが失敗して表面化する）。
#[doc(hidden)]
pub fn test_base_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let dir = PathBuf::from(local).join("fandhe-test-tmp");
            if std::fs::create_dir_all(&dir).is_ok() {
                if let Ok(c) = dir.canonicalize() {
                    return c;
                }
            }
        }
    }
    let tmp = std::env::temp_dir();
    tmp.canonicalize().unwrap_or(tmp)
}
