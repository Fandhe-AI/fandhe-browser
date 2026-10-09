//! compat_js_api_spike: ページ内 JS が要求する Web API を実測するスパイク
//! （TASK-106・MS-6・`JS-5`。b5・d2・d3 が対象）。
//!
//! 対象ページの HTML と `<script>` を `Fetcher` 経由だけで取得し、DOM バインディングを
//! 一切注入しない素の JS エンジンで文書順に評価して、script ごとの最初の未定義 API 名と
//! 例外分類を JSON Lines で出力する。結果は JS-5 の最小 DOM バインディングの API 範囲を
//! 確定する材料になる（範囲確定・追加 issue の要否判断は人間の担当）。
//!
//! 呼び出し元: 人間が手動実行する（実サイトへ出るため CI・`cargo test` では実行しない）。
//! 純ロジックと自己テストは `spike.rs`（オフライン・決定的）。手順は
//! `harness/compat-practical/README.md` の「js_api_spike」節。
//!
//! 終了コード: 0 = 記録完了（評価失敗はデータでありゲートではない）/ 1 = 書き込み失敗 /
//! 2 = 入力・使用エラー（引数不正・エンジン未同梱・ランタイム構築失敗）。
//!
//! UA は `Fetcher` 既定の `fandhe-browser/<ver>` のまま。anti-bot の回避や DOM / navigator の
//! 偽装は行わない（SEC 系）。簡易実装の制約は `spike.rs` 冒頭を参照（REPAIR-3）。

mod spike;

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fandhe_browser_core::FetchResponse;
use fandhe_browser_core::config::Config;
use fandhe_browser_core::js_stub::JsRuntime;
use fandhe_browser_core::{FetchOptions, Fetcher, ParseOptions, parse_document};
use reqwest::Url;
use spike::{
    Args, FetchErrorClass, FetchFailure, MAX_BODY_BYTES, MAX_EVAL_BYTES, ScriptEntry, ScriptRecord,
    ScriptSource, SiteRecord, document_base_url, fetch_script, meta_line, outcome_of_error,
    resolve_script_url, same_origin, sanitize_debug,
};

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn main() -> ExitCode {
    // V8 は子プロセスで評価されるため、ワーカー起動要求なら最初に処理する（JS-1 の契約）。
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    let args = match spike::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("usage error: {}", e.0);
            return ExitCode::from(2);
        }
    };
    run(args)
}

/// `--engine` 指定（固定 TOML 定数）または既定設定から JS ランタイムを作る。
fn make_runtime(args: &Args) -> Result<JsRuntime, String> {
    let cfg = match args.engine {
        Some(choice) => Config::from_toml_str(choice.toml()).map_err(|e| e.to_string())?,
        None => Config::from_toml_str("").map_err(|e| e.to_string())?,
    };
    if cfg.js().engine().is_none() {
        return Err(
            "no JS engine is compiled in; rebuild with --features js-v8 or --features js-boa"
                .to_string(),
        );
    }
    JsRuntime::from_config(cfg.js()).map_err(|e| e.to_string())
}

fn run(args: Args) -> ExitCode {
    // 起動時に 1 回作って構成を検証する（サイトごとの runtime は measure_site で作り直す）。
    let probe = match make_runtime(&args) {
        Ok(r) => r,
        Err(m) => {
            eprintln!("error: {m}");
            return ExitCode::from(2);
        }
    };
    let engine_name = probe.engine_kind().map_or("none".to_string(), |k| {
        format!("{k:?}").to_ascii_lowercase()
    });
    drop(probe);

    // 構成の検証だけ（取得ごとの Fetcher は fetch_bounded が残り時間つきで作る）。
    if let Err(e) = make_fetcher(Duration::from_secs(args.fetch_timeout_sec)) {
        eprintln!("error: cannot build fetcher: {e}");
        return ExitCode::from(2);
    }
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

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut lines = vec![meta_line(&args, &engine_name, now)];
    let deadline = Instant::now() + Duration::from_secs(args.total_timeout_sec);
    for (id, url) in &args.targets {
        match make_runtime(&args) {
            Ok(runtime) => rt.block_on(measure_site(&args, runtime, id, url, deadline, &mut lines)),
            Err(m) => {
                eprintln!("error: {m}");
                return ExitCode::from(2);
            }
        }
    }
    match write_output(&args, &lines) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: cannot write output: {e}");
            ExitCode::from(1)
        }
    }
}

fn make_fetcher(timeout: Duration) -> Result<Fetcher, fandhe_browser_core::Error> {
    Fetcher::new(
        FetchOptions::new()
            .with_timeout(timeout)
            .with_max_body_bytes(MAX_BODY_BYTES),
    )
}

/// 総時間上限の残り時間で打ち切る取得。tokio の time 機能（依存の feature 追加）に頼らず、
/// 取得ごとの `Fetcher` のタイムアウトを `min(--fetch-timeout, 残り時間)` にして実現する。
/// 期限切れ（取得前・取得中）は `TotalTimeLimit` として返す。
async fn fetch_bounded(
    args: &Args,
    url: &str,
    deadline: Instant,
) -> Result<FetchResponse, FetchFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(FetchFailure::of(FetchErrorClass::TotalTimeLimit));
    }
    let timeout = remaining.min(Duration::from_secs(args.fetch_timeout_sec));
    let fetcher = make_fetcher(timeout).map_err(|_| FetchFailure::of(FetchErrorClass::Other))?;
    fetch_script(&fetcher, url).await.map_err(|f| {
        if f.class == FetchErrorClass::Timeout && Instant::now() >= deadline {
            FetchFailure::of(FetchErrorClass::TotalTimeLimit)
        } else {
            f
        }
    })
}

/// 出力先へ書く。`--out` は一意名の一時ファイル経由（`write_atomic`）で置換する。
fn write_output(args: &Args, lines: &[String]) -> std::io::Result<()> {
    let mut text = lines.join("\n");
    text.push('\n');
    match &args.out {
        None => std::io::stdout().write_all(text.as_bytes()),
        Some(path) => write_atomic(path, text.as_bytes()),
    }
}

/// 同一ディレクトリに一意名の一時ファイルを `create_new` で排他作成して書き、rename で置換する。
/// 既存ファイル・シンボリックリンクを切り詰めず、自分が作成した一時ファイルだけを rename / 削除する。
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("--out has no file name"))?
        .to_os_string();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    for attempt in 0..16u32 {
        let mut tmp_name = name.clone();
        tmp_name.push(format!(".{}.{nanos}.{attempt}.tmp", std::process::id()));
        let tmp = path.with_file_name(tmp_name);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                drop(file);
                std::fs::rename(&tmp, path)
            });
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return result;
    }
    Err(std::io::Error::other("could not create a unique temp file"))
}

/// 1 サイトを計測し、script 行とサイト行を `lines` へ積む。
async fn measure_site(
    args: &Args,
    mut runtime: JsRuntime,
    id: &str,
    page_url: &Url,
    deadline: Instant,
    lines: &mut Vec<String>,
) {
    let start = Instant::now();
    let mut site = SiteRecord {
        id: id.to_string(),
        ..Default::default()
    };
    let finish = |site: SiteRecord, lines: &mut Vec<String>| {
        let mut site = site;
        site.elapsed_ms = elapsed_ms(start);
        lines.push(site.to_json_line());
    };

    let page = match fetch_bounded(args, page_url.as_str(), deadline).await {
        Ok(p) => p,
        Err(f) => {
            site.page_error = Some(f.class.as_str());
            site.page_status = f.status;
            finish(site, lines);
            return;
        }
    };
    site.page_status = Some(page.status());
    let final_url = Url::parse(page.final_url()).unwrap_or_else(|_| page_url.clone());
    let html = page.body_text_lossy();
    let parsed = match parse_document(&html, &ParseOptions::default().with_scripting_enabled(true))
    {
        Ok(p) => p,
        Err(_) => {
            site.page_error = Some("parse_error");
            finish(site, lines);
            return;
        }
    };
    let doc_base = document_base_url(&parsed.document, &final_url);
    let (entries, over_limit) = spike::extract_scripts(&parsed.document, args.max_scripts);
    if over_limit > 0 {
        site.scripts_over_limit = over_limit as u64;
        site.truncated = Some("max_scripts");
    }

    for entry in entries {
        if site.aborted.is_some() {
            break;
        }
        let rec = if Instant::now() >= deadline {
            skipped_record(id, &entry, "total_time_limit")
        } else {
            eval_entry(
                args,
                &mut runtime,
                id,
                &entry,
                (&doc_base, &final_url),
                deadline,
                &mut site,
            )
            .await
        };
        site.absorb(&rec);
        lines.push(rec.to_json_line());
    }
    finish(site, lines);
}

fn skipped_record(id: &str, entry: &ScriptEntry, reason: &'static str) -> ScriptRecord {
    ScriptRecord {
        id: id.to_string(),
        order: entry.order,
        external: matches!(entry.source, ScriptSource::External(_)),
        outcome: "skipped",
        skip_reason: Some(reason),
        ..Default::default()
    }
}

async fn eval_entry(
    args: &Args,
    runtime: &mut JsRuntime,
    id: &str,
    entry: &ScriptEntry,
    (doc_base, page_url): (&Url, &Url),
    deadline: Instant,
    site: &mut SiteRecord,
) -> ScriptRecord {
    if let Some(reason) = entry.skip {
        return skipped_record(id, entry, reason);
    }
    let mut rec = ScriptRecord {
        id: id.to_string(),
        order: entry.order,
        ..Default::default()
    };
    let external_body: String;
    let source: &str = match &entry.source {
        ScriptSource::Inline(text) => text,
        ScriptSource::External(src) => {
            rec.external = true;
            let url = match resolve_script_url(doc_base, src) {
                Ok(u) => u,
                Err(class) => {
                    rec.outcome = "fetch_error";
                    rec.fetch_error = Some(class.as_str());
                    return rec;
                }
            };
            rec.same_origin = Some(same_origin(&url, page_url));
            let fetch_start = Instant::now();
            match fetch_bounded(args, url.as_str(), deadline).await {
                Ok(resp) => {
                    external_body = resp.body_text_lossy();
                    rec.elapsed_ms = elapsed_ms(fetch_start);
                    &external_body
                }
                Err(f) if f.class == FetchErrorClass::TotalTimeLimit => {
                    return skipped_record(id, entry, "total_time_limit");
                }
                Err(f) => {
                    rec.outcome = "fetch_error";
                    rec.fetch_error = Some(f.class.as_str());
                    return rec;
                }
            }
        }
    };
    rec.bytes = source.len() as u64;
    if source.len() > MAX_EVAL_BYTES {
        rec.outcome = "too_large";
        return rec;
    }
    // 取得中に期限を過ぎた場合は評価を始めない。
    if Instant::now() >= deadline {
        return skipped_record(id, entry, "total_time_limit");
    }
    let eval_start = Instant::now();
    let result = runtime.execute(source);
    rec.elapsed_ms = elapsed_ms(eval_start);
    match result {
        Ok(_) => rec.outcome = "ok",
        Err(e) => {
            if args.debug_messages {
                eprintln!("[{id}#{}] {}", entry.order, sanitize_debug(&e.to_string()));
            }
            let (outcome, class, discarded) = outcome_of_error(&e);
            rec.outcome = outcome;
            if let Some(c) = class {
                rec.error_kind = c.error_kind;
                rec.message_class = Some(c.message_class);
                rec.missing_api = c.missing_api;
            }
            if discarded {
                site.aborted = Some("context_discarded");
            }
        }
    }
    rec
}
