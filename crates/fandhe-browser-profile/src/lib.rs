//! プロファイル（ユーザーデータディレクトリ）の分離を担う crate。
//!
//! `fandhe-browser-cli`（TASK-41 で追加予定）が `Profile::open(root)` を呼んで
//! プロファイルを開き、そこから構築した `AppState` を `fandhe-browser-cdp` /
//! `fandhe-browser-ai` 側のサーバーへ渡す設計を目指す。プロファイルは
//! データディレクトリ分離＋advisory lock＋パーミッション 700 によって、
//! 別プロファイル・別プロセスとのデータ混線を防ぐ（ビヘイビア `PROF-1`・`PROF-6`、
//! TASK-50、MS-3「基盤層・JS エンジン・プロファイル・OS 差異吸収層」）。
//!
//! # スタブについて
//!
//! 本ファイルは crate の雛形（TASK-1（1.7）・ビヘイビア `REPAIR-1`）であり、
//! 以下はいずれも未実装。実装済みを装う公開 API・ダミー実装は置かない
//! （`code-comment-style.md`・REPAIR-3）。
//!
//! - `Profile::open(root)` 本体（TASK-50）
//! - パストラバーサル防止・ルート配下検証（`PROF-4`、TASK-53）
//! - プロファイル削除処理（`PROF-5`、TASK-53）
//! - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
//! - クロスプラットフォーム advisory lock（`PROF-1`、TASK-54）
