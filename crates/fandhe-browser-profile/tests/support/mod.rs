//! 分離テスト（`tests/isolation.rs`・`tests/concurrent_isolation.rs`）が共有する書き込み
//! アダプタ（`PROF-2`・`PROF-3`・Issue #954・#1313）。
//!
//! 両テストの書き込み経路を 1 か所にまとめる。unix・Windows とも製品の
//! `Profile::create_file_in`（保持するデータ種別ディレクトリのハンドル相対の open）をそのまま
//! 使う。Windows 版は #1313 で実装済みで、以前の std の `OpenOptions` によるアダプタは無い。
//!
//! 呼び出し元: 各テストの `write_entry`・`write_once`・`write_in_place`。

use fandhe_browser_profile::{DataKind, Profile};
use std::ffi::OsStr;
use std::fs::File;

/// `profile` の `kind` ディレクトリ配下の `name` を、読み書き・作成ありで開く
/// （切り詰めない。`create_file_in` と同じ契約）。失敗は呼び出し側が panic させる。
pub fn open_data_file(profile: &Profile, kind: DataKind, name: &OsStr) -> Result<File, String> {
    profile
        .create_file_in(kind, name)
        .map_err(|e| format!("create_file_in({kind:?}, {name:?}): {e}"))
}
