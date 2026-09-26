//! `competitor_lightpanda` の計測本体（`measure.rs`）の結合テスト。
//!
//! レビュー指摘（コーディネーター指示。PR #442 再々々々々々々々々レビュー・
//! `crates/fandhe-browser-core/Cargo.toml:57`）: AGENTS.md「ユニットテストと
//! 結合テストの併置」に基づき、fixture サーバーの起動・対象プロセスとの
//! 通信・計測結果・終了コードまでを、対象バイナリを模したローカルプロセスで
//! 通す結合テストを追加する。対象は `PERF-3`（cold start）・`PERF-6`
//! （idle RSS）・`AISNAP-1`（MCP トークン削減率）の計測経路
//! （TASK-84（84.1）・Issue #211）。
//!
//! 依存を追加せず std のみで完結させるため（dependency-policy.md）、対象
//! バイナリを模したローカルプロセスは、このテストバイナリ自身を
//! `std::env::current_exe()` で再実行したものにする。役割は環境変数ではなく
//! 引数（`--fandhe-fake-role <role>`）で渡す。理由: (1) `serve_args`/
//! `mcp_args` は元々「実行ファイルへの引数テンプレート」という形なので、
//! 対象バイナリを模す場合も同じ経路（引数）で振る舞いを渡すのが自然、
//! (2) 環境変数はプロセス起動時に固定され、同じテストバイナリを複数の役割
//! （fake CDP・fake MCP・実際のテスト本体）で使い分ける際に
//! `std::env::set_var`（`edition 2024` では `unsafe`。coding-rust.md
//! 「unsafe は原則禁止」）を避けられる。
//!
//! この `[[test]]` ターゲットは `harness = false`（`Cargo.toml` 参照）で
//! 独自の `main` を持つ。理由: fake MCP は stdout に JSON-RPC 応答の行だけを
//! 書く契約（`measure::McpClient` が 1 行 = 1 JSON 値として読む）だが、
//! 通常の libtest ハーネスは "running N tests" 等の文字列を stdout へ書くため、
//! 同じテストバイナリを fake MCP 役として再実行すると、その出力が JSON-RPC
//! 応答へ混入してしまう。

#[path = "measure.rs"]
mod measure;
#[path = "support.rs"]
mod support;

use std::io::{BufRead, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use measure::{Target, measure_cold_start, measure_token_reduction, run_all};
use support::{JsonValue, Outcome, parse_json};

/// 役割切り替えの合図となる引数（`Target::serve_args`/`mcp_args` の
/// テンプレートとして [`fake_cdp_serve_args`]・[`FAKE_MCP_ARGS`] が使う）。
const FAKE_ROLE_FLAG: &str = "--fandhe-fake-role";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(FAKE_ROLE_FLAG) {
        run_fake_role(&args[2..]);
        return;
    }
    run_test_cases();
}

/// fake CDP（readiness probe に応答するローカル HTTP サーバー）・fake MCP
/// （stdio JSON-RPC サーバー）のいずれかとして振る舞う。`args` は
/// [`FAKE_ROLE_FLAG`] に続く引数列。
fn run_fake_role(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("cdp-ok") => run_fake_cdp(args, true),
        Some("cdp-invalid") => run_fake_cdp(args, false),
        Some("mcp-ok") => run_fake_mcp(false),
        Some("mcp-stale") => run_fake_mcp(true),
        Some("mcp-disconnect") => run_fake_mcp_disconnect(),
        other => {
            eprintln!("competitor_lightpanda_measure: unknown fake role: {other:?}");
            std::process::exit(2);
        }
    }
}

/// `--port <port>` を引数列から取り出す。
fn parse_port(args: &[String]) -> Option<u16> {
    let idx = args.iter().position(|a| a == "--port")?;
    args.get(idx + 1)?.parse().ok()
}

/// fake CDP: `GET /json/version` に応答するだけの最小限の HTTP サーバー。
///
/// `valid` が `true` なら [`support::looks_like_browser_readiness_response`]
/// が真になる本文（`Browser` フィールドを含む JSON オブジェクト）を返し、
/// `false` なら真にならない本文（既知フィールドを含まない JSON オブジェクト）
/// を返す（`measure_cold_start` が readiness を検知できず、期限切れで
/// `Outcome::Error` になることを検証するケース 2 が使う）。
///
/// キルされるまで接続を受け続ける（`kill_process_group`/`Child::kill` で
/// 終了させる前提。ローカルの loopback 接続のみを相手にするテスト専用の
/// サーバーであり、外部入力の検証・タイムアウトは
/// `measure::FixtureServer`（本番の fixture 配信サーバー）ほど厳格にしない）。
fn run_fake_cdp(args: &[String], valid: bool) {
    let Some(port) = parse_port(args) else {
        eprintln!("fake cdp: missing --port");
        std::process::exit(2);
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fake cdp: bind failed: {e}");
            std::process::exit(2);
        }
    };
    let body: &[u8] = if valid {
        b"{\"Browser\":\"FakeBrowser/1.0\"}"
    } else {
        b"{\"unrelated\":true}"
    };
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut reader = std::io::BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        });
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line == "\r\n" || line == "\n" => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }
}

/// fake MCP: stdio 越しの JSON-RPC 2.0 サーバー。`initialize`・
/// `notifications/initialized`・`tools/call`（`goto`/`html`/`tree`）にだけ
/// 応答する（このベンチが実際に呼ぶメソッドのみ）。
///
/// `html`/`tree` の応答文字数は [`HTML_TEXT_LEN`]/[`TREE_TEXT_LEN`] に固定し、
/// トークン削減率（`AISNAP-1`）の期待値を具体値で検証できるようにする
/// （coding-rust.md「テスト」: 「期待値は具体値で書く」）。それぞれの
/// テキストは「その `goto` が最後に指した fixture のマーカー」（各ページの
/// `<title>`。`support::fixture_marker`） + 埋め草文字で固定長にした文字列にする。
///
/// `stale` が `true` の場合、`goto` を受けてもマーカーを更新しない（最初の
/// `goto` のマーカーを使い続ける）ことで、「2 件目以降の `goto` の後も
/// 1 件目のページ内容を返し続ける（実際には遷移していない）」対象を模す
/// （レビュー指摘 P1。Codex。PR #442 再々々々々々々々々々レビュー・
/// measure.rs:1450。結合テストのケース 5
/// `stale_mcp_response_after_second_goto_is_error` が使う）。
const HTML_TEXT_LEN: usize = 400;
const TREE_TEXT_LEN: usize = 100;

fn run_fake_mcp(stale: bool) {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut current_marker: Option<&'static str> = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(value) = parse_json(line.trim()) else {
            continue;
        };
        let method = value
            .get("method")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        let id = value.get("id").and_then(JsonValue::as_f64);
        let Some(id) = id else {
            // 通知（`notifications/initialized` 等）は応答しない。
            continue;
        };
        let response = match method {
            "initialize" => Some(format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}}}}}}"
            )),
            "tools/call" => {
                let name = value
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let text = match name {
                    "goto" => {
                        let url = value
                            .get("params")
                            .and_then(|p| p.get("arguments"))
                            .and_then(|a| a.get("url"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        // `stale` でなければ毎回のマーカーを更新し、`stale` なら
                        // 最初の 1 回だけ設定して以降は無視する（実際には遷移
                        // していない対象を模す）。
                        if !stale || current_marker.is_none() {
                            current_marker = support::FIXTURE_TABLE
                                .iter()
                                .find(|(path, _, _)| url.ends_with(*path))
                                .and_then(|(path, _, _)| support::fixture_marker(path));
                        }
                        String::from("ok")
                    }
                    "html" => padded_marker_text(current_marker, HTML_TEXT_LEN),
                    "tree" => padded_marker_text(current_marker, TREE_TEXT_LEN),
                    _ => String::new(),
                };
                Some(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}],\"isError\":false}}}}"
                ))
            }
            _ => None,
        };
        if let Some(response) = response {
            let _ = stdout.write_all(response.as_bytes());
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        }
    }
}

/// `marker`（現在 `goto` 済みとみなしている fixture のマーカー。未設定なら
/// マーカーを含まない埋め草のみ）を先頭に置き、`pad` 文字（`marker` が無い
/// 場合は `'x'`。ある場合はマーカーと紛れない `'a'`/`'b'` は呼び出し側で
/// 区別する必要が無いためここでは `'a'` に統一）で `total_len` 文字まで
/// 埋めた文字列を返す。`marker` の文字数によらず常に `total_len` 文字に
/// なるため、トークン削減率（`AISNAP-1`）の期待値を具体値で検証できる。
fn padded_marker_text(marker: Option<&'static str>, total_len: usize) -> String {
    let prefix = marker.unwrap_or("");
    let prefix_len = prefix.chars().count();
    let pad_len = total_len.saturating_sub(prefix_len);
    format!("{prefix}{}", "a".repeat(pad_len))
}

/// fake MCP（切断版）: `initialize` リクエストを 1 件読み取ったあと、改行を
/// 送らずに応答の一部だけを書いて終了する。
///
/// `measure::McpClient` の読み取りスレッドは改行に達する前に子プロセスの
/// 標準出力が閉じたことを `support::read_line_bounded` の
/// `ErrorKind::UnexpectedEof` 契約で検知し、`line_read_error` へ記録して
/// スレッドを終了する（`mpsc::Sender` の drop により、待機中の
/// `recv_timeout` は `RecvTimeoutError::Disconnected` で即座に返る）。
/// ケース 3（`mcp_disconnect_without_newline_fails_immediately`）が、20 秒の
/// 呼び出しタイムアウトを待たずに即時でエラーになることを検証する。
fn run_fake_mcp_disconnect() {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    // `initialize` リクエストの書き込みが正常に完了してから切断するため、
    // 1 行読んでから応答を返す（読まずに即終了すると、親側の書き込み自体が
    // `BrokenPipe` で失敗し、検証したい「応答の途中切断」とは異なる経路に
    // なってしまう）。
    let _ = reader.read_line(&mut line);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{");
    let _ = stdout.flush();
    // 改行を送らないまま終了し、標準出力を閉じる。
}

/// 通常のテスト実行（`--fandhe-fake-role` を伴わない起動）。`#[test]` 属性は
/// 使わず（`harness = false`）、ケースを順に呼んで最初の panic で失敗させる
/// （coding-rust.md「テストの skip・ignore・アサーション弱体化で CI を
/// 通さない」。個々のケースを黙って握りつぶさない）。
fn run_test_cases() {
    eprintln!("case: successful_measurement_path_reports_measured_values_and_exit_code_zero");
    successful_measurement_path_reports_measured_values_and_exit_code_zero();
    eprintln!("case: cold_start_invalid_readiness_response_is_error_with_exit_code_one");
    cold_start_invalid_readiness_response_is_error_with_exit_code_one();
    eprintln!("case: mcp_disconnect_without_newline_fails_immediately");
    mcp_disconnect_without_newline_fails_immediately();
    eprintln!("case: unconfigured_target_is_skipped_with_exit_code_zero");
    unconfigured_target_is_skipped_with_exit_code_zero();
    eprintln!("case: stale_mcp_response_after_second_goto_is_error");
    stale_mcp_response_after_second_goto_is_error();
    eprintln!("competitor_lightpanda_measure: all cases passed");
}

/// fake CDP（`cdp-ok`）・fake MCP（`mcp-ok`）を対象にした 1 件の [`Target`] を
/// 構築する。
fn fake_target(name: &'static str) -> Target {
    let current_exe = std::env::current_exe().expect("current_exe");
    Target {
        name,
        bin: Some(current_exe),
        serve_args: vec![
            FAKE_ROLE_FLAG.to_string(),
            "cdp-ok".to_string(),
            "--port".to_string(),
            "{port}".to_string(),
        ],
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-ok".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    }
}

/// ケース 1（正常系）: `PERF-1`（バイナリサイズ）・`PERF-3`（cold start）・
/// `PERF-6`（idle RSS。Windows は `Unsupported`）・`AISNAP-1`（トークン削減率）
/// のすべてが `measured`（Windows の `idleRssKb` のみ `unsupported`）になり、
/// `run_all` の終了コードが `0` になることを、fixture サーバーの起動から
/// 対象プロセス（fake CDP・fake MCP。ともにこのテストバイナリ自身の再実行）
/// との通信・計測結果・終了コードまで通しで検証する。
fn successful_measurement_path_reports_measured_values_and_exit_code_zero() {
    let target = fake_target("fake-browser");
    let expected_binary_size = std::fs::metadata(std::env::current_exe().expect("current_exe"))
        .expect("metadata")
        .len() as f64;

    let (body, exit_code) = run_all(&[target], 1);
    let value = parse_json(&body).expect("run_all output should be valid JSON");
    let report = value
        .get("fake-browser")
        .expect("report for fake-browser target");

    assert_eq!(
        report
            .get("binarySizeBytes")
            .and_then(|v| v.get("status"))
            .and_then(JsonValue::as_str),
        Some("measured"),
        "binarySizeBytes should be measured: {body}"
    );
    assert_eq!(
        report
            .get("binarySizeBytes")
            .and_then(|v| v.get("value"))
            .and_then(JsonValue::as_f64),
        Some(expected_binary_size),
        "binarySizeBytes should equal the size of this test binary itself (PERF-1): {body}"
    );

    let cold_start_status = report
        .get("coldStartMs")
        .and_then(|v| v.get("status"))
        .and_then(JsonValue::as_str);
    assert_eq!(
        cold_start_status,
        Some("measured"),
        "coldStartMs should be measured (PERF-3): {body}"
    );
    let cold_start_value = report
        .get("coldStartMs")
        .and_then(|v| v.get("value"))
        .and_then(JsonValue::as_f64)
        .expect("coldStartMs value");
    assert!(
        cold_start_value > 0.0,
        "coldStartMs should be a positive duration: {cold_start_value}"
    );

    let idle_rss = report.get("idleRssKb").expect("idleRssKb field");
    let idle_rss_status = idle_rss.get("status").and_then(JsonValue::as_str);
    if cfg!(windows) {
        assert_eq!(
            idle_rss_status,
            Some("unsupported"),
            "idleRssKb should be unsupported on Windows (PERF-6): {body}"
        );
    } else {
        assert_eq!(
            idle_rss_status,
            Some("measured"),
            "idleRssKb should be measured on unix (PERF-6): {body}"
        );
        let idle_rss_value = idle_rss
            .get("value")
            .and_then(JsonValue::as_f64)
            .expect("idleRssKb value");
        assert!(
            idle_rss_value > 0.0,
            "idleRssKb should be a positive KB value: {idle_rss_value}"
        );
    }

    // `approx_tokens` は `ceil(chars / 4)` なので、html=400 文字・tree=100 文字
    // なら html_tok=100・tree_tok=25 となり、削減率は
    // `(1 - 25/100) * 100 = 75.0`（丸め誤差の出ない除算）になる。
    // `FIXTURE_TABLE` は 3 ページあり、fake MCP はどのページでも同じ長さの
    // 応答を返すため、中央値も 75.0 になる(AISNAP-1)。
    assert_eq!(
        report
            .get("tokenReductionPct")
            .and_then(|v| v.get("status"))
            .and_then(JsonValue::as_str),
        Some("measured"),
        "tokenReductionPct should be measured (AISNAP-1): {body}"
    );
    assert_eq!(
        report
            .get("tokenReductionPct")
            .and_then(|v| v.get("value"))
            .and_then(JsonValue::as_f64),
        Some(75.0),
        "tokenReductionPct should be exactly 75.0 given the fixed fake MCP response lengths: {body}"
    );

    assert_eq!(
        exit_code, 0,
        "exit code should be 0 when every measurement succeeds: {body}"
    );
}

/// ケース 2: fake CDP（`cdp-invalid`）が readiness probe に不正な応答
/// （`looks_like_browser_readiness_response` が偽になる JSON）を返し続ける
/// 場合、cold start（`PERF-3`）の試行は readiness 期限切れで `Outcome::Error`
/// になり、`bench_exit_code` 相当の判定が非ゼロになることを確認する。
///
/// `run_all`（4 項目すべてを計測。トークン削減率だけで最大 20 秒×サイト数）
/// ではなく `measure_cold_start` を直接呼び、この 1 項目（`spawn_and_wait_ready`
/// の readiness 期限 10 秒×trials）だけを検証する（advisor 指摘: 無効な
/// readiness 応答は期限切れまで待つ以外の速い失敗経路が無いため、この
/// コストは避けられないが、`run_all` 経由にして他 3 項目分の時間まで
/// 上乗せしない）。
fn cold_start_invalid_readiness_response_is_error_with_exit_code_one() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-invalid",
        bin: Some(current_exe),
        serve_args: vec![
            FAKE_ROLE_FLAG.to_string(),
            "cdp-invalid".to_string(),
            "--port".to_string(),
            "{port}".to_string(),
        ],
        mcp_args: Vec::new(),
        serve_args_error: None,
        mcp_args_error: None,
    };

    let outcome = measure_cold_start(&target, 1);
    match outcome {
        Outcome::Error(reason) => {
            assert!(
                reason.contains("fake-browser-invalid"),
                "error reason should mention the target name: {reason}"
            );
        }
        other => panic!(
            "expected Outcome::Error when the fake CDP never returns a recognizable readiness response, got {other:?}"
        ),
    }
}

/// ケース 3: fake MCP（`mcp-disconnect`）が改行を送らないまま標準出力を
/// 閉じる場合、`initialize` 呼び出し（20 秒のタイムアウト）が即時に
/// `Outcome::Error` になり、20 秒を待たないことを経過時間で確認する。
fn mcp_disconnect_without_newline_fails_immediately() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-disconnect",
        bin: Some(current_exe),
        serve_args: Vec::new(),
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-disconnect".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    };

    // `validate_local_bench_url` は文字列としての形式検証のみ行い、実際の
    // 接続はしないため、ダミーの fixture サーバー情報（存在しないポート）を
    // 渡しても `initialize` が失敗する経路には影響しない。
    let dummy_port: u16 = 1;
    let dummy_base_url = format!("http://127.0.0.1:{dummy_port}");

    let start = Instant::now();
    let outcome = measure_token_reduction(&target, &dummy_base_url, dummy_port);
    let elapsed = start.elapsed();

    match outcome {
        Outcome::Error(reason) => {
            assert!(
                reason.contains("could not be decoded"),
                "error reason should mention the decode failure caused by the missing newline: {reason}"
            );
        }
        other => panic!(
            "expected Outcome::Error when the fake MCP disconnects without a trailing newline, got {other:?}"
        ),
    }
    assert!(
        elapsed < Duration::from_secs(10),
        "disconnecting without a newline should fail immediately, not wait for the 20s call timeout: {elapsed:?}"
    );
}

/// ケース 4: 対象バイナリが未設定（`bin: None`）の場合、4 項目すべてが
/// `skipped` になり、終了コードが `0` になることを確認する。
fn unconfigured_target_is_skipped_with_exit_code_zero() {
    let target = Target {
        name: "unconfigured",
        bin: None,
        serve_args: Vec::new(),
        mcp_args: Vec::new(),
        serve_args_error: None,
        mcp_args_error: None,
    };

    let (body, exit_code) = run_all(&[target], 1);
    let value = parse_json(&body).expect("run_all output should be valid JSON");
    let report = value
        .get("unconfigured")
        .expect("report for unconfigured target");

    for field in [
        "binarySizeBytes",
        "coldStartMs",
        "idleRssKb",
        "tokenReductionPct",
    ] {
        assert_eq!(
            report
                .get(field)
                .and_then(|v| v.get("status"))
                .and_then(JsonValue::as_str),
            Some("skipped"),
            "{field} should be skipped when the target binary is not configured: {body}"
        );
    }
    assert_eq!(
        exit_code, 0,
        "exit code should be 0 when every measurement is skipped: {body}"
    );
}

/// ケース 5: fake MCP（`mcp-stale`）が `goto` を受けてもマーカーを更新せず、
/// 常に 1 件目のページ（`/article.html`）の内容を返し続ける場合、`goto` の
/// JSON-RPC 応答自体は形式上妥当でも、2 件目以降の fixture
/// （`/listing.html`・`/form.html`）については `html`/`tree` にその
/// fixture 自身のマーカーが含まれないため、内容ベースの検証
/// （レビュー指摘 P1。Codex。PR #442 再々々々々々々々々々レビュー・
/// measure.rs:1450）によって計測失敗（`Outcome::Error`）になることを
/// 確認する。
fn stale_mcp_response_after_second_goto_is_error() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-stale",
        bin: Some(current_exe),
        serve_args: Vec::new(),
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-stale".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    };

    // `validate_local_bench_url` は文字列としての形式検証のみ行い、実際の
    // 接続はしないため、ダミーの fixture サーバー情報（存在しないポート）を
    // 渡しても各サイトへの `goto`（fake MCP 側は URL 文字列だけを見る）には
    // 影響しない。
    let dummy_port: u16 = 1;
    let dummy_base_url = format!("http://127.0.0.1:{dummy_port}");

    let outcome = measure_token_reduction(&target, &dummy_base_url, dummy_port);
    match outcome {
        Outcome::Error(reason) => {
            assert!(
                reason.contains("could not be verified"),
                "error reason should mention the content-based navigation check failing: {reason}"
            );
            assert!(
                reason.contains("listing.html") || reason.contains("form.html"),
                "error reason should mention one of the fixtures that never actually got navigated to: {reason}"
            );
        }
        other => panic!(
            "expected Outcome::Error when the fake MCP keeps returning the first page's content after later goto calls, got {other:?}"
        ),
    }
}
