//! fandhe-browser の AI 最適化 API・プラグイン API を担う crate。
//!
//! `crates/fandhe-browser-core` が保持する DOM を、AI エージェントが
//! 効率よく読める形（アクセシビリティツリー・役割ベースの簡約表現）へ
//! 変換して提供する層である。
//!
//! 依存方向（`docs/spec` submodule `04-behavior/self-repair-design.md`
//! 「crate 間の依存方向と `AppState` の配置」・`AGENTS.md`「crate 間の許可依存」）
//! により、本 crate は `fandhe-browser-core`・`fandhe-browser-profile` にのみ
//! 依存してよく、`fandhe-browser-cdp`・`fandhe-browser-render`・
//! `fandhe-browser-cli`・`fandhe-browser-mcp`・`fandhe-browser-js` には
//! 依存しない（上位 crate から下位 crate への一方向依存を保つため。
//! coding-rust.md）。同じ層である `fandhe-browser-cdp` との相互依存も禁止される。
//!
//! 本 crate は `/ai/*` の `Router` を [`api::router`] として自前で公開し
//! （TASK-19.1・Issue #223・`AISNAP-6`）、`fandhe-browser-cdp` とは互いに依存しない。
//! 両 crate は `core` の `Arc<AppState>` を共有し、ルータの合成（`Router::merge`）は
//! 両者に依存できる `fandhe-browser-cli` が担う（実装済み）
//! （TASK-19.3・Issue #225・`AISNAP-6`・`AISNAP-7`）。
//! 外部プラグイン（`crates/fandhe-browser-mcp` 等）からの利用も、mcp crate は
//! いずれの workspace crate にも依存しない契約のみの層であるため、同様に
//! `cli` 層での配線を経由する。
//!
//! 現在の workspace 内依存は `fandhe-browser-core` のみである（TASK-11.1・`AISNAP-1`・
//! `MS-2` で追加。`profile` は dev-dependency のみ）。外部依存は HTTP ルータ用の
//! `fandhe-backend-*` と JSON 生成用の `serde_json`（TASK-19.1）。
//!
//! # スタブについて
//!
//! 本ファイルは TASK-1（旧 TASK-1.5・Issue #26）でビルド可能にするための
//! 最小骨格を作り、TASK-11.1（`AISNAP-1`・`MS-2`・Issue #70）で
//! `fandhe-browser-core` への path 依存と [`snapshot`] モジュールの
//! スケルトンを追加した。TASK-11.2（Issue #71）で [`snapshot`] モジュールに
//! 公開型 `Snapshot`/`Node` を定義した。TASK-11.5（Issue #74）で状態
//! （`disabled`・`checked`）の算出ロジックを実装した（[`snapshot::compute_state`]）。
//! TASK-11.3.1（Issue #541）で役割（role）算出の骨格と button・link・
//! heading・table・list 等の代表要素を実装した（[`snapshot::compute_role`]）。
//! ただし `input[type]` の対応表（TASK-11.3.2・Issue #542）・`select`/
//! `header`/`footer`/`aside`（TASK-11.3.3・Issue #543）はまだ未実装で、
//! これらの要素は暫定的に `generic` になる。
//! TASK-11.4.2（Issue #545）で accessible name のうち HTML ネイティブの
//! ラベル付け分を実装し、TASK-11.4.1（Issue #544）で ARIA 分、
//! TASK-11.4.3（Issue #546）で子孫テキストと文書ルートの `<title>` を実装した
//! （[`snapshot::compute_name`]）。ツリー構築への組み込みは TASK-11.7（Issue #76）で実装した
//! （[`snapshot::build_snapshot`]。簡約は未実装）。今後、以下のタスクで段階的に実装する：
//!
//! - 役割ベース DOM 簡約表現の中核実装（本 crate の中心機能。[`snapshot`]
//!   モジュール。`AISNAP-1`、`TASK-11`、`MS-2`。公開型は定義済み
//!   （TASK-11.2・Issue #71）、状態算出は実装済み（TASK-11.5・Issue #74）、
//!   role 算出は骨格と代表要素のみ実装済み（TASK-11.3.1・Issue #541）、
//!   accessible name のネイティブ分・ARIA 分・子孫テキスト分は実装済み
//!   （TASK-11.4.2・Issue #545、TASK-11.4.1・Issue #544、TASK-11.4.3・
//!   Issue #546）、ref 生成器は実装済み（TASK-11.6・Issue #75）。
//!   ツリー構築統合は実装済み（TASK-11.7・Issue #76）。
//!   残作業は TASK-11.3.2・11.3.3（Issue #542・#543）。TASK-11.8（Issue #77）の
//!   結合テスト一式は `tests/snapshot.rs` で実装済み）
//! - 表・一覧の圧縮（`AISNAP-2`・`TASK-12`・`MS-2`）。規則的な行列構造の
//!   検出ロジックは実装済み（TASK-12.1・Issue #79。[`compress_table`]）。
//!   ヘッダ ref 付与は実装済み（TASK-12.2・Issue #80。`assign_header_refs`）。
//!   データ行の圧縮 1 行表現も実装済み（TASK-12.3・Issue #81。
//!   [`compress_table::compress_rows`]）。
//!   超過行数 `truncated_rows` も実装済み（TASK-12.4・Issue #82。
//!   [`compress_table::CompressedRows`]）。ツリー構築への統合は実装済み
//!   （TASK-12.5・Issue #83。[`snapshot::build_snapshot`] が [`snapshot::Node::table`] へ
//!   格納）。リンク・ボタンを含む表・一覧の圧縮と行内操作要素への ref 付与は実装済み
//!   （Issue #632。`snapshot::TableRow::controls`）。回帰テストは `tests/snapshot.rs` に実装済み
//!   （TASK-12.6・Issue #84。圧縮可能な HN 相当形状での削減確認と、`title` 属性・行数上限により
//!   圧縮されない忠実な HN 形状の既知ギャップの固定）
//! - データ葉（非インタラクティブなデータ値）の検出（`AISNAP-3`・`TASK-13`・
//!   `MS-2`）。`td`/`th` の検出（TASK-13.1・Issue #86）と
//!   価格クラス名パターン（TASK-13.2・Issue #87）・引用要素 `blockquote`/`q`（TASK-15.1・Issue #99）・
//!   地の文クラス（TASK-15.2・Issue #100）は実装済み（[`data_leaf`]）。
//!   snapshot 構築への統合は実装済み（TASK-13.3・Issue #88、拡充分は TASK-15.3・Issue #101。`Node::data_leaf`・圧縮表ヘッダの
//!   `HeaderCell::data_leaf`）
//! - 重要要素優先保持（`AISNAP-12`・`TASK-16`・`MS-2`）。先頭件の優先保持と
//!   予算モデルは実装済み（TASK-16.1・Issue #104。[`retention`]）。ページネーションリンクの優先保持（TASK-16.2・Issue #105。
//!   [`retention::pagination`]）・フォーム送信ボタンの優先保持（TASK-16.3・
//!   Issue #106。[`retention::submit_button`]）も実装済み。
//!   固定キャップの置換（TASK-16.4・Issue #107。`compress_table::compress_rows`）は
//!   実装済み。キャップ境界要素の軽微変化耐性テストも実装済み（TASK-16.5・
//!   Issue #108。`tests/retention.rs`）
//! - `/ai/*` ルータ向けスナップショット API（`AISNAP-6`・`AISNAP-14`・`CDP-1`
//!   （API 単体動作は `AISNAP-7`）、`TASK-19`（単体動作確認は `TASK-20`）、
//!   `MS-4`）。`GET /ai/snapshot` の正常系の骨格は実装済み（TASK-19.1・Issue #223。
//!   [`api::router`]）。未ナビゲート時エラー（409 / `no_navigation`）も実装済み（TASK-19.2・
//!   Issue #224）。cli への合成も実装済み（TASK-19.3・Issue #225。`default_router_factories`）。結合テスト（TASK-19.4・Issue #226）は cli の `server.rs` テストで実装済み
//! - プラグインレジストリ（プロセス分離・stdio 経由の外部プラグイン呼び出し。
//!   動的ライブラリの実行時ロードは行わない方針。security.md 参照。
//!   `PLUG-2`、`TASK-92`、`MS-9`）。マニフェスト型は実装済み（TASK-92.1・Issue #353。
//!   [`plugin_api`]）。レジストリ・エンドポイントは未実装（TASK-92.2〜92.4）

pub mod api;
pub mod compress_table;
pub mod data_leaf;
pub mod plugin_api;
pub mod retention;
pub mod snapshot;

#[cfg(test)]
mod reference_stability;
