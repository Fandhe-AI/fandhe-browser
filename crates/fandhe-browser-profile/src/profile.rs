//! プロファイルルート（ユーザーデータディレクトリ）の骨格を担うモジュール。
//!
//! `fandhe-browser-cli`（TASK-41）が `Profile::open(root)` を呼んで
//! プロファイルを開き、そこから構築した `AppState`（core `state.rs`・
//! TASK-41.1）を `fandhe-browser-cdp`・`fandhe-browser-ai` 側のサーバーへ
//! 渡す設計を目指す（ビヘイビア `PROF-1`・`PROF-6`、TASK-50（50.1）、
//! MS-3「基盤層・JS エンジン・プロファイル・OS 差異吸収層」）。
//!
//! 本モジュールの責務はルートディレクトリと、データ種別ごとに分離した
//! 4 つのサブディレクトリ（[`DataKind`] の 4 種）の作成に限る。以下は
//! 後続タスクが本モジュールへ差し込む契約であり、本モジュールは実装済みを
//! 装わない（REPAIR-3・code-comment-style.md）。
//!
//! - パストラバーサル防止・ルート配下検証（`assert_within_root` 等。
//!   `PROF-4`、TASK-50（50.2）・#177）
//! - `profile.lock`（`std::fs::File::try_lock`）による二重 open の拒否
//!   （`PROF-1`、TASK-50（50.3）・#178。#175 の決定により `fs2` は使わない）
//! - プロファイル削除処理（`PROF-5`、TASK-53）
//! - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
//!
//! 上記が未実装のため、`Profile::open` は同一プロファイルへの二重 open を
//! 拒否しない（#178 で解消するまでの既知の制約）。

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// プロファイルディレクトリ構築に失敗した際のエラー。
///
/// `fandhe-browser-core::error::Error` と同様、自前で `Display`・
/// `std::error::Error`・`From<io::Error>` を実装する（`thiserror` 等の
/// 外部依存は追加しない。dependency-policy.md）。`#[non_exhaustive]` により、
/// `assert_within_root`（#177）・ロック（#178）が新規バリアントを追加しても
/// 非破壊にする（REPAIR-4）。
#[derive(Debug)]
#[non_exhaustive]
pub enum ProfileError {
    /// ディレクトリ作成・パーミッション設定時の入出力エラー。
    Io(std::io::Error),
    /// プロファイル配下に想定外のレイアウトを検出した場合に返す。
    ///
    /// ルート・サブディレクトリの想定パスが symlink である場合や、
    /// ディレクトリ以外（通常ファイル等）として存在する場合に使う
    /// （security.md「プロファイル境界」: symlink をたどって境界外の
    /// ディレクトリへ書き込む経路を作らないための検出）。
    InvalidLayout {
        /// 想定外のレイアウトを検出したパス。
        path: PathBuf,
        /// 検出内容を示す英語メッセージ（プログラム出力文字列は英語。
        /// japanese-style.md）。
        reason: &'static str,
    },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileError::Io(source) => write!(f, "I/O error: {source}"),
            ProfileError::InvalidLayout { path, reason } => {
                write!(f, "invalid profile layout at {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProfileError::Io(source) => Some(source),
            ProfileError::InvalidLayout { .. } => None,
        }
    }
}

impl From<std::io::Error> for ProfileError {
    fn from(source: std::io::Error) -> Self {
        ProfileError::Io(source)
    }
}

/// プロファイル配下でデータ種別ごとに分離するディレクトリの種類。
///
/// `Profile::open` の作成ループと、境界検証を行う後続タスク（#177・#179）
/// のテストの両方が [`DataKind::ALL`] を介してこの一覧を共有する想定。
/// `#[non_exhaustive]` により、将来のデータ種別追加を非破壊にする
/// （REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DataKind {
    /// HTTP Cookie の保存領域。
    Cookies,
    /// `localStorage`/`sessionStorage` 等の Web Storage 保存領域。
    Storage,
    /// HTTP キャッシュ・レンダリング用中間データの保存領域。
    Cache,
    /// 閲覧履歴の保存領域。
    History,
}

impl DataKind {
    /// 全データ種別を列挙する（`Profile::open` の作成ループ、`data_dir` の
    /// テスト、#179 の隔離テストが共有する一覧）。
    ///
    /// スライスで公開する（固定長配列 `[DataKind; N]` にしない）。`N` は
    /// 公開シグネチャの一部になるため、配列のままだと将来バリアントを
    /// 追加した際に呼び出し元の型が変わり、`#[non_exhaustive]`（REPAIR-4）が
    /// 意図する非破壊性と矛盾する（PR #437 レビュー指摘を反映）。
    pub const ALL: &'static [DataKind] = &[
        DataKind::Cookies,
        DataKind::Storage,
        DataKind::Cache,
        DataKind::History,
    ];

    /// プロファイルルート直下でのディレクトリ名を返す。
    pub fn dir_name(self) -> &'static str {
        match self {
            DataKind::Cookies => "cookies",
            DataKind::Storage => "storage",
            DataKind::Cache => "cache",
            DataKind::History => "history",
        }
    }
}

/// プロファイル（ユーザーデータディレクトリ）を表す型。
///
/// フィールドは非公開の `root: PathBuf` のみ持つ。#178 が `profile.lock` の
/// `File` ハンドルをここへ追加してもフィールド構成の変更が呼び出し元に
/// 見えないよう、`Clone`/`Copy` は derive しない（`Debug` のみ導出する）。
/// プロセス内にグローバル状態を持たず、値として受け渡しする設計
/// （TASK-50・PoC-7 の設計を踏襲）。
#[derive(Debug)]
pub struct Profile {
    root: PathBuf,
}

impl Profile {
    /// `root` にプロファイルを開く。存在しなければルートと 4 つの
    /// データ種別ディレクトリ（[`DataKind::ALL`]）を作成し、Unix では
    /// それぞれのパーミッションを `0o700` に設定する（`PROF-1`・`PROF-6`）。
    ///
    /// 既存のルート・サブディレクトリを渡した場合も Unix ではパーミッションを
    /// `0o700` へ締め直す副作用がある（`PROF-1` が要求する挙動）。例えば
    /// ホームディレクトリを誤って `root` に指定すると、そのディレクトリの
    /// モードが変わる点に注意（呼び出し元が正しいプロファイルルートを
    /// 渡す前提。ルート配下検証・パストラバーサル防止は #177 の
    /// `assert_within_root` が担当し、本関数はまだ持たない）。
    ///
    /// まだ `profile.lock` を取らないため、本関数は同一プロファイルへの
    /// 二重 open を拒否しない（REPAIR-3。#178 で解消する）。
    ///
    /// # エラー
    ///
    /// - ディレクトリの作成・パーミッション設定で入出力エラーが起きた場合
    ///   [`ProfileError::Io`]
    /// - ルートまたはサブディレクトリの想定パスが symlink、または
    ///   ディレクトリ以外として存在する場合 [`ProfileError::InvalidLayout`]
    ///   （symlink 先のパーミッションを変更しないよう、chmod より前に検査する）
    pub fn open(root: impl AsRef<Path>) -> Result<Profile, ProfileError> {
        // `Path::components()` を経由して再構築し、末尾区切り文字（例:
        // "root/"）を除去する。Unix の `lstat` は末尾に区切り文字が付いた
        // パスだと最終要素が symlink でも実体をたどってしまうため、
        // 末尾区切り文字が残ったままだと `ensure_real_directory` の
        // symlink 検出をすり抜ける（PR #437 Bugbot 指摘）。
        let root: PathBuf = root.as_ref().components().collect();

        std::fs::create_dir_all(&root)?;
        ensure_real_directory(&root)?;
        set_dir_permissions_0700(&root)?;

        for kind in DataKind::ALL.iter().copied() {
            let path = root.join(kind.dir_name());
            std::fs::create_dir_all(&path)?;
            ensure_real_directory(&path)?;
            set_dir_permissions_0700(&path)?;
        }

        Ok(Profile { root })
    }

    /// プロファイルルートのパスを返す。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 指定したデータ種別のディレクトリパス（`root/<dir_name>`）を返す。
    ///
    /// パスの生成のみを行い、存在確認はしない（存在は `open` 内で
    /// 保証済みの前提。ルート配下検証は #177 が個別のパス生成箇所へ
    /// 差し込む）。
    pub fn data_dir(&self, kind: DataKind) -> PathBuf {
        self.root.join(kind.dir_name())
    }
}

/// `path` が symlink ではなく実ディレクトリであることを確認する。
///
/// symlink 先をたどってパーミッションを変更しないよう、chmod の呼び出し
/// より必ず前に検査する（security.md「プロファイル境界」）。
fn ensure_real_directory(path: &Path) -> Result<(), ProfileError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_symlink() {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path is a symlink, expected a real directory",
        });
    }
    if !metadata.is_dir() {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path exists but is not a directory",
        });
    }
    Ok(())
}

/// Unix ではディレクトリのパーミッションを `0o700` に設定する。
///
/// `ensure_real_directory`（`lstat` 相当）による事前検査と、パス文字列を
/// 経由する chmod の間には、書き込み可能な第三者がディレクトリを symlink へ
/// 差し替える TOCTOU の隙が生じる（PR #437 レビュー指摘。`libc` 等の追加
/// 依存を要する `O_NOFOLLOW` + `fchmod` は dependency-policy.md によりここでは
/// 使えないため、代わりに `std::fs::File::open` でディレクトリの fd を取得し、
/// `lstat` で確認した (dev, ino) と fd の `fstat` 結果が一致することを検証
/// してから、その fd に対して `set_permissions`（`fchmod` 相当）を呼ぶ。
/// `open` から `fchmod` までは同一 fd に対する操作になるため、検査後の
/// 差し替えが window を残さない。
///
/// Windows は POSIX パーミッションを持たないため何もしない。ACL による
/// 隔離は本 crate の未実装範囲（`XOS-7`〜`XOS-10`。TASK-50（50.1）・#176 の
/// スコープ外。coding-rust.md「クロスプラットフォーム」で OS 固有処理を
/// `cfg(target_os = ...)` に局所化する方針に従う）。
#[cfg(unix)]
fn set_dir_permissions_0700(path: &Path) -> Result<(), ProfileError> {
    use std::os::unix::fs::MetadataExt;

    let link_meta = std::fs::symlink_metadata(path)?;
    if link_meta.is_symlink() || !link_meta.is_dir() {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path is a symlink or not a directory",
        });
    }

    // ディレクトリは `File::open` で読み取り用にオープンできる（Unix では
    // ディレクトリの内容を読む権限が無くても open 自体は成功する）。
    let dir = std::fs::File::open(path)?;
    let fd_meta = dir.metadata()?;

    // 直前の `symlink_metadata` が指したのと同一の実体かどうかを
    // (dev, ino) で確認する。異なれば、検査後にパスが差し替えられた
    // （symlink や別ディレクトリに置き換えられた）とみなして拒否する。
    if (fd_meta.dev(), fd_meta.ino()) != (link_meta.dev(), link_meta.ino()) {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "directory was replaced between the symlink check and permission change",
        });
    }

    dir.set_permissions(Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_permissions_0700(_path: &Path) -> Result<(), ProfileError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// テスト用の一時ディレクトリ。drop 時に再帰削除する（tempfile 等の
    /// 外部依存は追加しない方針。dependency-policy.md）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("fandhe-profile-test-{}-{n}", std::process::id()));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// PROF-1: `DataKind::dir_name` が期待する 4 値を返す。
    #[test]
    fn prof_1_data_kind_dir_names() {
        assert_eq!(DataKind::Cookies.dir_name(), "cookies");
        assert_eq!(DataKind::Storage.dir_name(), "storage");
        assert_eq!(DataKind::Cache.dir_name(), "cache");
        assert_eq!(DataKind::History.dir_name(), "history");
    }

    /// PROF-1: `Profile::data_dir` が `root.join(<名前>)` と一致する。
    #[test]
    fn prof_1_data_dir_matches_root_join() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");
        for kind in DataKind::ALL.iter().copied() {
            assert_eq!(profile.data_dir(kind), tmp.path().join(kind.dir_name()));
        }
    }

    /// PROF-1: ルートが通常ファイルとして存在すると `Err` になる。
    #[test]
    fn prof_1_open_fails_when_root_is_a_file() {
        let tmp = TempDir::new();
        // 親ディレクトリを作ってから、root パス自体を通常ファイルにする。
        let parent = tmp.path();
        std::fs::create_dir_all(parent).expect("親ディレクトリの作成");
        let root = parent.join("root-as-file");
        std::fs::write(&root, b"not a directory").expect("ファイル作成");

        let err = Profile::open(&root).expect_err("root がファイルなら失敗する");
        // `create_dir_all` は対象パスが既存の非ディレクトリ（通常ファイル）だと
        // 必ず `Io`（EEXIST 相当）を返し、`ensure_real_directory` の
        // 「非ディレクトリ」検出パスへは到達しない。期待値は具体的に絞る
        // （coding-rust.md「期待値は具体値で書く」）。
        assert!(matches!(err, ProfileError::Io(_)));
    }

    /// PROF-1: サブディレクトリのパスが通常ファイルとして存在すると
    /// `open` は失敗する。`create_dir_all` が対象パスの既存の通常ファイルと
    /// 衝突して必ず `Io`（EEXIST 相当）を返すため、ルートが通常ファイルの場合
    /// （`prof_1_open_fails_when_root_is_a_file`）と同様に `Io` に絞って確認する。
    #[test]
    fn prof_1_open_fails_when_subdir_is_a_file() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");
        let cache_path = tmp.path().join(DataKind::Cache.dir_name());
        std::fs::write(&cache_path, b"not a directory").expect("ファイル作成");

        let err = Profile::open(tmp.path()).expect_err("cache がファイルなら失敗する");
        assert!(matches!(err, ProfileError::Io(_)));
    }

    /// PROF-1（Unix 固有）: `root/cookies` が外部ディレクトリへの symlink だと
    /// `Err(InvalidLayout)` になり、symlink 先のモードは変更されない。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_subdir_is_a_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");

        let external = TempDir::new();
        std::fs::create_dir_all(external.path()).expect("外部ディレクトリの作成");
        std::fs::set_permissions(external.path(), Permissions::from_mode(0o755))
            .expect("外部ディレクトリのパーミッション設定");

        let cookies_path = tmp.path().join(DataKind::Cookies.dir_name());
        symlink(external.path(), &cookies_path).expect("symlink 作成");

        let err = Profile::open(tmp.path()).expect_err("symlink なら失敗する");
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == cookies_path
        ));

        // symlink 先のパーミッションが chmod で変更されていないことを確認する。
        let mode = std::fs::metadata(external.path())
            .expect("外部ディレクトリの metadata 取得")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    /// PROF-1（Unix 固有）: `root` 自体が外部ディレクトリへの symlink だと
    /// `Err(InvalidLayout)` になり、symlink 先のモードは変更されない。
    /// サブディレクトリ側（`prof_1_open_fails_when_subdir_is_a_symlink`）と
    /// 同じ `ensure_real_directory` を `root` にも適用していることを確認する。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_root_is_a_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new();
        let root = tmp.path();
        // `root` の親ディレクトリだけを作り、`root` パス自体は symlink にする。
        let parent = root.parent().expect("親ディレクトリが存在する");
        std::fs::create_dir_all(parent).expect("親ディレクトリの作成");

        let external = TempDir::new();
        std::fs::create_dir_all(external.path()).expect("外部ディレクトリの作成");
        std::fs::set_permissions(external.path(), Permissions::from_mode(0o755))
            .expect("外部ディレクトリのパーミッション設定");

        symlink(external.path(), root).expect("symlink 作成");

        let err = Profile::open(root).expect_err("root が symlink なら失敗する");
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == root
        ));

        // symlink 先のパーミッションが chmod で変更されていないことを確認する。
        let mode = std::fs::metadata(external.path())
            .expect("外部ディレクトリの metadata 取得")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    /// PROF-1（Unix 固有）: `root` に末尾区切り文字を付けても、`root` が
    /// symlink であることの検出をすり抜けない（PR #437 Bugbot 指摘の
    /// 回帰テスト）。末尾に `/` が付いたパスは `lstat` が symlink を
    /// たどって実体を返すため、正規化せずに `symlink_metadata` へ渡すと
    /// 検査が無効化される。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_rejects_root_symlink_with_trailing_slash() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new();
        let root = tmp.path();
        let parent = root.parent().expect("親ディレクトリが存在する");
        std::fs::create_dir_all(parent).expect("親ディレクトリの作成");

        let external = TempDir::new();
        std::fs::create_dir_all(external.path()).expect("外部ディレクトリの作成");
        std::fs::set_permissions(external.path(), Permissions::from_mode(0o755))
            .expect("外部ディレクトリのパーミッション設定");

        symlink(external.path(), root).expect("symlink 作成");

        // root の文字列表現に末尾区切り文字を付けて渡す。
        let root_with_trailing_slash = format!("{}/", root.display());
        let err = Profile::open(&root_with_trailing_slash)
            .expect_err("末尾に / を付けても symlink なら失敗する");
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == root
        ));

        // symlink 先のパーミッションが chmod で変更されていないことを確認する。
        let mode = std::fs::metadata(external.path())
            .expect("外部ディレクトリの metadata 取得")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    /// PROF-1（Unix 固有）: ルートと 4 サブディレクトリのパーミッションが
    /// `0o700` になる（網羅的な検証は #179 が行う。ここでは最小限を確認する）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_sets_mode_0700_on_unix() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        let root_mode = std::fs::metadata(profile.root())
            .expect("root の metadata 取得")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);

        for kind in DataKind::ALL.iter().copied() {
            let mode = std::fs::metadata(profile.data_dir(kind))
                .expect("サブディレクトリの metadata 取得")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{:?} のパーミッションが 0o700 でない", kind);
        }
    }
}
