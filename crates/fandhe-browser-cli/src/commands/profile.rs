//! 全コマンド共通のプロファイル保管場所指定 `--profile-dir <PATH>`（TASK-47.6・Issue #287・`CLI-1`・MS-5）。
//!
//! `launch`・`navigate`・`query`・`screenshot` が同じ解析ヘルパ [`take_profile_dir`] を通り、
//! 4 コマンドで同一書式（サブコマンド名の後ろ・別引数の値）を受け付ける。パスの検証と保管は
//! profile crate に委譲し、cli では自作しない（`PROF-1`・`PROF-4`）。
//!
//! - 字句検証（空・`..` 畳み込み後のファイルシステムルート・symlink の後の `..`・既存の共有ディレクトリ）は
//!   `ExplicitStore::new` が副作用の前に行う。[`ProfileOptionError::InvalidPath`]（終了コード 2）
//! - 祖先 symlink の拒否・排他ロック・0700・Windows の `%LOCALAPPDATA%` 限定は `Profile::open` が行う。
//!   [`ProfileOptionError::Open`]（終了コード 1）
//!
//! 名前は `--profile` でなく `--profile-dir` とした。core の `cssom_profile`（`PLUG-8`・TASK-100）が
//! ブラウザ挙動プロファイルの `--profile chrome|safari` を cli の契約として予約しているため。
//! `--profile` は未知のオプションのまま残す。
//!
//! one-shot コマンドは指定があるときだけ [`open_explicit`] でプロファイルを開き、コマンド終了まで
//! `profile.lock` を保持する。指定が無ければ OS 既定のプロファイルにも触れない。
//!
//! # スタブについて（REPAIR-3）
//!
//! - one-shot コマンドでのプロファイルデータ（Cookie 等）の実利用は TASK-116・`PROF-7`。現状は
//!   検証・レイアウト作成・ロック保持のみ
//! - 設定ファイルの `[profile] root` は未配線（`startup_config` が拒否）。優先順位は
//!   `--profile-dir` > 設定 > OS 既定の予定（TASK-60.4）
//! - 名前によるプロファイル選択は `XOS-9`・TASK-62

use std::fmt;
use std::path::PathBuf;

use fandhe_browser_profile::{
    ExplicitStore, OverridableStore, Profile, ProfileError, ProfileStore,
};

use super::UsageError;
use crate::server::UNSUPPORTED_OS_MESSAGE;

/// 全コマンド共通のオプション名。
pub(crate) const PROFILE_DIR_OPTION: &str = "--profile-dir";
/// 値の位置引数名（usage エラー表示用）。
const VALUE_NAME: &str = "PATH";

/// プロファイルの選択方法。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum ProfileSelection {
    /// OS 既定のプロファイルルート。
    OsDefault,
    /// `--profile-dir` で明示指定された保管場所（未検証。検証は [`build_store`]）。
    Explicit(PathBuf),
}

/// 各コマンドの解析ループから呼ぶ。`arg` が `--profile-dir` なら値を `iter` から消費して
/// `current` に設定し `true` を返す。それ以外は何もせず `false`（副作用なし・パスの検証もしない）。
pub(crate) fn take_profile_dir<'a>(
    arg: &str,
    iter: &mut impl Iterator<Item = &'a String>,
    current: &mut ProfileSelection,
) -> Result<bool, UsageError> {
    if arg != PROFILE_DIR_OPTION {
        return Ok(false);
    }
    if matches!(current, ProfileSelection::Explicit(_)) {
        return Err(UsageError::UnexpectedArgument(arg.to_string()));
    }
    let value = iter.next().ok_or(UsageError::MissingArgument(VALUE_NAME))?;
    if value.starts_with('-') {
        // 後続のオプションを値として飲み込まない。
        return Err(UsageError::MissingArgument(VALUE_NAME));
    }
    if value.is_empty() {
        return Err(UsageError::InvalidOptionValue {
            option: PROFILE_DIR_OPTION,
        });
    }
    *current = ProfileSelection::Explicit(PathBuf::from(value));
    Ok(true)
}

/// プロファイル指定の失敗。入力のパスは表示に含めない（端末出力の安全・情報漏えい防止）。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum ProfileOptionError {
    /// パスの検証に失敗した（終了コード 2）。
    InvalidPath { reason: &'static str },
    /// プロファイルを開けなかった（終了コード 1）。
    Open(ProfileError),
}

impl ProfileOptionError {
    /// 利用者の入力（パス）が原因の検証エラーか。終了コード 2 に写す。
    pub(crate) fn is_validation(&self) -> bool {
        matches!(self, Self::InvalidPath { .. })
    }
}

impl fmt::Display for ProfileOptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { reason } => write!(f, "invalid profile directory: {reason}"),
            Self::Open(ProfileError::Locked { .. }) => f.write_str("profile is already in use"),
            Self::Open(ProfileError::Unsupported { .. }) => f.write_str(UNSUPPORTED_OS_MESSAGE),
            Self::Open(ProfileError::InvalidLayout { reason, .. }) => {
                write!(f, "invalid profile directory: {reason}")
            }
            Self::Open(_) => f.write_str("failed to open profile"),
        }
    }
}

impl std::error::Error for ProfileOptionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPath { .. } => None,
            Self::Open(e) => Some(e),
        }
    }
}

/// 選択からストアを作る。`Explicit` は `ExplicitStore::new` で検証する（失敗時は何も作らない）。
pub(crate) fn build_store(
    selection: &ProfileSelection,
) -> Result<OverridableStore, ProfileOptionError> {
    match selection {
        ProfileSelection::OsDefault => Ok(OverridableStore::new(None)),
        ProfileSelection::Explicit(path) => {
            let explicit = ExplicitStore::new(path.clone()).map_err(|e| match e {
                ProfileError::InvalidLayout { reason, .. } => {
                    ProfileOptionError::InvalidPath { reason }
                }
                ProfileError::Io(_) => ProfileOptionError::InvalidPath {
                    reason: "cannot access profile directory",
                },
                _ => ProfileOptionError::InvalidPath {
                    reason: "invalid profile directory",
                },
            })?;
            Ok(OverridableStore::new(Some(explicit)))
        }
    }
}

/// one-shot コマンド用。明示指定があるときだけプロファイルを開いて返す（呼び出し側が終了まで保持する）。
/// `OsDefault` は OS 既定に触れず `Ok(None)`。
pub(crate) fn open_explicit(
    selection: &ProfileSelection,
) -> Result<Option<Profile>, ProfileOptionError> {
    if matches!(selection, ProfileSelection::OsDefault) {
        return Ok(None);
    }
    let store = build_store(selection)?;
    store
        .open_or_create()
        .map(Some)
        .map_err(ProfileOptionError::Open)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::server::tests::support::TempDir;

    fn parse(v: &[&str]) -> Result<ProfileSelection, UsageError> {
        let owned: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        let mut iter = owned.iter();
        let mut sel = ProfileSelection::OsDefault;
        while let Some(a) = iter.next() {
            take_profile_dir(a, &mut iter, &mut sel)?;
        }
        Ok(sel)
    }

    #[test]
    fn cli1_profile_dir_parse_forms() {
        assert_eq!(parse(&[]), Ok(ProfileSelection::OsDefault));
        assert_eq!(
            parse(&["--profile-dir", "p"]),
            Ok(ProfileSelection::Explicit(PathBuf::from("p")))
        );
        assert_eq!(
            parse(&["--profile-dir"]),
            Err(UsageError::MissingArgument("PATH"))
        );
        assert_eq!(
            parse(&["--profile-dir", "-x"]),
            Err(UsageError::MissingArgument("PATH"))
        );
        assert_eq!(
            parse(&["--profile-dir", ""]),
            Err(UsageError::InvalidOptionValue {
                option: "--profile-dir"
            })
        );
        assert_eq!(
            parse(&["--profile-dir", "a", "--profile-dir", "b"]),
            Err(UsageError::UnexpectedArgument("--profile-dir".into()))
        );
    }

    #[test]
    fn cli1_profile_dir_ignores_other_args() {
        let owned: Vec<String> = Vec::new();
        let mut iter = owned.iter();
        let mut sel = ProfileSelection::OsDefault;
        assert_eq!(take_profile_dir("--all", &mut iter, &mut sel), Ok(false));
        assert_eq!(sel, ProfileSelection::OsDefault);
    }

    #[test]
    fn prof1_filesystem_root_is_rejected_as_validation_error() {
        let root = std::path::absolute("/").expect("absolute");
        let err = build_store(&ProfileSelection::Explicit(root)).expect_err("root rejected");
        assert!(err.is_validation(), "{err}");
        assert_eq!(
            err.to_string(),
            "invalid profile directory: explicit profile root must not be the filesystem root"
        );
    }

    #[test]
    fn prof1_error_display_does_not_echo_path() {
        let marker = "uniq-marker-zzz";
        let err = build_store(&ProfileSelection::Explicit(PathBuf::from(format!(
            "/{marker}/.."
        ))))
        .expect_err("root rejected");
        assert!(!err.to_string().contains(marker), "{err}");
    }

    #[test]
    fn prof1_os_default_opens_nothing() {
        assert!(matches!(
            open_explicit(&ProfileSelection::OsDefault),
            Ok(None)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn prof1_symlink_then_dotdot_is_rejected() {
        let t = TempDir::new();
        let other = t.owned.join("other");
        std::fs::create_dir(&other).expect("mkdir");
        let link = t.owned.join("link");
        std::os::unix::fs::symlink(&other, &link).expect("symlink");
        let err = build_store(&ProfileSelection::Explicit(link.join("..").join("p")))
            .expect_err("rejected");
        assert!(err.is_validation(), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn prof1_existing_shared_dir_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new();
        std::fs::set_permissions(&t.owned, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let err = build_store(&ProfileSelection::Explicit(t.owned.clone())).expect_err("shared");
        assert!(err.is_validation(), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn prof1_ancestor_symlink_is_rejected_on_open_and_creates_nothing() {
        let t = TempDir::new();
        let other = t.owned.join("other");
        std::fs::create_dir(&other).expect("mkdir");
        let link = t.owned.join("link");
        std::os::unix::fs::symlink(&other, &link).expect("symlink");
        let err = open_explicit(&ProfileSelection::Explicit(link.join("p")))
            .expect_err("symlink rejected");
        assert!(matches!(err, ProfileOptionError::Open(_)), "{err}");
        assert!(!err.is_validation());
        assert!(!other.join("p").exists());
    }

    #[cfg(unix)]
    #[test]
    fn prof1_dotdot_folding_to_valid_root_is_accepted_without_side_dirs() {
        let t = TempDir::new();
        let p = t.owned.join("new").join("..").join("p");
        let profile = open_explicit(&ProfileSelection::Explicit(p))
            .expect("opens")
            .expect("some");
        assert!(t.owned.join("p").join("profile.lock").exists());
        assert!(!t.owned.join("new").exists());
        drop(profile);
    }

    #[cfg(unix)]
    #[test]
    fn prof4_second_open_of_same_dir_is_locked() {
        let t = TempDir::new();
        let sel = ProfileSelection::Explicit(t.path.clone());
        let _first = open_explicit(&sel).expect("first").expect("some");
        let err = open_explicit(&sel).expect_err("locked");
        assert!(matches!(err, ProfileOptionError::Open(_)));
        assert_eq!(err.to_string(), "profile is already in use");
        assert!(!err.is_validation());
    }
}
