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
//! - 相対パス（`..` 等）を用いたルート配下検証（`assert_within_root` 等。
//!   `PROF-4`、TASK-50（50.2）・#177）。本モジュールは unix では
//!   `rustix` の `openat`/`mkdirat`（ディレクトリハンドル基準の作成。下記
//!   `open_or_create_child_dir` 参照）により、ファイルシステムのルートから
//!   `root` に至るまでの各パス要素が symlink でないことを検証するが、任意の
//!   相対パス文字列の正規化・境界判定は行わない。この検証により、呼び出し元は
//!   `root` に至る経路上に symlink を含まない実体パスを渡す必要がある
//!   （macOS の `/var`・一部 Linux ディストリビューションの `/home` のような
//!   OS 標準 symlink を経由する場合は事前に `canonicalize` する運用を要する。
//!   #177 で境界定義が固まった際に見直しうる制約）
//! - `profile.lock`（`std::fs::File::try_lock`）による二重 open の拒否
//!   （`PROF-1`、TASK-50（50.3）・#178。#175 の決定により `fs2` は使わない）
//! - プロファイル削除処理（`PROF-5`、TASK-53）
//! - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
//! - Windows での ACL によるアクセス制限（`XOS-7`〜`XOS-10`）。実装がないため
//!   `Profile::open` は Windows では常に `Err(ProfileError::Unsupported)` を
//!   返し、既定 ACL のまま機密データを書き込む偽装成功を避ける（security.md
//!   「偽装・回避機能の禁止」）。ハンドル基準の作成（unix。下記参照）は
//!   Windows へは移植しておらず、XOS-7〜XOS-10 実装時にパス文字列でなく
//!   ハンドル相対（`FILE_FLAG_OPEN_REPARSE_POINT` 等）で作成する要件を
//!   引き継ぐ（REPAIR-3）
//!
//! 上記が未実装のため、`Profile::open` は同一プロファイルへの二重 open を
//! 拒否しない（#178 で解消するまでの既知の制約）。

use std::fmt;
use std::path::{Path, PathBuf};

// unix ではディレクトリハンドル（fd）基準の openat/mkdirat/fchmod を使い、
// パス文字列の再解決に伴う TOCTOU（symlink 差し替え競合。PR #437 レビュー
// 指摘）を解消する（ユーザー承認済み・2026-09-26。dependency-policy.md）。
// `Component` は `open_dir_all_verified` の走査でのみ使うため、Windows
// ビルドで unused import（`-D warnings`）にならないよう unix 限定にする。
#[cfg(unix)]
use rustix::fd::OwnedFd;
#[cfg(unix)]
use rustix::fs::{CWD, Mode, OFlags};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use std::path::Component;

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
    /// ディレクトリへ書き込む経路を作らないための検出）。unix では
    /// `openat`/`mkdirat` に `OFlags::NOFOLLOW` を指定するため、判定時点と
    /// 作成・検証時点の間でパスが symlink へ差し替えられても、ハンドルは
    /// 既に確定した実体を指したままで別の実体へすり替わらない
    /// （PR #437 レビュー指摘: ディレクトリハンドル基準の作成）。
    InvalidLayout {
        /// 想定外のレイアウトを検出したパス。
        path: PathBuf,
        /// 検出内容を示す英語メッセージ（プログラム出力文字列は英語。
        /// japanese-style.md）。
        reason: &'static str,
    },
    /// 現在のプラットフォームではプロファイルの隔離を安全に実施できないため、
    /// あえてプロファイルを作成せず `open` を失敗させる場合に返す。
    ///
    /// Windows は ACL によるアクセス制限が未実装（`XOS-7`〜`XOS-10`。
    /// TASK-50（50.1）・#176 のスコープ外）であり、何もせず成功を返すと
    /// 既定 ACL（親から継承した権限）のまま Cookie・Storage 等の機密データを
    /// 書き込むことになる。「未実装の機能で成功を一律に返すフォールバックを
    /// 禁止する」方針（security.md・REPAIR-3）に従い、実効的な隔離ができる
    /// ようになるまで `open` 自体を拒否する。
    Unsupported {
        /// 未対応の理由を示す英語メッセージ。
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
            ProfileError::Unsupported { reason } => {
                write!(
                    f,
                    "profile isolation unsupported on this platform: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProfileError::Io(source) => Some(source),
            ProfileError::InvalidLayout { .. } => None,
            ProfileError::Unsupported { .. } => None,
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
    /// 渡す前提。相対パスの境界検証は #177 の `assert_within_root` が担当し、
    /// 本関数はまだ持たない）。
    ///
    /// まだ `profile.lock` を取らないため、本関数は同一プロファイルへの
    /// 二重 open を拒否しない（REPAIR-3。#178 で解消する）。
    ///
    /// Windows では現時点で ACL による隔離を実装していないため、常に
    /// [`ProfileError::Unsupported`] を返す（何も作成しない。`XOS-7`〜
    /// `XOS-10` で解消するまでの既知の制約）。
    ///
    /// # エラー
    ///
    /// - ディレクトリの作成・パーミッション設定で入出力エラーが起きた場合
    ///   [`ProfileError::Io`]（unix では途中の祖先ディレクトリに読み取り
    ///   権限がない場合も `openat` が `EACCES` を返し、ここに含まれる）
    /// - ファイルシステムのルートから `root`（またはサブディレクトリ）に
    ///   至るまでのいずれかのパス要素（既存・新規作成のいずれも）が
    ///   symlink、またはディレクトリ以外として存在する場合
    ///   [`ProfileError::InvalidLayout`]（symlink 先のパーミッションを
    ///   変更しないよう、chmod より前に検査する）
    /// - Windows など ACL 隔離が未実装のプラットフォームでは常に
    ///   [`ProfileError::Unsupported`]
    pub fn open(root: impl AsRef<Path>) -> Result<Profile, ProfileError> {
        #[cfg(not(unix))]
        {
            // ディレクトリを一切作成せず即座に拒否する（fail-closed。
            // 上記モジュール doc・`ProfileError::Unsupported` 参照）。
            let _ = root;
            Err(ProfileError::Unsupported {
                reason: "ACL-based directory isolation is not implemented on this platform yet (tracked by XOS-7..XOS-10); refusing to create a profile with inherited, unrestricted permissions",
            })
        }

        #[cfg(unix)]
        {
            // `Path::components()` を経由して再構築し、末尾区切り文字（例:
            // "root/"）を除去する。以降の走査は `PathBuf` の各 component を
            // 順に辿るため、この正規化は表示・比較用のパスを整えるだけで、
            // 境界検証そのものは下記のハンドル基準の走査が担う
            // （PR #437 Bugbot 指摘）。
            let root: PathBuf = root.as_ref().components().collect();

            // ファイルシステムのルート（または CWD）から `root` まで、
            // 1 要素ずつディレクトリハンドル（fd）を辿りながら作成する。
            // `create_dir_all` のようにパス全体を一括でカーネルへ渡さず、
            // 前段で得たハンドルを次段の `openat`/`mkdirat` の起点として
            // 使うため、判定と作成・chmod の間でパス文字列を再解決する
            // window が生じない（PR #437 レビュー指摘の TOCTOU 解消）。
            let root_fd = open_dir_all_verified(&root)?;
            set_dir_permissions_0700(&root_fd)?;

            for kind in DataKind::ALL.iter().copied() {
                let path = root.join(kind.dir_name());
                let dir_fd = open_or_create_child_dir(&root_fd, kind.dir_name().as_ref(), &path)?;
                set_dir_permissions_0700(&dir_fd)?;
            }

            Ok(Profile { root })
        }
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

/// `openat`/`mkdirat` に渡す共通フラグ。既存ディレクトリを開く場合も
/// 新規作成後に開き直す場合も同じフラグを使う。`NOFOLLOW` により、対象が
/// symlink であれば `ELOOP` として拒否し、シンボリックリンクの先を決して
/// たどらない（security.md「プロファイル境界」）。`CLOEXEC` は子プロセスへ
/// 意図せず fd を継承しないための標準的な多層防御。
#[cfg(unix)]
const OPEN_DIR_FLAGS: OFlags = OFlags::DIRECTORY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::RDONLY);

/// `rustix` の `Errno` を、対象パスを添えた [`ProfileError`] へ変換する。
///
/// `Errno::LOOP`（symlink を検出）・`Errno::NOTDIR`（ディレクトリ以外が
/// 存在）は境界検証としての拒否（[`ProfileError::InvalidLayout`]）に、
/// それ以外は入出力エラー（[`ProfileError::Io`]）に分類する。
#[cfg(unix)]
fn classify_dir_open_error(path: &Path, err: Errno) -> ProfileError {
    match err {
        Errno::LOOP => ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path is a symlink, expected a real directory",
        },
        Errno::NOTDIR => ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path exists but is not a directory",
        },
        other => ProfileError::Io(other.into()),
    }
}

/// `parent`（検証済みのディレクトリハンドル）配下の `name` を開く。
/// 存在しなければ `mkdirat` で作成してから同じフラグで開き直す。
///
/// `parent` はハンドル（fd）であり、呼び出し時点で既にディレクトリの実体を
/// 指している。`openat`/`mkdirat` は `parent` を起点にカーネル内で名前解決
/// するため、`name` の位置に他プロセスが symlink を仕込んでいても
/// `OFlags::NOFOLLOW` により `ELOOP` で拒否され、そのリンク先を作成・確認
/// することはない。判定（存在確認）から作成、作成後の確認までが同一の
/// `openat` 呼び出しに閉じるため、パス文字列を再解決する呼び出し間の
/// TOCTOU（PR #437 レビュー指摘）が生じない。
///
/// `display_path` はエラー時に返す `Path`（呼び出し元が組み立てた表示用の
/// 累積パス）。実際の名前解決には使わない。
#[cfg(unix)]
fn open_or_create_child_dir(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<OwnedFd, ProfileError> {
    match rustix::fs::openat(parent, name, OPEN_DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => {
            match rustix::fs::mkdirat(parent, name, Mode::RWXU) {
                Ok(()) => {}
                // 並行して他の呼び出し（同一プロファイルへの再 open 等）が
                // 同じ要素を作成した可能性がある。直後の openat で symlink
                // でないことを確認するため、ここでは許容する。
                Err(Errno::EXIST) => {}
                Err(err) => return Err(ProfileError::Io(err.into())),
            }
            rustix::fs::openat(parent, name, OPEN_DIR_FLAGS, Mode::empty())
                .map_err(|err| classify_dir_open_error(display_path, err))
        }
        Err(err) => Err(classify_dir_open_error(display_path, err)),
    }
}

/// `path` の祖先をファイルシステムのルート（絶対パスの場合）または
/// カレントディレクトリ（相対パスの場合）から順にハンドルで辿り、必要な
/// 中間ディレクトリを作成しながら `path` 自身のディレクトリハンドルを返す
/// （`std::fs::create_dir_all` の安全な代替）。
///
/// `create_dir_all` はパス全体を一括でカーネルへ渡すため、途中の親要素が
/// symlink でもカーネル側でたどってしまう（PR #437 レビュー指摘: 親パスの
/// symlink を経由した境界外への書き込み）。本関数は前段で得たディレクトリ
/// ハンドルを次段の `open_or_create_child_dir` の起点として使い回すため、
/// 各要素の判定・作成・確認がすべて直前に確定したハンドルを基準に行われ、
/// パスの文字列表現を再解決する window が生じない。
///
/// 一般的な OS 標準ディレクトリ（例: macOS の `/var` -> `/private/var`）が
/// 途中に含まれる環境では、そのまま渡すと `InvalidLayout` になる。呼び出し元
/// （テストコード含む）は `std::env::temp_dir()` のような取得直後のパスを
/// そのまま使わず、`canonicalize` 済みの基点を使うことでこれを回避できる。
/// 本関数はセキュリティ境界（プロファイル境界。security.md）を優先し、
/// 利便性のために symlink を黙って信用する分岐を持たない。
#[cfg(unix)]
fn open_dir_all_verified(path: &Path) -> Result<OwnedFd, ProfileError> {
    let mut components = path.components().peekable();

    // 起点のハンドルを用意する。絶対パスなら "/" を、相対パスなら "." を
    // 現在の作業ディレクトリ（`CWD`）基準で開く。以降はこのハンドルだけを
    // 名前解決の起点として使い、パス文字列全体を渡し直すことはない。
    let is_absolute = matches!(components.peek(), Some(Component::RootDir));
    let mut current: OwnedFd = if is_absolute {
        components.next();
        rustix::fs::openat(CWD, "/", OPEN_DIR_FLAGS, Mode::empty())
            .map_err(|err| classify_dir_open_error(Path::new("/"), err))?
    } else {
        rustix::fs::openat(CWD, ".", OPEN_DIR_FLAGS, Mode::empty())
            .map_err(|err| classify_dir_open_error(Path::new("."), err))?
    };

    // エラー時に呼び出し元へ返す表示用パスを、実際に検証した実体と対応する
    // 完全なパス文字列として組み立てる（絶対パスなら "/" から積み上げる）。
    // これにより `ProfileError::InvalidLayout { path, .. }` が呼び出し元の
    // 渡した `root` と一致する（テストでの比較対象）。
    let mut display_path = if is_absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for component in components {
        match component {
            Component::Normal(name) => {
                display_path.push(name);
                current = open_or_create_child_dir(&current, name, &display_path)?;
            }
            Component::ParentDir => {
                display_path.push("..");
                current = rustix::fs::openat(&current, "..", OPEN_DIR_FLAGS, Mode::empty())
                    .map_err(|err| classify_dir_open_error(&display_path, err))?;
            }
            // `Path::components()` は連続する区切り文字や単独の "." を
            // 正規化して取り除くため、`CurDir`/`Prefix`（unix では出現
            // しない）はここには到達しない前提だが、フォールスルーで
            // 現在のハンドルをそのまま使い回して安全側に倒す。
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }

    Ok(current)
}

/// unix ではディレクトリハンドル `fd` のパーミッションを `0o700` に設定する。
///
/// `fd` は [`open_dir_all_verified`]・[`open_or_create_child_dir`] が
/// `OFlags::NOFOLLOW` 付きの `openat` で取得した、symlink ではないことを
/// 確認済みのハンドルである。`fchmod` はそのハンドルに対して直接作用する
/// ため、パス文字列を再解決する chmod（`std::fs::set_permissions`）と異なり、
/// 呼び出しの直前に対象を symlink へ差し替える TOCTOU が原理的に生じない
/// （PR #437 レビュー指摘）。
///
/// Windows は POSIX パーミッションを持たないため、`Profile::open` は本関数へ
/// 到達する前に `ProfileError::Unsupported` で拒否する（ACL による隔離は
/// 本 crate の未実装範囲。`XOS-7`〜`XOS-10`。TASK-50（50.1）・#176 のスコープ
/// 外。coding-rust.md「クロスプラットフォーム」で OS 固有処理を
/// `cfg(target_os = ...)` に局所化する方針に従う）。
#[cfg(unix)]
fn set_dir_permissions_0700(fd: &OwnedFd) -> Result<(), ProfileError> {
    rustix::fs::fchmod(fd, Mode::RWXU).map_err(|err| ProfileError::Io(err.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
            // `std::env::temp_dir()` を `canonicalize` した基点から組み立てる。
            // macOS では `std::env::temp_dir()` が `/var/...`（`/var` は
            // `/private/var` への OS 標準 symlink）を返すため、正規化せずに
            // 使うと `open_dir_all_verified` の厳格な symlink 検証
            // （PR #437 レビュー指摘: 途中要素の symlink を無条件に信用しない）
            // に「意図しない既存の symlink」として弾かれてしまう。テスト用
            // 一時ディレクトリの基点をあらかじめ正規化しておくことで、
            // 本番コードの検証を緩めずにこの環境依存差異を吸収する。
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            let path = base.join(format!("fandhe-profile-test-{}-{n}", std::process::id()));
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

    /// PROF-1（Unix 固有）: `Profile::data_dir` が `root.join(<名前>)` と
    /// 一致する。
    #[cfg(unix)]
    #[test]
    fn prof_1_data_dir_matches_root_join() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");
        for kind in DataKind::ALL.iter().copied() {
            assert_eq!(profile.data_dir(kind), tmp.path().join(kind.dir_name()));
        }
    }

    /// PROF-1（Unix 固有）: ルートが通常ファイルとして存在すると `Err` になる。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_root_is_a_file() {
        let tmp = TempDir::new();
        // 親ディレクトリを作ってから、root パス自体を通常ファイルにする。
        let parent = tmp.path();
        std::fs::create_dir_all(parent).expect("親ディレクトリの作成");
        let root = parent.join("root-as-file");
        std::fs::write(&root, b"not a directory").expect("ファイル作成");

        let err = Profile::open(&root).expect_err("root がファイルなら失敗する");
        // `open_dir_all_verified` は `openat(..., OFlags::DIRECTORY, ...)` で
        // 開こうとするため、通常ファイルとの衝突は `ENOTDIR` として
        // `InvalidLayout`（非ディレクトリ）に分類される（PR #437 レビュー対応）。
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == root
        ));
    }

    /// PROF-1（Unix 固有）: サブディレクトリのパスが通常ファイルとして
    /// 存在すると `open` は失敗する。`open_or_create_child_dir` の
    /// `openat(..., OFlags::DIRECTORY, ...)` が `mkdirat` を試みる前に
    /// 非ディレクトリを検出するため、ルートが通常ファイルの場合
    /// （`prof_1_open_fails_when_root_is_a_file`）と同様に `InvalidLayout`
    /// になる。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_subdir_is_a_file() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");
        let cache_path = tmp.path().join(DataKind::Cache.dir_name());
        std::fs::write(&cache_path, b"not a directory").expect("ファイル作成");

        let err = Profile::open(tmp.path()).expect_err("cache がファイルなら失敗する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));
    }

    /// PROF-1（Unix 固有）: `root/cookies` が外部ディレクトリへの symlink だと
    /// `Err(InvalidLayout)` になり、symlink 先のモードは変更されない。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_subdir_is_a_symlink() {
        use std::os::unix::fs::symlink;

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

    /// PROF-1（Unix 固有）: `root/cookies` が存在しない実体を指す dangling
    /// symlink でも `Err(InvalidLayout)` になり、symlink 先には何も作られない。
    /// `mkdirat` はリンク先の存在有無に関わらず `OFlags::NOFOLLOW` により
    /// symlink 自体を対象として `ELOOP` を返すため、ハンドル基準の作成が
    /// リンク先を辿って作成していないことを直接確認する（PR #437 P0 指摘の
    /// 回帰テスト: 判定と作成の間で symlink へ差し替えられてもリンク先へは
    /// 書き込まれないこと）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_subdir_is_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");

        let external = TempDir::new();
        // `external.path()` 自体は作成しない（未作成の実体を指す dangling
        // symlink にする）。
        let dangling_target = external.path().join("never-created");

        let cookies_path = tmp.path().join(DataKind::Cookies.dir_name());
        symlink(&dangling_target, &cookies_path).expect("symlink 作成");

        let err = Profile::open(tmp.path()).expect_err("dangling symlink なら失敗する");
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == cookies_path
        ));

        assert!(
            !dangling_target.exists(),
            "symlink のリンク先にディレクトリが作成されてはならない"
        );
        assert!(
            !external.path().exists(),
            "symlink のリンク先の親ディレクトリも作成されてはならない"
        );
    }

    /// PROF-1（Unix 固有）: 既存のルート・サブディレクトリが `0o755` で
    /// 作られていても、`open` はディレクトリハンドル基準の `fchmod` により
    /// すべて `0o700` へ締め直す。パス基準の chmod ではなくハンドル基準の
    /// `fchmod` が実際に効いていることを、既存ディレクトリの再 open で確認する
    /// （PR #437 P0 指摘の回帰テスト）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_rechmods_preexisting_dirs_to_0700() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");
        std::fs::set_permissions(tmp.path(), Permissions::from_mode(0o755))
            .expect("ルートのパーミッション設定");
        for kind in DataKind::ALL.iter().copied() {
            let path = tmp.path().join(kind.dir_name());
            std::fs::create_dir_all(&path).expect("サブディレクトリ作成");
            std::fs::set_permissions(&path, Permissions::from_mode(0o755))
                .expect("サブディレクトリのパーミッション設定");
        }

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
            assert_eq!(
                mode, 0o700,
                "{:?} のパーミッションが 0o700 に締め直されない",
                kind
            );
        }
    }

    /// PROF-1（Unix 固有）: `root` 自体が外部ディレクトリへの symlink だと
    /// `Err(InvalidLayout)` になり、symlink 先のモードは変更されない。
    /// サブディレクトリ側（`prof_1_open_fails_when_subdir_is_a_symlink`）と
    /// 同じ検証をルートにも適用していることを確認する。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_root_is_a_symlink() {
        use std::os::unix::fs::symlink;

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
    /// 回帰テスト）。`open_dir_all_verified` は `Path::components()` を
    /// 1 要素ずつ辿って `openat` するため、末尾に `/` が付いていても
    /// component の集合は変わらず、検査が無効化されることはない。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_rejects_root_symlink_with_trailing_slash() {
        use std::os::unix::fs::symlink;

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

    /// PROF-1（Unix 固有）: `root` に至る途中の祖先が存在しない場合、
    /// `open_dir_all_verified` がそれらをすべて新規作成する（各要素は
    /// `openat`/`mkdirat` により作成の都度 symlink でないことを検証されるため、
    /// 中間ディレクトリの一括作成でも境界検証を素通りしない）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_creates_missing_intermediate_dirs() {
        let tmp = TempDir::new();
        let nested_root = tmp.path().join("a").join("b").join("profile-root");

        let profile = Profile::open(&nested_root).expect("中間ディレクトリごと作成できる");
        let root_mode = std::fs::metadata(profile.root())
            .expect("root の metadata 取得")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
    }

    /// PROF-1（Unix 固有）: `root` の親（祖先）が外部ディレクトリへの
    /// symlink だと `Err(InvalidLayout)` になり、`root` 自体は作成されない
    /// （PR #437 レビュー指摘の本丸: 「親パスの symlink を経由してプロファイル
    /// 境界外へ書き込める」の回帰テスト。`prof_1_open_fails_when_root_is_a_symlink`
    /// は `root` 自体が symlink のケースを検証するが、こちらは `root` の
    /// *親* が symlink で、`root` 自体はまだ存在しないケースを検証する）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_parent_dir_is_a_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("親ディレクトリの作成");

        let external = TempDir::new();
        std::fs::create_dir_all(external.path()).expect("外部ディレクトリの作成");

        let link_path = tmp.path().join("link");
        symlink(external.path(), &link_path).expect("symlink 作成");

        let root = link_path.join("profile-root");
        let err = Profile::open(&root).expect_err("親が symlink なら失敗する");
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == link_path
        ));

        // symlink 先（境界外）にはプロファイルディレクトリが作られていない。
        assert!(
            !external.path().join("profile-root").exists(),
            "境界外にディレクトリが作成されてはならない"
        );
    }

    /// PROF-1（Windows 固有）: ACL 隔離が未実装のため、`open` は常に
    /// `Unsupported` を返し、ディレクトリを一切作成しない（security.md
    /// 「偽装・回避機能の禁止」。PR #437 レビュー指摘への対応）。
    #[cfg(windows)]
    #[test]
    fn prof_1_open_is_unsupported_on_windows() {
        let tmp = TempDir::new();
        let err = Profile::open(tmp.path()).expect_err("Windows では常に失敗する");
        assert!(matches!(err, ProfileError::Unsupported { .. }));
        assert!(
            !tmp.path().exists(),
            "ACL 隔離を実装できないプラットフォームではディレクトリを作成しない"
        );
    }
}
