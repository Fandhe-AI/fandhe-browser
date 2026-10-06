//! 軽微なページ変化の前後で、対象要素を role+name シグネチャで再特定できるかを判定する
//! ヘルパーとそのユニットテスト（`AISNAP-10`・TASK-17.1・Issue #110・`MS-2`）。
//!
//! 呼び出し文脈: core の `parse_document`・`query_selector_str` と、ai の `build_snapshot`・
//! `compute_role`・`compute_name` を組み合わせる。後続の 15 ケース定義（TASK-17.2・#111）と
//! 参照破損率の算出（TASK-17.3・#112）から使われる予定で、本ファイルはその判定ロジックの
//! 土台だけを持つ。
//!
//! PoC（`stability_check.mjs`）との差分:
//! - core の `Document` に変更 API が無いため、軽微な変化は HTML 文字列（変化前・変化後）で表す
//! - PoC の `tag::text` シグネチャは、本実装の role+name シグネチャ（`AISNAP-10`）へ置き換える
//! - PoC の ref は連番で文字列一致を見ていなかったが、本実装は安定設計なので ref 文字列の
//!   前後一致も成功条件に加える
//! - 設計上 ref を持たない要素（圧縮表のデータ行セル等）は「破損」と区別して
//!   [`Reidentification::NotInSnapshot`] で返す
//!
//! セレクタは変化前後とも 1 要素にだけ一致する必要がある。ref は対象 DOM ノードの順位で
//! 対応づけ、同名の別要素の ref は代用しない。
//!
//! 範囲外: 15 ケースのフィクスチャ化（#111）・破損率の算出とレポート（#112）・
//! 同名要素の順位指定による `Ambiguous` の解消（必要になった時点で #111 で拡張する）。
//! フィクスチャはすべて静的なダミー値で、外部通信をしない。

use std::path::PathBuf;

use fandhe_browser_ai::snapshot::build::MAX_TREE_DEPTH;
use fandhe_browser_ai::snapshot::{
    NameIndex, Node, Snapshot, build_snapshot, compute_name_with_index, compute_role,
};
use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;

/// 1 ケース分の入力。変化は HTML 文字列で表す。
struct StabilityCase<'a> {
    before_html: &'a str,
    after_html: &'a str,
    /// 対象要素の CSS セレクタ。前後それぞれで 1 要素にだけ一致することを求める
    /// （複数一致は先頭一致で別要素を追う恐れがあるため `Ambiguous` にする）。
    selector: &'a str,
}

/// どちら側の Snapshot か。
#[derive(Debug, PartialEq, Eq)]
enum Side {
    Before,
    After,
}

/// 再特定の判定結果。真偽値で済ませず、破損の種別を区別する（REPAIR-4）。
#[derive(Debug, PartialEq, Eq)]
enum Reidentification {
    /// 前後で role+name が一致し、ref も同一。
    Stable {
        role: String,
        name: String,
        r#ref: String,
    },
    /// 前後とも見つかったが ref が変わった（破損）。
    RefChanged { before: String, after: String },
    /// 前後で対象の role+name が変わった（破損）。
    SignatureChanged {
        before: (String, String),
        after: (String, String),
    },
    /// 変化後の Snapshot から対象が消えた（破損。キャップ押し出し・除外等）。
    MissingAfter { role: String, name: String },
    /// セレクタが DOM に一致しない（ケース定義の誤り。PoC の SKIP 相当）。
    TargetNotFound { side: Side },
    /// 変化前の Snapshot に ref 付きで存在しない（設計上 ref を持たない要素。破損とは別扱い）。
    NotInSnapshot { role: String, name: String },
    /// 変化前の対象が hidden・深さ上限・行保持上限で Snapshot から省略されている。設計上 ref を
    /// 持たない要素（[`Reidentification::NotInSnapshot`]）とは区別し、後続の破損率集計（#112）が
    /// 「ref を得られるはずだったケース」として別に数えられるようにする。
    OmittedFromSnapshot {
        role: String,
        name: String,
        cause: OmissionCause,
    },
    /// 同じ role+name の ref 保持要素が複数あり一意に決まらない。
    Ambiguous { side: Side, count: usize },
}

/// Snapshot から省略された理由。
#[derive(Debug, PartialEq, Eq)]
enum OmissionCause {
    /// 対象または祖先が `hidden` / `aria-hidden="true"`。
    Hidden,
    /// 対象または祖先が head・script・style・noscript・template・`input[type=hidden]`。
    Excluded,
    /// 対象の深さが `MAX_TREE_DEPTH` を超える。
    DepthLimit,
    /// 操作可能な要素なのに ref が無い（行内コントロール保持上限等による省略とみなす）。
    RetentionLimit,
}

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("フィクスチャのパースは成功する")
        .document
}

/// Snapshot 内の ref 保持要素（`Node`・表ヘッダ・行内コントロール）を `(role, name, ref)` で
/// 先行順に列挙する。明示スタックの反復で再帰しない。
fn ref_holders(snapshot: &Snapshot) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<&Node> = vec![&snapshot.tree];
    while let Some(node) = stack.pop() {
        if let Some(r) = &node.r#ref {
            out.push((node.role.clone(), node.name.clone(), r.clone()));
        }
        if let Some(table) = &node.table {
            for h in &table.header {
                out.push((h.role.clone(), h.name.clone(), h.r#ref.clone()));
            }
            for row in &table.rows {
                for c in &row.controls {
                    out.push((c.role.clone(), c.name.clone(), c.r#ref.clone()));
                }
            }
        }
        stack.extend(node.children.iter().rev());
    }
    out
}

/// 対象要素の Snapshot 上の扱い。
enum Placement {
    /// 対象 DOM ノードに対応する ref がある。
    Ref(String),
    /// 省略された（hidden・深さ上限・保持上限）。
    Omitted(OmissionCause),
    /// 設計上 ref を持たない要素（圧縮表のデータセル等）。
    NoRefByDesign,
}

/// 片側の対象特定結果。
enum Located {
    /// セレクタが DOM のどの要素にも一致しない。
    NotFound,
    /// セレクタが複数要素に一致し、対象を一意に決められない（先頭一致への依存を排除する）。
    Multiple(usize),
    /// 対象は見つかったが role が無く Snapshot に載らない。
    NoRole,
    /// 省略されていない同 role+name の DOM 要素数と Snapshot の ref 保持要素数が食い違い、
    /// 対象 DOM ノードと ref を対応づけられない。同名の別要素の ref は代用しない。
    Unmappable { peers: usize },
    Found {
        role: String,
        name: String,
        placement: Placement,
    },
}

/// ref を持たない場合に「省略」とみなす、操作可能な role。
const INTERACTIVE_ROLES: &[&str] = &[
    "link",
    "button",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "switch",
    "slider",
    "spinbutton",
    "menuitem",
    "tab",
    "option",
];

/// 要素自身が `build_snapshot` の `is_excluded` と同じ条件でサブツリーごと除外されるか。
/// head・script・style・noscript・template・hidden 系・`input[type=hidden]` を判定する。
/// 公開 API の範囲で再現したもので、hidden 系は [`OmissionCause::Hidden`]、それ以外の
/// 非描画要素は [`OmissionCause::Excluded`] を返す。
fn exclusion_cause(doc: &Document, n: NodeId) -> Option<OmissionCause> {
    if !doc.is_element(n) {
        return None;
    }
    // Snapshot 側（`is_hidden_element`）は `aria-hidden` を HTML ASCII 空白だけで trim する。
    // `str::trim` は Unicode 空白も除くため使わない。
    if doc.attribute(n, "hidden").is_some()
        || doc.attribute(n, "aria-hidden").is_some_and(|v| {
            v.trim_matches(|c| matches!(c, '\t' | '\n' | '\u{0C}' | '\r' | ' '))
                .eq_ignore_ascii_case("true")
        })
    {
        return Some(OmissionCause::Hidden);
    }
    // Snapshot 側（`is_html_element_named`）は HTML 名前空間の要素だけを対象にする
    // （SVG の `<style>` 等は除外しない）。
    let is_html = doc.namespace_url(n) == Some("http://www.w3.org/1999/xhtml");
    let named = |name: &str| {
        is_html
            && doc
                .local_name(n)
                .is_some_and(|l| l.eq_ignore_ascii_case(name))
    };
    // `input[type=hidden]` は Snapshot の `normalized_input_type` と同じく小文字化のみで
    // 前後の空白は除去しない（`type=" hidden "` は無効値で text 扱い。Snapshot では ref を持つ）。
    let non_rendered = ["head", "script", "style", "noscript", "template"]
        .iter()
        .any(|t| named(t))
        || (named("input")
            && doc
                .attribute(n, "type")
                .is_some_and(|v| v.eq_ignore_ascii_case("hidden")));
    non_rendered.then_some(OmissionCause::Excluded)
}

/// 要素が Snapshot から省略される理由（除外条件を持つ祖先・深さ上限）を返す。省略されなければ
/// `None`。対象自身と全祖先に `is_excluded` と同じ条件を適用する。深さは Snapshot と同じく
/// 文書ルートを 0 とし要素だけを 1 段ずつ数える（`MAX_TREE_DEPTH` ちょうどは保持される）。
fn omission_by_position(doc: &Document, id: NodeId) -> Option<OmissionCause> {
    let mut depth = 0usize;
    for n in std::iter::once(id).chain(doc.ancestors(id)) {
        if let Some(cause) = exclusion_cause(doc, n) {
            return Some(cause);
        }
        if doc.is_element(n) {
            depth += 1;
        }
    }
    (depth > MAX_TREE_DEPTH).then_some(OmissionCause::DepthLimit)
}

/// 文書順で `(role, name)` が一致し、かつ Snapshot から省略されない要素を返す
/// （対象 DOM ノードの順位付けに使う）。名前は Snapshot と同じ `compute_name_with_index` で
/// 求める。
fn live_same_signature_elements(
    doc: &Document,
    index: &NameIndex<'_>,
    role: &str,
    name: &str,
) -> Vec<NodeId> {
    doc.descendants(doc.root())
        .filter(|&n| doc.is_element(n))
        .filter(|&n| omission_by_position(doc, n).is_none())
        .filter(|&n| {
            compute_role(doc, n).is_some_and(|r| r.as_str() == role)
                && compute_name_with_index(doc, index, n).text == name
        })
        .collect()
}

/// セレクタが一意に一致する要素を特定し、その DOM ノードに対応する ref だけを返す。
///
/// 対応づけ: 対象がまず hidden・深さ上限で省略されるかを DOM から判定する（省略なら別要素の
/// ref を探さない）。省略されない場合、同 role+name で省略されない DOM 要素（文書順）の中での
/// 対象の順位 k を求め、Snapshot 側の同 role+name の ref 保持要素（先行順）が同数ならその
/// k 番目を採る。数が合わないときは把握できない省略（保持上限等）があり順位がずれるため
/// `Unmappable` とし、確定できない結果は `Ambiguous` に倒す。
fn locate(html: &str, selector: &str) -> Located {
    let doc = parse(html);
    let snapshot = build_snapshot(&doc).expect("フィクスチャの構築は成功する");
    let Ok(matches) = query_selector_all_str(&doc, doc.root(), selector) else {
        return Located::NotFound;
    };
    let id = match matches.as_slice() {
        [] => return Located::NotFound,
        [one] => *one,
        many => return Located::Multiple(many.len()),
    };
    let Some(role) = compute_role(&doc, id) else {
        return Located::NoRole;
    };
    let role = role.as_str().to_string();
    let index = NameIndex::build(&doc);
    let name = compute_name_with_index(&doc, &index, id).text;
    if let Some(cause) = omission_by_position(&doc, id) {
        return Located::Found {
            role,
            name,
            placement: Placement::Omitted(cause),
        };
    }
    // `snapshot.truncated` は文書全体のフラグで、対象と無関係な打ち切りでも立つ。ここでは
    // 早期に `Unmappable` へ倒さず、下の同 role+name の ref 保持要素数と DOM 要素数の一致で
    // 対応づけられる対象は比較を続ける（AISNAP-10）。
    let peers = live_same_signature_elements(&doc, &index, &role, &name);
    let holders: Vec<String> = ref_holders(&snapshot)
        .into_iter()
        .filter(|(r, n, _)| *r == role && *n == name)
        .map(|(_, _, x)| x)
        .collect();
    let placement = if holders.is_empty() {
        // 打ち切りのある文書で ref 保持要素が見つからない場合、対象の名前自体が打ち切りで
        // 食い違った可能性があり、保持上限による省略と区別できない。誤分類せず倒す。
        if snapshot.truncated {
            return Located::Unmappable { peers: peers.len() };
        }
        if INTERACTIVE_ROLES.contains(&role.as_str()) {
            Placement::Omitted(OmissionCause::RetentionLimit)
        } else {
            Placement::NoRefByDesign
        }
    } else if holders.len() == peers.len() {
        match peers
            .iter()
            .position(|&p| p == id)
            .and_then(|k| holders.get(k))
        {
            Some(r) => Placement::Ref(r.clone()),
            None => return Located::Unmappable { peers: peers.len() },
        }
    } else {
        return Located::Unmappable { peers: peers.len() };
    };
    Located::Found {
        role,
        name,
        placement,
    }
}

/// 変化前後の HTML で対象を role+name により再特定し、ref の一致まで判定する。
///
/// ケース定義の誤りとして扱うのは変化前の不一致（`TargetNotFound { Before }`）だけで、
/// 変化後に対象が消えた・一意に決まらないものは参照破損として集計対象に残す。
fn check_reidentification(case: &StabilityCase<'_>) -> Reidentification {
    let before = locate(case.before_html, case.selector);
    let after = locate(case.after_html, case.selector);
    let (b_role, b_name, b_ref) = match before {
        Located::NotFound => return Reidentification::TargetNotFound { side: Side::Before },
        Located::Multiple(count) => {
            return Reidentification::Ambiguous {
                side: Side::Before,
                count,
            };
        }
        Located::Unmappable { peers } => {
            return Reidentification::Ambiguous {
                side: Side::Before,
                count: peers,
            };
        }
        Located::NoRole => {
            return Reidentification::NotInSnapshot {
                role: String::new(),
                name: String::new(),
            };
        }
        Located::Found {
            role,
            name,
            placement,
        } => match placement {
            Placement::Ref(r) => (role, name, r),
            Placement::NoRefByDesign => {
                return Reidentification::NotInSnapshot { role, name };
            }
            Placement::Omitted(cause) => {
                return Reidentification::OmittedFromSnapshot { role, name, cause };
            }
        },
    };
    let (a_role, a_name, a_ref) = match after {
        Located::NotFound | Located::NoRole => {
            return Reidentification::MissingAfter {
                role: b_role,
                name: b_name,
            };
        }
        Located::Multiple(count) => {
            return Reidentification::Ambiguous {
                side: Side::After,
                count,
            };
        }
        Located::Unmappable { peers } => {
            return Reidentification::Ambiguous {
                side: Side::After,
                count: peers,
            };
        }
        Located::Found {
            role,
            name,
            placement,
        } => match placement {
            Placement::Ref(r) => (role, name, Some(r)),
            Placement::NoRefByDesign | Placement::Omitted(_) => (role, name, None),
        },
    };
    if (&a_role, &a_name) != (&b_role, &b_name) {
        return Reidentification::SignatureChanged {
            before: (b_role, b_name),
            after: (a_role, a_name),
        };
    }
    match a_ref {
        None => Reidentification::MissingAfter {
            role: b_role,
            name: b_name,
        },
        Some(a) if a == b_ref => Reidentification::Stable {
            role: b_role,
            name: b_name,
            r#ref: b_ref,
        },
        Some(a) => Reidentification::RefChanged {
            before: b_ref,
            after: a,
        },
    }
}

fn page(body: &str) -> String {
    format!("<!DOCTYPE html><html><head><title>t</title></head><body>{body}</body></html>")
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path).expect("リポ内フィクスチャは読める")
}

/// AISNAP-10・TASK-17.1・Issue #110（受入基準）: 変化なしのフィクスチャで再特定に成功する。
#[test]
fn aisnap_10_unchanged_fixture_is_stable() {
    let html = fixture("login-form.html");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "button[type=submit]",
    });
    let Reidentification::Stable { role, name, r#ref } = result else {
        panic!("expected Stable, got {result:?}");
    };
    assert_eq!(role, "button");
    assert_eq!(name, "Login");
    assert_eq!(r#ref, "e7e730a2753f66c98");
}

/// AISNAP-10・TASK-17.1・Issue #110: バナー追加の軽微変化でも ref が前後で同一。
#[test]
fn aisnap_10_banner_added_keeps_ref() {
    let before = page("<h1>Title</h1><button id=\"go\">Go</button>");
    let after = page("<div>お知らせ</div><h1>Title</h1><button id=\"go\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "button",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "ee727a43f79604781".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 対照。識別属性が変わると破損として検出する
/// （ヘルパーが常に成功を返す実装でないことの証明）。
#[test]
fn aisnap_10_changed_id_is_detected_as_ref_changed() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button id=\"run\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "button",
    });
    assert_eq!(
        result,
        Reidentification::RefChanged {
            before: "ee727a43f79604781".into(),
            after: "e5254037ee0b5e5cf".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 名前が変わると `SignatureChanged` になる。
#[test]
fn aisnap_10_renamed_target_is_signature_changed() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button id=\"go\">Stop</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "button",
    });
    assert_eq!(
        result,
        Reidentification::SignatureChanged {
            before: ("button".into(), "Go".into()),
            after: ("button".into(), "Stop".into()),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 対照。変化後に Snapshot から除外されると `MissingAfter`。
#[test]
fn aisnap_10_hidden_after_is_missing_after() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button id=\"go\" hidden>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "button",
    });
    assert_eq!(
        result,
        Reidentification::MissingAfter {
            role: "button".into(),
            name: "Go".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: セレクタ不一致はケース定義の誤りとして区別する。
#[test]
fn aisnap_10_unmatched_selector_is_target_not_found() {
    let html = page("<button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#absent",
    });
    assert_eq!(
        result,
        Reidentification::TargetNotFound { side: Side::Before }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 同名・識別属性なしの要素 2 個は `Ambiguous`。
#[test]
fn aisnap_10_duplicate_signature_is_ambiguous() {
    let html = page("<button>Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "button",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::Before,
            count: 2
        }
    );
}

/// 表（ヘッダ + データ行 + 行内リンク）の HTML。
fn regular_table() -> String {
    let rows: String = (0..60)
        .map(|i| format!("<tr><td>row{i}</td><td><a href=\"/r/{i}\">Open{i}</a></td></tr>"))
        .collect();
    page(&format!(
        "<table><thead><tr><th class=\"h0\">n</th><th>link</th></tr></thead><tbody>{rows}</tbody></table>"
    ))
}

/// AISNAP-10・TASK-17.1・Issue #110: 圧縮表のデータセルは設計上 ref を持たず、
/// 破損ではなく `NotInSnapshot` として区別される。
#[test]
fn aisnap_10_compressed_data_cell_is_not_in_snapshot() {
    let rows: String = (0..60)
        .map(|i| format!("<tr><td class=\"c{i}\">row{i}</td></tr>"))
        .collect();
    let html = page(&format!(
        "<table><thead><tr><th>n</th></tr></thead><tbody>{rows}</tbody></table>"
    ));
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "td.c59",
    });
    assert_eq!(
        result,
        Reidentification::NotInSnapshot {
            role: "cell".into(),
            name: "row59".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 表のヘッダセルと行内リンクも ref 保持要素として
/// 拾われ、変化なしで `Stable` になる。
#[test]
fn aisnap_10_table_header_and_row_control_are_stable() {
    let html = regular_table();
    let header = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "th.h0",
    });
    assert!(
        matches!(header, Reidentification::Stable { .. }),
        "{header:?}"
    );
    let link = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "a[href=\"/r/3\"]",
    });
    assert!(matches!(link, Reidentification::Stable { .. }), "{link:?}");
}
/// AISNAP-10・TASK-17.1・Issue #110: 変化後に対象が DOM から消えたら、ケース定義エラーではなく
/// 参照破損（`MissingAfter`）として扱う。
#[test]
fn aisnap_10_removed_after_is_missing_after() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<p>none</p>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#go",
    });
    assert_eq!(
        result,
        Reidentification::MissingAfter {
            role: "button".into(),
            name: "Go".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 対象が hidden で除外されたうえ同名の別要素が加わり件数が
/// 前後で揃っても、別要素の ref を代用せず `MissingAfter` にする。
#[test]
fn aisnap_10_hidden_target_does_not_borrow_sibling_ref() {
    let before = page("<button id=\"a\">Go</button><button id=\"b\">Go</button>");
    let after = page(
        "<button id=\"a\" hidden>Go</button><button id=\"b\">Go</button><button id=\"c\">Go</button>",
    );
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#a",
    });
    assert_eq!(
        result,
        Reidentification::MissingAfter {
            role: "button".into(),
            name: "Go".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 変化前に hidden で省略された対象は、設計上 ref を持たない
/// 要素（`NotInSnapshot`）ではなく `OmittedFromSnapshot` で返す。
#[test]
fn aisnap_10_hidden_before_is_omitted_from_snapshot() {
    let html = page("<div hidden><button id=\"go\">Go</button></div>");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#go",
    });
    assert_eq!(
        result,
        Reidentification::OmittedFromSnapshot {
            role: "button".into(),
            name: "Go".into(),
            cause: OmissionCause::Hidden,
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 行内コントロールの保持上限で落ちたリンクは
/// `OmittedFromSnapshot`（`RetentionLimit`）で返し、圧縮表のデータセルと区別する。
#[test]
fn aisnap_10_row_control_over_cap_is_omitted_from_snapshot() {
    let links: String = (0..=8)
        .map(|i| format!("<a href=\"/l/{i}\">L{i}</a>"))
        .collect();
    let html = page(&format!(
        "<table><thead><tr><th>n</th></tr></thead><tbody><tr><td>{links}</td></tr></tbody></table>"
    ));
    // 保持上限の挙動は ai 側の実装に依存するため、Snapshot に載らないリンクを探して検証する。
    let doc = parse(&html);
    let snapshot = build_snapshot(&doc).expect("フィクスチャの構築は成功する");
    let held: Vec<String> = ref_holders(&snapshot)
        .into_iter()
        .map(|(_, n, _)| n)
        .collect();
    let dropped = (0..=8)
        .map(|i| format!("L{i}"))
        .find(|n| !held.contains(n))
        .expect("上限超過で落ちるリンクがある");
    let idx = dropped.trim_start_matches('L');
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: &format!("a[href=\"/l/{idx}\"]"),
    });
    assert_eq!(
        result,
        Reidentification::OmittedFromSnapshot {
            role: "link".into(),
            name: dropped,
            cause: OmissionCause::RetentionLimit,
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 深さ上限を超える対象は `OmittedFromSnapshot`
/// （`DepthLimit`）で返す。
#[test]
fn aisnap_10_beyond_depth_limit_is_omitted_from_snapshot() {
    let wrap = MAX_TREE_DEPTH + 8;
    let html = page(&format!(
        "{}<button id=\"deep\">Go</button>{}",
        "<div>".repeat(wrap),
        "</div>".repeat(wrap)
    ));
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#deep",
    });
    assert_eq!(
        result,
        Reidentification::OmittedFromSnapshot {
            role: "button".into(),
            name: "Go".into(),
            cause: OmissionCause::DepthLimit,
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 対象と無関係な深いサブツリーが打ち切られて
/// `Snapshot::truncated` が立っても、対応づけられる対象は `Stable` のまま比較を続ける。
#[test]
fn aisnap_10_unrelated_truncation_keeps_target_stable() {
    let wrap = MAX_TREE_DEPTH + 8;
    let html = page(&format!(
        "<button id=\"t\">Go</button>{}<p>x</p>{}",
        "<div>".repeat(wrap),
        "</div>".repeat(wrap)
    ));
    let doc = parse(&html);
    let snapshot = build_snapshot(&doc).expect("フィクスチャの構築は成功する");
    assert!(snapshot.truncated, "前提: 無関係な深い枝で打ち切りが起きる");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#t",
    });
    assert!(
        matches!(
            &result,
            Reidentification::Stable { role, name, .. } if role == "button" && name == "Go"
        ),
        "unexpected: {result:?}"
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: noscript 内の対象は Snapshot の除外条件に従い
/// `NoRefByDesign` / `RetentionLimit` ではなく `Excluded` の省略として返す。
#[test]
fn aisnap_10_noscript_target_is_omitted_as_excluded() {
    let html = page("<noscript><button id=\"t\">Go</button></noscript>");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#t",
    });
    assert_eq!(
        result,
        Reidentification::OmittedFromSnapshot {
            role: "button".into(),
            name: "Go".into(),
            cause: OmissionCause::Excluded,
        }
    );
}

/// 要素だけを数えた Snapshot 上の深さ（文書ルートを 0 とする）。
fn element_depth(doc: &Document, id: NodeId) -> usize {
    std::iter::once(id)
        .chain(doc.ancestors(id))
        .filter(|&n| doc.is_element(n))
        .count()
}

/// AISNAP-10・TASK-17.1・Issue #110: 深さがちょうど `MAX_TREE_DEPTH` の対象は Snapshot に
/// 残るため `DepthLimit` と誤報告せず、1 段深い対象だけが `DepthLimit` になる。
#[test]
fn aisnap_10_depth_boundary_is_exact() {
    let build = |wrap: usize| {
        page(&format!(
            "{}<button id=\"deep\">Go</button>{}",
            "<div>".repeat(wrap),
            "</div>".repeat(wrap)
        ))
    };
    // ラッパー数 0 のときの深さから、ちょうど MAX_TREE_DEPTH になる数を求める。
    let base_html = build(0);
    let base_doc = parse(&base_html);
    let base_id =
        query_selector_all_str(&base_doc, base_doc.root(), "#deep").expect("有効なセレクタ")[0];
    let wrap = MAX_TREE_DEPTH - element_depth(&base_doc, base_id);
    let at_limit = build(wrap);
    let doc = parse(&at_limit);
    let id = query_selector_all_str(&doc, doc.root(), "#deep").expect("有効なセレクタ")[0];
    assert_eq!(element_depth(&doc, id), MAX_TREE_DEPTH);
    let result = check_reidentification(&StabilityCase {
        before_html: &at_limit,
        after_html: &at_limit,
        selector: "#deep",
    });
    assert!(
        matches!(result, Reidentification::Stable { .. }),
        "境界ちょうどは Snapshot に残る: {result:?}"
    );
    let over = build(wrap + 1);
    let result = check_reidentification(&StabilityCase {
        before_html: &over,
        after_html: &over,
        selector: "#deep",
    });
    assert_eq!(
        result,
        Reidentification::OmittedFromSnapshot {
            role: "button".into(),
            name: "Go".into(),
            cause: OmissionCause::DepthLimit,
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 同 role・name の要素が前方に挿入され、セレクタの
/// 先頭一致が別要素になりうる変化は `Stable` にしない。
#[test]
fn aisnap_10_same_signature_inserted_before_is_ambiguous() {
    let before = page("<button>Go</button>");
    let after = page("<button class=\"x\">Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "button",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 一意なセレクタ（id）なら、同名要素が前方に挿入されても
/// 同じ要素を追跡できる。
#[test]
fn aisnap_10_unique_selector_tracks_target_after_insertion() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button class=\"x\">Go</button><button id=\"go\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#go",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "ee727a43f79604781".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: `type=" hidden "`（前後空白つき）の input は Snapshot では
/// 無効値として text 扱いで ref を持つため、`OmittedFromSnapshot` ではなく `Stable` を返す。
#[test]
fn aisnap_10_input_type_with_spaces_is_not_excluded() {
    let html = page("<input id=\"t\" type=\" hidden \" aria-label=\"Q\">");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#t",
    });
    assert!(
        matches!(&result, Reidentification::Stable { role, name, .. } if role == "textbox" && name == "Q"),
        "got {result:?}"
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: `aria-hidden` の前後の Unicode 空白（NBSP）は Snapshot では
/// trim されず `true` と一致しないため、対象は省略されない。
#[test]
fn aisnap_10_aria_hidden_with_nbsp_is_not_excluded() {
    let html = page("<button id=\"t\" aria-hidden=\"\u{a0}true\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#t",
    });
    assert!(
        matches!(&result, Reidentification::Stable { name, .. } if name == "Go"),
        "got {result:?}"
    );
}
