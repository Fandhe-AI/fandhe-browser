//! プロファイルルート（ユーザーデータディレクトリ）の骨格を担うモジュール。
//!
//! `fandhe-browser-cli`（TASK-41）が `Profile::open(root)` を呼んで
//! プロファイルを開き、そこから構築した `AppState`（core `state.rs`・
//! TASK-41.1）を `fandhe-browser-cdp`・`fandhe-browser-ai` 側のサーバーへ
//! 渡す設計を目指す（ビヘイビア `PROF-1`・`PROF-6`、TASK-50（50.1）、
//! MS-3「基盤層・JS エンジン・プロファイル・OS 差異吸収層」）。
//!
//! 本モジュールの責務はルートディレクトリと、データ種別ごとに分離した
//! 4 つのサブディレクトリ（[`DataKind`] の 4 種）の作成、および
//! パストラバーサル防止（[`sanitize_component`]・[`assert_within_root`]。
//! `PROF-4`、TASK-50（50.2）・#177）、およびプロファイル削除
//! （[`Profile::delete`]。`PROF-5`、TASK-53（53.2）・#188）に限る。以下は後続タスクが本モジュールへ
//! 差し込む契約であり、本モジュールは実装済みを装わない（REPAIR-3・
//! code-comment-style.md）。
//!
//! - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
//! - Windows での ACL によるアクセス制限（`XOS-7`〜`XOS-10`）。実装がないため
//!   `Profile::open` は Windows では常に `Err(ProfileError::Unsupported)` を
//!   返し、既定 ACL のまま機密データを書き込む偽装成功を避ける（security.md
//!   「偽装・回避機能の禁止」）。ハンドル基準の作成（unix。下記参照）は
//!   Windows へは移植しておらず、XOS-7〜XOS-10 実装時にパス文字列でなく
//!   ハンドル相対（`FILE_FLAG_OPEN_REPARSE_POINT` 等）で作成する要件を
//!   引き継ぐ（REPAIR-3）
//!
//! `Profile::open` はルート直下の `profile.lock` に advisory lock を取ることで
//! 同一プロファイルへの二重 open を拒否する（`PROF-1`、TASK-50（50.3）・#178。
//! ロック処理本体は本モジュールから分離した [`crate::lock`] を参照。#175 の
//! 決定により `fs2` 等の追加依存は使わず、std の `File::try_lock` のみで
//! 実装する）。
//!
//! [`assert_within_root`] は字句判定のみで、`canonicalize`・`exists` 等で
//! ファイルシステムに対して再解決しない（[`assert_within_root`] の doc
//! 参照）。境界検証は「コンポーネント単位の拒否（[`sanitize_component`]）＋
//! 字句判定（[`assert_within_root`]）」と「symlink に対するハンドル基準の
//! `openat`（`open_or_create_child_dir`・[`Profile::create_file_in`]）」の
//! 二層で成り立つ。本モジュールは unix ではこのハンドル基準の作成により、
//! ファイルシステムのルートから `root` に至るまでの各パス要素が symlink
//! でないことを検証する。この検証により、呼び出し元は `root` に至る経路上に
//! symlink を含まない実体パスを渡す必要がある（macOS の `/var`・一部
//! Linux ディストリビューションの `/home` のような OS 標準 symlink を経由
//! する場合は事前に `canonicalize` する運用を要する。字句判定を採用した
//! ため今後も残る制約である）。
//!
//! `Profile::root`・`Profile::data_dir` が返す `Path` は表示・ログ用に限る
//! （PR #437 P1 レビュー指摘: `open` で検証した実体とは無関係にパス文字列を
//! 再解決してしまうと、`open` から `data_dir` 呼び出しまでの間にルートや
//! サブディレクトリが symlink へ差し替えられた場合、検証済みの実体とは
//! 別の場所を指してしまう。相対パスを渡した場合は作業ディレクトリの変更
//! でも参照先が変わり得る）。境界判定やファイル操作は unix 限定の
//! `Profile::root_fd`・`Profile::data_dir_fd` が返す検証済みハンドルを
//! 起点に `openat` 系 API で行う契約とする。後続タスク（Cookie・Storage 等
//! の実データ操作）はこの契約に従う。Windows はハンドル基準 API を持たず
//! （ACL 隔離が未実装で `open` 自体が `Unsupported` になるため）、この契約は
//! unix 限定である。

use std::fmt;
use std::path::{Path, PathBuf};

// unix ではディレクトリハンドル（fd）基準の openat/mkdirat/fchmod を使い、
// パス文字列の再解決に伴う TOCTOU（symlink 差し替え競合。PR #437 レビュー
// 指摘）を解消する（ユーザー承認済み・2026-09-26。dependency-policy.md）。
// `Component` は unix 限定の走査でのみ使うため、Windows ビルドで
// unused import（`-D warnings`）にならないよう unix 限定にする。
// `AsFd`/`BorrowedFd`/`OwnedFd` は std のものをそのまま使う（rustix は
// "std" feature 有効時にこれらを再エクスポートするのみで型は同一のため、
// 公開 API のシグネチャを std 型で表現できる）。
#[cfg(unix)]
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
// `sanitize_component`・`assert_within_root`（PROF-4、TASK-50（50.2）・#177）は
// 純粋なパス処理で unix 限定ではないため、Windows でも使う。cfg で unix に
// 限定すると Windows ビルドで未使用（`-D warnings`）になる。
use std::path::Component;

#[cfg(unix)]
use crate::lock::{self, LOCK_FILE_NAME};

/// プロファイルディレクトリ構築に失敗した際のエラー。
///
/// `fandhe-browser-core::error::Error` と同様、自前で `Display`・
/// `std::error::Error`・`From<io::Error>` を実装する（`thiserror` 等の
/// 外部依存は追加しない。dependency-policy.md）。`#[non_exhaustive]` により、
/// `assert_within_root`（#177）・ロック（#178）が新規バリアントを追加しても
/// 非破壊にする（REPAIR-4）。`InvalidComponent`・`OutsideRoot`（`PROF-4`、
/// TASK-50（50.2）・#177）・`Locked`（`PROF-1`、TASK-50（50.3）・#178）は
/// 本バージョンで追加済み。
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
    /// 別の `Profile`（同一プロセスの別インスタンス、または別プロセス）が
    /// 既に `profile.lock` を保持しているため `open` を拒否する場合に返す
    /// （`PROF-1`、TASK-50（50.3）・#178。ロック取得の詳細は [`crate::lock`]
    /// を参照）。
    Locked {
        /// ロックを取得しようとしたパス（`root/profile.lock`）。
        path: PathBuf,
    },
    /// [`sanitize_component`] がパスコンポーネントとして不正と判定した入力を
    /// 拒否する場合に返す（`PROF-4`、TASK-50（50.2）・#177）。
    ///
    /// 生の入力（untrusted なドメイン名・キー名）はフィールドに保持しない。
    /// エラー値をそのままログ・レスポンスへ流用しても、攻撃者が与えた
    /// 任意長の文字列をコピーする確保やログ汚染を招かないため（security.md
    /// 「OWASP Top 10 観点」の不安全な設計対策）。
    InvalidComponent {
        /// 拒否理由を示す英語メッセージ。
        reason: &'static str,
    },
    /// [`assert_within_root`] が、組み立てたパスがプロファイルルート配下に
    /// 収まらないと判定した場合に返す（`PROF-4`、TASK-50（50.2）・#177）。
    ///
    /// `root`・`candidate` は呼び出し元が構築した表示用のパスであり、
    /// ファイルシステムに対して検証済みの実体ではない（[`assert_within_root`]
    /// の doc 参照。字句判定のみで `canonicalize`/`exists` は行わない）。
    OutsideRoot {
        /// 比較対象としたプロファイルルート。
        root: PathBuf,
        /// ルート配下に収まらないと判定された候補パス。
        candidate: PathBuf,
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
            ProfileError::Locked { path } => {
                write!(
                    f,
                    "profile is already in use: lock is held on {}",
                    path.display()
                )
            }
            ProfileError::InvalidComponent { reason } => {
                write!(f, "invalid path component: {reason}")
            }
            ProfileError::OutsideRoot { root, candidate } => {
                write!(
                    f,
                    "path {} is outside profile root {}",
                    candidate.display(),
                    root.display()
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
            ProfileError::Locked { .. } => None,
            ProfileError::InvalidComponent { .. } => None,
            ProfileError::OutsideRoot { .. } => None,
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

/// [`sanitize_component`] を通過した、単一のパスコンポーネントであることが
/// 保証された値（`PROF-4`、TASK-50（50.2）・#177）。
///
/// フィールドを非公開にし、`sanitize_component` を経由しないと構築できない
/// ようにすることで「検証済みであること」を型で表す（REPAIR-4・coding-rust.md
/// 「公開 API」: フラットな文字列で済ませない）。`Profile::create_file_in`
/// （現状唯一の書き込み API）と、Cookie・Storage 等の実データ操作を担う後続
/// タスク（TASK-51 以降）が、ここで検証したコンポーネントを `Path::join` の
/// 引数として使う契約とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeComponent<'a>(&'a std::ffi::OsStr);

impl<'a> SafeComponent<'a> {
    /// 検証済みのコンポーネントを `&OsStr` として返す。
    pub fn as_os_str(&self) -> &'a std::ffi::OsStr {
        self.0
    }
}

impl AsRef<std::ffi::OsStr> for SafeComponent<'_> {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0
    }
}

impl AsRef<Path> for SafeComponent<'_> {
    fn as_ref(&self) -> &Path {
        Path::new(self.0)
    }
}

/// プロファイル配下でファイル・ディレクトリ名として使う 1 要素を検証する
/// （`PROF-4`、TASK-50（50.2）・#177）。
///
/// `Profile::create_file_in` から、Cookie・Storage 等の実データ操作を担う
/// 後続タスク（TASK-51 以降）まで共有する入口。ドメイン名・キー名のような
/// untrusted な外部入力は、パスへ組み込む前に必ず本関数を通す
/// （security.md「プロファイル境界」）。以下のいずれかに該当すれば拒否する。
///
/// - 空文字列、または 255 バイト（`NAME_MAX` 相当）を超える
/// - `/`・`\`（unix の `Path::components` は `\` を区切り文字として扱わない
///   ため、3 OS で挙動を揃えるために明示的に拒否する）・NUL バイトを含む
/// - `..` を部分文字列として含む（`a..b` のような値も保守的に拒否する）
/// - `.` と完全に一致する
/// - `Path::new(raw).components()` が単一の `Component::Normal(raw)` に
///   一致しない（Windows の `C:foo` のようなドライブ相対パスを弾く）
///
/// 戻り値は [`SafeComponent`]（REPAIR-4: 真偽値・フラットな文字列で返さない）。
pub fn sanitize_component(raw: &std::ffi::OsStr) -> Result<SafeComponent<'_>, ProfileError> {
    let bytes = raw.as_encoded_bytes();

    if bytes.is_empty() {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must not be empty",
        });
    }
    // 長さ検証を他の走査より先に行う（coding-rust.md「長さ・件数を上限検証
    // してからアロケーションに使う」。以降の走査は最大 255 バイトに収まる）。
    if bytes.len() > 255 {
        return Err(ProfileError::InvalidComponent {
            reason: "path component exceeds 255 bytes",
        });
    }
    if bytes.contains(&b'/') {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must not contain '/'",
        });
    }
    if bytes.contains(&b'\\') {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must not contain '\\'",
        });
    }
    if bytes.contains(&0u8) {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must not contain a NUL byte",
        });
    }
    if bytes.windows(2).any(|pair| pair == b"..") {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must not contain '..'",
        });
    }
    if bytes == b"." {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must not be '.'",
        });
    }

    let mut components = Path::new(raw).components();
    let is_single_normal_component = matches!(components.next(), Some(Component::Normal(n)) if n == raw)
        && components.next().is_none();
    if !is_single_normal_component {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must resolve to a single normal component",
        });
    }

    Ok(SafeComponent(raw))
}

/// `candidate` がプロファイルルート `root` の配下にあることを字句的に検証
/// する（`PROF-4`、TASK-50（50.2）・#177）。
///
/// **字句判定のみであり、ファイルシステム上の実体は再検証しない**
/// （`canonicalize`・`exists`・`metadata` を一切呼ばない。REPAIR-3:
/// 実装済みを装わない）。PoC-7（`docs/spec/03-poc/profile-isolation/
/// profile-proto`）は `canonicalize()`/`exists()` で解決してから比較する
/// 実装だったが、本 crate は `root()`/`data_dir()` を表示専用とし、実際の
/// 操作は `root_fd`/`data_dir_fd` を起点にした `openat` + `NOFOLLOW`
/// （[`open_or_create_child_dir`]・[`Profile::create_file_in`]）で行う契約
/// （PR #437 レビュー指摘の TOCTOU 解消）のため、ここでパス文字列を
/// ファイルシステムに対して解決し直すとその契約が崩れる。symlink に対する
/// 防御はハンドル基準の `openat` 層が担い、本関数は「組み立てたパス文字列が
/// `..` 等でルート外を指していないか」だけを見る。
///
/// 判定手順:
/// 1. `candidate.strip_prefix(root)` が失敗すれば `Err`（`/tmp/prof` と
///    `/tmp/prof2/x` のような兄弟プレフィクスは一致しない）
/// 2. 残りのパスが空なら `Err`（ルート自身は書き込み先として認めない）
/// 3. 残りのコンポーネントがすべて `Component::Normal` でなければ `Err`
///    （`ParentDir`・`RootDir`・`Prefix`・先頭の `CurDir` を拒否する。
///    シンボリックリンクが絡むと `..` の字句的な解決は意味が曖昧になるため、
///    `ParentDir` を解決して許容する方式は採らない）
///
/// 大文字小文字を区別しない OS（Windows・macOS 既定）では、大文字小文字だけが
/// 異なるパスを「ルート外」と判定し得る（拒否側に倒れるため安全側の制約。
/// coding-rust.md「クロスプラットフォーム」）。
pub fn assert_within_root(root: &Path, candidate: &Path) -> Result<(), ProfileError> {
    let outside_root = || ProfileError::OutsideRoot {
        root: root.to_path_buf(),
        candidate: candidate.to_path_buf(),
    };

    let rest = candidate.strip_prefix(root).map_err(|_| outside_root())?;

    let mut components = rest.components().peekable();
    if components.peek().is_none() {
        // ルート自身（`rest` が空）は書き込み先として認めない。
        return Err(outside_root());
    }
    for component in components {
        if !matches!(component, Component::Normal(_)) {
            return Err(outside_root());
        }
    }

    Ok(())
}

/// unix 限定: [`Profile::open`] が検証したルート直下の 4 サブディレクトリの
/// ハンドル（fd）。フィールドをデータ種別ごとに明示することで、
/// [`DataKind`] にバリアントが追加された際にコンパイラがフィールド追加・
/// `Profile::data_dir_fd` の match 追加を強制する（`Vec` 等で動的に持つより
/// REPAIR の「非破壊だが取りこぼしを機械的に検出できる」性質を保てる）。
#[cfg(unix)]
#[derive(Debug)]
struct DataDirFds {
    cookies: OwnedFd,
    storage: OwnedFd,
    cache: OwnedFd,
    history: OwnedFd,
}

/// プロファイル（ユーザーデータディレクトリ）を表す型。
///
/// `root: PathBuf` は表示・ログ用（[`Profile::root`]・[`Profile::data_dir`]
/// 参照）。unix ではこれに加えて `open` 時に検証済みのディレクトリハンドル
/// （`root_fd`・`data_fds`）を保持し、`Drop` まで生存させる。ハンドルを
/// 保持せずパスだけを保存すると、`open` から後続のファイル操作までの間に
/// ルートやサブディレクトリが symlink へ差し替えられた場合、パスの再解決が
/// 検証済みの実体とは別の場所を指してしまう（PR #437 P1 レビュー指摘）。
/// `Clone`/`Copy` は derive しない（`Debug` のみ導出する。fd の二重所有を
/// 防ぐため）。プロセス内にグローバル状態を持たず、値として受け渡しする
/// 設計（TASK-50・PoC-7 の設計を踏襲）。`OwnedFd` は `Send`/`Sync` のため、
/// 本構造体も自動導出で `Send`/`Sync` になる。
#[derive(Debug)]
pub struct Profile {
    root: PathBuf,
    #[cfg(unix)]
    root_fd: OwnedFd,
    #[cfg(unix)]
    data_fds: DataDirFds,
    // フィールドは宣言順に drop されるため、ロックは他のハンドルより後、
    // 最後に解放される。一度も読まないため `_` を付けて dead_code を避ける
    // （`ProfileLock` は `Drop` でアンロックする副作用のためだけに保持する。
    // PROF-1、TASK-50（50.3）・#178）。
    #[cfg(unix)]
    _lock: lock::ProfileLock,
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
    /// 渡す前提。`root` 自体の妥当性検証は本関数の範囲外であり、
    /// [`assert_within_root`]（`PROF-4`・#177）はここで検証済みの `root` を
    /// 基点に、その配下で組み立てたパスがルート外を指していないかを
    /// 検証する役割を担う）。
    ///
    /// ルート直下の `profile.lock` に advisory lock（std `File::try_lock`）を
    /// 取ることで、同一プロファイルへの二重 open を拒否する（`PROF-1`、
    /// TASK-50（50.3）・#178。ロック取得の詳細は [`crate::lock`] を参照）。
    /// 副作用として `root/profile.lock`（モード `0o600`）を作成し、`Profile`
    /// を drop した後もこのファイル自体は残り続ける（unlink しない理由は
    /// [`crate::lock`] のモジュール doc を参照）。
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
    /// - 既に別の `Profile` が同じルートの `profile.lock` を保持している場合
    ///   [`ProfileError::Locked`]（この場合データ種別ディレクトリは一切
    ///   作成・変更しない）
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

            // 相対パスを渡された場合、ここで一度だけ `std::env::current_dir`
            // 基準の絶対パスへ変換して `Profile` に保存する。変換しないまま
            // 保存すると、`open` 呼び出し後にプロセスの作業ディレクトリが
            // 変わった際、`root()`/`data_dir()` が返すパスの意味も変わって
            // しまう（PR #437 P1 レビュー指摘）。`std::path::absolute` は
            // シンボリックリンクを解決せず、絶対パスはそのまま返す
            // （相対パスのみ `current_dir()` を前置する）ため、直後に行う
            // ハンドル基準の走査（`open_dir_all_verified`）の結果と矛盾しない。
            let root = std::path::absolute(&root)?;

            // ファイルシステムのルート（絶対パスなので必ず "/"）から `root`
            // まで、1 要素ずつディレクトリハンドル（fd）を辿りながら作成する。
            // `create_dir_all` のようにパス全体を一括でカーネルへ渡さず、
            // 前段で得たハンドルを次段の `openat`/`mkdirat` の起点として
            // 使うため、判定と作成・chmod の間でパス文字列を再解決する
            // window が生じない（PR #437 レビュー指摘の TOCTOU 解消）。
            let root_fd = open_dir_all_verified(&root)?;
            set_dir_permissions_0700(&root_fd)?;

            // ロックファイルはデータ種別ディレクトリを作成する前に取得する。
            // ロック取得に失敗した（= 既に別の Profile が開いている）2 回目の
            // opener は、以降の行に到達せずここで返るため、data dir 側には
            // 一切触れない。ハンドル基準の `open_verified_regular_file` で
            // 開くため、`root_fd` を検証済みの所有者比較の起点として使う
            // （symlink・ハードリンク・他ユーザー所有ファイルは拒否する。
            // security.md「プロファイル境界」）。
            let lock_display_path = root.join(LOCK_FILE_NAME);
            let lock_file = open_verified_regular_file(
                root_fd.as_fd(),
                root_fd.as_fd(),
                LOCK_FILE_NAME.as_ref(),
                lock_display_path.clone(),
            )?;
            let profile_lock = lock::try_acquire(lock_file, &lock_display_path)?;

            // 4 データ種別のハンドルを開く。`DataDirFds` の各フィールドに
            // 対応させるため、`DataKind::ALL` をループする代わりに 1 種別
            // ずつ明示する（[`DataDirFds`] のフィールド追加をコンパイラに
            // 強制させるため）。
            let open_data_dir = |kind: DataKind| -> Result<OwnedFd, ProfileError> {
                let path = root.join(kind.dir_name());
                let dir_fd = open_or_create_child_dir(&root_fd, kind.dir_name().as_ref(), &path)?;
                set_dir_permissions_0700(&dir_fd)?;
                Ok(dir_fd)
            };
            let data_fds = DataDirFds {
                cookies: open_data_dir(DataKind::Cookies)?,
                storage: open_data_dir(DataKind::Storage)?,
                cache: open_data_dir(DataKind::Cache)?,
                history: open_data_dir(DataKind::History)?,
            };

            Ok(Profile {
                root,
                root_fd,
                data_fds,
                _lock: profile_lock,
            })
        }
    }

    /// プロファイルルートのパスを返す（表示・ログ用）。
    ///
    /// このパスを再度 `open`/`symlink_metadata` 等で解決すると、`open` 時に
    /// 検証した実体と異なるものを指す可能性がある（PR #437 P1 レビュー指摘。
    /// モジュール doc 参照）。境界判定やファイル操作には unix 限定の
    /// [`Profile::root_fd`] を使う。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 指定したデータ種別のディレクトリパス（`root/<dir_name>`）を返す
    /// （表示・ログ用）。
    ///
    /// パスの生成のみを行い、存在確認はしない。このパスを再度解決する
    /// ファイル操作は行わないこと（[`Profile::root`] と同じ理由。PR #437 P1
    /// レビュー指摘）。境界判定やファイル操作には unix 限定の
    /// [`Profile::data_dir_fd`] を使う。
    pub fn data_dir(&self, kind: DataKind) -> PathBuf {
        self.root.join(kind.dir_name())
    }

    /// unix 限定: `open` が検証済みのプロファイルルートのディレクトリ
    /// ハンドルを返す。
    ///
    /// 後続タスク（Cookie・Storage 等の実データ操作）は、このハンドルを
    /// `openat`/`mkdirat`/`unlinkat` 系 API の `dirfd` として使うことで、
    /// [`Profile::root`] が返すパスを再解決せずに境界内へアクセスできる
    /// （PR #437 P1 レビュー指摘）。
    #[cfg(unix)]
    pub fn root_fd(&self) -> BorrowedFd<'_> {
        self.root_fd.as_fd()
    }

    /// unix 限定: `open` が検証済みの、指定データ種別ディレクトリのハンドルを
    /// 返す。
    ///
    /// [`Profile::root_fd`] と同様、後続タスクのファイル操作はこのハンドルを
    /// `openat` 系 API の `dirfd` として使う契約とする。
    #[cfg(unix)]
    pub fn data_dir_fd(&self, kind: DataKind) -> BorrowedFd<'_> {
        match kind {
            DataKind::Cookies => self.data_fds.cookies.as_fd(),
            DataKind::Storage => self.data_fds.storage.as_fd(),
            DataKind::Cache => self.data_fds.cache.as_fd(),
            DataKind::History => self.data_fds.history.as_fd(),
        }
    }

    /// unix 限定: 指定データ種別ディレクトリ直下に `name` という名前の
    /// ファイルを作成（既存なら開く）する最小のヘルパー。
    ///
    /// [`Profile::data_dir_fd`] が返すハンドルを起点に `openat` するため、
    /// `open` から本呼び出しまでの間にディレクトリが symlink へ差し替え
    /// られていてもリンク先へは作成しない（PR #437 P1 レビュー指摘）。
    /// `OFlags::NOFOLLOW` により、`name` の位置に symlink が存在する場合も
    /// リンク先を辿らず [`ProfileError::InvalidLayout`] で拒否する
    /// （`ELOOP`）。ディレクトリ（`EISDIR`）や、対象が非ディレクトリを
    /// 経由しようとした場合（`ENOTDIR`）も同様に拒否する（PR #437 再レビュー
    /// Cursor Low 指摘: `O_RDWR` でディレクトリを開くと `EISDIR` になり、
    /// 以前は `InvalidLayout` ではなく `Io` に分類されていた）。
    ///
    /// `name` は [`sanitize_component`] を通過した単一の要素であることを
    /// 要求する（`..`・`a/b` 等は [`ProfileError::InvalidComponent`] で拒否し、
    /// `dir_fd` を起点にした名前解決であってもパストラバーサルを許さない。
    /// security.md「プロファイル境界」）。組み立てた表示用パスはさらに
    /// [`assert_within_root`] でプロファイルルート配下に収まることを再確認
    /// する（コンポーネント単位の拒否に続く二重防御。PROF-4・#177）。
    ///
    /// `openat` には `OFlags::NONBLOCK` を付ける。`name` の位置に FIFO が
    /// あると、`NONBLOCK` なしでは相手側が読み書きのため open するまで
    /// `openat` 自体がブロックし得るため（PR #437 再レビュー Codex P1
    /// 指摘）。`NONBLOCK` を付けると FIFO の open 自体は（相手側の有無に
    /// 関わらず）即座に成功する（`ENXIO` にはならない）ため、成功した
    /// ハンドルを直後の `fstat` で検査する「成功 → 検査 → 拒否」の流れになる。
    /// 通常ファイルでなければ（FIFO・デバイス・ソケット等）そのハンドルを
    /// 閉じて [`ProfileError::InvalidLayout`] で拒否する。
    ///
    /// 通常ファイルであることを確認した後、以下も併せて検証し、いずれかに
    /// 該当すれば拒否する（PR #437 再レビュー Codex P0 指摘: 既存ファイルを
    /// 無条件に信用して書き込むとプロファイル分離が成立しない）。
    ///
    /// - 所有者 uid が [`Profile::root_fd`]（`open` で検証済みのプロファイル
    ///   ルート）の所有者と異なる（`geteuid` は rustix の `process` feature が
    ///   要り依存を増やすため使わず、検証済みルートの所有者との比較で代替
    ///   する）
    /// - ハードリンク数（`st_nlink`）が 1 でない（境界外の実体を指す
    ///   ハードリンク経由で、書き込みが別の場所へも及ぶことを防ぐ）
    ///
    /// 検証を通過した通常ファイルのみ、`fchmod` でパーミッションを `0o600`
    /// へ締め直す（既存ファイルが他ユーザーから読める権限のまま Cookie 等を
    /// 書き込む事故を防ぐ）。最後に `NONBLOCK` を解除する（通常ファイルの
    /// I/O には本来影響しないが、呼び出し元が `fcntl` でフラグを確認した
    /// 際に驚かせないよう、明示的に元へ戻す）。
    ///
    /// 戻り値の `File` は独立したハンドルであり、`Profile`（延いては
    /// `root_fd`/`data_dir_fd`）を drop した後も有効なまま使い続けられる
    /// （逆に、`Profile` を drop すると `root_fd`/`data_fds` のディレクトリ
    /// ハンドルは閉じるが、既に返した `File` には影響しない）。
    #[cfg(unix)]
    pub fn create_file_in(
        &self,
        kind: DataKind,
        name: &std::ffi::OsStr,
    ) -> Result<std::fs::File, ProfileError> {
        // 1 層目: コンポーネント単位の拒否（PROF-4）。
        let safe_name = sanitize_component(name)?;
        let display_path = self.data_dir(kind).join(safe_name);

        // 2 層目: 組み立てたパスがルート配下に収まるかを字句的に再確認する
        // （PROF-4。`display_path` はルート直下のデータディレクトリのさらに
        // 1 階層下のため通常は自明に真だが、`data_dir`/`join` の組み立てを
        // 変更した際の回帰を検出する境界検証として維持する）。
        assert_within_root(&self.root, &display_path)?;

        // 実体の検証・パーミッション締め直しは `open_verified_regular_file`
        // （#178 で切り出し。ロックファイルの作成と共有する）に委譲する。
        // 所有者比較の基準は常に検証済みのプロファイルルート（`root_fd`）。
        open_verified_regular_file(self.data_dir_fd(kind), self.root_fd(), name, display_path)
    }

    /// プロファイルを削除する。ロックを解放したうえで、ルートディレクトリごと
    /// 全データ（Cookie・ストレージ・キャッシュ・履歴）を消す（`PROF-5`、
    /// TASK-53（53.2）・#188、MS-3）。値を消費するため、削除後にハンドルを
    /// 使うことはできない。
    ///
    /// unix では検証済みの [`Profile::root_fd`] を起点に `openat`/`unlinkat` で
    /// 再帰削除し、パス文字列は再解決しない（[`Profile::root`] は表示専用）。
    /// ディレクトリは `NOFOLLOW` で開き、symlink はエントリとして unlink する
    /// だけでリンク先へは触れない。マウントポイントはまたがず（`st_dev` に加え
    /// Linux では `statx` の `mnt_id` の一致確認。同一 FS 上の bind mount は
    /// `st_dev` では検出できないため。`mnt_id` を取得できなければ再帰しない）、
    /// 深さは `MAX_DELETE_DEPTH` で制限する。
    ///
    /// 処理順序:
    ///
    /// 1. ルートのパスが `open` 時と同じ実体を指すか `(st_dev, st_ino)` で
    ///    確認する。不一致なら何も消さず [`ProfileError::InvalidLayout`]
    ///    （fail-closed）
    /// 2. `profile.lock` を保持したまま、ルートを親ディレクトリ内の一時名
    ///    （`.fandhe-deleting-<pid>-<nanos>`）へ `renameat` で退避する。以降
    ///    元のパスは空きになり、並行 `open` は別 inode の新規プロファイルを
    ///    作るだけで、削除中のツリーには到達できない
    /// 3. 退避したツリーの全エントリを（`profile.lock` を含めて）消す。ロックは
    ///    保持した fd を通じて最後まで維持し、`profile.lock` を unlink するのは
    ///    もう名前で到達できない退避ツリー内に限る（`lock.rs` が禁じる inode の
    ///    すり替わりは起きない）
    /// 4. 退避ルートを `rmdir` してからロックを解放する
    ///
    /// 途中で失敗した場合は退避を元の名前へ戻す（元の名前が空いている場合のみ）。
    /// 呼び出し元は `Profile::open` で開き直して `delete` を再試行できる。
    ///
    /// 残る競合: 手順 1 の確認から手順 2 の `renameat` までの間にパスが差し替え
    /// られ得るが、退避後に inode を再確認し、不一致なら戻して拒否する。
    ///
    /// Windows では `open` が常に失敗するため到達しないが、成功を装わず
    /// [`ProfileError::Unsupported`] を返す（`XOS-7`〜`XOS-10`、REPAIR-3）。
    pub fn delete(self) -> Result<(), ProfileError> {
        #[cfg(not(unix))]
        {
            Err(ProfileError::Unsupported {
                reason: "profile deletion is not supported on this platform",
            })
        }
        #[cfg(unix)]
        {
            let Profile {
                root,
                root_fd,
                data_fds,
                _lock: profile_lock,
            } = self;

            let Some(root_name) = root.file_name().map(|n| n.to_os_string()) else {
                return Err(ProfileError::InvalidLayout {
                    path: root,
                    reason: "refusing to delete a profile root without a parent directory",
                });
            };

            // 親ハンドルは root_fd の ".." から得る（"/" から辿り直さない）。
            let parent_fd = rustix::fs::openat(&root_fd, "..", OPEN_DIR_FLAGS, Mode::empty())
                .map_err(|err| classify_dir_open_error(&root, err))?;
            let root_stat =
                rustix::fs::fstat(&root_fd).map_err(|err| ProfileError::Io(err.into()))?;
            let named_stat = rustix::fs::statat(&parent_fd, &root_name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|err| ProfileError::Io(err.into()))?;
            if named_stat.st_dev != root_stat.st_dev || named_stat.st_ino != root_stat.st_ino {
                return Err(ProfileError::InvalidLayout {
                    path: root,
                    reason: "profile root path no longer refers to the opened profile directory",
                });
            }

            drop(data_fds);

            let tomb_name = std::ffi::OsString::from(format!(
                ".fandhe-deleting-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            match rustix::fs::statat(&parent_fd, &tomb_name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => {}
                Ok(_) => {
                    return Err(ProfileError::InvalidLayout {
                        path: root,
                        reason: "deletion staging name already exists",
                    });
                }
                Err(err) => return Err(ProfileError::Io(err.into())),
            }
            rustix::fs::renameat(&parent_fd, &root_name, &parent_fd, &tomb_name)
                .map_err(|err| ProfileError::Io(err.into()))?;

            let result = (|| {
                // 退避したものが検証済みの root_fd と同じ実体であることを確認する。
                let moved = rustix::fs::statat(&parent_fd, &tomb_name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|err| ProfileError::Io(err.into()))?;
                if moved.st_dev != root_stat.st_dev || moved.st_ino != root_stat.st_ino {
                    return Err(ProfileError::InvalidLayout {
                        path: root.clone(),
                        reason: "profile root was replaced during deletion",
                    });
                }
                let root_mount = mount_identity(root_fd.as_fd())?;
                remove_dir_contents_at(root_fd.as_fd(), root_mount, 0, &root, None)?;
                match rustix::fs::unlinkat(&parent_fd, &tomb_name, AtFlags::REMOVEDIR) {
                    Ok(()) => Ok(()),
                    Err(Errno::NOTEMPTY) => Err(ProfileError::InvalidLayout {
                        path: root.clone(),
                        reason: "profile root became non-empty during deletion",
                    }),
                    Err(err) => Err(ProfileError::Io(err.into())),
                }
            })();

            if result.is_err() {
                // 元の名前が空いている場合のみ戻す（既存エントリを上書きしない）。
                if matches!(
                    rustix::fs::statat(&parent_fd, &root_name, AtFlags::SYMLINK_NOFOLLOW),
                    Err(Errno::NOENT)
                ) {
                    let _ = rustix::fs::renameat(&parent_fd, &tomb_name, &parent_fd, &root_name);
                }
            }
            // ロックは最後まで保持する（ここで初めて解放）。
            drop(profile_lock);
            drop(root_fd);
            result
        }
    }
}

/// マウント境界の識別子。同一 FS 上の bind mount は `st_dev` が同じになるため、
/// Linux では `statx` の `mnt_id` を併用する（`PROF-5`）。
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct MountIdentity {
    dev: rustix::fs::Dev,
    /// Linux（カーネル 5.8 以降）のみ Some。取得できない場合は None。
    mnt_id: Option<u64>,
}

/// `fd` が属するマウントの識別子を返す。
#[cfg(unix)]
fn mount_identity(fd: BorrowedFd<'_>) -> Result<MountIdentity, ProfileError> {
    let st = rustix::fs::fstat(fd).map_err(|err| ProfileError::Io(err.into()))?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let mnt_id =
        match rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, rustix::fs::StatxFlags::MNT_ID) {
            Ok(sx) if sx.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0 => {
                Some(sx.stx_mnt_id)
            }
            _ => None,
        };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let mnt_id = None;
    Ok(MountIdentity {
        dev: st.st_dev,
        mnt_id,
    })
}

/// `child` がルートと同じマウントに属するか。Linux で `mnt_id` を取得できない
/// 場合は bind mount を識別できないため fail-closed で false とする。
#[cfg(unix)]
fn same_mount(root: MountIdentity, child: MountIdentity) -> bool {
    if root.dev != child.dev {
        return false;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        matches!((root.mnt_id, child.mnt_id), (Some(a), Some(b)) if a == b)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        true
    }
}

/// 再帰削除の深さ上限。これを超える入れ子はスタック・fd の消費を抑えるため
/// [`ProfileError::InvalidLayout`] で拒否する（`PROF-5`）。
#[cfg(unix)]
const MAX_DELETE_DEPTH: usize = 64;

/// ディレクトリ再走査の上限回数。readdir 中の unlink は POSIX で挙動が
/// 未規定のため、空になるまで読み直す（無限ループを避けるための上限）。
#[cfg(unix)]
const MAX_DELETE_PASSES: usize = 4;

/// `dir_fd` 直下のエントリを（`keep` を除いて）再帰的に削除する
/// （[`Profile::delete`] から呼ばれる。`PROF-5`）。
///
/// symlink・通常ファイル等は `unlinkat` でエントリとして消すだけで辿らない。
/// ディレクトリは `NOFOLLOW` で開き、`root_mount` と同じマウントであること
/// （bind mount を含むマウントポイントをまたがないこと）を確認してから再帰する。
#[cfg(unix)]
fn remove_dir_contents_at(
    dir_fd: BorrowedFd<'_>,
    root_mount: MountIdentity,
    depth: usize,
    display_path: &Path,
    keep: Option<&std::ffi::OsStr>,
) -> Result<(), ProfileError> {
    use std::os::unix::ffi::OsStrExt;

    if depth > MAX_DELETE_DEPTH {
        return Err(ProfileError::InvalidLayout {
            path: display_path.to_path_buf(),
            reason: "profile directory tree exceeds maximum deletion depth",
        });
    }
    for _ in 0..MAX_DELETE_PASSES {
        let dir = Dir::read_from(dir_fd).map_err(|err| ProfileError::Io(err.into()))?;
        let mut removed_any = false;
        for entry in dir {
            let entry = entry.map_err(|err| ProfileError::Io(err.into()))?;
            let name = entry.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if keep.is_some_and(|k| k.as_bytes() == bytes) {
                continue;
            }
            removed_any = true;
            let is_dir = match entry.file_type() {
                FileType::Directory => true,
                FileType::Unknown => {
                    let st = rustix::fs::statat(dir_fd, name, AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(|err| ProfileError::Io(err.into()))?;
                    FileType::from_raw_mode(st.st_mode) == FileType::Directory
                }
                _ => false,
            };
            if is_dir {
                let child_path = display_path.join(std::ffi::OsStr::from_bytes(bytes));
                let child_fd = rustix::fs::openat(dir_fd, name, OPEN_DIR_FLAGS, Mode::empty())
                    .map_err(|err| classify_dir_open_error(&child_path, err))?;
                if !same_mount(root_mount, mount_identity(child_fd.as_fd())?) {
                    return Err(ProfileError::InvalidLayout {
                        path: child_path,
                        reason: "refusing to cross a mount point during profile deletion",
                    });
                }
                remove_dir_contents_at(child_fd.as_fd(), root_mount, depth + 1, &child_path, None)?;
                drop(child_fd);
                match rustix::fs::unlinkat(dir_fd, name, AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(err) => return Err(ProfileError::Io(err.into())),
                }
            } else {
                match rustix::fs::unlinkat(dir_fd, name, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(err) => return Err(ProfileError::Io(err.into())),
                }
            }
        }
        if !removed_any {
            return Ok(());
        }
    }
    Err(ProfileError::InvalidLayout {
        path: display_path.to_path_buf(),
        reason: "profile directory kept changing during deletion",
    })
}

/// `dir_fd` 配下に `name` という名前の通常ファイルを作成（既存なら開く）し、
/// 実体を検証したうえでパーミッション `0o600` に締め直す。
///
/// [`Profile::create_file_in`]（データ種別ディレクトリ配下のファイル作成）と
/// [`Profile::open`]（`profile.lock` の作成。#178・TASK-50（50.3））が共有する
/// ハンドル基準の作成経路。`dir_fd` はこの関数の呼び出し元が既に検証済みの
/// ディレクトリハンドルであることを前提とし、`owner_fd` はそのファイルの
/// 所有者が一致すべき基準（通常は [`Profile::root_fd`]、または `profile.lock`
/// 自身のように `dir_fd` と同じ）を渡す。
///
/// `OFlags::NOFOLLOW` により、`name` の位置に symlink が存在する場合は
/// リンク先を辿らず [`ProfileError::InvalidLayout`] で拒否する（`ELOOP`）。
/// ディレクトリ（`EISDIR`）や、対象が非ディレクトリを経由しようとした場合
/// （`ENOTDIR`）も同様に拒否する（PR #437 再レビュー Cursor Low 指摘: `O_RDWR`
/// でディレクトリを開くと `EISDIR` になり、以前は `InvalidLayout` ではなく
/// `Io` に分類されていた）。
///
/// `openat` には `OFlags::NONBLOCK` を付ける。`name` の位置に FIFO があると、
/// `NONBLOCK` なしでは相手側が読み書きのため open するまで `openat` 自体が
/// ブロックし得るため（PR #437 再レビュー Codex P1 指摘）。`NONBLOCK` を
/// 付けると FIFO の open 自体は（相手側の有無に関わらず）即座に成功する
/// （`ENXIO` にはならない）ため、成功したハンドルを直後の `fstat` で検査する
/// 「成功 → 検査 → 拒否」の流れになる。通常ファイルでなければ（FIFO・デバイス・
/// ソケット等）そのハンドルを閉じて [`ProfileError::InvalidLayout`] で拒否する。
///
/// 通常ファイルであることを確認した後、以下も併せて検証し、いずれかに
/// 該当すれば拒否する（PR #437 再レビュー Codex P0 指摘: 既存ファイルを
/// 無条件に信用して書き込むとプロファイル分離が成立しない）。
///
/// - 所有者 uid が `owner_fd` の所有者と異なる（`geteuid` は rustix の
///   `process` feature が要り依存を増やすため使わず、検証済みハンドルの
///   所有者との比較で代替する）
/// - ハードリンク数（`st_nlink`）が 1 でない（境界外の実体を指すハードリンク
///   経由で、書き込みが別の場所へも及ぶことを防ぐ）
///
/// 検証を通過した通常ファイルのみ、`fchmod` でパーミッションを `0o600` へ
/// 締め直す（既存ファイルが他ユーザーから読める権限のまま Cookie・ロック
/// ファイル等を書き込む事故を防ぐ）。最後に `NONBLOCK` を解除する（通常
/// ファイルの I/O には本来影響しないが、呼び出し元が `fcntl` でフラグを
/// 確認した際に驚かせないよう、明示的に元へ戻す）。
///
/// 戻り値の `File` は独立したハンドルであり、`dir_fd`/`owner_fd` の元になった
/// 値（`Profile`）を drop した後も有効なまま使い続けられる。
#[cfg(unix)]
fn open_verified_regular_file(
    dir_fd: BorrowedFd<'_>,
    owner_fd: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    display_path: PathBuf,
) -> Result<std::fs::File, ProfileError> {
    // `NONBLOCK` は FIFO 越しの open ブロックを避けるためだけに付ける
    // （下記で実体確認後に解除する）。
    let flags =
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let create_mode = Mode::RUSR | Mode::WUSR;
    let owned = rustix::fs::openat(dir_fd, name, flags, create_mode)
        .map_err(|err| classify_file_open_error(&display_path, err))?;

    // 実体を確認する。ここで拒否する場合、`owned` はこのスコープを
    // 抜ける際に drop され、FIFO・デバイス等を握ったまま放置しない。
    let file_stat = rustix::fs::fstat(&owned).map_err(|err| ProfileError::Io(err.into()))?;
    if !rustix::fs::FileType::from_raw_mode(file_stat.st_mode).is_file() {
        return Err(ProfileError::InvalidLayout {
            path: display_path,
            reason: "path exists but is not a regular file",
        });
    }

    // 所有者を検証済みの基準ハンドルと比較する（`geteuid` の代わり。
    // 上記 doc 参照）。
    let owner_stat = rustix::fs::fstat(owner_fd).map_err(|err| ProfileError::Io(err.into()))?;
    if file_stat.st_uid != owner_stat.st_uid {
        return Err(ProfileError::InvalidLayout {
            path: display_path,
            reason: "file owner does not match the profile root owner",
        });
    }

    // ハードリンク経由で境界外の実体へ書き込ませる攻撃を防ぐ。
    if file_stat.st_nlink != 1 {
        return Err(ProfileError::InvalidLayout {
            path: display_path,
            reason: "file has multiple hard links, refusing to write through a shared inode",
        });
    }

    // ここまでの検証を通過した通常ファイルのみパーミッションを締める。
    rustix::fs::fchmod(&owned, create_mode).map_err(|err| ProfileError::Io(err.into()))?;

    // `NONBLOCK` を解除する（通常ファイルの I/O には影響しないが、
    // フラグを元の意図どおりに戻しておく）。
    let current_flags =
        rustix::fs::fcntl_getfl(&owned).map_err(|err| ProfileError::Io(err.into()))?;
    rustix::fs::fcntl_setfl(&owned, current_flags.difference(OFlags::NONBLOCK))
        .map_err(|err| ProfileError::Io(err.into()))?;

    Ok(std::fs::File::from(owned))
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

/// [`Profile::create_file_in`] 用の `Errno` 分類。`classify_dir_open_error`
/// と異なり `Errno::ISDIR` も扱う。`create_file_in` は `OFlags::RDWR` で
/// 開くため、対象がディレクトリだと `ENOTDIR` ではなく `EISDIR` が返る
/// （PR #437 再レビュー Cursor Low 指摘: 以前は `EISDIR` を分類しておらず
/// `ProfileError::Io` になっていた）。
#[cfg(unix)]
fn classify_file_open_error(path: &Path, err: Errno) -> ProfileError {
    match err {
        Errno::LOOP => ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "file name is a symlink, refusing to follow",
        },
        Errno::NOTDIR => ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "a path component is not a directory",
        },
        Errno::ISDIR => ProfileError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "path exists but is a directory",
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
    /// `0o700` になる（最小限の確認。網羅的な検証は
    /// `tests/profile_open.rs` の受入基準 A のテスト群を参照）。
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

    /// PROF-1（Unix 固有）: `Profile` は `Send`・`Sync`（`OwnedFd` フィールドが
    /// 両方を満たすことの回帰テスト。core の `core_1_error_is_send_sync_static`
    /// と同型。PR #437 P1 レビュー指摘: ハンドルを追加してもこの性質を壊さない
    /// ことを機械的に確認する）。
    #[test]
    fn prof_1_profile_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Profile>();
    }

    /// PROF-1（Unix 固有）: `open` 後にルートを別の実体（symlink）へ差し替えても、
    /// `create_file_in` はハンドル基準で判定した実ディレクトリ側へファイルを
    /// 作成する。`data_dir` が返すパス（パス再解決の起点）を辿ると symlink 先へ
    /// 書き込んでしまうところ、`root_fd`/`data_dir_fd` を起点にした
    /// `create_file_in` はそれを回避することを確認する（PR #437 P1 レビュー
    /// 指摘の本丸）。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_uses_handle_not_re_resolved_path_after_root_is_replaced() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new();
        let profile_path = tmp.path().join("profile");
        let profile = Profile::open(&profile_path).expect("open は成功する");

        // `open` が返したハンドルは維持したまま、ルートパスを real_root へ
        // 退避し、元の位置には外部ディレクトリへの symlink を置く。
        let real_root = tmp.path().join("real-root");
        std::fs::rename(&profile_path, &real_root).expect("ルートの退避");
        let external = TempDir::new();
        std::fs::create_dir_all(external.path()).expect("外部ディレクトリの作成");
        symlink(external.path(), &profile_path).expect("symlink 作成");

        let file = profile
            .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("session"))
            .expect("ハンドル基準で作成できる");
        drop(file);

        // ハンドル基準のため、ファイルは退避前の実ディレクトリ（real_root）側に
        // 作られる。パスを再解決した場合に書き込まれるはずの symlink 先
        // （external）には何も作られない。
        assert!(
            real_root
                .join(DataKind::Cookies.dir_name())
                .join("session")
                .is_file(),
            "検証済みの実ディレクトリ側にファイルが作られていない"
        );
        assert!(
            !external
                .path()
                .join(DataKind::Cookies.dir_name())
                .join("session")
                .exists(),
            "symlink 差し替え後の再解決先にファイルが作られてはならない"
        );
        // `data_dir` はパス再解決用であり、差し替え後は symlink 先を指す
        // （表示用 API であることの裏付け。ファイル操作には使わない契約）。
        assert!(
            !profile
                .data_dir(DataKind::Cookies)
                .join("session")
                .is_file(),
            "data_dir が返すパスを再解決した場所にはファイルが存在しないはず"
        );
    }

    /// PROF-1（Unix 固有）: `root/cookies` 配下のファイル名位置に dangling
    /// symlink があると、`create_file_in` は `InvalidLayout` で拒否し、
    /// リンク先には何も作られない（`OFlags::NOFOLLOW` の直接確認）。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_fails_on_dangling_symlink_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        let external = TempDir::new();
        let dangling_target = external.path().join("never-created");
        let file_path = tmp
            .path()
            .join(DataKind::Cookies.dir_name())
            .join("session");
        symlink(&dangling_target, &file_path).expect("symlink 作成");

        let err = profile
            .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("session"))
            .expect_err("symlink なら失敗する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));
        assert!(
            !dangling_target.exists(),
            "symlink のリンク先にファイルが作成されてはならない"
        );
    }

    /// PROF-1（Unix 固有）: `create_file_in` はディレクトリ区切り文字や `..`
    /// を含む名前を単一コンポーネントとして拒否し、パストラバーサルを許さない
    /// （security.md「プロファイル境界」。`dir_fd` を起点にした `openat` でも
    /// `name` 自体に `/`・`..` が含まれれば境界外へ解決されてしまうため）。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_rejects_multi_component_names() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        for name in ["../escape", "a/b", ".."] {
            let err = profile
                .create_file_in(DataKind::Cookies, std::ffi::OsStr::new(name))
                .expect_err("複数コンポーネント・'..' は拒否される");
            assert!(
                matches!(err, ProfileError::InvalidComponent { .. }),
                "{name:?} が拒否されなかった"
            );
        }

        // 境界外（プロファイルルートの親）に何も作られていないこと。
        assert!(!tmp.path().join("escape").exists());
    }

    /// PROF-1（Unix 固有）: 既存ファイルが `0o644`（他ユーザーから読める
    /// 権限）で存在していても、`create_file_in` はハードリンク・所有者の
    /// 検証を通過した通常ファイルであれば `0o600` へ締め直す（PR #437
    /// 再レビュー Codex P0 指摘: 既存ファイルの権限を無条件に信用しない）。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_rechmods_preexisting_file_to_0600() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        let file_path = tmp
            .path()
            .join(DataKind::Cookies.dir_name())
            .join("session");
        std::fs::write(&file_path, b"existing").expect("既存ファイルの作成");
        std::fs::set_permissions(&file_path, Permissions::from_mode(0o644))
            .expect("既存ファイルのパーミッション設定");

        let file = profile
            .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("session"))
            .expect("既存の通常ファイルは開ける");
        let mode = file.metadata().expect("metadata 取得").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "既存ファイルのパーミッションが締め直されない");
    }

    /// PROF-1（Unix 固有）: `name` の位置に FIFO があると `create_file_in` は
    /// ブロックせずに `InvalidLayout` で拒否する（PR #437 再レビュー Codex P1
    /// 指摘: `OFlags::NONBLOCK` を付けない場合、相手側が open するまで
    /// `openat` 自体がブロックし得る）。`mkfifo` コマンドを使うのは
    /// `rustix::fs::mkfifoat` が macOS では利用できない（`cfg(not(apple))`）
    /// ため、3 OS のテストで共通に使えるようにするため。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_fails_on_fifo_without_blocking() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        let fifo_path = tmp
            .path()
            .join(DataKind::Cookies.dir_name())
            .join("session");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo コマンドの起動");
        assert!(status.success(), "mkfifo コマンドが失敗した");

        // `NONBLOCK` が効いていれば、対向の reader/writer がいなくても
        // ここで即座に返る（ブロックしない）。
        let err = profile
            .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("session"))
            .expect_err("FIFO は拒否される");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));
    }

    /// PROF-1（Unix 固有）: `name` の位置がディレクトリだと `create_file_in`
    /// は `InvalidLayout` で拒否する（PR #437 再レビュー Cursor Low 指摘:
    /// `O_RDWR` でディレクトリを開くと `EISDIR` になり、以前は
    /// `ProfileError::Io` に分類されて `InvalidLayout` にならなかった）。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_fails_when_name_is_a_directory() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        let dir_path = tmp
            .path()
            .join(DataKind::Cookies.dir_name())
            .join("session");
        std::fs::create_dir(&dir_path).expect("ディレクトリの作成");

        let err = profile
            .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("session"))
            .expect_err("ディレクトリは拒否される");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));
    }

    /// PROF-1（Unix 固有）: `name` の位置がハードリンク（`st_nlink != 1`）だと
    /// `create_file_in` は `InvalidLayout` で拒否する（PR #437 再レビュー
    /// Codex P0 指摘: ハードリンク経由で境界外の実体へ書き込ませる攻撃を
    /// 防ぐ）。
    #[cfg(unix)]
    #[test]
    fn prof_1_create_file_in_fails_on_hard_linked_file() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        // リンク元は別のデータ種別ディレクトリ（Storage）配下に置く。
        // `tmp.path()` 直下（root）は `open` が `0o700` に締めるため、祖先の
        // search 権限や `fs.protected_hardlinks` の挙動に左右されずに
        // 同一ファイルシステム内でハードリンクを作れる。かつ「Cookies 側の
        // ハードリンク経由で Storage 側の実体を書き換えられてはならない」
        // という境界検証の意図とも一致する。
        let source_path = tmp
            .path()
            .join(DataKind::Storage.dir_name())
            .join("hardlink-source");
        std::fs::write(&source_path, b"shared").expect("リンク元ファイルの作成");
        let link_path = tmp
            .path()
            .join(DataKind::Cookies.dir_name())
            .join("session");
        std::fs::hard_link(&source_path, &link_path).expect("ハードリンクの作成");

        let err = profile
            .create_file_in(DataKind::Cookies, std::ffi::OsStr::new("session"))
            .expect_err("ハードリンクは拒否される");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));

        // リンク元の内容が変更されていないこと（誤って書き込まれていない）。
        let content = std::fs::read(&source_path).expect("リンク元の読み取り");
        assert_eq!(content, b"shared");
    }

    /// PROF-1（Unix 固有）: 相対パスで `open` しても、`root()` は絶対パスを
    /// 返す（作業ディレクトリが後から変わっても `root`/`data_dir` の意味が
    /// 変わらないようにするため。PR #437 P1 レビュー指摘）。
    ///
    /// `std::env::set_current_dir` はプロセス全体に効き、他の（並行実行される）
    /// テストと干渉して flaky になり得るため使わない。代わりに現在の作業
    /// ディレクトリ（cargo が crate ルートに設定する、書き込み可能な既知の
    /// ディレクトリ）配下に一意名の相対パスを渡し、`std::path::absolute` が
    /// それを `current_dir()` 基準で絶対化した結果と一致することを確認する。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_with_relative_path_stores_absolute_root() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("fandhe-profile-relative-test-{}-{n}", std::process::id());
        struct RemoveOnDrop(PathBuf);
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let cwd = std::env::current_dir().expect("current_dir の取得");
        let expected_root = cwd.join(&name);
        let _cleanup = RemoveOnDrop(expected_root.clone());

        let profile = Profile::open(&name).expect("相対パスでも open できる");

        assert!(profile.root().is_absolute(), "root が絶対パスでない");
        assert_eq!(profile.root(), expected_root.as_path());
    }

    /// PROF-1（Unix 固有）: 既に開いている `Profile` があるルートへ再度
    /// `open` すると `ProfileError::Locked { path }` になり、`path` は
    /// `root/profile.lock` と一致する（TASK-50（50.3）・#178 の受入基準 1）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_twice_same_root_fails_with_locked() {
        let tmp = TempDir::new();
        let root = tmp.path().to_path_buf();

        let first = Profile::open(&root).expect("1 回目の open は成功する");
        let err = Profile::open(&root).expect_err("2 回目の open は失敗する");
        assert!(matches!(
            err,
            ProfileError::Locked { path } if path == root.join(lock::LOCK_FILE_NAME)
        ));

        drop(first);
    }

    /// PROF-1（Unix 固有）: `open` が作成する `profile.lock` は通常ファイルで
    /// モードが `0o600` である。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_creates_lock_file_with_mode_0600() {
        let tmp = TempDir::new();
        let profile = Profile::open(tmp.path()).expect("open は成功する");

        let lock_path = tmp.path().join(lock::LOCK_FILE_NAME);
        let metadata = std::fs::metadata(&lock_path).expect("lock ファイルの metadata 取得");
        assert!(metadata.is_file(), "profile.lock が通常ファイルでない");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        drop(profile);
    }

    /// PROF-1（Unix 固有）: `profile.lock` の位置が symlink だと `open` は
    /// `InvalidLayout` で拒否し、symlink 先には何も作成・変更しない
    /// （[`open_verified_regular_file`] の `OFlags::NOFOLLOW` の直接確認）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_lock_file_is_a_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");

        let external = TempDir::new();
        let dangling_target = external.path().join("never-created");
        let lock_path = tmp.path().join(lock::LOCK_FILE_NAME);
        symlink(&dangling_target, &lock_path).expect("symlink 作成");

        let err = Profile::open(tmp.path()).expect_err("lock ファイルが symlink なら失敗する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));
        assert!(
            !dangling_target.exists(),
            "symlink のリンク先にファイルが作成されてはならない"
        );

        // ロック取得より前に拒否されるため、データ種別ディレクトリは
        // 作られていない（`Profile::open` の手順どおり: ロック取得 → data dir
        // 作成の順であることの確認）。
        for kind in DataKind::ALL.iter().copied() {
            assert!(
                !tmp.path().join(kind.dir_name()).exists(),
                "{:?} 用ディレクトリが作られてはならない",
                kind
            );
        }
    }

    /// PROF-1（Unix 固有）: `profile.lock` の位置がディレクトリだと `open` は
    /// `InvalidLayout` で拒否する（`create_file_in` の
    /// `prof_1_create_file_in_fails_when_name_is_a_directory` と同型）。
    #[cfg(unix)]
    #[test]
    fn prof_1_open_fails_when_lock_file_is_a_directory() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path()).expect("ルート作成");
        std::fs::create_dir(tmp.path().join(lock::LOCK_FILE_NAME)).expect("ディレクトリの作成");

        let err = Profile::open(tmp.path()).expect_err("lock がディレクトリなら失敗する");
        assert!(matches!(err, ProfileError::InvalidLayout { .. }));
    }

    /// PROF-1（Unix 固有）: 2 回目の `open` がロック取得に失敗した場合、
    /// 既存のデータ種別ディレクトリを再作成・変更しない（`Profile::open` の
    /// 手順「ロック取得 → data dir 作成」の順序を、既存ディレクトリを一つ
    /// 消しておくことで確認する）。
    #[cfg(unix)]
    #[test]
    fn prof_1_second_open_does_not_create_data_dirs() {
        let tmp = TempDir::new();
        let root = tmp.path().to_path_buf();

        let first = Profile::open(&root).expect("1 回目の open は成功する");
        let cache_path = root.join(DataKind::Cache.dir_name());
        std::fs::remove_dir_all(&cache_path).expect("cache ディレクトリの削除");

        let err = Profile::open(&root).expect_err("2 回目の open は失敗する");
        assert!(matches!(err, ProfileError::Locked { .. }));
        assert!(
            !cache_path.exists(),
            "ロック取得に失敗した場合は data dir を再作成してはならない"
        );

        drop(first);
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

    /// PROF-4: PoC-7（`docs/spec/03-poc/profile-isolation/profile-proto`）が
    /// 使ったパストラバーサル入力 9 種を `sanitize_component` がすべて拒否する。
    #[test]
    fn prof_4_sanitize_component_rejects_poc7_traversal_inputs() {
        for input in [
            "../../../etc",
            "..",
            "a/../../b",
            "/etc/passwd",
            "sub/dir",
            "../escape",
            "a/b",
            "..",
            "",
        ] {
            let err = sanitize_component(std::ffi::OsStr::new(input))
                .expect_err(&format!("{input:?} は拒否される"));
            assert!(
                matches!(err, ProfileError::InvalidComponent { .. }),
                "{input:?} が InvalidComponent で拒否されなかった"
            );
        }
    }

    /// PROF-4: PoC-7 の入力に加え、`sanitize_component` が追加で拒否すべき
    /// 入力（`.`・`\`・NUL・`..` の部分文字列・256 バイト超過）を確認する。
    #[test]
    fn prof_4_sanitize_component_rejects_extra_unsafe_inputs() {
        let too_long = "a".repeat(256);
        let with_backslash = "a\\b";
        let with_nul = "a\0b";
        let embedded_dotdot = "a..b";

        for input in [".", with_backslash, with_nul, embedded_dotdot, &too_long] {
            let err = sanitize_component(std::ffi::OsStr::new(input))
                .expect_err(&format!("{input:?} は拒否される"));
            assert!(
                matches!(err, ProfileError::InvalidComponent { .. }),
                "{input:?} が InvalidComponent で拒否されなかった"
            );
        }
    }

    /// PROF-4: 通常の名前（255 バイト丁度を含む）は `sanitize_component` を
    /// 通過し、`as_os_str()` が入力と一致する。
    #[test]
    fn prof_4_sanitize_component_accepts_plain_names() {
        let exactly_255 = "a".repeat(255);
        for input in ["example.com", "kv.json", "cookie_1", exactly_255.as_str()] {
            let safe = sanitize_component(std::ffi::OsStr::new(input))
                .unwrap_or_else(|_| panic!("{input:?} は許可される"));
            assert_eq!(safe.as_os_str(), std::ffi::OsStr::new(input));
        }
    }

    /// PROF-4: `assert_within_root` は字句判定のみを行い、ルート外を指す
    /// パス（`..` を含む組み立て・兄弟プレフィクス・ルート自身）を `Err` で
    /// 拒否する。ファイルシステムには一切触れないため、`root` は実在しない
    /// パスで組み立てる。
    #[test]
    fn prof_4_assert_within_root_rejects_outside_paths() {
        let root = std::env::temp_dir().join("fandhe-prof4-root");
        let sibling_root = std::env::temp_dir().join("fandhe-prof4-root2");

        let cases: Vec<PathBuf> = vec![
            PathBuf::from("/etc/passwd"),
            root.join("..").join("x"),
            root.join("a").join("..").join("..").join("b"),
            sibling_root.join("x"),
            root.clone(),
        ];

        for candidate in cases {
            let err = assert_within_root(&root, &candidate)
                .expect_err(&format!("{candidate:?} はルート外と判定される"));
            assert!(
                matches!(err, ProfileError::OutsideRoot { .. }),
                "{candidate:?} が OutsideRoot で拒否されなかった"
            );
        }
    }

    /// PROF-4: ルート配下の通常パスは `assert_within_root` を通過する。
    #[test]
    fn prof_4_assert_within_root_accepts_paths_under_root() {
        let root = std::env::temp_dir().join("fandhe-prof4-root");

        for candidate in [
            root.join("cookies").join("a.json"),
            root.join("cache").join("x"),
        ] {
            assert_within_root(&root, &candidate)
                .unwrap_or_else(|_| panic!("{candidate:?} はルート配下として許可される"));
        }
    }

    /// PROF-4（Unix 固有）: `create_file_in` は PoC-7 の 9 入力（空文字列含む）
    /// をすべて拒否し、プロファイルルート外に何も作らない（PoC-7 と同じ
    /// 「境界外への書き込み 0 件」の具体値 assert）。
    #[cfg(unix)]
    #[test]
    fn prof_4_create_file_in_rejects_all_traversal_inputs_and_writes_nothing_outside() {
        let tmp = TempDir::new();
        let profile_path = tmp.path().join("profile");
        let profile = Profile::open(&profile_path).expect("open は成功する");

        for name in [
            "../../../etc",
            "..",
            "a/../../b",
            "/etc/passwd",
            "sub/dir",
            "../escape",
            "a/b",
            "",
        ] {
            for kind in [DataKind::Cookies, DataKind::Storage] {
                let err = profile
                    .create_file_in(kind, std::ffi::OsStr::new(name))
                    .expect_err(&format!("{name:?} は {kind:?} でも拒否される"));
                assert!(
                    matches!(err, ProfileError::InvalidComponent { .. }),
                    "{name:?} / {kind:?} が InvalidComponent で拒否されなかった"
                );
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("一時ディレクトリの読み取り")
            .collect::<Result<Vec<_>, _>>()
            .expect("read_dir エントリの取得");
        assert_eq!(
            entries.len(),
            1,
            "プロファイルルート（profile）以外にエントリが作られてはならない: {entries:?}"
        );
    }

    /// `PROF-5`: 深さ上限（`MAX_DELETE_DEPTH`）を超える入れ子は何も装わず拒否する。
    #[cfg(unix)]
    #[test]
    fn prof_5_delete_fails_when_tree_exceeds_max_depth() {
        let tmp = TempDir::new();
        let root = tmp.path().join("profile");
        let profile = Profile::open(&root).expect("open");
        // cache 直下から MAX_DELETE_DEPTH + 1 段の入れ子を作る（root が深さ 0、
        // cache が 1 段目のため、上限超過となる）。
        let mut deep = root.join("cache");
        for i in 0..=MAX_DELETE_DEPTH {
            deep.push(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).expect("deep dirs");

        let err = profile.delete().expect_err("must refuse");
        assert!(
            matches!(
                err,
                ProfileError::InvalidLayout {
                    reason: "profile directory tree exceeds maximum deletion depth",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// `PROF-5`: 削除失敗時は退避を元の名前へ戻し、親ディレクトリに退避名を
    /// 残さない。戻った後は `open` し直して再試行できる。
    #[cfg(unix)]
    #[test]
    fn prof_5_delete_failure_restores_root_name() {
        let tmp = TempDir::new();
        let root = tmp.path().join("profile");
        let profile = Profile::open(&root).expect("open");
        let mut deep = root.join("cache");
        for i in 0..=MAX_DELETE_DEPTH {
            deep.push(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).expect("deep dirs");

        profile.delete().expect_err("must refuse");

        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("profile")]);
        Profile::open(&root).expect("reopen after failed delete");
    }
}
