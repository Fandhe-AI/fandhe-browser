//! `compare.rs` のテスト（TASK-22.2・`AISNAP-9`・Issue #125）。
//!
//! 回答は合成データだけで組む。ここで確認する「50 件が揃う」「23/25」等は収集機構と採点器の検算で、
//! 実エージェントの測定結果ではない（実測は親 #122。`REPAIR-3`）。
//! 実験用の入口は `aisnap9_write_packets_if_requested`（パケット生成）と
//! `aisnap9_collect_answers_if_requested`（回収検査）。

// 未使用の公開項目は本 target では使わない。
#[allow(dead_code)]
#[path = "compare.rs"]
mod compare;
#[allow(dead_code)]
#[path = "generate_reduced.rs"]
mod generate_reduced;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/raw_dom.rs"]
mod raw_dom;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/retention_check.rs"]
mod retention_check;
#[allow(dead_code)]
#[path = "score.rs"]
mod score;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/snapshot_text.rs"]
mod snapshot_text;
#[allow(dead_code)]
#[path = "tasks.rs"]
mod tasks;

use compare::{
    Collected, DIFF_THRESHOLD_POINTS, MethodDiff, RAW_ALLOWED_SELECTOR_EXAMPLES,
    RAW_FORBIDDEN_SELECTOR_EXAMPLES, ReportMeta, SELECT_VALUE_NOTE, allocation, build_report,
    check_out_dir, collect, compare_results, instruction, packets, render_allocation_json,
    render_allocation_markdown, render_report_json, render_report_markdown, render_status_json,
    write_merged, write_packets, write_report,
};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;
use generate_reduced::{InputMode, raw_dom_documents, resolve_golden, task_inputs};
use score::{FailureClass, Tally, TaskResult, resolve_raw_dom, score};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tasks::{Category, GOLDEN, Golden, TASKS};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    manifest().join("benches").join("fixtures")
}

/// テスト専用の一意な temp ディレクトリ（後始末は呼び出し側）。
fn temp_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("fandhe-aisnap9-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn first_of(id: &str) -> InputMode {
    allocation(&TASKS)
        .into_iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("{id} missing"))
        .first
}

/// 割付表が設計書の規則（種別ごとに交互・余りは次種別の先頭から）どおりになる（AISNAP-9・TASK-22.2）。
#[test]
fn aisnap9_allocation_matches_design() {
    use InputMode::{RawDom as R, Reduced as B};
    let expected: [(&str, InputMode); 25] = [
        ("click-01", B),
        ("click-02", R),
        ("click-03", B),
        ("click-04", R),
        ("click-05", B),
        ("click-06", R),
        ("click-07", B),
        ("extract-01", R),
        ("extract-02", B),
        ("extract-03", R),
        ("extract-04", B),
        ("extract-05", R),
        ("extract-06", B),
        ("form-01", R),
        ("form-02", B),
        ("form-03", R),
        ("form-04", B),
        ("form-05", R),
        ("form-06", B),
        ("nav-01", R),
        ("nav-02", B),
        ("nav-03", R),
        ("nav-04", B),
        ("nav-05", R),
        ("nav-06", B),
    ];
    let rows = allocation(&TASKS);
    assert_eq!(rows.len(), 25);
    for (id, mode) in expected {
        assert_eq!(first_of(id), mode, "{id}");
    }
    let reduced_first = rows.iter().filter(|r| r.first == B).count();
    assert_eq!((reduced_first, rows.len() - reduced_first), (13, 12));
    for c in [
        Category::Click,
        Category::Extract,
        Category::Form,
        Category::Nav,
    ] {
        let in_cat: Vec<_> = rows.iter().filter(|r| r.category == c).collect();
        let b = in_cat.iter().filter(|r| r.first == B).count() as i64;
        let r = in_cat.len() as i64 - b;
        assert!((b - r).abs() <= 1, "{c:?}: {b} vs {r}");
    }
    let mut orders: Vec<u32> = rows.iter().flat_map(|r| r.run_order).collect();
    orders.sort_unstable();
    assert_eq!(orders, (1..=50).collect::<Vec<u32>>());
    assert_eq!(rows.first().map(|r| r.run_order), Some([1, 2]));
    assert_eq!(rows.last().map(|r| r.run_order), Some([49, 50]));
}

/// 入力順に依存しない（AISNAP-9・TASK-22.2）。
#[test]
fn aisnap9_allocation_ignores_input_order() {
    let mut rev = TASKS;
    rev.reverse();
    assert_eq!(allocation(&rev), allocation(&TASKS));
}

/// 割付表の出力形式（Markdown 表・JSON）を具体値で固定する（AISNAP-9・TASK-22.2）。
#[test]
fn aisnap9_allocation_render_snapshot() {
    let rows = allocation(&TASKS);
    let md = render_allocation_markdown(&rows);
    let lines: Vec<&str> = md.lines().collect();
    assert_eq!(lines.len(), 27);
    assert_eq!(
        lines[0],
        "| タスク id | 種別 | ページ | 先に提示する方式 | 実行番号（先, 後） |"
    );
    assert!(
        lines[2].starts_with("| click-01 | click | "),
        "{}",
        lines[2]
    );
    assert!(lines[2].ends_with("| 簡約 | 1, 2 |"), "{}", lines[2]);
    assert!(
        lines[9].starts_with("| extract-01 | extract | "),
        "{}",
        lines[9]
    );
    assert!(lines[9].ends_with("| 生 DOM | 15, 16 |"), "{}", lines[9]);
    let json: serde_json::Value =
        serde_json::from_str(&render_allocation_json(&rows)).expect("json");
    let arr = json.as_array().expect("array");
    assert_eq!(arr.len(), 25);
    assert_eq!(arr[0]["id"], "click-01");
    assert_eq!(arr[0]["first"], "reduced");
    assert_eq!(arr[0]["second"], "raw");
    assert_eq!(arr[0]["run_order"], serde_json::json!([1, 2]));
    assert_eq!(arr[1]["first"], "raw");
}

/// テンプレートが列挙する使用可能構文は core で解決でき、使用不可の構文は拒否される（乖離検出）。
#[test]
fn aisnap9_raw_template_selectors_match_core() {
    let html = "<body><h1>t</h1><p>p</p><form id=\"login\"><input type=\"text\"><button type=\"submit\" disabled class=\"item\">go</button></form><a href=\"/docs/x.pdf\" lang=\"en\" class=\"item\">d</a><ul><li>1</li></ul></body>";
    let doc = parse_document(html, &ParseOptions::default())
        .expect("parse")
        .document;
    for sel in RAW_ALLOWED_SELECTOR_EXAMPLES {
        let r = query_selector_all_str(&doc, doc.root(), sel);
        assert!(r.is_ok(), "allowed selector rejected: {sel}: {r:?}");
    }
    for sel in RAW_FORBIDDEN_SELECTOR_EXAMPLES {
        let r = query_selector_all_str(&doc, doc.root(), sel);
        assert!(r.is_err(), "forbidden selector accepted: {sel}");
    }
}

/// テンプレートに golden・比較の意図・他方式の回答形式が混入しない（ブラインド性。AISNAP-9）。
#[test]
fn aisnap9_templates_do_not_leak() {
    for t in &TASKS {
        let red = instruction(InputMode::Reduced, t);
        let raw = instruction(InputMode::RawDom, t);
        for text in [&red, &raw] {
            for banned in ["golden", "Golden", "比較", "compare", "判定", "方式"] {
                assert!(!text.contains(banned), "{}: {banned}", t.id);
            }
            assert!(text.contains(&format!("\"id\": \"{}\"", t.id)));
            assert!(text.contains(SELECT_VALUE_NOTE));
        }
        assert!(!red.contains("\"selector\""), "{}", t.id);
        assert!(!red.contains("\"index\""), "{}", t.id);
        assert!(!raw.contains("\"ref\""), "{}", t.id);
        if t.category != Category::Extract {
            assert!(red.contains("\"ref\""), "{}", t.id);
        }
        for (id, g) in GOLDEN.iter() {
            if *id == t.id
                && let Golden::Value { value, .. } = g
            {
                assert!(!red.contains(value) && !raw.contains(value), "{id}");
            }
        }
    }
}

/// パケットが 25 タスク × 2 方式を網羅し、自タスクの入力だけを含む（AISNAP-9・TASK-22.2）。
#[test]
fn aisnap9_packets_cover_25_x_2() {
    let all = packets(&fixtures()).expect("packets");
    assert_eq!(all.len(), 50);
    let names: BTreeSet<&str> = all.iter().map(|p| p.file_name.as_str()).collect();
    assert_eq!(names.len(), 50);
    assert!(names.contains("click-01-reduced.txt"));
    assert!(names.contains("nav-06-raw.txt"));
    for mode in [InputMode::Reduced, InputMode::RawDom] {
        let inputs = task_inputs(&fixtures(), mode).expect("inputs");
        for (t, inp) in TASKS.iter().zip(&inputs) {
            let p = all
                .iter()
                .find(|p| p.id == t.id && p.mode == mode)
                .unwrap_or_else(|| panic!("{} missing", t.id));
            assert!(p.body.contains(t.prompt), "{}", t.id);
            assert!(p.body.contains(&inp.input), "{}", t.id);
            assert!(p.body.contains(&instruction(mode, t)), "{}", t.id);
            for other in TASKS
                .iter()
                .filter(|o| o.id != t.id && o.prompt != t.prompt)
            {
                assert!(!p.body.contains(other.prompt), "{} has {}", t.id, other.id);
            }
        }
    }
}

/// 出力先の安全弁: 相対パス・`..`・リポジトリ配下を拒否する（誤コミット防止）。
#[test]
fn aisnap9_out_dir_check_rejects_unsafe_paths() {
    // OS ごとに絶対パスとなるよう temp_dir 起点で組み立てる（Windows はドライブ指定が必要）
    // macOS の /var→/private/var 等の正規化差でも成立するよう、temp_dir を正規化した上で組み立てる
    let tmp = std::env::temp_dir()
        .canonicalize()
        .expect("canonical temp dir");
    let root_buf = tmp.join("repo-root-for-test");
    let root = root_buf.as_path();
    assert!(check_out_dir(Path::new("relative/dir"), root).is_err());
    assert!(check_out_dir(&root.join("..").join("etc"), root).is_err());
    assert!(check_out_dir(&root.join("out"), root).is_err());
    assert!(check_out_dir(root, root).is_err());
    let other = tmp.join("other-root").join("out");
    assert!(check_out_dir(&other, root).is_ok());
}

/// リポジトリ内・相対パスへは書かない（AISNAP-9・TASK-22.2）。
#[test]
fn aisnap9_write_packets_rejects_repo_dir() {
    let inside = manifest()
        .join("target")
        .join("aisnap9-compare-must-not-exist");
    assert!(write_packets(&fixtures(), &inside).is_err());
    assert!(!inside.exists());
    assert!(write_packets(&fixtures(), Path::new("rel-out")).is_err());
}

/// temp ディレクトリへパケット 50 件 + 割付表 2 本を書く（AISNAP-9・TASK-22.2）。
#[test]
fn aisnap9_write_packets_to_temp_dir() {
    let dir = temp_dir("packets");
    let paths = write_packets(&fixtures(), &dir).expect("write");
    assert_eq!(paths.len(), 52);
    assert!(dir.join("click-01-reduced.txt").is_file());
    assert!(dir.join("form-06-raw.txt").is_file());
    assert!(dir.join("allocation.json").is_file());
    assert!(dir.join("allocation.md").is_file());
    let body = std::fs::read_to_string(dir.join("extract-02-raw.txt")).expect("read");
    assert!(!body.contains('\r'));
    let _ = std::fs::remove_dir_all(&dir);
}

/// golden から組んだ正答を回答ファイルとして書く。`null_ids` は回答不能（null）にする。
fn write_synthetic_answers(dir: &Path, null_ids: &[&str]) {
    std::fs::create_dir_all(dir).expect("mkdir");
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    for ((id, g), r) in GOLDEN.iter().zip(&resolved) {
        let ref_of = |i: usize| -> String {
            r.locators
                .get(i)
                .and_then(|l| l.refs.first())
                .cloned()
                .unwrap_or_default()
        };
        let null = null_ids.contains(id);
        let (red, raw) = match g {
            Golden::Ref { any_of } => {
                let l = any_of.first().expect("locator");
                (
                    serde_json::json!({"id": id, "ref": if null { None } else { Some(ref_of(0)) }}),
                    serde_json::json!({"id": id, "selector": if null { None } else { Some(l.selector) }, "index": if null { None } else { Some(l.index) }}),
                )
            }
            Golden::Value { value, .. } => {
                let v = if null { None } else { Some(*value) };
                (
                    serde_json::json!({"id": id, "value": v}),
                    serde_json::json!({"id": id, "value": v}),
                )
            }
            Golden::Steps(steps) => {
                let red: Vec<_> = steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| serde_json::json!({"action": s.action.as_str(), "ref": ref_of(i), "value": s.value}))
                    .collect();
                let raw: Vec<_> = steps
                    .iter()
                    .map(|s| serde_json::json!({"action": s.action.as_str(), "selector": s.target.selector, "index": s.target.index, "value": s.value}))
                    .collect();
                let (red, raw) = if null {
                    (serde_json::Value::Null, serde_json::Value::Null)
                } else {
                    (red.into(), raw.into())
                };
                (
                    serde_json::json!({"id": id, "steps": red}),
                    serde_json::json!({"id": id, "steps": raw}),
                )
            }
        };
        std::fs::write(dir.join(format!("{id}-reduced.json")), red.to_string()).expect("w");
        std::fs::write(dir.join(format!("{id}-raw.json")), raw.to_string()).expect("w");
    }
}

fn pass_counts(c: &Collected) -> (u32, u32) {
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    let red = score(&TASKS, &GOLDEN, &resolved, &c.reduced).expect("score reduced");
    let pages = raw_dom_documents(&fixtures()).expect("pages");
    let (res, ans) = resolve_raw_dom(&TASKS, &GOLDEN, &pages, &c.raw).expect("resolve raw");
    let raw = score(&TASKS, &GOLDEN, &res, &ans).expect("score raw");
    (red.overall.pass, raw.overall.pass)
}

/// 合成回答 50 件が揃えば完全で、結合 JSON が採点入口を通る（受入基準: 25 件それぞれで両方式の回答が収集される）。
#[test]
fn aisnap9_collect_complete_synthetic() {
    let dir = temp_dir("complete");
    write_synthetic_answers(&dir, &["click-01", "extract-02"]);
    let c = collect(&dir);
    assert!(c.status.is_complete(), "{}", render_status_json(&c.status));
    assert_eq!(c.status.reduced.accepted, 25);
    assert_eq!(c.status.raw.accepted, 25);
    assert_eq!(c.status.reduced.unanswered, ["click-01", "extract-02"]);
    assert_eq!(c.status.raw.unanswered, ["click-01", "extract-02"]);
    assert!(c.status.reduced.invalid.is_empty() && c.status.raw.invalid.is_empty());
    assert_eq!((c.reduced.len(), c.raw.len()), (25, 25));
    assert_eq!(pass_counts(&c), (23, 23));
    // 結合 JSON は既存の採点入口の形式（配列・25 件）
    let v: serde_json::Value = serde_json::from_str(&c.reduced_json).expect("json");
    assert_eq!(v.as_array().map(Vec::len), Some(25));
    let v: serde_json::Value = serde_json::from_str(&c.raw_json).expect("json");
    assert_eq!(v.as_array().map(Vec::len), Some(25));
    // 整形せずコンパクトに直列化され、採点入口の上限に収まる（改行は末尾 1 つのみ）
    assert_eq!(c.reduced_json.matches('\n').count(), 1);
    assert!(c.reduced_json.len() <= score::MAX_INPUT_BYTES);
    assert!(c.raw_json.len() <= score::MAX_INPUT_BYTES);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 出力先の既存シンボリックリンクは追跡せず拒否する（リポジトリ外制約の迂回防止）。
#[cfg(unix)]
#[test]
fn aisnap9_write_packets_rejects_symlinked_output_file() {
    let dir = temp_dir("symlink");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // 被害側も temp 内に実在する親ディレクトリで用意し、ガードが無ければ実際に書き込まれる状態にする
    let victim_dir = temp_dir("symlink-victim");
    std::fs::create_dir_all(&victim_dir).expect("mkdir victim");
    let target = victim_dir.join("victim.txt");
    std::os::unix::fs::symlink(&target, dir.join("allocation.json")).expect("symlink");
    assert!(write_packets(&fixtures(), &dir).is_err());
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&victim_dir);
}

/// 欠落・id 不一致・不正 JSON・上限超過は理由付きで記録され、完全にならない。
#[test]
fn aisnap9_collect_reports_missing_and_rejected() {
    let dir = temp_dir("incomplete");
    write_synthetic_answers(&dir, &[]);
    std::fs::remove_file(dir.join("click-03-reduced.json")).expect("rm");
    std::fs::write(
        dir.join("click-02-raw.json"),
        "{\"id\": \"click-05\", \"selector\": \"a\", \"index\": 0}",
    )
    .expect("w");
    std::fs::write(dir.join("nav-01-reduced.json"), "{not json").expect("w");
    std::fs::write(
        dir.join("nav-02-raw.json"),
        format!(
            "{{\"id\": \"nav-02\", \"value\": \"{}\"}}",
            "x".repeat(compare::MAX_ANSWER_FILE_BYTES)
        ),
    )
    .expect("w");
    std::fs::write(dir.join("nav-03-raw.json"), "[1]").expect("w");
    let c = collect(&dir);
    assert!(!c.status.is_complete());
    assert_eq!(c.status.reduced.missing, ["click-03"]);
    assert_eq!(c.status.reduced.rejected.len(), 1);
    assert_eq!(c.status.reduced.rejected[0].id, "nav-01");
    assert!(
        c.status.reduced.rejected[0]
            .reason
            .starts_with("invalid JSON")
    );
    assert_eq!(c.status.reduced.accepted, 23);
    assert!(c.status.raw.missing.is_empty());
    let reasons: Vec<(&str, &str)> = c
        .status
        .raw
        .rejected
        .iter()
        .map(|r| (r.id.as_str(), r.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("click-02", "id does not match the file name"),
            ("nav-02", "file exceeds size limit"),
            ("nav-03", "top level must be an object"),
        ]
    );
    assert_eq!(c.status.raw.accepted, 22);
    let status = render_status_json(&c.status);
    assert!(status.contains("\"complete\": false"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 空ディレクトリ・存在しないディレクトリは全件 missing（panic しない）。
#[test]
fn aisnap9_collect_empty_dir_is_all_missing() {
    let c = collect(&temp_dir("nonexistent"));
    assert_eq!(c.status.reduced.missing.len(), 25);
    assert_eq!(c.status.raw.missing.len(), 25);
    assert!(!c.status.is_complete());
}

/// 結合 JSON の書き出しもリポジトリ内を拒否する。
#[test]
fn aisnap9_write_merged_rejects_repo_dir() {
    let c = collect(&temp_dir("nonexistent2"));
    assert!(write_merged(&c, &manifest().join("target").join("aisnap9-merged-no")).is_err());
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

/// 実験用: `AGENT_EVAL_COMPARE_OUT=/abs/dir` でパケットと割付表をリポ外へ書く。
#[test]
fn aisnap9_write_packets_if_requested() {
    let Some(dir) = env_dir("AGENT_EVAL_COMPARE_OUT") else {
        return;
    };
    let paths = write_packets(&fixtures(), &dir).expect("write packets");
    println!("wrote {} files to {}", paths.len(), dir.display());
}

/// 実験用: `AGENT_EVAL_COMPARE_ANSWERS=/abs/dir` の回答を回収検査する。不完全なら失敗（fail-closed）。
/// `AGENT_EVAL_COMPARE_MERGED_OUT=/abs/dir` があれば結合 JSON 2 本をリポ外へ書く。
#[test]
fn aisnap9_collect_answers_if_requested() {
    let Some(dir) = env_dir("AGENT_EVAL_COMPARE_ANSWERS") else {
        return;
    };
    let c = collect(&dir);
    println!("{}", render_status_json(&c.status));
    assert!(c.status.is_complete(), "answers are incomplete");
    if let Some(out) = env_dir("AGENT_EVAL_COMPARE_MERGED_OUT") {
        write_merged(&c, &out).expect("write merged");
    }
}
// ---- 比較測定レポート（TASK-22.3・Issue #126） ----

fn meta() -> ReportMeta {
    ReportMeta::from_parts(
        Some("abc1234"),
        Some("2026-10-10"),
        Some("test-model"),
        None,
    )
    .expect("meta")
}

/// 合成回答（全問正解）から、簡約側だけ `reduced_null_ids` を回答不能に書き換えて回収する。
fn collect_with_reduced_nulls(name: &str, reduced_null_ids: &[&str]) -> Collected {
    let dir = temp_dir(name);
    write_synthetic_answers(&dir, &[]);
    for id in reduced_null_ids {
        let body = if id.starts_with("extract") {
            format!("{{\"id\": \"{id}\", \"value\": null}}")
        } else {
            format!("{{\"id\": \"{id}\", \"ref\": null}}")
        };
        std::fs::write(dir.join(format!("{id}-reduced.json")), body).expect("w");
    }
    let c = collect(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    c
}

/// 全タスクの `TaskResult` を `f(id)` の失敗分類で組む（None = 合格）。
fn results_with(f: impl Fn(&str) -> Option<FailureClass>) -> Vec<TaskResult> {
    TASKS
        .iter()
        .map(|t| {
            let failure = f(t.id);
            TaskResult {
                id: t.id.to_owned(),
                category: t.category,
                pass: failure.is_none(),
                failure,
            }
        })
        .collect()
}

fn tally(pass: u32, total: u32) -> Tally {
    Tally { pass, total }
}

/// 両方式とも全問正解なら全体は 25/25 で差 +0.0、達成（AISNAP-9・TASK-22.3）。
#[test]
fn aisnap9_report_all_pass() {
    let dir = temp_dir("rep-all");
    write_synthetic_answers(&dir, &[]);
    let c = collect(&dir);
    let r = build_report(&fixtures(), &c).expect("report");
    let md = render_report_markdown(&r, &meta());
    assert!(md.contains("| 全体 | 25 | 25 | 100.0% | 25 | 100.0% | +0.0 |\n"));
    assert!(r.including.meets && r.excluding_union.meets);
    assert!(md.contains("差 +0.0 pt → 達成"));
    assert!(md.contains("- 基準コミット: abc1234\n"));
    assert!(md.contains("- 関連 Issue: 未記入\n"));
    assert!(!md.contains('\r') && md.ends_with("|\n") && !md.ends_with("\n\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 簡約側だけ 2 件回答不能なら 23/25 対 25/25 で -8.0、未達。種別行にも具体値が出る。
#[test]
fn aisnap9_report_reduced_two_misses() {
    let c = collect_with_reduced_nulls("rep-two", &["click-01", "extract-02"]);
    let r = build_report(&fixtures(), &c).expect("report");
    let md = render_report_markdown(&r, &meta());
    assert!(md.contains("| 全体 | 25 | 23 | 92.0% | 25 | 100.0% | -8.0 |\n"));
    assert!(md.contains("| click | 7 | 6 | 85.7% | 7 | 100.0% | -14.3 |\n"));
    assert!(!r.including.meets);
    assert!(md.contains("差 -8.0 pt → 未達（しきい値 -5pt）"));
}

/// 1 件差（24/25 対 25/25 = -4.0）は達成。
#[test]
fn aisnap9_report_one_miss_meets() {
    let c = collect_with_reduced_nulls("rep-one", &["click-01"]);
    let r = build_report(&fixtures(), &c).expect("report");
    assert_eq!(r.including.overall.diff_text(), "-4.0");
    assert!(r.including.meets);
}

/// 生 DOM 方式の方が悪い場合は符号が正（25 対 23 = +8.0）で達成。
#[test]
fn aisnap9_report_raw_worse_is_positive() {
    let red = results_with(|_| None);
    let raw =
        results_with(|id| (id == "click-01" || id == "nav-01").then_some(FailureClass::Mismatch));
    let r = compare_results(&TASKS, &red, &raw).expect("compare");
    assert_eq!(r.including.overall.diff_text(), "+8.0");
    assert!(r.including.meets);
}

/// 整数比較の境界: 19/20 対 20/20 は -5.0 で達成（>=）、17/20 対 19/20 は -10.0 で未達。
#[test]
fn aisnap9_diff_threshold_boundary() {
    let a = MethodDiff {
        reduced: tally(19, 20),
        raw: tally(20, 20),
    };
    assert_eq!(a.diff_text(), "-5.0");
    assert!(a.meets(DIFF_THRESHOLD_POINTS));
    let b = MethodDiff {
        reduced: tally(17, 20),
        raw: tally(19, 20),
    };
    assert_eq!(b.diff_text(), "-10.0");
    assert!(!b.meets(DIFF_THRESHOLD_POINTS));
}

/// 丸めはゼロから遠い側へ寄せ、符号が対称になる。
#[test]
fn aisnap9_diff_rounding_is_symmetric() {
    let d = |p, r| MethodDiff {
        reduced: tally(p, 3),
        raw: tally(r, 3),
    };
    assert_eq!(d(1, 0).diff_text(), "+33.3");
    assert_eq!(d(0, 1).diff_text(), "-33.3");
    assert_eq!(d(2, 0).diff_text(), "+66.7");
    assert_eq!(d(0, 2).diff_text(), "-66.7");
}

/// 分母 0 または不一致は n/a・未達で、panic しない。
#[test]
fn aisnap9_diff_zero_or_mismatched_total_is_na() {
    let z = MethodDiff {
        reduced: tally(0, 0),
        raw: tally(0, 0),
    };
    assert_eq!((z.diff_text().as_str(), z.meets(-5)), ("n/a", false));
    let m = MethodDiff {
        reduced: tally(1, 2),
        raw: tally(1, 3),
    };
    assert_eq!((m.diff_text().as_str(), m.meets(-5)), ("n/a", false));
}

/// 和集合除外: 簡約側 A・生 DOM 側 B が golden_unresolved なら両方式から A・B を除き 23 件分母になる。
#[test]
fn aisnap9_union_exclusion() {
    let red = results_with(|id| (id == "click-01").then_some(FailureClass::GoldenUnresolved));
    let raw = results_with(|id| (id == "click-02").then_some(FailureClass::GoldenUnresolved));
    let r = compare_results(&TASKS, &red, &raw).expect("compare");
    assert_eq!(r.excluding_union.excluded, ["click-01", "click-02"]);
    assert_eq!(r.excluding_union.overall.reduced, tally(23, 23));
    assert_eq!(r.excluding_union.overall.raw, tally(23, 23));
    assert_eq!(r.including.overall.reduced, tally(24, 25));
    assert_eq!(r.including.overall.raw, tally(24, 25));
    let md = render_report_markdown(&r, &meta());
    assert!(md.contains("除外タスク: click-01, click-02\n"));
}

/// 種別を丸ごと除外すると、その行の差は n/a・未達になり panic しない。
#[test]
fn aisnap9_fully_excluded_category_is_na() {
    let click_ids: Vec<&str> = TASKS
        .iter()
        .filter(|t| t.category == Category::Click)
        .map(|t| t.id)
        .collect();
    let red = results_with(|id| {
        click_ids
            .contains(&id)
            .then_some(FailureClass::GoldenUnresolved)
    });
    let raw = results_with(|_| None);
    let r = compare_results(&TASKS, &red, &raw).expect("compare");
    let md = render_report_markdown(&r, &meta());
    assert!(md.contains("| click | 0 | 0 | n/a | 0 | n/a | n/a |\n"));
    let (_, click) = r.excluding_union.by_category[0];
    assert!(!click.meets(DIFF_THRESHOLD_POINTS));
    assert_eq!(r.excluding_union.overall.reduced.total, 18);
}

/// 回答ファイルが 1 つ欠けた状態ではレポートを作らない（fail-closed）。
#[test]
fn aisnap9_report_rejects_incomplete() {
    let dir = temp_dir("rep-inc");
    write_synthetic_answers(&dir, &[]);
    std::fs::remove_file(dir.join("click-01-raw.json")).expect("rm");
    let c = collect(&dir);
    assert!(matches!(
        build_report(&fixtures(), &c),
        Err(compare::CompareError::Incomplete)
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// id 集合が tasks と一致しない入力は拒否する。
#[test]
fn aisnap9_compare_rejects_mismatched_ids() {
    let red = results_with(|_| None);
    let mut raw = results_with(|_| None);
    raw.pop();
    assert!(compare_results(&TASKS, &red, &raw).is_err());
    let mut dup = results_with(|_| None);
    dup.pop();
    dup.push(dup[0].clone());
    assert!(compare_results(&TASKS, &dup, &red).is_err());
}

/// ページ別は task_pages() の順で、件数の合計が 25 になる。
#[test]
fn aisnap9_report_by_page_order_and_sum() {
    let red = results_with(|_| None);
    let r = compare_results(&TASKS, &red, &red).expect("compare");
    let names: Vec<&str> = r.including.by_page.iter().map(|(p, _)| *p).collect();
    assert_eq!(names, generate_reduced::task_pages());
    assert_eq!(names.len(), 13);
    let sum: u32 = r
        .including
        .by_page
        .iter()
        .map(|(_, d)| d.reduced.total)
        .sum();
    assert_eq!(sum, 25);
}

/// 付録のタスク別行は割付表と同じ「先に提示した方式」を示し、割付表と限界節の固定文を含む。
#[test]
fn aisnap9_report_appendix_and_limits() {
    let red = results_with(|_| None);
    let r = compare_results(&TASKS, &red, &red).expect("compare");
    let md = render_report_markdown(&r, &meta());
    let line = |id: &str| {
        md.lines()
            .find(|l| l.starts_with(&format!("| {id} |")))
            .unwrap_or_else(|| panic!("{id} row"))
            .to_owned()
    };
    assert!(line("click-01").contains("| 簡約 | 合格 | 合格 |"));
    assert!(line("extract-01").contains("| 生 DOM | 合格 | 合格 |"));
    assert!(md.contains("| タスク id | 種別 | ページ | 先に提示する方式 |"));
    assert!(md.contains("1 件 = 4pt"));
    assert!(md.contains("簡約方式に甘い"));
}

/// メタ情報は禁止文字・長さ超過を拒否し、未指定は「未記入」になる。
#[test]
fn aisnap9_report_meta_validation() {
    for bad in ["a|b", "a\nb", "<x>", "a\u{7}b"] {
        assert!(ReportMeta::from_parts(Some(bad), None, None, None).is_err());
    }
    let long = "a".repeat(129);
    assert!(ReportMeta::from_parts(None, Some(&long), None, None).is_err());
    let ok = "a".repeat(128);
    assert!(ReportMeta::from_parts(None, Some(&ok), None, None).is_ok());
    let m = ReportMeta::from_parts(None, Some("  "), None, None).expect("meta");
    assert_eq!(m.date, "未記入");
}

/// JSON 出力はパースでき、主判定の具体値を持つ。
#[test]
fn aisnap9_report_json() {
    let c = collect_with_reduced_nulls("rep-json", &["click-01", "extract-02"]);
    let r = build_report(&fixtures(), &c).expect("report");
    let v: serde_json::Value = serde_json::from_str(&render_report_json(&r)).expect("json");
    assert_eq!(v["threshold_points"], -5);
    assert_eq!(v["including"]["overall"]["diff_points"], "-8.0");
    assert_eq!(v["including"]["meets"], false);
    assert_eq!(v["rows"].as_array().map(Vec::len), Some(25));
    assert_eq!(v["including"]["by_page"].as_array().map(Vec::len), Some(13));
}

/// write_report はリポ内・相対パスを拒否し、temp には LF のみの 2 ファイルを書く。
#[test]
fn aisnap9_write_report_paths() {
    let red = results_with(|_| None);
    let r = compare_results(&TASKS, &red, &red).expect("compare");
    let (md, json) = (render_report_markdown(&r, &meta()), render_report_json(&r));
    assert!(
        write_report(
            &md,
            &json,
            &manifest().join("target").join("aisnap9-report-no")
        )
        .is_err()
    );
    assert!(write_report(&md, &json, Path::new("relative/dir")).is_err());
    let dir = temp_dir("rep-out");
    let paths = write_report(&md, &json, &dir).expect("write");
    assert_eq!(paths.len(), 2);
    for p in &paths {
        let body = std::fs::read_to_string(p).expect("read");
        assert!(!body.contains('\r') && body.ends_with('\n'));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 実験用: `AGENT_EVAL_COMPARE_ANSWERS=/abs/dir` の回答からレポートを作り stdout へ出す。不完全なら失敗。
/// `AGENT_EVAL_COMPARE_REPORT_OUT=/abs/dir` があればリポ外へ書く。メタ情報は
/// `AGENT_EVAL_REPORT_COMMIT`・`_DATE`・`_MODEL`・`_ISSUES`。
#[test]
fn aisnap9_report_if_requested() {
    let Some(dir) = env_dir("AGENT_EVAL_COMPARE_ANSWERS") else {
        return;
    };
    let c = collect(&dir);
    assert!(c.status.is_complete(), "answers are incomplete");
    let r = build_report(&fixtures(), &c).expect("report");
    let var = |k: &str| std::env::var(k).ok();
    let m = ReportMeta::from_parts(
        var("AGENT_EVAL_REPORT_COMMIT").as_deref(),
        var("AGENT_EVAL_REPORT_DATE").as_deref(),
        var("AGENT_EVAL_REPORT_MODEL").as_deref(),
        var("AGENT_EVAL_REPORT_ISSUES").as_deref(),
    )
    .expect("meta");
    let md = render_report_markdown(&r, &m);
    println!("{md}");
    if let Some(out) = env_dir("AGENT_EVAL_COMPARE_REPORT_OUT") {
        write_report(&md, &render_report_json(&r), &out).expect("write report");
    }
}
