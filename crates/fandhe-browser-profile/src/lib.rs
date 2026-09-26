//! プロファイル（ユーザーデータディレクトリ）の分離を担う crate。
//!
//! `fandhe-browser-cli`（TASK-41 で追加予定）が `Profile::open(root)` を呼んで
//! プロファイルを開き、そこから構築した `AppState` を `fandhe-browser-cdp` /
//! `fandhe-browser-ai` 側のサーバーへ渡す設計を目指す。プロファイルは
//! データディレクトリ分離＋advisory lock＋パーミッション 700 によって、
//! 別プロファイル・別プロセスとのデータ混線を防ぐ（ビヘイビア `PROF-1`・`PROF-6`、
//! TASK-50、MS-3「基盤層・JS エンジン・プロファイル・OS 差異吸収層」）。

pub mod profile;

pub use profile::{DataKind, Profile, ProfileError};

// スタブについて（`code-comment-style.md`・REPAIR-3）:
//
// `Profile::open(root)` はルートディレクトリと 4 つのデータ種別ディレクトリの
// 作成・Unix でのパーミッション 700 設定までを実装済み（TASK-50（50.1）・#176）。
// 以下はいずれも未実装であり、実装済みを装う公開 API・ダミー実装は置かない。
//
// - パストラバーサル防止・ルート配下検証（`assert_within_root` 等。`PROF-4`、
//   TASK-50（50.2）・#177）
// - `profile.lock`（`std::fs::File::try_lock`）による二重 open の拒否
//   （`PROF-1`、TASK-50（50.3）・#178。#175 の決定により `fs2` は使わない）
// - パーミッション・隔離の網羅的なテスト（TASK-50（50.4）・#179）
// - プロファイル削除処理（`PROF-5`、TASK-53）
// - 並行アクセス時のデータ分離（`PROF-2`・`PROF-3`、TASK-51・TASK-52）
// - クロスプラットフォーム advisory lock の 3 OS 確認（`PROF-1`、TASK-54）
