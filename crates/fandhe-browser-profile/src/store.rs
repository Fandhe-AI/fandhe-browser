//! プロファイル保存先の解決・作成・削除を OS 差異ごと隠す抽象層。
//!
//! `fandhe-browser-cli`（TASK-41 で追加予定）が [`ProfileStore`] 越しに
//! [`Profile`] を得る設計を目指す（`XOS-7`、TASK-60（60.2）、MS-3）。保存先の
//! 慣習（Linux の XDG・macOS の Application Support・Windows の AppData）、
//! パス長、大文字小文字、ロックといった OS 差異をこのトレイトの背後へ閉じ込める。
//!
//! ## 実装者が守る契約
//!
//! - [`ProfileStore::resolve_root`] は絶対パスを返す。相対パスは
//!   [`ProfileStore::open_or_create`] が [`ProfileError::InvalidLayout`] で拒否する
//! - ルート経路上の symlink の検証は [`Profile::open`] がハンドル基準で行う
//!   （`PROF-1`・`PROF-4`）。macOS の `/var` のような OS 標準の symlink を含む
//!   パスは、[`Profile::open`] の doc と同様に事前の `canonicalize` が必要
//!   （[`OsDefaultStore`] は既存の祖先を実体パスへ解決してから返す）
//!
//! ## スタブについて（`code-comment-style.md`・REPAIR-3）
//!
//! 具象型は OS 既定パス解決の [`OsDefaultStore`] のみ実装済みである。
//!
//! - OS 既定パスの解決は [`OsDefaultStore`]（TASK-60（60.3）・#202）。
//!   `directories` は #200 の判断（推移的依存 `option-ext` が MPL-2.0）で不採用と
//!   し、std の環境変数だけで自前解決する
//! - 明示指定による上書き（`RootSource::Explicit`。`PROF-1`）: #203（TASK-60（60.4））
//! - 削除（`PROF-5`）: #188（TASK-53（53.2））。[`ProfileStore::delete`] の既定実装は
//!   成功を装わず [`ProfileError::Unsupported`] を返す
//! - Windows 長パス（`XOS-8`、TASK-61）・名前正規化（`XOS-9`、TASK-62）は
//!   将来この層へ差し込む。Windows の ACL 隔離が未実装のため、現状 Windows の
//!   [`Profile::open`] は `Unsupported` を返し、[`ProfileStore::open_or_create`] も
//!   それをそのまま伝える

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::profile::{Profile, ProfileError};

/// プロファイルルートの解決元。将来の解決経路追加に備え `non_exhaustive`（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RootSource {
    /// OS 慣習に基づく既定保存先（`XOS-7`。[`OsDefaultStore`]）。
    OsDefault,
    /// 呼び出し元による明示指定（`PROF-1`。#203 で実装）。
    ///
    /// 相対パスの絶対化は上書き境界（#203）の責務で、ここへ渡る値は絶対パスとする。
    Explicit,
}

/// [`ProfileStore::resolve_root`] の結果。解決したパスとその解決元を持つ。
///
/// 生の `PathBuf` ではなく構造体にすることで、解決元などの情報を後から
/// 拡張できるようにする（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoot {
    path: PathBuf,
    source: RootSource,
}

impl ResolvedRoot {
    /// 解決結果を作る。`ProfileStore` 実装者（#202・#203）が使う。
    pub fn new(path: PathBuf, source: RootSource) -> Self {
        Self { path, source }
    }

    /// 解決したプロファイルルートのパス。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 解決元。
    pub fn source(&self) -> RootSource {
        self.source
    }

    /// パスの所有権を取り出す。
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

/// プロファイル保存先のパス解決・作成・削除を担うトレイト（`XOS-7`、TASK-60（60.2））。
///
/// dyn 互換（`Box<dyn ProfileStore>` を cli が保持できる）。
pub trait ProfileStore {
    /// プロファイルルートを解決する。絶対パスを返すこと。
    ///
    /// 既定実装は置かない（偽装や #202 との重複を避ける）。
    fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError>;

    /// 解決したルートでプロファイルを開く。存在しなければ作成する。
    ///
    /// 解決結果が相対パスなら何も作成せず [`ProfileError::InvalidLayout`] を返す
    /// （カレントディレクトリ次第で保存先が変わるのを防ぐ）。境界検証は
    /// [`Profile::open`] に委譲する。
    fn open_or_create(&self) -> Result<Profile, ProfileError> {
        let resolved = self.resolve_root()?;
        if !resolved.path().is_absolute() {
            return Err(ProfileError::InvalidLayout {
                path: resolved.into_path(),
                reason: "profile root must be an absolute path",
            });
        }
        Profile::open(resolved.path())
    }

    /// プロファイルを削除する。
    ///
    /// 既定実装は fail-closed で [`ProfileError::Unsupported`] を返し、成功を装わない
    /// （`PROF-5`・REPAIR-3）。#188（TASK-53（53.2））が置き換える。`Profile` を借用で
    /// 受け取るのは、ロック保持者だけが削除できるようにするためと、削除失敗時に
    /// 呼び出し側が `Profile`（と `profile.lock`）を保持し続けられるようにするため
    /// （値渡しだと失敗時にロックが解放され、他プロセスが同一プロファイルを開ける。
    /// `PROF-1`）。成功時のロック解放の扱いは #188 が定める。
    fn delete(&self, profile: &Profile) -> Result<(), ProfileError> {
        let _ = profile;
        Err(ProfileError::Unsupported {
            reason: "profile deletion is not implemented yet (tracked by PROF-5 / TASK-53)",
        })
    }
}

/// 既定プロファイルルートのアプリディレクトリ名（`XOS-7`）。
///
/// OS 慣習のデータ置き場（XDG データ・Application Support・LocalAppData）直下に
/// この名前のディレクトリを作り、そのディレクトリ自体をプロファイルルートとする。
pub const APP_DIR_NAME: &str = "fandhe-browser";

/// OS 慣習に従って既定のプロファイルルートを解決する [`ProfileStore`]
/// （`XOS-7`、TASK-60（60.3）・#202、MS-3）。
///
/// `fandhe-browser-cli`（TASK-41 で追加予定）が、明示指定がないときの保存先として
/// 使う。解決規則（いずれも `<app>` は [`APP_DIR_NAME`]）:
///
/// - Linux 等（macOS 以外の Unix）: `$XDG_DATA_HOME/<app>`。未設定・空・相対パス・
///   `..` 含みのときは XDG Base Directory 仕様どおり無視し `$HOME/.local/share/<app>`
/// - macOS: `$HOME/Library/Application Support/<app>`
/// - Windows: `%LOCALAPPDATA%\<app>`
/// - 上記以外: 既定値を持たないため [`ProfileError::DefaultRootUnavailable`]
///
/// 必要な環境変数が使えなければ暗黙の代替先へ落とさず
/// [`ProfileError::DefaultRootUnavailable`] を返す（fail-closed）。プロセスの環境変数は
/// 書き換えない。明示指定による上書き（`RootSource::Explicit`）は #203（TASK-60（60.4））
/// が担う。
#[derive(Debug, Clone, Default)]
pub struct OsDefaultStore {
    _private: (),
}

impl OsDefaultStore {
    /// 実行中の OS の慣習で既定パスを解決するストアを作る。
    pub fn new() -> Self {
        Self::default()
    }
}

impl ProfileStore for OsDefaultStore {
    fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
        let path = platform_default_root(&|key| std::env::var_os(key))?;
        Ok(ResolvedRoot::new(
            canonicalize_base(&path),
            RootSource::OsDefault,
        ))
    }
}

/// OS 標準のトップレベル symlink エイリアス（別名 → 実体）。macOS の `/var`・`/tmp`・
/// `/etc` は OS が `/private/...` への symlink として提供する。他 OS は空（何も解決しない）。
#[cfg(target_os = "macos")]
const OS_STANDARD_ALIASES: &[(&str, &str)] = &[
    ("/var", "/private/var"),
    ("/tmp", "/private/tmp"),
    ("/etc", "/private/etc"),
];
#[cfg(not(target_os = "macos"))]
const OS_STANDARD_ALIASES: &[(&str, &str)] = &[];

/// 基点先頭の OS 標準エイリアスだけを実体パスへ置き換える（`XOS-7`・`PROF-1`・`PROF-4`）。
fn canonicalize_base(path: &Path) -> PathBuf {
    let aliases: Vec<(&Path, &Path)> = OS_STANDARD_ALIASES
        .iter()
        .map(|(alias, real)| (Path::new(*alias), Path::new(*real)))
        .collect();
    canonicalize_base_with(path, &aliases)
}

/// `aliases`（別名, 実体）のうち、パス先頭が別名に一致し、かつ別名を実際に
/// `canonicalize` した結果が宣言された実体と一致するものだけ置き換える。
///
/// [`Profile::open`] は祖先の symlink を拒否する（`PROF-1`・`PROF-4`）。任意の祖先
/// symlink を解決すると、環境変数で指した先へ拒否を迂回して書き込めてしまうため
/// （プロファイル境界の迂回）、OS 標準エイリアス以外の symlink は解決せず
/// [`Profile::open`] の拒否に委ねる（境界を検証できない symlink は fail-closed）。
/// 別名が想定どおりの実体を指さない場合も置き換えない。
fn canonicalize_base_with(path: &Path, aliases: &[(&Path, &Path)]) -> PathBuf {
    for (alias, real) in aliases {
        let Ok(rest) = path.strip_prefix(alias) else {
            continue;
        };
        if alias.canonicalize().is_ok_and(|resolved| resolved == *real) {
            return real.join(rest);
        }
    }
    path.to_path_buf()
}

/// 環境変数の値をベースディレクトリとして採用できるか検証する。
///
/// 空・相対パス・`..` を含むものは採用しない（カレントディレクトリ次第で保存先が
/// 変わる・境界外へ抜ける経路を作らないため。security.md「プロファイル境界」）。
fn usable_base(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return None;
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    Some(path)
}

/// XDG 系（macOS 以外の Unix）の既定ルート。`env` は環境変数の参照関数（テスト注入用）。
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn resolve_xdg<F: Fn(&str) -> Option<OsString>>(env: &F) -> Result<PathBuf, ProfileError> {
    if let Some(data_home) = usable_base(env("XDG_DATA_HOME")) {
        return Ok(data_home.join(APP_DIR_NAME));
    }
    match usable_base(env("HOME")) {
        Some(home) => Ok(home.join(".local").join("share").join(APP_DIR_NAME)),
        None => Err(ProfileError::DefaultRootUnavailable {
            reason: "neither XDG_DATA_HOME nor HOME is set to an absolute path",
        }),
    }
}

/// macOS の既定ルート。XDG 系の変数は参照しない。
#[cfg(any(target_os = "macos", test))]
fn resolve_macos<F: Fn(&str) -> Option<OsString>>(env: &F) -> Result<PathBuf, ProfileError> {
    match usable_base(env("HOME")) {
        Some(home) => Ok(home
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME)),
        None => Err(ProfileError::DefaultRootUnavailable {
            reason: "HOME is not set to an absolute path",
        }),
    }
}

/// Windows の既定ルート。`USERPROFILE` 等へはフォールバックしない。
#[cfg(any(windows, test))]
fn resolve_windows<F: Fn(&str) -> Option<OsString>>(env: &F) -> Result<PathBuf, ProfileError> {
    match usable_base(env("LOCALAPPDATA")) {
        Some(local) => Ok(local.join(APP_DIR_NAME)),
        None => Err(ProfileError::DefaultRootUnavailable {
            reason: "LOCALAPPDATA is not set to an absolute path",
        }),
    }
}

/// 実行中の OS に対応する解決規則へ振り分ける。
#[cfg(target_os = "macos")]
fn platform_default_root<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<PathBuf, ProfileError> {
    resolve_macos(env)
}

/// 実行中の OS に対応する解決規則へ振り分ける。
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_default_root<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<PathBuf, ProfileError> {
    resolve_xdg(env)
}

/// 実行中の OS に対応する解決規則へ振り分ける。
#[cfg(windows)]
fn platform_default_root<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<PathBuf, ProfileError> {
    resolve_windows(env)
}

/// 実行中の OS に対応する解決規則へ振り分ける（既定値の定義がない OS）。
#[cfg(not(any(unix, windows)))]
fn platform_default_root<F: Fn(&str) -> Option<OsString>>(
    _env: &F,
) -> Result<PathBuf, ProfileError> {
    Err(ProfileError::DefaultRootUnavailable {
        reason: "no OS default profile location is defined for this platform",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedStore {
        resolved: ResolvedRoot,
    }

    impl ProfileStore for FixedStore {
        fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
            Ok(self.resolved.clone())
        }
    }

    #[cfg(unix)]
    struct TempDir(PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir().canonicalize().unwrap();
            let path = base.join(format!(
                "fandhe-profile-store-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            TempDir(path)
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// XOS-7: dyn 互換で解決結果が具体値で返る。
    #[test]
    fn xos_7_profile_store_is_dyn_compatible() {
        let root = std::env::temp_dir().join("fandhe-store-dyn");
        let store: Box<dyn ProfileStore> = Box::new(FixedStore {
            resolved: ResolvedRoot::new(root.clone(), RootSource::OsDefault),
        });
        let resolved = store.resolve_root().unwrap();
        assert_eq!(resolved.path(), root.as_path());
        assert_eq!(resolved.source(), RootSource::OsDefault);
    }

    /// XOS-7: `ResolvedRoot` のアクセサが入力と一致する。
    #[test]
    fn xos_7_resolved_root_accessors() {
        let path = PathBuf::from("some").join("path");
        let r = ResolvedRoot::new(path.clone(), RootSource::Explicit);
        assert_eq!(r.path(), path.as_path());
        assert_eq!(r.source(), RootSource::Explicit);
        assert_eq!(r.into_path(), path);
    }

    /// XOS-7: 相対パスは拒否され、何も作成されない。
    #[test]
    fn xos_7_open_or_create_rejects_relative_root() {
        let rel = PathBuf::from("relative").join("fandhe-store-profile");
        let store = FixedStore {
            resolved: ResolvedRoot::new(rel.clone(), RootSource::Explicit),
        };
        match store.open_or_create() {
            Err(ProfileError::InvalidLayout { path, reason }) => {
                assert_eq!(path, rel);
                assert_eq!(reason, "profile root must be an absolute path");
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
        assert!(!rel.exists());
    }

    /// XOS-7: 絶対パスなら `Profile::open` に委譲される。
    #[cfg(unix)]
    #[test]
    fn xos_7_open_or_create_delegates_to_profile_open() {
        let tmp = TempDir::new();
        let store = FixedStore {
            resolved: ResolvedRoot::new(tmp.0.clone(), RootSource::OsDefault),
        };
        let profile = store.open_or_create().unwrap();
        assert_eq!(profile.root(), tmp.0.as_path());
    }

    /// XOS-7: Windows では既存の fail-closed（`Unsupported`）を伝え、作成しない。
    #[cfg(not(unix))]
    #[test]
    fn xos_7_open_or_create_is_unsupported_on_windows() {
        let root = std::env::temp_dir().join("fandhe-store-win-unsupported");
        let store = FixedStore {
            resolved: ResolvedRoot::new(root.clone(), RootSource::OsDefault),
        };
        assert!(matches!(
            store.open_or_create(),
            Err(ProfileError::Unsupported { .. })
        ));
        assert!(!root.exists());
    }

    /// PROF-5: 既定の `delete` は削除を装わず `Unsupported` を返しルートが残る。
    #[cfg(unix)]
    #[test]
    fn prof_5_delete_default_is_fail_closed() {
        let tmp = TempDir::new();
        let store = FixedStore {
            resolved: ResolvedRoot::new(tmp.0.clone(), RootSource::OsDefault),
        };
        let profile = store.open_or_create().unwrap();
        match store.delete(&profile) {
            Err(ProfileError::Unsupported { reason }) => {
                assert_eq!(
                    reason,
                    "profile deletion is not implemented yet (tracked by PROF-5 / TASK-53)"
                );
            }
            other => panic!("expected Unsupported, got {:?}", other),
        }
        assert!(tmp.0.is_dir());
        // 失敗後もロックは保持されたまま（PROF-1）: 別ハンドルの open は Locked になる。
        assert!(matches!(
            Profile::open(&tmp.0),
            Err(ProfileError::Locked { .. })
        ));
        drop(profile);
    }

    // ---- OsDefaultStore（XOS-7・TASK-60（60.3）） ----

    use std::collections::HashMap;

    fn env_of(pairs: &[(&'static str, OsString)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<&'static str, OsString> = pairs.iter().cloned().collect();
        move |key| map.get(key).cloned()
    }

    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn os(p: &Path) -> OsString {
        p.as_os_str().to_os_string()
    }

    fn unavailable(r: Result<PathBuf, ProfileError>) -> &'static str {
        match r {
            Err(ProfileError::DefaultRootUnavailable { reason }) => reason,
            other => panic!("expected DefaultRootUnavailable, got {other:?}"),
        }
    }

    /// XOS-7: XDG_DATA_HOME が絶対パスならその直下。
    #[test]
    fn xos_7_xdg_uses_xdg_data_home() {
        let xdg = abs("xdg-data");
        let env = env_of(&[("XDG_DATA_HOME", os(&xdg)), ("HOME", os(&abs("home")))]);
        assert_eq!(resolve_xdg(&env).unwrap(), xdg.join("fandhe-browser"));
    }

    /// XOS-7: XDG_DATA_HOME が空・相対・`..` 含みなら HOME の `.local/share` へ。
    #[test]
    fn xos_7_xdg_falls_back_to_home() {
        let home = abs("home");
        let expected = home.join(".local").join("share").join("fandhe-browser");
        let bad_values = [
            OsString::new(),
            OsString::from("relative-data"),
            os(&abs("a").join("..").join("b")),
        ];
        for bad in bad_values {
            let env = env_of(&[("XDG_DATA_HOME", bad), ("HOME", os(&home))]);
            assert_eq!(resolve_xdg(&env).unwrap(), expected);
        }
        let env = env_of(&[("HOME", os(&home))]);
        assert_eq!(resolve_xdg(&env).unwrap(), expected);
    }

    /// XOS-7: XDG_DATA_HOME も HOME も使えなければ fail-closed。
    #[test]
    fn xos_7_xdg_errors_without_usable_env() {
        let env = env_of(&[("HOME", OsString::from("relative-home"))]);
        assert_eq!(
            unavailable(resolve_xdg(&env)),
            "neither XDG_DATA_HOME nor HOME is set to an absolute path"
        );
        assert_eq!(
            unavailable(resolve_xdg(&env_of(&[]))),
            "neither XDG_DATA_HOME nor HOME is set to an absolute path"
        );
    }

    /// XOS-7: macOS は Application Support。XDG_DATA_HOME は無視する。
    #[test]
    fn xos_7_macos_uses_application_support() {
        let home = abs("mac-home");
        let env = env_of(&[("HOME", os(&home)), ("XDG_DATA_HOME", os(&abs("ignored")))]);
        assert_eq!(
            resolve_macos(&env).unwrap(),
            home.join("Library")
                .join("Application Support")
                .join("fandhe-browser")
        );
    }

    /// XOS-7: macOS で HOME が無い・相対ならエラー。
    #[test]
    fn xos_7_macos_errors_without_home() {
        for env in [
            env_of(&[]),
            env_of(&[("HOME", OsString::from("relative-home"))]),
        ] {
            assert_eq!(
                unavailable(resolve_macos(&env)),
                "HOME is not set to an absolute path"
            );
        }
    }

    /// XOS-7: Windows は LOCALAPPDATA 直下。
    #[test]
    fn xos_7_windows_uses_local_app_data() {
        let local = abs("local-app-data");
        let env = env_of(&[("LOCALAPPDATA", os(&local))]);
        assert_eq!(resolve_windows(&env).unwrap(), local.join("fandhe-browser"));
    }

    /// XOS-7: Windows で LOCALAPPDATA が未設定・空・相対ならエラー（HOME 等へ落とさない）。
    #[test]
    fn xos_7_windows_errors_without_local_app_data() {
        for env in [
            env_of(&[("HOME", os(&abs("home")))]),
            env_of(&[("LOCALAPPDATA", OsString::new())]),
            env_of(&[("LOCALAPPDATA", OsString::from("relative"))]),
        ] {
            assert_eq!(
                unavailable(resolve_windows(&env)),
                "LOCALAPPDATA is not set to an absolute path"
            );
        }
    }

    /// XOS-7: Linux 等でディスパッチャが XDG 規則を選ぶ。
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xos_7_dispatcher_selects_xdg() {
        let xdg = abs("dispatch-xdg");
        let env = env_of(&[("XDG_DATA_HOME", os(&xdg))]);
        assert_eq!(
            platform_default_root(&env).unwrap(),
            xdg.join("fandhe-browser")
        );
    }

    /// XOS-7: macOS でディスパッチャが Application Support 規則を選ぶ。
    #[cfg(target_os = "macos")]
    #[test]
    fn xos_7_dispatcher_selects_application_support() {
        let home = abs("dispatch-home");
        let env = env_of(&[("HOME", os(&home))]);
        assert_eq!(
            platform_default_root(&env).unwrap(),
            home.join("Library")
                .join("Application Support")
                .join("fandhe-browser")
        );
    }

    /// XOS-7: Windows でディスパッチャが LOCALAPPDATA 規則を選ぶ。
    #[cfg(windows)]
    #[test]
    fn xos_7_dispatcher_selects_local_app_data() {
        let local = abs("dispatch-local");
        let env = env_of(&[("LOCALAPPDATA", os(&local))]);
        assert_eq!(
            platform_default_root(&env).unwrap(),
            local.join("fandhe-browser")
        );
    }

    /// XOS-7: 実環境での解決結果の構造（ディレクトリは作成しない）。
    #[test]
    fn xos_7_os_default_store_resolves_real_env_shape() {
        // HOME / LOCALAPPDATA 等が未設定の有効な環境では契約どおり
        // DefaultRootUnavailable になるため、その場合は形状検査を行わない。
        let resolved = match OsDefaultStore::new().resolve_root() {
            Ok(resolved) => resolved,
            Err(ProfileError::DefaultRootUnavailable { .. }) => return,
            Err(other) => panic!("unexpected error: {other:?}"),
        };
        assert_eq!(resolved.source(), RootSource::OsDefault);
        assert!(resolved.path().is_absolute());
        assert_eq!(
            resolved.path().file_name(),
            Some(std::ffi::OsStr::new("fandhe-browser"))
        );
        #[cfg(target_os = "macos")]
        assert!(
            resolved
                .path()
                .parent()
                .unwrap()
                .ends_with("Library/Application Support")
        );
    }

    /// XOS-7・PROF-1: OS 標準エイリアスだけ実体へ解決し、任意の祖先 symlink は解決しない。
    #[cfg(unix)]
    #[test]
    fn xos_7_canonicalize_base_resolves_only_declared_aliases() {
        let tmp = TempDir::new();
        let real = tmp.0.join("real");
        std::fs::create_dir_all(real.join("data")).unwrap();
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let other = tmp.0.join("other");
        std::os::unix::fs::symlink(&real, &other).unwrap();

        let aliases: Vec<(&Path, &Path)> = vec![(link.as_path(), real.as_path())];
        // 宣言済みエイリアスは実体へ置き換わり、`Profile::open` が開ける。
        let root = canonicalize_base_with(&link.join("data").join("fandhe-browser"), &aliases);
        assert_eq!(root, real.join("data").join("fandhe-browser"));
        assert!(Profile::open(&root).is_ok());
        // 宣言外の祖先 symlink はそのまま返り、`Profile::open` が拒否する。
        let via_other = other.join("data").join("fandhe-browser");
        let unresolved = canonicalize_base_with(&via_other, &aliases);
        assert_eq!(unresolved, via_other);
        assert!(matches!(
            Profile::open(&unresolved),
            Err(ProfileError::InvalidLayout { .. })
        ));
        // 宣言と実体が食い違うエイリアスは置き換えない。
        let wrong = tmp.0.join("wrong");
        let bad: Vec<(&Path, &Path)> = vec![(link.as_path(), wrong.as_path())];
        let p = link.join("data").join("fandhe-browser");
        assert_eq!(canonicalize_base_with(&p, &bad), p);
    }

    /// PROF-1: 末尾のアプリディレクトリが symlink なら解決せず、`Profile::open` が拒否する。
    #[cfg(unix)]
    #[test]
    fn prof_1_canonicalize_base_keeps_symlinked_leaf_rejected() {
        let tmp = TempDir::new();
        let target = tmp.0.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let leaf = tmp.0.join("fandhe-browser");
        std::os::unix::fs::symlink(&target, &leaf).unwrap();

        let root = canonicalize_base(&leaf);
        assert_eq!(root, leaf);
        assert!(matches!(
            Profile::open(&root),
            Err(ProfileError::InvalidLayout { .. })
        ));
    }
}
