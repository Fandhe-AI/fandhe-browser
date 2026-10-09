//! V8 / boa 共通のページスクリプトランナーのテストケース（TASK-109・Issue #780・
//! ビヘイビア `JS-4` / `JS-6`）。
//!
//! `tests/page_runner_v8.rs` と `tests/page_runner_boa.rs` が `#[path]` で取り込む。
//! 期待値は具体値で書く。

use std::time::Duration;

use fandhe_browser_core::{
    AbortKind, Config, PageRunInput, PageRunOptions, PageRunOutput, ScriptOutcome, WallTimeScope,
    run_page_scripts,
};

const BASE: &str = "https://example.test/";

fn run_page(engine: &str, html: &str, options: &PageRunOptions) -> PageRunOutput {
    let cfg = Config::from_toml_str(&format!("[js]\nengine = \"{engine}\"\n")).expect("config");
    run_page_scripts(&PageRunInput::new(html, BASE), cfg.js(), options).expect("run")
}

fn append(n: &str) -> String {
    format!(
        "<script>console.log('{n}');var p=document.createElement('p');p.textContent='{n}';\
         document.body.appendChild(p)</script>"
    )
}

/// 直下の子プロセス数（Linux のみ。`/proc/*/stat` の ppid を走査する）。
#[cfg(target_os = "linux")]
fn child_process_count() -> usize {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok())
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter(|stat| {
            // 形式: pid (comm) state ppid ...。comm は括弧を含み得るので最後の ')' 以降を読む。
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
                == Some(me)
        })
        .count()
}

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。
pub fn run(engine: &str) {
    let opts = PageRunOptions::default();

    eprintln!("case: JS-4 document order + console order");
    let html = format!("<body>{}{}{}</body>", append("1"), append("2"), append("3"));
    let out = run_page(engine, &html, &opts);
    assert!(
        out.html().contains("<p>1</p><p>2</p><p>3</p>"),
        "{}",
        out.html()
    );
    let texts: Vec<&str> = out
        .bridge_diagnostics()
        .console_messages
        .iter()
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(texts, vec!["1", "2", "3"]);
    assert!(out.abort().is_none());

    eprintln!("case: JS-4 exception does not stop following scripts");
    let html = format!(
        "<body>{}<script>throw new Error('boom')</script>{}</body>",
        append("1"),
        append("3")
    );
    let out = run_page(engine, &html, &opts);
    assert!(out.html().contains("<p>1</p><p>3</p>"), "{}", out.html());
    match out.scripts()[1].outcome() {
        ScriptOutcome::Exception { message, truncated } => {
            assert!(message.contains("boom"), "{message}");
            assert!(!truncated);
        }
        other => panic!("unexpected: {other:?}"),
    }

    eprintln!("case: JS-4 huge exception message stays bounded");
    let out = run_page(
        engine,
        "<body><script>throw new Error('x'.repeat(10000))</script></body>",
        &opts,
    );
    match out.scripts()[0].outcome() {
        // エンジン側が先に 1024 文字へ切り詰めるため `truncated` は問わない（4 KiB の
        // 境界はランナーの単体テストで検証する）。
        ScriptOutcome::Exception { message, .. } => {
            assert!(message.len() <= 4096, "{}", message.len());
        }
        other => panic!("unexpected: {other:?}"),
    }

    eprintln!("case: JS-6 infinite loop is aborted by script wall time");
    let html = format!(
        "<body>{}<script>while(true){{}}</script>{}</body>",
        append("1"),
        append("3")
    );
    let o = PageRunOptions::default().with_script_wall_time(Duration::from_millis(200));
    let out = run_page(engine, &html, &o);
    assert!(out.html().contains("<p>1</p>"), "{}", out.html());
    assert!(!out.html().contains("<p>3</p>"), "{}", out.html());
    match out.scripts()[1].outcome() {
        ScriptOutcome::Aborted { kind, .. } => assert_eq!(
            kind,
            &AbortKind::WallTime {
                scope: WallTimeScope::Script,
                limit: Duration::from_millis(200)
            }
        ),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(out.scripts()[2].outcome().as_str(), "not_executed");

    eprintln!("case: JS-6 page wall time");
    let o = PageRunOptions::default().with_page_wall_time(Duration::from_millis(300));
    let out = run_page(engine, "<body><script>while(true){}</script></body>", &o);
    let abort = out.abort().expect("abort");
    assert_eq!(
        abort.kind(),
        &AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit: Duration::from_millis(300)
        }
    );

    eprintln!("case: JS-6 globals do not leak between pages (also after abort)");
    let leak = "<body><script>var leaked = 1; globalThis.leaked2 = 2;</script></body>";
    let probe = "<body><script>document.body.setAttribute('data-x', typeof leaked + ',' + typeof leaked2)</script></body>";
    run_page(engine, leak, &opts);
    let out = run_page(engine, probe, &opts);
    assert!(
        out.html().contains("data-x=\"undefined,undefined\""),
        "{}",
        out.html()
    );
    let o = PageRunOptions::default().with_script_wall_time(Duration::from_millis(200));
    run_page(
        engine,
        "<body><script>var leaked = 1; globalThis.leaked2 = 2; while(true){}</script></body>",
        &o,
    );
    let out = run_page(engine, probe, &opts);
    assert!(
        out.html().contains("data-x=\"undefined,undefined\""),
        "{}",
        out.html()
    );

    eprintln!("case: JS-5 currentScript during execution");
    let body = "document.body.setAttribute('data-cs', document.currentScript.textContent)";
    let out = run_page(
        engine,
        &format!("<body><script>{body}</script></body>"),
        &opts,
    );
    assert!(
        out.html().contains(&format!("data-cs=\"{body}\"")),
        "{}",
        out.html()
    );

    #[cfg(target_os = "linux")]
    {
        eprintln!("case: JS-6 no child process remains");
        assert_eq!(child_process_count(), 0);
    }
}
