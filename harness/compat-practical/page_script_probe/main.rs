//! compat_page_script_probe: 本番と同じ経路でページ内スクリプトの実行結果を
//! script 単位に観測する診断プローブ（TASK-71・Issue #964・`MEAS-4`・`JS-4`・`JS-6`）。
//!
//! b5（Trello）が JS 有効でも `no_match` になる原因を切り分けるために使う。本番の
//! `Page.navigate` 後のスクリプト実行（core の `run_page_scripts`）は、打ち切り・busy
//! 以外の結果を stderr に出さないため、「実行して成功した」のか「実行対象が無かった」のかを
//! 外から区別できない。このプローブは同じ関数を同じ既定の上限・同じ `src` 取得設定
//! （`page_script_fetch_options()`）で呼び、script ごとの outcome と、初期 HTML・実行後 HTML
//! の `h1` 件数を JSON Lines で出す。
//!
//! 呼び出し元: 人間が手動実行する（実サイトへ出るため CI・`cargo test` では実行しない）。
//! 手順は `harness/compat-practical/README.md` の「page_script_probe」節。
//!
//! 出力に載せるのは件数・分類名・種別だけで、HTML・script 本文・URL・例外メッセージの
//! 本文は載せない（`--debug-messages` 指定時の stderr だけ切り詰めて出す）。
//!
//! 終了コード: 0 = 記録完了（失敗はデータでありゲートではない）/ 1 = 書き込み失敗 /
//! 2 = 入力・使用エラー（引数不正・エンジン未同梱・ランタイム構築失敗）。
//!
//! 制約（REPAIR-3）: UA は `Fetcher` 既定のまま（偽装しない。SEC-2）。実行上限は本番の既定で、
//! 引数から変えられない。`<base>` 要素は `src` の同一オリジン判定に反映しない（簡易実装）。

#[path = "../js_api_spike/common.rs"]
mod common;

mod probe;

use std::io::Write;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use common::{Obj, map_fetch_error, same_origin, sanitize_debug};
use fandhe_browser_core::config::Config;
use fandhe_browser_core::{
    FetchOptions, Fetcher, PageRunInput, PageRunOptions, ParseOptions, ScriptCollectionOptions,
    ScriptKind, ScriptOutcome, ScriptSource, collect_page_scripts, page_script_fetch_options,
    parse_document, query_selector_all_str, run_page_scripts,
};
use probe::{Origin, ProbeArgs, class_label, lifecycle_label, script_line};
use reqwest::Url;

fn main() -> ExitCode {
    // V8 は子プロセスで評価されるため、ワーカー起動要求なら最初に処理する（JS-1 の契約）。
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    let args = match probe::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("usage error: {}", e.0);
            return ExitCode::from(2);
        }
    };
    run(&args)
}

/// セレクタに一致する要素の件数（grep ではなく本実装のパーサーとセレクタで数える。既定は `h1`）。
/// スクリプト実行用の解析と条件を揃えるため scripting 有効で解析する
/// （無効だと `<noscript>` 内の `h1` が要素として数えられ、実行後の件数とずれる）。
fn count_matches(html: &str, selector: &str) -> Option<u64> {
    let parsed =
        parse_document(html, &ParseOptions::default().with_scripting_enabled(true)).ok()?;
    let doc = parsed.document;
    let found = query_selector_all_str(&doc, doc.root(), selector).ok()?;
    Some(found.len() as u64)
}

/// 本番（core の `run_options_for`）と同じく、module 有効時は収集オプションの gate を真にする。
/// 収集と実行の両方に同じオプションを渡す（片方だけだと module は `skipped_module` のままになる）。
fn run_options(args: &ProbeArgs) -> PageRunOptions {
    if args.modules {
        PageRunOptions::default()
            .with_collection(ScriptCollectionOptions::default().with_execute_modules(true))
    } else {
        PageRunOptions::default()
    }
}

fn bool_s(b: bool) -> String {
    b.to_string()
}

fn run(args: &ProbeArgs) -> ExitCode {
    // セレクタは実行前に検証する（不正なら使用エラー）。
    if let Ok(p) = parse_document("<p></p>", &ParseOptions::default())
        && query_selector_all_str(&p.document, p.document.root(), &args.selector).is_err()
    {
        eprintln!("usage error: invalid --selector");
        return ExitCode::from(2);
    }
    // 設定は固定文面だけで作る（引数文字列は連結しない）。module は本番と同じく gate 設定も真にする。
    let toml = if args.modules {
        format!(
            "{}execute_page_scripts = true\nexecute_modules = true\n",
            args.engine.toml()
        )
    } else {
        args.engine.toml().to_string()
    };
    let cfg = match Config::from_toml_str(&toml) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if cfg.js().engine().is_none() {
        eprintln!(
            "error: no JS engine is compiled in; rebuild with --features js-v8 or --features js-boa"
        );
        return ExitCode::from(2);
    }
    // ページ取得は `Page.navigate` と同じ既定、`src` とページ内 fetch は本番の専用設定。
    let (page_fetcher, script_fetcher) = match (
        Fetcher::new(FetchOptions::default()),
        Fetcher::new(page_script_fetch_options()),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            eprintln!("error: cannot build fetcher");
            return ExitCode::from(2);
        }
    };
    // `run_page_scripts` の future は Send ではないため current_thread で block_on する。
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot build async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let lines = rt.block_on(measure(args, &cfg, &page_fetcher, &script_fetcher));
    match write_output(args, &lines) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: cannot write output: {e}");
            ExitCode::from(1)
        }
    }
}

/// 出力先へ書く。`--out` は既存ファイル・シンボリックリンクを切り詰めないよう排他作成する。
fn write_output(args: &ProbeArgs, lines: &[String]) -> std::io::Result<()> {
    let mut text = lines.join("\n");
    text.push('\n');
    match &args.out {
        None => std::io::stdout().write_all(text.as_bytes()),
        Some(path) => {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        }
    }
}

async fn measure(
    args: &ProbeArgs,
    cfg: &Config,
    page_fetcher: &Fetcher,
    script_fetcher: &Fetcher,
) -> Vec<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let engine = format!("{:?}", args.engine).to_ascii_lowercase();
    let mut lines = vec![
        Obj::new()
            .s("type", "meta")
            .n("schema_version", 1)
            .n("measured_at_unix", now)
            .s("id", &args.id)
            .s("engine", &engine)
            .s("os", std::env::consts::OS)
            .s("limits", "run_page_scripts_defaults")
            .raw("modules", bool_s(args.modules))
            .s(
                "selector",
                if args.selector == "h1" {
                    "h1"
                } else {
                    "custom"
                },
            )
            .line(),
    ];

    let resp = match page_fetcher.get(args.url.as_str()).await {
        Ok(r) => r,
        Err(e) => {
            lines.push(
                Obj::new()
                    .s("type", "page")
                    .s("id", &args.id)
                    .s("fetch_error", map_fetch_error(&e).as_str())
                    .line(),
            );
            return lines;
        }
    };
    let html = resp.body_text_lossy();
    let final_url_str = resp.final_url().to_string();
    let final_url = Url::parse(&final_url_str).unwrap_or_else(|_| args.url.clone());

    let Ok(parsed) = parse_document(&html, &ParseOptions::default().with_scripting_enabled(true))
    else {
        lines.push(
            Obj::new()
                .s("type", "page")
                .s("id", &args.id)
                .s("page_error", "parse_error")
                .line(),
        );
        return lines;
    };
    let run_opts = run_options(args);
    let collected = collect_page_scripts(&parsed.document, run_opts.collection());
    let count_diag = |k: &str| {
        collected
            .diagnostics()
            .iter()
            .filter(|d| d.kind().as_str() == k)
            .count() as u64
    };
    let entries = collected.entries();
    let inline = entries
        .iter()
        .filter(|e| matches!(e.source(), ScriptSource::Inline(_)))
        .count() as u64;
    let scripts = Obj::new()
        .n("seen", collected.seen_scripts() as u64)
        .n("executable", entries.len() as u64)
        .n("inline", inline)
        .n("src", entries.len() as u64 - inline)
        .n(
            "async",
            entries.iter().filter(|e| e.is_async()).count() as u64,
        )
        .n(
            "defer",
            entries.iter().filter(|e| e.is_defer()).count() as u64,
        )
        .n(
            "module",
            entries
                .iter()
                .filter(|e| e.kind() == ScriptKind::Module)
                .count() as u64,
        )
        .n("skipped_module", count_diag("skipped_module"))
        .n(
            "skipped_unsupported_type",
            count_diag("skipped_unsupported_type"),
        )
        .n("empty_src", count_diag("empty_src"))
        .line();
    lines.push(
        Obj::new()
            .s("type", "page")
            .s("id", &args.id)
            .n("status", u64::from(resp.status()))
            .n("body_bytes", resp.body().len() as u64)
            .raw("redirected", bool_s(final_url != args.url))
            .raw(
                "final_same_origin",
                bool_s(same_origin(&final_url, &args.url)),
            )
            .opt_n("h1_initial", count_matches(&html, "h1"))
            .opt_n("selector_initial", count_matches(&html, &args.selector))
            .raw("scripts", scripts)
            .line(),
    );

    let input = PageRunInput::new(&html, &final_url_str);
    let out = match run_page_scripts(&input, cfg.js(), &run_opts, script_fetcher).await {
        Ok(o) => o,
        Err(e) => {
            // 成功を装わない。エラーの種別名だけを残す（本文は URL を含み得るため出さない）。
            lines.push(
                Obj::new()
                    .s("type", "run")
                    .s("id", &args.id)
                    .s("run_error", &class_label(&e.to_string()))
                    .line(),
            );
            return lines;
        }
    };

    for rec in out.scripts() {
        let origin = match rec
            .index()
            .and_then(|i| entries.iter().find(|e| e.index() == i))
            .map(|e| e.source())
        {
            Some(ScriptSource::Inline(_)) => Origin::Inline,
            Some(ScriptSource::External(src)) => Origin::Src {
                same_origin: final_url
                    .join(src)
                    .map(|u| same_origin(&u, &final_url))
                    .unwrap_or(false),
            },
            _ => Origin::Unknown,
        };
        if args.debug_messages
            && let ScriptOutcome::Exception { message, .. } = rec.outcome()
        {
            eprintln!(
                "[{}#{:?}] {}",
                args.id,
                rec.index(),
                sanitize_debug(message)
            );
        }
        lines.push(script_line(&args.id, rec.index(), origin, rec.outcome()));
    }

    let diag = out.bridge_diagnostics();
    let console_count = |lv: &str| {
        diag.console_messages
            .iter()
            .filter(|m| format!("{:?}", m.level).eq_ignore_ascii_case(lv))
            .count() as u64
    };
    let labels = |v: Vec<String>| {
        let mut v = v;
        v.sort();
        v
    };
    let diag_obj = Obj::new()
        .n(
            "console_log",
            console_count("log") + console_count("info") + console_count("debug"),
        )
        .n("console_warn", console_count("warn"))
        .n("console_error", console_count("error"))
        .strs(
            "listener_errors",
            &labels(
                diag.listener_errors
                    .iter()
                    .map(|e| class_label(&e.message))
                    .collect(),
            ),
        )
        .strs(
            "timer_errors",
            &labels(
                diag.timer_errors
                    .iter()
                    .map(|e| class_label(&e.message))
                    .collect(),
            ),
        )
        .strs(
            "net_callback_errors",
            &labels(
                diag.net_callback_errors
                    .iter()
                    .map(|e| class_label(&e.message))
                    .collect(),
            ),
        )
        .strs(
            "net_rejections",
            &labels(
                diag.net_rejections
                    .iter()
                    .map(|r| format!("{}:{}", r.request.as_str(), r.kind.as_str()))
                    .collect(),
            ),
        )
        .n(
            "location_changes_ignored",
            diag.location_changes_ignored.len() as u64,
        )
        .n(
            "timer_string_handlers_ignored",
            diag.timer_string_handlers_ignored,
        )
        .raw(
            "listener_limit_exceeded",
            bool_s(diag.listener_limit_exceeded),
        )
        .raw(
            "microtask_limit_exceeded",
            bool_s(diag.microtask_limit_exceeded),
        )
        .raw("net_limit_exceeded", bool_s(diag.net_limit_exceeded))
        .n("dropped", diag.dropped)
        .line();
    let dyn_imp = out.dynamic_imports();
    let mut fail_kinds: Vec<String> = dyn_imp
        .failures()
        .iter()
        .map(|k| k.as_str().to_string())
        .collect();
    fail_kinds.sort();
    // 動的 import は件数と reject の種別名だけを出す（URL・specifier は出さない）
    let dyn_obj = Obj::new()
        .n("requested", dyn_imp.requested() as u64)
        .n("fulfilled", dyn_imp.fulfilled() as u64)
        .n("rejected", dyn_imp.rejected() as u64)
        .n("pending", dyn_imp.pending() as u64)
        .strs("failures", &fail_kinds)
        .line();
    let lifecycle: Vec<String> = out
        .lifecycle()
        .iter()
        .map(|l| lifecycle_label(l.event(), l.outcome()))
        .collect();
    let net = out.net();
    let net_obj = Obj::new()
        .n("started", net.started() as u64)
        .n("completed", net.completed() as u64)
        .n("cancelled", net.cancelled() as u64)
        .n("response_bytes", net.response_bytes())
        .n("pending_at_end", net.pending_at_end() as u64)
        .line();
    let ev = out.event_loop();
    let ev_obj = Obj::new()
        .n("fired", ev.fired() as u64)
        .n("pending_at_end", ev.pending_at_end() as u64)
        .line();
    lines.push(
        Obj::new()
            .s("type", "run")
            .s("id", &args.id)
            .opt_s("abort", out.abort().map(|a| a.kind().as_str()))
            .opt_n(
                "abort_index",
                out.abort().and_then(|a| a.index()).map(|i| i as u64),
            )
            .strs("lifecycle", &lifecycle)
            .raw("event_loop", ev_obj)
            .raw("net", net_obj)
            .raw("bridge", diag_obj)
            .raw("dynamic_imports", dyn_obj)
            .opt_n("h1_after", count_matches(out.html(), "h1"))
            .opt_n("selector_after", count_matches(out.html(), &args.selector))
            .n("html_bytes_after", out.html().len() as u64)
            .n(
                "elapsed_ms",
                u64::try_from(out.elapsed().as_millis()).unwrap_or(u64::MAX),
            )
            .line(),
    );
    lines
}
