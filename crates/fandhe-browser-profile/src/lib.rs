//! プロファイル（ユーザーデータディレクトリ）の分離を担う crate。
//!
//! `fandhe-browser-cli`（TASK-41 で追加予定）が `Profile::open(root)` を呼んで
//! プロファイルを開き、そこから構築した `AppState` を `fandhe-browser-cdp` /
//! `fandhe-browser-ai` 側のサーバーへ渡す設計を目指す。プロファイルは
//! データディレクトリ分離＋advisory lock＋パーミッション 700 によって、
//! 別プロファイル・別プロセスとのデータ混線を防ぐ（ビヘイビア `PROF-1`・`PROF-6`、
//! TASK-50、MS-3「基盤層・JS エンジン・プロファイル・OS 差異吸収層」）。
//!
//! ## スタブについて（`code-comment-style.md`・REPAIR-3）
//!
//! `Profile::open(root)` はルートディレクトリと 4 つのデータ種別ディレクトリの
//! 作成・Unix でのパーミッション 700 設定までを実装済み（TASK-50（50.1）・#176）。
//! パストラバーサル防止（`sanitize_component`・`assert_within_root`。`PROF-4`、
//! TASK-50（50.2）・#177）も実装済みで、コンポーネント単位の拒否（字句検査）
//! と組み立てたパスの字句判定の二重防御に加え、Unix ではハンドル基準の
//! `openat` + `NOFOLLOW` が symlink への書き込みを防ぐ（`Profile::create_file_in`
//! 参照）。`profile.lock`（`std::fs::File::try_lock`）による二重 open の拒否も
//! 実装済み（`PROF-1`、TASK-50（50.3）・#178。std のロックが 3 OS の advisory
//! lock を既にカバーするため、TASK-54 が想定していた `fs2`/`fs4`/`fd-lock`
//! への置換は不要であり、#175 の決定により `fs2` は使わない）。3 OS の CI 上で
//! 別プロセスからのロック競合が実際に検出されることは、本結合テストが
//! 3 OS matrix（`.github/workflows/ci.yml`）で全 crate 対象に実行される
//! ことで担保する
//! （`tests/lock_cross_platform.rs`、`PROF-1`、TASK-54（54.3）・#193。std
//! レベルの確認は 3 OS 共通、`Profile::open` 経由の確認は Unix 限定。Windows
//! の `Profile::open` は `%LOCALAPPDATA%` 配下のルートに限り成功するが、別プロセスからの
//! 競合確認は未追加）。パーミッション（ルート・4 サブディレクトリの `0o700`）・データ隔離
//! （各データ種別の書き込みが対応するサブディレクトリ配下にだけ置かれること）
//! の網羅的な確認も実装済み（TASK-50（50.4）・#179、
//! `crates/fandhe-browser-profile/tests/profile_open.rs`）。並行アクセス時の
//! データ分離は新規コードではなくデータディレクトリ分離（`PROF-6`）自体で
//! 成立し、`PROF-2`（TASK-51・#182）は `tests/isolation.rs`、`PROF-3`
//! （TASK-52・#184・#185）は `tests/concurrent_isolation.rs` が Unix 上で
//! 確認する（Windows 版の追加は別 Issue の課題）。プロファイル削除
//! （`Profile::delete`。`PROF-5`、TASK-53（53.2）・#188）も実装済み。以下はいずれも
//! 未実装であり、実装済みを装う公開 API・ダミー実装は置かない。
//!
//! - `ProfileStore::delete`（`PROF-5`）から `Profile::delete` への接続。
//!   `store.rs` の `ProfileStore`（`XOS-7`・TASK-60（60.2）・#201）と OS 慣習の
//!   既定パス解決 `OsDefaultStore`（TASK-60（60.3）・#202）は実装済み。明示指定
//!   による上書き `ExplicitStore`・`OverridableStore`（`PROF-1`、TASK-60（60.4）・
//!   #203）も実装済み。`Profile::delete` は `self` を消費するが
//!   `ProfileStore::delete` は `&Profile` を取るため、トレイト契約の見直しが
//!   要る（既定実装は `Unsupported` を返す）
//! - 名前正規化（`XOS-9`、TASK-62（62.1）・#208）は `normalize.rs` に ASCII 限定の
//!   正規化と大文字小文字衝突検出を実装済み。非 ASCII の明示的エンコードと、
//!   `Profile::create_file_in`・`ProfileStore` への組み込みは未実装
//! - Windows 長パス（`XOS-8`、TASK-61（61.1）・#205）は `store::to_long_path` として実装済み。
//!   `Profile::open`（Windows）は verbatim ルートも `canonicalize` して判定する
//! - Windows のアクセス制限（`XOS-7`〜`XOS-10`）は第 1 段階として実装済み（Issue #928）。
//!   プロファイルルートが `%LOCALAPPDATA%` 配下のときだけ `Profile::open` が成功し、
//!   新規に作るルートには作成時に保護付き DACL（SYSTEM・Administrators・本人のみ。
//!   Administrators と SYSTEM は読める）を付けて読み戻し検証する（Issue #952）。既存ルートの
//!   所有者・DACL の検証は未実装（#953）。配下外のルートは
//!   `ProfileError::Unsupported` で拒否してディレクトリを作らない。ACL を
//!   明示設定して任意のパスを許可する第 2 段階は未実装（後続。未判断）。Windows の `Profile` はルートのディレクトリ
//!   ハンドルと `profile.lock` を改名・削除を共有しない形で保持する。ルートまでの各要素・
//!   `profile.lock`・データ種別ディレクトリの作成と open は、親のハンドルを起点にした
//!   NT API（`win32` モジュール。`unsafe` はこのモジュールに限る。Issue #949・#950）で行い、
//!   作成の途中の再解析ポイント化でも配下外に作らない（残る限界は `local_root.rs` の「限界」。
//!   実機の FSCTL では未検証）。Windows では
//!   ハンドル基準の走査・
//!   `Profile::create_file_in`・`Profile::delete` も未実装

pub mod normalize;
pub mod profile;
pub mod store;

// Windows の実装と、他 OS では単体テストだけが使う（未使用警告をモジュール単位の
// `allow` で消さず、コンパイル対象を cfg で絞る）。
#[cfg(any(windows, test))]
mod local_root;
// Windows 限定。Win32/NT 呼び出しと `unsafe` を置く唯一のモジュール（Issue #949・#947 の承認）。
#[cfg(windows)]
mod win32;
// unix・Windows の `Profile::open` と単体テストが使う。それ以外の OS では未使用。
#[cfg(any(unix, windows, test))]
mod lock;

pub use normalize::{MAX_REGISTRY_ENTRIES, NameRegistry, NormalizedName, normalize_name};
pub use profile::{
    DataKind, Profile, ProfileError, SafeComponent, assert_within_root, sanitize_component,
};
pub use store::{
    APP_DIR_NAME, ExplicitStore, LongPath, LongPathKind, OsDefaultStore, OverridableStore,
    ProfileStore, ResolvedRoot, RootSource, to_long_path,
};
