//! Windows 限定: 親ディレクトリのハンドルと 1 要素の名前だけで、子のディレクトリ・ファイルを
//! 作成・open する関数と、Win32 / NT 呼び出しを置く唯一のモジュール。
//!
//! unix の `openat` + `O_NOFOLLOW` に相当する。Win32 の `CreateFileW` には親ハンドル相対の
//! 指定がないため、NT API の `NtCreateFile`（`OBJECT_ATTRIBUTES.RootDirectory` に親の
//! ハンドル、`ObjectName` に 1 要素の名前）を使う。API にパス文字列を渡さないので、
//! 親のパスが開いた後にジャンクションへ差し替えられても子は親の実体の下にだけできる。
//! 子が再解析ポイントなら `FILE_OPEN_REPARSE_POINT` でリンク自体を開き、属性で拒否する。
//!
//! ## 文脈
//!
//! 呼び出し元は `local_root.rs` の継ぎ目（`open_or_create_child_dir`・`open_or_create_child_file`）
//! で、`Profile::open`（Windows）のルートまでの各要素・`profile.lock`・データ種別ディレクトリの
//! 作成と open はすべて本モジュール経由（Issue #950）。既知フォルダ API による基準ディレクトリの
//! 取得（[`known_local_app_data`]。Issue #951）も本モジュールにあり、`local_root.rs`・`store.rs` が
//! 呼ぶ。DACL（#952・#953）の `unsafe` も本モジュールに置く。`SecurityDescriptor` は null（親から継承）で、設定経路は
//! #952 で足す。
//!
//! ## 承認
//!
//! `windows-sys` の直接依存と、`unsafe` を profile crate のこのモジュールに限ることは、
//! Issue #947 のオーナー判断（2026-10-12）による。この範囲を超える `unsafe` を他へ置かない。
//! 対応するビヘイビアは `PROF-1`・`PROF-4`・`XOS-10`（TASK-63 の前提・MS-5）。
//!
//! ## 限界（`REPAIR-3`）
//!
//! 保持中の空の親へ実際に `FSCTL_SET_REPARSE_POINT` を発行して競合を再現するテストは
//! 置いていない（`DeviceIoControl` 用の追加の `unsafe` と feature が要り、Issue #947 の決定の
//! 範囲外のため #950 でも追加しない）。代わりに、パスとハンドルを食い違わせる模擬テスト
//! （`local_root.rs`・`profile.rs`）で「作成がハンドルの実体の下にだけ行われる」ことを確かめる。
//! 親ハンドル相対の open は親のファイルオブジェクトを起点にするため、親の再解析ポイントは
//! 評価されない、という理解に基づく。実機の FSCTL では未検証。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::PathBuf;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_sys::core::PWSTR;

use crate::local_root::validate_single_component;
use crate::profile::ProfileError;

/// `IoStatusBlock.Information` の値: 新規に作成した（wdm.h の `FILE_CREATED`）。
/// `Win32_System_WindowsProgramming` の feature を増やさないためローカルに定義する。
const FILE_CREATED: usize = 2;
/// `FILE_ATTRIBUTE_REPARSE_POINT`（`GetFileAttributes` の再解析ポイント属性）。
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// `STATUS_FILE_IS_A_DIRECTORY`（ファイルとして開こうとした名前がディレクトリ）。
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BA_u32 as i32;
/// `STATUS_NOT_A_DIRECTORY`（ディレクトリとして開こうとした名前がファイル）。
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103_u32 as i32;

const REASON_REPARSE: &str =
    "entry is a reparse point (junction or symlink); refusing to follow or use it";
const REASON_NOT_DIR: &str = "entry exists but is not a directory";
const REASON_NOT_FILE: &str = "entry exists but is not a regular file";

/// 作成したか、既存を開いたか（`IoStatusBlock.Information` 由来）。
///
/// #952 が「新規に作ったルートにだけ DACL を設定する」判定に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildDisposition {
    /// 新規に作成した。
    Created,
    /// 既に存在したものを開いた。
    Opened,
}

/// 親ハンドル相対で開いた子。
///
/// `file` は改名・削除を共有しない（`DELETE` を共有しない）ハンドルで、保持している間は
/// 改名・削除による差し替えを防ぐ。
#[derive(Debug)]
pub(crate) struct OpenedChild {
    /// 開いたディレクトリ / ファイルのハンドル。
    pub(crate) file: File,
    /// 作成か既存の open か。
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Issue #952 で新規作成したルートにだけ DACL を設定する判定に使う"
        )
    )]
    pub(crate) disposition: ChildDisposition,
}

/// 親 `parent` の直下にディレクトリ `name` を作成する。既にあれば開く。
///
/// `name` は 1 要素の名前に限り（[`validate_single_component`] で検証）、再解析ポイントは
/// 辿らず拒否する。`InvalidLayout.path` は親からの相対名。呼び出し側で必要ならフルパスに補う。
/// `SecurityDescriptor` は null（親から継承）。
pub(crate) fn open_or_create_dir_at(
    parent: &File,
    name: &OsStr,
) -> Result<OpenedChild, ProfileError> {
    let child = nt_create_at(
        parent,
        name,
        FILE_GENERIC_READ | FILE_TRAVERSE,
        0,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    let meta = child.file.metadata().map_err(ProfileError::Io)?;
    if meta.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_layout(name, REASON_REPARSE));
    }
    if !meta.is_dir() {
        return Err(invalid_layout(name, REASON_NOT_DIR));
    }
    Ok(child)
}

/// 親 `parent` の直下にファイル `name` を読み書き可能で作成する。既にあれば開く。
///
/// 共有モードは READ・WRITE を共有し DELETE を共有しない（`profile.lock` の
/// `File::try_lock` に使える同期ハンドル）。そのほかは [`open_or_create_dir_at`] と同じ。
pub(crate) fn open_or_create_file_at(
    parent: &File,
    name: &OsStr,
) -> Result<OpenedChild, ProfileError> {
    let child = nt_create_at(
        parent,
        name,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_ATTRIBUTE_NORMAL,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    let meta = child.file.metadata().map_err(ProfileError::Io)?;
    if meta.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_layout(name, REASON_REPARSE));
    }
    if !meta.is_file() {
        return Err(invalid_layout(name, REASON_NOT_FILE));
    }
    Ok(child)
}

fn invalid_layout(name: &OsStr, reason: &'static str) -> ProfileError {
    ProfileError::InvalidLayout {
        path: PathBuf::from(name),
        reason,
    }
}

/// `NtCreateFile` を親ハンドル相対で呼ぶ。`unsafe` はここと [`ntstatus_to_error`] に集約する。
///
/// `OBJ_CASE_INSENSITIVE` を付ける（付けないと大文字小文字だけが異なる既存名を見落とし、
/// `FILE_OPEN_IF` が衝突する兄弟を作る。`XOS-9`）。`SYNCHRONIZE`（`FILE_GENERIC_READ` に含む）と
/// `FILE_SYNCHRONOUS_IO_NONALERT` の組で同期ハンドルにする。成功判定は `NT_SUCCESS`
/// （`status >= 0`）で、失敗はすべてエラーにする（fail-closed）。
fn nt_create_at(
    parent: &File,
    name: &OsStr,
    access: u32,
    attributes: u32,
    options: u32,
) -> Result<OpenedChild, ProfileError> {
    validate_single_component(name)?;
    let wide: Vec<u16> = name.encode_wide().collect();
    // 検証済みで 255 以下のため失敗しないが、外部入力経路なので `try_into` で明示的に扱う。
    let byte_len: u16 = wide
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or(ProfileError::InvalidComponent {
            reason: "name is too long",
        })?;
    let object_name = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: wide.as_ptr().cast_mut(),
    };
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: `object_attributes`・`object_name`・`wide`（`Buffer` の指す領域）・`io_status`・
    // `handle` はこの呼び出しが戻るまでスタック / ヒープ上で生存し、NtCreateFile はそれらを
    // 保持しない。`Length` は `size_of::<OBJECT_ATTRIBUTES>()`、`RootDirectory` は借用中の
    // `parent` が持つ有効なハンドル。`allocationsize`・`eabuffer` は null・0 で不使用。
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &object_attributes,
            &mut io_status,
            std::ptr::null(),
            attributes,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN_IF,
            options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(map_failure(status, name));
    }
    if handle.is_null() {
        return Err(ProfileError::Io(std::io::Error::other(
            "NtCreateFile succeeded without returning a handle",
        )));
    }
    // SAFETY: 成功した NtCreateFile が返した直後の有効なハンドルで、他に所有者がいない。
    // ここで `File` に所有権を移し、以後は `File` の drop だけが閉じる。
    let file = unsafe { File::from_raw_handle(handle as _) };
    let disposition = if io_status.Information == FILE_CREATED {
        ChildDisposition::Created
    } else {
        ChildDisposition::Opened
    };
    Ok(OpenedChild { file, disposition })
}

/// 失敗した NTSTATUS を `ProfileError` に写す。
///
/// ディレクトリ / ファイルの種別不一致は `InvalidLayout`、それ以外は Win32 エラーへ変換して
/// `Io` にする（`NotFound`・`AlreadyExists` 等のエラーの種別を判別できるよう意味を残す。
/// `FILE_OPEN_IF` により作成の競合の再試行は不要）。
fn map_failure(status: i32, name: &OsStr) -> ProfileError {
    match status {
        STATUS_FILE_IS_A_DIRECTORY => invalid_layout(name, REASON_NOT_FILE),
        STATUS_NOT_A_DIRECTORY => invalid_layout(name, REASON_NOT_DIR),
        _ => ProfileError::Io(ntstatus_to_error(status)),
    }
}

fn ntstatus_to_error(status: i32) -> std::io::Error {
    // SAFETY: ポインタ引数を持たない純粋な変換関数で、任意の NTSTATUS 値を渡してよい。
    let code = unsafe { RtlNtStatusToDosError(status) };
    std::io::Error::from_raw_os_error(code as i32)
}

// ---- 既知フォルダ API（Issue #951） ----

/// 既知フォルダを取得できなかった場合の拒否理由（パスは含めない）。
const REASON_KNOWN_FOLDER_UNAVAILABLE: &str = "the Local AppData known folder (FOLDERID_LocalAppData) could not be resolved; refusing to use an unverified base directory (XOS-7, XOS-10)";
/// 既知フォルダのパス長の上限（UTF-16 要素数。`\\?\` 付きの最大長 32,767 に合わせる）。
const KNOWN_FOLDER_MAX_UNITS: usize = 32_767;

/// `SHGetKnownFolderPath` が返した文字列を `CoTaskMemFree` で解放するガード。
///
/// API の契約では、成否にかかわらず（失敗時に非 null が入り得るため）返ったポインタを
/// 解放する必要がある。HRESULT の判定より前に作り、早期 return でも漏らさない。
struct CoTaskMemString(PWSTR);

impl Drop for CoTaskMemString {
    fn drop(&mut self) {
        // SAFETY: `self.0` は `SHGetKnownFolderPath` が COM タスクアロケータで確保した
        // ポインタ、または null（初期値）。`CoTaskMemFree` は null を許容し、ガードは
        // 所有権を 1 つだけ持つので解放は drop の 1 回に限られる。
        unsafe { CoTaskMemFree(self.0.cast_const().cast()) };
    }
}

/// 既知フォルダ `FOLDERID_LocalAppData` の実パスを OS から取得する。
///
/// `Profile::open`（`profile.rs`）の基準ディレクトリと、既定ルート（`store.rs` の
/// `OsDefaultStore`。`XOS-7`）の起点に使う。環境変数 `LOCALAPPDATA` は見ない。プロセスの
/// 環境ブロックを書き換えて基準を共有場所へ向けることを防ぐため（Issue #951・`PROF-6`・
/// `XOS-10`）。ただし値の出所はレジストリ（`User Shell Folders`）で、同じユーザーは
/// 書き換えられる（`local_root.rs` の「限界」）。取得できない場合は
/// [`ProfileError::Unsupported`]（fail-closed。パスはエラーに含めない）。
///
/// `SHGetKnownFolderPath`・`CoTaskMemFree` は COM の初期化（`CoInitialize`）を必要としない。
pub(crate) fn known_local_app_data() -> Result<PathBuf, ProfileError> {
    let unavailable = || ProfileError::Unsupported {
        reason: REASON_KNOWN_FOLDER_UNAVAILABLE,
    };
    let mut raw: PWSTR = std::ptr::null_mut();
    // SAFETY: `FOLDERID_LocalAppData` は呼び出しの間生存する定数、`hToken` の null は現在の
    // ユーザー、`&mut raw` は書き込み可能な有効なポインタ。`KF_FLAG_DEFAULT` は非負の定数で
    // `u32` へ損失なく変換できる。
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    // 成否にかかわらず解放が必要（上記）。判定より前にガードへ渡す。
    let guard = CoTaskMemString(raw);
    if hr < 0 || guard.0.is_null() {
        return Err(unavailable());
    }
    let mut len = 0usize;
    // SAFETY: 成功時の `guard.0` は NUL 終端の UTF-16 文字列で、`guard` の drop まで有効。
    // 走査は上限 `KNOWN_FOLDER_MAX_UNITS` 要素までに限り、終端が見つからなければ読むのをやめる。
    while len <= KNOWN_FOLDER_MAX_UNITS && unsafe { *guard.0.add(len) } != 0 {
        len += 1;
    }
    if len == 0 || len > KNOWN_FOLDER_MAX_UNITS {
        return Err(unavailable());
    }
    // SAFETY: 上の走査で `guard.0` から `len` 要素が読めることを確認済み（NUL の手前まで）。
    // 読んだ内容は `OsString` へ複製し、ガードの解放前に使い終える。
    let wide = unsafe { std::slice::from_raw_parts(guard.0, len) };
    Ok(PathBuf::from(OsString::from_wide(wide)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    /// `canonicalize` 済みの一意な一時ディレクトリ。drop 時に再帰削除する。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::fs::canonicalize(std::env::temp_dir()).expect("temp_dir");
            let path = base.join(format!("fandhe-win32-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn sub(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::create_dir_all(&p).expect("create sub dir");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // ジャンクションは先に外す（remove_dir_all が先を辿らないよう念のため）。
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 親を `local_root` と同じオプション（DELETE を共有しない）で開く。
    fn open_parent(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .expect("open parent")
    }

    fn strip_verbatim(p: &Path) -> String {
        let s = p.to_string_lossy().into_owned();
        s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s)
    }

    fn mklink_junction(link: &Path, target: &Path) {
        let out = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(strip_verbatim(link))
            .arg(strip_verbatim(target))
            .output()
            .expect("run mklink");
        assert!(out.status.success(), "mklink failed: {out:?}");
    }

    fn entry_count(dir: &Path) -> usize {
        std::fs::read_dir(dir).expect("read_dir").count()
    }

    fn is_invalid_component<T: std::fmt::Debug>(r: &Result<T, ProfileError>) -> bool {
        matches!(r, Err(ProfileError::InvalidComponent { .. }))
    }

    fn is_invalid_layout<T: std::fmt::Debug>(r: &Result<T, ProfileError>) -> bool {
        matches!(r, Err(ProfileError::InvalidLayout { .. }))
    }

    #[test]
    fn prof1_xos10_creates_then_opens_existing_dir_and_file() {
        let t = TempDir::new();
        let p = t.sub("p");
        let parent = open_parent(&p);

        let d = open_or_create_dir_at(&parent, OsStr::new("d")).expect("create dir");
        assert_eq!(d.disposition, ChildDisposition::Created);
        assert!(p.join("d").is_dir());
        drop(d);
        let d = open_or_create_dir_at(&parent, OsStr::new("d")).expect("open dir");
        assert_eq!(d.disposition, ChildDisposition::Opened);

        let f = open_or_create_file_at(&parent, OsStr::new("profile.lock")).expect("create file");
        assert_eq!(f.disposition, ChildDisposition::Created);
        assert!(p.join("profile.lock").is_file());
        drop(f);
        let f = open_or_create_file_at(&parent, OsStr::new("profile.lock")).expect("open file");
        assert_eq!(f.disposition, ChildDisposition::Opened);
        // 同期ハンドルであること（advisory lock が取れる）。
        f.file.try_lock().expect("try_lock on returned handle");
    }

    #[test]
    fn prof1_xos10_held_parent_cannot_be_swapped_and_children_stay_inside() {
        let t = TempDir::new();
        let p = t.sub("p");
        let outside = t.sub("outside");
        let parent = open_parent(&p);

        // 親を開いている間は、改名・削除によるジャンクションへの差し替えができない。
        let p2 = t.0.join("p2");
        assert!(std::fs::rename(&p, &p2).is_err(), "rename must fail");
        assert!(std::fs::remove_dir(&p).is_err(), "remove must fail");

        let d = open_or_create_dir_at(&parent, OsStr::new("d")).expect("create dir");
        let f = open_or_create_file_at(&parent, OsStr::new("f")).expect("create file");
        drop((d, f));
        assert!(p.join("d").is_dir());
        assert!(p.join("f").is_file());
        assert_eq!(entry_count(&outside), 0);
    }

    #[test]
    fn prof1_xos10_reparse_point_child_is_rejected_without_following() {
        let t = TempDir::new();
        let p = t.sub("p");
        let outside = t.sub("outside");
        let parent = open_parent(&p);

        // 空の子を作り、削除して同名のジャンクションに置き換える（空のディレクトリの再解析ポイント化）。
        drop(open_or_create_dir_at(&parent, OsStr::new("c")).expect("create dir"));
        std::fs::remove_dir(p.join("c")).expect("remove empty child");
        mklink_junction(&p.join("c"), &outside);

        let r = open_or_create_dir_at(&parent, OsStr::new("c"));
        assert!(is_invalid_layout(&r), "dir: {r:?}");
        let r = open_or_create_file_at(&parent, OsStr::new("c"));
        assert!(is_invalid_layout(&r), "file: {r:?}");
        assert_eq!(entry_count(&outside), 0);
    }

    #[test]
    fn prof1_xos10_returned_handle_denies_rename_and_delete_until_dropped() {
        let t = TempDir::new();
        let p = t.sub("p");
        let parent = open_parent(&p);

        let d = open_or_create_dir_at(&parent, OsStr::new("d")).expect("create dir");
        assert!(std::fs::rename(p.join("d"), p.join("d2")).is_err());
        assert!(std::fs::remove_dir(p.join("d")).is_err());
        drop(d);
        std::fs::rename(p.join("d"), p.join("d2")).expect("rename after drop");

        let f = open_or_create_file_at(&parent, OsStr::new("f")).expect("create file");
        assert!(std::fs::rename(p.join("f"), p.join("f2")).is_err());
        assert!(std::fs::remove_file(p.join("f")).is_err());
        drop(f);
        std::fs::remove_file(p.join("f")).expect("remove after drop");
    }

    #[test]
    fn prof4_xos10_invalid_names_are_rejected_and_nothing_is_created() {
        let t = TempDir::new();
        let p = t.sub("p");
        let parent = open_parent(&p);

        for name in [
            "", ".", "..", r"a\b", "a/b", "a:b", "CON", "x.", "x ", "a\0b",
        ] {
            let r = open_or_create_dir_at(&parent, OsStr::new(name));
            assert!(is_invalid_component(&r), "dir {name:?}: {r:?}");
            let r = open_or_create_file_at(&parent, OsStr::new(name));
            assert!(is_invalid_component(&r), "file {name:?}: {r:?}");
        }
        assert_eq!(entry_count(&p), 0);
    }

    #[test]
    fn xos9_case_insensitive_open_does_not_create_sibling() {
        let t = TempDir::new();
        let p = t.sub("p");
        let parent = open_parent(&p);

        drop(open_or_create_dir_at(&parent, OsStr::new("Dir")).expect("create dir"));
        let again = open_or_create_dir_at(&parent, OsStr::new("dir")).expect("reopen dir");
        assert_eq!(again.disposition, ChildDisposition::Opened);
        assert_eq!(entry_count(&p), 1);
    }

    #[test]
    fn prof1_kind_mismatch_is_invalid_layout() {
        let t = TempDir::new();
        let p = t.sub("p");
        let parent = open_parent(&p);

        drop(open_or_create_dir_at(&parent, OsStr::new("d")).expect("create dir"));
        drop(open_or_create_file_at(&parent, OsStr::new("f")).expect("create file"));
        assert!(is_invalid_layout(&open_or_create_file_at(
            &parent,
            OsStr::new("d")
        )));
        assert!(is_invalid_layout(&open_or_create_dir_at(
            &parent,
            OsStr::new("f")
        )));
    }

    /// XOS-7・PROF-6（Windows）: 既知フォルダは絶対パスで、実体は `...\AppData\Local`。
    /// CI の通常環境では環境変数 `LOCALAPPDATA` の実体と一致する（対応の健全性確認）。
    #[test]
    fn xos7_known_local_app_data_is_absolute_and_ends_with_app_data_local() {
        let kf = known_local_app_data().expect("known folder");
        assert!(kf.is_absolute(), "{kf:?}");
        let real = std::fs::canonicalize(&kf).expect("canonicalize known folder");
        let tail: Vec<_> = real
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_ascii_lowercase())
            .collect();
        assert_eq!(tail, [OsString::from("local"), OsString::from("appdata")]);
        if let Some(env) = std::env::var_os("LOCALAPPDATA") {
            assert_eq!(std::fs::canonicalize(env).expect("env canonicalize"), real);
        }
    }
}
