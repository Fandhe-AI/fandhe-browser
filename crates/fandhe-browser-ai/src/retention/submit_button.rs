//! フォーム送信ボタンの DOM 検出（`AISNAP-12`・`TASK-16.3`・`MS-2`・Issue #106）。
//!
//! 役割: 親モジュール [`super`]（選択層）は index しか扱わないため、本モジュールが
//! 一覧・表の各項目（`li`・`tr` 等）のサブツリーから「フォームの送信ボタン」を検出し、
//! 優先候補（[`PriorityCandidate`]）へ変換する。TASK-16.4（Issue #107）で
//! `compress_table::compress_rows` が `submit_button_candidates(doc, &structure.body_rows)`
//! の結果を [`select_retained_with_priority`](super::select_retained_with_priority) へ
//! 渡す想定で、現時点の呼び出し元はテストのみ（統合前）。ページネーション候補との
//! 合成（連結して選択層へ渡すこと）も TASK-16.4 の責務である。
//!
//! # 判定規則
//!
//! 対象は HTML 名前空間の次の要素で、非表示（`hidden`・`aria-hidden="true"`）でなく、
//! フォームオーナーを持つもの。
//!
//! - `<button>`: `type` が省略・`submit`・無効値（Auto 状態）なら送信ボタン。ただし
//!   WHATWG の定義どおり、Auto 状態で `command` または `commandfor` を持つものは
//!   送信ボタンではない。`reset`・`button` は対象外
//! - `<input type=submit>`・`<input type=image>`
//!
//! `type` は ASCII 大文字小文字を区別せず完全一致で照合し、前後の空白は除去しない
//! （`type=" submit "` は無効値）。`disabled` の送信ボタンも対象に含める。有効・無効の
//! 切り替えで簡約表現から出入りすると参照が不安定になるため（`AISNAP-12` の目的）。
//!
//! フォームオーナーは、祖先に HTML の `<form>` があるか、空でない `form` 属性を持つこと。
//! core に id から要素を引く API が無いため `form=` の参照先は解決しない。誤検出は
//! 保持枠を 1 つ使うだけ、未検出は文書順の充填へ劣化するだけで、どちらも安全側である。
//!
//! # 上限（security.md「不安全な設計」）
//!
//! ネットワーク由来の HTML を走査するため、属性値は複製・正規化せず（`type` は
//! `eq_ignore_ascii_case`、`form` は空判定のみ）、項目サブツリー走査
//! （`MAX_ITEM_SCAN_STEPS`）・祖先探索（`MAX_FORM_ANCESTOR_DEPTH`）を定数上限で
//! 打ち切り、明示スタックで再帰しない。[`submit_button_candidates`] の確保量は
//! `items.len()` 以下、総走査量は `items.len()` × 定数で上限が決まる。

use super::{PriorityCandidate, RetentionReason};
use crate::snapshot::name::{SKIPPED_SUBTREES, is_hidden_element};
use fandhe_browser_core::dom::{Document, NodeId};

/// HTML 名前空間の URI（`core` 側の定義が `pub(crate)` のためローカルに持つ）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// 項目サブツリー走査で訪問するノード数の上限（訪問済み + 未訪問スタックの総量）。
const MAX_ITEM_SCAN_STEPS: usize = 256;
/// フォームオーナー探索で辿る祖先の最大深さ。超えた先の `<form>` は無いものとして扱う。
const MAX_FORM_ANCESTOR_DEPTH: usize = 32;
/// 項目の祖先に非表示要素が無いか確認する最大深さ。超えた先は表示扱いとする。
const MAX_HIDDEN_ANCESTOR_DEPTH: usize = 256;

/// 送信ボタンの種別（`AISNAP-12`・`TASK-16.3`）。
///
/// 真偽値でなく拡張可能な型で根拠を返す（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubmitButtonKind {
    /// `<button>`（`type` 省略・`submit`・無効値）。
    Button,
    /// `<input type=submit>`。
    InputSubmit,
    /// `<input type=image>`。
    InputImage,
}

fn is_html_named(doc: &Document, id: NodeId, name: &str) -> bool {
    doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && doc
            .local_name(id)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
}

/// `<button>` の送信可否（Auto 状態の判定込み）。属性値は複製しない。
fn button_submits(doc: &Document, id: NodeId) -> bool {
    match doc.attribute(id, "type") {
        Some(t) if t.eq_ignore_ascii_case("submit") => return true,
        Some(t) if t.eq_ignore_ascii_case("reset") || t.eq_ignore_ascii_case("button") => {
            return false;
        }
        // 省略・無効値は Auto 状態。
        _ => {}
    }
    // Auto 状態では command / commandfor を持つ button は送信しない（WHATWG）。
    doc.attribute(id, "command").is_none() && doc.attribute(id, "commandfor").is_none()
}

/// フォームオーナーを持つか（祖先の `<form>` または空でない `form` 属性）。
fn has_form_owner(doc: &Document, id: NodeId) -> bool {
    if doc.attribute(id, "form").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    doc.ancestors(id)
        .take(MAX_FORM_ANCESTOR_DEPTH)
        .any(|a| doc.is_element(a) && is_html_named(doc, a, "form"))
}

/// 単一要素がフォームの送信ボタンか判定する（`AISNAP-12`・`TASK-16.3`）。
///
/// 範囲外の `NodeId`・非要素・HTML 以外の名前空間・非表示要素・フォームオーナーの
/// 無い要素は `None`。規則の詳細はモジュール doc を参照。
pub fn classify_submit_button(doc: &Document, id: NodeId) -> Option<SubmitButtonKind> {
    if !doc.is_element(id) || is_hidden_element(doc, id) {
        return None;
    }
    let kind = if is_html_named(doc, id, "button") {
        if !button_submits(doc, id) {
            return None;
        }
        SubmitButtonKind::Button
    } else if is_html_named(doc, id, "input") {
        let t = doc.attribute(id, "type")?;
        if t.eq_ignore_ascii_case("submit") {
            SubmitButtonKind::InputSubmit
        } else if t.eq_ignore_ascii_case("image") {
            SubmitButtonKind::InputImage
        } else {
            return None;
        }
    } else {
        return None;
    };
    has_form_owner(doc, id).then_some(kind)
}

/// 項目（`li`・`tr` 等）のサブツリーから送信ボタンを探す（`AISNAP-12`・`TASK-16.3`）。
///
/// 項目自身も対象に含む。項目自身・祖先（`MAX_HIDDEN_ANCESTOR_DEPTH` まで）が非表示なら
/// `None`。非表示・`script`/`style`/`noscript`/`template` のサブツリーは読み飛ばし、
/// 訪問済み + 未訪問の総量を内部上限（256 ノード）以内に保つ。子が残り予算を超える場合は
/// 文書順で予算内の子だけを調べ、超過分は打ち切る。複数ある場合は文書順で最初の種別を返す。
pub fn find_submit_button(doc: &Document, item: NodeId) -> Option<SubmitButtonKind> {
    if doc
        .ancestors(item)
        .take(MAX_HIDDEN_ANCESTOR_DEPTH)
        .any(|a| doc.is_element(a) && is_hidden_element(doc, a))
    {
        return None;
    }
    let mut stack = vec![item];
    let mut steps = 0usize;
    while let Some(id) = stack.pop() {
        steps += 1;
        if steps > MAX_ITEM_SCAN_STEPS {
            return None;
        }
        if !doc.is_element(id) {
            continue;
        }
        if is_hidden_element(doc, id)
            || (id != item && SKIPPED_SUBTREES.iter().any(|n| is_html_named(doc, id, n)))
        {
            continue;
        }
        if let Some(kind) = classify_submit_button(doc, id) {
            return Some(kind);
        }
        let budget = MAX_ITEM_SCAN_STEPS.saturating_sub(steps + stack.len());
        // 予算超過分の子は打ち切る（先頭側の子は文書順で必ず調べる）。
        let mut kids: Vec<NodeId> = doc.children(id).take(budget).collect();
        kids.reverse();
        stack.extend(kids);
    }
    None
}

/// 各項目を走査し、送信ボタンを含む項目の優先候補を index 昇順で返す
/// （`AISNAP-12`・`TASK-16.3`）。
///
/// 理由は [`RetentionReason::SubmitButton`]。ページネーション候補との合成は
/// TASK-16.4 で呼び出し側が行う。
pub fn submit_button_candidates(doc: &Document, items: &[NodeId]) -> Vec<PriorityCandidate> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| find_submit_button(doc, **item).is_some())
        .map(|(index, _)| PriorityCandidate::new(index, RetentionReason::SubmitButton))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FORM_ANCESTOR_DEPTH, SubmitButtonKind, classify_submit_button, find_submit_button,
        submit_button_candidates,
    };
    use crate::compress_table::detect_regular_structure;
    use crate::retention::{
        PriorityCandidate, RetentionPolicy, RetentionReason, select_retained,
        select_retained_with_priority,
    };
    use fandhe_browser_core::dom::{Document, NodeId};
    use fandhe_browser_core::parse::{ParseOptions, parse_document};
    use fandhe_browser_core::query::query_selector_str;

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn select(doc: &Document, selector: &str) -> NodeId {
        query_selector_str(doc, doc.root(), selector)
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる")
    }

    /// `<form>` 内の単一要素を判定する。
    fn classify_in_form(inner: &str, selector: &str) -> Option<SubmitButtonKind> {
        let doc = parse(&format!("<form>{inner}</form>"));
        classify_submit_button(&doc, select(&doc, selector))
    }

    fn indexes_of(cands: &[PriorityCandidate]) -> Vec<usize> {
        cands.iter().map(|c| c.index).collect()
    }

    /// AISNAP-12（TASK-16.3・Issue #106）: キャップ外の行の送信ボタンが保持される。
    #[test]
    fn aisnap_12_submit_button_row_kept_beyond_cap() {
        let mut html = String::from("<form><ul>");
        for i in 0..30 {
            if i == 29 {
                html.push_str("<li>item29 <button>Send</button></li>");
            } else {
                html.push_str(&format!("<li>item{i}</li>"));
            }
        }
        html.push_str("</ul></form>");
        let doc = parse(&html);
        let det = detect_regular_structure(&doc, select(&doc, "ul"));
        let structure = det.as_regular().expect("規則的な一覧");
        let cands = submit_button_candidates(&doc, &structure.body_rows);
        assert_eq!(indexes_of(&cands), vec![29]);

        let policy = RetentionPolicy::new(20, 5);
        let plain = select_retained(structure.body_rows.len(), &policy);
        assert!(plain.kept.iter().all(|i| i.index != 29));

        let r = select_retained_with_priority(structure.body_rows.len(), &policy, &cands);
        let by = |reason: RetentionReason| -> Vec<usize> {
            r.kept
                .iter()
                .filter(|i| i.reason == reason)
                .map(|i| i.index)
                .collect()
        };
        assert_eq!(by(RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert_eq!(by(RetentionReason::SubmitButton), vec![29]);
        assert_eq!(by(RetentionReason::Fill), (5..19).collect::<Vec<_>>());
        assert_eq!(r.kept.len(), 20);
        assert_eq!(r.omitted, 10);
        let idx: Vec<usize> = r.kept.iter().map(|i| i.index).collect();
        assert!(idx.windows(2).all(|w| w[0] < w[1]));
        let rows = r.apply(&structure.body_rows);
        assert_eq!(
            **rows.last().expect("末尾要素がある"),
            structure.body_rows[29]
        );
    }

    /// AISNAP-12: 各項目の中に `<form>` がある場合も保持される。
    #[test]
    fn aisnap_12_form_nested_in_item_kept() {
        let mut html = String::from("<ul>");
        for i in 0..30 {
            if i == 27 {
                html.push_str("<li><form><input type=submit value=Go></form></li>");
            } else {
                html.push_str(&format!("<li><form>item{i}</form></li>"));
            }
        }
        html.push_str("</ul>");
        let doc = parse(&html);
        let det = detect_regular_structure(&doc, select(&doc, "ul"));
        let structure = det.as_regular().expect("規則的な一覧");
        let cands = submit_button_candidates(&doc, &structure.body_rows);
        assert_eq!(indexes_of(&cands), vec![27]);
        let r = select_retained_with_priority(30, &RetentionPolicy::new(20, 5), &cands);
        assert!(r.kept.iter().any(|i| i.index == 27));
    }

    /// AISNAP-12: `<button>` の type 判定。
    #[test]
    fn aisnap_12_button_type_rules() {
        for inner in [
            "<button>x</button>",
            "<button type=submit>x</button>",
            "<button type=SUBMIT>x</button>",
            "<button type=foo>x</button>",
            "<button type=' submit '>x</button>",
        ] {
            assert_eq!(
                classify_in_form(inner, "button"),
                Some(SubmitButtonKind::Button),
                "{inner}"
            );
        }
        for inner in [
            "<button type=reset>x</button>",
            "<button type=button>x</button>",
        ] {
            assert_eq!(classify_in_form(inner, "button"), None, "{inner}");
        }
    }

    /// AISNAP-12: Auto 状態の command / commandfor 付き button は送信しない（WHATWG）。
    #[test]
    fn aisnap_12_button_with_command_attrs_is_not_submit() {
        assert_eq!(
            classify_in_form("<button commandfor=d>x</button>", "button"),
            None
        );
        assert_eq!(
            classify_in_form("<button command=show-modal>x</button>", "button"),
            None
        );
        assert_eq!(
            classify_in_form("<button type=submit commandfor=d>x</button>", "button"),
            Some(SubmitButtonKind::Button)
        );
    }

    /// AISNAP-12: `<input>` の type 判定。
    #[test]
    fn aisnap_12_input_type_rules() {
        assert_eq!(
            classify_in_form("<input type=submit>", "input"),
            Some(SubmitButtonKind::InputSubmit)
        );
        assert_eq!(
            classify_in_form("<input type=IMAGE>", "input"),
            Some(SubmitButtonKind::InputImage)
        );
        for inner in [
            "<input type=' submit '>",
            "<input type=text>",
            "<input type=reset>",
            "<input>",
        ] {
            assert_eq!(classify_in_form(inner, "input"), None, "{inner}");
        }
    }

    /// AISNAP-12: フォームオーナーの有無。
    #[test]
    fn aisnap_12_form_owner_required() {
        let doc = parse("<div><button>a</button></div>");
        assert_eq!(classify_submit_button(&doc, select(&doc, "button")), None);
        let doc = parse("<div><button form=f1>a</button></div>");
        assert_eq!(
            classify_submit_button(&doc, select(&doc, "button")),
            Some(SubmitButtonKind::Button)
        );
        let doc = parse("<div><button form=''>a</button></div>");
        assert_eq!(classify_submit_button(&doc, select(&doc, "button")), None);
    }

    /// AISNAP-12: 非表示・template 内・非 HTML・非要素・範囲外は検出しない。
    #[test]
    fn aisnap_12_hidden_and_foreign_are_ignored() {
        let doc = parse("<form><button hidden>a</button></form>");
        assert_eq!(classify_submit_button(&doc, select(&doc, "button")), None);
        let doc = parse("<form><button aria-hidden=true>a</button></form>");
        assert_eq!(classify_submit_button(&doc, select(&doc, "button")), None);
        let doc = parse("<form><div hidden><button>a</button></div></form>");
        assert_eq!(find_submit_button(&doc, select(&doc, "form")), None);
        let doc = parse("<form><template><button>a</button></template></form>");
        assert_eq!(find_submit_button(&doc, select(&doc, "form")), None);
        let doc = parse("<form><svg><button>a</button></svg></form>");
        assert_eq!(find_submit_button(&doc, select(&doc, "form")), None);

        let doc = parse("<form><button>a</button></form>");
        assert_eq!(classify_submit_button(&doc, doc.root()), None);
        // 大きな文書の NodeId を小さな文書へ渡しても panic せず None になる。
        let big = parse(&format!("<form>{}</form>", "<button>a</button>".repeat(50)));
        let last = big
            .children(select(&big, "form"))
            .next_back()
            .expect("子が存在する");
        assert_eq!(classify_submit_button(&doc, last), None);
    }

    /// AISNAP-12: disabled の送信ボタンも保持対象（状態変化で参照を揺らさない）。
    #[test]
    fn aisnap_12_disabled_submit_is_included() {
        assert_eq!(
            classify_in_form("<button disabled>x</button>", "button"),
            Some(SubmitButtonKind::Button)
        );
    }

    /// AISNAP-12: 巨大な属性値でも panic せず無効値として扱う。
    #[test]
    fn aisnap_12_huge_attribute_value_is_bounded() {
        let big = "a".repeat(1_000_000);
        let inner = format!("<button type='{big}'>x</button>");
        assert_eq!(
            classify_in_form(&inner, "button"),
            Some(SubmitButtonKind::Button)
        );
    }

    /// AISNAP-12: 幅広の兄弟があっても、予算内の先頭側の送信ボタンは見つける。
    #[test]
    fn aisnap_12_wide_children_keep_leading_button() {
        let mut html = String::from("<form><div><button>first</button>");
        for _ in 0..500 {
            html.push_str("<span>x</span>");
        }
        html.push_str("</div></form>");
        let doc = parse(&html);
        assert_eq!(
            find_submit_button(&doc, select(&doc, "div")),
            Some(SubmitButtonKind::Button)
        );

        // 先行する兄弟のボタンは、後続の幅広要素に妨げられず見つかる。
        let mut html = String::from("<form><div><p><button>a</button></p><section>");
        for _ in 0..500 {
            html.push_str("<span>x</span>");
        }
        html.push_str("</section></div></form>");
        let doc = parse(&html);
        assert_eq!(
            find_submit_button(&doc, select(&doc, "div")),
            Some(SubmitButtonKind::Button)
        );
    }

    /// AISNAP-12: 非表示の祖先を持つ項目は送信ボタン候補にしない。
    #[test]
    fn aisnap_12_hidden_ancestor_of_item_is_ignored() {
        let doc = parse("<form><ul hidden><li><button>a</button></li></ul></form>");
        assert_eq!(find_submit_button(&doc, select(&doc, "li")), None);
        let doc = parse(
            "<form><div aria-hidden=true><table><tr><td><button>a</button></td></tr></table></div></form>",
        );
        assert_eq!(find_submit_button(&doc, select(&doc, "tr")), None);
        let doc = parse("<form><ul><li><button>a</button></li></ul></form>");
        assert_eq!(
            find_submit_button(&doc, select(&doc, "li")),
            Some(SubmitButtonKind::Button)
        );
    }

    /// AISNAP-12: 子が多い・深いネストでも有界に終了し、上限先のボタンは見つけない。
    #[test]
    fn aisnap_12_scan_is_bounded() {
        let mut html = String::from("<form><div>");
        for _ in 0..500 {
            html.push_str("<span>x</span>");
        }
        html.push_str("<button>late</button></div></form>");
        let doc = parse(&html);
        assert_eq!(find_submit_button(&doc, select(&doc, "div")), None);

        let mut html = String::from("<form>");
        for _ in 0..2000 {
            html.push_str("<div>");
        }
        html.push_str("<button>deep</button></form>");
        let doc = parse(&html);
        assert_eq!(find_submit_button(&doc, select(&doc, "form")), None);
    }

    /// AISNAP-12: 祖先の深さが上限を超える位置の `<form>` はオーナーとみなさない。
    #[test]
    fn aisnap_12_form_ancestor_depth_is_bounded() {
        let depth = MAX_FORM_ANCESTOR_DEPTH + 10;
        let html = format!(
            "<form>{}<button>x</button>{}</form>",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        );
        let doc = parse(&html);
        assert_eq!(classify_submit_button(&doc, select(&doc, "button")), None);
    }

    /// AISNAP-12: 候補は index 昇順で、空入力には空を返す。
    #[test]
    fn aisnap_12_candidates_ascending_and_empty() {
        let doc = parse(
            "<form><ul><li><button>a</button></li><li>b</li>\
             <li><input type=submit></li></ul></form>",
        );
        let items: Vec<NodeId> = doc.children(select(&doc, "ul")).collect();
        assert_eq!(
            indexes_of(&submit_button_candidates(&doc, &items)),
            vec![0, 2]
        );
        assert!(submit_button_candidates(&doc, &[]).is_empty());
    }
}
