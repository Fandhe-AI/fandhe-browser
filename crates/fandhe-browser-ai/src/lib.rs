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
//! `fandhe-browser-cdp` の `/ai/*` 系ルータは本 crate を直接呼び出さない。
//! 両 crate はいずれも `fandhe-browser-core` にのみ依存する同じ層に位置し、
//! 結合は `fandhe-browser-render` と同様に `core` が定義する抽象（トレイト）
//! 経由で行い、具体的な結線（本 crate の実装を `core` のトレイトへ適合させ、
//! cdp のルータへ渡す配線）は両者に依存できる `fandhe-browser-cli` が担う
//! 想定である（TASK-19・AISNAP-6・AISNAP-7 で具体化）。
//! 外部プラグイン（`crates/fandhe-browser-mcp` 等）からの利用も、mcp crate は
//! いずれの workspace crate にも依存しない契約のみの層であるため、同様に
//! `cli` 層での配線を経由する。
//!
//! 現在の実依存は `fandhe-browser-core` のみである（TASK-11.1・`AISNAP-1`・
//! `MS-2` で追加。`profile` は必要になったタスクで追加する）。
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
//!   [`compress_table::compress_rows`]・統合前のため呼び出し元なし）。
//!   `truncated_rows` 注記（#82）・ツリー構築への統合（#83）・回帰テスト（#84）は未実装
//! - `/ai/*` ルータ向けスナップショット API（`AISNAP-6`・`AISNAP-14`・`CDP-1`
//!   （API 単体動作は `AISNAP-7`）、`TASK-19`（単体動作確認は `TASK-20`）、
//!   `MS-4`）
//! - プラグインレジストリ（プロセス分離・stdio 経由の外部プラグイン呼び出し。
//!   動的ライブラリの実行時ロードは行わない方針。security.md 参照。
//!   `PLUG-2`、`TASK-92`、`MS-9`）

pub mod compress_table;
pub mod snapshot;
