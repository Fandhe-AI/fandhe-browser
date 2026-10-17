//! Windows 向け: プロファイルルートが `%LOCALAPPDATA%` 配下かを判定し、
//! ルート・`profile.lock`・データ種別ディレクトリを作る補助モジュール。
//!
//! `profile.rs` の `Profile::open`（Windows 分岐）から呼ばれる。ルートまでの各要素・
//! `profile.lock`・データ種別ディレクトリの作成と open は、親のハンドルを起点に 1 要素の名前
//! だけを渡す NT API（`win32` モジュール。Issue #949・#950）で行い、パス文字列を再解決しない。
//! 次の方針で境界を守る。
//!
//! - 作成前（[`plan_root`]）: ルートの絶対パスから、存在する最も深い祖先を `canonicalize`
//!   （短縮名・大文字小文字・`\\?\` 表記・symlink／ジャンクションを実体に解決）し、まだ存在しない
//!   残りの要素を足した「期待する実体パス」が、同じく `canonicalize` した
//!   `%LOCALAPPDATA%`（既知フォルダ `FOLDERID_LocalAppData`。下記）の**厳密な配下**であることをコンポーネント単位で確認する
//!   （文字列の前方一致ではない。`...\Local` と `...\LocalEvil` を取り違えない）。
//!   基準ディレクトリ自体も、実体が `...\AppData\Local` で終わり UNC でないことを要求する。
//! - 基準ディレクトリ: 環境変数 `LOCALAPPDATA` ではなく、既知フォルダ API
//!   （`win32::known_local_app_data`。Issue #951）で取得した値を使う。プロセスの環境ブロックを
//!   書き換えて基準を共有場所へ向けられない。
//! - 経路の固定（[`pin_root`]）: 基準ディレクトリだけはパスで開き（`canonicalize` 済み・再解析
//!   ポイントの検査あり）、ルートまでの各要素は親のハンドル相対で作成・open する
//!   （`FILE_OPEN_IF` で作成と open が原子的）。各ハンドルは `FILE_SHARE_DELETE` なしで保持され、
//!   保持している間、その要素の**改名・削除**は共有違反で失敗する。再解析ポイントは
//!   `FILE_OPEN_REPARSE_POINT` でリンク自体を開き、属性で検出して `InvalidLayout` にする。
//!   固定した後に `canonicalize` の結果も事後に再確認する（検出専用の [`verify_created_root`]）。
//! - 固定後: `profile.lock`（[`open_lock_file`]）とデータ種別ディレクトリ（[`ensure_data_dirs`]）も
//!   ルートのハンドル相対で作成・open する。ロックは常にルートの実体の `profile.lock` に取られる。
//!   `profile.lock` は `FILE_SHARE_DELETE` なしで開き、ロックの生存中は改名・削除による別実体への
//!   差し替えを防ぐ。ルートのハンドルは `Profile` が生存中保持し、途中の要素のハンドルは `open` の
//!   終了時に閉じる。検出時に、それまでに作ったディレクトリ・ファイルは削除しない（削除先が
//!   差し替え後の実体だと境界外を消しかねないため。unix 側も途中で作ったディレクトリを戻さない
//!   方針に揃える）。
//!
//! アクセス制限: Windows で新規に作るルートには、作成時に保護付き DACL（親からの継承を切り、
//! 本人・SYSTEM・Administrators だけを許可。配下へは継承）を `win32::open_or_create_protected_dir_at`
//! が渡し、作成直後に読み戻して検証する。失敗すれば `open` は失敗する（Issue #952）。
//! Administrators と SYSTEM は読める。`profile.lock`・データ種別ディレクトリはルートの DACL を
//! 継承する。中間ディレクトリは親から継承のまま（DACL を付けない）。既存ルートの所有者・DACL は
//! 検証しない（#953）。対応するビヘイビアは `PROF-1`・`PROF-4`・`PROF-6`・`XOS-7`・`XOS-10`
//! （Issue #928、#813、#950、#952）。
//!
//! ## 前提とする攻撃者像
//!
//! 同じユーザー権限で動く別プロセス。ただしその攻撃者は `%LOCALAPPDATA%` 配下を元々
//! 読み書きできるため、本モジュールが守るのは「別ユーザー・配下外への書き込み」のうち、
//! 次の範囲に限る。同一ユーザーからの読み取りは防がない。
//!
//! - 守れる: `open` の前から存在する、または競合なしで置かれた細工（既存のジャンクション・
//!   symlink、不正な名前、配下外の指定）。`plan_root` と、開く時点の属性検査で拒否する。
//!   固定中の要素・`profile.lock` の改名・削除による別実体への差し替え。
//!   同じユーザーの別プロセスが `open` の途中で空のディレクトリを再解析ポイント化する競合
//!   （設定して戻す場合を含む）: 子は親のハンドルの実体の下にだけでき、ロックもルートの実体の
//!   `profile.lock` に取られるため、配下外への作成と `PROF-1` の二重 open の拒否の破れは起きない
//!   （親のファイルオブジェクトを起点にした open は親の再解析ポイントを評価しない、という
//!   理解に基づく。パスとハンドルを食い違わせる模擬テストで確認し、実機の FSCTL では未検証）。
//!
//! ## 限界（`REPAIR-3`。実装済みを装わない）
//!
//! - 起点は既知フォルダ API で取得した実体で、パスで開く（`canonicalize` 済み・再解析ポイントの
//!   検査あり）。それより上の祖先は保持しない（子のハンドルが開いている間は祖先の改名も失敗すると
//!   期待しているが、未確認〔Windows で要確認〕）。
//! - 固定後に要素が再解析ポイント化された場合、作成はハンドル相対で配下に留まる。
//!   [`verify_created_root`] の `canonicalize` が食い違いを検出すると `InvalidLayout` になり、
//!   作ったものは配下に残る。実機の FSCTL では未検証。
//! - 作成直後で空のデータ種別ディレクトリは再解析ポイント化され得る。ただし Windows の
//!   `Profile` にはパス基準の書き込み API が無く、本モジュールの範囲では配下外への書き込みに
//!   ならない。書き込み系を Windows で有効にする際（#813 の第 2 段階）はハンドル基準が必須。
//! - `open` の成功後に保持するのはルートのハンドルだけ。データ種別ディレクトリのハンドルは
//!   確認後に閉じる。
//! - 既存の要素にも名前の検査（`validate_single_component`）がかかる。Win32 で通常は作れない
//!   名前（予約名・末尾の `.` 等）の既存要素は `InvalidComponent` で拒否される（fail-closed）。
//! - 入力パスの中に `%LOCALAPPDATA%` の内側へ向くジャンクションがある場合は、`plan_root` が
//!   実体に解決した（正規化後の）パスを開く（拒否はしない。unix は途中の symlink を
//!   すべて拒否するので差がある）。固定の段階で見つかる再解析ポイントは拒否する。
//!   `Local` 配下に OneDrive 等の再解析ポイントがある環境では、それを通る経路は拒否される。
//! - 既存のルート・既存の中間ディレクトリの ACL は確認しない。保護付き DACL を付けるのは
//!   新規に作るルートだけで、既存ルートの検証は #953。中間ディレクトリには DACL を付けない
//!   ため、その名前が他ユーザーから見える可能性は残る。所有者は設定しない（昇格トークンでは
//!   既定の所有者が Administrators になる）。
//! - 既知フォルダの値の出所はレジストリ（`HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\
//!   User Shell Folders`）で、同じユーザーは書き換えられる。同じユーザーによる付け替えは防げない
//!   （Issue #947 で受容した攻撃者像の範囲）。本変更で得られるのは、プロセスの環境ブロック
//!   （`LOCALAPPDATA`）から基準が導かれないことに限る。取得した値にも、ローカルドライブ・
//!   `AppData\Local` で終わる形・ドライブルート拒否の検証をかけ続ける。`USERPROFILE` の
//!   展開の扱いは `tests/known_folder_base.rs` のシナリオ (c) で観測し固定する。
//! - `profile.lock` の所有者・ハードリンク数は検査しない（unix との差）。
//!
//! 判定ロジックは `std::fs` と `std::path` だけで書いてあり、Linux・macOS でも
//! コンパイル・テストできる（Windows 形式のパス固有の挙動は `cfg(windows)` のテストが担う）。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::lock::LOCK_FILE_NAME;
use crate::profile::{DataKind, ProfileError};

/// 基準ディレクトリ（既知フォルダ）が使えない場合の拒否理由。
const REASON_NO_LOCALAPPDATA: &str = "the Local AppData known folder (FOLDERID_LocalAppData) must resolve to an absolute path of an existing directory; on Windows, profile roots are only allowed under it (XOS-7, XOS-10)";
/// 基準ディレクトリが UNC、または `...\AppData\Local` で終わらない場合の拒否理由。
const REASON_BAD_LOCALAPPDATA: &str = "the Local AppData known folder must resolve to a local drive path ending in AppData\\Local (UNC and redirected locations are not supported; the inherited-ACL assumption was only verified for the default location; XOS-7, XOS-10)";
/// ルートが配下でない場合の拒否理由。
const REASON_OUTSIDE: &str = "on Windows, the profile root must be a directory strictly under the Local AppData known folder (inherited ACL is relied on; other locations are not supported yet; XOS-7, XOS-10)";
/// 正規化など、判定に必要な操作が失敗した場合の拒否理由。
const REASON_CANNOT_VERIFY: &str = "on Windows, the profile root could not be verified to be under the Local AppData known folder (path resolution failed); refusing to create it";
/// `.` / `..` / 非ドライブのプレフィックス / ドライブ相対を含むパスの拒否理由。
const REASON_UNSUPPORTED_FORM: &str = "on Windows, the profile root must be a plain drive-absolute path without '.' or '..' components (UNC, device and drive-relative paths are not supported)";
/// 要素の名前が不正（代替データストリーム・予約デバイス名・予約文字・末尾の `.`／空白）な場合の拒否理由。
const REASON_BAD_NAME: &str = "on Windows, a profile root path component has an unsupported name (alternate data stream marker ':', reserved device name, reserved character, or trailing dot/space)";

fn unsupported(reason: &'static str) -> ProfileError {
    ProfileError::Unsupported { reason }
}

/// `child` が `base` の**厳密な配下**（同一パスを含まない）かを、コンポーネント単位で判定する。
///
/// 大文字小文字は区別して比べる（誤って配下でないものを配下と判定しない側に倒す）。
/// 両方を `canonicalize` 済みで渡す前提で、そうすれば大文字小文字・短縮名・`\\?\` 表記の
/// 差は実体の表記に揃っている。`base` と同一のパスは、他のアプリのデータと混ざるため
/// 配下と認めない。
pub(crate) fn is_strictly_under(child: &Path, base: &Path) -> bool {
    match child.strip_prefix(base) {
        Ok(rest) => {
            let mut components = rest.components().peekable();
            components.peek().is_some() && components.all(|c| matches!(c, Component::Normal(_)))
        }
        Err(_) => false,
    }
}

/// 名前に代替データストリームの区切り `:` を含むか（存在する要素にも適用する。OS 非依存）。
pub(crate) fn has_stream_marker(name: &OsStr) -> bool {
    name.to_string_lossy().contains(':')
}

/// Windows の予約デバイス名（`CON`・`PRN`・`AUX`・`NUL`・`CONIN$`・`CONOUT$`・
/// `COM0`〜`COM9`・`LPT0`〜`LPT9`）か。大文字小文字は区別せず、最初の `.` 以降
/// （拡張子）と、その前の末尾空白は無視する（`NUL.txt`・`con .x` も予約）。
///
/// 上付き数字（`COM¹`・`COM²`・`COM³`・`LPT¹`〜`LPT³`）も予約として扱う。Windows 10
/// 以降でこれらが予約される環境があり、拒否して失われるのは稀な名前だけで、通すと
/// デバイスへ書き込み得るため、fail-closed に倒す（OS 非依存の純粋関数）。
pub(crate) fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let digit = |rest: &str| {
        let mut chars = rest.chars();
        matches!(
            (chars.next(), chars.next()),
            (Some('0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}'), None)
        )
    };
    upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(digit)
}

/// 新規作成する要素の名前として許さないか（OS 非依存の純粋関数）。
///
/// `:`（代替データストリーム）と予約文字 `<>"/\|?*`・制御文字、末尾の `.`・空白
/// （Win32 の正規化で名前が変わり、期待する実体と食い違う）、予約デバイス名
/// （[`is_reserved_device_name`]）を拒否する。verbatim 形では `/` が区切りにならず
/// 名前に含まれ得て、`...\Local\CON` のようなリテラル名も作れてしまうため、
/// Windows 以外で動くテストでも同じ判定を使う。
pub(crate) fn is_invalid_new_name(name: &OsStr) -> bool {
    let s = name.to_string_lossy();
    s.chars()
        .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        || s.ends_with('.')
        || s.ends_with(' ')
        || is_reserved_device_name(&s)
}

/// 親ハンドル相対で作る・開く 1 要素の名前の UTF-16 長の上限（NTFS の要素長の上限）。
pub(crate) const MAX_COMPONENT_UTF16_LEN: usize = 255;

/// 名前が [`MAX_COMPONENT_UTF16_LEN`] 個の UTF-16 単位を超えるか。アロケーションせずに判定する。
///
/// Windows では `encode_wide` を上限 + 1 個で打ち切って数える（孤立サロゲートも 1 単位）。
/// それ以外では、UTF-16 の 1 単位あたり高々 3 バイト（不正バイトは損失変換で 1 単位）
/// なので、バイト長が上限の 3 倍を超えれば必ず超過とみなす。
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn exceeds_component_len(name: &OsStr) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        name.encode_wide().take(MAX_COMPONENT_UTF16_LEN + 1).count() > MAX_COMPONENT_UTF16_LEN
    }
    #[cfg(not(windows))]
    {
        let bytes = name.as_encoded_bytes();
        bytes.len() > MAX_COMPONENT_UTF16_LEN * 3
            || String::from_utf8_lossy(bytes)
                .encode_utf16()
                .take(MAX_COMPONENT_UTF16_LEN + 1)
                .count()
                > MAX_COMPONENT_UTF16_LEN
    }
}

/// 親ハンドル相対の作成・open に渡す名前が、単一要素として安全かを検査する
/// （`PROF-4`・`XOS-10`。OS 非依存の純粋関数）。
///
/// `win32` モジュールの 2 関数が FFI を呼ぶ**前**に必ず通す。NT API は Win32 の正規化を通らず、
/// 予約デバイス名や末尾の `.`・空白をリテラルの名前として作ってしまうため、
/// [`is_invalid_new_name`] の規則をそのまま適用する。加えて、空・`.`・`..` と、UTF-16 で
/// [`MAX_COMPONENT_UTF16_LEN`] を超える名前を拒否する（長さはアロケーション前に検査する）。
/// 生の入力はエラーに保持しない。
// 呼び出し元は Windows の `win32` モジュールと単体テストだけ。
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn validate_single_component(name: &OsStr) -> Result<(), ProfileError> {
    let invalid = |reason: &'static str| Err(ProfileError::InvalidComponent { reason });
    if name.is_empty() || name == "." || name == ".." {
        return invalid("name must be a single non-empty path component");
    }
    // 文字列変換（to_string_lossy）は入力全体を確保するため、その前に長さを検査する。
    if exceeds_component_len(name) {
        return invalid("name is longer than 255 UTF-16 code units");
    }
    if is_invalid_new_name(name) {
        return invalid(
            "name contains a separator, reserved character, stream marker, control character, trailing dot/space, or is a reserved device name",
        );
    }
    Ok(())
}

/// 作成前の判定結果。ファイルシステムには何も作っていない。
#[derive(Debug)]
pub(crate) struct RootPlan {
    /// `canonicalize` した `%LOCALAPPDATA%`。
    base: PathBuf,
    /// 作成後にルートがあるべき実体パス（`anchor` + `rest`）。
    expected: PathBuf,
}

impl RootPlan {
    /// 以降の操作の基準にする実体パス。
    pub(crate) fn expected(&self) -> &Path {
        &self.expected
    }
}

/// 基準ディレクトリの候補を検証し、`canonicalize` した実体パスを返す。
///
/// 本番では `win32::known_local_app_data()`（既知フォルダ API）の値だけが渡り、環境変数は
/// 渡さない（Issue #951）。テストだけが偽の基準を渡す。
/// 継承 ACL の実測（#813）は既定の `C:\Users\<user>\AppData\Local` で行われたため、
/// 実体が UNC（フォルダリダイレクト・マップドドライブの先）や、末尾が
/// `AppData\Local` でない場所（`C:\ProgramData` など）は認めない。緩める場合は
/// 第 2 段階（既知フォルダ API で取得した値の検証）で扱う。
fn resolve_base(base_candidate: Option<OsString>) -> Result<PathBuf, ProfileError> {
    let value = base_candidate
        .filter(|v| !v.is_empty())
        .ok_or(unsupported(REASON_NO_LOCALAPPDATA))?;
    let raw = PathBuf::from(value);
    if !raw.is_absolute() {
        return Err(unsupported(REASON_NO_LOCALAPPDATA));
    }
    let base = std::fs::canonicalize(&raw).map_err(|_| unsupported(REASON_NO_LOCALAPPDATA))?;
    let is_dir = std::fs::metadata(&base).is_ok_and(|m| m.is_dir());
    // ドライブルート等（親がない）を `LOCALAPPDATA` とする設定では、ほぼ任意の場所を
    // 許可してしまうため認めない。
    if !is_dir || base.parent().is_none() {
        return Err(unsupported(REASON_NO_LOCALAPPDATA));
    }
    let is_local_drive = !matches!(
        base.components().next(),
        Some(Component::Prefix(p)) if !matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    );
    let ends_with_app_data_local = base
        .file_name()
        .zip(base.parent().and_then(Path::file_name))
        .is_some_and(|(local, app_data)| {
            local.eq_ignore_ascii_case("Local") && app_data.eq_ignore_ascii_case("AppData")
        });
    if !is_local_drive || !ends_with_app_data_local {
        return Err(unsupported(REASON_BAD_LOCALAPPDATA));
    }
    Ok(base)
}

/// `root` が `%LOCALAPPDATA%` の厳密な配下に収まるかを判定する（副作用なし。fail-closed）。
///
/// 配下外・判定不能・不正な形式や名前は [`ProfileError::Unsupported`]、存在する祖先が
/// ディレクトリでない場合は（`ErrorKind::NotADirectory` で見つかった場合も、祖先を
/// たどった結果ファイルに当たった場合も）全 OS で [`ProfileError::InvalidLayout`] を
/// 返す。いずれの場合もディレクトリは作らない（`PROF-1`・`XOS-7`・`XOS-10`）。
pub(crate) fn plan_root(
    root: &Path,
    base_candidate: Option<OsString>,
) -> Result<RootPlan, ProfileError> {
    let base = resolve_base(base_candidate)?;

    // ドライブ相対（`C:foo`）は、そのドライブのカレントディレクトリ次第で意味が変わるため
    // 絶対化の前に拒否する。
    let mut raw_components = root.components();
    if let (Some(Component::Prefix(p)), next) = (raw_components.next(), raw_components.next())
        && matches!(p.kind(), Prefix::Disk(_))
        && !matches!(next, Some(Component::RootDir))
    {
        return Err(unsupported(REASON_UNSUPPORTED_FORM));
    }
    let absolute = std::path::absolute(root).map_err(|_| unsupported(REASON_CANNOT_VERIFY))?;

    // `..`・`.`、ドライブ以外のプレフィックス（UNC・デバイス・`\\?\GLOBALROOT` 等）を持つ
    // パスは受け付けない。verbatim 形では `..` が実在の名前として扱われ、字句的に畳めない。
    // 代替データストリームの `:` は、存在する要素も含む全要素で拒否する。
    for component in absolute.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                if has_stream_marker(name) {
                    return Err(unsupported(REASON_BAD_NAME));
                }
            }
            Component::Prefix(p) => {
                if !matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
                    return Err(unsupported(REASON_UNSUPPORTED_FORM));
                }
            }
            Component::CurDir | Component::ParentDir => {
                return Err(unsupported(REASON_UNSUPPORTED_FORM));
            }
        }
    }

    // 存在する最も深い祖先を探す。NotFound と NotADirectory（祖先にファイルがある）は
    // さらに上へたどる（OS によってどちらが返るかが違っても、最終的にファイルに当たり
    // 同じ結果になる）。それ以外のエラーは判定不能として拒否する。
    let mut existing: Option<&Path> = None;
    for ancestor in absolute.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                existing = Some(ancestor);
                break;
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(_) => return Err(unsupported(REASON_CANNOT_VERIFY)),
        }
    }
    let existing = existing.ok_or(unsupported(REASON_CANNOT_VERIFY))?;
    let canonical_existing =
        std::fs::canonicalize(existing).map_err(|_| unsupported(REASON_CANNOT_VERIFY))?;
    let existing_is_dir = std::fs::metadata(&canonical_existing).is_ok_and(|m| m.is_dir());

    let rest_path = absolute
        .strip_prefix(existing)
        .map_err(|_| unsupported(REASON_CANNOT_VERIFY))?;
    for component in rest_path.components() {
        match component {
            Component::Normal(name) if !is_invalid_new_name(name) => {}
            Component::Normal(_) => return Err(unsupported(REASON_BAD_NAME)),
            _ => return Err(unsupported(REASON_UNSUPPORTED_FORM)),
        }
    }
    let expected = canonical_existing.join(rest_path);

    if !is_strictly_under(&expected, &base) {
        return Err(unsupported(REASON_OUTSIDE));
    }
    if !existing_is_dir {
        return Err(ProfileError::InvalidLayout {
            path: canonical_existing,
            reason: "an existing path component of the profile root is not a directory",
        });
    }
    Ok(RootPlan { base, expected })
}

/// `FILE_SHARE_READ`。ディレクトリのハンドルを開くときの共有モード（読み取りの共有を許す）。
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
/// `FILE_SHARE_WRITE`。同上（書き込みの共有を許す）。`FILE_SHARE_DELETE`（0x4）は意図的に付けない。
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
/// `FILE_FLAG_BACKUP_SEMANTICS`。ディレクトリを `CreateFile` で開くために必要。
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// `FILE_FLAG_OPEN_REPARSE_POINT`。リンク先を辿らず再解析ポイント自体を開く
/// （std の `OpenOptionsExt::custom_flags` に渡すだけで Win32 の直接呼び出しではない）。
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// `FILE_ATTRIBUTE_REPARSE_POINT`。ジャンクション・symlink 等の再解析ポイントを示す属性ビット。
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// ディレクトリのハンドルをパスで開く。無ければ `Ok(None)`。起点の `LOCALAPPDATA`
/// （[`pin_root`]）と、ハンドル相対にしない非 Windows の骨格だけが使う。
///
/// Windows では `read(true)` + `FILE_SHARE_READ | FILE_SHARE_WRITE`（`FILE_SHARE_DELETE` なし）
/// で開くため、このハンドルを保持している間、そのディレクトリの改名・削除（`DELETE`
/// アクセスを要する open）は共有違反で失敗し、改名・削除による別実体への差し替えは防げる
/// （属性アクセスだけのハンドルは共有モードの検査に参加しないので `read(true)` は必須）。
/// 空のディレクトリへの `FSCTL_SET_REPARSE_POINT`（再解析ポイント化）は防げないが、
/// 子の作成はハンドル相対のため配下外には作られない（モジュール doc「限界」）。
/// `FILE_FLAG_OPEN_REPARSE_POINT` により、その名前がジャンクション・symlink ならリンク
/// 自体が開き、ハンドルの属性で再解析ポイントを検出して [`ProfileError::InvalidLayout`]
/// にする。Windows 以外ではハンドルによる固定はなく、通常の open 後にパスが symlink で
/// ないことを確認するだけ（3 OS で動く骨格のテスト用。本番は Windows のみ）。
fn try_open_dir(path: &Path) -> Result<Option<File>, ProfileError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ProfileError::Io(err)),
    };
    let metadata = file.metadata().map_err(ProfileError::Io)?;
    #[cfg(windows)]
    let is_reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    #[cfg(not(windows))]
    let is_reparse = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if is_reparse {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "profile path component is a symlink or junction (reparse point)",
        });
    }
    if !metadata.is_dir() {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "profile path component is not a directory",
        });
    }
    Ok(Some(file))
}

/// [`try_open_dir`] で、無ければ `NotFound` の入出力エラーにする。
fn open_dir(path: &Path) -> Result<File, ProfileError> {
    try_open_dir(path)?.ok_or_else(|| ProfileError::Io(std::io::ErrorKind::NotFound.into()))
}

/// `LOCALAPPDATA` からルートまでの各ディレクトリのハンドル（先頭が `LOCALAPPDATA`、
/// 末尾がルート）。保持している間、Windows ではそれらの改名・削除ができない。
/// ルートまでの各要素は親のハンドル相対で作成・open している（[`open_or_create_child_dir`]）。
#[derive(Debug)]
pub(crate) struct PinnedPath {
    dirs: Vec<File>,
}

impl PinnedPath {
    /// ルート（末尾の要素）のハンドルを借りる。`profile.lock`・データ種別ディレクトリを
    /// このハンドル相対で作る・開くために使う（`profile.rs` の `open_under_local_app_data` から呼ばれる）。
    pub(crate) fn root(&self) -> Result<&File, ProfileError> {
        self.dirs.last().ok_or(ProfileError::InvalidLayout {
            path: PathBuf::new(),
            reason: "no pinned directory handle",
        })
    }

    /// 途中の要素のハンドルを閉じ、ルートのハンドルだけを返す。`Profile` が生存中保持する
    /// （ルートの改名・削除・差し替えを防ぐ）。
    pub(crate) fn into_root_handle(mut self) -> Result<File, ProfileError> {
        self.dirs.pop().ok_or(ProfileError::InvalidLayout {
            path: PathBuf::new(),
            reason: "no pinned directory handle",
        })
    }

    /// 保持しているハンドルの数（`LOCALAPPDATA` を含む）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.dirs.len()
    }
}

/// `win32` が返す `InvalidLayout`（親からの相対名）のパスを、表示用に親のパスと連結して
/// フルパスへ補う。それ以外のエラーはそのまま返す。
#[cfg(windows)]
fn with_full_path(err: ProfileError, parent_path: &Path) -> ProfileError {
    match err {
        ProfileError::InvalidLayout { path, reason } => ProfileError::InvalidLayout {
            path: parent_path.join(path),
            reason,
        },
        other => other,
    }
}

/// 親ハンドル `parent` の直下に子ディレクトリ `name` を作成する（既にあれば開く）。
///
/// `pin_root`（ルートまでの各要素）と [`ensure_data_dirs`]（データ種別ディレクトリ）から
/// 呼ばれる継ぎ目（`PROF-1`・`PROF-4`・`XOS-10`）。Windows では `win32::open_or_create_dir_at`
/// （NT API。親のハンドルを起点にし、パス文字列を再解決しない）を呼ぶ。`parent_path` は
/// エラー表示にだけ使い、解決には使わない。非 Windows はテスト用の骨格（本番の経路ではない）で、
/// `create_dir` と [`open_dir`] によりパスで解決し、`parent` は使わない。
fn open_or_create_child_dir(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
) -> Result<File, ProfileError> {
    #[cfg(windows)]
    {
        crate::win32::open_or_create_dir_at(parent, name)
            .map(|opened| opened.file)
            .map_err(|err| with_full_path(err, parent_path))
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        let path = parent_path.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(ProfileError::Io(err)),
        }
        open_dir(&path)
    }
}

/// `pin_root` の最後の要素（プロファイルのルート）用の継ぎ目。Windows は
/// `win32::open_or_create_protected_dir_at`（新規作成時に保護付き DACL を付けて検証する。
/// Issue #952）、非 Windows は [`open_or_create_child_dir`] に委ねる（骨格。挙動は同じ）。
fn open_or_create_root_dir(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
) -> Result<File, ProfileError> {
    #[cfg(windows)]
    {
        crate::win32::open_or_create_protected_dir_at(parent, name)
            .map(|opened| opened.file)
            .map_err(|err| with_full_path(err, parent_path))
    }
    #[cfg(not(windows))]
    {
        open_or_create_child_dir(parent, parent_path, name)
    }
}

/// 親ハンドル `parent` の直下にファイル `name`（`profile.lock`）を読み書き可能で作成する
/// （既にあれば開く）。[`open_lock_file`] から呼ばれる継ぎ目で、方針は
/// [`open_or_create_child_dir`] と同じ（Windows は `win32::open_or_create_file_at`、
/// 非 Windows はパスで解決するテスト用の骨格）。通常ファイルでなければ `InvalidLayout`。
fn open_or_create_child_file(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
) -> Result<File, ProfileError> {
    #[cfg(windows)]
    {
        crate::win32::open_or_create_file_at(parent, name)
            .map(|opened| opened.file)
            .map_err(|err| with_full_path(err, parent_path))
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        let path = parent_path.join(name);
        // 既存物が通常ファイルでない（ディレクトリ・symlink 等）場合は、開く前に拒否する。
        match std::fs::symlink_metadata(&path) {
            Ok(m) if !m.file_type().is_file() => {
                return Err(ProfileError::InvalidLayout {
                    path,
                    reason: "profile lock path is not a regular file",
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(ProfileError::Io(err)),
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(ProfileError::Io)?;
        if !file.metadata().map_err(ProfileError::Io)?.is_file() {
            return Err(ProfileError::InvalidLayout {
                path,
                reason: "profile lock path is not a regular file",
            });
        }
        Ok(file)
    }
}

/// `LOCALAPPDATA` からルートまでを 1 要素ずつ親のハンドル相対で作成・open し（既にあれば開く）、
/// 各ディレクトリのハンドルを保持して経路を固定する。最後の要素（ルート）だけは新規作成時に
/// 保護付き DACL を付ける（[`open_or_create_root_dir`]。Issue #952）。
///
/// 起点の `LOCALAPPDATA` だけはパスで開く（`canonicalize` 済み・再解析ポイント検査あり。
/// 既知フォルダ API は #951 まで未対応）。以降は [`open_or_create_child_dir`] により、
/// 作成と open が原子的（Windows は `FILE_OPEN_IF`）で、パスの再解決による配下外への作成は
/// 起きない。並行して同じルートを開く別プロセスがあってもそのまま開ける。各ハンドルは
/// 再解析ポイントでないディレクトリであることを開く時点で確認する。固定後に
/// [`verify_created_root`] で実体の食い違いを事後に検出する（多層防御。OS 非依存の骨格。
/// `PROF-1`・`PROF-4`）。既存の要素にも名前の検査（`validate_single_component`）がかかるため、
/// Win32 で通常は作れない名前の既存要素は `InvalidComponent` で拒否される（fail-closed）。
pub(crate) fn pin_root(plan: &RootPlan) -> Result<PinnedPath, ProfileError> {
    let relative = plan
        .expected
        .strip_prefix(&plan.base)
        .map_err(|_| unsupported(REASON_OUTSIDE))?;
    let mut dirs = vec![open_dir(&plan.base)?];
    let mut current = plan.base.clone();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(unsupported(REASON_UNSUPPORTED_FORM));
        };
        let is_root = components.peek().is_none();
        let parent = dirs.last().ok_or(ProfileError::InvalidLayout {
            path: current.clone(),
            reason: "no pinned directory handle",
        })?;
        let handle = if is_root {
            open_or_create_root_dir(parent, &current, name)?
        } else {
            open_or_create_child_dir(parent, &current, name)?
        };
        current.push(name);
        dirs.push(handle);
    }
    verify_created_root(plan)?;
    Ok(PinnedPath { dirs })
}

/// ルートの実体が作成前の判定どおりであることを再確認する（検出専用）。
///
/// 作成はハンドル相対のため、配下外への作成を防ぐ役割はもう持たない。食い違い
/// （ジャンクション・symlink への差し替え）は [`ProfileError::InvalidLayout`]
/// （作ったものは消さない。モジュール doc の「限界」）。パスの再解決による確認なので、
/// この確認より後の再解析ポイント化や、確認前に外された再解析ポイントは検出できない。
fn verify_created_root(plan: &RootPlan) -> Result<(), ProfileError> {
    let actual = std::fs::canonicalize(&plan.expected).map_err(ProfileError::Io)?;
    if actual != plan.expected || !is_strictly_under(&actual, &plan.base) {
        return Err(ProfileError::InvalidLayout {
            path: plan.expected.clone(),
            reason: "profile root was replaced or redirected (symlink/junction) during creation",
        });
    }
    let is_dir = std::fs::symlink_metadata(&actual).is_ok_and(|m| m.is_dir());
    if !is_dir {
        return Err(ProfileError::InvalidLayout {
            path: plan.expected.clone(),
            reason: "profile root is not a directory",
        });
    }
    Ok(())
}

/// ルートのハンドル `root_dir` の直下の `profile.lock` を開く（無ければ作る）。
///
/// `root_path` は表示とエラー用で、Windows では解決に使わない（ハンドル相対。再解析ポイントや
/// 種別の不一致は `win32` が [`ProfileError::InvalidLayout`] で拒否する）。ロックは常にルートの
/// 実体の `profile.lock` に取られ、作成の途中でルートが再解析ポイント化されても配下外の
/// ファイルにはならない（`PROF-1`）。返すパスは `root_path.join("profile.lock")`
/// （ロックのエラー表示用）。unix 側が行う所有者・ハードリンク数の検査に相当するものは
/// Windows にはない（ACL は継承に依存し検証しない。REPAIR-3）。
///
/// Windows のハンドルは READ・WRITE を共有し DELETE を共有しないため、`ProfileLock` が保持
/// している間は改名・削除による別実体への差し替えを防ぐ。2 つ目の open は共有違反でなく
/// ロック取得失敗（[`ProfileError::Locked`]）で拒否される。
pub(crate) fn open_lock_file(
    root_dir: &File,
    root_path: &Path,
) -> Result<(File, PathBuf), ProfileError> {
    let file = open_or_create_child_file(root_dir, root_path, OsStr::new(LOCK_FILE_NAME))?;
    Ok((file, root_path.join(LOCK_FILE_NAME)))
}

/// ルートのハンドル `root_dir` の直下に 4 つのデータ種別ディレクトリを作り（既にあれば開き）、
/// 通常のディレクトリであることを確認する（確認後はハンドルを閉じる）。
///
/// `root_path` は表示とエラー用。Windows ではハンドル相対のため構造上ルートの直下になり、
/// パスの `canonicalize` による確認は行わない。非 Windows の骨格はパスで解決するため、
/// `canonicalize` の結果が `root_path` の直下であることを確認する。
pub(crate) fn ensure_data_dirs(root_dir: &File, root_path: &Path) -> Result<(), ProfileError> {
    for kind in DataKind::ALL {
        let name = OsStr::new(kind.dir_name());
        let _dir = open_or_create_child_dir(root_dir, root_path, name)?;
        #[cfg(not(windows))]
        {
            let path = root_path.join(name);
            let actual = std::fs::canonicalize(&path).map_err(ProfileError::Io)?;
            let is_dir = std::fs::symlink_metadata(&actual).is_ok_and(|m| m.is_dir());
            if !is_dir
                || actual.parent() != Some(root_path)
                || actual.file_name() != path.file_name()
            {
                return Err(ProfileError::InvalidLayout {
                    path,
                    reason: "profile data directory is not a plain directory directly under the root",
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// `canonicalize` 済みの一意な一時ディレクトリ。drop 時に再帰削除する。
    /// `lad()` が `LOCALAPPDATA` 相当（`<top>/AppData/Local`）を返す。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::fs::canonicalize(std::env::temp_dir()).expect("temp_dir");
            let path = base.join(format!("fandhe-localroot-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        /// `<top>/AppData/Local`（作成済み）。
        fn lad(&self) -> PathBuf {
            let lad = self.0.join("AppData").join("Local");
            std::fs::create_dir_all(&lad).expect("create fake LOCALAPPDATA");
            lad
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env(dir: &Path) -> Option<OsString> {
        Some(dir.as_os_str().to_os_string())
    }

    fn is_unsupported<T: std::fmt::Debug>(result: &Result<T, ProfileError>) -> bool {
        matches!(result, Err(ProfileError::Unsupported { .. }))
    }

    /// XOS-7・XOS-10: 厳密な配下判定はコンポーネント単位で、同一・兄弟・前方一致は配下でない。
    #[test]
    fn xos7_is_strictly_under_compares_by_component() {
        let base = Path::new("/a/Local");
        assert!(is_strictly_under(Path::new("/a/Local/p"), base));
        assert!(is_strictly_under(Path::new("/a/Local/p/q"), base));
        assert!(!is_strictly_under(Path::new("/a/Local"), base));
        assert!(!is_strictly_under(Path::new("/a/LocalEvil/p"), base));
        assert!(!is_strictly_under(Path::new("/a"), base));
        assert!(!is_strictly_under(Path::new("/b/Local/p"), base));
        assert!(!is_strictly_under(Path::new("/a/local/p"), base));
        assert!(!is_strictly_under(Path::new("/a/Local/../x"), base));
    }

    /// XOS-10: 新規要素の名前検査。予約デバイス名（大文字小文字・拡張子・末尾の空白やドット・
    /// 上付き数字）、`:`、予約文字、制御文字、末尾のドット・空白を拒否し、通常名は通す。
    #[test]
    fn xos10_invalid_new_name_rejects_reserved_and_special() {
        for bad in [
            "CON",
            "con",
            "Prn",
            "AUX",
            "NUL",
            "nul.txt",
            "NUL.tar.gz",
            "COM0",
            "COM1",
            "com9",
            "LPT0",
            "lpt9",
            "LPT5.log",
            "CON ",
            "CON.",
            "con .x",
            "COM\u{b9}",
            "LPT\u{b2}",
            "COM\u{b3}.txt",
            "CONIN$",
            "CONOUT$",
            "a:b",
            "name:stream",
            ":",
            "a<b",
            "a>b",
            "a\"b",
            "a/b",
            "a\\b",
            "a|b",
            "a?b",
            "a*b",
            "tab\there",
            "dot.",
            "space ",
            "..",
            " ",
        ] {
            assert!(
                is_invalid_new_name(OsStr::new(bad)),
                "{bad:?} が拒否されなかった"
            );
        }
        for good in [
            "profile",
            "Local",
            "a.b",
            "CONSOLE",
            "COM10",
            "LPT10",
            "COMX",
            "NULL",
            "AUXX",
            "my-profile_1",
            "conf",
            "x.CON",
            ".hidden",
            "fandhe-browser",
        ] {
            assert!(
                !is_invalid_new_name(OsStr::new(good)),
                "{good:?} が拒否された"
            );
        }
        assert!(has_stream_marker(OsStr::new("file:stream")));
        assert!(!has_stream_marker(OsStr::new("file")));
    }

    /// XOS-10: 予約名・`:` を含む新規要素は、`LOCALAPPDATA` 配下でも何も作らず拒否される。
    #[test]
    fn xos10_plan_rejects_bad_names_without_creating() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        for name in ["CON", "nul.txt", "COM1", "a:b", "trail.", "trail "] {
            let root = lad.join("parent").join(name);
            let result = plan_root(&root, env(&lad));
            assert!(is_unsupported(&result), "{name:?}: {result:?}");
        }
        assert!(!lad.join("parent").exists());
    }

    /// XOS-10: `LOCALAPPDATA` の末尾が `AppData/Local` でない場合は拒否（大文字小文字は区別しない）。
    #[test]
    fn xos10_resolve_base_requires_app_data_local_suffix() {
        let tmp = TempDir::new();
        for bad in [
            tmp.0.clone(),
            tmp.0.join("ProgramData"),
            tmp.0.join("Local"),
            tmp.0.join("AppData"),
            tmp.0.join("AppData").join("LocalLow"),
            tmp.0.join("Other").join("Local"),
        ] {
            std::fs::create_dir_all(&bad).unwrap();
            let result = plan_root(&bad.join("p"), env(&bad));
            assert!(
                matches!(&result, Err(ProfileError::Unsupported { reason }) if reason.contains("AppData")),
                "{bad:?}: {result:?}"
            );
            assert!(!bad.join("p").exists());
        }
        let mixed = tmp.0.join("x").join("APPDATA").join("local");
        std::fs::create_dir_all(&mixed).unwrap();
        let plan = plan_root(&mixed.join("p"), env(&mixed)).expect("大文字小文字は区別しない");
        assert_eq!(plan.expected(), mixed.join("p").as_path());
    }

    /// XOS-10（Windows）: verbatim UNC の `LOCALAPPDATA` と、デバイス・UNC・GLOBALROOT 形式は拒否。
    #[cfg(windows)]
    #[test]
    fn xos10_windows_rejects_unc_base_and_odd_prefixes() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let result = plan_root(
            &lad.join("p"),
            Some(OsString::from(r"\\?\UNC\server\share\AppData\Local")),
        );
        assert!(is_unsupported(&result), "{result:?}");
        for root in [
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\AppData\Local\p",
            r"\\.\C:\AppData\Local\p",
            r"\\server\share\AppData\Local\p",
            r"C:foo",
        ] {
            let result = plan_root(Path::new(root), env(&lad));
            assert!(is_unsupported(&result), "{root:?}: {result:?}");
        }
    }

    /// XOS-10: 新規ルートは判定だけでは何も作らず、期待する実体パスを返す。
    #[test]
    fn xos10_plan_accepts_new_root_under_base_without_creating() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("a").join("b");
        let plan = plan_root(&root, env(&lad)).expect("配下は許可される");
        assert_eq!(plan.expected(), root.as_path());
        assert!(!lad.join("a").exists(), "判定だけでは作らない");
    }

    /// XOS-10: 既存ルートも許可される。
    #[test]
    fn xos10_plan_accepts_existing_root() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("existing");
        std::fs::create_dir(&root).unwrap();
        let plan = plan_root(&root, env(&lad)).expect("既存でも許可");
        assert_eq!(plan.expected(), root.as_path());
    }

    /// PROF-1・XOS-10: `LOCALAPPDATA` と同一のルートは他アプリのデータと混ざるため拒否。
    #[test]
    fn xos10_plan_rejects_root_equal_to_base() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        assert!(is_unsupported(&plan_root(&lad, env(&lad))));
    }

    /// PROF-1・XOS-10: 親・兄弟・名前が前方一致するだけのディレクトリは配下でない。
    #[test]
    fn xos10_plan_rejects_parent_sibling_and_prefix_lookalike() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let evil = lad.with_file_name("LocalEvil");
        std::fs::create_dir(&evil).unwrap();
        for root in [
            lad.parent().unwrap().to_path_buf(),
            lad.with_file_name("sibling"),
            evil.join("p"),
            evil.clone(),
        ] {
            assert!(
                is_unsupported(&plan_root(&root, env(&lad))),
                "{root:?} が拒否されなかった"
            );
        }
        assert!(!lad.with_file_name("sibling").exists());
        assert!(!evil.join("p").exists());
    }

    /// PROF-4・XOS-10: `..` を含むパスは配下外へ出られない。
    #[test]
    fn xos10_plan_rejects_dotdot_escape() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("..").join("escaped");
        assert!(is_unsupported(&plan_root(&root, env(&lad))));
        assert!(!lad.with_file_name("escaped").exists());
    }

    /// XOS-10: `LOCALAPPDATA` が未設定・空・相対・存在しない・ディレクトリでない場合は拒否。
    #[test]
    fn xos10_plan_rejects_unusable_local_app_data() {
        let tmp = TempDir::new();
        let root = tmp.0.join("p");
        let file = tmp.0.join("file");
        std::fs::write(&file, b"x").unwrap();
        for value in [
            None,
            Some(OsString::new()),
            Some(OsString::from("relative/dir")),
            Some(tmp.0.join("missing").into_os_string()),
            Some(file.into_os_string()),
        ] {
            assert!(
                is_unsupported(&plan_root(&root, value.clone())),
                "{value:?} が拒否されなかった"
            );
        }
        assert!(!root.exists());
    }

    /// PROF-1: 祖先が非ディレクトリなら、全 OS で `InvalidLayout`（`NotADirectory` で見つかる
    /// 経路でも、`NotFound` でたどった結果ファイルに当たる経路でも同じ）。何も作らない。
    #[test]
    fn prof1_plan_rejects_non_directory_ancestor() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let file = lad.join("file");
        std::fs::write(&file, b"x").unwrap();
        for root in [file.join("p"), file.join("p").join("q")] {
            let result = plan_root(&root, env(&lad));
            assert!(
                matches!(result, Err(ProfileError::InvalidLayout { .. })),
                "{root:?}: {result:?}"
            );
        }
        assert!(!file.join("p").exists());
        assert!(file.is_file());
    }

    /// PROF-4・XOS-10: 配下のシンボリックリンクが配下外を指す場合は拒否（unix で検証）。
    #[cfg(unix)]
    #[test]
    fn xos10_plan_rejects_symlink_escape() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, lad.join("link")).unwrap();
        let root = lad.join("link").join("p");
        assert!(is_unsupported(&plan_root(&root, env(&lad))));
        assert!(!outside.join("p").exists());
    }

    /// PROF-1: 正常系は要素を 1 つずつ作り、ロックファイルと 4 サブディレクトリも作られる。
    #[test]
    fn prof1_pin_root_lock_and_data_dirs() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("p").join("q");
        let plan = plan_root(&root, env(&lad)).unwrap();
        pin_root(&plan).expect("通常のディレクトリは作れる");
        let pinned = pin_root(&plan).expect("既存（AlreadyExists）でも成功する");
        let root_dir = pinned.root().expect("ルートのハンドル");
        let (_file, lock_path) = open_lock_file(root_dir, plan.expected()).unwrap();
        assert_eq!(lock_path, root.join("profile.lock"));
        ensure_data_dirs(root_dir, plan.expected()).unwrap();
        for name in ["cookies", "storage", "cache", "history"] {
            assert!(root.join(name).is_dir(), "{name} がない");
        }
        assert!(lock_path.is_file());
    }

    /// PROF-1: 作成の途中の要素が（判定後に）通常ファイルへ差し替わった場合は `InvalidLayout`
    /// で止まり、その先を作らない。
    #[test]
    fn prof1_pin_root_stops_at_non_directory_component() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let plan = plan_root(&lad.join("p").join("q").join("r"), env(&lad)).unwrap();
        std::fs::write(lad.join("p"), b"x").unwrap();
        let err = pin_root(&plan).expect_err("非ディレクトリで止まる");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert!(lad.join("p").is_file());
    }

    /// PROF-1・PROF-4: 判定後、作成前にルートが配下外へのリンクに差し替えられると、
    /// 検出して拒否し、配下外に子を作らない（unix で検証）。
    #[cfg(unix)]
    #[test]
    fn prof1_pin_root_detects_redirect_after_plan() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let plan = plan_root(&lad.join("p"), env(&lad)).unwrap();
        std::os::unix::fs::symlink(&outside, lad.join("p")).unwrap();
        let err = pin_root(&plan).expect_err("差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// PROF-1・PROF-4: 途中の要素が差し替えられた場合は 1 段で止まり、その先に 2 段目
    /// （`q`）が作られていない（unix で検証。`create_dir_all` では `outside/q` ができる）。
    #[cfg(unix)]
    #[test]
    fn prof1_pin_root_stops_after_one_level_when_intermediate_swapped() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let plan = plan_root(&lad.join("p").join("q").join("r"), env(&lad)).unwrap();
        std::os::unix::fs::symlink(&outside, lad.join("p")).unwrap();
        let err = pin_root(&plan).expect_err("途中の差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert!(!outside.join("q").exists(), "2 段目が配下外に作られた");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// PROF-1: `profile.lock` の位置に通常ファイル以外（unix: symlink 先のディレクトリ等）が
    /// あれば拒否される。ここではディレクトリで代用する。
    #[test]
    fn prof1_open_lock_file_rejects_directory_in_lock_position() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("p");
        std::fs::create_dir_all(root.join("profile.lock")).unwrap();
        let root_dir = open_dir(&root).unwrap();
        let result = open_lock_file(&root_dir, &root);
        assert!(
            matches!(result, Err(ProfileError::InvalidLayout { .. })),
            "{result:?}"
        );
    }

    /// PROF-1: データ種別ディレクトリの位置が通常ファイルなら `InvalidLayout`。
    #[test]
    fn prof1_ensure_data_dirs_rejects_file_in_subdir_position() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cookies"), b"x").unwrap();
        let root_dir = open_dir(&root).unwrap();
        let err = ensure_data_dirs(&root_dir, &root).expect_err("ファイルは拒否される");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
    }

    /// XOS-10（Windows）: Windows 形式のパス（ドライブ・verbatim）でもコンポーネント単位で判定する。
    #[cfg(windows)]
    #[test]
    fn xos10_windows_is_strictly_under_handles_drive_and_verbatim() {
        let base = Path::new(r"\\?\C:\Users\u\AppData\Local");
        assert!(is_strictly_under(
            Path::new(r"\\?\C:\Users\u\AppData\Local\p"),
            base
        ));
        assert!(!is_strictly_under(base, base));
        assert!(!is_strictly_under(
            Path::new(r"\\?\C:\Users\u\AppData\LocalEvil\p"),
            base
        ));
        assert!(!is_strictly_under(
            Path::new(r"\\?\C:\Users\u\AppData"),
            base
        ));
        assert!(!is_strictly_under(
            Path::new(r"\\?\D:\Users\u\AppData\Local\p"),
            base
        ));
        assert!(!is_strictly_under(
            Path::new(r"\\?\UNC\s\share\Local\p"),
            base
        ));
    }

    /// PROF-1・PROF-4・XOS-10（Windows）: 判定後、作成前にルートが配下外へのジャンクションへ
    /// 差し替えられても、作成時の確認が検出し、配下外に何も作らない。
    #[cfg(windows)]
    #[test]
    fn prof1_windows_pin_root_detects_junction_swapped_after_plan() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let plan = plan_root(&lad.join("p").join("q"), env(&lad)).unwrap();
        // cmd.exe は `\\?\` 付きパスを扱えないため、ドライブ形式へ戻して渡す。
        let plain = |p: &Path| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"));
        let output = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(plain(&lad.join("p")))
            .arg(plain(&outside))
            .output()
            .expect("cmd.exe を起動できる");
        assert!(
            output.status.success(),
            "mklink /J failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let err = pin_root(&plan).expect_err("差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "配下外に作られた"
        );
    }

    /// PROF-1: 固定したハンドルは `LOCALAPPDATA` からルートまで 1 要素ごと（基点を含む）で、
    /// ルートのハンドルだけを取り出せる。
    #[test]
    fn prof1_pin_root_holds_one_handle_per_component() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let plan = plan_root(&lad.join("p").join("q"), env(&lad)).unwrap();
        let pinned = pin_root(&plan).unwrap();
        assert_eq!(pinned.len(), 3);
        pinned.into_root_handle().expect("ルートのハンドル");
    }

    /// PROF-1（Windows）: ハンドルを保持している間は途中の要素・ルートの改名が共有違反で失敗し、
    /// ジャンクションへ差し替えられない。解放後は改名できる。
    #[cfg(windows)]
    #[test]
    fn prof1_windows_pinned_components_cannot_be_renamed() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let plan = plan_root(&lad.join("a").join("b"), env(&lad)).unwrap();
        let pinned = pin_root(&plan).unwrap();
        assert!(std::fs::rename(lad.join("a").join("b"), lad.join("a").join("b2")).is_err());
        assert!(std::fs::rename(lad.join("a"), lad.join("a2")).is_err());
        assert!(lad.join("a").join("b").is_dir());
        assert!(!lad.join("a2").exists());
        drop(pinned);
        std::fs::rename(lad.join("a"), lad.join("a2")).expect("解放後は改名できる");
        std::fs::rename(lad.join("a2"), lad.join("a")).expect("戻せる");
    }

    /// PROF-1・PROF-4（Windows）: 存在する途中の要素・ルートがジャンクションなら
    /// 固定の段階で `InvalidLayout` になり、先には何も作られない。
    #[cfg(windows)]
    #[test]
    fn prof1_windows_pin_root_rejects_junction_components() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        // 途中の要素 `a` と、ルート `r` をそれぞれ判定後にジャンクションへ差し替える。
        let plan_mid = plan_root(&lad.join("a").join("b"), env(&lad)).unwrap();
        let plan_root_ = plan_root(&lad.join("r"), env(&lad)).unwrap();
        let plain = |p: &Path| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"));
        for link in ["a", "r"] {
            let output = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(plain(&lad.join(link)))
                .arg(plain(&outside))
                .output()
                .expect("cmd.exe を起動できる");
            assert!(output.status.success(), "mklink /J failed");
        }
        for plan in [&plan_mid, &plan_root_] {
            let err = pin_root(plan).expect_err("ジャンクションは拒否される");
            assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        }
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "配下外に作られた"
        );
    }

    /// `mklink /J`（cmd.exe は `\\?\` 付きパスを扱えないためドライブ形式へ戻して渡す）。
    #[cfg(windows)]
    fn mklink_junction(link: &Path, target: &Path) {
        let plain = |p: &Path| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"));
        let output = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(plain(link))
            .arg(plain(target))
            .output()
            .expect("cmd.exe を起動できる");
        assert!(output.status.success(), "mklink /J failed");
    }

    /// `FILE_SHARE_DELETE` 付きで開く（保持中でも改名できる。パスとハンドルを食い違わせる模擬用）。
    #[cfg(windows)]
    fn open_dir_share_delete(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | 0x4)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("ディレクトリを開ける")
    }

    /// PROF-1・PROF-4・XOS-10（Windows）: 親のパスがジャンクションへ差し替わっても（ハンドルは
    /// 元の実体のまま）、`profile.lock` とデータ種別ディレクトリはハンドルの実体の下にだけでき、
    /// 配下外には作られない（空のディレクトリの再解析ポイント化を、パスとハンドルの食い違いで模擬。
    /// 実機の FSCTL は未検証。win32.rs の「限界」）。
    #[cfg(windows)]
    #[test]
    fn prof1_xos10_windows_lock_and_data_dirs_follow_root_handle_not_path() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(lad.join("real")).unwrap();
        let handle = open_dir_share_delete(&lad.join("real"));
        std::fs::rename(lad.join("real"), lad.join("moved")).expect("改名できる");
        mklink_junction(&lad.join("real"), &outside);
        let stale = lad.join("real");
        open_lock_file(&handle, &stale).expect("ロックはハンドルの実体に作られる");
        ensure_data_dirs(&handle, &stale).expect("データ種別はハンドルの実体に作られる");
        assert!(lad.join("moved").join("profile.lock").is_file());
        for name in ["cookies", "storage", "cache", "history"] {
            assert!(lad.join("moved").join(name).is_dir(), "{name} がない");
        }
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "配下外に作られた"
        );
    }

    /// PROF-1（Windows）: 同じ新規ルートへ同時に `pin_root` しても両方成功する
    /// （`FILE_OPEN_IF` の原子性。作成の競合で失敗しない）。
    #[cfg(windows)]
    #[test]
    fn prof1_windows_pin_root_concurrent_create_is_atomic() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let plan = plan_root(&lad.join("a").join("b"), env(&lad)).unwrap();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| pin_root(&plan))).collect();
            for handle in handles {
                handle.join().unwrap().expect("同時作成でも成功する");
            }
        });
        assert_eq!(std::fs::read_dir(&lad).unwrap().count(), 1);
    }

    #[test]
    fn prof4_xos10_validate_single_component_rejects_unsafe_names() {
        let long = "a".repeat(MAX_COMPONENT_UTF16_LEN + 1);
        for name in [
            "",
            ".",
            "..",
            "a\\b",
            "a/b",
            "a:b",
            "CON",
            "x.",
            "x ",
            "a\0b",
            "a*b",
            long.as_str(),
        ] {
            assert!(
                matches!(
                    validate_single_component(OsStr::new(name)),
                    Err(ProfileError::InvalidComponent { .. })
                ),
                "must reject {name:?}"
            );
        }
    }

    #[test]
    fn prof4_xos10_validate_single_component_rejects_oversized_before_conversion() {
        // 巨大な名前は変換前に上限超過として拒否する（アロケーション前検査）。
        let huge = "a".repeat(1 << 20);
        assert!(matches!(
            validate_single_component(OsStr::new(&huge)),
            Err(ProfileError::InvalidComponent { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            // 不正 UTF-8 バイトの長大な名前も拒否する。
            let bad = vec![0xFFu8; MAX_COMPONENT_UTF16_LEN + 1];
            assert!(matches!(
                validate_single_component(OsStr::from_bytes(&bad)),
                Err(ProfileError::InvalidComponent { .. })
            ));
            let bad_huge = vec![0xFFu8; 1 << 20];
            assert!(matches!(
                validate_single_component(OsStr::from_bytes(&bad_huge)),
                Err(ProfileError::InvalidComponent { .. })
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let lone: Vec<u16> = vec![0xD800; MAX_COMPONENT_UTF16_LEN + 1];
            assert!(matches!(
                validate_single_component(&OsString::from_wide(&lone)),
                Err(ProfileError::InvalidComponent { .. })
            ));
        }
    }

    #[test]
    fn prof4_xos10_validate_single_component_accepts_plain_names() {
        let max = "a".repeat(MAX_COMPONENT_UTF16_LEN);
        for name in ["profile.lock", "cookies", "Dir", max.as_str()] {
            assert!(
                validate_single_component(OsStr::new(name)).is_ok(),
                "must accept {name:?}"
            );
        }
    }
}
