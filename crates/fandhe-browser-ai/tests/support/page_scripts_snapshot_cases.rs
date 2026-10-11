//! V8 / boa 共通の「JS 実行後 DOM の snapshot」テストケース（TASK-113・Issue #787・
//! `JS-4`/`AISNAP-6`/`AISNAP-10`/`PLUG-4`）。
//!
//! `tests/page_scripts_snapshot_v8.rs` と `tests/page_scripts_snapshot_boa.rs` が `#[path]` で
//! 取り込む。実エンジンは子プロセスで動くため、取り込み側が `main` の先頭で
//! `run_js_worker_if_requested` を呼ぶ独自 `main`（`harness = false`）であることが前提。
//!
//! 役割: cdp の `navigate_gated` と同じデータの流れ（実行 → 実行後 HTML を commit →
//! ai の `snapshot_body`）を、ai ⇔ cdp の依存禁止を守って core の公開 API だけで再現し、
//! (1) JS が描画した要素が snapshot に現れ ref が安定すること、(2) 診断・スクリプト本文が AI 向け
//! 出力へ混入しないこと、(3) 代表 5 サイトの削減率測定への影響を確認して数値を stdout へ出す。
//! 5 サイトの絶対値は `tests/mcp_token_reduction.rs` の `PINNED` が正本のため再ピン留めせず、
//! 同一プロセス内で測った既定オフの行との相対比較で検証する。期待値は具体値で書く。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fandhe_browser_ai::api::snapshot_body;
use fandhe_browser_core::{
    Config, NavigationResult, NavigationState, PageRunRequest, PageScriptExecutor,
    PageScriptHandle, PageScriptOutcome, ScriptDiagnosticKind, ScriptOutcome, SrcFetchErrorKind,
};
use serde_json::Value;

// テスト間共有モジュールのため、取り込み側のテストバイナリで使わない項目は未使用警告になる。それを許容する。
#[allow(dead_code)]
#[path = "page_scripts_fixtures.rs"]
mod fixtures;

use crate::reduction::{ReductionRow, reduction_pct, summarize};
use crate::tokens::{TokenCounter, fixtures_dir};

/// PLUG-4 の合格しきい値（%）。正本は `tests/mcp_token_reduction.rs` の `PLUG4_THRESHOLD_PCT`。
/// 有効時の平均との差を表示するためだけに持ち、合否ゲートには使わない（既存ハーネスを無変更に保つ）。
const PLUG4_THRESHOLD_PCT_FOR_DISPLAY: f64 = 84.0;

/// PLUG-4 の代表 5 サイト（`mcp_token_reduction.rs` の `SITES` と同じ順）。
const SITES: [&str; 5] = [
    "example-minimal",
    "wikipedia-article",
    "hn-list",
    "login-form",
    "mdn-docs",
];

/// 現スレッドの tokio ランタイムで future を駆動する実行器（cli の実行器と同じ形）。
struct TokioExecutor;

impl PageScriptExecutor for TokioExecutor {
    fn block_on<'a>(&self, future: Pin<Box<dyn Future<Output = ()> + 'a>>) -> std::io::Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(future);
        Ok(())
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn enabled_handle(engine: &str) -> PageScriptHandle {
    let cfg = Config::from_toml_str(&format!(
        "[js]\nengine = \"{engine}\"\nexecute_page_scripts = true\n"
    ))
    .expect("config");
    let handle = PageScriptHandle::new(cfg.js(), Arc::new(TokioExecutor));
    assert!(handle.is_enabled());
    handle
}

fn site_url(name: &str) -> String {
    format!("http://127.0.0.1:8899/{name}.html")
}

/// アクターの結果の要約（`PageScriptOutcome` は `#[non_exhaustive]` のため文字列で持つ）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Executed,
    Skipped(String),
}

/// `navigate_gated` と同じ流れを 1 回実行した結果。
struct Piped {
    stage: Stage,
    /// 確定した HTML（`Executed` なら実行後 HTML、それ以外は取得したまま）。
    html: String,
    /// `Executed` のときのスクリプトごとの結果。
    scripts: Vec<ScriptOutcome>,
    /// `snapshot_body` の応答本文。
    body: String,
}

/// 実行 → 確定 → `snapshot_body`。`nav` は再ナビゲーションの検証のため呼び出し側が持つ。
fn pipeline(handle: &PageScriptHandle, nav: &NavigationState, url: &str, html: &str) -> Piped {
    let generation = nav.begin_navigation().expect("generation");
    handle.begin_generation(nav.scope(), generation);
    let request = PageRunRequest::new(Arc::from(html), url, nav.scope(), generation);
    let result = rt()
        .block_on(handle.run_page_scripts(request))
        .expect("run_page_scripts");
    let (stage, final_html, scripts) = match result {
        PageScriptOutcome::Executed(out) => {
            let scripts: Vec<ScriptOutcome> =
                out.scripts().iter().map(|r| r.outcome().clone()).collect();
            (Stage::Executed, out.html().to_string(), scripts)
        }
        PageScriptOutcome::Skipped(reason) => (
            Stage::Skipped(format!("{reason:?}")),
            html.to_string(),
            Vec::new(),
        ),
        other => panic!("unexpected outcome: {other:?}"),
    };
    nav.commit_navigation(generation, NavigationResult::new(url, final_html.clone()))
        .expect("commit");
    let body = String::from_utf8(snapshot_body(nav).expect("snapshot_body")).expect("utf8");
    Piped {
        stage,
        html: final_html,
        scripts,
        body,
    }
}

/// JS なしの基準: HTML を直接 commit して `snapshot_body` を得る。
fn direct_body(url: &str, html: &str) -> String {
    let nav = NavigationState::new();
    let g = nav.begin_navigation().expect("generation");
    nav.commit_navigation(g, NavigationResult::new(url, html))
        .expect("commit");
    String::from_utf8(snapshot_body(&nav).expect("snapshot_body")).expect("utf8")
}

/// mcp の `interpret` と同じ変換（解析 → コンパクト再直列化）。
fn reserialize(body: &str) -> String {
    let value: Value = serde_json::from_str(body).expect("body is JSON");
    serde_json::to_string(&value).expect("serialize")
}

/// snapshot の 1 ノード（役割・名前・ref）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Flat {
    role: String,
    name: String,
    r#ref: Option<String>,
}

/// 応答本文のツリーを先行順に平坦化する。ページ由来の JSON は `get` 系だけで辿る。
fn flatten(body: &str) -> Vec<Flat> {
    fn walk(node: &Value, out: &mut Vec<Flat>) {
        let text = |k: &str| node.get(k).and_then(Value::as_str).map(str::to_owned);
        out.push(Flat {
            role: text("role").unwrap_or_default(),
            name: text("name").unwrap_or_default(),
            r#ref: text("ref"),
        });
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            for c in children {
                walk(c, out);
            }
        }
    }
    let value: Value = serde_json::from_str(body).expect("body is JSON");
    let mut out = Vec::new();
    if let Some(tree) = value.get("tree") {
        walk(tree, &mut out);
    }
    out
}

fn find<'a>(nodes: &'a [Flat], name: &str) -> &'a Flat {
    nodes
        .iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("node named {name:?} not found in {nodes:?}"))
}

/// ref の形式（`AISNAP-10`）: `e` + 小文字 16 進 8〜16 桁（+ 任意の `v<n>`・`-<n>`）。
fn assert_ref_shape(r: &str) {
    let rest = r
        .strip_prefix('e')
        .unwrap_or_else(|| panic!("ref {r:?} must start with 'e'"));
    let hex_len = rest.chars().take_while(char::is_ascii_hexdigit).count();
    assert!(
        (8..=16).contains(&hex_len),
        "ref {r:?}: hex digits {hex_len} not in 8..=16"
    );
    assert!(
        rest.chars().take(hex_len).all(|c| !c.is_ascii_uppercase()),
        "ref {r:?} must be lowercase hex"
    );
}

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。失敗は panic（非 0 終了）。
pub fn run(engine: &str) {
    let handle = enabled_handle(engine);
    let spa = spa_cases(&handle);
    stability_cases(&handle, &spa);
    diagnostics_cases(&spa);
    let report = measurement_cases(&handle, &spa);
    println!("{report}");
}

/// SPA 風ページを有効で 1 回通した結果と、JS なしの基準本文。
struct SpaRun {
    on: Piped,
    off_body: String,
}

/// `JS-4`・`AISNAP-6`: 描画の反映。
fn spa_cases(handle: &PageScriptHandle) -> SpaRun {
    eprintln!("case: JS-4/AISNAP-6 script-rendered elements reach the snapshot");
    let nav = NavigationState::new();
    let on = pipeline(handle, &nav, fixtures::SPA_URL, fixtures::SPA_HTML);
    assert_eq!(on.stage, Stage::Executed);
    let latest = nav.latest().expect("latest");
    assert!(
        latest.html().contains("Rendered dashboard"),
        "{}",
        latest.html()
    );
    let off_body = direct_body(fixtures::SPA_URL, fixtures::SPA_HTML);

    let nodes = flatten(&on.body);
    for name in [
        "Rendered dashboard",
        "Item 1",
        "Item 2",
        "Item 3",
        "Search query",
        "Run search",
        "After error",
        "Static action",
        "Home",
    ] {
        assert!(
            nodes.iter().any(|n| n.name == name) || on.body.contains(name),
            "{name:?} missing from the JS-enabled snapshot: {}",
            on.body
        );
    }
    for name in fixtures::RENDERED_NAMES {
        assert!(
            !off_body.contains(name),
            "{name:?} must not appear without JS: {off_body}"
        );
    }
    SpaRun { on, off_body }
}

/// `AISNAP-6`・`AISNAP-10`・`JS-4`: ref の安定性。
fn stability_cases(handle: &PageScriptHandle, spa: &SpaRun) {
    eprintln!("case: AISNAP-6/AISNAP-10 refs of static elements are stable across JS execution");
    let on_nodes = flatten(&spa.on.body);
    let off_nodes = flatten(&spa.off_body);
    for name in ["Static action", "Home"] {
        let on_ref = find(&on_nodes, name).r#ref.clone().expect("on ref");
        let off_ref = find(&off_nodes, name).r#ref.clone().expect("off ref");
        assert_ref_shape(&on_ref);
        assert_eq!(on_ref, off_ref, "ref of {name:?} changed by JS execution");
    }

    eprintln!("case: AISNAP-6 repeated reads are byte-identical and refs are unique");
    let nav = NavigationState::new();
    let first = pipeline(handle, &nav, fixtures::SPA_URL, fixtures::SPA_HTML);
    let again = String::from_utf8(snapshot_body(&nav).expect("snapshot_body")).expect("utf8");
    assert_eq!(first.body, again, "two reads of one shared state differ");
    let nodes = flatten(&first.body);
    let refs: Vec<&String> = nodes.iter().filter_map(|n| n.r#ref.as_ref()).collect();
    let unique: HashSet<&&String> = refs.iter().collect();
    assert_eq!(refs.len(), unique.len(), "duplicate refs: {refs:?}");
    for name in ["Rendered dashboard", "Run search", "After error"] {
        let r = find(&nodes, name).r#ref.clone();
        if let Some(r) = r {
            assert_ref_shape(&r);
        }
    }
    for name in ["Run search", "After error"] {
        assert!(
            find(&nodes, name).r#ref.is_some(),
            "script-rendered interactive element {name:?} must carry a ref"
        );
    }

    eprintln!("case: AISNAP-6/JS-4 re-navigation to the same page yields identical bytes");
    let second = pipeline(handle, &nav, fixtures::SPA_URL, fixtures::SPA_HTML);
    assert_eq!(second.stage, Stage::Executed);
    assert_eq!(
        first.body, second.body,
        "a new generation changed the snapshot of the same page"
    );
}

/// `JS-4`（診断の集約）・`PLUG-4`: 診断・スクリプト本文が AI 向け出力へ混入しない。
fn diagnostics_cases(spa: &SpaRun) {
    eprintln!("case: JS-4/PLUG-4 diagnostics happen but never reach AI-facing output");
    // 前提: 例外・module スキップ・後続スクリプトの成功がアクター側で実際に記録されている。
    let has_exception = spa.on.scripts.iter().any(|s| {
        matches!(s, ScriptOutcome::Exception { message, .. } if message.contains("DIAG_MARKER_42"))
    });
    assert!(
        has_exception,
        "exception not recorded: {:?}",
        spa.on.scripts
    );
    let has_module_skip = spa.on.scripts.iter().any(|s| {
        matches!(
            s,
            ScriptOutcome::Skipped {
                kind: ScriptDiagnosticKind::SkippedModule,
                ..
            }
        )
    });
    assert!(
        has_module_skip,
        "module skip not recorded: {:?}",
        spa.on.scripts
    );
    assert_eq!(
        spa.on.scripts.last(),
        Some(&ScriptOutcome::Succeeded),
        "the script after the exception must still run: {:?}",
        spa.on.scripts
    );

    // 本題: snapshot 本文と interpret 相当テキストに診断・本文が現れない。
    let text = reserialize(&spa.on.body);
    for marker in fixtures::FORBIDDEN_IN_AI_OUTPUT {
        assert!(
            !spa.on.body.contains(marker),
            "{marker:?} leaked into the snapshot body: {}",
            spa.on.body
        );
        assert!(!text.contains(marker), "{marker:?} leaked into MCP text");
    }
    // 診断が DOM へ書き戻されていない。
    for marker in ["DIAG_MARKER_42", "CONSOLE_MARKER_7"] {
        assert!(
            !spa.on.html.contains(marker),
            "{marker:?} was written into the executed HTML"
        );
    }

    eprintln!("case: PLUG-4 envelope keeps exactly the three keys and survives re-serialization");
    assert_eq!(text, spa.on.body, "interpret-like transform changed bytes");
    let value: Value = serde_json::from_str(&spa.on.body).expect("JSON");
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["tree", "truncated", "url"]);
}

/// 1 サイト分の測定行（有効時）。
struct SiteRun {
    row: ReductionRow,
    stage: Stage,
    src_fetch_failures: Vec<SrcFetchErrorKind>,
    executed_html_tokens: usize,
    body: String,
}

fn measure_site(
    handle: &PageScriptHandle,
    counter: &TokenCounter,
    name: &str,
    html: &str,
    url: &str,
) -> SiteRun {
    let nav = NavigationState::new();
    let piped = pipeline(handle, &nav, url, html);
    let text = reserialize(&piped.body);
    let raw = counter.count(html);
    let snap = counter.count(&text);
    let truncated = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("truncated").and_then(Value::as_bool))
        .expect("truncated flag");
    let pct = reduction_pct(raw, snap).expect("raw tokens > 0");
    let src_fetch_failures = piped
        .scripts
        .iter()
        .filter_map(|s| match s {
            ScriptOutcome::SrcFetchFailed { kind } => Some(*kind),
            _ => None,
        })
        .collect();
    SiteRun {
        row: ReductionRow {
            name: name.to_string(),
            bytes: html.len(),
            raw_html_tokens: raw,
            snapshot_tokens: snap,
            reduction_pct: pct,
            snapshot_truncated: truncated,
        },
        stage: piped.stage,
        src_fetch_failures,
        executed_html_tokens: counter.count(&piped.html),
        body: piped.body,
    }
}

/// `PLUG-4`・`JS-4`: 有効時の 5 サイトと SPA フィクスチャの測定。Markdown の表を返す。
fn measurement_cases(handle: &PageScriptHandle, spa: &SpaRun) -> String {
    eprintln!("case: PLUG-4/JS-4 five-site measurement with page scripts enabled");
    let counter = TokenCounter::new().expect("tokenizer");
    let mut rows = Vec::new();
    let mut report = String::new();
    writeln!(
        report,
        "### JS enabled: per site\n\n| site | rawHtmlTokens | snapshotTokens | reductionPct | truncated | executedHtmlTokens | script stage |\n| ---- | ------------- | -------------- | ------------ | --------- | ------------------ | ------------ |"
    )
    .expect("fmt");
    for name in SITES {
        let path = fixtures_dir().join(format!("{name}.html"));
        let html = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
        let url = site_url(name);
        let on = measure_site(handle, &counter, name, &html, &url);
        // 既定オフ相当の基準（同一プロセス内で測り、絶対値は再ピン留めしない）。
        let off_body = direct_body(&url, &html);
        let off_text = reserialize(&off_body);
        let off_snap = counter.count(&off_text);

        let has_script = html.contains("<script");
        if has_script {
            assert_eq!(on.stage, Stage::Executed, "{name}");
        } else {
            assert_eq!(
                on.stage,
                Stage::Skipped("NoScriptElement".to_string()),
                "{name}"
            );
            assert_eq!(on.body, off_body, "{name}: body changed without scripts");
            assert_eq!(on.row.snapshot_tokens, off_snap, "{name}");
        }
        if name == "login-form" {
            // loopback 上の `<script src>` は接続前に拒否され、JS は 1 行も実行されない。
            assert_eq!(
                on.src_fetch_failures,
                vec![SrcFetchErrorKind::DisallowedAddress],
                "{name}"
            );
            // JS が走らないため、DOM の再シリアライズを経ても snapshot は既定オフとバイト一致する。
            assert_eq!(
                on.body, off_body,
                "{name}: re-serialization changed the snapshot"
            );
        }
        writeln!(
            report,
            "| {} | {} | {} | {:.1} | {} | {} | {:?} (off snapshotTokens {}) |",
            on.row.name,
            on.row.raw_html_tokens,
            on.row.snapshot_tokens,
            on.row.reduction_pct,
            on.row.snapshot_truncated,
            on.executed_html_tokens,
            on.stage,
            off_snap
        )
        .expect("fmt");
        rows.push(on.row);
    }
    let s = summarize(&rows).expect("rows");
    writeln!(
        report,
        "\n### JS enabled: summary\n\n| pages | meanReductionPct | minReductionPct | maxReductionPct | medianSnapshotTokens | gapToThresholdPts (display only) |\n| ----- | ---------------- | --------------- | --------------- | -------------------- | -------------------------------- |\n| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
        s.pages,
        s.mean_reduction_pct,
        s.min_reduction_pct,
        s.max_reduction_pct,
        s.median_snapshot_tokens,
        PLUG4_THRESHOLD_PCT_FOR_DISPLAY - s.mean_reduction_pct
    )
    .expect("fmt");

    eprintln!("case: PLUG-4/JS-4 SPA fixture measurement");
    let raw = counter.count(fixtures::SPA_HTML);
    let executed = counter.count(&spa.on.html);
    let on_snap = counter.count(&reserialize(&spa.on.body));
    let off_snap = counter.count(&reserialize(&spa.off_body));
    assert!(
        on_snap > off_snap,
        "rendered elements must add tokens: on {on_snap} off {off_snap}"
    );
    let on_pct = reduction_pct(raw, on_snap).expect("raw > 0");
    let off_pct = reduction_pct(raw, off_snap).expect("raw > 0");
    assert!(on_pct.is_finite() && off_pct.is_finite());
    writeln!(
        report,
        "\n### SPA fixture\n\n| rawHtmlTokens | executedHtmlTokens | snapshotTokensOff | snapshotTokensOn | reductionPctOff | reductionPctOn |\n| ------------- | ------------------ | ----------------- | ---------------- | --------------- | -------------- |\n| {raw} | {executed} | {off_snap} | {on_snap} | {off_pct:.1} | {on_pct:.1} |"
    )
    .expect("fmt");
    report
}
