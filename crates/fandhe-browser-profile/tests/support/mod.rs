//! 分離テスト（`tests/isolation.rs`・`tests/concurrent_isolation.rs`）が共有する書き込み
//! アダプタ（`PROF-2`・`PROF-3`・Issue #954）。
//!
//! 両テストの書き込み経路を 1 か所にまとめ、OS ごとの差をここに閉じ込める。
//!
//! - unix: 製品の `Profile::create_file_in`（ハンドル相対の `openat` 系）をそのまま使う
//! - Windows: `Profile::create_file_in` は未実装（`REPAIR-3`。#948 配下の Issue が完了するまで
//!   Windows の書き込み系 API を有効にしない決まり。#947 のオーナー判断）。そのため、
//!   `sanitize_component`・`assert_within_root` で検証したパスを std の `OpenOptions` で開く。
//!   再解析ポイント（symlink・ジャンクション）は開いたハンドルの属性で拒否する。
//!   これは製品のハンドル相対書き込みの検証ではなく、ディスク上の分離の検証に限る。
//!   #948 の完了後に Windows の書き込み API が有効になったら、本アダプタを置き換える
//!
//! 呼び出し元: 各テストの `write_entry`・`write_once`・`write_in_place`。

use fandhe_browser_profile::{DataKind, Profile};
use std::ffi::OsStr;
use std::fs::File;

/// `profile` の `kind` ディレクトリ配下の `name` を、読み書き・作成ありで開く
/// （切り詰めない。`create_file_in` と同じ契約）。失敗は呼び出し側が panic させる。
#[cfg(unix)]
pub fn open_data_file(profile: &Profile, kind: DataKind, name: &OsStr) -> Result<File, String> {
    profile
        .create_file_in(kind, name)
        .map_err(|e| format!("create_file_in({kind:?}, {name:?}): {e}"))
}

/// Windows 版。理由と限界はモジュール doc 参照。
#[cfg(windows)]
pub fn open_data_file(profile: &Profile, kind: DataKind, name: &OsStr) -> Result<File, String> {
    use fandhe_browser_profile::{assert_within_root, sanitize_component};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    /// `FILE_FLAG_OPEN_REPARSE_POINT`: 再解析ポイントを辿らず、それ自体を開く。
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    /// `FILE_ATTRIBUTE_REPARSE_POINT`。
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    sanitize_component(name).map_err(|e| format!("sanitize_component({name:?}): {e}"))?;
    let path = profile.data_dir(kind).join(name);
    assert_within_root(profile.root(), &path)
        .map_err(|e| format!("assert_within_root({path:?}): {e}"))?;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|e| format!("open({path:?}): {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("metadata({path:?}): {e}"))?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !meta.is_file() {
        return Err(format!("{path:?} is a reparse point or not a regular file"));
    }
    Ok(file)
}
