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
//!   `PROF-4`、TASK-50（50.2）・#177）。本モジュールはファイルシステムの
//!   ルートから `root` に至るまでの各パス要素（既存・新規作成のいずれも）が
//!   symlink でないことを検証する（PR #437 レビュー指摘への対応。下記
//!   `create_dir_all_verified` 参照）が、任意の相対パス文字列の正規化・
//!   境界判定は行わない。この検証により、呼び出し元は `root` に至る経路上に
//!   symlink を含まない実体パスを渡す必要がある（macOS の `/var`・一部
//!   Linux ディストリビューションの `/home` のような OS 標準 symlink を経由
//!   する場合は事前に `canonicalize` する運用を要する。#177 で境界定義が
//!   固まった際に見直しうる制約）
//! - `profile.lock`（`std::fs::File::try_lock`）による二重 open の拒否
//!   （`PROF-1`、TASK-50（50.3）・#178。#175 の決定により `fs2` は使わない）
//! - プロファイル削除処理（`PROF-5`、TASK-53）
//! - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
//! - Windows での ACL によるアクセス制限（`XOS-7`〜`XOS-10`）。実装がないため
//!   `Profile::open` は Windows では常に `Err(ProfileError::Unsupported)` を
//!   返し、既定 ACL のまま機密データを書き込む偽装成功を避ける（security.md
//!   「偽装・回避機能の禁止」）
//!
//! 上記が未実装のため、`Profile::open` は同一プロファイルへの二重 open を
//! 拒否しない（#178 で解消するまでの既知の制約）。

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
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
    /// ディレクトリ以外（通常ファイル等）として存在する場合、または
    /// ルート検証後からサブディレクトリ作成までの間にルートが差し替え
    /// られた場合に使う（security.md「プロファイル境界」: symlink を
    /// たどって境界外のディレクトリへ書き込む経路を作らないための検出）。
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
    ///   [`ProfileError::Io`]
    /// - ファイルシステムのルートから `root`（またはサブディレクトリ）に
    ///   至るまでのいずれかのパス要素（既存・新規作成のいずれも）が
    ///   symlink、またはディレクトリ以外として存在する場合、もしくは
    ///   ルート検証後にルートが差し替えられたことを検出した場合
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
            // "root/"）を除去する。Unix の `lstat` は末尾に区切り文字が付いた
            // パスだと最終要素が symlink でも実体をたどってしまうため、
            // 末尾区切り文字が残ったままだと symlink 検出をすり抜ける
            // （PR #437 Bugbot 指摘）。
            let root: PathBuf = root.as_ref().components().collect();

            // ルートを作成する。`create_dir_all_verified` はファイルシステムの
            // ルートから 1 要素ずつ累積しながら、既存かどうかを問わず各段階が
            // symlink でないことを検証するため、`std::fs::create_dir_all` と
            // 異なり親パスの symlink を経由した境界外への書き込みを許さない
            // （PR #437 レビュー指摘）。
            let root = create_dir_all_verified(&root)?;
            // ルート検証後からサブディレクトリ作成までの間にルートが symlink へ
            // 差し替えられる TOCTOU（PR #437 レビュー指摘）を防ぐため、
            // `set_dir_permissions_0700` が fd 経由の `fstat` で確認した
            // (dev, ino) をそのまま「検証済みの実体識別子」として受け取り、
            // サブディレクトリを作成する直前ごとに再検証する。ここで新たに
            // `symlink_metadata` を呼び直すと、その呼び出し自体の前に生じた
            // 差し替えを見逃す窓ができるため、chmod 検証で得た値を再利用する。
            // 完全な解消には openat/mkdirat 相当のディレクトリハンドル（fd）
            // 基準の作成が要るが、それには `libc` 等の新規依存追加、または
            // 新規 `unsafe` FFI（Windows の ACL 実装にも同様に新規依存が要る）
            // のいずれかが必要で、いずれも dependency-policy.md・
            // coding-rust.md によりユーザー承認が要る（本 PR では承認待ちのため
            // 未実施。承認事項として報告する）。本実装は承認なしで実施できる
            // 範囲の多層防御として、直前の (dev, ino) 再検証を行う。
            let root_identity = set_dir_permissions_0700(&root)?;

            for kind in DataKind::ALL.iter().copied() {
                let current_root_meta = std::fs::symlink_metadata(&root)?;
                if current_root_meta.is_symlink()
                    || (current_root_meta.dev(), current_root_meta.ino()) != root_identity
                {
                    return Err(ProfileError::InvalidLayout {
                        path: root.clone(),
                        reason: "root directory identity changed while creating subdirectories",
                    });
                }

                let path = root.join(kind.dir_name());
                create_single_dir_verified(&path)?;
                set_dir_permissions_0700(&path)?;
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

/// `path` が symlink ではなく実ディレクトリであることを確認する。
///
/// symlink 先をたどってパーミッションを変更しないよう、chmod の呼び出し
/// より必ず前に検査する（security.md「プロファイル境界」）。Windows では
/// `Profile::open` がここへ到達する前に `ProfileError::Unsupported` で
/// 拒否するため、本関数は Unix 専用とする。
#[cfg(unix)]
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

/// パスの 1 要素（単一階層。既に存在すれば作成せず、なければ作成する）に
/// ついて symlink でない実ディレクトリであることを検証する。
///
/// `create_dir_all_verified`（ファイルシステムのルートから累積する呼び出し
/// 元）と、`Profile::open` のサブディレクトリ作成（root 直下 1 階層）の
/// 両方から使う共通の最小単位。`std::fs::create_dir_all` と異なり単一階層の
/// `std::fs::create_dir` を使うため、中間ディレクトリを一括作成しない。
/// 作成と検証の間の競合（他プロセスによる symlink への差し替え）への
/// 多層防御として、作成直後に必ず `ensure_real_directory` で再検証する
/// （PR #437 レビュー指摘）。
#[cfg(unix)]
fn create_single_dir_verified(path: &Path) -> Result<(), ProfileError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => ensure_real_directory(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                // 並行して他の呼び出し（同一プロファイルへの再 open 等）が
                // 同じ要素を作成した可能性がある。symlink でなければ許容する。
                Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(create_err) => return Err(ProfileError::Io(create_err)),
            }
            ensure_real_directory(path)
        }
        Err(err) => Err(ProfileError::Io(err)),
    }
}

/// `path` に至るまでのディレクトリを作成する（`std::fs::create_dir_all` の
/// 安全な代替）。
///
/// `create_dir_all` はパス全体を一括でカーネルへ渡すため、途中の親要素が
/// symlink でもカーネル側でたどってしまう（PR #437 レビュー指摘: 親パスの
/// symlink を経由した境界外への書き込み）。本関数はパスの先頭（ファイル
/// システムのルート）から 1 要素ずつ累積しながら、**既存かどうかを問わず**
/// 各段階の累積パスに対して `create_single_dir_verified` を適用する。
/// 既に存在する段階では symlink でないことだけを検証し（新規作成しない）、
/// 存在しない段階では作成した上で直後に検証する。これにより、呼び出し元が
/// 渡した `root` のどの階層であっても、事前に攻撃者が仕込んだ symlink を
/// 無条件に信用することがない（「既存の祖先だから安全」という前提を置かない）。
///
/// 一般的な OS 標準ディレクトリ（例: macOS の `/var` -> `/private/var`）が
/// 途中に含まれる環境では、そのまま渡すと `InvalidLayout` になる。呼び出し元
/// （テストコード含む）は `std::env::temp_dir()` のような取得直後のパスを
/// そのまま使わず、`canonicalize` 済みの基点を使うことでこれを回避できる。
/// 本関数はセキュリティ境界（プロファイル境界。security.md）を優先し、
/// 利便性のために symlink を黙って信用する分岐を持たない。
///
/// 完全な TOCTOU 除去には openat/mkdirat 相当のディレクトリハンドル基準の
/// 実装が要るが、それには新規依存（`libc` 等）または新規 `unsafe` FFI が
/// 必要で、いずれもユーザー承認が要る（dependency-policy.md・
/// coding-rust.md）。本関数は承認なしで実施できる範囲の多層防御として、
/// 各段階の検証と（必要なら）作成を隣接させ、時間窓を「1 要素分」に縮める。
#[cfg(unix)]
fn create_dir_all_verified(path: &Path) -> Result<PathBuf, ProfileError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        create_single_dir_verified(&current)?;
    }
    Ok(current)
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
/// `cfg(target_os = ...)` に局所化する方針に従う）。`Profile::open` は
/// Windows では本関数に到達する前に `ProfileError::Unsupported` で拒否する。
///
/// 戻り値は fd 経由の `fstat`（`fd_meta`）で確認した検証済みの `(dev, ino)`。
/// 呼び出し元（`Profile::open`）はこれをルートの「検証済み実体識別子」の
/// 起点として保持し、以降の再検証に使う。ここで改めて `symlink_metadata` を
/// 呼び直すと、その呼び出し自体より前に生じた差し替えを見逃す新たな時間窓が
/// できるため、本関数内で確認済みの値をそのまま返す（PR #437 レビュー指摘の
/// 多層防御を呼び出し元へ伝播する）。
#[cfg(unix)]
fn set_dir_permissions_0700(path: &Path) -> Result<(u64, u64), ProfileError> {
    let link_meta = std::fs::symlink_metadata(path)?;
    if link_meta.is_symlink() || !link_meta.is_dir() {
        return Err(ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path is a symlink or not a directory",
        });
    }

    // ディレクトリを `File::open` でオープンし、fd 経由で fstat する
    // （実体確認のため。read (r) ビットが無ければここで EACCES となり、
    // `?` によりエラーとして呼び出し元へ伝播する。fail-closed のため
    // 実害はないが、open 自体が権限非依存で成功するわけではない）。
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
    Ok((fd_meta.dev(), fd_meta.ino()))
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
            // `std::env::temp_dir()` を `canonicalize` した基点から組み立てる。
            // macOS では `std::env::temp_dir()` が `/var/...`（`/var` は
            // `/private/var` への OS 標準 symlink）を返すため、正規化せずに
            // 使うと `create_dir_all_verified` の厳格な symlink 検証
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
        // `create_dir_all_verified` は新規作成前に `symlink_metadata` で
        // 実体を確認するため、通常ファイルとの衝突は `create_dir` を試みる
        // 前に `InvalidLayout`（非ディレクトリ）として検出される
        // （旧実装は `create_dir_all` の `Io`（EEXIST 相当）だったが、
        // 事前検証を導入したことで検出経路が変わった。PR #437 レビュー対応）。
        assert!(matches!(
            err,
            ProfileError::InvalidLayout { path, .. } if path == root
        ));
    }

    /// PROF-1（Unix 固有）: サブディレクトリのパスが通常ファイルとして
    /// 存在すると `open` は失敗する。事前検証（`create_single_dir_verified`）
    /// が `create_dir` を試みる前に非ディレクトリを検出するため、
    /// ルートが通常ファイルの場合（`prof_1_open_fails_when_root_is_a_file`）
    /// と同様に `InvalidLayout` になる。
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
    /// 同じ検証をルートにも適用していることを確認する。
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

    /// PROF-1（Unix 固有）: `root` に至る途中の祖先が存在しない場合、
    /// `create_dir_all_verified` がそれらをすべて新規作成する（各要素は
    /// 作成の都度 symlink でないことを検証されるため、中間ディレクトリの
    /// 一括作成でも境界検証を素通りしない）。
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
