//! `score.rs` のテスト（TASK-21.3・`AISNAP-8`・Issue #121）。
//!
//! 採点器の検証は合成回答だけで行う。ここで組む「22/25」等は採点器の検算であり、
//! 実エージェントの測定結果ではない（実測は親 #118。`REPAIR-3`）。
//! 実回答を採点する入口は `aisnap8_score_real_answers_if_requested`。

// 未使用の公開項目は本 target では使わない。
#[allow(dead_code)]
#[path = "generate_reduced.rs"]
mod generate_reduced;
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

use generate_reduced::{ResolvedTask, resolve_golden};
use score::{
    Answer, AnswerStep, FailureClass, ScoreError, ScoreReport, Tally, parse_answers,
    render_results_json, score,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tasks::{Action, Category, GOLDEN, Golden, TASKS};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    manifest().join("benches").join("fixtures")
}

fn resolved() -> Vec<ResolvedTask> {
    resolve_golden(&fixtures()).expect("resolve")
}

/// 各ロケータの先頭 ref・golden の値で正答を組む。
fn perfect_answers(resolved: &[ResolvedTask]) -> BTreeMap<String, Answer> {
    let mut m = BTreeMap::new();
    for ((id, g), r) in GOLDEN.iter().zip(resolved) {
        let first = |i: usize| -> String {
            r.locators
                .get(i)
                .and_then(|l| l.refs.first())
                .cloned()
                .unwrap_or_default()
        };
        let a = match g {
            Golden::Ref { .. } => {
                let rf = r
                    .locators
                    .iter()
                    .find_map(|l| l.refs.first().cloned())
                    .unwrap_or_default();
                Answer::Ref(rf)
            }
            Golden::Value { value, .. } => Answer::Value((*value).to_owned()),
            Golden::Steps(steps) => Answer::Steps(
                steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| AnswerStep {
                        action: s.action,
                        target_ref: first(i),
                        value: s.value.map(str::to_owned),
                    })
                    .collect(),
            ),
        };
        m.insert((*id).to_owned(), a);
    }
    m
}

fn run(answers: &BTreeMap<String, Answer>) -> ScoreReport {
    score(&TASKS, &GOLDEN, &resolved(), answers).expect("score")
}

fn run_with(resolved: &[ResolvedTask], answers: &BTreeMap<String, Answer>) -> ScoreReport {
    score(&TASKS, &GOLDEN, resolved, answers).expect("score")
}

fn failure_of(rep: &ScoreReport, id: &str) -> Option<FailureClass> {
    rep.results
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("{id} missing"))
        .failure
}

fn tally(rep: &ScoreReport, c: Category) -> (u32, u32) {
    let t = rep
        .by_category
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, t)| *t)
        .expect("category");
    (t.pass, t.total)
}

#[test]
fn aisnap8_perfect_answers_pass_all() {
    let rep = run(&perfect_answers(&resolved()));
    assert_eq!(tally(&rep, Category::Click), (7, 7));
    assert_eq!(tally(&rep, Category::Extract), (6, 6));
    assert_eq!(tally(&rep, Category::Form), (6, 6));
    assert_eq!(tally(&rep, Category::Nav), (6, 6));
    assert_eq!((rep.overall.pass, rep.overall.total), (25, 25));
    assert_eq!(rep.overall.rate_text(), "100.0%");
    assert!(rep.verdict.meets);
    assert_eq!(rep.verdict.threshold_percent, 70);
}

#[test]
fn aisnap8_no_answers_fail_all_as_unanswered() {
    let rep = run(&BTreeMap::new());
    assert_eq!((rep.overall.pass, rep.overall.total), (0, 25));
    assert_eq!(rep.overall.rate_text(), "0.0%");
    assert!(!rep.verdict.meets);
    assert!(
        rep.results
            .iter()
            .all(|r| r.failure == Some(FailureClass::Unanswered))
    );
}

/// `AISNAP-8` 記載の内訳（nav 1 件・extract 2 件が誤答）を再現する合成。採点器の検算で測定結果ではない。
#[test]
fn aisnap8_synthetic_breakdown_22_of_25() {
    let mut a = perfect_answers(&resolved());
    a.insert("nav-01".into(), Answer::Ref("e-wrong".into()));
    a.insert("extract-02".into(), Answer::Value("wrong".into()));
    a.insert("extract-03".into(), Answer::Value("wrong".into()));
    let rep = run(&a);
    assert_eq!(tally(&rep, Category::Click), (7, 7));
    assert_eq!(tally(&rep, Category::Form), (6, 6));
    assert_eq!(tally(&rep, Category::Nav), (5, 6));
    assert_eq!(tally(&rep, Category::Extract), (4, 6));
    let text = |c| {
        rep.by_category
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, t)| t.rate_text())
            .expect("cat")
    };
    assert_eq!(text(Category::Click), "100.0%");
    assert_eq!(text(Category::Form), "100.0%");
    assert_eq!(text(Category::Nav), "83.3%");
    assert_eq!(text(Category::Extract), "66.7%");
    assert_eq!((rep.overall.pass, rep.overall.total), (22, 25));
    assert_eq!(rep.overall.rate_text(), "88.0%");
    assert!(rep.verdict.meets);
    assert_eq!(failure_of(&rep, "nav-01"), Some(FailureClass::Mismatch));
}

fn with_failures(n: usize) -> ScoreReport {
    let mut a = perfect_answers(&resolved());
    for t in TASKS.iter().take(n) {
        a.remove(t.id);
    }
    run(&a)
}

#[test]
fn aisnap8_threshold_boundary_on_25_tasks() {
    let r17 = with_failures(8);
    assert_eq!(r17.overall.pass, 17);
    assert_eq!(r17.overall.rate_text(), "68.0%");
    assert!(!r17.verdict.meets);
    let r18 = with_failures(7);
    assert_eq!(r18.overall.pass, 18);
    assert_eq!(r18.overall.rate_text(), "72.0%");
    assert!(r18.verdict.meets);
}

#[test]
fn aisnap8_tally_threshold_edges() {
    assert!(Tally { pass: 7, total: 10 }.meets(70));
    assert!(
        !Tally {
            pass: 69,
            total: 100
        }
        .meets(70)
    );
    let zero = Tally { pass: 0, total: 0 };
    assert!(!zero.meets(70));
    assert_eq!(zero.rate_permille(), None);
    assert_eq!(zero.rate_text(), "n/a");
    // 丸め後は 70.0% でも丸め前が未達なら未達（699/1000 → 69.9%）。
    assert!(
        !Tally {
            pass: 699,
            total: 1000
        }
        .meets(70)
    );
}

#[test]
fn aisnap8_extract_matches_by_value_even_if_golden_ref_unresolved() {
    let r = resolved();
    let e1 = r.iter().find(|t| t.id == "extract-01").expect("e1");
    assert!(e1.locators.iter().all(|l| l.refs.is_empty()));
    let mut a = perfect_answers(&r);
    a.insert(
        "extract-01".into(),
        Answer::Value("  user3@example.com \n".into()),
    );
    assert_eq!(failure_of(&run(&a), "extract-01"), None);
    a.insert(
        "extract-01".into(),
        Answer::Value("user4@example.com".into()),
    );
    assert_eq!(
        failure_of(&run(&a), "extract-01"),
        Some(FailureClass::Mismatch)
    );
    a.insert(
        "extract-01".into(),
        Answer::Value("USER3@example.com".into()),
    );
    assert_eq!(
        failure_of(&run(&a), "extract-01"),
        Some(FailureClass::Mismatch)
    );
}

#[test]
fn aisnap8_extract_collapses_inner_whitespace() {
    let mut a = perfect_answers(&resolved());
    a.insert("extract-03".into(), Answer::Value("20   points".into()));
    assert_eq!(failure_of(&run(&a), "extract-03"), None);
}

#[test]
fn aisnap8_ref_accepts_any_of_locators() {
    let r = resolved();
    let nav = r.iter().find(|t| t.id == "nav-01").expect("nav-01");
    let mut accepted = 0;
    for l in &nav.locators {
        for rf in &l.refs {
            let mut a = perfect_answers(&r);
            a.insert("nav-01".into(), Answer::Ref(format!(" {rf} ")));
            assert_eq!(failure_of(&run(&a), "nav-01"), None, "{rf}");
            accepted += 1;
        }
    }
    assert!(accepted >= 2, "nav-01 must have several accepted refs");
}

fn form_steps(a: &BTreeMap<String, Answer>, id: &str) -> Vec<AnswerStep> {
    match a.get(id) {
        Some(Answer::Steps(s)) => s.clone(),
        other => panic!("{id}: {other:?}"),
    }
}

#[test]
fn aisnap8_form_mismatch_cases() {
    let r = resolved();
    let base = perfect_answers(&r);
    let steps = form_steps(&base, "form-01");
    assert!(steps.len() >= 3);

    let mut swapped = steps.clone();
    swapped.swap(0, 1);
    let mut short = steps.clone();
    short.pop();
    let mut wrong_value = steps.clone();
    wrong_value[0].value = Some("other".into());
    let mut wrong_ref = steps.clone();
    wrong_ref[0].target_ref = "e-wrong".into();
    let mut wrong_action = steps.clone();
    wrong_action[0].action = Action::Select;

    for (name, s) in [
        ("swapped", swapped),
        ("short", short),
        ("value", wrong_value),
        ("ref", wrong_ref),
        ("action", wrong_action),
    ] {
        let mut a = base.clone();
        a.insert("form-01".into(), Answer::Steps(s));
        assert_eq!(
            failure_of(&run(&a), "form-01"),
            Some(FailureClass::Mismatch),
            "{name}"
        );
    }
}

#[test]
fn aisnap8_form_04_double_click_is_mismatch() {
    let base = perfect_answers(&resolved());
    let steps = form_steps(&base, "form-04");
    assert_eq!(steps.len(), 1);
    let mut a = base.clone();
    a.insert(
        "form-04".into(),
        Answer::Steps(vec![steps[0].clone(), steps[0].clone()]),
    );
    assert_eq!(
        failure_of(&run(&a), "form-04"),
        Some(FailureClass::Mismatch)
    );
}

#[test]
fn aisnap8_shape_and_unanswered_classification() {
    let mut a = perfect_answers(&resolved());
    a.insert("click-01".into(), Answer::Value("x".into()));
    a.insert("extract-02".into(), Answer::Ref("x".into()));
    a.insert("form-02".into(), Answer::Ref("x".into()));
    a.insert("nav-03".into(), Answer::Unanswered);
    a.insert("nav-02".into(), Answer::Invalid);
    let rep = run(&a);
    assert_eq!(
        failure_of(&rep, "click-01"),
        Some(FailureClass::InvalidShape)
    );
    assert_eq!(
        failure_of(&rep, "extract-02"),
        Some(FailureClass::InvalidShape)
    );
    assert_eq!(
        failure_of(&rep, "form-02"),
        Some(FailureClass::InvalidShape)
    );
    assert_eq!(failure_of(&rep, "nav-03"), Some(FailureClass::Unanswered));
    assert_eq!(failure_of(&rep, "nav-02"), Some(FailureClass::InvalidShape));
}

#[test]
fn aisnap8_unresolved_golden_ref_fails_closed() {
    let mut r = resolved();
    for t in r
        .iter_mut()
        .filter(|t| t.id == "click-01" || t.id == "form-01")
    {
        for l in &mut t.locators {
            l.refs.clear();
        }
    }
    let mut a = perfect_answers(&resolved());
    a.insert("click-01".into(), Answer::Ref("e-any".into()));
    let rep = run_with(&r, &a);
    assert_eq!(
        failure_of(&rep, "click-01"),
        Some(FailureClass::GoldenUnresolved)
    );
    assert_eq!(
        failure_of(&rep, "form-01"),
        Some(FailureClass::GoldenUnresolved)
    );
}

#[test]
fn aisnap8_parse_answers_shapes() {
    let json = r#"[
      {"id": "click-01", "ref": "e1", "reasoning": "ignored"},
      {"id": "extract-02", "value": "x"},
      {"id": "form-01", "steps": [
        {"action": "fill", "ref": "e2", "value": "v"},
        {"action": "click", "ref": "e3", "value": null}]},
      {"id": "nav-03", "ref": null},
      {"id": "click-02", "ref": 5},
      {"id": "form-02", "steps": [{"action": "fill", "ref": "e2"}]},
      {"id": "form-03", "steps": [{"action": "click", "ref": "e2", "value": "v"}]},
      {"id": "nav-01", "ref": "e1", "value": "x"}
    ]"#;
    let m = parse_answers(&TASKS, json).expect("parse");
    assert_eq!(m.get("click-01"), Some(&Answer::Ref("e1".into())));
    assert_eq!(m.get("extract-02"), Some(&Answer::Value("x".into())));
    assert_eq!(
        m.get("form-01"),
        Some(&Answer::Steps(vec![
            AnswerStep {
                action: Action::Fill,
                target_ref: "e2".into(),
                value: Some("v".into())
            },
            AnswerStep {
                action: Action::Click,
                target_ref: "e3".into(),
                value: None
            },
        ]))
    );
    assert_eq!(m.get("nav-03"), Some(&Answer::Unanswered));
    assert_eq!(m.get("click-02"), Some(&Answer::Invalid));
    assert_eq!(m.get("form-02"), Some(&Answer::Invalid));
    assert_eq!(m.get("form-03"), Some(&Answer::Invalid));
    assert_eq!(m.get("nav-01"), Some(&Answer::Invalid));
    assert_eq!(m.len(), 8);
}

#[test]
fn aisnap8_parse_answers_errors() {
    assert!(matches!(
        parse_answers(&TASKS, "not json"),
        Err(ScoreError::Json(_))
    ));
    assert!(matches!(
        parse_answers(&TASKS, "{}"),
        Err(ScoreError::Json(_))
    ));
    assert!(matches!(
        parse_answers(&TASKS, "[1]"),
        Err(ScoreError::Json(_))
    ));
    assert_eq!(
        parse_answers(
            &TASKS,
            r#"[{"id":"click-01","ref":"a"},{"id":"click-01","ref":"b"}]"#
        ),
        Err(ScoreError::DuplicateId("click-01".into()))
    );
    assert_eq!(
        parse_answers(&TASKS, r#"[{"id":"nope","ref":"a"}]"#),
        Err(ScoreError::UnknownId("nope".into()))
    );
}

#[test]
fn aisnap8_parse_answers_limits() {
    let long = "a".repeat(score::MAX_STRING_BYTES + 1);
    let s = format!(r#"[{{"id":"click-01","ref":"{long}"}}]"#);
    assert!(matches!(
        parse_answers(&TASKS, &s),
        Err(ScoreError::TooLarge(_))
    ));

    let many = vec![r#"{"id":"click-01"}"#; score::MAX_ENTRIES + 1].join(",");
    assert!(matches!(
        parse_answers(&TASKS, &format!("[{many}]")),
        Err(ScoreError::TooLarge(_))
    ));

    let steps = vec![r#"{"action":"click","ref":"e1"}"#; score::MAX_STEPS + 1].join(",");
    let s = format!(r#"[{{"id":"form-01","steps":[{steps}]}}]"#);
    assert!(matches!(
        parse_answers(&TASKS, &s),
        Err(ScoreError::TooLarge(_))
    ));

    let pad = " ".repeat(score::MAX_INPUT_BYTES + 1);
    assert!(matches!(
        parse_answers(&TASKS, &format!("[]{pad}")),
        Err(ScoreError::TooLarge(_))
    ));
}

#[test]
fn aisnap8_score_rejects_unknown_answer_id() {
    let mut a = BTreeMap::new();
    a.insert("zzz".to_owned(), Answer::Unanswered);
    assert_eq!(
        score(&TASKS, &GOLDEN, &resolved(), &a),
        Err(ScoreError::UnknownId("zzz".into()))
    );
}

#[test]
fn aisnap8_results_json_is_fixed_format() {
    let rep = run(&BTreeMap::new());
    let out = render_results_json(&rep);
    assert!(out.starts_with(
        "{\n  \"threshold_percent\": 70,\n  \"meets\": false,\n  \"overall\": {\"pass\": 0, \"total\": 25, \"rate\": \"0.0%\"},\n  \"by_category\": [\n    {\"category\": \"click\", \"pass\": 0, \"total\": 7, \"rate\": \"0.0%\"},\n"
    ));
    assert!(out.contains(
        "    {\"id\": \"click-01\", \"category\": \"click\", \"pass\": false, \"failure\": \"unanswered\"},\n"
    ));
    assert!(out.ends_with("  ]\n}\n"));
    assert!(!out.contains('\r'));
    assert!(!out.ends_with("\n\n"));

    let ok = render_results_json(&run(&perfect_answers(&resolved())));
    assert!(ok.contains("\"meets\": true"));
    assert!(ok.contains(
        "    {\"id\": \"nav-06\", \"category\": \"nav\", \"pass\": true, \"failure\": null}\n"
    ));
}

/// 親 #118 の測定工程向けの入口。`AGENT_EVAL_ANSWERS=<回答ファイルの絶対パス>` が
/// 設定されたときだけ実回答を採点して結果を stdout へ出す（未設定時は何もしない）。
/// `AGENT_EVAL_WRITE` を併用したときのみ `harness/agent_eval/results.json` へ書く。
#[test]
fn aisnap8_score_real_answers_if_requested() {
    let Some(path) = std::env::var_os("AGENT_EVAL_ANSWERS") else {
        return;
    };
    let json = std::fs::read_to_string(PathBuf::from(path)).expect("read answers");
    let answers = parse_answers(&TASKS, &json).expect("parse answers");
    let rep = run(&answers);
    let out = render_results_json(&rep);
    println!("{out}");
    if std::env::var_os("AGENT_EVAL_WRITE").is_some() {
        let p = manifest()
            .join("harness")
            .join("agent_eval")
            .join("results.json");
        std::fs::write(p, out).expect("write results");
    }
}
