//! query: `dom` が保持する木構造に対する CSS セレクタ問い合わせを担うモジュール。
//!
//! `dom` モジュール（TASK-24.5・#39）の走査 API と `selector` モジュール
//! （TASK-24.7・#41）が解析する `SelectorList` を組み合わせ、
//! `Element.matches()` / `querySelector` / `querySelectorAll` 相当の API を
//! 提供する（TASK-24（24.10）・ビヘイビア `CORE-1`・Issue #418）。`cdp`
//! （`DOM.querySelector` 系ハンドラ）・`ai`（簡約 DOM 抽出）から呼ばれる想定で、
//! 戻り値は要素の要約（tag・text・attrs 等）ではなく [`crate::dom::NodeId`] に
//! 留める。呼び出し側が `dom` のアクセサ（`local_name`・`attributes`・
//! `text_content`）で必要な要約を組み立てる（REPAIR-4: 戻り値は将来拡張できる
//! 構造にする。PoC-2 の `ElementInfo` 相当の高レベル helper は本モジュールの
//! スコープ外。dom.rs モジュール doc の方針と揃える）。
//!
//! # scope の意味論
//!
//! 候補は [`Document::descendants`]`(scope)` のうち [`Document::is_element`]
//! を満たすノードのみとし、`scope` 自身は候補に含めない（DOM の
//! `querySelectorAll` と同じ）。`descendants` はもともと `<template>` の
//! template contents を辿らないため、template contents 配下を検索したい
//! 場合は呼び出し側が [`Document::template_contents`] を `scope` に渡す
//! （`DocumentFragment` も `scope` にできる）。
//!
//! 結合子（子孫・子）の祖先照合は `scope` より上の祖先まで辿る。これは DOM
//! 仕様どおりの挙動で `:scope` 相対ではない（例: `scope` が `<span>` の親
//! `<p>` でも、`div span` は `<p>` の外側にある `<div>` によって一致し得る）。
//!
//! `scope` が範囲外の [`crate::dom::NodeId`] やテキストノードの場合は、
//! 候補となる子孫要素が存在しないため空の結果になる（panic しない）。
//! 文書全体を対象にする場合は [`Document::root`] を `scope` に渡す。
//!
//! # 照合規則
//!
//! - 要素以外のノードは常に不一致（[`Document::is_element`] で判定）。
//! - 型名（[`crate::selector::CompoundSelector::type_name`]）: HTML 名前空間
//!   の要素なら [`crate::selector::html_local_name_eq`]（ASCII 大文字小文字
//!   を無視）、それ以外（SVG・MathML 等）は完全一致（`==`）で比較する
//!   （`selector` モジュール doc の照合契約と `dom::Document::attribute` の
//!   規則に揃える）。HTML 名前空間かどうかは `dom::Document::namespace_url`
//!   が返す名前空間 URI で判定する。
//! - ID・クラス: [`Document::attribute`]・[`Document::class_names`] を使う。
//!   `dom::Document::quirks_mode` が `Quirks` の場合、Selectors 仕様に従い
//!   ASCII 大文字小文字を無視して比較する。`NoQuirks`/`LimitedQuirks` では
//!   完全一致にする。
//! - 属性（[`crate::selector::AttributeSelector`]）: [`Document::attribute`]
//!   を使う（名前空間なしの属性に限定。HTML 名前空間の要素なら属性名は
//!   大文字小文字を無視して照合する。`Document::attribute` の契約どおり）。
//!   `Exists` は値の有無、`Equals` は値の完全一致（大文字小文字を区別する）
//!   で判定する。HTML 仕様の「値を大文字小文字無視で比較する属性」
//!   （`type`・`lang` 等）への対応は本モジュールのスコープ外（REPAIR-3:
//!   実装済みを装わない。必要になった段階で別 Issue で拡張する）。
//!
//! # 文書順・重複排除
//!
//! [`Document::descendants`] を 1 回だけ走査し、各要素がリスト中いずれかの
//! `ComplexSelector` に一致するかを判定する。この方式により文書順の保持と
//! 重複排除が同時に満たされる（arena のインデックスは foster parenting・
//! adoption agency により文書順と一致しないため、セレクタごとに結果を集めて
//! マージ・ソートする方式は採らない）。
//!
//! # 計算量・DoS 対策
//!
//! 複雑セレクタの照合は右から左へバックトラックしながら行う（右端の複合
//! セレクタから照合し、子結合子は親を辿り、子孫結合子は祖先を順に試す）。
//! 再帰は複合セレクタのインデックス方向のみに限り（最大
//! [`crate::selector::MAX_COMPOUNDS_PER_COMPLEX`] 段）、祖先方向は
//! [`any_ancestor_matches`] が [`Document::ancestors`] を用いて反復
//! （非再帰）で走査するため、文書の深さに対する再帰は積まない（壊れた
//! arena・深いネストでもスタックオーバーフローせず必ず停止する。
//! security.md「不安全な設計」対策）。`Document::ancestors` は `parent`
//! リンクが循環していても [`Document::node_count`] 歩で打ち切る契約を
//! 持つため、本モジュール側で手動の祖先ループを実装し直さず、その契約を
//! そのまま継承する（PR #439 レビュー指摘・Cursor Bugbot: 手動ループでは
//! `node_count` 上限を持たず、循環・破損 arena で無限ループ・無制限
//! アロケーションになり得た）。
//!
//! 子孫結合子は複数の祖先を試すバックトラックを伴うため、単純に毎回祖先を
//! 辿り直すと、深いネストに `a b c d ...` のように子孫結合子を連ねた
//! セレクタを左端で不一致にした場合、祖先の組み合わせ数に対して指数的に
//! 探索が膨れる（PR #439 レビュー指摘・discussion_r4111107367）。これを
//! 避けるため [`MatchCache`] で `(selector_idx, ノード, up_to)` 単位の
//! 部分問題をメモ化し、[`query_selector_all`]・[`query_selector`] は
//! すべての候補要素にわたって 1 つの `MatchCache` を共有する（候補ごとに
//! 使い捨てると祖先の判定結果を候補数だけ再計算してしまうため）。
//! これにより最悪計算量は O(候補要素数 × セレクタ内の複合セレクタ数) に
//! 収まり、文書の深さに対して指数的には増えない。
//!
//! `MatchCache` のエントリ数は理論上、候補要素数（`ParseOptions::max_nodes`。
//! 既定 100 万）× セレクタ数（[`crate::selector::MAX_SELECTORS_PER_LIST`]。
//! 64）× 複合セレクタ数（[`crate::selector::MAX_COMPOUNDS_PER_COMPLEX`]。
//! 32）に縛られるが、この理論上限は実効的なメモリ上限としては大きすぎる
//! （PR #439 レビュー指摘 P0: `max_nodes`・セレクタ上限の範囲内でもキャッシュ
//! だけでメモリを使い尽くせる）。そのため [`MAX_MATCH_CACHE_ENTRIES`] を
//! 独立した実効上限として設け、`MatchCache` へのエントリ追加がこれを超える
//! 場合は [`crate::error::Error::MatchCacheLimitExceeded`] を明示的な API
//! エラーとして返す（一律 `false`/空の結果へフォールバックしない。誤った
//! 照合結果を「一致なし」として返すと呼び出し側の判断を誤らせるため）。
//! [`element_matches`]・[`query_selector_all`]・[`query_selector`] は
//! いずれもこのエラーを伝播する。bloom filter 等によるさらなる高速化は
//! 本モジュールのスコープ外（REPAIR-3: 将来課題として明記する）。
//!
//! 対応 ID: `CORE-1`・TASK-24（24.10）・MS-1。

use std::collections::HashMap;

use crate::dom::{Document, NodeId, QuirksMode};
use crate::error::{Error, Result};
use crate::selector::{
    AttributeMatcher, Combinator, ComplexSelector, CompoundSelector, SelectorList, SimpleSelector,
    html_local_name_eq, parse_selector_list,
};

/// `MatchCache`（`chain`・`ancestor` 合計）が保持できるエントリ数の実効上限。
///
/// ノード数×セレクタ数×複合セレクタ数の理論上限（モジュール doc 参照）は
/// 実用上のメモリ上限としては大きすぎるため、独立した固定値でメモリ使用量を
/// 抑える（PR #439 レビュー指摘 P0）。超過時は
/// [`crate::error::Error::MatchCacheLimitExceeded`] を返す。
pub const MAX_MATCH_CACHE_ENTRIES: usize = 1_000_000;

/// `dom::Document::HTML_NAMESPACE_URI` 相当の値（`dom` モジュールに
/// `pub(crate)` として定義済みのものを再利用する）。
///
/// crate 内の単一の真実源を保つため、ここで独自に定数を再定義せず
/// `dom` の関数（[`Document::namespace_url`]）が返す値をそのまま比較する。
fn is_html_element(document: &Document, element: NodeId) -> bool {
    document.namespace_url(element) == Some(crate::dom::HTML_NAMESPACE_URI)
}

/// `element` が `compound` に一致するかを判定する（型名・ID・クラス・属性）。
///
/// 呼び出し前提: `element` は [`Document::is_element`] を満たすノードである
/// こと（呼び出し元の [`complex_matches`] がすでに検証済み）。
fn compound_matches(document: &Document, element: NodeId, compound: &CompoundSelector) -> bool {
    let is_html = is_html_element(document, element);

    if let Some(type_name) = compound.type_name() {
        let matches_type = match document.local_name(element) {
            Some(local_name) if is_html => html_local_name_eq(local_name, type_name),
            Some(local_name) => local_name == type_name,
            None => false,
        };
        if !matches_type {
            return false;
        }
    }

    compound
        .simple_selectors()
        .iter()
        .all(|simple| simple_matches(document, element, simple))
}

/// `element` が単一の [`SimpleSelector`] に一致するかを判定する。
fn simple_matches(document: &Document, element: NodeId, simple: &SimpleSelector) -> bool {
    match simple {
        SimpleSelector::Id(value) => document
            .attribute(element, "id")
            .is_some_and(|actual| id_or_class_eq(document, actual, value)),
        SimpleSelector::Class(value) => document
            .class_names(element)
            .any(|actual| id_or_class_eq(document, actual, value)),
        SimpleSelector::Attribute(attr) => match_attribute(document, element, attr),
        // `SimpleSelector` は `#[non_exhaustive]`（selector.rs）だが、本
        // モジュールは crate 内でその全 variant を網羅する義務を負う
        // （coding-rust.md: crate 内の match はワイルドカード腕なしで書き、
        // 将来 variant 追加時に更新漏れをコンパイルエラーで検出させる）。
        // `non_exhaustive` は crate 外にしか効かないため、ここではワイルド
        // カード腕を書かない。
    }
}

/// ID・クラスの値比較。`quirks_mode` が `Quirks` の場合のみ ASCII
/// 大文字小文字を無視する（Selectors 仕様の quirks mode 挙動）。
fn id_or_class_eq(document: &Document, actual: &str, expected: &str) -> bool {
    if document.quirks_mode() == QuirksMode::Quirks {
        actual.eq_ignore_ascii_case(expected)
    } else {
        actual == expected
    }
}

/// 属性セレクタの照合。`Document::attribute` が要素自身の名前空間から
/// HTML 名前空間かどうかを判定し、大文字小文字の扱いを決める契約になって
/// いるため、本関数側で `is_html` を別途受け取る必要はない。
fn match_attribute(
    document: &Document,
    element: NodeId,
    attr: &crate::selector::AttributeSelector,
) -> bool {
    match &attr.matcher {
        AttributeMatcher::Exists => document.attribute(element, &attr.name).is_some(),
        AttributeMatcher::Equals(expected) => {
            document.attribute(element, &attr.name) == Some(expected.as_str())
        }
    }
}

/// 子孫結合子（[`Combinator::Descendant`]）の祖先バックトラックを
/// メモ化するためのキャッシュ（DoS 対策。モジュール doc 参照）。1 回の
/// `query_selector_all` / `query_selector` / `element_matches` 呼び出しの
/// 間だけ生存し、呼び出しをまたいで再利用しない（`Document` の変更を
/// 考慮しなくてよいようにするため）。
///
/// - `chain`: `(selector_idx, node のインデックス, up_to)` →
///   「`node` 自身が `up_to` から左側の連鎖に一致するか」の判定結果
///   （[`matches_compound_chain`]）。
/// - `ancestor`: `(selector_idx, node のインデックス, up_to)` →
///   「`node` の祖先（要素のみ）のいずれかが `up_to` から左側の連鎖に
///   一致するか」の判定結果（[`any_ancestor_matches`]）。
///
/// `selector_idx` は `SelectorList::selectors()` 内での位置で、複合セレクタ
/// の構造（`rest` の中身）はセレクタごとに異なるため、添字だけでは
/// 異なるセレクタ間でキーが衝突しうる（`up_to` はあくまで添字であって
/// 複合セレクタの内容そのものではない）。`selector_idx` をキーに含めて
/// セレクタ単位で名前空間を分けることでこれを避ける。
///
/// 挿入は [`MatchCache::insert_chain`]/[`MatchCache::insert_ancestor`] を
/// 経由し、合計エントリ数が [`MAX_MATCH_CACHE_ENTRIES`] を超える新規キーの
/// 追加を拒否する（メモリ使用量の実効上限。モジュール doc 参照）。
struct MatchCache {
    chain: HashMap<(usize, usize, Option<usize>), bool>,
    ancestor: HashMap<(usize, usize, Option<usize>), bool>,
}

impl MatchCache {
    fn new() -> Self {
        Self {
            chain: HashMap::new(),
            ancestor: HashMap::new(),
        }
    }

    /// 現在の合計エントリ数（`chain` + `ancestor`）。
    fn len(&self) -> usize {
        self.chain.len() + self.ancestor.len()
    }

    /// `key` が新規キーで、かつ挿入すると合計エントリ数が
    /// [`MAX_MATCH_CACHE_ENTRIES`] を超える場合に `true` を返す
    /// （既存キーの上書きは合計エントリ数を増やさないため許可する）。
    fn would_exceed_limit(&self, is_new_key: bool) -> bool {
        is_new_key && self.len() >= MAX_MATCH_CACHE_ENTRIES
    }

    /// `chain` へ `(key, value)` を記録する。上限超過時は
    /// [`Error::MatchCacheLimitExceeded`] を返し、挿入しない。
    fn insert_chain(&mut self, key: (usize, usize, Option<usize>), value: bool) -> Result<()> {
        if self.would_exceed_limit(!self.chain.contains_key(&key)) {
            return Err(Error::MatchCacheLimitExceeded {
                limit: MAX_MATCH_CACHE_ENTRIES,
            });
        }
        self.chain.insert(key, value);
        Ok(())
    }

    /// `ancestor` へ `(key, value)` を記録する。上限超過時は
    /// [`Error::MatchCacheLimitExceeded`] を返し、挿入しない。
    fn insert_ancestor(&mut self, key: (usize, usize, Option<usize>), value: bool) -> Result<()> {
        if self.would_exceed_limit(!self.ancestor.contains_key(&key)) {
            return Err(Error::MatchCacheLimitExceeded {
                limit: MAX_MATCH_CACHE_ENTRIES,
            });
        }
        self.ancestor.insert(key, value);
        Ok(())
    }
}

/// `element` が `complex`（`selectors` 内で `selector_idx` 番目）に一致
/// するかを、右端の複合セレクタから左へバックトラックしながら判定する。
///
/// 呼び出し前提: `element` は [`Document::is_element`] を満たすノードである
/// こと。
fn complex_matches(
    document: &Document,
    element: NodeId,
    complex: &ComplexSelector,
    selector_idx: usize,
    cache: &mut MatchCache,
) -> Result<bool> {
    // `rest` の最後の添字を「次に照合する複合セレクタ」として
    // `matches_compound_chain` に渡す。`rest` が空（`checked_sub` が `None`）
    // なら `first` 自身が右端になる。
    let up_to = complex.rest().len().checked_sub(1);
    matches_compound_chain(document, element, complex, up_to, selector_idx, cache)
}

/// `node` が、`up_to`（`rest` の添字。`None` なら `first`）が指す複合セレクタ
/// から左側（`first` 方向）にかけての連鎖全体に一致するかを判定する。
///
/// 1. `up_to` が指す複合セレクタを `node` 自身に照合する。
/// 2. 一致すれば、その複合セレクタに紐づく結合子（`rest[up_to].0`。`up_to`
///    が `None`＝`first` の場合はこれ以上左がないため即座に一致で終える）に
///    従って左隣（子結合子）または祖先（子孫結合子）へ進み、1 つ手前の
///    複合セレクタ（`up_to - 1`。`0` なら `first`）との照合を再帰する。
///
/// 再帰は `up_to` を単調に減らすだけなので、深さは高々
/// `complex.rest().len()`（≤ [`crate::selector::MAX_COMPOUNDS_PER_COMPLEX`]）
/// に収まり、文書の深さには比例しない（祖先方向は [`any_ancestor_matches`]
/// が反復（非再帰）で走査するため、こちらも文書の深さ分の再帰を積まない）。
///
/// 判定結果は `cache.chain` に `(selector_idx, node のインデックス, up_to)`
/// をキーに記録する（[`MatchCache`] のドキュメント参照。DoS 対策の
/// メモ化）。キャッシュが [`MAX_MATCH_CACHE_ENTRIES`] に達している場合は
/// [`Error::MatchCacheLimitExceeded`] を返す。
fn matches_compound_chain(
    document: &Document,
    node: NodeId,
    complex: &ComplexSelector,
    up_to: Option<usize>,
    selector_idx: usize,
    cache: &mut MatchCache,
) -> Result<bool> {
    let cache_key = (selector_idx, node.index(), up_to);
    if let Some(&cached) = cache.chain.get(&cache_key) {
        return Ok(cached);
    }

    let result =
        matches_compound_chain_uncached(document, node, complex, up_to, selector_idx, cache)?;
    cache.insert_chain(cache_key, result)?;
    Ok(result)
}

/// [`matches_compound_chain`] のキャッシュ未命中時の本体。祖先探索
/// （子孫結合子）・親探索（子結合子）で再帰する際は必ず
/// [`matches_compound_chain`]（キャッシュ経由）を呼び、直接自分自身を
/// 呼ばない（メモ化を素通りさせないため）。
fn matches_compound_chain_uncached(
    document: &Document,
    node: NodeId,
    complex: &ComplexSelector,
    up_to: Option<usize>,
    selector_idx: usize,
    cache: &mut MatchCache,
) -> Result<bool> {
    let Some(index) = up_to else {
        return Ok(compound_matches(document, node, complex.first()));
    };

    // 外部入力（セレクタ文字列）から構築した `rest` への添字アクセスを
    // 避け、`get` で範囲外を検出する（`index` は常に呼び出し元が
    // `rest.len()` 未満の値として渡すため通常は到達しないが、添字アクセス
    // `[]` を使わない方針を徹底する）。
    let Some((combinator, compound)) = complex.rest().get(index) else {
        return Ok(false);
    };
    if !compound_matches(document, node, compound) {
        return Ok(false);
    }

    let next_up_to = index.checked_sub(1);
    match combinator {
        Combinator::Child => {
            let Some(parent) = document.parent(node) else {
                return Ok(false);
            };
            Ok(document.is_element(parent)
                && matches_compound_chain(
                    document,
                    parent,
                    complex,
                    next_up_to,
                    selector_idx,
                    cache,
                )?)
        }
        Combinator::Descendant => {
            any_ancestor_matches(document, node, complex, next_up_to, selector_idx, cache)
        }
    }
}

/// `node` の祖先（[`Document::is_element`] を満たすもののみ）のいずれかが、
/// `up_to` から左側の複合セレクタ連鎖に一致するかを判定する
/// （[`Combinator::Descendant`] の照合本体）。
///
/// 素朴な実装（各祖先ごとに [`matches_compound_chain`] を再帰的に試す）は、
/// 深くネストした文書に子孫結合子を連ねたセレクタ（例:
/// `.missing div div div ...`）を照合すると、祖先の組み合わせを指数的に
/// 探索してしまう（security.md「不安全な設計」・AGENTS.md
/// 「リソース上限」対策）。本関数は代わりに [`Document::ancestors`] で
/// 祖先の並びを 1 度だけ反復で遡り、各ノードでの判定結果を
/// `cache.ancestor` に記録することで、同一 `(selector_idx, ノード, up_to)`
/// の部分問題を高々 1 回しか計算しない。`Document::ancestors` は `parent`
/// リンクが循環・破損していても [`Document::node_count`] 歩で必ず打ち切る
/// 契約を持つため、本関数を含め祖先方向の走査全体がその保証を継承する
/// （手動ループを独自実装しない。PR #439 レビュー指摘・Cursor Bugbot:
/// 独自ループは `node_count` 上限を持たず循環 arena で無限ループし得た）。
///
/// これにより `element_matches` / `query_selector_all` 1 回あたりの
/// 計算量は O(文書のノード数 × セレクタ内の複合セレクタ数) に収まり、
/// 文書の深さに対して指数的には増えない。反復（非再帰）で遡るため、
/// 文書がどれだけ深くネストしていてもスタックオーバーフローしない。
///
/// 漸化式: `any_ancestor_matches(X) = matches_compound_chain(parent(X))
/// || any_ancestor_matches(parent(X))`。各祖先 `parent` について、まず
/// `matches_compound_chain(parent)`（`parent` 自身が一致するか）を必ず
/// 先に確認してから `cache.ancestor`（`parent` の「さらに祖先」が一致
/// するか）を参照する。この順序を逆にすると、`parent` 自身の一致を
/// 確認しないまま `cache.ancestor` の既知の値（多くは他ノードの探索で
/// 先に確定した `false`）だけで打ち切ってしまい、本来一致するはずの
/// 深いノードを誤って不一致と判定する（`matches_compound_chain` 自身も
/// `cache.chain` でメモ化されているため、この呼び出し自体のコストは
/// 償却 O(1)）。キャッシュが [`MAX_MATCH_CACHE_ENTRIES`] に達している
/// 場合は [`Error::MatchCacheLimitExceeded`] を返す。
fn any_ancestor_matches(
    document: &Document,
    node: NodeId,
    complex: &ComplexSelector,
    up_to: Option<usize>,
    selector_idx: usize,
    cache: &mut MatchCache,
) -> Result<bool> {
    let start_key = (selector_idx, node.index(), up_to);
    if let Some(&cached) = cache.ancestor.get(&start_key) {
        return Ok(cached);
    }

    // 遡った要素祖先を記録し、結果が確定した後にまとめてキャッシュへ
    // 書き込む（途中でキャッシュ済みの祖先に到達した場合は、その祖先より
    // 手前（今回遡った分）も同じ結果になる）。
    let mut visited = Vec::new();
    let mut result = false;
    for ancestor in document.ancestors(node) {
        if !document.is_element(ancestor) {
            // 要素でない祖先（例: DocumentFragment）は候補にならないが、
            // その先の祖先は候補になりうるため遡りを継続する。
            continue;
        }

        // `ancestor` 自身が一致するかを必ず先に確認する（上記ドキュメント
        // 参照）。`cache.chain` 経由でメモ化されるため、`ancestor` がすでに
        // 他の探索で確認済みなら実質 O(1) で戻る。
        if matches_compound_chain(document, ancestor, complex, up_to, selector_idx, cache)? {
            result = true;
            break;
        }
        // `ancestor` 自身は不一致。`ancestor` のさらに祖先の判定がすでに
        // 分かっていれば、その値がここでの結果にもなる
        // （`any_ancestor_matches(node) = false || any_ancestor_matches(ancestor)`）。
        let ancestor_key = (selector_idx, ancestor.index(), up_to);
        if let Some(&cached) = cache.ancestor.get(&ancestor_key) {
            result = cached;
            break;
        }
        visited.push(ancestor);
    }

    for ancestor in visited {
        cache.insert_ancestor((selector_idx, ancestor.index(), up_to), result)?;
    }
    cache.insert_ancestor(start_key, result)?;
    Ok(result)
}

/// `element` がセレクタリストの少なくとも 1 個の [`ComplexSelector`] に
/// 一致するかを判定する。
///
/// `cache` は呼び出し元（[`element_matches`]・[`query_selector_all`]・
/// [`query_selector`]）が用意し、複数の候補要素にわたって共有する
/// （DoS 対策のメモ化を候補間でも効かせるため。[`MatchCache`] 参照）。
fn list_matches(
    document: &Document,
    element: NodeId,
    selectors: &SelectorList,
    cache: &mut MatchCache,
) -> Result<bool> {
    for (selector_idx, complex) in selectors.selectors().iter().enumerate() {
        if complex_matches(document, element, complex, selector_idx, cache)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `element` が `selectors` に一致するかどうかを判定する（`Element.matches()`
/// 相当）。`element` が要素でない場合や範囲外の場合は `Ok(false)` を返す
/// （panic しない）。
///
/// # エラー
///
/// 内部の照合メモ化キャッシュが [`MAX_MATCH_CACHE_ENTRIES`] を超える場合、
/// [`Error::MatchCacheLimitExceeded`] を返す（モジュール doc 参照）。
pub fn element_matches(
    document: &Document,
    element: NodeId,
    selectors: &SelectorList,
) -> Result<bool> {
    let mut cache = MatchCache::new();
    if !document.is_element(element) {
        return Ok(false);
    }
    list_matches(document, element, selectors, &mut cache)
}

/// `scope` の子孫要素のうち `selectors` に一致するものを、文書順・重複なしで
/// 全件返す（`querySelectorAll` 相当）。`scope` 自身は対象に含めない。
///
/// scope の意味論・照合規則はモジュール doc を参照。一致がなければ空の
/// `Vec` を返す（panic しない）。
///
/// `MatchCache` は呼び出し 1 回分をすべての候補要素で共有する（候補ごとに
/// 使い捨てると、祖先ノードの判定結果を候補の数だけ再計算してしまい、
/// メモ化の効果が薄れる。[`MatchCache`] のドキュメント参照）。
///
/// # エラー
///
/// 内部の照合メモ化キャッシュが [`MAX_MATCH_CACHE_ENTRIES`] を超える場合、
/// [`Error::MatchCacheLimitExceeded`] を返す（モジュール doc 参照）。
pub fn query_selector_all(
    document: &Document,
    scope: NodeId,
    selectors: &SelectorList,
) -> Result<Vec<NodeId>> {
    let mut cache = MatchCache::new();
    let mut results = Vec::new();
    for candidate in document.descendants(scope) {
        if document.is_element(candidate)
            && list_matches(document, candidate, selectors, &mut cache)?
        {
            results.push(candidate);
        }
    }
    Ok(results)
}

/// `scope` の子孫要素のうち `selectors` に一致する最初のもの（文書順）を
/// 返す（`querySelector` 相当）。最初の一致で走査を打ち切る。
///
/// scope の意味論・照合規則はモジュール doc を参照。一致がなければ `None`
/// を返す（panic しない）。
///
/// # エラー
///
/// 内部の照合メモ化キャッシュが [`MAX_MATCH_CACHE_ENTRIES`] を超える場合、
/// [`Error::MatchCacheLimitExceeded`] を返す（モジュール doc 参照）。
pub fn query_selector(
    document: &Document,
    scope: NodeId,
    selectors: &SelectorList,
) -> Result<Option<NodeId>> {
    let mut cache = MatchCache::new();
    for candidate in document.descendants(scope) {
        if document.is_element(candidate)
            && list_matches(document, candidate, selectors, &mut cache)?
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// `selector` 文字列を [`parse_selector_list`] で解析してから
/// [`query_selector_all`] を呼ぶ薄いラッパー。
///
/// # エラー
///
/// `parse_selector_list` が返すエラー（[`crate::error::Error::InvalidInput`]・
/// [`crate::error::Error::Unsupported`]）・[`query_selector_all`] が返す
/// [`Error::MatchCacheLimitExceeded`] をそのまま返す。
pub fn query_selector_all_str(
    document: &Document,
    scope: NodeId,
    selector: &str,
) -> Result<Vec<NodeId>> {
    let selectors = parse_selector_list(selector)?;
    query_selector_all(document, scope, &selectors)
}

/// `selector` 文字列を [`parse_selector_list`] で解析してから
/// [`query_selector`] を呼ぶ薄いラッパー。
///
/// # エラー
///
/// `parse_selector_list` が返すエラー（[`crate::error::Error::InvalidInput`]・
/// [`crate::error::Error::Unsupported`]）・[`query_selector`] が返す
/// [`Error::MatchCacheLimitExceeded`] をそのまま返す。
pub fn query_selector_str(
    document: &Document,
    scope: NodeId,
    selector: &str,
) -> Result<Option<NodeId>> {
    let selectors = parse_selector_list(selector)?;
    query_selector(document, scope, &selectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::NodeData;
    use crate::error::Error;
    use crate::parse::{ParseOptions, parse_document};

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn find_by_local_name(doc: &Document, root: NodeId, local_name: &str) -> NodeId {
        doc.descendants(root)
            .find(|&id| doc.local_name(id) == Some(local_name))
            .unwrap_or_else(|| panic!("要素 {local_name} が見つからない"))
    }

    fn selectors(input: &str) -> SelectorList {
        parse_selector_list(input).unwrap_or_else(|e| panic!("{input:?} は解析できるはず: {e}"))
    }

    fn text(doc: &Document, id: NodeId) -> String {
        doc.text_content(id).unwrap_or_default()
    }

    /// テスト用ヘルパー: `query_selector_all` を呼び、`Result` を展開する
    /// （テストでは `MatchCacheLimitExceeded` は発生しない想定）。
    fn query_all(doc: &Document, scope: NodeId, selectors: &SelectorList) -> Vec<NodeId> {
        query_selector_all(doc, scope, selectors).expect("キャッシュ上限に達しないはず")
    }

    /// テスト用ヘルパー: `query_selector` を呼び、`Result` を展開する。
    fn query_first(doc: &Document, scope: NodeId, selectors: &SelectorList) -> Option<NodeId> {
        query_selector(doc, scope, selectors).expect("キャッシュ上限に達しないはず")
    }

    /// CORE-1: 型セレクタが文書順で全件一致する。
    #[test]
    fn core_1_type_selector_matches_in_document_order() {
        let doc = parse("<ul><li>a</li><li>b</li><li>c</li></ul>");
        let root = doc.root();
        let results = query_all(&doc, root, &selectors("li"));
        let texts: Vec<String> = results.iter().map(|&id| text(&doc, id)).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    /// CORE-1: ID・複数クラス・属性存在・属性完全一致・複合セレクタが
    /// それぞれ期待する要素だけに一致する。
    #[test]
    fn core_1_id_class_attribute_and_compound_selectors() {
        let doc = parse(
            r#"<div id="main">
                <a class="link primary" href="/x">a</a>
                <input type="checkbox" data-x="1">
                <span class="link">not a link tag</span>
            </div>"#,
        );
        let root = doc.root();

        let by_id = query_all(&doc, root, &selectors("#main"));
        assert_eq!(by_id.len(), 1);
        assert_eq!(doc.local_name(by_id[0]), Some("div"));

        let by_class = query_all(&doc, root, &selectors(".link"));
        assert_eq!(by_class.len(), 2);

        let by_href = query_all(&doc, root, &selectors("[href]"));
        assert_eq!(by_href.len(), 1);
        assert_eq!(doc.local_name(by_href[0]), Some("a"));

        let by_checkbox = query_all(&doc, root, &selectors("[type=checkbox]"));
        assert_eq!(by_checkbox.len(), 1);
        assert_eq!(doc.local_name(by_checkbox[0]), Some("input"));

        let by_data = query_all(&doc, root, &selectors("[data-x=\"1\"]"));
        assert_eq!(by_data.len(), 1);
        assert_eq!(doc.local_name(by_data[0]), Some("input"));

        let by_compound = query_all(&doc, root, &selectors("a.link[href]"));
        assert_eq!(by_compound.len(), 1);
        assert_eq!(doc.local_name(by_compound[0]), Some("a"));
    }

    /// CORE-1: `div > p` は直接の子のみ、`div p` は子孫すべてに一致する
    /// （子結合子・子孫結合子の差）。
    #[test]
    fn core_1_child_vs_descendant_combinator() {
        // `<p>` の中に `<p>` を書くと、HTML5 のパース規則（新しい `<p>` 開始
        // タグは開いている `<p>` を暗黙に閉じる）により意図した入れ子構造に
        // ならないため、`<section>` で挟んで検証する。
        let doc = parse(
            "<div><p id=\"direct\"><span>a</span></p><section><p id=\"nested\">b</p></section></div>",
        );
        let root = doc.root();

        let child_matches = query_all(&doc, root, &selectors("div > p"));
        assert_eq!(child_matches.len(), 1);
        assert_eq!(doc.attribute(child_matches[0], "id"), Some("direct"));

        let descendant_matches = query_all(&doc, root, &selectors("div p"));
        assert_eq!(descendant_matches.len(), 2);
        let ids: Vec<Option<&str>> = descendant_matches
            .iter()
            .map(|&id| doc.attribute(id, "id"))
            .collect();
        assert_eq!(ids, vec![Some("direct"), Some("nested")]);
    }

    /// CORE-1: `.a > .b .c` は直近の `.b` では親が `.a` にならず失敗し、
    /// より外側の `.b` まで遡って一致するバックトラックを要求する。
    #[test]
    fn core_1_backtracking_across_ancestors() {
        let doc = parse(
            r#"<div class="a">
                <div class="b">
                    <div class="b">
                        <p class="c">x</p>
                    </div>
                </div>
            </div>"#,
        );
        let root = doc.root();
        let results = query_all(&doc, root, &selectors(".a > .b .c"));
        assert_eq!(results.len(), 1);
        assert_eq!(doc.local_name(results[0]), Some("p"));
    }

    /// CORE-1: カンマ区切りのセレクタリストは記述順ではなく文書順で返り、
    /// 両方に一致する要素は 1 回だけ現れる（重複排除）。
    #[test]
    fn core_1_selector_list_document_order_without_duplicates() {
        let doc = parse("<h1 class=\"x\">t</h1><p class=\"x\">a</p><p>b</p>");
        let root = doc.root();
        let results = query_all(&doc, root, &selectors("p, h1"));
        let names: Vec<&str> = results
            .iter()
            .filter_map(|&id| doc.local_name(id))
            .collect();
        assert_eq!(names, vec!["h1", "p", "p"]);

        let dedup = query_all(&doc, root, &selectors("p, .x"));
        // <h1 class="x">・<p class="x">・<p>（class なし）が候補。
        // <p class="x"> は両方の枝に一致するが結果には 1 回のみ現れる。
        assert_eq!(dedup.len(), 3);
    }

    /// CORE-1: scope 自身は結果に含まれず、scope より上の祖先を使う結合子
    /// （`div span`）も一致する。
    #[test]
    fn core_1_scope_excludes_self_and_ancestor_combinator_reaches_outside_scope() {
        let doc = parse("<div><p><span>x</span></p></div>");
        let root = doc.root();
        let p = find_by_local_name(&doc, root, "p");

        // scope = p。`p` 自身は `p` セレクタの候補にならない。
        let self_matches = query_all(&doc, p, &selectors("p"));
        assert!(self_matches.is_empty());

        // `div span` は scope（p）の外側にある `div` を祖先として辿って一致する。
        let outside_ancestor = query_all(&doc, p, &selectors("div span"));
        assert_eq!(outside_ancestor.len(), 1);
        assert_eq!(doc.local_name(outside_ancestor[0]), Some("span"));
    }

    /// CORE-1: `<template>` の中身は root からの検索では返らないが、
    /// `template_contents` を scope に渡すと返る。
    #[test]
    fn core_1_template_contents_require_explicit_scope() {
        let doc = parse("<template><p class=\"in-template\">x</p></template>");
        let root = doc.root();

        let from_root = query_all(&doc, root, &selectors(".in-template"));
        assert!(from_root.is_empty());

        let template = find_by_local_name(&doc, root, "template");
        let contents = doc
            .template_contents(template)
            .expect("template contents が存在する");
        let from_contents = query_all(&doc, contents, &selectors(".in-template"));
        assert_eq!(from_contents.len(), 1);
    }

    /// CORE-1: HTML 名前空間の要素・属性は大文字小文字を無視して照合する。
    #[test]
    fn core_1_html_namespace_is_case_insensitive() {
        let doc = parse(r#"<DIV HREF="/x"></DIV>"#);
        let root = doc.root();

        let by_type = query_all(&doc, root, &selectors("div"));
        assert_eq!(by_type.len(), 1);

        let by_attr = query_all(&doc, root, &selectors("[href]"));
        assert_eq!(by_attr.len(), 1);
    }

    /// CORE-1: SVG 名前空間の要素・属性は完全一致でのみ照合する
    /// （大文字小文字混在の名前を大文字小文字無視にしない）。
    #[test]
    fn core_1_svg_namespace_is_case_sensitive() {
        let doc = parse(r#"<svg><foreignObject viewBox="0 0 1 1"></foreignObject></svg>"#);
        let root = doc.root();

        assert_eq!(query_all(&doc, root, &selectors("foreignObject")).len(), 1);
        assert!(query_all(&doc, root, &selectors("foreignobject")).is_empty());

        assert_eq!(query_all(&doc, root, &selectors("[viewBox]")).len(), 1);
        assert!(query_all(&doc, root, &selectors("[viewbox]")).is_empty());
    }

    /// CORE-1: ID・クラスの大文字小文字は quirks mode でのみ無視される
    /// （NoQuirks では区別、Quirks では無視）。
    #[test]
    fn core_1_id_and_class_case_sensitivity_depends_on_quirks_mode() {
        let no_quirks = parse("<!DOCTYPE html><div id=\"main\" class=\"Item\"></div>");
        assert_eq!(no_quirks.quirks_mode(), QuirksMode::NoQuirks);
        let root = no_quirks.root();
        assert!(query_all(&no_quirks, root, &selectors("#Main")).is_empty());
        assert!(query_all(&no_quirks, root, &selectors(".item")).is_empty());
        assert_eq!(query_all(&no_quirks, root, &selectors("#main")).len(), 1);

        let quirks = parse("<div id=\"main\" class=\"Item\"></div>");
        assert_eq!(quirks.quirks_mode(), QuirksMode::Quirks);
        let root = quirks.root();
        assert_eq!(query_all(&quirks, root, &selectors("#Main")).len(), 1);
        assert_eq!(query_all(&quirks, root, &selectors(".item")).len(), 1);
    }

    /// CORE-1: 一致がなければ `query_selector_all` は空、`query_selector` は
    /// `None` を返す。
    #[test]
    fn core_1_no_match_returns_empty_or_none() {
        let doc = parse("<div></div>");
        let root = doc.root();
        assert!(query_all(&doc, root, &selectors("span")).is_empty());
        assert_eq!(query_first(&doc, root, &selectors("span")), None);
    }

    /// CORE-1: 範囲外の `NodeId` を scope に渡しても空になる（panic しない）。
    #[test]
    fn core_1_out_of_range_scope_returns_empty() {
        let doc = parse("<div></div>");
        let out_of_range = NodeId::new(usize::MAX);
        assert!(query_all(&doc, out_of_range, &selectors("div")).is_empty());
        assert_eq!(query_first(&doc, out_of_range, &selectors("div")), None);
    }

    /// CORE-1: テキストノードを scope に渡しても空になる（子孫の要素がない）。
    #[test]
    fn core_1_text_node_scope_returns_empty() {
        let doc = parse("<p>hello</p>");
        let root = doc.root();
        let text_node = doc
            .descendants(root)
            .find(|&id| matches!(doc.node_data(id), Some(NodeData::Text { .. })))
            .expect("テキストノードが見つかる");
        assert!(query_all(&doc, text_node, &selectors("p")).is_empty());
    }

    /// CORE-1: `element_matches` は要素以外に対して `Ok(false)` を返す。
    #[test]
    fn core_1_element_matches_returns_false_for_non_element() {
        let doc = parse("<p>hello</p>");
        let root = doc.root();
        let text_node = doc
            .descendants(root)
            .find(|&id| matches!(doc.node_data(id), Some(NodeData::Text { .. })))
            .expect("テキストノードが見つかる");
        assert!(
            !element_matches(&doc, text_node, &selectors("p")).expect("キャッシュ上限に達しない")
        );
        assert!(
            !element_matches(&doc, NodeId::new(usize::MAX), &selectors("p"))
                .expect("キャッシュ上限に達しない")
        );

        let p = find_by_local_name(&doc, root, "p");
        assert!(element_matches(&doc, p, &selectors("p")).expect("キャッシュ上限に達しない"));
    }

    /// CORE-1: 結果は要素ノードのみで、テキスト・コメントは含まれない。
    #[test]
    fn core_1_results_contain_only_element_nodes() {
        let doc = parse("<div>text<!--comment--><p>x</p></div>");
        let root = doc.root();
        let all = query_all(&doc, root, &selectors("div, p"));
        assert!(all.iter().all(|&id| doc.is_element(id)));
    }

    /// CORE-1: 深いネスト（2 万段）でも `div span` の照合が完了し、
    /// スタックオーバーフローしない（件数を具体値で確認する）。
    #[test]
    fn core_1_deeply_nested_document_does_not_overflow_stack() {
        const DEPTH: usize = 20_000;
        let mut input = String::with_capacity(DEPTH * 5 + 20);
        for _ in 0..DEPTH {
            input.push_str("<div>");
        }
        input.push_str("<span>x</span>");
        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let doc = parse_document(&input, &options)
            .expect("深いネストでも成功する")
            .document;
        let root = doc.root();

        let results = query_all(&doc, root, &selectors("div span"));
        assert_eq!(results.len(), 1);
        assert_eq!(doc.local_name(results[0]), Some("span"));
    }

    /// CORE-1: `parent` リンクが循環している壊れた arena に対して照合しても、
    /// 無限ループに陥らず必ず終了する（[`Document::ancestors`] の
    /// `node_count` 上限打ち切りを [`any_ancestor_matches`] が継承すること
    /// の回帰テスト。Cursor Bugbot 指摘: 手動ループでは `node_count` 上限が
    /// なく循環 arena で無限ループし得た。レビュー指摘: PR #439
    /// discussion_r4111177871 — 以前の実装は循環を実際には構築せず通常の
    /// `parse` 結果を照合するだけだったため、この回帰を検出できなかった）。
    ///
    /// `parse_document` は循環を作らないため、通常の HTML 文書を構築した
    /// 後で `Document::nodes`（`pub(crate)`。同一 crate の `dom.rs` の
    /// `core_1_corrupted_cyclic_arena_traversal_terminates` と同じ手法）を
    /// 直接書き換え、`span` 要素の `parent` を自分自身に向けて実際に
    /// 循環させる。`any_ancestor_matches` はこの `span` を起点に
    /// `Document::ancestors` を辿るため、循環を実際に構築せずには
    /// `remaining_steps` 打ち切りへの依存を回帰させられない。
    #[test]
    fn core_1_cyclic_parent_link_does_not_infinite_loop() {
        let mut doc = parse("<div><p><span>x</span></p></div>");
        let root = doc.root();
        let span = find_by_local_name(&doc, root, "span");
        // `span` の親を自分自身に書き換え、実際に循環した `parent` リンクを
        // 作る（`nodes`/`Node::parent`/`NodeId::index` はいずれも
        // `pub(crate)` で同一 crate から直接操作できる）。
        doc.nodes[span.index()].parent = Some(span);

        let started = std::time::Instant::now();
        // `span` からの祖先探索は循環に陥るため `div` へ到達できず、
        // 一致件数は 0 になる。ここで確認したいのは件数そのものより、
        // `node_count` 打ち切りにより有限時間で `Ok` が返ることである。
        let results = query_all(&doc, root, &selectors("div span"));
        assert_eq!(results.len(), 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// CORE-1: 子孫結合子（`Combinator::Descendant`）を
    /// [`crate::selector::MAX_COMPOUNDS_PER_COMPLEX`] まで連ねたセレクタを
    /// 深くネストした（かつ左端で必ず不一致になる）文書に照合しても、
    /// `matches_compound_chain` のメモ化により祖先の組み合わせを
    /// 指数的に探索せず短時間で完了する（DoS 対策の回帰テスト。
    /// レビュー指摘: PR #439 discussion_r4111107367）。メモ化なしの実装では
    /// この規模でも実用的な時間内に終わらない。
    #[test]
    fn core_1_descendant_backtracking_does_not_explode_with_deep_nesting() {
        const DEPTH: usize = 2_000;
        // 子孫結合子を最大数まで連ね、先頭（左端）を文書中に存在しない
        // クラス名にすることで、素朴なバックトラック実装だと不一致確定
        // までに祖先の組み合わせを総当たりしてしまう入力にする。
        let compound_count = crate::selector::MAX_COMPOUNDS_PER_COMPLEX;
        let selector_text = std::iter::once(".missing")
            .chain(std::iter::repeat_n("div", compound_count - 1))
            .collect::<Vec<_>>()
            .join(" ");

        let mut input = String::with_capacity(DEPTH * 5 + 20);
        for _ in 0..DEPTH {
            input.push_str("<div>");
        }
        input.push_str("<span>x</span>");
        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let doc = parse_document(&input, &options)
            .expect("深いネストでも成功する")
            .document;
        let root = doc.root();

        let started = std::time::Instant::now();
        let results = query_all(&doc, root, &selectors(&selector_text));
        // `.missing` が文書中に存在しないため必ず不一致になる。
        assert_eq!(results.len(), 0);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "メモ化なしの指数的バックトラックが疑われる（所要時間: {:?}）",
            started.elapsed()
        );

        // 常に `false` を返すだけの壊れたメモ化ではこのテストを検出できない
        // ため、実際に一致が発生するケースも同じ規模で確認する（先頭を
        // `div`（文書中に存在する）にし、`.missing` を使わない）。
        let matching_selector = vec!["div"; compound_count].join(" ");
        let matching_results = query_all(&doc, root, &selectors(&matching_selector));
        // 深さ `DEPTH` の `div` の連なりのうち、`compound_count` 個の `div`
        // 連鎖を子孫方向に満たせるのは、根から `compound_count - 1` 個目
        // より深い各 `div`（`DEPTH - (compound_count - 1)` 個）である。
        assert_eq!(matching_results.len(), DEPTH - (compound_count - 1));
    }

    /// CORE-1: `MatchCache` の合計エントリ数が [`MAX_MATCH_CACHE_ENTRIES`]
    /// を超える場合、`query_selector_all` は一律 `false`/空へフォールバック
    /// せず `Error::MatchCacheLimitExceeded` を返す（PR #439 レビュー指摘
    /// P0: メモリ使用量に実効上限がなかった）。テストを高速に保つため、
    /// 一時的に極端に小さい上限（`test_with_max_cache_entries`）で判定条件
    /// のみを検証する。
    #[test]
    fn core_1_match_cache_limit_exceeded_returns_explicit_error() {
        // `MAX_MATCH_CACHE_ENTRIES`（100 万）に到達させる実データを毎回
        // 生成するのは重いため、`MatchCache` の上限判定ロジック単体
        // （`would_exceed_limit`）を直接検証する（`pub(self)` の内部関数
        // へのユニットテストで、公開 API の契約を裏付ける）。
        let mut cache = MatchCache::new();
        for i in 0..MAX_MATCH_CACHE_ENTRIES {
            cache
                .insert_chain((0, i, None), true)
                .expect("上限に達するまでは挿入できる");
        }
        assert_eq!(cache.len(), MAX_MATCH_CACHE_ENTRIES);

        let err = cache
            .insert_chain((0, MAX_MATCH_CACHE_ENTRIES, None), true)
            .expect_err("上限超過後の新規キー挿入は拒否される");
        assert!(matches!(
            err,
            Error::MatchCacheLimitExceeded { limit } if limit == MAX_MATCH_CACHE_ENTRIES
        ));

        // 既存キーの上書きは合計エントリ数を増やさないため許可される。
        cache
            .insert_chain((0, 0, None), false)
            .expect("既存キーの上書きは上限に関わらず成功する");
    }

    /// CORE-1: `&str` 版は `parse_selector_list` のエラーをそのまま返す。
    #[test]
    fn core_1_str_variants_propagate_parse_errors() {
        let doc = parse("<div></div>");
        let root = doc.root();

        let err = query_selector_all_str(&doc, root, "a:hover")
            .expect_err("疑似クラスは Unsupported のはず");
        assert!(matches!(err, Error::Unsupported { .. }));

        let err = query_selector_str(&doc, root, "").expect_err("空文字列は InvalidInput のはず");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-1: `&str` 版は正常系で `query_selector_all`/`query_selector` と
    /// 同じ結果を返す。
    #[test]
    fn core_1_str_variants_match_typed_variants_on_success() {
        let doc = parse("<ul><li>a</li><li>b</li></ul>");
        let root = doc.root();

        let typed = query_all(&doc, root, &selectors("li"));
        let from_str = query_selector_all_str(&doc, root, "li").expect("li は解析できるはず");
        assert_eq!(typed, from_str);

        let typed_first = query_first(&doc, root, &selectors("li"));
        let from_str_first = query_selector_str(&doc, root, "li").expect("li は解析できるはず");
        assert_eq!(typed_first, from_str_first);
    }

    /// テスト用ヘルパー: `results` に含まれる各要素の `id` 属性値を文書順に
    /// 並べて返す（TASK-24（24.8）・MS-1・Issue #42）。件数だけの assert ではなく
    /// 具体的な id 列で結果を比較できるようにする。`id` を持たない要素は
    /// 番兵文字列 `"<none>"` にする（実在の id 値と衝突しない前提で、
    /// テスト fixture では常に検索対象の要素に一意な id を振る）。
    fn ids<'a>(doc: &'a Document, results: &[NodeId]) -> Vec<&'a str> {
        results
            .iter()
            .map(|&id| doc.attribute(id, "id").unwrap_or("<none>"))
            .collect()
    }

    /// テスト用ヘルパー: `(セレクタ, 期待する id 列)` の表を順に検証する
    /// （TASK-24（24.8）・MS-1・Issue #42）。失敗時にどのセレクタで落ちたかが
    /// わかるよう、`assert_eq!` にセレクタ文字列を含める。
    fn assert_query_ids(doc: &Document, scope: NodeId, cases: &[(&str, &[&str])]) {
        for &(selector_text, expected) in cases {
            let results = query_all(doc, scope, &selectors(selector_text));
            let actual = ids(doc, &results);
            assert_eq!(
                actual, expected,
                "selector {selector_text:?} の結果が一致しない"
            );
        }
    }

    /// CORE-1: 型セレクタは一致する要素すべてを文書順で返し、存在しない
    /// 型名は空になる（TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_type_selector_match_and_mismatch() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <ul>
                <li id="a">a</li>
                <li id="b">b</li>
                <li id="c">c</li>
            </ul>"#,
        );
        let root = doc.root();
        assert_query_ids(&doc, root, &[("li", &["a", "b", "c"]), ("article", &[])]);
    }

    /// CORE-1: ID セレクタは一致する id を持つ要素だけを返し、他の id・
    /// 存在しない id は一致しない。同じ id が複数の要素に付いている場合、
    /// `query_selector_all` は両方を文書順で返し、`query_selector` は
    /// 先頭の要素を返す（TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_id_selector_match_and_mismatch() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <div id="main">
                <span id="dup">x</span>
                <span id="dup">y</span>
            </div>"#,
        );
        let root = doc.root();

        assert_query_ids(&doc, root, &[("#main", &["main"]), ("#other", &[])]);

        let dup_all = query_all(&doc, root, &selectors("#dup"));
        assert_eq!(ids(&doc, &dup_all), vec!["dup", "dup"]);
        let texts: Vec<String> = dup_all.iter().map(|&id| text(&doc, id)).collect();
        assert_eq!(texts, vec!["x", "y"]);

        let dup_first =
            query_first(&doc, root, &selectors("#dup")).expect("先頭の #dup が見つかるはず");
        assert_eq!(text(&doc, dup_first), "x");
    }

    /// CORE-1: クラスセレクタはトークン単位で照合する（前方一致では
    /// 一致しない・空白区切りのクラス値のどのトークンにも一致する・複合
    /// クラスセレクタは全クラスを持つ要素だけに一致する。TASK-24（24.8）・
    /// MS-1・Issue #42）。
    #[test]
    fn core_1_class_selector_token_semantics() {
        let doc = parse(
            "<!DOCTYPE html>\n\
             <div id=\"both\" class=\"link primary\"></div>\n\
             <div id=\"onlylink\" class=\"link\"></div>\n\
             <div id=\"whitespace\" class=\"a\tb\nc\"></div>\n\
             <div id=\"upper\" class=\"Primary\"></div>",
        );
        let root = doc.root();

        assert_query_ids(
            &doc,
            root,
            &[
                (".primary", &["both"]),
                // `.lin` は `link` の前方一致に過ぎず、クラストークンとして
                // 一致しないため空になる。
                (".lin", &[]),
                (".link.primary", &["both"]),
                (".b", &["whitespace"]),
                (".c", &["whitespace"]),
                // NoQuirks（doctype あり）なのでクラス名の大文字小文字を
                // 区別し、`.Primary` の要素は `.primary` に一致しない。
                (".Primary", &["upper"]),
            ],
        );
    }

    /// CORE-1: 属性の有無セレクタ（`[href]`）は同じタグでも属性を持つ要素
    /// だけを返す（TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_attribute_exists_match_and_mismatch() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <a id="withhref" href="/x">link</a>
            <a id="withouthref">no href</a>"#,
        );
        let root = doc.root();
        assert_query_ids(&doc, root, &[("[href]", &["withhref"])]);
    }

    /// CORE-1: 属性値の完全一致セレクタ（`[name=value]`）は引用符の有無に
    /// 関わらず同じ結果になり、値が異なる要素・大文字小文字が異なる値には
    /// 一致しない（値の比較は大文字小文字を区別するという現状の契約の
    /// 固定。query.rs モジュール doc の「属性値の完全一致」節参照。
    /// REPAIR-3: HTML 仕様上の大文字小文字無視属性への対応はスコープ外
    /// のまま。TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_attribute_equals_match_and_mismatch() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <input id="checkbox" type="checkbox">
            <input id="textinput" type="text">
            <div id="emptydata" data-x=""></div>
            <div id="titled" title="a b"></div>"#,
        );
        let root = doc.root();

        let unquoted = ids(&doc, &query_all(&doc, root, &selectors("[type=checkbox]")));
        let double_quoted = ids(
            &doc,
            &query_all(&doc, root, &selectors("[type=\"checkbox\"]")),
        );
        let single_quoted = ids(
            &doc,
            &query_all(&doc, root, &selectors("[type='checkbox']")),
        );
        assert_eq!(unquoted, vec!["checkbox"]);
        assert_eq!(unquoted, double_quoted);
        assert_eq!(unquoted, single_quoted);

        assert_query_ids(
            &doc,
            root,
            &[
                ("[type=text]", &["textinput"]),
                ("[data-x=\"\"]", &["emptydata"]),
                ("[title=\"a b\"]", &["titled"]),
                // 属性値の大文字小文字は区別するため、実際の値と大文字違い
                // の `CHECKBOX` は一致しない。
                ("[type=CHECKBOX]", &[]),
            ],
        );
    }

    /// CORE-1: 複合セレクタは構成する単純セレクタ・型名がすべて一致した
    /// 要素だけに一致し、いずれか 1 つでも満たさなければ空になる
    /// （TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_compound_selector_partial_failure() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <div id="main" class="container">
                <a id="match" class="link" href="/x">a</a>
            </div>"#,
        );
        let root = doc.root();
        assert_query_ids(
            &doc,
            root,
            &[
                ("a.link[href]", &["match"]),
                // 型名が異なる。
                ("span.link[href]", &[]),
                // 持っていないクラス。
                ("a.missing", &[]),
                // 持っていない属性。
                ("a.link[target]", &[]),
                // 型名・クラスは一致するが id のみ異なるため、id 不一致
                // だけを理由に空になることを検証する
                // （`div` は一致・`.container` も持つが `#other` を持たない）。
                ("div#other.container", &[]),
            ],
        );
    }

    /// CORE-1: 子結合子（`div > span`）は直接の子にしか一致せず、孫要素
    /// には一致しない。子孫結合子（`div span`）は孫要素にも一致する
    /// （TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_child_combinator_rejects_grandchild() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <div><section><span id="grandchild">x</span></section></div>"#,
        );
        let root = doc.root();
        assert_query_ids(
            &doc,
            root,
            &[("div > span", &[]), ("div span", &["grandchild"])],
        );
    }

    /// CORE-1: セレクタリストに複数の複合セレクタが混在していても、
    /// `query_selector` は記述順ではなく文書順で最初に現れる要素を返す
    /// （TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_query_selector_returns_first_in_document_order() {
        let doc = parse(r#"<!DOCTYPE html><h1 id="heading">t</h1><p id="para">a</p>"#);
        let root = doc.root();
        // セレクタの記述順は `p, h1` だが、文書順で先に現れるのは `h1`。
        let first = query_first(&doc, root, &selectors("p, h1")).expect("一致するはず");
        assert_eq!(doc.attribute(first, "id"), Some("heading"));
    }

    /// CORE-1: `element_matches` はセレクタの種類ごとに true / false の両方
    /// を具体的に確認する（TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_element_matches_per_selector_kind() {
        let doc = parse(
            r#"<!DOCTYPE html>
            <div id="wrap"><a id="t" class="link" href="/x">link</a></div>"#,
        );
        let root = doc.root();
        let a = find_by_local_name(&doc, root, "a");

        let cases: &[(&str, bool)] = &[
            ("a", true),
            ("span", false),
            ("#t", true),
            ("#u", false),
            (".link", true),
            (".other", false),
            ("[href]", true),
            ("[target]", false),
            ("[href=\"/x\"]", true),
            ("[href=\"/y\"]", false),
            ("div > a", true),
            ("section > a", false),
            ("div a", true),
            ("section a", false),
            ("span, a", true),
        ];
        for &(selector_text, expected) in cases {
            let actual = element_matches(&doc, a, &selectors(selector_text))
                .expect("キャッシュ上限に達しない");
            assert_eq!(
                actual, expected,
                "selector {selector_text:?} の element_matches が一致しない"
            );
        }
    }

    /// CORE-1・PoC-2 移植: クラスセレクタで一致がない場合
    /// `query_selector` は `None` を返す（`docs/spec` の PoC-2
    /// `select_first_returns_none_when_no_match` 相当。型セレクタ版は
    /// `core_1_no_match_returns_empty_or_none` で確認済みのため、ここでは
    /// クラスセレクタ版を追加する。TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_poc2_select_first_returns_none_for_class_selector() {
        let doc = parse("<div></div>");
        let root = doc.root();
        assert_eq!(query_first(&doc, root, &selectors(".missing")), None);
    }

    /// CORE-1・PoC-2 移植: `query_selector` で見つけた要素から `dom` の
    /// アクセサ（`local_name`・`attribute`・`class_names`・`text_content`）
    /// で属性・テキストを取り出せる（`docs/spec` の PoC-2
    /// `select_extracts_attributes` 相当。PoC の `ElementInfo` のような
    /// 高レベル helper は query モジュールのスコープ外なので、dom
    /// アクセサを直接組み合わせて確認する。TASK-24（24.8）・MS-1・Issue #42）。
    #[test]
    fn core_1_poc2_select_extracts_attributes() {
        let doc = parse(r#"<a href="https://example.com" class="link">click</a>"#);
        let root = doc.root();
        let a = query_first(&doc, root, &selectors("a")).expect("a 要素が見つかるはず");

        assert_eq!(doc.local_name(a), Some("a"));
        assert_eq!(doc.attribute(a, "href"), Some("https://example.com"));
        let classes: Vec<&str> = doc.class_names(a).collect();
        assert_eq!(classes, vec!["link"]);
        assert_eq!(text(&doc, a), "click");
    }

    /// CORE-1・PoC-2 移植: 不正なセレクタ構文は `query_selector_all_str`・
    /// `parse_selector_list` の両方でエラーになる（`docs/spec` の PoC-2
    /// `compile_selector_rejects_invalid_syntax` 相当）。先頭の `:` は
    /// 疑似クラスの分岐に入るため `Error::Unsupported` になる（現状の
    /// 実装の挙動を `matches!` で具体的に固定する。TASK-24（24.8）・
    /// MS-1・Issue #42）。
    #[test]
    fn core_1_poc2_compile_selector_rejects_invalid_syntax() {
        let doc = parse("<div></div>");
        let root = doc.root();

        let err = query_selector_all_str(&doc, root, ":::not-a-selector:::")
            .expect_err("不正なセレクタ構文はエラーになるはず");
        assert!(matches!(err, Error::Unsupported { .. }));

        let err = parse_selector_list(":::not-a-selector:::")
            .expect_err("不正なセレクタ構文はエラーになるはず");
        assert!(matches!(err, Error::Unsupported { .. }));
    }
}
