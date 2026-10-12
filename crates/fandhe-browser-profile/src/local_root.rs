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
//! - 作成後: 作ったルートを再度 `canonicalize` し、期待する実体パスと一致することを
//!   確認する（判定と作成の間にジャンクション等へ差し替えられた場合の検出。TOCTOU）。
//!   検出時に、それまでに作ったディレクトリは削除しない（削除先が差し替え後の実体だと
//!   境界外を消しかねないため。unix 側も途中で作ったディレクトリを戻さない方針に揃える）。
//!
//! アクセス制限は OS の既定の継承 ACL に依存し、ACL の設定・検証はしない
//! （`%LOCALAPPDATA%` 配下に新規作成したディレクトリは SYSTEM・Administrators・
//! 本人のみにアクセス権が付く。実測は Issue #813 のコメント）。Administrators と
//! SYSTEM は読める。ACL を明示設定して任意のパスを許可する第 2 段階は依存の
//! feature 追加と `unsafe` の承認が要るため未実装（後続。未判断）。
//! 対応するビヘイビアは `PROF-1`・`PROF-6`・`XOS-7`・`XOS-10`（Issue #928、#813）。
//!
//! 判定ロジックは `std::fs` と `std::path` だけで書いてあり、Linux・macOS でも
//! コンパイル・テストできる（Windows 形式のパス固有の挙動は `cfg(windows)` のテストが担う）。
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

use std::ffi::OsString;
use std::fs::File;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::lock::LOCK_FILE_NAME;
use crate::profile::{DataKind, ProfileError};

/// `LOCALAPPDATA` が使えない場合の拒否理由。
const REASON_NO_LOCALAPPDATA: &str = "LOCALAPPDATA must be set to an absolute path of an existing directory; on Windows, profile roots are only allowed under %LOCALAPPDATA% (XOS-7, XOS-10)";
/// ルートが配下でない場合の拒否理由。
const REASON_OUTSIDE: &str = "on Windows, the profile root must be a directory strictly under %LOCALAPPDATA% (inherited ACL is relied on; other locations are not supported yet; XOS-7, XOS-10)";
/// 正規化など、判定に必要な操作が失敗した場合の拒否理由。
const REASON_CANNOT_VERIFY: &str = "on Windows, the profile root could not be verified to be under %LOCALAPPDATA% (path resolution failed); refusing to create it";
/// `.` / `..` / 非ドライブのプレフィックスを含むパスの拒否理由。
const REASON_UNSUPPORTED_FORM: &str = "on Windows, the profile root must be a plain drive-absolute path without '.' or '..' components (UNC and device paths are not supported)";

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

/// 作成前の判定結果。ファイルシステムには何も作っていない。
#[derive(Debug)]
pub(crate) struct RootPlan {
    /// `canonicalize` した `%LOCALAPPDATA%`。
    base: PathBuf,
    /// 作成後にルートがあるべき実体パス（存在する祖先の実体 + 残りの要素）。
    expected: PathBuf,
}

impl RootPlan {
    /// 以降の操作の基準にする実体パス。
    pub(crate) fn expected(&self) -> &Path {
        &self.expected
    }
}

/// `LOCALAPPDATA` の値を検証し、`canonicalize` した実体パスを返す。
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
    Ok(base)
}

/// 新規作成する要素の名前として許さない文字を含むか（Windows のみ判定）。
///
/// `:` は代替データストリーム、他は予約文字。verbatim 形では `/` が区切りにならず
/// 名前に含まれ得るため、ここで拒否する。末尾の `.`・空白は Win32 の正規化で名前が
/// 変わり、期待する実体と食い違うため拒否する。
#[cfg(windows)]
fn is_unsafe_new_name(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    s.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
        || s.ends_with('.')
        || s.ends_with(' ')
}

#[cfg(not(windows))]
fn is_unsafe_new_name(_name: &std::ffi::OsStr) -> bool {
    false
}

/// `root` が `%LOCALAPPDATA%` の厳密な配下に収まるかを判定する（副作用なし。fail-closed）。
///
/// 配下外・判定不能は [`ProfileError::Unsupported`]、存在する祖先がディレクトリでない
/// 場合は [`ProfileError::InvalidLayout`] を返す。いずれの場合もディレクトリは作らない
/// （`PROF-1`・`XOS-7`・`XOS-10`）。
pub(crate) fn plan_root(
    root: &Path,
    local_app_data: Option<OsString>,
) -> Result<RootPlan, ProfileError> {
    let base = resolve_base(local_app_data)?;
    let absolute = std::path::absolute(root).map_err(|_| unsupported(REASON_CANNOT_VERIFY))?;

    // `..`・`.`、ドライブ以外のプレフィックス（UNC・デバイス）を持つパスは受け付けない。
    // verbatim 形では `..` が実在の名前として扱われ、字句的に畳めない。
    for component in absolute.components() {
        match component {
            Component::Normal(_) | Component::RootDir => {}
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

    // 存在する最も深い祖先を探す。NotFound 以外のエラーは判定不能として拒否する。
    let mut existing: Option<&Path> = None;
    for ancestor in absolute.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                existing = Some(ancestor);
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(unsupported(REASON_CANNOT_VERIFY)),
        }
    }
    let existing = existing.ok_or(unsupported(REASON_CANNOT_VERIFY))?;
    let canonical_existing =
        std::fs::canonicalize(existing).map_err(|_| unsupported(REASON_CANNOT_VERIFY))?;
    let existing_is_dir = std::fs::metadata(&canonical_existing).is_ok_and(|m| m.is_dir());

    let rest = absolute
        .strip_prefix(existing)
        .map_err(|_| unsupported(REASON_CANNOT_VERIFY))?;
    if rest
        .components()
        .any(|c| !matches!(c, Component::Normal(name) if !is_unsafe_new_name(name)))
    {
        return Err(unsupported(REASON_UNSUPPORTED_FORM));
    }
    let expected = canonical_existing.join(rest);

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

/// ルートを作成した後に、実体が作成前の判定どおりであることを再確認する。
///
/// 判定と作成の間にジャンクション・symlink へ差し替えられていれば、`canonicalize`
/// の結果が期待する実体パスと食い違う（`PROF-1`・`PROF-4`、TOCTOU 対策）。
/// 食い違いの場合は [`ProfileError::InvalidLayout`]（作ったものは消さない。モジュール doc）。
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
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::fs::canonicalize(std::env::temp_dir()).expect("temp_dir");
            let path = base.join(format!("fandhe-localroot-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn lad(dir: &Path) -> Option<OsString> {
        Some(dir.as_os_str().to_os_string())
    }

    fn is_unsupported(result: &Result<RootPlan, ProfileError>) -> bool {
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

    /// XOS-10: 新規ルートは判定だけでは何も作らず、期待する実体パスを返す。
    #[test]
    fn xos10_plan_accepts_new_root_under_base_without_creating() {
        let tmp = TempDir::new();
        let root = tmp.0.join("a").join("b");
        let plan = plan_root(&root, lad(&tmp.0)).expect("配下は許可される");
        assert_eq!(plan.expected(), root.as_path());
        assert!(!tmp.0.join("a").exists(), "判定だけでは作らない");
    }

    /// XOS-10: 既存ルートも許可される。
    #[test]
    fn xos10_plan_accepts_existing_root() {
        let tmp = TempDir::new();
        let root = tmp.0.join("existing");
        std::fs::create_dir(&root).unwrap();
        let plan = plan_root(&root, lad(&tmp.0)).expect("既存でも許可");
        assert_eq!(plan.expected(), root.as_path());
    }

    /// PROF-1・XOS-10: `LOCALAPPDATA` と同一のルートは他アプリのデータと混ざるため拒否。
    #[test]
    fn xos10_plan_rejects_root_equal_to_base() {
        let tmp = TempDir::new();
        assert!(is_unsupported(&plan_root(&tmp.0, lad(&tmp.0))));
    }

    /// PROF-1・XOS-10: 親・兄弟・名前が前方一致するだけのディレクトリは配下でない。
    #[test]
    fn xos10_plan_rejects_parent_sibling_and_prefix_lookalike() {
        let tmp = TempDir::new();
        let base = tmp.0.join("Local");
        let evil = tmp.0.join("LocalEvil");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&evil).unwrap();
        for root in [
            tmp.0.clone(),
            tmp.0.join("sibling"),
            evil.join("p"),
            evil.clone(),
        ] {
            assert!(
                is_unsupported(&plan_root(&root, lad(&base))),
                "{root:?} が拒否されなかった"
            );
        }
        assert!(!tmp.0.join("sibling").exists());
        assert!(!evil.join("p").exists());
    }

    /// PROF-4・XOS-10: `..` を含むパスは配下外へ出られない。
    #[test]
    fn xos10_plan_rejects_dotdot_escape() {
        let tmp = TempDir::new();
        let base = tmp.0.join("Local");
        std::fs::create_dir(&base).unwrap();
        let root = base.join("..").join("escaped");
        assert!(is_unsupported(&plan_root(&root, lad(&base))));
        assert!(!tmp.0.join("escaped").exists());
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

    /// PROF-1: 祖先がディレクトリでない場合は `InvalidLayout`。
    #[test]
    fn prof1_plan_rejects_non_directory_ancestor() {
        let tmp = TempDir::new();
        let file = tmp.0.join("file");
        std::fs::write(&file, b"x").unwrap();
        let result = plan_root(&file.join("p"), lad(&tmp.0));
        assert!(
            matches!(
                result,
                Err(ProfileError::InvalidLayout { .. }) | Err(ProfileError::Unsupported { .. })
            ),
            "{result:?}"
        );
    }

    /// PROF-4・XOS-10: 配下のシンボリックリンクが配下外を指す場合は拒否（unix で検証）。
    #[cfg(unix)]
    #[test]
    fn xos10_plan_rejects_symlink_escape() {
        let tmp = TempDir::new();
        let base = tmp.0.join("Local");
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();
        let root = base.join("link").join("p");
        assert!(is_unsupported(&plan_root(&root, lad(&base))));
        assert!(!outside.join("p").exists());
    }

    /// PROF-1・PROF-4: 作成後に実体が差し替わっていれば検出する（unix で検証）。
    #[cfg(unix)]
    #[test]
    fn prof1_verify_detects_redirect_after_plan() {
        let tmp = TempDir::new();
        let base = tmp.0.join("Local");
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = base.join("p");
        let plan = plan_root(&root, lad(&base)).unwrap();
        // 判定後、作成前に配下外への symlink へ差し替えられた状況。
        std::os::unix::fs::symlink(&outside, &root).unwrap();
        std::fs::create_dir_all(plan.expected()).unwrap();
        let err = verify_created_root(&plan).expect_err("差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
    }

    /// PROF-1: 差し替えがなければ検証は通り、ロックファイルと 4 サブディレクトリが作られる。
    #[test]
    fn prof1_create_lock_and_data_dirs() {
        let tmp = TempDir::new();
        let root = tmp.0.join("p");
        let plan = plan_root(&root, lad(&tmp.0)).unwrap();
        std::fs::create_dir_all(plan.expected()).unwrap();
        verify_created_root(&plan).expect("通常のディレクトリは検証を通る");
        let (_file, lock_path) = open_lock_file(plan.expected()).unwrap();
        assert_eq!(lock_path, root.join("profile.lock"));
        ensure_data_dirs(plan.expected()).unwrap();
        for name in ["cookies", "storage", "cache", "history"] {
            assert!(root.join(name).is_dir(), "{name} がない");
        }
        assert!(lock_path.is_file());
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
    /// 差し替えられても、作成後の再確認が検出する（TOCTOU）。
    #[cfg(windows)]
    #[test]
    fn prof1_windows_verify_detects_junction_swapped_after_plan() {
        let tmp = TempDir::new();
        let base = tmp.0.join("Local");
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = base.join("p");
        let plan = plan_root(&root, lad(&base)).unwrap();
        // cmd.exe は `\\?\` 付きパスを扱えないため、ドライブ形式へ戻して渡す。
        let plain = |p: &Path| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"));
        let output = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(plain(&root))
            .arg(plain(&outside))
            .output()
            .expect("cmd.exe を起動できる");
        assert!(
            output.status.success(),
            "mklink /J failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::create_dir_all(plan.expected()).unwrap();
        let err = verify_created_root(&plan).expect_err("差し替えを検出する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }), "{err:?}");
    }
}
