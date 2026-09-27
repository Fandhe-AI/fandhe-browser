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
//! で `Profile::open` 経由の確認をするには下記の `XOS-7`〜`XOS-10` の実装が
//! 前提）。パーミッション（ルート・4 サブディレクトリの `0o700`）・データ隔離
//! （各データ種別の書き込みが対応するサブディレクトリ配下にだけ置かれること）
//! の網羅的な確認も実装済み（TASK-50（50.4）・#179、
//! `crates/fandhe-browser-profile/tests/profile_open.rs`）。以下はいずれも
//! 未実装であり、実装済みを装う公開 API・ダミー実装は置かない。
//!
//! - プロファイル削除処理（`PROF-5`、TASK-53）
//! - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
//! - Windows での ACL によるアクセス制限（`XOS-7`〜`XOS-10`。Unix の
//!   パーミッション 0o700 相当の隔離を Windows でも実現する。TASK-50・#176
//!   では意図的にスコープ外とした。新規依存が必要になるため、導入時は
//!   dependency-policy.md に従いユーザー承認を経る）

pub mod profile;

mod lock;

pub use profile::{
    DataKind, Profile, ProfileError, SafeComponent, assert_within_root, sanitize_component,
};
