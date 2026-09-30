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
//! - Windows 長パス（`XOS-8`）は [`to_long_path`] として実装済み（TASK-61（61.1）・#205）。
//!   [`ProfileStore::open_or_create`] が適用するが、[`Profile::open`] が実際に使うのは
//!   `XOS-7` の ACL 実装後になる
//! - 名前正規化（`XOS-9`、TASK-62）は将来この層へ差し込む。Windows の ACL 隔離が未実装のため、現状 Windows の
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
    /// Windows では [`to_long_path`] で `\\?\` 付きパスへ変換してから渡す（`XOS-8`）。
    fn open_or_create(&self) -> Result<Profile, ProfileError> {
        let resolved = self.resolve_root()?;
        if !resolved.path().is_absolute() {
            return Err(ProfileError::InvalidLayout {
                path: resolved.into_path(),
                reason: "profile root must be an absolute path",
            });
        }
        // Windows では 260 文字制約を避けるため verbatim パスへ変換してから渡す（`XOS-8`、
        // TASK-61（61.1））。現状は `Profile::open` が ACL 未実装（`XOS-7`）で先に
        // `Unsupported` を返すため観測できない。ACL 実装時は `Profile::open` 側が
        // verbatim ルートを受け取る前提で、`assert_within_root` の比較対象も verbatim 形へ
        // 揃える必要がある（REPAIR-3 の引き継ぎ事項）。
        #[cfg(windows)]
        let target = to_long_path(resolved.path())?.into_path();
        #[cfg(not(windows))]
        let target = resolved.path().to_path_buf();
        Profile::open(&target)
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
/// - `..` を含むパスは受け付けるが、[`ExplicitStore::new`] が副作用の前に字句的に
///   畳み込んで保存する（`/tmp/new-dir/../profile` は `/tmp/profile`）。畳み込まずに
///   [`Profile::open`] へ渡すと、途中要素（`/tmp/new-dir`）が最終ルートの外に作成される
///   ため（`PROF-1`・`PROF-4`）。畳み込まれる要素（`a/link/..` の `link`）が既存の
///   symlink の場合は、OS の解決先と字句的な結果が食い違うため [`ProfileError::InvalidLayout`]
///   で拒否する
/// - 既存ディレクトリが他者と共有される形（unix で group/other にアクセス権がある、または sticky 等が立っている。
///   `/tmp`・`0755`・`0770` 等）の場合は、[`Profile::open`] の `fchmod(0700)` が共有ディレクトリ自体へ
///   及ぶため [`ProfileError::InvalidLayout`] で拒否する。専用の子ディレクトリを指定する
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
        let root = normalize_lexically(&std::path::absolute(&path)?)?;
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
        reject_shared_existing_dir(&root)?;
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

    /// 開いたルートのハンドルで共有ディレクトリ（sticky・group/other 書込可）を
    /// `fchmod(0700)` の前に拒否して開く（`PROF-1`・TOCTOU 回避）。
    fn open_or_create(&self) -> Result<Profile, ProfileError> {
        // Windows では既定実装と同じく verbatim パスへ変換してから開く（`XOS-8`、TASK-61（61.1））。
        #[cfg(windows)]
        let target = to_long_path(&self.root)?.into_path();
        #[cfg(not(windows))]
        let target = self.root.clone();
        Profile::open_rejecting_shared_root(&target)
    }
}

/// `..` と `.` を字句的に畳み込んだパスを返す（`PROF-1`）。
///
/// [`ExplicitStore::new`] が、[`Profile::open`] に `..` 付きパスを渡すと最終ルート外の
/// 途中要素が作成される問題を、副作用の前に防ぐために使う。ルート直上の `..` は
/// ルートに留まる（OS の解決規則と同じ）。畳み込まれる要素が既存の symlink なら、
/// 字句的な結果と OS の解決先が食い違い symlink 検証（`PROF-4`）を迂回し得るため
/// [`ProfileError::InvalidLayout`] を返す（fail-closed）。存在しない要素は symlink では
/// あり得ないのでそのまま畳み込む。
fn normalize_lexically(path: &Path) -> Result<PathBuf, ProfileError> {
    let mut out = PathBuf::new();
    let mut normal_depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                out.push(name);
                normal_depth += 1;
            }
            Component::ParentDir => {
                if normal_depth > 0 {
                    if std::fs::symlink_metadata(&out).is_ok_and(|m| m.file_type().is_symlink()) {
                        return Err(ProfileError::InvalidLayout {
                            path: out,
                            reason: "explicit profile root must not use '..' after a symlink",
                        });
                    }
                    out.pop();
                    normal_depth -= 1;
                }
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
        }
    }
    Ok(out)
}

/// 既存ディレクトリが他者にアクセス可能（unix で group/other の権限ビットまたは sticky 等が立っている）なら拒否する。
///
/// [`Profile::open`] はルート自体を `fchmod(0700)` するため、`/tmp` のような共有
/// ディレクトリを指定されると他者の利用を壊す（`PROF-1`）。存在しない・取得できない
/// 場合は [`Profile::open`] 側の検証に委ねる。unix 以外では何もしない。
fn reject_shared_existing_dir(root: &Path) -> Result<(), ProfileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(root)
            && meta.is_dir()
            && meta.mode() & 0o5077 != 0
        {
            return Err(ProfileError::InvalidLayout {
                path: root.to_path_buf(),
                reason: "explicit profile root must not be an existing shared directory",
            });
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
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

    /// 明示指定があれば [`ExplicitStore::open_or_create`]（fd 基準の共有ディレクトリ
    /// 拒否つき）、なければ既定側の `open_or_create` に委譲する。
    fn open_or_create(&self) -> Result<Profile, ProfileError> {
        match &self.explicit {
            Some(explicit) => explicit.open_or_create(),
            None => self.default.open_or_create(),
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

/// [`to_long_path`] の変換結果の種別（`XOS-8`、TASK-61（61.1）、MS-3）。
///
/// 将来の種別追加に備えて `non_exhaustive` とする（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LongPathKind {
    /// Windows 以外。Unix には `MAX_PATH` 相当の制約がないための意図的な no-op
    /// であり、未実装のスタブではない。
    Unchanged,
    /// ドライブ絶対パス（`C:\...` → `\\?\C:\...`）。
    Disk,
    /// UNC パス（`\\server\share\...` → `\\?\UNC\server\share\...`）。
    Unc,
    /// 入力が既に `\\?\` で始まっていたため変更しなかった。
    AlreadyVerbatim,
}

/// [`to_long_path`] の結果。変換後のパスとその種別を持つ。
///
/// [`ResolvedRoot`] と同様、生の `PathBuf` ではなく構造体にして後から情報を
/// 拡張できるようにする（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LongPath {
    path: PathBuf,
    kind: LongPathKind,
}

impl LongPath {
    /// 変換後のパス。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 変換の種別。
    pub fn kind(&self) -> LongPathKind {
        self.kind
    }

    /// パスの所有権を取り出す。
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

/// Win32 の長パス上限（終端 NUL を含めて 32767 UTF-16 単位）。
#[cfg(any(windows, test))]
const VERBATIM_MAX_UNITS_WITH_NUL: usize = 32767;
/// verbatim パス本体の UTF-16 単位数の上限。Win32 の上限 32767 単位には終端 NUL の
/// 1 単位が含まれるため、本体は最大 32766 単位とする（入力長・変換後長の両方で適用）。
#[cfg(any(windows, test))]
const VERBATIM_MAX_UNITS: usize = VERBATIM_MAX_UNITS_WITH_NUL - 1;
/// 付与しうる最長プレフィックス（`\\?\UNC\`）の UTF-16 単位数。
#[cfg(any(windows, test))]
const VERBATIM_MAX_PREFIX_UNITS: usize = 8;

/// `\\?\` プレフィックスを付けた verbatim パスへ変換する（`XOS-8`、TASK-61（61.1）、MS-3）。
///
/// [`ProfileStore::open_or_create`] が Windows でルートを [`Profile::open`] へ渡す前に
/// 呼ぶ。`LongPathsEnabled` レジストリ設定に依存せず 260 文字（`MAX_PATH`）の制約を
/// 回避するための変換で、規則は次のとおり。
///
/// - `C:\a` → `\\?\C:\a`（[`LongPathKind::Disk`]）
/// - `\\server\share\a` → `\\?\UNC\server\share\a`（[`LongPathKind::Unc`]）
/// - 既に `\\?\` で始まる → 変更しない（[`LongPathKind::AlreadyVerbatim`]）
///   （`\\?\X:\...` と `\\?\UNC\server\share\...` のみ許可し、`..`・`.`・空要素・`/`・
///   `GLOBALROOT` 等のデバイス名前空間は検証して拒否する）
/// - `\\?\` は Win32 の正規化を無効にするため、`/` を `\` へ、連続区切りを 1 つへ、
///   `.` 要素を除去する。末尾のドット・空白は verbatim では保存される（名前の
///   正規化は `XOS-9`・TASK-62 の責務でここでは行わない）
///
/// fail-closed で [`ProfileError::InvalidLayout`] を返す入力: `..` 要素（verbatim では
/// リテラル扱いとなり、解決するとプロファイル境界外へ抜ける経路になるため）、
/// 相対・ドライブ相対・ルート相対・空のパス、デバイス名前空間（`\\.\`・`//?/` 等）、
/// 上限（終端 NUL を含め 32767 UTF-16 単位、すなわち本体 32766 単位）超過。
///
/// Rust の `std::fs` も Windows で長い絶対パスを自動的に verbatim 化するが、どの長さで
/// 変換されるかは std の実装詳細である。本関数は std 内部への暗黙依存をなくし、
/// 子プロセスへ渡す引数や `XOS-7` の ACL 実装で使う Win32 直接呼び出し等、`std::fs` を
/// 経由しない経路でも長パスを扱えるようにする。
///
/// Windows 以外では入力をそのまま [`LongPathKind::Unchanged`] で返す（意図的な no-op）。
#[cfg(windows)]
pub fn to_long_path(path: &Path) -> Result<LongPath, ProfileError> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    // 確保前に UTF-16 単位数を上限 + 1 で打ち切り、巨大入力でのメモリ消費を防ぐ。
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .take(VERBATIM_MAX_UNITS + 1)
        .collect();
    match to_verbatim_wide(&wide) {
        Ok((out, kind)) => Ok(LongPath {
            path: PathBuf::from(OsString::from_wide(&out)),
            kind,
        }),
        Err(reason) => Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason,
        }),
    }
}

/// [`to_long_path`] の非 Windows 版。Unix には同種の制約がないため入力をそのまま返す
/// 意図的な no-op（スタブではない）。
#[cfg(not(windows))]
pub fn to_long_path(path: &Path) -> Result<LongPath, ProfileError> {
    Ok(LongPath {
        path: path.to_path_buf(),
        kind: LongPathKind::Unchanged,
    })
}

/// [`to_long_path`] の変換規則の本体（UTF-16 単位列に対する純関数）。
///
/// Linux 上では `Path::components` が `C:\` を解釈できないため、target 非依存の
/// 純関数として実装し、全 OS の単体テストで全規則を検証できるようにする。
#[cfg(any(windows, test))]
fn to_verbatim_wide(input: &[u16]) -> Result<(Vec<u16>, LongPathKind), &'static str> {
    const BS: u16 = b'\\' as u16;
    const FS: u16 = b'/' as u16;
    const QUESTION: u16 = b'?' as u16;
    const DOT: u16 = b'.' as u16;
    const COLON: u16 = b':' as u16;
    const VERBATIM: [u16; 4] = [BS, BS, QUESTION, BS];
    const UNC_PREFIX: [u16; 8] = [
        BS,
        BS,
        QUESTION,
        BS,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        BS,
    ];
    let is_sep = |u: u16| u == BS || u == FS;

    // アロケーション前に入力長を粗く検証する（無制限確保による DoS の防止）。変換後の
    // 長さによる厳密な判定は各経路の末尾で行う（付与するプレフィックスが入力形式で
    // 異なり、既存 verbatim は付与なしのため、一律の差し引きは有効な長パスを誤拒否する）。
    let too_long = "path is too long for long path conversion";
    if input.len() > VERBATIM_MAX_UNITS {
        return Err(too_long);
    }
    // 埋め込み NUL は Win32 のファイル API が文字列終端として扱い、別パスへ切り詰められる
    // （プロファイル境界の回避経路になる）ため、全経路で変換前に拒否する。
    if input.contains(&0) {
        return Err("path must not contain NUL characters");
    }
    if let Some(rest) = input.strip_prefix(&VERBATIM) {
        validate_verbatim_rest(rest)?;
        // 入力長は上記で上限以下のため、変換後（無変更）の長さ判定は不要。
        return Ok((input.to_vec(), LongPathKind::AlreadyVerbatim));
    }
    // `\\?\` の厳密一致以外のデバイス名前空間（`\\.\`・`//?/` 等）は拒否する。
    if let (Some(&a), Some(&b), Some(&c), Some(&d)) =
        (input.first(), input.get(1), input.get(2), input.get(3))
        && is_sep(a)
        && is_sep(b)
        && (c == QUESTION || c == DOT)
        && is_sep(d)
    {
        return Err("device namespace paths are not supported for long path conversion");
    }

    // verbatim では `/` が区切りにならないため先に `\` へ揃える。
    let norm: Vec<u16> = input
        .iter()
        .map(|&u| if u == FS { BS } else { u })
        .collect();

    // 区切りで分割した要素から `.`・空要素を除き、`..` は拒否する。
    let push_elements =
        |out: &mut Vec<u16>, rest: &[u16], first_sep: bool| -> Result<(), &'static str> {
            let mut need_sep = first_sep;
            for elem in rest.split(|&u| u == BS) {
                if elem.is_empty() || elem == [DOT] {
                    continue;
                }
                if elem == [DOT, DOT] {
                    return Err("parent directory components are not allowed in long paths");
                }
                if need_sep {
                    out.push(BS);
                }
                out.extend_from_slice(elem);
                need_sep = true;
            }
            Ok(())
        };

    let not_absolute = "long path conversion requires an absolute drive or UNC path";
    let mut out: Vec<u16> = Vec::with_capacity(norm.len() + VERBATIM_MAX_PREFIX_UNITS);

    // ドライブ絶対パス `X:\...`
    if let (Some(&letter), Some(&colon), Some(&sep)) = (norm.first(), norm.get(1), norm.get(2))
        && u8::try_from(letter).is_ok_and(|l| l.is_ascii_alphabetic())
        && colon == COLON
        && sep == BS
    {
        out.extend_from_slice(&VERBATIM);
        out.push(letter);
        out.push(COLON);
        out.push(BS);
        let rest = norm.get(3..).unwrap_or(&[]);
        push_elements(&mut out, rest, false)?;
        if out.len() > VERBATIM_MAX_UNITS {
            return Err(too_long);
        }
        return Ok((out, LongPathKind::Disk));
    }

    // UNC `\\server\share\...`（3 連続以上の区切りは拒否）
    if let (Some(&a), Some(&b)) = (norm.first(), norm.get(1))
        && a == BS
        && b == BS
    {
        let rest = norm.get(2..).unwrap_or(&[]);
        let mut segs = rest.split(|&u| u == BS);
        let server = segs.next().unwrap_or(&[]);
        let share = segs.next().unwrap_or(&[]);
        let invalid = |s: &[u16]| s.is_empty() || s == [DOT] || s == [DOT, DOT];
        if invalid(server) || invalid(share) {
            return Err("UNC path requires a valid server and share name");
        }
        out.extend_from_slice(&UNC_PREFIX);
        out.extend_from_slice(server);
        out.push(BS);
        out.extend_from_slice(share);
        // server と share の直後の残り要素を追加する。
        let consumed = server.len() + 1 + share.len();
        let tail = rest.get(consumed..).unwrap_or(&[]);
        push_elements(&mut out, tail, true)?;
        if out.len() > VERBATIM_MAX_UNITS {
            return Err(too_long);
        }
        return Ok((out, LongPathKind::Unc));
    }

    // 相対・ドライブ相対・ルート相対・NT 形式（`\??\`）・空は拒否（fail-closed）。
    Err(not_absolute)
}

/// 既に `\\?\` で始まる入力の残り部分（プレフィックス除去後）を検証する（`XOS-8`・`PROF-1`）。
///
/// 許可する形式は `\\?\X:\...`（ドライブ）と `\\?\UNC\server\share\...` のみ。
/// `\\?\GLOBALROOT\...`・`\\?\Volume{...}` 等のデバイス名前空間は拒否する。verbatim では
/// `/`・`.`・`..` がリテラル扱いとなり境界外へ抜ける経路になり得るため、`/` を含む入力と
/// `.`・`..`・空の要素（末尾の 1 つの区切りを除く）も拒否する（fail-closed）。
#[cfg(any(windows, test))]
fn validate_verbatim_rest(rest: &[u16]) -> Result<(), &'static str> {
    const BS: u16 = b'\\' as u16;
    const DOT: u16 = b'.' as u16;
    let bad = "verbatim path must be a drive or UNC path without '.', '..' or '/' components";

    if rest.contains(&(b'/' as u16)) {
        return Err(bad);
    }
    let is_drive = matches!(
        (rest.first(), rest.get(1), rest.get(2)),
        (Some(&l), Some(&c), Some(&s))
            if u8::try_from(l).is_ok_and(|l| l.is_ascii_alphabetic())
                && c == b':' as u16
                && s == BS
    );
    let is_unc = rest.len() > 4
        && rest.get(3) == Some(&BS)
        && rest.get(..3).is_some_and(|p| {
            p.iter()
                .zip(b"UNC")
                .all(|(&u, &b)| u8::try_from(u).is_ok_and(|u| u.eq_ignore_ascii_case(&b)))
        });
    let tail = if is_drive {
        rest.get(3..).unwrap_or(&[])
    } else if is_unc {
        rest.get(4..).unwrap_or(&[])
    } else {
        return Err(bad);
    };
    // ドライブのルート（`\\?\C:\`）はそのまま許可する。UNC は server・share が必要。
    if tail.is_empty() {
        return if is_drive { Ok(()) } else { Err(bad) };
    }
    // 末尾の区切り 1 つだけは許可する。取り除いて空になる（`\\?\C:\\` のように区切りが
    // 連続する）場合は空要素を含むため拒否する。
    let tail = tail.strip_suffix(&[BS]).unwrap_or(tail);
    if tail.is_empty() {
        return Err(bad);
    }
    let mut count = 0usize;
    for elem in tail.split(|&u| u == BS) {
        if elem.is_empty() || elem == [DOT] || elem == [DOT, DOT] {
            return Err(bad);
        }
        count += 1;
    }
    if is_unc && count < 2 {
        return Err("UNC path requires a valid server and share name");
    }
    Ok(())
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
        for (candidate, expected) in [
            ("/a", "/a"),
            ("/a/b/..", "/a"),
            ("/a/../b", "/b"),
            ("/tmp/new-dir/../profile", "/tmp/profile"),
            ("/a/./b", "/a/b"),
        ] {
            let store = ExplicitStore::new(candidate).unwrap();
            assert_eq!(store.root(), Path::new(expected));
        }
    }

    /// PROF-1: `..` を含む明示パスでも、最終ルートの外に途中要素が作成される前に畳み込まれる。
    /// `Profile::open` は unix のみ対応（Windows は XOS-7..10 で未実装）のため unix に限る。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_dotdot_creates_nothing_outside_final_root() {
        let tmp = TempDir::new();
        // macOS の `/var` などの symlink を Profile::open が拒否するため実体化する。
        std::fs::create_dir_all(&tmp.0).unwrap();
        let base = std::fs::canonicalize(&tmp.0).unwrap();
        let raw = base.join("new-dir").join("..").join("profile");
        let store = ExplicitStore::new(raw).unwrap();
        assert_eq!(store.root(), base.join("profile").as_path());
        let profile = Profile::open(store.root()).unwrap();
        assert!(base.join("profile").is_dir());
        assert!(!base.join("new-dir").exists());
        drop(profile);
    }

    /// PROF-1・PROF-4: 既存 symlink の直後の `..` は字句的畳み込みと OS 解決が食い違うため拒否する。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_dotdot_after_symlink_is_rejected() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(&tmp.0).unwrap();
        let base = std::fs::canonicalize(&tmp.0).unwrap();
        std::fs::create_dir_all(base.join("real").join("deep")).unwrap();
        std::os::unix::fs::symlink(base.join("real").join("deep"), base.join("link")).unwrap();
        let raw = base.join("link").join("..").join("profile");
        match ExplicitStore::new(raw) {
            Err(ProfileError::InvalidLayout { reason, .. }) => {
                assert_eq!(
                    reason,
                    "explicit profile root must not use '..' after a symlink"
                );
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
    }

    /// PROF-1: sticky bit 付きの共有ディレクトリ（`/tmp` 相当）は専用ルートとして拒否する。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_shared_existing_dir_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let shared = tmp.0.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        match ExplicitStore::new(shared.clone()) {
            Err(ProfileError::InvalidLayout { reason, .. }) => {
                assert_eq!(
                    reason,
                    "explicit profile root must not be an existing shared directory"
                );
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
        // 専用の子ディレクトリは受け付ける。
        assert!(ExplicitStore::new(shared.join("profile")).is_ok());
    }

    /// PROF-1: 親から setgid が継承された 0700 ディレクトリ（mode 02700）は専用ルートとして受け付ける。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_setgid_only_dir_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.0.join("setgid-root");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o2700)).unwrap();
        assert!(ExplicitStore::new(dir).is_ok());
    }

    /// PROF-1: group 書込可（0770）の既存ディレクトリも共有とみなして拒否する。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_group_writable_dir_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let shared = tmp.0.join("group-shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        match ExplicitStore::new(shared.clone()) {
            Err(ProfileError::InvalidLayout { reason, .. }) => {
                assert_eq!(
                    reason,
                    "explicit profile root must not be an existing shared directory"
                );
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
    }

    /// PROF-1: 他者が読み取り・通過できる 0755 の既存ディレクトリも拒否し、
    /// `fchmod(0700)` で他利用者の権限を奪わない。
    #[test]
    #[cfg(unix)]
    fn prof_1_explicit_world_readable_dir_is_rejected_and_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let shared = tmp.0.join("world-readable");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        match ExplicitStore::new(shared.clone()) {
            Err(ProfileError::InvalidLayout { reason, .. }) => {
                assert_eq!(
                    reason,
                    "explicit profile root must not be an existing shared directory"
                );
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
        let mode = std::fs::metadata(&shared).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    /// PROF-1: `new` の後に共有形へ変わったルートも、`open_or_create` が開いた fd の
    /// `fstat` で拒否し、`fchmod(0700)` で権限を奪わない。
    #[test]
    #[cfg(unix)]
    fn prof_1_open_or_create_rejects_shared_root_by_fd() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let root = tmp.0.join("late-shared");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = ExplicitStore::new(root.clone()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).unwrap();
        match store.open_or_create() {
            Err(ProfileError::InvalidLayout { reason, .. }) => {
                assert_eq!(
                    reason,
                    "explicit profile root must not be an existing shared directory"
                );
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o770);
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
    // ---- XOS-8（TASK-61（61.1））: 長パス変換 ----

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn conv(s: &str) -> Result<(String, LongPathKind), &'static str> {
        to_verbatim_wide(&w(s)).map(|(v, k)| (String::from_utf16(&v).unwrap(), k))
    }

    #[test]
    fn xos_8_disk_path_gets_verbatim_prefix() {
        assert_eq!(
            conv(r"C:\Users\a\fandhe-browser"),
            Ok((
                r"\\?\C:\Users\a\fandhe-browser".to_string(),
                LongPathKind::Disk
            ))
        );
        assert_eq!(
            conv(r"C:\"),
            Ok((r"\\?\C:\".to_string(), LongPathKind::Disk))
        );
    }

    #[test]
    fn xos_8_unc_path_gets_unc_prefix() {
        assert_eq!(
            conv(r"\\server\share\dir"),
            Ok((r"\\?\UNC\server\share\dir".to_string(), LongPathKind::Unc))
        );
        assert_eq!(
            conv(r"\\server\share"),
            Ok((r"\\?\UNC\server\share".to_string(), LongPathKind::Unc))
        );
    }

    #[test]
    fn xos_8_already_verbatim_is_unchanged() {
        assert_eq!(
            conv(r"\\?\C:\x"),
            Ok((r"\\?\C:\x".to_string(), LongPathKind::AlreadyVerbatim))
        );
    }

    #[test]
    fn xos_8_forward_slashes_and_dot_are_normalized() {
        assert_eq!(
            conv("C:/a/./b//c/"),
            Ok((r"\\?\C:\a\b\c".to_string(), LongPathKind::Disk))
        );
    }

    #[test]
    fn xos_8_verbatim_input_is_validated() {
        for ok in [
            r"\\?\C:\",
            r"\\?\C:\a\b",
            r"\\?\UNC\srv\share",
            r"\\?\unc\srv\share\a",
        ] {
            assert!(conv(ok).is_ok(), "{ok}");
        }
        for bad in [
            r"\\?\C:\root\..\outside",
            r"\\?\C:\a\.\b",
            r"\\?\C:\a\\b",
            "\\\\?\\C:/a",
            r"\\?\GLOBALROOT\Device\x",
            r"\\?\Volume{1234}\a",
            r"\\?\UNC\srv",
            r"\\?\UNC\srv\share\..\x",
            r"\\?\C:",
        ] {
            assert!(conv(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn xos_8_parent_dir_is_rejected() {
        let reason = "parent directory components are not allowed in long paths";
        assert_eq!(conv(r"C:\a\..\b"), Err(reason));
        assert_eq!(conv(r"\\server\share\..\b"), Err(reason));
    }

    #[test]
    fn xos_8_non_absolute_is_rejected() {
        let reason = "long path conversion requires an absolute drive or UNC path";
        for input in [r"a\b", "C:foo", r"\foo", r"\??\C:\x", ""] {
            assert_eq!(conv(input), Err(reason), "input: {input:?}");
        }
        let unc = "UNC path requires a valid server and share name";
        for input in [r"\\server", r"\\server\", r"\\\server\share"] {
            assert_eq!(conv(input), Err(unc), "input: {input:?}");
        }
    }

    #[test]
    fn xos_8_device_namespace_is_rejected() {
        let reason = "device namespace paths are not supported for long path conversion";
        for input in [r"\\.\C:\x", "//?/C:/x", r"\\?/C:\x"] {
            assert_eq!(conv(input), Err(reason), "input: {input:?}");
        }
    }

    #[test]
    fn xos_8_over_limit_is_rejected() {
        // 終端 NUL の 1 単位を確保するため、本体の上限は 32766 単位（32767 は拒否）。
        assert_eq!(VERBATIM_MAX_UNITS, 32766);
        let max = VERBATIM_MAX_UNITS;
        let at_win32_limit = format!(r"\\?\C:\{}", "a".repeat(32767 - 7));
        assert_eq!(at_win32_limit.len(), 32767);
        assert_eq!(
            to_verbatim_wide(&w(&at_win32_limit)),
            Err("path is too long for long path conversion")
        );
        let reason = Err("path is too long for long path conversion");
        // ドライブ: プレフィックス 4 単位を付与した結果がちょうど上限。
        let ok = format!(r"C:\{}", "a".repeat(max - 4 - 3));
        let (out, kind) = to_verbatim_wide(&w(&ok)).unwrap();
        assert_eq!(kind, LongPathKind::Disk);
        assert_eq!(out.len(), max);
        let over = format!(r"C:\{}", "a".repeat(max - 4 - 2));
        assert_eq!(to_verbatim_wide(&w(&over)), reason);
        // UNC: プレフィックス 6 単位（`\\` を `\\?\UNC\` へ）の増加でちょうど上限。
        let ok = format!(r"\\s\h\{}", "a".repeat(max - 6 - 6));
        let (out, kind) = to_verbatim_wide(&w(&ok)).unwrap();
        assert_eq!(kind, LongPathKind::Unc);
        assert_eq!(out.len(), max);
        let over = format!(r"\\s\h\{}", "a".repeat(max - 6 - 5));
        assert_eq!(to_verbatim_wide(&w(&over)), reason);
    }

    #[test]
    fn xos_8_existing_verbatim_near_limit_is_accepted() {
        // 既存 verbatim は付与なしのため、32760 単位でも上限内なら受理する。
        let max = VERBATIM_MAX_UNITS;
        let input = format!(r"\\?\C:\{}", "a".repeat(32760 - 7));
        assert_eq!(input.len(), 32760);
        let (out, kind) = to_verbatim_wide(&w(&input)).unwrap();
        assert_eq!(kind, LongPathKind::AlreadyVerbatim);
        assert_eq!(out.len(), 32760);
        let exact = format!(r"\\?\C:\{}", "a".repeat(max - 7));
        assert!(to_verbatim_wide(&w(&exact)).is_ok());
        let over = format!(r"\\?\C:\{}", "a".repeat(max - 6));
        assert_eq!(
            to_verbatim_wide(&w(&over)),
            Err("path is too long for long path conversion")
        );
    }

    #[test]
    fn xos_8_embedded_nul_is_rejected() {
        let nul = "path must not contain NUL characters";
        for input in [
            "C:/a\0b",
            "C:\\a\0",
            "\\\\?\\C:\\a\0b",
            "\\\\server\\share\\a\0b",
        ] {
            assert_eq!(to_verbatim_wide(&w(input)), Err(nul), "input: {input:?}");
        }
    }

    #[test]
    fn xos_8_verbatim_drive_root_with_extra_separator_is_rejected() {
        let bad = "verbatim path must be a drive or UNC path without '.', '..' or '/' components";
        for input in [r"\\?\C:\\", r"\\?\C:\\\"] {
            assert_eq!(to_verbatim_wide(&w(input)), Err(bad), "input: {input:?}");
        }
        assert!(to_verbatim_wide(&w(r"\\?\C:\")).is_ok());
    }

    #[test]
    fn xos_8_long_disk_path_over_260() {
        let input = format!(r"C:\{}", vec!["d".repeat(60); 5].join(r"\"));
        assert!(input.len() > 260);
        let (out, kind) = conv(&input).unwrap();
        assert_eq!(kind, LongPathKind::Disk);
        assert_eq!(out, format!(r"\\?\{input}"));
        assert_eq!(out.len(), input.len() + 4);
    }

    #[cfg(not(windows))]
    #[test]
    fn xos_8_non_windows_is_unchanged() {
        let long = to_long_path(Path::new("/tmp/a/b")).unwrap();
        assert_eq!(long.path(), Path::new("/tmp/a/b"));
        assert_eq!(long.kind(), LongPathKind::Unchanged);
        assert_eq!(long.into_path(), PathBuf::from("/tmp/a/b"));
    }

    #[cfg(windows)]
    #[test]
    fn xos_8_long_path_fs_roundtrip_on_windows() {
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Ok(long) = to_long_path(&self.0) {
                    let _ = std::fs::remove_dir_all(long.path());
                }
            }
        }
        let top = std::env::temp_dir().join(format!("fandhe-long-{}", std::process::id()));
        let _guard = Guard(top.clone());
        let mut deep = top.clone();
        for i in 0..6 {
            deep.push(format!("{i}-{}", "x".repeat(50)));
        }
        assert!(deep.as_os_str().len() > 300);

        let long = to_long_path(&deep).unwrap();
        assert!(long.path().to_string_lossy().starts_with(r"\\?\"));
        assert!(matches!(
            long.kind(),
            LongPathKind::Disk | LongPathKind::Unc
        ));

        std::fs::create_dir_all(long.path()).unwrap();
        let file = long.path().join("data.txt");
        std::fs::write(&file, b"long-path").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"long-path");
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 9);
    }
}
