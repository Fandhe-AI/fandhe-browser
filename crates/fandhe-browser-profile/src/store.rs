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
//!   パス（Linux の `/home` 等も含む）は、[`Profile::open`] の doc と同様に事前の
//!   `canonicalize` が必要（[`OsDefaultStore`] は OS 標準の基点ディレクトリ
//!   （`$HOME` 等）だけを実体パスへ解決し、アプリ用の固定サフィックスは解決しない）
//!
//! ## スタブについて（`code-comment-style.md`・REPAIR-3）
//!
//! 具象型は OS 既定パス解決の [`OsDefaultStore`]・明示指定の [`ExplicitStore`]・
//! 両者を合成する [`OverridableStore`] を実装済みである。
//!
//! - OS 既定パスの解決は [`OsDefaultStore`]（TASK-60（60.3）・#202）。
//!   `directories` は #200 の判断（推移的依存 `option-ext` が MPL-2.0）で不採用と
//!   し、std の環境変数だけで自前解決する
//! - 明示指定による上書き（`RootSource::Explicit`。`PROF-1`）は [`ExplicitStore`]・
//!   [`OverridableStore`]（TASK-60（60.4）・#203）。明示指定が既定解決より優先される
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
    /// 呼び出し元による明示指定（`PROF-1`。[`ExplicitStore`] が返す）。
    ///
    /// 相対パスの絶対化は [`ExplicitStore::new`] が行うため、ここへ渡る値は絶対パスとする。
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
    /// 解決結果を作る。[`OsDefaultStore`]・[`ExplicitStore`] などの `ProfileStore` 実装者が使う。
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
/// 書き換えない。明示指定による上書き（`RootSource::Explicit`）は [`OverridableStore`]
/// （TASK-60（60.4）・#203）が担う。
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
        let (base, tail) = platform_default_parts(&|key| std::env::var_os(key))?;
        Ok(ResolvedRoot::new(
            canonicalize_base(&base).join(tail),
            RootSource::OsDefault,
        ))
    }
}

/// 明示指定されたプロファイルルートを返す [`ProfileStore`]
/// （`PROF-1`・`XOS-7`、TASK-60（60.4）・#203、MS-3）。
///
/// `fandhe-browser-cli`（TASK-41 で追加予定）が、利用者の「任意のパスをルートに
/// 指定する」オプションを受け取ったときに作る。通常は [`OverridableStore`] 経由で
/// 既定解決より優先させる。
///
/// - 相対パスは [`ExplicitStore::new`] の時点で 1 回だけ絶対化して固定する
///   （実行中の `chdir` で保存先が変わらないようにするため）
/// - `canonicalize` はしない。symlink の検証は [`Profile::open`] がハンドル基準で
///   行う（`PROF-1`・`PROF-4`）。macOS の `/var` など OS 標準の symlink を含む
///   パスは、[`Profile::open`] の doc と同様に呼び出し側で `canonicalize` が必要
/// - `..` を含むパスは受け付ける。`PROF-1` が「任意のパス」を求めるうえ、
///   [`Profile::open`] は `..` をハンドル基準でたどるため境界外への書き込み経路に
///   ならない（環境変数由来の暗黙の値を拒否する `usable_base` とは入力の信頼度が異なる）
/// - `..` を畳み込んだ結果が OS のルート（`/`・ドライブルート）になる指定は、
///   `fchmod(0700)` やファイル作成が OS ルートへ及ぶため [`ExplicitStore::new`] が
///   [`ProfileError::InvalidLayout`] で拒否する
///
/// [`ProfileStore::delete`] は既定実装（`Unsupported`）のままで、#188 が担う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitStore {
    root: PathBuf,
}

impl ExplicitStore {
    /// 明示指定パスからストアを作る。相対パスは現在のカレントディレクトリで絶対化する。
    ///
    /// 空パスは [`ProfileError::InvalidLayout`]、カレントディレクトリが取得できない
    /// などの OS エラーは [`ProfileError::Io`] を返す（fail-closed）。
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, ProfileError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(ProfileError::InvalidLayout {
                path,
                reason: "explicit profile root must not be empty",
            });
        }
        let root = std::path::absolute(&path)?;
        if !root.is_absolute() {
            return Err(ProfileError::InvalidLayout {
                path: root,
                reason: "explicit profile root must be an absolute path",
            });
        }
        if is_filesystem_root(&root) {
            return Err(ProfileError::InvalidLayout {
                path: root,
                reason: "explicit profile root must not be the filesystem root",
            });
        }
        Ok(Self { root })
    }

    /// 絶対化済みの明示指定ルート。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl ProfileStore for ExplicitStore {
    fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
        Ok(ResolvedRoot::new(self.root.clone(), RootSource::Explicit))
    }
}

/// `..` を字句的に畳み込んだ結果が OS のルート（`/`・`C:\\` などのドライブルート・
/// UNC 共有ルート）を指すかを返す（`PROF-1`）。
///
/// [`ExplicitStore::new`] が、[`Profile::open`] による `fchmod(0700)` やファイル作成が
/// OS ルートへ及ぶのを副作用の前に防ぐために使う。`std::path::absolute` は `..` を
/// 畳み込まないため、`/a/..` のような指定もここで検出する。
fn is_filesystem_root(path: &Path) -> bool {
    let mut normal_depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => normal_depth += 1,
            // ルート直上の `..` はルートに留まる（OS の解決規則と同じ）。
            Component::ParentDir => normal_depth = normal_depth.saturating_sub(1),
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }
    normal_depth == 0
}

/// 明示指定 > 既定（既定は型パラメータ `D`。通常 [`OsDefaultStore`]）の優先順位で
/// ルートを解決する [`ProfileStore`]（`PROF-1`・`XOS-7`、TASK-60（60.4）・#203）。
///
/// `fandhe-browser-cli`（TASK-41 で追加予定）が `Box<dyn ProfileStore>` として保持する
/// 想定（`'static` なら dyn 化できる。enum 分岐にはしない）。明示指定があるときは
/// 既定側の `resolve_root` を呼ばないため、`HOME` 等が使えない環境
/// （`DefaultRootUnavailable`）でも明示指定だけで動く。明示指定がなく既定も解決
/// できなければ、暗黙の代替先へ落とさずそのエラーを返す。
/// [`ProfileStore::delete`] は既定実装のまま（#188）。
#[derive(Debug, Clone)]
pub struct OverridableStore<D = OsDefaultStore> {
    explicit: Option<ExplicitStore>,
    default: D,
}

impl OverridableStore<OsDefaultStore> {
    /// 明示指定がなければ OS 既定（[`OsDefaultStore`]）を使うストアを作る。
    pub fn new(explicit: Option<ExplicitStore>) -> Self {
        Self::with_default(explicit, OsDefaultStore::new())
    }
}

impl<D: ProfileStore> OverridableStore<D> {
    /// 既定側のストアを差し替えて作る（テストや将来の既定解決の差し替え用）。
    pub fn with_default(explicit: Option<ExplicitStore>, default: D) -> Self {
        Self { explicit, default }
    }
}

impl<D: ProfileStore> ProfileStore for OverridableStore<D> {
    fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
        match &self.explicit {
            Some(explicit) => explicit.resolve_root(),
            None => self.default.resolve_root(),
        }
    }
}

/// OS 標準の基点ディレクトリ（`$HOME`・`$XDG_DATA_HOME`・`%LOCALAPPDATA%`）を
/// 実体パスへ解決する（`XOS-7`・`PROF-1`・`PROF-4`）。
///
/// [`Profile::open`] は祖先の symlink を拒否する（`PROF-1`・`PROF-4`）が、macOS の
/// `/var` や Linux の `/home`、`$HOME` 自体が symlink の環境では、OS 標準の経路でも
/// 祖先に symlink が含まれる。そこで信頼できる基点（環境変数で利用者自身が指定する
/// ディレクトリ）だけを `canonicalize` する。基点に付けるアプリ用サフィックス
/// （`.local/share/<app>` 等）は呼び出し側が解決後に連結するため解決されず、
/// サフィックス内の symlink は従来どおり [`Profile::open`] が拒否する
/// （プロファイル境界は弱めない）。基点が存在しない・解決できない場合と Unix 以外
/// （Windows の `canonicalize` は verbatim パスを返し、かつ [`Profile::open`] が
/// 未対応）では、そのまま返して [`Profile::open`] の検証に委ねる。
fn canonicalize_base(base: &Path) -> PathBuf {
    if cfg!(unix)
        && let Ok(resolved) = base.canonicalize()
    {
        return resolved;
    }
    base.to_path_buf()
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
fn resolve_xdg<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    if let Some(data_home) = usable_base(env("XDG_DATA_HOME")) {
        return Ok((data_home, PathBuf::from(APP_DIR_NAME)));
    }
    match usable_base(env("HOME")) {
        Some(home) => Ok((
            home,
            PathBuf::from(".local").join("share").join(APP_DIR_NAME),
        )),
        None => Err(ProfileError::DefaultRootUnavailable {
            reason: "neither XDG_DATA_HOME nor HOME is set to an absolute path",
        }),
    }
}

/// macOS の既定ルート。XDG 系の変数は参照しない。
#[cfg(any(target_os = "macos", test))]
fn resolve_macos<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    match usable_base(env("HOME")) {
        Some(home) => Ok((
            home,
            PathBuf::from("Library")
                .join("Application Support")
                .join(APP_DIR_NAME),
        )),
        None => Err(ProfileError::DefaultRootUnavailable {
            reason: "HOME is not set to an absolute path",
        }),
    }
}

/// Windows の既定ルート。`USERPROFILE` 等へはフォールバックしない。
#[cfg(any(windows, test))]
fn resolve_windows<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    match usable_base(env("LOCALAPPDATA")) {
        Some(local) => Ok((local, PathBuf::from(APP_DIR_NAME))),
        None => Err(ProfileError::DefaultRootUnavailable {
            reason: "LOCALAPPDATA is not set to an absolute path",
        }),
    }
}

/// 実行中の OS に対応する解決規則へ振り分け、（基点, アプリ用サフィックス）を返す。
#[cfg(target_os = "macos")]
fn platform_default_parts<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    resolve_macos(env)
}

/// 実行中の OS に対応する解決規則へ振り分け、（基点, アプリ用サフィックス）を返す。
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_default_parts<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    resolve_xdg(env)
}

/// 実行中の OS に対応する解決規則へ振り分け、（基点, アプリ用サフィックス）を返す。
#[cfg(windows)]
fn platform_default_parts<F: Fn(&str) -> Option<OsString>>(
    env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    resolve_windows(env)
}

/// 実行中の OS に対応する解決規則へ振り分ける（既定値の定義がない OS）。
#[cfg(not(any(unix, windows)))]
fn platform_default_parts<F: Fn(&str) -> Option<OsString>>(
    _env: &F,
) -> Result<(PathBuf, PathBuf), ProfileError> {
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

    fn joined(r: Result<(PathBuf, PathBuf), ProfileError>) -> PathBuf {
        let (base, tail) = r.unwrap();
        base.join(tail)
    }

    fn unavailable(r: Result<(PathBuf, PathBuf), ProfileError>) -> &'static str {
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
        assert_eq!(joined(resolve_xdg(&env)), xdg.join("fandhe-browser"));
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
            assert_eq!(joined(resolve_xdg(&env)), expected);
        }
        let env = env_of(&[("HOME", os(&home))]);
        assert_eq!(joined(resolve_xdg(&env)), expected);
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
            joined(resolve_macos(&env)),
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
        assert_eq!(joined(resolve_windows(&env)), local.join("fandhe-browser"));
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
            joined(platform_default_parts(&env)),
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
            joined(platform_default_parts(&env)),
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
            joined(platform_default_parts(&env)),
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

    /// XOS-7・PROF-1: 基点が symlink 経由（Linux の `/home` 等）でも実体へ解決され開ける。
    /// 基点が存在しなければそのまま返す。
    #[cfg(unix)]
    #[test]
    fn xos_7_canonicalize_base_resolves_symlinked_base() {
        let tmp = TempDir::new();
        let real = tmp.0.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let base = canonicalize_base(&link);
        assert_eq!(base, real);
        let root = base.join(".local").join("share").join("fandhe-browser");
        assert!(Profile::open(&root).is_ok());

        let missing = tmp.0.join("missing");
        assert_eq!(canonicalize_base(&missing), missing);
    }

    /// PROF-1: 基点配下のアプリ用サフィックスが symlink なら解決されず、
    /// `Profile::open` が拒否する（基点だけを解決しサフィックスは検証に委ねる）。
    #[cfg(unix)]
    #[test]
    fn prof_1_symlinked_suffix_stays_rejected() {
        let tmp = TempDir::new();
        let base = tmp.0.join("home");
        let target = tmp.0.join("elsewhere");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let leaf = base.join("fandhe-browser");
        std::os::unix::fs::symlink(&target, &leaf).unwrap();

        let root = canonicalize_base(&base).join("fandhe-browser");
        assert_eq!(root, leaf);
        assert!(matches!(
            Profile::open(&root),
            Err(ProfileError::InvalidLayout { .. })
        ));
    }

    /// XOS-7: `$XDG_DATA_HOME` 自体が symlink でも `OsDefaultStore` 相当の解決で
    /// プロファイルを開ける（Linux で `$HOME` が symlink の環境の再現）。
    #[cfg(unix)]
    #[test]
    fn xos_7_symlinked_data_home_opens_profile() {
        let tmp = TempDir::new();
        let real = tmp.0.join("real-data");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.0.join("link-data");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let env = env_of(&[("XDG_DATA_HOME", os(&link))]);
        let (base, tail) = resolve_xdg(&env).unwrap();
        let root = canonicalize_base(&base).join(tail);
        assert_eq!(root, real.join("fandhe-browser"));
        assert!(Profile::open(&root).is_ok());
    }

    /// 常に失敗する既定ストア（明示指定時に参照されないことの確認用）。
    struct FailingDefault;

    impl ProfileStore for FailingDefault {
        fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
            Err(ProfileError::DefaultRootUnavailable {
                reason: "test default must not be consulted",
            })
        }
    }

    /// 呼び出し回数を数える既定ストア。
    struct CountingDefault {
        calls: std::cell::Cell<usize>,
        root: PathBuf,
    }

    impl CountingDefault {
        fn new(root: PathBuf) -> Self {
            Self {
                calls: std::cell::Cell::new(0),
                root,
            }
        }
    }

    impl ProfileStore for CountingDefault {
        fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
            self.calls.set(self.calls.get() + 1);
            Ok(ResolvedRoot::new(self.root.clone(), RootSource::OsDefault))
        }
    }

    /// PROF-1: 明示指定は既定解決より優先され、既定側は呼ばれない。
    #[test]
    fn prof_1_explicit_root_overrides_default() {
        let explicit_root = abs("explicit-root");
        let store = OverridableStore::with_default(
            Some(ExplicitStore::new(explicit_root.clone()).unwrap()),
            CountingDefault::new(abs("default-root")),
        );
        let resolved = store.resolve_root().unwrap();
        assert_eq!(resolved.path(), explicit_root.as_path());
        assert_eq!(resolved.source(), RootSource::Explicit);
        assert_eq!(store.default.calls.get(), 0);

        // 既定が解決不能でも明示指定だけで成功する。
        let failing = OverridableStore::with_default(
            Some(ExplicitStore::new(explicit_root.clone()).unwrap()),
            FailingDefault,
        );
        assert_eq!(
            failing.resolve_root().unwrap().path(),
            explicit_root.as_path()
        );
    }

    /// XOS-7: 明示指定がなければ既定側の結果がそのまま返る。
    #[test]
    fn xos_7_falls_back_to_default_without_explicit() {
        let default_root = abs("default-root");
        let store =
            OverridableStore::with_default(None, CountingDefault::new(default_root.clone()));
        let resolved = store.resolve_root().unwrap();
        assert_eq!(resolved.path(), default_root.as_path());
        assert_eq!(resolved.source(), RootSource::OsDefault);
        assert_eq!(store.default.calls.get(), 1);

        // 既定も解決できなければ暗黙の代替先へ落とさずエラーを返す。
        let failing = OverridableStore::with_default(None, FailingDefault);
        assert!(matches!(
            failing.resolve_root(),
            Err(ProfileError::DefaultRootUnavailable { .. })
        ));
    }

    /// PROF-1: 相対パスは生成時に絶対化され、`resolve_root` が返す値は固定される。
    #[test]
    fn prof_1_explicit_relative_path_is_absolutized() {
        let store = ExplicitStore::new(PathBuf::from("rel").join("profile")).unwrap();
        let expected = std::env::current_dir().unwrap().join("rel").join("profile");
        assert_eq!(store.root(), expected.as_path());
        assert!(store.root().is_absolute());
        assert_eq!(
            store.resolve_root().unwrap(),
            ResolvedRoot::new(expected, RootSource::Explicit)
        );
    }

    /// PROF-1: 空パスは拒否される（fail-closed）。
    #[test]
    fn prof_1_explicit_empty_path_is_rejected() {
        match ExplicitStore::new("") {
            Err(ProfileError::InvalidLayout { reason, .. }) => {
                assert_eq!(reason, "explicit profile root must not be empty");
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
    }

    /// PROF-1: OS ルートは `..` 経由の指定も含めて副作用の前に拒否される。
    #[test]
    fn prof_1_explicit_filesystem_root_is_rejected() {
        #[cfg(unix)]
        let candidates = ["/", "//", "/.", "/..", "/a/..", "/a/b/../..", "/../.."];
        #[cfg(windows)]
        let candidates = ["C:\\", "C:/", "C:\\a\\..", "C:\\..", "C:/a/../.."];
        for candidate in candidates {
            match ExplicitStore::new(candidate) {
                Err(ProfileError::InvalidLayout { reason, .. }) => {
                    assert_eq!(
                        reason,
                        "explicit profile root must not be the filesystem root"
                    );
                }
                other => panic!(
                    "expected InvalidLayout for {candidate:?}, got {:?}",
                    other.map(|_| ())
                ),
            }
        }
    }

    /// PROF-1: ルート直下の通常ディレクトリや、`..` を含んでもルート外を指す指定は受け付ける。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_non_root_paths_are_accepted() {
        for candidate in ["/a", "/a/b/..", "/a/../b"] {
            let store = ExplicitStore::new(candidate).unwrap();
            assert_eq!(store.root(), Path::new(candidate));
        }
    }

    /// PROF-1: `OverridableStore` は dyn 互換で明示パスを返す。
    #[test]
    fn prof_1_overridable_store_is_dyn_compatible() {
        let root = abs("explicit-dyn");
        let store: Box<dyn ProfileStore> = Box::new(OverridableStore::new(Some(
            ExplicitStore::new(root.clone()).unwrap(),
        )));
        let resolved = store.resolve_root().unwrap();
        assert_eq!(resolved.path(), root.as_path());
        assert_eq!(resolved.source(), RootSource::Explicit);
    }

    /// PROF-1: `open_or_create` は明示ルートだけを作成し、既定側は作らない。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_or_create_uses_explicit_root_only() {
        let tmp = TempDir::new();
        let explicit = tmp.0.join("explicit");
        let default = tmp.0.join("default");
        let store = OverridableStore::with_default(
            Some(ExplicitStore::new(explicit.clone()).unwrap()),
            FixedStore {
                resolved: ResolvedRoot::new(default.clone(), RootSource::OsDefault),
            },
        );
        let profile = store.open_or_create().unwrap();
        assert_eq!(profile.root(), explicit.as_path());
        assert!(explicit.is_dir());
        assert!(!default.exists());
    }

    /// PROF-1: Windows では明示ストアでも既存の fail-closed（`Unsupported`）を伝える。
    #[cfg(not(unix))]
    #[test]
    fn prof_1_explicit_open_or_create_is_unsupported_on_windows() {
        let root = std::env::temp_dir().join("fandhe-store-win-explicit");
        let store = ExplicitStore::new(root.clone()).unwrap();
        assert!(matches!(
            store.open_or_create(),
            Err(ProfileError::Unsupported { .. })
        ));
        assert!(!root.exists());
    }
}
