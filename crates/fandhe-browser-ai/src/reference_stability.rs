//! 軽微なページ変化の前後で、対象要素を role+name シグネチャで再特定できるかを判定する
//! ヘルパーとそのユニットテスト（`AISNAP-10`・TASK-17.1・Issue #110・`MS-2`）。
//!
//! 配置: `build_snapshot` が実際に算出した要素ごとの role・name・ref
//! （[`crate::snapshot::build::build_snapshot_with_holders`]。crate 内部のテスト専用フック）を
//! 使うため、crate 内の `#[cfg(test)]` モジュールに置く。名前の再計算はしない（共有予算つきの
//! `NameIndex` による Snapshot 側の結果と食い違わせないため）。後続の参照破損率の算出
//! （TASK-17.3・#112）が同じ判定を必要とする場合も、この crate 内のテストから使う想定。
//!
//! 呼び出し文脈: core の `parse_document`・`query_selector_all_str` と、ai の
//! `build_snapshot`（内部フック版）を組み合わせる。フィクスチャ 19 ケース（TASK-17.2・#111）は
//! `tests/stability_fixtures.rs` が資産として検査し、本ヘルパーには依存しない。
//!
//! PoC（`stability_check.mjs`）との差分:
//! - core の `Document` に変更 API が無いため、軽微な変化は HTML 文字列（変化前・変化後）で表す
//! - PoC の `tag::text` シグネチャは、本実装の role+name シグネチャ（`AISNAP-10`）へ置き換える
//! - PoC の ref は連番で文字列一致を見ていなかったが、本実装は安定設計なので ref 文字列の
//!   前後一致も成功条件に加える
//! - 設計上 ref を持たない要素（圧縮表のデータ行セル等）は「破損」と区別して
//!   [`Reidentification::NotInSnapshot`] で返す
//!
//! 判定モデル: セレクタは「変化前の対象」を定めるためだけに使い、変化前に 1 要素にだけ一致する
//! 必要がある（[`locate_in`]）。変化後の判定は、変化前の対象 [`Target`] と前後の [`RefHolder`]
//! 列だけから観測値 [`Facts`] を求め（[`observe`]）、判定表（[`decide`]。`docs/design/
//! reference-disambiguation.md` の「判定表」A1〜A9）で結果を決める。変化後の DOM・セレクタ・位置は
//! 使わない（`check_reidentification` は変化後の `Document` を保持しない）。区別子 ` #n` と ref の
//! 出現番号 `-n` は出現順で決まるため、順序を決める集合が変わる変化は `Ambiguous` にする。
//! 名前の変更（accname の変化）は消失と区別できず `MissingAfter`（#1228）。
//!
//! 破損率の算出と測定出力は子モジュール `measure`（TASK-17.3・#112）が担う。フィクスチャは
//! すべて静的なダミー値で、外部通信をしない。

mod measure;

use std::path::PathBuf;

use crate::snapshot::build::{MAX_TREE_DEPTH, RefHolder, build_snapshot_with_holders};
use crate::snapshot::{NameIndex, compute_name_with_index, compute_role};
use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;

/// 1 ケース分の入力。変化は HTML 文字列で表す。
struct StabilityCase<'a> {
    before_html: &'a str,
    after_html: &'a str,
    /// 変化前の対象要素の CSS セレクタ。変化前の HTML で 1 要素にだけ一致することを求める
    /// （複数一致は先頭一致で別要素を追う恐れがあるため `Ambiguous` にする）。変化後の HTML では
    /// 評価しない（変化後は role+name・ref と holder 列だけで判定する。AISNAP-10）。
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

/// 対象要素の Snapshot 上の扱い。
enum Placement {
    /// 対象 DOM ノードに対応する ref がある。
    Ref(Target),
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

/// 変化前の対象 T（判定表の入力。AISNAP-10）。変化後の判定は T と前後の [`RefHolder`] 列だけで
/// 行い、変化後の DOM・セレクタ・位置は使わない。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    role: String,
    /// 区別子付与後の出力 name。変化後の照合（role+name による再特定）に使う。
    name: String,
    /// 区別子付与前の基底 name。出現順を決める集合 `(role, group_name)` のキーにだけ使う。
    group_name: String,
    r#ref: String,
}

/// 変化後の role+name 照合と ref 照合の分類（判定表の L1〜L5。上から順に評価し、必ず 1 つに決まる）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookup {
    /// L1: 同じ role+name の保持要素が 2 件以上ある。
    ManyByName(usize),
    /// L2: 変化前の ref を持つ保持要素があり、その role+name が T と同じ。
    Agree,
    /// L3: role+name の候補が 1 件で、変化前の ref はどこにも無い。
    OneByNameRefGone { after_ref: String },
    /// L4: 変化前の ref を持つ保持要素 Z があるが、Z の role+name は T と違う。
    RefElsewhere {
        role: String,
        name: String,
        group_name: String,
    },
    /// L5: role+name の候補も変化前の ref も無い。
    Gone,
}

/// 判定表の観測値。すべて前後の holder 列だけから求める（[`observe`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Facts {
    lookup: Lookup,
    /// ref が出現順に依存しない（`-n` が無く、ref ファミリが前後とも 1 件）。
    ref_order_independent: bool,
    /// 出現順を決める集合（グループ `(role, group_name)` と ref ファミリ）が前後で同じ。
    context_unchanged: bool,
    /// 変化後のグループ `(role, group_name)` の件数。
    group_after: usize,
    /// 変化後の ref ファミリの件数。
    family_after: usize,
}

/// ref から出現番号の接尾辞 `-<数字>` だけを除いたキー（ダイジェスト＋variant）。`v<n>` や
/// 延長した hex は ref の同一性の一部なので残す。同じキーの保持要素が ref ファミリ。
fn ref_family_key(r: &str) -> &str {
    match r.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => r,
    }
}

/// 変化前後の holder 列から判定表の観測値を求める。DOM もセレクタも受け取らない。
fn observe(t: &Target, before: &[RefHolder], after: &[RefHolder]) -> Facts {
    let by_name: Vec<&RefHolder> = after
        .iter()
        .filter(|h| h.role == t.role && h.name == t.name)
        .collect();
    let by_ref = after.iter().find(|h| h.r#ref == t.r#ref);
    let lookup = if by_name.len() >= 2 {
        Lookup::ManyByName(by_name.len())
    } else if by_ref.is_some_and(|z| z.role == t.role && z.name == t.name) {
        Lookup::Agree
    } else if let (None, [one]) = (by_ref, by_name.as_slice()) {
        Lookup::OneByNameRefGone {
            after_ref: one.r#ref.clone(),
        }
    } else if let Some(z) = by_ref {
        Lookup::RefElsewhere {
            role: z.role.clone(),
            name: z.name.clone(),
            group_name: z.group_name.clone(),
        }
    } else {
        Lookup::Gone
    };

    let family = ref_family_key(&t.r#ref);
    let in_family = |hs: &[RefHolder]| -> Vec<(String, String, String)> {
        hs.iter()
            .filter(|h| ref_family_key(&h.r#ref) == family)
            .map(|h| (h.r#ref.clone(), h.role.clone(), h.group_name.clone()))
            .collect()
    };
    let in_group = |hs: &[RefHolder]| -> Vec<String> {
        hs.iter()
            .filter(|h| h.role == t.role && h.group_name == t.group_name)
            .map(|h| h.r#ref.clone())
            .collect()
    };
    let (family_before, family_after) = (in_family(before), in_family(after));
    let group_after = in_group(after);
    Facts {
        lookup,
        ref_order_independent: family == t.r#ref
            && family_before.len() == 1
            && family_after.len() == 1,
        context_unchanged: in_group(before) == group_after && family_before == family_after,
        group_after: group_after.len(),
        family_after: family_after.len(),
    }
}

/// 判定表（変化後）。1 アームが 1 行に対応する。`count` は常に 1 以上。
fn decide(t: Target, f: Facts) -> Reidentification {
    let ambiguous = |count: usize| Reidentification::Ambiguous {
        side: Side::After,
        count: count.max(1),
    };
    let stable = |t: Target| Reidentification::Stable {
        role: t.role,
        name: t.name,
        r#ref: t.r#ref,
    };
    match f.lookup {
        // 行 A1: 区別子の衝突解消が試行上限（#1222）を超えて残った同名の重複。fail-closed
        Lookup::ManyByName(k) => ambiguous(k),
        Lookup::Agree => {
            if f.ref_order_independent {
                // 行 A2: 順序に依存しない ref が同じ要素を指し、role・name も一致する
                stable(t)
            } else if f.context_unchanged {
                // 行 A3: 出現順で決まる同一性だが、順序を決める集合が変わっていない
                stable(t)
            } else {
                // 行 A4: 順序の集合が変わり、別要素が番号を引き継いだ可能性を否定できない
                ambiguous(f.group_after)
            }
        }
        // 行 A5: role+name の候補の ref が変わった（観測事実だけを述べる）
        Lookup::OneByNameRefGone { after_ref } => Reidentification::RefChanged {
            before: t.r#ref,
            after: after_ref,
        },
        Lookup::RefElsewhere {
            role,
            name,
            group_name,
        } => {
            if role == t.role && group_name == t.group_name {
                // 行 A6: 基底は同じで、区別子 #n がずれただけ（順序の問題）
                ambiguous(f.group_after)
            } else if f.ref_order_independent {
                // 行 A7: 順序に依存しない ref で特定した対象の表示 name が変わった
                Reidentification::SignatureChanged {
                    before: (t.role, t.name),
                    after: (role, name),
                }
            } else {
                // 行 A8: ref が順序に依存し、Z が対象だと確かめられない
                ambiguous(f.family_after)
            }
        }
        // 行 A9: 候補も ref も無い（名前変更と消失は区別しない）
        Lookup::Gone => Reidentification::MissingAfter {
            role: t.role,
            name: t.name,
        },
    }
}

/// セレクタが一意に一致する要素を特定し、その DOM ノードに対して Snapshot が発行した ref を返す
/// （変化前の対象を定める。判定表 B1〜B7）。
///
/// 対応づけ: 対象がまず hidden・深さ上限で省略されるかを DOM から判定する（省略なら別要素の
/// ref を探さない）。省略されない場合は、`build_snapshot` が記録した ref 発行要素
/// （[`RefHolder`]）から対象の `NodeId` で直接引く。role・name も記録された Snapshot 自身の結果を
/// 使い、再計算しない。記録に無い要素（圧縮表のデータセル・行内コントロールの保持上限で落ちた
/// 要素等）は Snapshot が名前を算出していないため、名前は予算なしの索引で参考値として求める。
fn locate_in(doc: &Document, holders: &[RefHolder], selector: &str) -> Located {
    let Ok(matches) = query_selector_all_str(doc, doc.root(), selector) else {
        return Located::NotFound;
    };
    let id = match matches.as_slice() {
        [] => return Located::NotFound,
        [one] => *one,
        many => return Located::Multiple(many.len()),
    };
    let Some(role) = compute_role(doc, id) else {
        return Located::NoRole;
    };
    let role = role.as_str().to_string();
    if let Some(holder) = holders.iter().find(|h: &&RefHolder| h.node == id) {
        return Located::Found {
            role: holder.role.clone(),
            name: holder.name.clone(),
            placement: Placement::Ref(Target {
                role: holder.role.clone(),
                name: holder.name.clone(),
                group_name: holder.group_name.clone(),
                r#ref: holder.r#ref.clone(),
            }),
        };
    }
    let index = NameIndex::build(doc);
    let name = compute_name_with_index(doc, &index, id).text;
    let placement = if let Some(cause) = omission_by_position(doc, id) {
        Placement::Omitted(cause)
    } else if INTERACTIVE_ROLES.contains(&role.as_str()) {
        // 記録に無い操作要素は保持上限による省略として扱う（Snapshot が ref を発行していない）。
        Placement::Omitted(OmissionCause::RetentionLimit)
    } else {
        Placement::NoRefByDesign
    };
    Located::Found {
        role,
        name,
        placement,
    }
}

/// HTML をパースして Snapshot を構築し、ref 保持要素の記録だけを返す（Document は返さない）。
fn holders_of(html: &str) -> Vec<RefHolder> {
    let doc = parse(html);
    build_snapshot_with_holders(&doc)
        .expect("フィクスチャの構築は成功する")
        .1
}

/// [`locate_in`] の HTML 版（measure 用）。
fn locate(html: &str, selector: &str) -> Located {
    let doc = parse(html);
    let (_snapshot, holders) =
        build_snapshot_with_holders(&doc).expect("フィクスチャの構築は成功する");
    locate_in(&doc, &holders, selector)
}

/// 変化前後の HTML で対象を role+name により再特定し、ref の一致まで判定する。
///
/// ケース定義の誤りとして扱うのは変化前の不一致（`TargetNotFound { Before }`）だけで、
/// 変化後に対象が消えた・一意に決まらないものは参照破損として集計対象に残す。変化前の HTML は
/// 1 回だけパース・構築してセレクタで対象を定め、変化後は holder 列だけを取り出す。変化後の
/// `Document` を保持しないため、変化後に DOM・セレクタを使えないことが型の上で保たれる。
fn check_reidentification(case: &StabilityCase<'_>) -> Reidentification {
    let before_doc = parse(case.before_html);
    let (_snapshot, before_holders) =
        build_snapshot_with_holders(&before_doc).expect("フィクスチャの構築は成功する");
    let target = match locate_in(&before_doc, &before_holders, case.selector) {
        // 行 B1
        Located::NotFound => return Reidentification::TargetNotFound { side: Side::Before },
        // 行 B2
        Located::Multiple(count) => {
            return Reidentification::Ambiguous {
                side: Side::Before,
                count,
            };
        }
        // 行 B3
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
            // 行 B4
            Placement::Ref(t) => t,
            // 行 B7
            Placement::NoRefByDesign => {
                return Reidentification::NotInSnapshot { role, name };
            }
            // 行 B5・B6
            Placement::Omitted(cause) => {
                return Reidentification::OmittedFromSnapshot { role, name, cause };
            }
        },
    };
    let after_holders = holders_of(case.after_html);
    let facts = observe(&target, &before_holders, &after_holders);
    decide(target, facts)
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
    assert_eq!(r#ref, "e7e730a27");
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
            r#ref: "ee727a43f".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 対照。識別属性が変わると破損として検出する
/// （ヘルパーが常に成功を返す実装でないことの証明）。
#[test]
fn aisnap_10_changed_id_is_detected_as_ref_changed() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button id=\"run\">Go</button>");
    // 変化後に `#go` は一致しないが、変化後は role+name の候補で探すため `MissingAfter` にならない。
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#go",
    });
    assert_eq!(
        result,
        Reidentification::RefChanged {
            before: "ee727a43f".into(),
            after: "e5254037e".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: accname が変わる名前変更は ref も変わる。変化後にセレクタを
/// 使わない契約（判定表 A9）では消失と区別できず、`MissingAfter` で返す
/// （`SignatureChanged` は accname が同じで表示 name だけが変わる場合。`adjacent_text_change_*`）。
#[test]
fn aisnap_10_renamed_target_is_missing_after() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button id=\"go\">Stop</button>");
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

/// AISNAP-10・TASK-17.1・Issue #110: 対象が hidden で除外されたうえ同名の別要素が加わる場合、
/// 変化後は role+name の候補で判定し、b の ref を対象 a のものとして代用しない（`Stable` にしない）。
/// 区別子 ` #n`（#1221）により変化後の候補は b（素の `Go`）1 件になり、ref が変わったため
/// `RefChanged`（以前は同名 2 件で `Ambiguous`）。
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
    let (before_ref, after_ref) = match &result {
        Reidentification::RefChanged { before, after } => (before.clone(), after.clone()),
        other => panic!("expected RefChanged, got {other:?}"),
    };
    assert_ne!(before_ref, after_ref);
    assert_eq!(
        result,
        Reidentification::RefChanged {
            before: before_ref,
            after: after_ref,
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
    let (_, holders) = build_snapshot_with_holders(&doc).expect("フィクスチャの構築は成功する");
    let held: Vec<String> = holders.into_iter().map(|h| h.name).collect();
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
    let snapshot = crate::snapshot::build_snapshot(&doc).expect("フィクスチャの構築は成功する");
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

/// AISNAP-10・TASK-17.1・Issue #110・#1221: 同 role・name の要素が前方に挿入される変化は
/// `Stable` にしない。区別子 ` #n` は出現順で付くため、挿入された先頭の `Go` が素の name と元の
/// ref を得てしまうが、同 role・base name の要素が増えた変化として `Ambiguous` で検出する。
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

/// AISNAP-10・TASK-17.1・Issue #110: 変化後はセレクタではなく role+name の候補で判定するため、
/// `#go` で一意に選べても同 role+name の別要素が増えれば `Ambiguous` になる（案 Y。#1221 後も維持）。
#[test]
fn aisnap_10_unique_selector_but_duplicate_signature_after_is_ambiguous() {
    let before = page("<button id=\"go\">Go</button>");
    let after = page("<button class=\"x\">Go</button><button id=\"go\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#go",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221: 構造セレクタ（疑似クラス `:not` 等）は挿入された別要素にも同じ位置で
/// 一致しうるため、一致位置を同一性の根拠にしない。ここでは元の対象に class が付き、挿入された
/// 先頭の `Go` が `button:not(.z)` に一致する（前後とも位置 0）。`Stable` ではなく `Ambiguous`。
/// なお `:first-of-type` 等は core のセレクタ実装が未対応（Unsupported）で `TargetNotFound`。
#[test]
fn aisnap_10_structural_selector_insert_before_is_ambiguous() {
    let before = page("<button>Go</button>");
    let after = page("<button>Go</button><button class=\"z\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "button:not(.z)",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221・判定表 A4: 識別属性の無い対象の後ろへ同名要素が追加される変化は
/// `Ambiguous`。変化後の holder 列は「前方に同名要素が挿入された」場合と同じで、契約の範囲
/// （role・name・ref・holder 列）では区別できない（オーナー承認済みの安全側の限界。#113）。
#[test]
fn aisnap_10_same_signature_appended_after_is_ambiguous() {
    let before = page("<button class=\"target\">Go</button>");
    let after = page("<button class=\"target\">Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221: 重複が減って対象自身が消え、残った別要素が素の name と先頭の ref を
/// 引き継ぐ場合は `Stable` にせず `Ambiguous`。
#[test]
fn aisnap_10_duplicate_removed_target_is_ambiguous() {
    let before = page("<button class=\"target\">Go</button><button>Go</button>");
    let after = page("<button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 1
        }
    );
}

/// AISNAP-10・Issue #1221・判定表 A3（既知の限界）: 対象が hidden で省略され、同名の要素が
/// 1 件加わって件数が同じになる変化は `Stable` になる。変化後の holder 列が「変化のないページ」
/// （`Go`・`Go #2` の 2 件）と同じで、契約の範囲では検出できない。オーナー承認済みの限界。
#[test]
fn aisnap_10_hidden_target_with_added_sibling_same_count_is_known_limit() {
    let before = page("<button class=\"target\">Go</button><button>Go</button>");
    let after =
        page("<button class=\"target\" hidden>Go</button><button>Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "e9ddb5239".into(),
        }
    );
}

/// AISNAP-10・Issue #1221・判定表 A3: 変化前のセレクタが変化後に 0 件でも、role+name と ref が
/// 維持される対象は `Stable`（class だけの変更を参照破損にしない）。
#[test]
fn aisnap_10_class_only_change_keeps_stable() {
    let before = page("<button class=\"target\">Go</button>");
    let after = page("<button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "e9ddb5239".into(),
        }
    );
}

/// AISNAP-10・Issue #1221・判定表 A3: 同名ボタンが並ぶ中で対象が class を失うだけの変化は、
/// 変化前後の HTML が `hidden_target_with_added_sibling_same_count_is_known_limit` と同じ形の
/// holder 列になり、どの判定関数でも `Stable` か `Ambiguous` の一方にしか決められない。
/// 4284bb6f（Ambiguous）と eb6c9ad3（Stable）の往復を止めるため `Stable` に固定する
/// （順序を決める集合が変わっていない）。
#[test]
fn aisnap_10_class_only_change_among_duplicates_same_count_is_stable() {
    let before = page("<button class=\"target\">Go</button><button>Go</button>");
    let after = page("<button>Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "e9ddb5239".into(),
        }
    );
}

/// AISNAP-10・Issue #1221: 対象が実際に消え、同名の別要素だけが残る（グループ件数が減る）場合は
/// `Ambiguous`。
#[test]
fn aisnap_10_removed_target_with_remaining_same_name_is_ambiguous() {
    let before = page("<button class=\"target\">Go</button><button>Go</button><button>Go</button>");
    let after = page("<button>Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221: name が上限（120 文字）で区別子付きの表示 name が切り詰められても、
/// 切り詰め前のグループキーで件数を比較するので、前方挿入は `Ambiguous`。
#[test]
fn aisnap_10_truncated_name_group_insert_before_is_ambiguous() {
    let long = "a".repeat(120);
    let before = page(&format!("<button id=\"t\">{long}</button>"));
    let after = page(&format!(
        "<button>{long}</button><button id=\"t\">{long}</button>"
    ));
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#t",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221・判定表 A4: 対象が名前変更で別グループへ移り、元の name の同名要素が
/// 増えた場合。変化後の holder 列は「`Stop` と `Go` 2 件を追加し対象はそのまま」の場合と同じで、
/// 変化後にセレクタを使わない契約では区別できないため `Ambiguous`（先頭の別ボタンを `Stable`
/// にしない）。
#[test]
fn aisnap_10_renamed_into_other_group_is_ambiguous() {
    let before = page("<button class=\"target\">Go</button>");
    let after =
        page("<button class=\"target\">Stop</button><button>Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
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

/// AISNAP-10・TASK-17.1・Issue #110: name 算出の共有予算（`MAX_TOTAL_CONTENT_STEPS`）に達する
/// HTML でも、対象の role・name・ref は Snapshot 自身の算出結果と一致する。
/// 予算なしで再計算していた頃は、予算超過後の要素の name が食い違い誤判定した。
#[test]
fn aisnap_10_name_budget_exhaustion_matches_snapshot() {
    // 1 要素あたりの走査上限（1024）に届く内容を持つ role=button の入れ子を、40 グループ並べる。
    // 入れ子の各祖先が同じ子孫を重複して走査するため、少ないノード数で総予算（2^21）を使い切る。
    // 葉は空要素にする（要素境界の区切り〔TASK-17・#835〕で文字数上限へ早く届き、
    // ステップを使い切る前に打ち切られるのを避けるため）。
    let group = format!(
        "{}{}{}",
        "<div role=\"button\">".repeat(60),
        "<i></i>".repeat(1100),
        "</div>".repeat(60)
    );
    let html = page(&format!(
        "{}<a id=\"t\" href=\"/t\">Go</a>",
        group.repeat(40)
    ));
    let doc = parse(&html);
    let (snapshot, holders) = build_snapshot_with_holders(&doc).expect("構築は成功する");
    assert!(snapshot.truncated, "前提: 予算超過で打ち切りが起きる");
    let id = query_selector_all_str(&doc, doc.root(), "#t").expect("有効なセレクタ")[0];
    let held = holders
        .iter()
        .find(|h| h.node == id)
        .expect("対象は Snapshot が ref を発行している");
    assert_eq!(held.name, "", "前提: 予算超過後の name は空になる");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#t",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "link".into(),
            name: held.name.clone(),
            r#ref: held.r#ref.clone(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 変化後に名前が変わり、同時に hidden で省略された対象は
/// `SignatureChanged` ではなく、ref が消えた `MissingAfter`（変化前の role・name）で返す。
#[test]
fn aisnap_10_renamed_and_hidden_after_is_missing_after() {
    let before = page("<button id=\"t\">Go</button>");
    let after = page("<button id=\"t\" hidden>Stop</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#t",
    });
    assert_eq!(
        result,
        Reidentification::MissingAfter {
            role: "button".into(),
            name: "Go".into(),
        }
    );
}

/// AISNAP-10・TASK-17.1・Issue #110: 圧縮表のデータセルと同 role・name の ref 保持要素が
/// 別にあっても、対象セルは `NotInSnapshot` のまま（別要素の ref を割り当てず Ambiguous にもしない）。
#[test]
fn aisnap_10_compressed_cell_with_same_signature_holder_is_not_in_snapshot() {
    let rows: String = (0..60)
        .map(|i| format!("<tr><td class=\"c{i}\">row{i}</td></tr>"))
        .collect();
    let html = page(&format!(
        "<div role=\"cell\">row59</div><table><thead><tr><th>n</th></tr></thead><tbody>{rows}</tbody></table>"
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

/// AISNAP-10・Issue #1221（修正 #1228）: 変化後に対象と別のセレクタ一致先へ class が移っても、
/// 変化後はセレクタを使わないので、role+name と ref が維持されている対象は `Stable`（判定表 A2。
/// codex P1 の指摘）。
#[test]
fn aisnap_10_selector_moved_to_other_element_keeps_stable() {
    let before = page("<button class=\"target\">Go</button><button>Stop</button>");
    let after = page("<button>Go</button><button class=\"target\">Stop</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "e9ddb5239".into(),
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A4: 属性セレクタでも変化後の位置を根拠にせず、
/// 後ろへの同名追加は `Ambiguous`（構造セレクタ以外でも穴が無いこと）。
#[test]
fn aisnap_10_attribute_selector_insert_is_ambiguous() {
    let before = page("<button data-k=\"t\">Go</button>");
    let after = page("<button data-k=\"t\">Go</button><button data-k=\"u\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "[data-k=\"t\"]",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A2: `id` を持つ対象は ref が出現順に依存しない
/// ので、後ろに同名要素が増えても `Stable`。
#[test]
fn aisnap_10_discriminated_target_appended_after_is_stable() {
    let before = page("<button id=\"t\">Go</button>");
    let after = page("<button id=\"t\">Go</button><button>Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#t",
    });
    assert_eq!(
        result,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "e51bd2d70".into(),
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A3: 重複グループが変わらない無関係な変化
/// （先頭への段落追加）では、1 件目も 2 件目（`Go #2`）も `Stable`。
#[test]
fn aisnap_10_duplicates_unrelated_change_is_stable() {
    let before = page("<button class=\"t\">Go</button><button>Go</button>");
    let after = page("<p>note</p><button class=\"t\">Go</button><button>Go</button>");
    let first = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".t",
    });
    assert_eq!(
        first,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go".into(),
            r#ref: "e9ddb5239".into(),
        }
    );
    let before = page("<button>Go</button><button class=\"t\">Go</button>");
    let after = page("<p>note</p><button>Go</button><button class=\"t\">Go</button>");
    let second = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".t",
    });
    assert_eq!(
        second,
        Reidentification::Stable {
            role: "button".into(),
            name: "Go #2".into(),
            r#ref: "e9ddb5239-2".into(),
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A4: 対象が hidden になり、別の識別属性を持つ
/// 同名要素が加わって、同じ件数の同名グループが残る変化は検出できる（#4 の検出可能な版）。
#[test]
fn aisnap_10_hidden_target_replaced_by_distinct_ref_is_ambiguous() {
    let before = page("<button class=\"target\">Go</button><button>Go</button>");
    let after = page(
        "<button class=\"target\" hidden>Go</button><button>Go</button><button id=\"n\">Go</button>",
    );
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".target",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A6: 前の同名要素が消えて `id` 付き対象の区別子
/// `#2` が外れても、ref は同じ要素を指す。基底が同じで #n がずれただけなので `Ambiguous`。
#[test]
fn aisnap_10_suffix_shift_with_ref_kept_is_ambiguous() {
    let before = page("<button>Go</button><button id=\"t\">Go</button>");
    let after = page("<button id=\"t\">Go</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#t",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 1
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A7: accname が同じで隣接テキストだけが変わる
/// 変化は、順序に依存しない ref（`id`）で特定できるので `SignatureChanged`。
#[test]
fn aisnap_10_adjacent_text_change_is_signature_changed() {
    let before = page("<input type=\"checkbox\" id=\"c\"> checkbox 1");
    let after = page("<input type=\"checkbox\" id=\"c\"> checkbox one");
    let names = |html: &str| -> Vec<String> {
        holders_of(html)
            .into_iter()
            .filter(|h| h.role == "checkbox")
            .map(|h| h.name)
            .collect()
    };
    assert_eq!(names(&before), vec!["checkbox 1".to_string()]);
    assert_eq!(names(&after), vec!["checkbox one".to_string()]);
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: "#c",
    });
    assert_eq!(
        result,
        Reidentification::SignatureChanged {
            before: ("checkbox".into(), "checkbox 1".into()),
            after: ("checkbox".into(), "checkbox one".into()),
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）・判定表 A8: 出現順に依存する ref（識別属性なしの重複）の
/// 要素で表示 name だけが変わる場合、ref を持つ別要素が対象だと確かめられず `Ambiguous`。
#[test]
fn aisnap_10_positional_ref_display_change_is_ambiguous() {
    let before = page("<input type=\"checkbox\" class=\"t\"> a<br><input type=\"checkbox\"> b");
    let after = page("<input type=\"checkbox\" class=\"t\"> c<br><input type=\"checkbox\"> b");
    let result = check_reidentification(&StabilityCase {
        before_html: &before,
        after_html: &after,
        selector: ".t",
    });
    assert_eq!(
        result,
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
}

/// AISNAP-10・Issue #1222・判定表 A1: ページ由来の name `Go #2` と衝突する場合は次の番号
/// `Go #3` へ進み、対象は一意に再特定できる（`Stable`）。
#[test]
fn aisnap_10_literal_suffix_collision_resolves_to_next_number() {
    let html = page("<button>Go</button><button id=\"t\">Go</button><button>Go #2</button>");
    let result = check_reidentification(&StabilityCase {
        before_html: &html,
        after_html: &html,
        selector: "#t",
    });
    match result {
        Reidentification::Stable { role, name, .. } => {
            assert_eq!(role, "button");
            assert_eq!(name, "Go #3");
        }
        other => panic!("{other:?}"),
    }
}

/// AISNAP-10・Issue #1222・判定表 A1: literal の `Go #2`…が上限分続くと解消できず、
/// 重複が残って `Ambiguous`（fail-closed）。直前の長さ（上限 - 1 件）なら解消して `Stable`。
#[test]
fn aisnap_10_collision_beyond_attempt_limit_is_ambiguous() {
    let k = crate::snapshot::MAX_SUFFIX_ATTEMPTS;
    let build = |n: usize| {
        let lits: String = (2..2 + n)
            .map(|i| format!("<button>Go #{i}</button>"))
            .collect();
        page(&format!(
            "<button>Go</button><button id=\"t\">Go</button>{lits}"
        ))
    };
    let over = build(k);
    assert_eq!(
        check_reidentification(&StabilityCase {
            before_html: &over,
            after_html: &over,
            selector: "#t",
        }),
        Reidentification::Ambiguous {
            side: Side::After,
            count: 2
        }
    );
    let ok = build(k - 1);
    match check_reidentification(&StabilityCase {
        before_html: &ok,
        after_html: &ok,
        selector: "#t",
    }) {
        Reidentification::Stable { name, .. } => assert_eq!(name, format!("Go #{}", k + 1)),
        other => panic!("{other:?}"),
    }
}

fn target(role: &str, name: &str, group: &str, r: &str) -> Target {
    Target {
        role: role.into(),
        name: name.into(),
        group_name: group.into(),
        r#ref: r.into(),
    }
}

fn facts(lookup: Lookup, oi: bool, same: bool, group: usize, family: usize) -> Facts {
    Facts {
        lookup,
        ref_order_independent: oi,
        context_unchanged: same,
        group_after: group,
        family_after: family,
    }
}

fn elsewhere(role: &str, name: &str, group: &str) -> Lookup {
    Lookup::RefElsewhere {
        role: role.into(),
        name: name.into(),
        group_name: group.into(),
    }
}

fn stable_go() -> Reidentification {
    Reidentification::Stable {
        role: "button".into(),
        name: "Go".into(),
        r#ref: "eabc".into(),
    }
}

/// AISNAP-10・Issue #1221（修正 #1228）: 判定表の変化後の行 A1〜A9 を `Facts` から直接検証する。
#[test]
fn aisnap_10_decide_table_rows() {
    let t = || target("button", "Go", "Go", "eabc");
    let amb = |count| Reidentification::Ambiguous {
        side: Side::After,
        count,
    };
    // 行 A1
    assert_eq!(
        decide(t(), facts(Lookup::ManyByName(3), true, true, 3, 1)),
        amb(3)
    );
    // 行 A2（context_unchanged に関係なく Stable）
    assert_eq!(
        decide(t(), facts(Lookup::Agree, true, false, 2, 1)),
        stable_go()
    );
    // 行 A3
    assert_eq!(
        decide(t(), facts(Lookup::Agree, false, true, 2, 2)),
        stable_go()
    );
    // 行 A4
    assert_eq!(
        decide(t(), facts(Lookup::Agree, false, false, 2, 2)),
        amb(2)
    );
    // 行 A5
    assert_eq!(
        decide(
            t(),
            facts(
                Lookup::OneByNameRefGone {
                    after_ref: "eold".into()
                },
                true,
                true,
                1,
                1
            )
        ),
        Reidentification::RefChanged {
            before: "eabc".into(),
            after: "eold".into()
        }
    );
    // 行 A6
    assert_eq!(
        decide(
            t(),
            facts(elsewhere("button", "Go #2", "Go"), true, false, 2, 1)
        ),
        amb(2)
    );
    // 行 A7
    assert_eq!(
        decide(
            t(),
            facts(elsewhere("button", "Stop", "Stop"), true, true, 1, 1)
        ),
        Reidentification::SignatureChanged {
            before: ("button".into(), "Go".into()),
            after: ("button".into(), "Stop".into()),
        }
    );
    // 行 A8
    assert_eq!(
        decide(
            t(),
            facts(elsewhere("button", "Stop", "Stop"), false, true, 1, 2)
        ),
        amb(2)
    );
    // 行 A9
    assert_eq!(
        decide(t(), facts(Lookup::Gone, true, true, 0, 0)),
        Reidentification::MissingAfter {
            role: "button".into(),
            name: "Go".into()
        }
    );
}

/// AISNAP-10・Issue #1221（修正 #1228）: 順序に依存する ref（`-n` 付き）の `Agree` は A2 ではなく
/// A3・A4 になり、`Ambiguous` の件数は 0 にならない。
#[test]
fn aisnap_10_decide_positional_ref_and_count_floor() {
    let t = target("button", "Go #2", "Go", "eabc-2");
    let f = observe(
        &t,
        &[
            holder("button", "Go", "Go", "eabc"),
            holder("button", "Go #2", "Go", "eabc-2"),
        ],
        &[holder("button", "Go #2", "Go", "eabc-2")],
    );
    assert_eq!(f.lookup, Lookup::Agree);
    assert!(!f.ref_order_independent);
    assert!(!f.context_unchanged);
    assert_eq!(
        decide(t, f),
        Reidentification::Ambiguous {
            side: Side::After,
            count: 1
        }
    );
    // 件数 0 でも `Ambiguous` の count は 1 以上（行 A4・A6・A8）。
    let t = || target("button", "Go", "Go", "eabc");
    for f in [
        facts(Lookup::Agree, false, false, 0, 0),
        facts(elsewhere("button", "Go #2", "Go"), true, false, 0, 0),
        facts(elsewhere("link", "x", "x"), false, false, 0, 0),
    ] {
        assert_eq!(
            decide(t(), f),
            Reidentification::Ambiguous {
                side: Side::After,
                count: 1
            }
        );
    }
}

fn holder(role: &str, name: &str, group: &str, r: &str) -> RefHolder {
    RefHolder {
        node: parse("").root(),
        role: role.into(),
        name: name.into(),
        group_name: group.into(),
        r#ref: r.into(),
    }
}

/// AISNAP-10・Issue #1221（修正 #1228）: ref ファミリのキーは末尾の `-<数字>` だけを除く。
#[test]
fn aisnap_10_ref_family_key_strips_only_occurrence_suffix() {
    assert_eq!(ref_family_key("e1a2b3c4d5"), "e1a2b3c4d5");
    assert_eq!(ref_family_key("e1a2b3c4d5-12"), "e1a2b3c4d5");
    assert_eq!(ref_family_key("e1a2b3c4d5v2-3"), "e1a2b3c4d5v2");
    assert_eq!(ref_family_key("e1a2b3c4d5v2"), "e1a2b3c4d5v2");
    assert_eq!(ref_family_key("e1a2-"), "e1a2-");
    assert_eq!(ref_family_key("e1a2-x"), "e1a2-x");
}

/// AISNAP-10・Issue #1221（修正 #1228）・TASK-17.2: `benches/fixtures` の全ページで、変化なし
/// （前後が同じ holder 列）の各 ref 保持要素の判定は、衝突解消（#1222）により
/// すべて `Stable` になる。
#[test]
fn aisnap_10_unchanged_fixtures_are_stable_for_every_holder() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("フィクスチャディレクトリは読める")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".html"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 17, "{names:?}");
    let mut failures: Vec<String> = Vec::new();
    for n in &names {
        let holders = holders_of(&fixture(n));
        for h in &holders {
            let t = Target {
                role: h.role.clone(),
                name: h.name.clone(),
                group_name: h.group_name.clone(),
                r#ref: h.r#ref.clone(),
            };
            let f = observe(&t, &holders, &holders);
            let result = decide(t.clone(), f);
            let expected = Reidentification::Stable {
                role: t.role.clone(),
                name: t.name.clone(),
                r#ref: t.r#ref.clone(),
            };
            if result != expected {
                failures.push(format!("{n}: {t:?} -> {result:?}"));
            }
        }
    }
    assert_eq!(failures, Vec::<String>::new());
}
