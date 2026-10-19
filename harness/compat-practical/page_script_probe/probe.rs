//! page_script_probe の純ロジック（引数検証・出力 JSON の組み立て）と自己テスト
//! （TASK-71・Issue #964・`MEAS-4`・`JS-4`）。
//!
//! 呼び出し元は同ディレクトリの `main.rs` だけ。ネットワークにも JS エンジンにも触れない
//! ため、テストはオフラインで決定的に動く。出力は件数・分類名・種別だけで、ページ由来の
//! 本文・URL・生の例外メッセージは含めない（security.md「秘密情報の混入防止」）。
//! 例外の分類・JSON 組み立ては `js_api_spike/common.rs` の純関数を再利用する。

use std::path::PathBuf;

use fandhe_browser_core::{
    ExceptionKind, ExceptionSummary, LifecycleEvent, LifecycleOutcome, ScriptDiagnosticKind,
    ScriptOutcome, SrcFetchErrorKind,
};
use reqwest::Url;

use crate::common::{EngineChoice, Obj, UsageError, classify, is_valid_id, parse_https_url};

/// 引数（対象は 1 件。実行上限は本番の既定のままで、引数から変えられない）。
#[derive(Debug)]
pub struct ProbeArgs {
    pub id: String,
    pub url: Url,
    pub engine: EngineChoice,
    pub out: Option<PathBuf>,
    pub debug_messages: bool,
    /// ES module 実行（`JS-2`・設定 gate `[js] execute_modules`）を有効にして収集・実行する。
    pub modules: bool,
    /// 件数を数える CSS セレクタ（既定 `h1`。d2・d3 の `form` 判定用。#1119）。
    pub selector: String,
}

fn usage<T>(msg: &str) -> Result<T, UsageError> {
    Err(UsageError(msg.to_string()))
}

/// `--target <id>=<https url>`・`--engine v8|boa`（既定 v8）・`--out <path>`・`--debug-messages`・`--modules`・`--selector <css>`（既定 `h1`）。
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<ProbeArgs, UsageError> {
    let mut it = args.into_iter();
    let mut target: Option<(String, Url)> = None;
    let mut engine = EngineChoice::V8;
    let mut out = None;
    let mut debug_messages = false;
    let mut modules = false;
    let mut selector = "h1".to_string();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => {
                let Some(v) = it.next() else {
                    return usage("--target requires <id>=<url>");
                };
                let Some((id, url)) = v.split_once('=') else {
                    return usage("--target must look like <id>=<url>");
                };
                if !is_valid_id(id) {
                    return usage("target id must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$");
                }
                if target.is_some() {
                    return usage("only one --target is allowed");
                }
                target = Some((id.to_string(), parse_https_url(url)?));
            }
            "--engine" => match it.next().as_deref() {
                Some("v8") => engine = EngineChoice::V8,
                Some("boa") => engine = EngineChoice::Boa,
                _ => return usage("--engine must be v8 or boa"),
            },
            "--out" => match it.next() {
                Some(p) if !p.is_empty() => out = Some(PathBuf::from(p)),
                _ => return usage("--out requires a path"),
            },
            "--debug-messages" => debug_messages = true,
            "--modules" => modules = true,
            "--selector" => match it.next() {
                // 長さを上限検証する（セレクタは core のパーサーへ渡すだけで、連結・評価はしない）
                Some(v) if !v.is_empty() && v.len() <= 200 => selector = v,
                _ => return usage("--selector requires a CSS selector (1..200 bytes)"),
            },
            _ => return usage("unknown argument"),
        }
    }
    let Some((id, url)) = target else {
        return usage("--target <id>=<url> is required");
    };
    Ok(ProbeArgs {
        id,
        url,
        engine,
        out,
        debug_messages,
        modules,
        selector,
    })
}

/// 例外メッセージを「分類名[:API 名]」へ要約する（本文は残さない）。
pub fn class_label(msg: &str) -> String {
    let c = classify(msg);
    match c.missing_api {
        Some(api) => format!("{}:{api}", c.message_class),
        None => c.message_class.to_string(),
    }
}

/// script 1 件の出どころ。`same_origin` は src のときだけ意味を持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Inline,
    Src {
        same_origin: bool,
    },
    /// 収集時に実行対象から外れた（出どころを持たない）。
    Unknown,
}

/// script 1 件の JSON 行。URL・本文・生メッセージは含めない。
pub fn script_line(
    id: &str,
    index: Option<usize>,
    origin: Origin,
    outcome: &ScriptOutcome,
    exception: Option<&ExceptionSummary>,
) -> String {
    let mut o = Obj::new()
        .s("type", "script")
        .s("id", id)
        .opt_n("index", index.map(|i| i as u64));
    o = match origin {
        Origin::Inline => o.s("source", "inline"),
        Origin::Src { same_origin } => o
            .s("source", "src")
            .raw("same_origin", same_origin.to_string()),
        Origin::Unknown => o.s("source", "unknown"),
    };
    o = o.s("outcome", outcome.as_str());
    match outcome {
        // module 評価失敗は core が生メッセージを捨てて要約だけを渡す（Issue #1133）。
        // 要約が無い（classic）ときは従来どおり固定文でなく生メッセージを `classify` する。
        ScriptOutcome::Exception { truncated, .. } if exception.is_some() => {
            if let Some(e) = exception {
                let kind = match e.kind() {
                    ExceptionKind::Other => None,
                    k => Some(k.as_str()),
                };
                o = o
                    .opt_s("error_kind", kind)
                    .s("message_class", e.message_class().as_str())
                    .opt_s("missing_api", e.identifier())
                    .opt_s("property", e.property())
                    .opt_s("error_code", e.code())
                    .s("exception_source", "module_evaluation")
                    .raw("truncated", truncated.to_string());
            }
        }
        ScriptOutcome::Exception { message, truncated } => {
            let c = classify(message);
            o = o
                .opt_s("error_kind", c.error_kind.as_deref())
                .s("message_class", c.message_class)
                .opt_s("missing_api", c.missing_api.as_deref())
                .opt_s("property", c.property.as_deref())
                .opt_s("error_code", c.code.as_deref())
                .raw("truncated", truncated.to_string());
        }
        ScriptOutcome::Skipped { kind, count } => {
            o = o
                .s("skip_kind", skip_name(kind))
                .opt_n("count", count.map(|c| c as u64));
        }
        ScriptOutcome::SrcFetchFailed { kind } => {
            o = o.s("fetch_error", kind.as_str());
            if let SrcFetchErrorKind::HttpStatus { status } = kind {
                o = o.n("http_status", u64::from(*status));
            }
        }
        ScriptOutcome::Aborted { kind, message } => {
            o = o.s("abort", kind.as_str());
            if let Some(m) = message {
                o = o.s("message_class", &class_label(m));
            }
        }
        ScriptOutcome::NotExecuted { kind } => {
            o = o.s("abort", kind.as_str());
        }
        _ => {}
    }
    o.line()
}

/// 動的 import の評価失敗の要約を `<kind>:<class>[:<identifier>]` のラベルにする。
pub fn exception_label(e: &ExceptionSummary) -> String {
    match e.identifier() {
        Some(id) => format!("{}:{}:{id}", e.kind().as_str(), e.message_class().as_str()),
        None => format!("{}:{}", e.kind().as_str(), e.message_class().as_str()),
    }
}

fn skip_name(k: &ScriptDiagnosticKind) -> &'static str {
    k.as_str()
}

/// ライフサイクル 1 件の表記（`dom_content_loaded:dispatched` 形式）。
pub fn lifecycle_label(event: LifecycleEvent, outcome: &LifecycleOutcome) -> String {
    let e = match event {
        LifecycleEvent::DomContentLoaded => "dom_content_loaded",
        LifecycleEvent::Load => "load",
        _ => "other",
    };
    format!("{e}:{}", outcome.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<ProbeArgs, UsageError> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    /// `MEAS-4`・#964: https の対象だけを受け付け、エンジンは固定語彙で既定は v8。
    #[test]
    fn accepts_https_target_and_defaults_to_v8() {
        let a = args(&["--target", "b5=https://example.com/"]).expect("ok");
        assert_eq!(a.id, "b5");
        assert_eq!(a.url.as_str(), "https://example.com/");
        assert_eq!(a.engine, EngineChoice::V8);
        assert!(!a.debug_messages);
        assert!(!a.modules);
        assert_eq!(a.selector, "h1");
        let b = args(&["--target", "b5=https://example.com/", "--engine", "boa"]).expect("ok");
        assert_eq!(b.engine, EngineChoice::Boa);
    }

    /// #1119: `--modules` と `--selector` を受け付ける。
    #[test]
    fn accepts_modules_and_selector() {
        let a = args(&[
            "--target",
            "d2=https://example.com/",
            "--modules",
            "--selector",
            "form",
        ])
        .expect("ok");
        assert!(a.modules);
        assert_eq!(a.selector, "form");
        assert!(args(&["--target", "d2=https://example.com/", "--selector"]).is_err());
        assert!(args(&["--target", "d2=https://example.com/", "--selector", ""]).is_err());
    }

    /// SSRF 対策（security.md）: http・file・不正 id・重複・未知引数は使用エラー。
    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            vec!["--target", "b5=http://example.com/"],
            vec!["--target", "b5=file:///etc/passwd"],
            vec!["--target", "b/5=https://example.com/"],
            vec![
                "--target",
                "b5=https://example.com/",
                "--target",
                "b6=https://example.org/",
            ],
            vec!["--target", "b5=https://example.com/", "--engine", "x"],
            vec!["--target", "b5=https://example.com/", "--user-agent", "x"],
            vec![],
        ] {
            assert!(args(&bad).is_err(), "{bad:?}");
        }
    }

    /// `MEAS-4`・`JS-4`: 例外は分類名と API 名だけを出し、メッセージ本文は出力に混入しない。
    #[test]
    fn exception_line_has_class_but_no_raw_message() {
        let leaky = ScriptOutcome::Exception {
            message: "Uncaught Error: SECRET_TOKEN_VALUE".to_string(),
            truncated: false,
        };
        let line = script_line(
            "b5",
            Some(4),
            Origin::Src { same_origin: false },
            &leaky,
            None,
        );
        assert!(!line.contains("SECRET_TOKEN_VALUE"), "{line}");
        assert!(line.contains(r#""outcome":"exception""#), "{line}");
        assert!(line.contains(r#""source":"src""#), "{line}");
        assert!(line.contains(r#""same_origin":false"#), "{line}");
        assert!(line.contains(r#""index":4"#), "{line}");
        assert!(line.contains(r#""message_class":"other""#), "{line}");

        let known = ScriptOutcome::Exception {
            message: "Uncaught ReferenceError: IntersectionObserver is not defined (line 3)"
                .to_string(),
            truncated: false,
        };
        let line = script_line("b5", Some(4), Origin::Inline, &known, None);
        assert!(line.contains(r#""error_kind":"ReferenceError""#), "{line}");
        assert!(line.contains(r#""message_class":"not_defined""#), "{line}");
        assert!(
            line.contains(r#""missing_api":"IntersectionObserver""#),
            "{line}"
        );
    }

    /// `JS-4`: スキップは理由の種別名だけ、取得失敗は HTTP ステータス値だけを出す。
    #[test]
    fn skipped_and_fetch_failed_lines_carry_kinds_only() {
        let s = ScriptOutcome::Skipped {
            kind: ScriptDiagnosticKind::SkippedModule,
            count: None,
        };
        let line = script_line("b5", Some(2), Origin::Unknown, &s, None);
        assert!(line.contains(r#""skip_kind":"skipped_module""#), "{line}");
        let f = ScriptOutcome::SrcFetchFailed {
            kind: SrcFetchErrorKind::HttpStatus { status: 403 },
        };
        let line = script_line("b5", Some(1), Origin::Src { same_origin: true }, &f, None);
        assert!(line.contains(r#""fetch_error":"http_status""#), "{line}");
        assert!(line.contains(r#""http_status":403"#), "{line}");
    }

    /// `JS-2`・#1119: module の保留・未対応と別オリジン拒否は種別名だけを出す。
    #[test]
    fn module_outcomes_carry_kinds_only() {
        let line = script_line(
            "b5",
            Some(0),
            Origin::Inline,
            &ScriptOutcome::ModulePending,
            None,
        );
        assert!(line.contains(r#""outcome":"module_pending""#), "{line}");
        let line = script_line(
            "b5",
            Some(1),
            Origin::Src { same_origin: false },
            &ScriptOutcome::ModuleUnsupported,
            None,
        );
        assert!(line.contains(r#""outcome":"module_unsupported""#), "{line}");
        let f = ScriptOutcome::SrcFetchFailed {
            kind: SrcFetchErrorKind::CrossOrigin,
        };
        let line = script_line("b5", Some(2), Origin::Src { same_origin: false }, &f, None);
        assert!(line.contains(r#""fetch_error":"cross_origin""#), "{line}");
    }

    /// `JS-5`・Issue #1133: module 評価失敗は core の要約から分類と識別子を出し、
    /// 固定文のメッセージ・specifier・クエリは出力に混入しない。
    #[test]
    fn module_exception_line_uses_summary_only() {
        let fixed = ScriptOutcome::Exception {
            message: "module evaluation failed".to_string(),
            truncated: false,
        };
        let sum = ExceptionSummary::from_engine_message(
            "Uncaught ReferenceError: ResizeObserver is not defined (line 9)",
        );
        let line = script_line("d2", Some(1), Origin::Inline, &fixed, Some(&sum));
        assert!(line.contains(r#""error_kind":"ReferenceError""#), "{line}");
        assert!(line.contains(r#""message_class":"not_defined""#), "{line}");
        assert!(line.contains(r#""missing_api":"ResizeObserver""#), "{line}");
        assert!(
            line.contains(r#""exception_source":"module_evaluation""#),
            "{line}"
        );
        assert_eq!(
            exception_label(&sum),
            "ReferenceError:not_defined:ResizeObserver"
        );

        let sum = ExceptionSummary::from_engine_message(
            "Uncaught SyntaxError: The requested module './d.js?token=SECRET' does not provide an export named 'nope'",
        );
        let line = script_line("d2", Some(2), Origin::Inline, &fixed, Some(&sum));
        assert!(
            line.contains(r#""message_class":"missing_export""#),
            "{line}"
        );
        assert!(line.contains(r#""missing_api":"nope""#), "{line}");
        assert!(!line.contains("SECRET") && !line.contains("d.js"), "{line}");

        let sum = ExceptionSummary::from_engine_message("Uncaught Error: https://h/p?token=SECRET");
        let line = script_line("d2", Some(3), Origin::Inline, &fixed, Some(&sum));
        assert!(line.contains(r#""error_kind":"Error""#), "{line}");
        assert!(line.contains(r#""message_class":"other""#), "{line}");
        assert!(
            !line.contains("SECRET") && !line.contains("https"),
            "{line}"
        );
    }

    /// `JS-5`・`REPAIR-4`・Issue #1140: `property` は module 要約・classic の両経路で出し、
    /// 検証外のキーは `null`、ラベルには入れず、URL・クエリは出さない。
    #[test]
    fn property_key_is_emitted_on_both_paths() {
        let fixed = ScriptOutcome::Exception {
            message: "module evaluation failed".to_string(),
            truncated: false,
        };
        let sum = ExceptionSummary::from_engine_message(
            "Uncaught TypeError: Cannot use 'in' operator to search for 'WebkitAnimation' in undefined (line 1)",
        );
        let line = script_line("d2", Some(1), Origin::Inline, &fixed, Some(&sum));
        assert!(
            line.contains(r#""message_class":"cannot_use_in_operator""#),
            "{line}"
        );
        assert!(line.contains(r#""property":"WebkitAnimation""#), "{line}");
        assert!(line.contains(r#""missing_api":null"#), "{line}");
        assert_eq!(exception_label(&sum), "TypeError:cannot_use_in_operator");

        let sum = ExceptionSummary::from_engine_message(
            "Uncaught TypeError: Cannot use 'in' operator to search for 'https://h/p?token=SECRET' in undefined",
        );
        let line = script_line("d2", Some(2), Origin::Inline, &fixed, Some(&sum));
        assert!(line.contains(r#""property":null"#), "{line}");
        assert!(
            !line.contains("SECRET") && !line.contains("https"),
            "{line}"
        );

        let classic = ScriptOutcome::Exception {
            message: "TypeError: Cannot set properties of undefined (setting 'x')".to_string(),
            truncated: false,
        };
        let line = script_line("d3", Some(0), Origin::Inline, &classic, None);
        assert!(
            line.contains(r#""message_class":"cannot_set_properties""#),
            "{line}"
        );
        assert!(line.contains(r#""property":"x""#), "{line}");
    }

    /// `JS-5`・`REPAIR-4`・Issue #1145: `error_code` は検証済みの数字だけを module 要約・classic の
    /// 両経路で出し、検証外は `null`。案内 URL・トークンは出さず、ラベルにも入れない。
    #[test]
    fn error_code_is_emitted_on_both_paths() {
        let fixed = ScriptOutcome::Exception {
            message: "module evaluation failed".to_string(),
            truncated: false,
        };
        let sum = ExceptionSummary::from_engine_message(
            "Uncaught Error: Minified React error #299; visit https://h.example/errors/299?token=SECRET (line 9)",
        );
        let line = script_line("d3", Some(0), Origin::Inline, &fixed, Some(&sum));
        assert!(line.contains(r#""error_kind":"Error""#), "{line}");
        assert!(
            line.contains(r#""message_class":"minified_react_error""#),
            "{line}"
        );
        assert!(line.contains(r#""error_code":"299""#), "{line}");
        assert!(
            !line.contains("SECRET") && !line.contains("https") && !line.contains("token"),
            "{line}"
        );
        assert_eq!(exception_label(&sum), "Error:minified_react_error");

        let bad = ExceptionSummary::from_engine_message(
            "Uncaught Error: Minified React error #https://h.example/?token=SECRET",
        );
        let line = script_line("d3", Some(0), Origin::Inline, &fixed, Some(&bad));
        assert!(line.contains(r#""error_code":null"#), "{line}");
        assert!(
            !line.contains("SECRET") && !line.contains("https"),
            "{line}"
        );

        let classic = ScriptOutcome::Exception {
            message: "Minified React error #31; visit https://h.example/x".to_string(),
            truncated: false,
        };
        let line = script_line("d3", Some(0), Origin::Inline, &classic, None);
        assert!(line.contains(r#""error_kind":"Error""#), "{line}");
        assert!(line.contains(r#""error_code":"31""#), "{line}");
        assert!(!line.contains("https"), "{line}");
    }

    #[test]
    fn class_label_joins_class_and_api() {
        assert_eq!(
            class_label("Uncaught TypeError: foo.bar is not a function"),
            "not_a_function:foo.bar"
        );
        assert_eq!(class_label("boom"), "other");
    }

    /// `CORE-1`: 取得は core の `Fetcher` 経由のみで、独自 HTTP クライアントを作らない。
    #[test]
    fn sources_do_not_build_their_own_http_client() {
        let needles = [
            ["reqwest", "::Client"].concat(),
            ["reqwest", "::get"].concat(),
            ["Client", "::new"].concat(),
            ["Client", "Builder"].concat(),
        ];
        for (name, src) in [
            ("probe.rs", include_str!("probe.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            for n in &needles {
                assert!(!src.contains(n.as_str()), "{name} contains {n}");
            }
        }
    }
}
