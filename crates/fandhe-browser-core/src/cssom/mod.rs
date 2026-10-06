//! CSSOM 最小構築の入口（TASK-105・MS-8・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! TASK-100 の CSSOM プロファイル feature gating が将来呼ぶ computed style API の土台として、
//! スタイルシート・ルール・宣言・詳細度の型を core に置く。レイアウトは含まない。
//! TASK-33 の [`crate::render`] 境界とは重ならず、新規依存も足さない（#263 の決定:
//! `cssparser` / `selectors` は MPL-2.0 のため core では使わず、セレクタは
//! [`crate::selector`] を共有・拡張する）。
//!
//! 現状は型定義（TASK-105.1・#255）、宣言パーサー [`parse_declarations`]（TASK-105.2・#256）、
//! 詳細度計算 [`specificity`] / [`specificities`]（TASK-105.3・#257）、
//! CSS テキストのルールブロック分割 [`split_rule_blocks`]（TASK-105.4.1・#551）、
//! DOM からのスタイル源収集と [`Stylesheet`] 構築 [`collect_document_styles`] /
//! [`parse_stylesheet`] / [`parse_style_attribute`]（TASK-105.4.2・#552）、
//! セレクタマッチング [`match_rules`]（TASK-105.5・#259）、
//! カスケード解決と computed style API [`cascade`] / [`computed_style`]（TASK-105.6・#260）まで。
//! 以下は未実装（REPAIR-3: 実装済みを装わない）。
//!
//! - 上限検証（#261）・結合テスト（#262）

mod computed;
mod declaration;
mod matcher;
mod selector;
mod stylesheet;
mod types;

pub use computed::{
    ComputedDeclaration, ComputedStyle, DeclarationOrigin, cascade, computed_style,
};
pub use declaration::{
    MAX_DECLARATION_INPUT_BYTES, MAX_DECLARATIONS_PER_BLOCK, parse_declarations,
};
pub use matcher::{MatchedRule, match_rules};
pub use selector::{specificities, specificity};
pub use stylesheet::{
    DocumentStyles, InlineStyle, MAX_RULE_BLOCK_ERRORS, MAX_RULES_PER_STYLESHEET,
    MAX_STYLESHEET_INPUT_BYTES, ParsedStylesheet, RuleBlock, RuleBlockError, RuleBlockErrorKind,
    RuleBlocks, StyleElementSheet, collect_document_styles, parse_style_attribute,
    parse_stylesheet, split_rule_blocks,
};
pub use types::{Declaration, Importance, Specificity, StyleRule, Stylesheet};

/// CSS の空白（` ` `\t` `\n` `\r` `\u{0C}`）。宣言パーサーとルール分割が共有する。
pub(super) fn is_css_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}
