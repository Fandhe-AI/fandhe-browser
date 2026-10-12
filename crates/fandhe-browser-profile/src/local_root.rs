//! Windows 向け: プロファイルルートが `%LOCALAPPDATA%` 配下かを判定し、
//! ルート・`profile.lock`・データ種別ディレクトリを作る補助モジュール。
//!
//! `profile.rs` の `Profile::open`（Windows 分岐）から呼ばれる。Windows には
//! unix の `openat` + `NOFOLLOW` に相当するハンドル基準の走査（std だけでは書けない。
//! Win32 API の直接呼び出しと `unsafe` は使わない方針）がないため、次の方針で境界を守る。
//!
//! - 作成前: ルートの絶対パスから、存在する最も深い祖先を `canonicalize`（短縮名・
//!   大文字小文字・`\\?\` 表記・symlink／ジャンクションを実体に解決）し、まだ存在しない
//!   残りの要素を足した「期待する実体パス」が、同じく `canonicalize` した
//!   `%LOCALAPPDATA%` の**厳密な配下**であることをコンポーネント単位で確認する
//!   （文字列の前方一致ではない。`...\Local` と `...\LocalEvil` を取り違えない）。
//!   `LOCALAPPDATA` 自体も、実体が `...\AppData\Local` で終わり UNC でないことを要求する。
//! - 作成時: 存在する最深の祖先から 1 要素ずつ `create_dir` し、作るたびにその要素の
//!   `canonicalize` が期待する実体パスと一致することを確認する（[`create_root`]）。
//!   食い違い（ジャンクション・symlink への差し替え）を見つけた時点で止める。
//! - 作成後: `profile.lock` とデータ種別ディレクトリも、作る・開くたびに実体を確認する。
//!   検出時に、それまでに作ったディレクトリは削除しない（削除先が差し替え後の実体だと
//!   境界外を消しかねないため。unix 側も途中で作ったディレクトリを戻さない方針に揃える）。
//!
//! アクセス制限は OS の既定の継承 ACL に依存し、ACL の設定・検証はしない
//! （既定の継承 ACL のままなら、`%LOCALAPPDATA%` 配下に新規作成したディレクトリは
//! SYSTEM・Administrators・本人のみにアクセス権が付く。実測は Issue #813 のコメント）。
//! Administrators と SYSTEM は読める。ACL を明示設定して任意のパスを許可する第 2 段階
//! （既知フォルダ API を含む）は依存の feature 追加と `unsafe` の承認が要るため未実装
//! （後続。未判断）。対応するビヘイビアは `PROF-1`・`PROF-6`・`XOS-7`・`XOS-10`
//! （Issue #928、#813）。
//!
//! ## 前提とする攻撃者像
//!
//! 同じユーザー権限で動く別プロセス。ただしその攻撃者は `%LOCALAPPDATA%` 配下を元々
//! 読み書きできるため、本モジュールが守るのは「別ユーザー・配下外への書き込み」と
//! 「判定と作成の競合で配下外へ出ること」であり、同一ユーザーからの読み取りは防がない。
//!
//! ## 限界（`REPAIR-3`。実装済みを装わない）
//!
//! - 判定と作成の間に差し替えられると、検出して拒否する前に、配下外へ空ディレクトリが
//!   最大 1 つ作られ得る（1 要素ずつ作り、作るたびに確認するため）。作ったものは消さない
//!   （削除先が差し替え後の実体である恐れがあるため。unix 側も戻さない）。
//! - 作成後の確認の後にルートが差し替えられると、差し替え先に空の `profile.lock` と
//!   空のサブディレクトリ 1 つが作られ得る。その場合は `InvalidLayout` で `Profile` は返らない。
//! - `open` の成功後は、`Profile` がパスだけを持つため、ルートの差し替えを検出できない。
//!   Windows では書き込み系の操作（`delete` 等）が未実装で現時点の実害は無いが、書き込み系を
//!   Windows で有効にする際はハンドル基準の実装が必須。
//! - `%LOCALAPPDATA%` の内側へ向くジャンクションは許可する（unix は途中の symlink を
//!   すべて拒否する。差がある）。
//! - 既存のルート・既存の中間ディレクトリの ACL は確認しない。独自の ACL が設定されて
//!   いれば、継承 ACL の前提は成り立たない。
//! - `profile.lock` の所有者・ハードリンク数は検査しない（unix との差）。
//!
//! 判定ロジックは `std::fs` と `std::path` だけで書いてあり、Linux・macOS でも
//! コンパイル・テストできる（Windows 形式のパス固有の挙動は `cfg(windows)` のテストが担う）。
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::lock::LOCK_FILE_NAME;
use crate::profile::{DataKind, ProfileError};

/// `LOCALAPPDATA` が使えない場合の拒否理由。
const REASON_NO_LOCALAPPDATA: &str = "LOCALAPPDATA must be set to an absolute path of an existing directory; on Windows, profile roots are only allowed under %LOCALAPPDATA% (XOS-7, XOS-10)";
/// `LOCALAPPDATA` が UNC、または `...\AppData\Local` で終わらない場合の拒否理由。
const REASON_BAD_LOCALAPPDATA: &str = "LOCALAPPDATA must resolve to a local drive path ending in AppData\\Local (UNC and redirected locations are not supported; the inherited-ACL assumption was only verified for the default location; XOS-7, XOS-10)";
/// ルートが配下でない場合の拒否理由。
const REASON_OUTSIDE: &str = "on Windows, the profile root must be a directory strictly under %LOCALAPPDATA% (inherited ACL is relied on; other locations are not supported yet; XOS-7, XOS-10)";
/// 正規化など、判定に必要な操作が失敗した場合の拒否理由。
const REASON_CANNOT_VERIFY: &str = "on Windows, the profile root could not be verified to be under %LOCALAPPDATA% (path resolution failed); refusing to create it";
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

/// 作成前の判定結果。ファイルシステムには何も作っていない。
#[derive(Debug)]
pub(crate) struct RootPlan {
    /// `canonicalize` した `%LOCALAPPDATA%`。
    base: PathBuf,
    /// 存在する最も深い祖先の実体パス（これ以降の要素を 1 つずつ作る）。
    anchor: PathBuf,
    /// `anchor` の下に作る要素（すべて `Normal`、名前検査済み）。
    rest: Vec<OsString>,
    /// 作成後にルートがあるべき実体パス（`anchor` + `rest`）。
    expected: PathBuf,
}

impl RootPlan {
    /// 以降の操作の基準にする実体パス。
    pub(crate) fn expected(&self) -> &Path {
        &self.expected
    }
}

/// `LOCALAPPDATA` の値を検証し、`canonicalize` した実体パスを返す。
///
/// 継承 ACL の実測（#813）は既定の `C:\Users\<user>\AppData\Local` で行われたため、
/// 実体が UNC（フォルダリダイレクト・マップドドライブの先）や、末尾が
/// `AppData\Local` でない場所（`C:\ProgramData` など）は認めない。緩める場合は
/// 第 2 段階（既知フォルダ API で取得した値の検証）で扱う。
fn resolve_base(local_app_data: Option<OsString>) -> Result<PathBuf, ProfileError> {
    let value = local_app_data
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
    local_app_data: Option<OsString>,
) -> Result<RootPlan, ProfileError> {
    let base = resolve_base(local_app_data)?;

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
    let mut rest = Vec::new();
    for component in rest_path.components() {
        match component {
            Component::Normal(name) if !is_invalid_new_name(name) => rest.push(name.to_os_string()),
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
    Ok(RootPlan {
        base,
        anchor: canonical_existing,
        rest,
        expected,
    })
}

/// ルートを、存在する最深の祖先から 1 要素ずつ作り、作るたびに実体を確認する。
///
/// `create_dir_all` と違い、差し替えられたジャンクション・symlink の先に未存在の要素を
/// 複数作らない。各要素を `create_dir` した直後にその `canonicalize` が期待する実体パスと
/// 一致することを確認し、不一致ならそこで [`ProfileError::InvalidLayout`] を返して止める
/// （配下外に作られ得るのは最大で空ディレクトリ 1 つ。作ったものは消さない）。
/// `AlreadyExists`（並行して同じルートを開く別プロセスや既存の要素）は成功扱いにして
/// 同じ確認へ進む。全要素を作り終えたら [`verify_created_root`] で最終確認する
/// （`PROF-1`・`PROF-4`、TOCTOU 対策）。OS 非依存。
pub(crate) fn create_root(plan: &RootPlan) -> Result<(), ProfileError> {
    let mut current = plan.anchor.clone();
    for name in &plan.rest {
        current.push(name);
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(ProfileError::Io(err)),
        }
        let actual = std::fs::canonicalize(&current).map_err(ProfileError::Io)?;
        let is_dir = std::fs::symlink_metadata(&actual).is_ok_and(|m| m.is_dir());
        if actual != current || !is_dir {
            return Err(ProfileError::InvalidLayout {
                path: current,
                reason: "profile root path component was replaced, redirected (symlink/junction) or is not a directory",
            });
        }
    }
    verify_created_root(plan)
}

/// ルートの実体が作成前の判定どおりであることを再確認する。
///
/// 食い違い（ジャンクション・symlink への差し替え）は [`ProfileError::InvalidLayout`]
/// （作ったものは消さない。モジュール doc の「限界」）。
pub(crate) fn verify_created_root(plan: &RootPlan) -> Result<(), ProfileError> {
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

/// `FILE_FLAG_OPEN_REPARSE_POINT`。リンク先を辿らず再解析ポイント自体を開く
/// （std の `OpenOptionsExt::custom_flags` に渡すだけで Win32 の直接呼び出しではない）。
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// `root`（検証済みの実体パス）直下の `profile.lock` を開く（無ければ作る）。
///
/// symlink 等はリンク先を辿らず開き、通常ファイルでなければ
/// [`ProfileError::InvalidLayout`] で拒否する。unix 側が行う所有者・ハードリンク数の
/// 検査に相当するものは Windows にはない（ACL は継承に依存し検証しない。REPAIR-3）。
pub(crate) fn open_lock_file(root: &Path) -> Result<(File, PathBuf), ProfileError> {
    let path = root.join(LOCK_FILE_NAME);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(&path).map_err(ProfileError::Io)?;
    let metadata = file.metadata().map_err(ProfileError::Io)?;
    if !metadata.is_file() {
        return Err(ProfileError::InvalidLayout {
            path,
            reason: "profile lock path is not a regular file",
        });
    }
    Ok((file, path))
}

/// `root`（検証済みの実体パス）直下に 4 つのデータ種別ディレクトリを作り、
/// 実体が `root` の直下であること（symlink・ジャンクションでないこと）を確認する。
pub(crate) fn ensure_data_dirs(root: &Path) -> Result<(), ProfileError> {
    for kind in DataKind::ALL {
        let path = root.join(kind.dir_name());
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(ProfileError::Io(err)),
        }
        let actual = std::fs::canonicalize(&path).map_err(ProfileError::Io)?;
        let is_dir = std::fs::symlink_metadata(&actual).is_ok_and(|m| m.is_dir());
        if !is_dir || actual.parent() != Some(root) || actual.file_name() != path.file_name() {
            return Err(ProfileError::InvalidLayout {
                path,
                reason: "profile data directory is not a plain directory directly under the root",
            });
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
    fn prof1_create_root_lock_and_data_dirs() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("p").join("q");
        let plan = plan_root(&root, env(&lad)).unwrap();
        create_root(&plan).expect("通常のディレクトリは作れる");
        create_root(&plan).expect("既存（AlreadyExists）でも成功する");
        let (_file, lock_path) = open_lock_file(plan.expected()).unwrap();
        assert_eq!(lock_path, root.join("profile.lock"));
        ensure_data_dirs(plan.expected()).unwrap();
        for name in ["cookies", "storage", "cache", "history"] {
            assert!(root.join(name).is_dir(), "{name} がない");
        }
        assert!(lock_path.is_file());
    }

    /// PROF-1: 作成の途中の要素が（判定後に）通常ファイルへ差し替わった場合は `InvalidLayout`
    /// で止まり、その先を作らない。
    #[test]
    fn prof1_create_root_stops_at_non_directory_component() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let plan = plan_root(&lad.join("p").join("q").join("r"), env(&lad)).unwrap();
        std::fs::write(lad.join("p"), b"x").unwrap();
        let err = create_root(&plan).expect_err("非ディレクトリで止まる");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert!(lad.join("p").is_file());
    }

    /// PROF-1・PROF-4: 判定後、作成前にルートが配下外へのリンクに差し替えられると、
    /// 検出して拒否し、配下外に子を作らない（unix で検証）。
    #[cfg(unix)]
    #[test]
    fn prof1_create_root_detects_redirect_after_plan() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let plan = plan_root(&lad.join("p"), env(&lad)).unwrap();
        std::os::unix::fs::symlink(&outside, lad.join("p")).unwrap();
        let err = create_root(&plan).expect_err("差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// PROF-1・PROF-4: 途中の要素が差し替えられた場合は 1 段で止まり、その先に 2 段目
    /// （`q`）が作られていない（unix で検証。`create_dir_all` では `outside/q` ができる）。
    #[cfg(unix)]
    #[test]
    fn prof1_create_root_stops_after_one_level_when_intermediate_swapped() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let plan = plan_root(&lad.join("p").join("q").join("r"), env(&lad)).unwrap();
        std::os::unix::fs::symlink(&outside, lad.join("p")).unwrap();
        let err = create_root(&plan).expect_err("途中の差し替えを検出する");
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
        let result = open_lock_file(&root);
        assert!(result.is_err(), "{result:?}");
    }

    /// PROF-1: データ種別ディレクトリの位置が通常ファイルなら `InvalidLayout`。
    #[test]
    fn prof1_ensure_data_dirs_rejects_file_in_subdir_position() {
        let tmp = TempDir::new();
        let lad = tmp.lad();
        let root = lad.join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cookies"), b"x").unwrap();
        let err = ensure_data_dirs(&root).expect_err("ファイルは拒否される");
        assert!(
            matches!(
                err,
                ProfileError::InvalidLayout { .. } | ProfileError::Io(_)
            ),
            "{err:?}"
        );
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
    fn prof1_windows_create_root_detects_junction_swapped_after_plan() {
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
        let err = create_root(&plan).expect_err("差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "配下外に作られた"
        );
    }
}
