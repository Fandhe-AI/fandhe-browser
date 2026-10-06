//! 参照破損率の算出と測定出力（`AISNAP-10`・TASK-17.3・Issue #112・`MS-2`）。
//!
//! 役割: フィクスチャ 19 ケース（`tests/fixtures/stability/`・TASK-17.2・#111）を親モジュールの
//! 再特定判定（[`super::check_reidentification`]・TASK-17.1・#110）へ流し、ケース別の判定と
//! 破損率（%）を求め、Markdown 表として出力する。結果の解釈（10% 以下の達成可否）は
//! #113（TASK-17.h1・人間）の担当で、ここでは閾値を assert せず現状の実測値を具体値で固定する。
//! 値が変わる改修（TASK-15 / TASK-16 等）が入るとテストが落ち、レポート
//! （`docs/design/reference-stability-report.md`）の更新を強制する。
//!
//! 配置: 判定ヘルパーと `build_snapshot_with_holders` が crate 内 `#[cfg(test)]` のため、
//! spec 記載の `tests/reference_stability.rs`（結合テスト）ではなく crate 内の子モジュールに置く。
//!
//! 集計ルール: `Stable` は非破損。`RefChanged`・`SignatureChanged`・`MissingAfter`・
//! `Ambiguous { After }`・`OmittedFromSnapshot` は破損。`ref_expected: false` の
//! `NotInSnapshot` は分母から除外する。契約違反・ケース定義の誤りはエラーにする。
//! 診断値として、判定ヘルパーが要求するシグネチャの一意性を外し「変化前の ref が変化後も同じ
//! 対象を一意に指すか」だけを見る ref 同一性ベースの集計も併記する。
//! なお ref 同一性は「変化前後で同じセレクタが対象を一意に選ぶ」ことを前提にした診断値で、
//! 変化後にセレクタが一致しなくなったケースは ref が残っていても `Differs` になる（セレクタ依存）。
//! 変化後の対象を独立に識別するケース情報は現状の `cases.json` に無いため、定義の明示に留める。

use std::path::PathBuf;

use super::{
    Located, Reidentification, Side, StabilityCase, check_reidentification, locate, parse,
};
use crate::snapshot::build::build_snapshot_with_holders;
use fandhe_browser_core::dom::Document;
use fandhe_browser_core::query::query_selector_all_str;

/// `cases.json` 1 件分。
struct CaseDef {
    id: String,
    selector: String,
    poc_case: Option<u64>,
    ref_expected: bool,
}

/// 破損の種別（真偽値に潰さない。REPAIR-4）。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BreakKind {
    RefChanged,
    SignatureChanged,
    MissingAfter,
    Ambiguous,
    Omitted,
}

/// ケースの集計上の扱い。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Verdict {
    Stable,
    Broken(BreakKind),
    /// 設計上 ref を持たないため分母から除外（`ref_expected: false`）。
    Excluded,
}

/// 変化前の ref が変化後も同じ対象を一意に指すか（診断値）。
///
/// 「同じ対象」は変化後も同一セレクタが一意に選ぶ要素で判定する（セレクタ依存）。変化後にセレクタが
/// 一致しない場合は ref が残っていても `Differs` となる。判定ヘルパーの契約（変化後は role+name と
/// ref で再特定）とは別の定義である。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RefIdentity {
    Same,
    Differs,
    NoRefBefore,
}

/// 集計。`measured = stable + broken`。
#[derive(Debug, PartialEq, Eq, Default)]
struct Summary {
    measured: usize,
    broken: usize,
    excluded: usize,
}

/// 1 ケース分の測定結果。
struct Row {
    def: CaseDef,
    verdict: Verdict,
    identity: RefIdentity,
    role: String,
    name: String,
    /// 変化後の同一 role+name 候補数（ref 保持要素）。
    after_candidates: usize,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("stability")
}

/// `id` はパス結合に使うため、小文字英数字とハイフンのみを許す（パストラバーサル防止）。
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn load_cases() -> Result<Vec<CaseDef>, String> {
    let text = std::fs::read_to_string(fixtures_dir().join("cases.json"))
        .map_err(|e| format!("cannot read cases.json: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("invalid cases.json: {e}"))?;
    let items = value.as_array().ok_or("cases.json is not an array")?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("case without id")?;
        if !is_safe_id(id) {
            return Err(format!("unsafe case id: {id}"));
        }
        let selector = item
            .get("selector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{id}: missing selector"))?;
        out.push(CaseDef {
            id: id.to_string(),
            selector: selector.to_string(),
            poc_case: item.get("poc_case").and_then(|v| v.as_u64()),
            ref_expected: item
                .get("ref_expected")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        });
    }
    if out.len() < 15 {
        return Err(format!("AISNAP-10 requires >= 15 cases, got {}", out.len()));
    }
    Ok(out)
}

fn read_html(id: &str, file: &str) -> Result<String, String> {
    std::fs::read_to_string(fixtures_dir().join(id).join(file))
        .map_err(|e| format!("{id}/{file}: {e}"))
}

/// 判定結果を集計上の扱いへ写す。契約違反・ケース定義の誤りは `Err`。
fn classify(result: &Reidentification, ref_expected: bool) -> Result<Verdict, String> {
    use Reidentification as R;
    match (result, ref_expected) {
        (R::NotInSnapshot { .. }, false) => Ok(Verdict::Excluded),
        (R::NotInSnapshot { .. }, true) => {
            Err("NotInSnapshot without ref_expected: false".to_string())
        }
        (_, false) => Err("ref_expected: false but the target has a ref".to_string()),
        (R::Stable { .. }, true) => Ok(Verdict::Stable),
        (R::RefChanged { .. }, true) => Ok(Verdict::Broken(BreakKind::RefChanged)),
        (R::SignatureChanged { .. }, true) => Ok(Verdict::Broken(BreakKind::SignatureChanged)),
        (R::MissingAfter { .. }, true) => Ok(Verdict::Broken(BreakKind::MissingAfter)),
        (R::OmittedFromSnapshot { .. }, true) => Ok(Verdict::Broken(BreakKind::Omitted)),
        (
            R::Ambiguous {
                side: Side::After, ..
            },
            true,
        ) => Ok(Verdict::Broken(BreakKind::Ambiguous)),
        (
            R::Ambiguous {
                side: Side::Before, ..
            },
            true,
        ) => Err("selector matches multiple elements before the change".to_string()),
        (R::TargetNotFound { .. }, true) => Err("selector matches nothing before".to_string()),
    }
}

/// 率（%）を小数第 1 位へ整数演算で丸めて文字列化する（浮動小数を使わない。四捨五入）。
fn percent(num: usize, den: usize) -> Result<String, String> {
    if den == 0 {
        return Err("denominator is zero".to_string());
    }
    let tenths = (num * 1000 + den / 2) / den;
    Ok(format!("{}.{}", tenths / 10, tenths % 10))
}

/// 変化前の ref が変化後も同じ対象を一意に指すかと、変化後の同シグネチャ候補数を求める。
fn diagnose(def: &CaseDef, before: &str, after: &str) -> (RefIdentity, usize) {
    let b_doc = parse(before);
    let (_, b_holders) = build_snapshot_with_holders(&b_doc).expect("構築は成功する");
    let a_doc = parse(after);
    let (_, a_holders) = build_snapshot_with_holders(&a_doc).expect("構築は成功する");
    let pick = |doc: &Document| {
        query_selector_all_str(doc, doc.root(), &def.selector)
            .ok()
            .and_then(|m| match m.as_slice() {
                [id] => Some(*id),
                _ => None,
            })
    };
    let b_holder = pick(&b_doc).and_then(|id| b_holders.iter().find(|h| h.node == id));
    let Some(b_holder) = b_holder else {
        return (RefIdentity::NoRefBefore, 0);
    };
    let candidates = a_holders
        .iter()
        .filter(|h| h.role == b_holder.role && h.name == b_holder.name)
        .count();
    let same_ref: Vec<_> = a_holders
        .iter()
        .filter(|h| h.r#ref == b_holder.r#ref)
        .collect();
    let target = pick(&a_doc);
    let identity = match same_ref.as_slice() {
        [one] if Some(one.node) == target => RefIdentity::Same,
        _ => RefIdentity::Differs,
    };
    (identity, candidates)
}

fn measure_all() -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for def in load_cases()? {
        let before = read_html(&def.id, "before.html")?;
        let after = read_html(&def.id, "after.html")?;
        let result = check_reidentification(&StabilityCase {
            before_html: &before,
            after_html: &after,
            selector: &def.selector,
        });
        let verdict =
            classify(&result, def.ref_expected).map_err(|e| format!("{}: {e}", def.id))?;
        let (role, name) = match locate(&before, &def.selector) {
            Located::Found { role, name, .. } => (role, name),
            _ => (String::new(), String::new()),
        };
        let (identity, after_candidates) = diagnose(&def, &before, &after);
        rows.push(Row {
            def,
            verdict,
            identity,
            role,
            name,
            after_candidates,
        });
    }
    Ok(rows)
}

/// 行の部分集合を判定ベースで集計する。
fn summarize<'a>(rows: impl Iterator<Item = &'a Row>) -> Summary {
    let mut s = Summary::default();
    for r in rows {
        match r.verdict {
            Verdict::Excluded => s.excluded += 1,
            Verdict::Stable => s.measured += 1,
            Verdict::Broken(_) => {
                s.measured += 1;
                s.broken += 1;
            }
        }
    }
    s
}

/// ref 同一性ベースで集計する（除外ケースは分母に入れない）。
fn summarize_identity<'a>(rows: impl Iterator<Item = &'a Row>) -> Summary {
    let mut s = Summary::default();
    for r in rows {
        if r.verdict == Verdict::Excluded {
            s.excluded += 1;
            continue;
        }
        s.measured += 1;
        if r.identity != RefIdentity::Same {
            s.broken += 1;
        }
    }
    s
}

fn fmt_summary(label: &str, s: &Summary) -> Result<String, String> {
    Ok(format!(
        "{label}: {}/{} = {}% (excluded {})",
        s.broken,
        s.measured,
        percent(s.broken, s.measured)?,
        s.excluded
    ))
}

fn truncate(s: &str) -> String {
    let t: String = s.chars().take(24).collect();
    t.replace('|', "/")
}

fn render_report(rows: &[Row]) -> Result<String, String> {
    let mut out = String::from(
        "| case | poc | role | name | verdict | same-signature candidates after | ref identity |\n\
         | ---- | --- | ---- | ---- | ------- | -------------------------------- | ------------ |\n",
    );
    for r in rows {
        let poc = r.def.poc_case.map_or("-".to_string(), |n| n.to_string());
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:?} | {} | {:?} |\n",
            r.def.id,
            poc,
            r.role,
            truncate(&r.name),
            r.verdict,
            r.after_candidates,
            r.identity
        ));
    }
    let poc = |r: &&Row| r.def.poc_case.is_some();
    out.push('\n');
    out.push_str(&fmt_summary(
        "all (helper verdict)",
        &summarize(rows.iter()),
    )?);
    out.push('\n');
    out.push_str(&fmt_summary(
        "poc-equivalent (helper verdict)",
        &summarize(rows.iter().filter(poc)),
    )?);
    out.push('\n');
    out.push_str(&fmt_summary(
        "all (ref identity)",
        &summarize_identity(rows.iter()),
    )?);
    out.push('\n');
    out.push_str(&fmt_summary(
        "poc-equivalent (ref identity)",
        &summarize_identity(rows.iter().filter(poc)),
    )?);
    out.push('\n');
    Ok(out)
}

/// AISNAP-10・TASK-17.3・Issue #112: 率の整形（整数演算・四捨五入・分母 0 はエラー）。
#[test]
fn aisnap_10_percent_formatting() {
    assert_eq!(percent(1, 15).as_deref(), Ok("6.7"));
    assert_eq!(percent(4, 15).as_deref(), Ok("26.7"));
    assert_eq!(percent(0, 14).as_deref(), Ok("0.0"));
    assert_eq!(percent(14, 14).as_deref(), Ok("100.0"));
    assert!(percent(1, 0).is_err());
}

/// AISNAP-10・TASK-17.3・Issue #112: 判定から集計上の扱いへの写像。
#[test]
fn aisnap_10_classify_mapping() {
    use Reidentification as R;
    let s = |x: &str| x.to_string();
    let stable = R::Stable {
        role: s("r"),
        name: s("n"),
        r#ref: s("e1"),
    };
    assert_eq!(classify(&stable, true), Ok(Verdict::Stable));
    assert!(classify(&stable, false).is_err());
    let ambiguous = R::Ambiguous {
        side: Side::After,
        count: 2,
    };
    assert_eq!(
        classify(&ambiguous, true),
        Ok(Verdict::Broken(BreakKind::Ambiguous))
    );
    let before_amb = R::Ambiguous {
        side: Side::Before,
        count: 2,
    };
    assert!(classify(&before_amb, true).is_err());
    let changed = R::RefChanged {
        before: s("a"),
        after: s("b"),
    };
    assert_eq!(
        classify(&changed, true),
        Ok(Verdict::Broken(BreakKind::RefChanged))
    );
    let missing = R::MissingAfter {
        role: s("r"),
        name: s("n"),
    };
    assert_eq!(
        classify(&missing, true),
        Ok(Verdict::Broken(BreakKind::MissingAfter))
    );
    let not_in = R::NotInSnapshot {
        role: s("cell"),
        name: s("n"),
    };
    assert_eq!(classify(&not_in, false), Ok(Verdict::Excluded));
    assert!(classify(&not_in, true).is_err());
    assert!(classify(&R::TargetNotFound { side: Side::Before }, true).is_err());
}

/// AISNAP-10・TASK-17.3・Issue #112（受入基準）: 全ケースの判定結果と破損率（%）を具体値で固定し
/// 出力する。`cargo test -p fandhe-browser-ai --lib reference_stability -- --nocapture` で表示。
/// 10% 閾値は assert しない（達成可否の判断は #113）。
#[test]
fn aisnap_10_measure_break_rate() {
    let rows = measure_all().expect("measurement succeeds");
    println!("{}", render_report(&rows).expect("report renders"));

    let verdicts: Vec<(&str, Verdict)> = rows
        .iter()
        .map(|r| (r.def.id.as_str(), r.verdict))
        .collect();
    let amb = Verdict::Broken(BreakKind::Ambiguous);
    let st = Verdict::Stable;
    assert_eq!(
        verdicts,
        vec![
            ("01-login-submit", st),
            ("02-login-username", st),
            ("03-dropdown-select", st),
            ("04-checkbox-first", amb),
            ("05-number-input", st),
            ("06-quote-text", amb),
            ("07-quote-tag-link", amb),
            ("08-hn-first-title", st),
            ("09-hn-more-link", st),
            ("10-ec-price", amb),
            ("11-ec-product-link", st),
            ("12-table-header-cell", amb),
            ("13-table-data-cell", Verdict::Excluded),
            ("14-python-download-link", st),
            ("15-wiki-language-link", st),
            ("16-login-password", st),
            ("17-hn-second-title", st),
            ("18-quotes-author-link", amb),
            ("19-quotes-login-link", st),
        ]
    );

    // ケース別診断値（変化後の同シグネチャ候補数・ref 同一性）もレポートと一致させて固定する。
    // 候補数や ref 同一性だけが変わってもレポート更新を強制する。
    use RefIdentity::{NoRefBefore, Same};
    let diagnostics: Vec<(&str, usize, RefIdentity)> = rows
        .iter()
        .map(|r| (r.def.id.as_str(), r.after_candidates, r.identity))
        .collect();
    assert_eq!(
        diagnostics,
        vec![
            ("01-login-submit", 1, Same),
            ("02-login-username", 1, Same),
            ("03-dropdown-select", 1, Same),
            ("04-checkbox-first", 2, Same),
            ("05-number-input", 1, Same),
            ("06-quote-text", 71, Same),
            ("07-quote-tag-link", 2, Same),
            ("08-hn-first-title", 1, Same),
            ("09-hn-more-link", 1, Same),
            ("10-ec-price", 286, Same),
            ("11-ec-product-link", 1, Same),
            ("12-table-header-cell", 2, Same),
            ("13-table-data-cell", 0, NoRefBefore),
            ("14-python-download-link", 1, Same),
            ("15-wiki-language-link", 1, Same),
            ("16-login-password", 1, Same),
            ("17-hn-second-title", 1, Same),
            ("18-quotes-author-link", 10, Same),
            ("19-quotes-login-link", 1, Same),
        ]
    );

    let excluded: Vec<&str> = rows
        .iter()
        .filter(|r| !r.def.ref_expected)
        .map(|r| r.def.id.as_str())
        .collect();
    assert_eq!(excluded, vec!["13-table-data-cell"]);

    let all = summarize(rows.iter());
    assert_eq!(
        all,
        Summary {
            measured: 18,
            broken: 6,
            excluded: 1
        }
    );
    assert_eq!(percent(all.broken, all.measured).as_deref(), Ok("33.3"));
    let poc = summarize(rows.iter().filter(|r| r.def.poc_case.is_some()));
    assert_eq!(
        poc,
        Summary {
            measured: 14,
            broken: 5,
            excluded: 1
        }
    );
    assert_eq!(percent(poc.broken, poc.measured).as_deref(), Ok("35.7"));
}

/// AISNAP-10・TASK-17.3・Issue #112: ref 同一性ベースの診断値（判定ヘルパーのシグネチャ一意性
/// 要求を外した場合の破損率）を具体値で固定する。
#[test]
fn aisnap_10_measure_ref_identity_diagnostic() {
    let rows = measure_all().expect("measurement succeeds");
    let all = summarize_identity(rows.iter());
    let poc = summarize_identity(rows.iter().filter(|r| r.def.poc_case.is_some()));
    assert_eq!(
        all,
        Summary {
            measured: 18,
            broken: 0,
            excluded: 1
        }
    );
    assert_eq!(
        poc,
        Summary {
            measured: 14,
            broken: 0,
            excluded: 1
        }
    );
    assert_eq!(percent(all.broken, all.measured).as_deref(), Ok("0.0"));
}
