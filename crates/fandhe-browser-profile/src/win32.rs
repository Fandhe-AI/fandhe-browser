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
//! 作成と open はすべて本モジュール経由（Issue #950）。`Profile::create_file_in`（Windows）の
//! データファイルの open も `local_root::open_data_file` を介して本モジュールの
//! `open_or_create_file_at` を使う（Issue #1313）。既知フォルダ API による基準ディレクトリの
//! 取得（[`known_local_app_data`]。Issue #951）も本モジュールにあり、`local_root.rs`・`store.rs` が
//! 呼ぶ。DACL（#952・#953）の `unsafe` も本モジュールに置く。既存ルートを開く場合は所有者と DACL を
//! ハンドルから読んで検証する（#953）。通常の子の `SecurityDescriptor` は
//! null（親から継承）で、プロファイルのルートと中間ディレクトリ（#1098）だけ [`open_or_create_protected_dir_at`] が作成時に
//! 保護付き DACL（本人・SYSTEM・Administrators のみ。Administrators と SYSTEM は読める）を渡し、
//! 新規作成時は作成直後に読み戻して検証する（Issue #952）。
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

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, HLOCAL, LocalFree, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, INHERITED_ACE, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, GetFileInformationByHandle,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_sys::core::PWSTR;

use crate::local_root::{
    ProtectedDirRole, REASON_BAD_DACL, reason_for_role, validate_single_component,
};
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
/// [`open_or_create_protected_dir_at`] が「新規に作ったルート・中間ディレクトリだけ DACL を読み戻して検証する」
/// 判定に使う。
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
    open_dir_with_sd(parent, name, std::ptr::null())
}

/// [`open_or_create_dir_at`] の本体。`security_descriptor` は新規作成時にだけ使われる
/// （`NtCreateFile` は既存を開いた場合は無視する）。
fn open_dir_with_sd(
    parent: &File,
    name: &OsStr,
    security_descriptor: *const c_void,
) -> Result<OpenedChild, ProfileError> {
    let child = nt_create_at(
        parent,
        name,
        FILE_GENERIC_READ | FILE_TRAVERSE,
        0,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        security_descriptor,
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
        std::ptr::null(),
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

/// データファイル `name` を [`open_or_create_file_at`] で開き、開いたハンドルで検証して返す
/// （`Profile::create_file_in`〔Windows〕用。Issue #1313・`PROF-1`・`PROF-6`）。
///
/// 検証（満たさなければ `InvalidLayout`。ファイルは返さない・削除もしない）:
/// - ハードリンク数が 1（データディレクトリの外の実体への別名を拒否する）
/// - 所有者と DACL が許可リスト（本人・SYSTEM・Administrators）の内側
///   （[`verify_existing_root_security`] と同じ判定。ACL は書き換えない）
pub(crate) fn open_or_create_data_file_at(
    parent: &File,
    name: &OsStr,
) -> Result<File, ProfileError> {
    let opened = open_or_create_file_at(parent, name)?;
    if link_count(&opened.file)? != 1 {
        return Err(invalid_layout(
            name,
            crate::local_root::REASON_DATA_FILE_LINKS,
        ));
    }
    verify_existing_root_security(&opened.file).map_err(|err| match err {
        ProfileError::InvalidLayout { reason, .. } => {
            let reason = if reason == crate::local_root::REASON_EXISTING_ROOT_OWNER {
                crate::local_root::REASON_DATA_FILE_OWNER
            } else if reason == crate::local_root::REASON_EXISTING_ROOT_DACL {
                crate::local_root::REASON_DATA_FILE_DACL
            } else {
                reason
            };
            invalid_layout(name, reason)
        }
        other => other,
    })?;
    Ok(opened.file)
}

/// ハンドルのハードリンク数（`GetFileInformationByHandle` の `nNumberOfLinks`）。
fn link_count(file: &File) -> Result<u32, ProfileError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` は有効なハンドルで、`info` は書き込み可能な構造体。
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(last_os_error());
    }
    Ok(info.nNumberOfLinks)
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
    security_descriptor: *const c_void,
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
        SecurityDescriptor: security_descriptor.cast(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: `object_attributes`・`object_name`・`wide`（`Buffer` の指す領域）・`io_status`・
    // `handle` はこの呼び出しが戻るまでスタック / ヒープ上で生存し、NtCreateFile はそれらを
    // 保持しない。`Length` は `size_of::<OBJECT_ATTRIBUTES>()`、`RootDirectory` は借用中の
    // `parent` が持つ有効なハンドル。`security_descriptor` は null か、呼び出し側が
    // この呼び出しの間生存させる自己相対 SD（[`ProtectedSd`]）。`allocationsize`・`eabuffer` は null・0 で不使用。
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

// ---- DACL（Issue #952） ----

/// `ACCESS_ALLOWED_ACE_TYPE`（`Win32_System_SystemServices` の feature を増やさないためローカルに定義）。
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// SYSTEM の SID 文字列。
const SID_SYSTEM: &str = "S-1-5-18";
/// Administrators の SID 文字列。
const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
/// `GetTokenInformation(TokenUser)` が返すサイズの上限（SID は最大 68 バイト程度）。
const MAX_TOKEN_USER_BYTES: u32 = 1024;
/// SID 文字列の最大文字数（`S-1-5-21-...` でも 200 文字未満）。
const MAX_SID_STRING_CHARS: usize = 256;
/// 検査する ACE 数の上限（新規ルートは 3 件のはず。巨大な ACL で長く回らないため）。
const MAX_ACE_COUNT: u32 = 64;

const REASON_NO_USER_SID: &str =
    "could not determine the current user SID for the profile root DACL";

/// `LocalFree` で解放する OS 確保のバッファ（`ConvertSidToStringSidW`・
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`・`GetSecurityInfo` が返す）。
struct LocalMem(*mut c_void);

impl Drop for LocalMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 本型は OS が LocalAlloc 系で確保して返したポインタだけを所有し、
            // 所有者は 1 つで、ここで 1 回だけ解放する。
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

fn last_os_error() -> ProfileError {
    ProfileError::Io(std::io::Error::last_os_error())
}

/// 既存ルート検証（#953）の読み取り失敗を、既存ルート向けの理由で `InvalidLayout` にする。
fn existing_root_err(reason: &'static str) -> ProfileError {
    ProfileError::InvalidLayout {
        path: PathBuf::new(),
        reason,
    }
}

fn bad_dacl() -> ProfileError {
    ProfileError::InvalidLayout {
        path: PathBuf::new(),
        reason: REASON_BAD_DACL,
    }
}

/// SID（`PSID`）を `S-1-...` 形式の文字列にする。
///
/// # Safety
/// `sid` は有効な SID を指し、呼び出しの間生存すること。
unsafe fn sid_to_string(sid: *mut c_void) -> Result<String, ProfileError> {
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: 呼び出し元が `sid` の有効性を保証する。`raw` は有効な出力先。
    if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 {
        return Err(last_os_error());
    }
    let _guard = LocalMem(raw.cast());
    let mut units: Vec<u16> = Vec::new();
    for i in 0..MAX_SID_STRING_CHARS {
        // SAFETY: OS が返した NUL 終端文字列を、上限 `MAX_SID_STRING_CHARS` 内で NUL まで読む。
        let unit = unsafe { *raw.add(i) };
        if unit == 0 {
            return String::from_utf16(&units).map_err(|_| bad_dacl());
        }
        units.push(unit);
    }
    Err(bad_dacl())
}

/// 現在のプロセスのトークンのユーザー SID を文字列で返す。
fn current_user_sid_string() -> Result<String, ProfileError> {
    Ok(current_token_sids()?.0)
}

/// 現在のプロセスのトークンの `TokenUser` と `TokenOwner` の SID 文字列 `(user, owner)` を返す。
///
/// 昇格トークンでは新規オブジェクトの既定の所有者（`TokenOwner`）が Administrators になるため、
/// 既存ルートの所有者検証（#953）は両方を許可する。
fn current_token_sids() -> Result<(String, String), ProfileError> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` は擬似ハンドルを返す。`token` は有効な出力先。
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_os_error());
    }
    // SAFETY: 成功した `OpenProcessToken` が返した、他に所有者のいないハンドル。
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };
    let user_buf = read_token_info(&token, TokenUser)?;
    // SAFETY: 成功した呼び出しが `user_buf` の先頭に TOKEN_USER（Sid は buf 内を指す）を書いた。
    // `user_buf` は 8 バイト整列。
    let user_sid = unsafe { (*user_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: `user_sid` は `user_buf` 内の有効な SID で、`user_buf` はこの呼び出しの間生存する。
    let user = unsafe { sid_to_string(user_sid) }?;
    let owner_buf = read_token_info(&token, TokenOwner)?;
    // SAFETY: 成功した呼び出しが `owner_buf` の先頭に TOKEN_OWNER（Owner は buf 内を指す）を書いた。
    let owner_sid = unsafe { (*owner_buf.as_ptr().cast::<TOKEN_OWNER>()).Owner };
    // SAFETY: `owner_sid` は `owner_buf` 内の有効な SID で、`owner_buf` はこの呼び出しの間生存する。
    let owner = unsafe { sid_to_string(owner_sid) }?;
    Ok((user, owner))
}

/// `GetTokenInformation(class)` の結果を 8 バイト整列のバッファで返す（サイズは上限検証つき）。
fn read_token_info(
    token: &OwnedHandle,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u64>, ProfileError> {
    let mut needed: u32 = 0;
    // SAFETY: サイズ取得の呼び出しで、バッファは null・長さ 0。
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            class,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 || needed > MAX_TOKEN_USER_BYTES {
        return Err(ProfileError::InvalidLayout {
            path: PathBuf::new(),
            reason: REASON_NO_USER_SID,
        });
    }
    // 8 バイト境界を保証するため u64 で確保する。
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` は `needed` バイト以上の書き込み可能な領域。
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            class,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_os_error());
    }
    Ok(buf)
}

/// プロファイルのルートに渡す保護付き DACL を持つ自己相対のセキュリティ記述子。
///
/// SDDL `D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;<本人>)`。`P` で親からの継承を切り、
/// `OICI` で配下（`profile.lock`・データ種別ディレクトリ）へ継承させる。SDDL に埋め込むのは
/// OS が返した SID 文字列だけで、外部入力は入れない。
struct ProtectedSd {
    sd: LocalMem,
    user_sid: String,
}

impl ProtectedSd {
    fn for_current_user() -> Result<Self, ProfileError> {
        let user_sid = current_user_sid_string()?;
        let sddl = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user_sid})");
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd: *mut c_void = std::ptr::null_mut();
        // SAFETY: `wide` は NUL 終端で呼び出しの間生存する。`sd` は有効な出力先で、サイズは不要（null）。
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_os_error());
        }
        Ok(Self {
            sd: LocalMem(sd),
            user_sid,
        })
    }

    fn as_ptr(&self) -> *const c_void {
        self.sd.0
    }
}

/// DACL の ACE 1 件の要約（検証とテスト用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AceSummary {
    /// ACE の種別（0 が ACCESS_ALLOWED）。
    pub(crate) ace_type: u8,
    /// 対象の SID 文字列（ACCESS_ALLOWED 以外は空）。
    pub(crate) sid: String,
    /// アクセスマスク（ACCESS_ALLOWED 以外は 0）。
    pub(crate) mask: u32,
    /// ACE フラグ（`INHERITED_ACE` 等）。
    pub(crate) flags: u8,
}

/// ハンドルの DACL の要約。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaclSummary {
    /// `SE_DACL_PROTECTED`（親からの継承を受けない）が立っているか。
    pub(crate) protected: bool,
    /// ACE の一覧（先頭から）。
    pub(crate) aces: Vec<AceSummary>,
}

/// `file` のハンドルから DACL を読み戻して要約する（`READ_CONTROL`。`FILE_GENERIC_READ` に含む）。
///
/// DACL が無い（null）場合は全員に許可なので `InvalidLayout`。新規ルートの検証と、
/// 既存ルートの検証（#953。[`read_owner_and_dacl`] 経由）から使う。
///
/// 返す要約は SID・種別・継承フラグ・protected のみで、アクセスマスクの妥当性と本人 ACE の
/// 存在は判定しない。既存ルートの判定（`local_root::check_existing_root_security`）は、
/// マスクを見ずに許可外の主体への許可 ACE をすべて拒否することで答えている。
pub(crate) fn read_dacl(file: &File) -> Result<DaclSummary, ProfileError> {
    read_owner_and_dacl(file, false).map(|(_, dacl)| dacl)
}

/// `file` のハンドルから所有者 SID（文字列）と DACL を 1 回の呼び出しで読む。
///
/// `want_owner` が偽なら所有者は読まず `None`。真で所有者 SID が取れない（null）場合は
/// `InvalidLayout`（fail-closed）。既存ルートの検証（#953）が使う。
fn read_owner_and_dacl(
    file: &File,
    want_owner: bool,
) -> Result<(Option<String>, DaclSummary), ProfileError> {
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    let mut owner: *mut c_void = std::ptr::null_mut();
    let mut info = DACL_SECURITY_INFORMATION;
    if want_owner {
        info |= OWNER_SECURITY_INFORMATION;
    }
    // SAFETY: `file` は有効なハンドル。出力先は有効で、不要な出力は null。
    let err = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            info,
            if want_owner {
                std::ptr::addr_of_mut!(owner)
            } else {
                std::ptr::null_mut()
            },
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(ProfileError::Io(std::io::Error::from_raw_os_error(
            err as i32,
        )));
    }
    // `dacl` は `sd` の内部を指すため、`_sd` が生きている間だけ使う。
    let _sd = LocalMem(sd);
    // 既存ルート検証（`want_owner`）では、構造上の失敗を既存ルート向けの理由で返す（#953）。
    let dacl_err = || {
        if want_owner {
            existing_root_err(crate::local_root::REASON_EXISTING_ROOT_DACL)
        } else {
            bad_dacl()
        }
    };
    if dacl.is_null() || sd.is_null() {
        return Err(dacl_err());
    }
    let owner_sid = if want_owner {
        if owner.is_null() {
            return Err(existing_root_err(
                crate::local_root::REASON_EXISTING_ROOT_OWNER,
            ));
        }
        // SAFETY: `owner` は `sd` 内の有効な SID で、`_sd` が生きている間だけ使う。
        Some(
            unsafe { sid_to_string(owner) }
                .map_err(|_| existing_root_err(crate::local_root::REASON_EXISTING_ROOT_OWNER))?,
        )
    } else {
        None
    };
    let mut control: u16 = 0;
    let mut revision: u32 = 0;
    // SAFETY: `sd` は GetSecurityInfo が返した有効な SD。
    if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
        return Err(last_os_error());
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` は有効な ACL。`info` は `size_of` バイトの書き込み可能な領域。
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(last_os_error());
    }
    if info.AceCount > MAX_ACE_COUNT {
        return Err(dacl_err());
    }
    let mut aces = Vec::new();
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `dacl` は有効で、`index` は AceCount 未満。
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
            return Err(last_os_error());
        }
        // SAFETY: GetAce が返した ACE はヘッダ（4 バイト）で始まり、`sd` の生存中は有効。
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let (sid, mask) = if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
            // SAFETY: ACCESS_ALLOWED_ACE は `Mask` と、その直後の `SidStart` から始まる SID を持つ。
            let (sid_ptr, mask) = unsafe {
                let typed = ace.cast::<ACCESS_ALLOWED_ACE>();
                (
                    std::ptr::addr_of_mut!((*typed).SidStart).cast::<c_void>(),
                    (*typed).Mask,
                )
            };
            // SAFETY: `sid_ptr` は上記の ACE 内の有効な SID。
            (unsafe { sid_to_string(sid_ptr) }?, mask)
        } else {
            (String::new(), 0)
        };
        aces.push(AceSummary {
            ace_type: header.AceType,
            sid,
            mask,
            flags: header.AceFlags,
        });
    }
    Ok((
        owner_sid,
        DaclSummary {
            protected: control & SE_DACL_PROTECTED != 0,
            aces,
        },
    ))
}

/// 新規作成したルートの DACL が期待どおり（protected・継承 ACE なし・ACCESS_ALLOWED の
/// 本人 / SYSTEM / Administrators のみ）かを検証する。満たさなければ `InvalidLayout`（fail-closed）。
///
/// 検証失敗時も作成済みディレクトリは削除しない（モジュールの方針）。再実行は
/// `ChildDisposition::Opened` となり、新規用ではなく既存ルート用の検証を受ける。SD は作成時にアトミックに付くため
/// 実害は読み戻しの一時エラー程度で、再実行時の既存ルートの検証は
/// [`verify_existing_root_security`] が行う（#953）。
fn verify_protected_dacl(file: &File, user_sid: &str) -> Result<(), ProfileError> {
    let summary = read_dacl(file)?;
    let expected = |sid: &str| sid == user_sid || sid == SID_SYSTEM || sid == SID_ADMINISTRATORS;
    let ok = summary.protected
        && !summary.aces.is_empty()
        && summary.aces.iter().all(|ace| {
            ace.ace_type == ACCESS_ALLOWED_ACE_TYPE
                && u32::from(ace.flags) & INHERITED_ACE == 0
                && expected(&ace.sid)
        });
    if ok { Ok(()) } else { Err(bad_dacl()) }
}

/// テスト用: 現在のユーザー SID で [`verify_protected_dacl`] を呼ぶ。
#[cfg(test)]
fn verify_protected_dacl_for_test(file: &File) -> Result<(), ProfileError> {
    verify_protected_dacl(file, &current_user_sid_string()?)
}

/// 親 `parent` の直下にプロファイルのルートまたは中間ディレクトリ `name`（`role`）を、保護付き
/// DACL で作成する。既にあれば開く（Issue #1098 で中間ディレクトリにも適用）。
///
/// 新規作成のときだけ、`NtCreateFile` に [`ProtectedSd`] を渡して作成と DACL の付与を原子的に
/// 行う（緩い継承 ACL のままのディレクトリが存在する瞬間が無く、パスも再解決しない）。許可する
/// のは本人・SYSTEM・Administrators だけで、Administrators と SYSTEM は読める。作成直後に
/// ハンドルから DACL を読み戻して検証し、満たさなければ `Err`（作ったディレクトリは削除しない。
/// モジュールの方針）。既存のルートを開いた場合は ACL を書き換えず、ハンドルから所有者と DACL を
/// 読んで [`verify_existing_root_security`] で検証し、許可した主体の外に権限が出ていれば
/// `InvalidLayout`（Issue #953。拒否する理由は同関数）。ビヘイビアは `PROF-1`・`PROF-6`
/// （Issue #952・#953）。
pub(crate) fn open_or_create_protected_dir_at(
    parent: &File,
    name: &OsStr,
    role: ProtectedDirRole,
) -> Result<OpenedChild, ProfileError> {
    let sd = ProtectedSd::for_current_user()?;
    let child = open_dir_with_sd(parent, name, sd.as_ptr())?;
    let verified = match child.disposition {
        ChildDisposition::Created => verify_protected_dacl(&child.file, &sd.user_sid),
        ChildDisposition::Opened => verify_existing_root_security(&child.file),
    };
    // 検証エラーの path は空で返るため、拒否したディレクトリ自身の相対名を補う
    // （呼び出し側が親のパスと連結してフルパスにする）。理由文は役割に応じて写像する。
    verified.map_err(|err| match err {
        ProfileError::InvalidLayout { path, reason } => ProfileError::InvalidLayout {
            path: if path.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                path
            },
            reason: reason_for_role(role, reason),
        },
        other => other,
    })?;
    Ok(child)
}

/// 既存ルートの所有者と DACL を、ルートのハンドルから読んで検証する（Issue #953）。
///
/// 許可する主体は、現在のトークンの `TokenUser`・`TokenOwner`、SYSTEM、Administrators。
/// 判定そのものは `local_root::check_existing_root_security`（純粋関数。Linux でもテスト）。
///
/// **締め直さず拒否する**（unix の `open` は `fchmod(0700)` で締め直すのと異なる）。理由:
/// DACL の書き換えには `WRITE_DAC` が要り、継承 ACE を既存の子へ伝播させる処理はパスで配下を
/// たどり直す（パスを再解決しない方針に反する）。ハンドルだけの書き換えでは既存の子に緩い継承
/// ACE が残り、不完全な締め直しで成功を返すことになる（`REPAIR-3`）。緩い ACL の既存
/// ディレクトリは誤って指定された共有ディレクトリかもしれず、書き換えると他の利用者の
/// アクセスを奪う。ACL は変更せず、作ったものも削除しない。
fn verify_existing_root_security(file: &File) -> Result<(), ProfileError> {
    let (owner, dacl) = read_owner_and_dacl(file, true)?;
    let (user, token_owner) = current_token_sids()?;
    let allowed = [
        user.as_str(),
        token_owner.as_str(),
        SID_SYSTEM,
        SID_ADMINISTRATORS,
    ];
    let aces: Vec<(u8, &str)> = dacl
        .aces
        .iter()
        .map(|ace| (ace.ace_type, ace.sid.as_str()))
        .collect();
    crate::local_root::check_existing_root_security(owner.as_deref(), &aces, &allowed).map_err(
        |reason| ProfileError::InvalidLayout {
            path: PathBuf::new(),
            reason,
        },
    )
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

    const SID_EVERYONE: &str = "S-1-1-0";

    /// `parent` に Everyone への継承可能な許可 ACE を付ける（SID 指定でロケール非依存）。
    fn grant_everyone_inheritable(parent: &Path) {
        let out = Command::new("cmd")
            .args(["/C", "icacls"])
            .arg(strip_verbatim(parent))
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .output()
            .expect("run icacls");
        assert!(out.status.success(), "icacls failed: {out:?}");
    }

    fn sids(summary: &DaclSummary) -> Vec<String> {
        let mut v: Vec<String> = summary.aces.iter().map(|a| a.sid.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    fn expected_sids() -> Vec<String> {
        let mut v = vec![
            current_user_sid_string().expect("user sid"),
            SID_SYSTEM.to_owned(),
            SID_ADMINISTRATORS.to_owned(),
        ];
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn prof1_prof6_new_root_has_protected_dacl_under_permissive_parent() {
        let t = TempDir::new();
        let p = t.sub("p");
        grant_everyone_inheritable(&p);
        let parent = open_parent(&p);

        // 前提（空振り防止）: 通常の作成は親の Everyone の許可を継承する。
        std::fs::create_dir(p.join("control")).expect("control dir");
        let control = read_dacl(&open_parent(&p.join("control"))).expect("control dacl");
        assert!(
            control
                .aces
                .iter()
                .any(|a| a.sid == SID_EVERYONE && u32::from(a.flags) & INHERITED_ACE != 0),
            "control must inherit Everyone ACE: {control:?}"
        );

        let root =
            open_or_create_protected_dir_at(&parent, OsStr::new("root"), ProtectedDirRole::Root)
                .expect("root");
        assert_eq!(root.disposition, ChildDisposition::Created);
        let dacl = read_dacl(&root.file).expect("root dacl");
        assert!(dacl.protected, "{dacl:?}");
        assert!(
            dacl.aces
                .iter()
                .all(|a| u32::from(a.flags) & INHERITED_ACE == 0),
            "{dacl:?}"
        );
        assert!(!dacl.aces.iter().any(|a| a.sid == SID_EVERYONE), "{dacl:?}");
        assert_eq!(sids(&dacl), expected_sids());
        verify_protected_dacl_for_test(&root.file).expect("verify");

        // 配下はルートの DACL を継承し、Everyone を含まない。
        let lock = open_or_create_file_at(&root.file, OsStr::new("profile.lock")).expect("lock");
        let data = open_or_create_dir_at(&root.file, OsStr::new("cookies")).expect("data dir");
        for child in [&lock.file, &data.file] {
            let d = read_dacl(child).expect("child dacl");
            assert!(!d.aces.is_empty(), "{d:?}");
            assert!(
                d.aces
                    .iter()
                    .all(|a| u32::from(a.flags) & INHERITED_ACE != 0),
                "{d:?}"
            );
            assert!(!d.aces.iter().any(|a| a.sid == SID_EVERYONE), "{d:?}");
            assert_eq!(sids(&d), expected_sids());
        }
    }

    fn icacls(path: &Path, args: &[&str]) {
        let out = Command::new("cmd")
            .args(["/C", "icacls"])
            .arg(strip_verbatim(path))
            .args(args)
            .output()
            .expect("run icacls");
        assert!(out.status.success(), "icacls failed: {out:?}");
    }

    /// Issue #953: 親から Everyone の許可を継承した既存ルートは、ACL を書き換えずに拒否する。
    #[test]
    fn prof1_prof6_existing_root_with_inherited_everyone_is_rejected() {
        let t = TempDir::new();
        let p = t.sub("p");
        grant_everyone_inheritable(&p);
        std::fs::create_dir(p.join("root")).expect("pre-existing root");
        let parent = open_parent(&p);

        let r =
            open_or_create_protected_dir_at(&parent, OsStr::new("root"), ProtectedDirRole::Root);
        assert!(
            matches!(
                r,
                Err(ProfileError::InvalidLayout { reason, .. })
                    if reason == crate::local_root::REASON_EXISTING_ROOT_DACL
            ),
            "{r:?}"
        );
        // 書き換えていない（Everyone が残っている）。
        let probe = open_parent(&p.join("root"));
        let dacl = read_dacl(&probe).expect("dacl");
        assert!(
            dacl.aces.iter().any(|a| a.sid == SID_EVERYONE),
            "existing root must be left untouched: {dacl:?}"
        );
    }

    /// Issue #953: 継承でなく既存ルート自体に明示した Everyone の許可も拒否する。
    #[test]
    fn prof1_prof6_existing_root_with_explicit_everyone_is_rejected() {
        let t = TempDir::new();
        let p = t.sub("p");
        let root = p.join("root");
        std::fs::create_dir(&root).expect("pre-existing root");
        icacls(&root, &["/grant", "*S-1-1-0:F"]);
        let parent = open_parent(&p);

        let r =
            open_or_create_protected_dir_at(&parent, OsStr::new("root"), ProtectedDirRole::Root);
        assert!(
            matches!(
                r,
                Err(ProfileError::InvalidLayout { reason, .. })
                    if reason == crate::local_root::REASON_EXISTING_ROOT_DACL
            ),
            "{r:?}"
        );
    }

    /// Issue #953: 本人・SYSTEM・Administrators だけを継承する親の下の既存ルート
    /// （第 1 段階の継承 ACL のルート）は開ける。TEMP の ACL に頼らないよう親の ACL を固定する。
    #[test]
    fn prof1_prof6_existing_root_with_default_inherited_acl_is_opened() {
        let t = TempDir::new();
        let p = t.sub("p");
        let user = format!(
            "*{}:(OI)(CI)F",
            current_user_sid_string().expect("user sid")
        );
        icacls(
            &p,
            &[
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
                &user,
            ],
        );
        std::fs::create_dir(p.join("root")).expect("pre-existing root");
        let parent = open_parent(&p);

        let root =
            open_or_create_protected_dir_at(&parent, OsStr::new("root"), ProtectedDirRole::Root)
                .expect("open");
        assert_eq!(root.disposition, ChildDisposition::Opened);
    }

    /// Issue #953（回帰）: 自分が作ったルート（保護付き DACL・昇格時は所有者が Administrators）を
    /// 再び開くと通る。
    #[test]
    fn prof1_prof6_root_created_by_us_can_be_reopened() {
        let t = TempDir::new();
        let p = t.sub("p");
        let parent = open_parent(&p);

        let first =
            open_or_create_protected_dir_at(&parent, OsStr::new("root"), ProtectedDirRole::Root)
                .expect("create");
        assert_eq!(first.disposition, ChildDisposition::Created);
        drop(first);
        let second =
            open_or_create_protected_dir_at(&parent, OsStr::new("root"), ProtectedDirRole::Root)
                .expect("reopen");
        assert_eq!(second.disposition, ChildDisposition::Opened);
    }

    #[test]
    fn prof1_prof6_verify_rejects_unprotected_inherited_dacl() {
        let t = TempDir::new();
        let p = t.sub("p");
        grant_everyone_inheritable(&p);
        let parent = open_parent(&p);

        let plain = open_or_create_dir_at(&parent, OsStr::new("plain")).expect("plain dir");
        let r = verify_protected_dacl_for_test(&plain.file);
        assert!(is_invalid_layout(&r), "{r:?}");
    }

    /// 偽の `%LOCALAPPDATA%`（`<t>/AppData/Local`。作成済み）。基準自体は検証対象外。
    fn fake_lad(t: &TempDir) -> PathBuf {
        // `sub` は `AppData` までしか作らないため、`Local` も明示的に作成する。
        let lad = t.sub("AppData").join("Local");
        std::fs::create_dir_all(&lad).expect("create fake LOCALAPPDATA");
        lad
    }

    fn lad_env(lad: &Path) -> Option<OsString> {
        Some(lad.as_os_str().to_os_string())
    }

    /// Issue #1098: 緩い基準の下で新規に作った中間ディレクトリとルートは、すべて保護付き DACL になる。
    #[test]
    fn prof1_prof6_new_intermediates_get_protected_dacl_under_permissive_base() {
        let t = TempDir::new();
        let lad = fake_lad(&t);
        grant_everyone_inheritable(&lad);

        // 前提（空振り防止）: 通常の作成は基準の Everyone を継承する。
        std::fs::create_dir(lad.join("control")).expect("control");
        let control = read_dacl(&open_parent(&lad.join("control"))).expect("control dacl");
        assert!(
            control.aces.iter().any(|a| a.sid == SID_EVERYONE),
            "{control:?}"
        );

        let root = lad.join("a").join("b").join("root");
        let plan = crate::local_root::plan_root(&root, lad_env(&lad)).expect("plan");
        let _pinned = crate::local_root::pin_root(&plan).expect("pin");
        for dir in [
            lad.join("a"),
            lad.join("a").join("b"),
            lad.join("a").join("b").join("root"),
        ] {
            let dacl = read_dacl(&open_parent(&dir)).expect("dacl");
            assert!(dacl.protected, "{dir:?}: {dacl:?}");
            assert!(
                dacl.aces
                    .iter()
                    .all(|a| a.ace_type == ACCESS_ALLOWED_ACE_TYPE
                        && u32::from(a.flags) & INHERITED_ACE == 0),
                "{dir:?}: {dacl:?}"
            );
            assert_eq!(sids(&dacl), expected_sids(), "{dir:?}");
        }
    }

    /// Issue #1098: 明示的に Everyone を許可した既存の中間ディレクトリは拒否し、配下を作らず、
    /// ACL も書き換えない。
    #[test]
    fn prof1_prof6_existing_intermediate_with_explicit_everyone_is_rejected() {
        let t = TempDir::new();
        let lad = fake_lad(&t);
        let mid = lad.join("mid");
        std::fs::create_dir(&mid).expect("mid");
        icacls(&mid, &["/grant", "*S-1-1-0:F"]);

        let plan = crate::local_root::plan_root(&mid.join("root"), lad_env(&lad)).expect("plan");
        let r = crate::local_root::pin_root(&plan);
        assert!(
            matches!(
                &r,
                Err(ProfileError::InvalidLayout { path, reason })
                    if *reason == crate::local_root::REASON_EXISTING_INTERMEDIATE_DACL
                        && std::fs::canonicalize(path).ok() == std::fs::canonicalize(&mid).ok()
            ),
            "{r:?}"
        );
        assert_eq!(entry_count(&mid), 0, "nothing may be created under it");
        let dacl = read_dacl(&open_parent(&mid)).expect("dacl");
        assert!(dacl.aces.iter().any(|a| a.sid == SID_EVERYONE), "{dacl:?}");
    }

    /// Issue #1098: 基準から Everyone を継承した既存の中間ディレクトリも拒否する。
    #[test]
    fn prof1_prof6_existing_intermediate_with_inherited_everyone_is_rejected() {
        let t = TempDir::new();
        let lad = fake_lad(&t);
        grant_everyone_inheritable(&lad);
        let mid = lad.join("mid");
        std::fs::create_dir(&mid).expect("mid");

        let plan = crate::local_root::plan_root(&mid.join("root"), lad_env(&lad)).expect("plan");
        let r = crate::local_root::pin_root(&plan);
        assert!(
            matches!(
                &r,
                Err(ProfileError::InvalidLayout { reason, .. })
                    if *reason == crate::local_root::REASON_EXISTING_INTERMEDIATE_DACL
            ),
            "{r:?}"
        );
        assert_eq!(entry_count(&mid), 0);
    }

    /// Issue #1098（陽性対照）: 本人・SYSTEM・Administrators だけを継承する既存の中間ディレクトリは
    /// 開け、ACL は書き換えない。
    #[test]
    fn prof1_prof6_existing_intermediate_with_default_acl_is_opened() {
        let t = TempDir::new();
        let lad = fake_lad(&t);
        let user = format!(
            "*{}:(OI)(CI)F",
            current_user_sid_string().expect("user sid")
        );
        icacls(
            &lad,
            &[
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
                &user,
            ],
        );
        let mid = lad.join("mid");
        std::fs::create_dir(&mid).expect("mid");
        let before = read_dacl(&open_parent(&mid)).expect("dacl");

        let plan = crate::local_root::plan_root(&mid.join("root"), lad_env(&lad)).expect("plan");
        let _pinned = crate::local_root::pin_root(&plan).expect("pin");
        let after = read_dacl(&open_parent(&mid)).expect("dacl");
        assert_eq!(before, after, "existing intermediate must not be rewritten");
        assert!(!after.protected, "{after:?}");
    }

    /// Issue #1098: 役割が `Intermediate` のとき、拒否理由が中間ディレクトリ用に写像される。
    #[test]
    fn prof1_prof6_intermediate_role_maps_rejection_reason() {
        let t = TempDir::new();
        let p = t.sub("p");
        grant_everyone_inheritable(&p);
        std::fs::create_dir(p.join("mid")).expect("mid");
        let parent = open_parent(&p);

        let r = open_or_create_protected_dir_at(
            &parent,
            OsStr::new("mid"),
            ProtectedDirRole::Intermediate,
        );
        assert!(
            matches!(
                r,
                Err(ProfileError::InvalidLayout { reason, .. })
                    if reason == crate::local_root::REASON_EXISTING_INTERMEDIATE_DACL
            ),
            "{r:?}"
        );
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
