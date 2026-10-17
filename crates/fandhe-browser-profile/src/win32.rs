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
//! 作成と open はすべて本モジュール経由（Issue #950）。DACL（#952・#953）・既知フォルダ API（#951）の
//! `unsafe` も本モジュールに置く。通常の子の `SecurityDescriptor` は null（親から継承）で、
//! プロファイルのルートだけ [`open_or_create_protected_dir_at`] が作成時に保護付き DACL
//! （本人・SYSTEM・Administrators のみ。Administrators と SYSTEM は読める）を渡し、
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

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
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
    GetTokenInformation, INHERITED_ACE, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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
/// [`open_or_create_protected_dir_at`] が「新規に作ったルートだけ DACL を読み戻して検証する」
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

const REASON_BAD_DACL: &str = "newly created profile root does not have the expected protected DACL (owner, SYSTEM and Administrators only, no inherited entries)";
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
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` は擬似ハンドルを返す。`token` は有効な出力先。
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_os_error());
    }
    // SAFETY: 成功した `OpenProcessToken` が返した、他に所有者のいないハンドル。
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };
    let mut needed: u32 = 0;
    // SAFETY: サイズ取得の呼び出しで、バッファは null・長さ 0。
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
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
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_os_error());
    }
    // SAFETY: 成功した呼び出しが `buf` の先頭に TOKEN_USER（Sid は `buf` 内を指す）を書いた。
    // `buf` は 8 バイト整列。
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: `sid` は `buf` 内の有効な SID で、`buf` はこの呼び出しの間生存する。
    unsafe { sid_to_string(sid) }
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
/// 既存ルートを検証する #953 から使う。
pub(crate) fn read_dacl(file: &File) -> Result<DaclSummary, ProfileError> {
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: `file` は有効なハンドル。出力先は有効で、不要な出力は null。
    let err = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
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
    if dacl.is_null() || sd.is_null() {
        return Err(bad_dacl());
    }
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
        return Err(bad_dacl());
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
    Ok(DaclSummary {
        protected: control & SE_DACL_PROTECTED != 0,
        aces,
    })
}

/// 新規作成したルートの DACL が期待どおり（protected・継承 ACE なし・ACCESS_ALLOWED の
/// 本人 / SYSTEM / Administrators のみ）かを検証する。満たさなければ `InvalidLayout`（fail-closed）。
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

/// 親 `parent` の直下にプロファイルのルート `name` を、保護付き DACL で作成する。既にあれば開く。
///
/// 新規作成のときだけ、`NtCreateFile` に [`ProtectedSd`] を渡して作成と DACL の付与を原子的に
/// 行う（緩い継承 ACL のままのディレクトリが存在する瞬間が無く、パスも再解決しない）。許可する
/// のは本人・SYSTEM・Administrators だけで、Administrators と SYSTEM は読める。作成直後に
/// ハンドルから DACL を読み戻して検証し、満たさなければ `Err`（作ったディレクトリは削除しない。
/// モジュールの方針）。既存のルートを開いた場合は ACL を変えず検証もしない（所有者・DACL の
/// 検証は #953）。ビヘイビアは `PROF-1`・`PROF-6`（Issue #952）。
pub(crate) fn open_or_create_protected_dir_at(
    parent: &File,
    name: &OsStr,
) -> Result<OpenedChild, ProfileError> {
    let sd = ProtectedSd::for_current_user()?;
    let child = open_dir_with_sd(parent, name, sd.as_ptr())?;
    if child.disposition == ChildDisposition::Created {
        verify_protected_dacl(&child.file, &sd.user_sid)?;
    }
    Ok(child)
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

        let root = open_or_create_protected_dir_at(&parent, OsStr::new("root")).expect("root");
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

    #[test]
    fn prof1_prof6_existing_root_is_opened_without_rewriting_dacl() {
        let t = TempDir::new();
        let p = t.sub("p");
        grant_everyone_inheritable(&p);
        // 緩い ACL のまま先に作られた既存ルート（検証は #953 の範囲）。
        std::fs::create_dir(p.join("root")).expect("pre-existing root");
        let parent = open_parent(&p);

        let root = open_or_create_protected_dir_at(&parent, OsStr::new("root")).expect("open");
        assert_eq!(root.disposition, ChildDisposition::Opened);
        let dacl = read_dacl(&root.file).expect("dacl");
        assert!(
            dacl.aces.iter().any(|a| a.sid == SID_EVERYONE),
            "existing root must be left untouched: {dacl:?}"
        );
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
}
