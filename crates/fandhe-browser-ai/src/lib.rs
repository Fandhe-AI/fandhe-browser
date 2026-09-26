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
//! # スタブについて
//!
//! 本ファイルは TASK-1（旧 TASK-1.5・Issue #26）でビルド可能にするための
//! 最小骨格であり、公開 API は未実装（実装済みを装わない。REPAIR-3）。
//! 今後、以下のタスクで段階的に実装する：
//!
//! - 役割ベース DOM 簡約表現の中核実装（本 crate の中心機能。`AISNAP-1`、
//!   `TASK-11`、`MS-2`）
//! - `/ai/*` ルータ向けスナップショット API（`AISNAP-6`・`AISNAP-14`・`CDP-1`
//!   （API 単体動作は `AISNAP-7`）、`TASK-19`（単体動作確認は `TASK-20`）、
//!   `MS-4`）
//! - プラグインレジストリ（プロセス分離・stdio 経由の外部プラグイン呼び出し。
//!   動的ライブラリの実行時ロードは行わない方針。security.md 参照。
//!   `PLUG-2`、`TASK-92`、`MS-9`）
