//! `competitor_lightpanda` の計測本体（`measure.rs`）の結合テスト。
//!
//! AGENTS.md「ユニットテストと
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

use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
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
/// （stdio JSON-RPC サーバー）・プロセスグループ終了テスト用の子/孫
/// プロセスのいずれかとして振る舞う。`args` は [`FAKE_ROLE_FLAG`] に続く
/// 引数列。
fn run_fake_role(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("cdp-ok") => run_fake_cdp(args, CdpMode::Valid),
        Some("cdp-invalid") => run_fake_cdp(args, CdpMode::Invalid),
        Some("cdp-chunked") => run_fake_cdp(args, CdpMode::Chunked),
        Some("cdp-no-cl") => run_fake_cdp(args, CdpMode::NoContentLength),
        Some("cdp-malformed-chunked") => run_fake_cdp(args, CdpMode::MalformedChunked),
        Some("mcp-ok") => run_fake_mcp(McpMode {
            stale: false,
            empty_goto_content: false,
        }),
        Some("mcp-stale") => run_fake_mcp(McpMode {
            stale: true,
            empty_goto_content: false,
        }),
        Some("mcp-empty-goto") => run_fake_mcp(McpMode {
            stale: false,
            empty_goto_content: true,
        }),
        Some("mcp-disconnect") => run_fake_mcp_disconnect(),
        Some("mcp-non-json") => run_fake_mcp_non_json_line(),
        Some("sleep-parent") => run_fake_sleep_parent(),
        Some("sleep-grandchild") => run_fake_sleep_grandchild(),
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

/// fake CDP の応答形（PERF-3 の readiness probe が実プロトコルの各分岐
/// （`Content-Length`・`Transfer-Encoding: chunked`・どちらも無い場合・
/// 不正な chunked 枠組み）を通ることを検証するために切り替える）。
#[derive(Clone, Copy)]
enum CdpMode {
    /// `Content-Length` 付き、readiness と認識される本文。
    Valid,
    /// `Content-Length` 付きだが、readiness と認識されない本文（既存の
    /// `cdp-invalid`。ケース 2 が使う）。
    Invalid,
    /// `Transfer-Encoding: chunked` で readiness と認識される本文を送る
    /// （`measure::probe_once` のチャンクデコードが成功経路を通ることを
    /// 検証する）。
    Chunked,
    /// `Content-Length` も `Transfer-Encoding` も付けず、応答後に接続を
    /// 閉じるだけ（`probe_once` の EOF フォールバック経路を検証する）。
    NoContentLength,
    /// `Transfer-Encoding: chunked` を宣言しつつ、チャンクサイズ行が
    /// 16 進数として不正な応答を送る（`probe_once` のチャンクデコードが
    /// 失敗として扱うことを検証する。readiness は成立せず、
    /// `spawn_and_wait_ready` は 10 秒後にタイムアウトし、最後の失敗理由に
    /// チャンク不正の旨を含める）。
    MalformedChunked,
}

/// fake CDP: `GET /json/version` に応答するだけの最小限の HTTP サーバー。
///
/// キルされるまで接続を受け続ける（`kill_process_group`/`Child::kill` で
/// 終了させる前提。ローカルの loopback 接続のみを相手にするテスト専用の
/// サーバーであり、外部入力の検証・タイムアウトは
/// `measure::FixtureServer`（本番の fixture 配信サーバー）ほど厳格にしない）。
fn run_fake_cdp(args: &[String], mode: CdpMode) {
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
    let body: &[u8] = match mode {
        CdpMode::Invalid => b"{\"unrelated\":true}",
        _ => b"{\"Browser\":\"FakeBrowser/1.0\"}",
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
        match mode {
            CdpMode::Valid | CdpMode::Invalid => {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
            CdpMode::Chunked => {
                let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(header.as_bytes());
                let chunk = format!("{:x}\r\n", body.len());
                let _ = stream.write_all(chunk.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.write_all(b"\r\n0\r\n\r\n");
            }
            CdpMode::NoContentLength => {
                let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
            CdpMode::MalformedChunked => {
                let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(header.as_bytes());
                // 16 進数として不正なチャンクサイズ行（`probe_once` の
                // `read_chunked_body` が `Err` にする経路を踏ませる）。
                let _ = stream.write_all(b"not-a-hex-length\r\n");
            }
        }
        let _ = stream.flush();
    }
}

/// fake MCP の挙動を切り替えるフラグ（PERF-3/AISNAP-1）。
#[derive(Clone, Copy)]
struct McpMode {
    /// `true` なら `goto` を受けてもページ内容を更新しない（最初の `goto`
    /// が取得した内容を使い続ける）ことで、「2 件目以降の `goto` の後も
    /// 1 件目のページ内容を返し続ける（実際には遷移していない）」対象を
    /// 模す（結合テストのケース `stale_mcp_response_after_second_goto_is_error`
    /// が使う）。
    stale: bool,
    /// `true` なら `goto` の `tools/call` 応答の `result.content` を空配列
    /// にする（MCP 2025-06-18 は空配列を許す。`goto` 自体は本文を確認
    /// しないため、この応答でも後続の `html`/`tree` を使った計測が
    /// 成功することを検証する）。
    empty_goto_content: bool,
}

/// fake MCP: stdio 越しの JSON-RPC 2.0 サーバー。`initialize`・
/// `notifications/initialized`・`tools/call`（`goto`/`html`/`tree`）にだけ
/// 応答する（このベンチが実際に呼ぶメソッドのみ）。
///
/// `goto` は受け取った URL へ実際に [`http_get_body`] で HTTP GET を行い、
/// 取得した本文をそのまま `html` の応答として使う。`tree` は、その本文の
/// `<h1>...</h1>` 見出しテキスト（[`extract_h1`]）を含む簡約テキストにする
/// （実際のアクセシビリティツリー実装を模す最小限のスタブ。coding-rust.md
/// 「テスト」: 「期待値は具体値で書く」ため、期待するトークン削減率は
/// 呼び出し側のテストが `support::FIXTURE_TABLE` の実コンテンツから
/// 同じ式で計算する）。
fn run_fake_mcp(mode: McpMode) {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut current_html: Option<String> = None;
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
                match name {
                    "goto" => {
                        let url = value
                            .get("params")
                            .and_then(|p| p.get("arguments"))
                            .and_then(|a| a.get("url"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        // `stale` でなければ毎回 GET し直し、`stale` なら
                        // 最初の 1 回だけ取得して以降は無視する（実際には
                        // 遷移していない対象を模す）。
                        if !mode.stale || current_html.is_none() {
                            current_html = http_get_body(url).ok();
                        }
                        if mode.empty_goto_content {
                            Some(format!(
                                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[],\"isError\":false}}}}"
                            ))
                        } else {
                            Some(tools_call_text_response(id, ""))
                        }
                    }
                    "html" => {
                        let html = current_html.clone().unwrap_or_default();
                        Some(tools_call_text_response(id, &html))
                    }
                    "tree" => {
                        let heading = current_html
                            .as_deref()
                            .and_then(extract_h1)
                            .unwrap_or_default();
                        Some(tools_call_text_response(
                            id,
                            &format!("document\nheading: {heading}"),
                        ))
                    }
                    _ => Some(tools_call_text_response(id, "")),
                }
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

/// `tools/call` の成功応答（`content` に単一のテキスト項目を持つ）を JSON
/// 文字列として組み立てる。`text` は [`support::json_escape`] で JSON
/// 文字列として安全に埋め込む（実際の fixture HTML は `"` を含む属性
/// （`id="intro"` 等）を持つため、素朴な文字列埋め込みは壊れる）。
fn tools_call_text_response(id: f64, text: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}],\"isError\":false}}}}",
        support::json_escape(text)
    )
}

/// `url`（`support::fixture_url` が組み立てた `http://127.0.0.1:<port><path>`
/// 形式）へ実際に HTTP GET し、本文を返す最小限のクライアント。
///
/// このテストバイナリ内から `measure::FixtureServer`（本番の fixture 配信
/// サーバー）へ接続する用途専用であり、同サーバーは常に `Content-Length` を
/// 送る（`measure.rs` の `write_fixture_response` 参照）ため、chunked
/// デコードは実装しない。
fn http_get_body(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .ok_or_else(|| format!("unexpected URL (want http://127.0.0.1:<port><path>): {url}"))?;
    let (port_str, path) = rest
        .split_once('/')
        .ok_or_else(|| format!("URL is missing a path: {url}"))?;
    let port: u16 = port_str
        .parse()
        .map_err(|e| format!("invalid port in URL {url:?}: {e}"))?;
    let path = format!("/{path}");

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("connect failed: {e}"))?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("request write failed: {e}"))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| format!("status line read failed: {e}"))?;
    if !status_line.starts_with("HTTP/1.1 200") {
        return Err(format!("unexpected status line: {status_line:?}"));
    }
    let mut content_length: Option<usize> = None;
    loop {
        let mut header_line = String::new();
        reader
            .read_line(&mut header_line)
            .map_err(|e| format!("header line read failed: {e}"))?;
        if header_line.is_empty() || header_line == "\r\n" || header_line == "\n" {
            break;
        }
        if let Some(value) = header_line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            content_length = value.trim().parse().ok();
        }
    }
    let mut body = Vec::new();
    match content_length {
        Some(len) => {
            body.resize(len, 0);
            reader
                .read_exact(&mut body)
                .map_err(|e| format!("body read failed: {e}"))?;
        }
        None => {
            reader
                .read_to_end(&mut body)
                .map_err(|e| format!("body read failed: {e}"))?;
        }
    }
    String::from_utf8(body).map_err(|e| format!("body is not valid UTF-8: {e}"))
}

/// `html`（fixture の生の HTML）から `<h1>...</h1>` の中身をそのまま
/// 取り出す（このベンチの fixture は `<h1>` に属性・入れ子要素を持たない
/// 単純な見出しのみのため、簡易的な文字列検索で十分。
/// `fixture_markers_appear_as_body_heading_text` が全 fixture でこの前提を
/// 保証する）。
fn extract_h1(html: &str) -> Option<String> {
    let start = html.find("<h1>")? + "<h1>".len();
    let end = html[start..].find("</h1>")?;
    Some(html[start..start + end].to_string())
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

/// fake MCP（非 JSON 版）: `initialize` リクエストを 1 件読み取ったあと、
/// JSON として解析できない行を改行付きで書いて終了する。
///
/// `measure::McpClient` の読み取りスレッドはこの行を `parse_json` で解析
/// できないことを検知し、`line_read_error` へ記録して読み取りスレッドを
/// 終了する（`mpsc::Sender` の drop により、待機中の `recv_timeout` は
/// 即座に `RecvTimeoutError::Disconnected` で返る）。MCP の stdio
/// トランスポート仕様が stdout に応答行以外を書くことを許さないため、
/// 解析できない行を読み飛ばして待ち続けるのではなく即エラーにする契約
/// （`mcp_non_json_stdout_line_fails_immediately` が、20 秒の呼び出し
/// タイムアウトを待たずに即時でエラーになることを検証する）。
fn run_fake_mcp_non_json_line() {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"this line is not JSON at all\n");
    let _ = stdout.flush();
}

/// プロセスグループ終了テスト用の「子」役。自分自身をもう一段再実行して
/// 「孫」（[`run_fake_sleep_grandchild`]）を起動し、その PID を標準出力へ
/// 1 行書いてから孫の終了を待つ（OS シェル（`sh`）に頼らず、テストバイナリ
/// 自身の再実行だけで 3 OS 共通に子・孫プロセスを作る。
/// `kill_process_group_kills_descendant_process_cross_platform` が使う）。
///
/// 孫の標準出力にはこのプロセス自身の標準出力（＝呼び出し元テストへの
/// パイプ）を継承させる（`Stdio::inherit()`）。これにより孫も、このパイプの
/// 書き込み側を保持し続ける。呼び出し元は kill 前後でこのパイプから読み
/// 続け、EOF に達するかどうかで「子・孫の両方が終了したか」を外部コマンド
/// （`kill -0`・`tasklist` 等）を使わずに 3 OS 共通で判定できる
/// （[`process_is_alive`] 参照。外部コマンドの失敗・タイムアウトを
/// 「死んでいる」と誤判定する経路自体をなくす）。
fn run_fake_sleep_parent() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let mut command = std::process::Command::new(&current_exe);
    // 孫プロセスには明示的なプロセスグループ設定をしない。unix はデフォルトで
    // 親（この「子」プロセス。呼び出し元が `apply_new_process_group` で
    // グループリーダーにしている）と同じプロセスグループを継承するため、
    // `kill_process_group`（グループ宛てシグナル）が孫にも届く。Windows は
    // `taskkill /T` が生存中の親子関係を辿るため、同様に明示設定は不要。
    command
        .arg(FAKE_ROLE_FLAG)
        .arg("sleep-grandchild")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::null());
    let mut grandchild = command.spawn().expect("spawn sleep-grandchild");
    println!("{}", grandchild.id());
    let _ = std::io::stdout().flush();
    let _ = grandchild.wait();
}

/// プロセスグループ終了テスト用の「孫」役。生存確認できる程度の時間
/// （テストの期限より十分長い）だけ何もせず眠り続ける。
fn run_fake_sleep_grandchild() {
    std::thread::sleep(Duration::from_secs(60));
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
    eprintln!("case: cdp_chunked_readiness_response_is_measured");
    cdp_chunked_readiness_response_is_measured();
    eprintln!("case: cdp_no_content_length_readiness_response_is_measured");
    cdp_no_content_length_readiness_response_is_measured();
    eprintln!("case: cdp_malformed_chunked_readiness_response_is_error_with_last_error_reason");
    cdp_malformed_chunked_readiness_response_is_error_with_last_error_reason();
    eprintln!("case: mcp_empty_goto_content_still_succeeds");
    mcp_empty_goto_content_still_succeeds();
    eprintln!("case: mcp_non_json_stdout_line_fails_immediately");
    mcp_non_json_stdout_line_fails_immediately();
    eprintln!("case: fixture_server_direct_requests_behave_as_documented");
    fixture_server_direct_requests_behave_as_documented();
    eprintln!("case: read_line_bounded_rejects_over_limit_lines");
    read_line_bounded_rejects_over_limit_lines();
    eprintln!("case: read_line_bounded_rejects_invalid_utf8");
    read_line_bounded_rejects_invalid_utf8();
    eprintln!("case: read_line_bounded_rejects_mid_line_eof");
    read_line_bounded_rejects_mid_line_eof();
    eprintln!("case: kill_process_group_kills_descendant_process_cross_platform");
    kill_process_group_kills_descendant_process_cross_platform();
    eprintln!("case: wait_with_deadline_kills_descendant_process_cross_platform");
    wait_with_deadline_kills_descendant_process_cross_platform();
    eprintln!("case: read_chunked_body_decodes_chunk_extension");
    read_chunked_body_decodes_chunk_extension();
    eprintln!("case: read_chunked_body_skips_trailer_headers");
    read_chunked_body_skips_trailer_headers();
    eprintln!("case: read_chunked_body_rejects_size_exceeding_limit");
    read_chunked_body_rejects_size_exceeding_limit();
    eprintln!("case: read_chunked_body_rejects_missing_terminator_after_chunk_data");
    read_chunked_body_rejects_missing_terminator_after_chunk_data();
    eprintln!("case: read_chunked_body_accepts_bare_lf_terminators");
    read_chunked_body_accepts_bare_lf_terminators();
    eprintln!("case: read_chunked_body_rejects_plus_prefixed_size");
    read_chunked_body_rejects_plus_prefixed_size();
    eprintln!("case: read_chunked_body_accepts_bws_around_extension_separator");
    read_chunked_body_accepts_bws_around_extension_separator();
    eprintln!("case: probe_once_rejects_content_length_and_transfer_encoding_together");
    probe_once_rejects_content_length_and_transfer_encoding_together();
    eprintln!("case: probe_once_rejects_conflicting_content_length_values");
    probe_once_rejects_conflicting_content_length_values();
    eprintln!("competitor_lightpanda_measure: all cases passed");
}

/// AISNAP-1: fake MCP（`run_fake_mcp`）の `tree` 応答と同じ組み立て方
/// （[`extract_h1`] で見出しを取り出し `"document\nheading: <見出し>"` に
/// する）で、`support::FIXTURE_TABLE` の実コンテンツから期待される
/// トークン削減率の中央値を計算する。本番コードと同じ関数
/// （`approx_tokens`・`reduction_pct`・`median`）を使うことで、期待値を
/// マジックナンバーとしてハードコードせず、fixture の内容から具体値として
/// 検証する（coding-rust.md「テスト」: 「期待値は具体値で書く」）。
fn expected_token_reduction_pct_from_fixture_table() -> f64 {
    let mut reductions = Vec::new();
    for (_path, _content_type, content) in support::FIXTURE_TABLE {
        let heading = extract_h1(content).unwrap_or_default();
        let tree_text = format!("document\nheading: {heading}");
        let html_tok = support::approx_tokens(content.chars().count()) as f64;
        let tree_tok = support::approx_tokens(tree_text.chars().count()) as f64;
        let reduction = support::reduction_pct(html_tok, tree_tok)
            .expect("reduction_pct should succeed for a non-zero html token count");
        reductions.push(reduction);
    }
    support::median(&reductions).expect("FIXTURE_TABLE should have at least one fixture")
}

/// fake CDP（`cdp-ok`）・fake MCP（`mcp-ok`）を対象にした 1 件の [`Target`] を
/// 構築する。
fn fake_target(name: &'static str) -> Target {
    let current_exe = std::env::current_exe().expect("current_exe");
    Target {
        name,
        bin: Some(current_exe),
        bin_error: None,
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
/// `PERF-6`（idle RSS。3 OS 共通で `measured`。TASK-84.5）・`AISNAP-1`
/// （トークン削減率）のすべてが `measured` になり、`run_all` の終了コードが
/// `0` になることを、fixture サーバーの起動から対象プロセス（fake CDP・
/// fake MCP。ともにこのテストバイナリ自身の再実行）との通信・計測結果・
/// 終了コードまで通しで検証する。
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
    // 上限は「異常に長い」ことを検知するための緩い妥当性チェック
    // （厳密な性能目標ではない。ローカルの自己再実行プロセスなので、
    // 通常は数十〜数百 ms で readiness に達する）。
    assert!(
        cold_start_value < 10_000.0,
        "coldStartMs should be well under the 10s readiness deadline for a local self-exec process (PERF-3): {cold_start_value}"
    );

    let idle_rss = report.get("idleRssKb").expect("idleRssKb field");
    let idle_rss_status = idle_rss.get("status").and_then(JsonValue::as_str);
    assert_eq!(
        idle_rss_status,
        Some("measured"),
        "idleRssKb should be measured on all 3 OSes (PERF-6): {body}"
    );
    let idle_rss_value = idle_rss
        .get("value")
        .and_then(JsonValue::as_f64)
        .expect("idleRssKb value");
    assert!(
        idle_rss_value > 0.0,
        "idleRssKb should be a positive KB value: {idle_rss_value}"
    );
    // 上限は 1GiB 相当（1024 * 1024 KB）。テストバイナリ自身の
    // アイドル RSS がこれを超えることは通常無く、`ps`/`tasklist` の出力誤読等の
    // 明らかな異常値を検知するための緩い妥当性チェック（PERF-6）。
    assert!(
        idle_rss_value < 1024.0 * 1024.0,
        "idleRssKb should be well under 1GiB for a local self-exec process (PERF-6): {idle_rss_value}"
    );

    // `tree` は fake MCP が実際に HTTP GET した fixture の `<h1>` 見出しを
    // 含む簡約テキストにする（`run_fake_mcp` 参照）ため、期待する削減率は
    // `support::FIXTURE_TABLE` の実コンテンツから同じ式で計算する
    // （AISNAP-1。マジックナンバーをハードコードしない）。
    let expected_token_reduction = expected_token_reduction_pct_from_fixture_table();
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
        Some(expected_token_reduction),
        "tokenReductionPct should match the value computed from FIXTURE_TABLE's actual content (AISNAP-1): {body}"
    );

    // `sitesCount` は `tokenReductionPct` の対象として構成されている
    // fixture の件数（`support::FIXTURE_TABLE`。計測の成否とは独立）。
    // PoC-13（5 類型）と比較しないための注記を結果からも判別できるように
    // する（モジュールドキュメント参照）。
    assert_eq!(
        report.get("sitesCount").and_then(JsonValue::as_f64),
        Some(3.0),
        "sitesCount should equal the number of fixtures in FIXTURE_TABLE: {body}"
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
/// の readiness 期限 10 秒×trials）だけを検証する。
fn cold_start_invalid_readiness_response_is_error_with_exit_code_one() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-invalid",
        bin: Some(current_exe),
        bin_error: None,
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

/// ケース 3（AISNAP-1）: fake MCP（`mcp-disconnect`）が改行を送らないまま
/// 標準出力を閉じる場合、`initialize` 呼び出し（20 秒のタイムアウト）が
/// 即時に `Outcome::Error` になり、20 秒を待たないことを経過時間で確認する。
fn mcp_disconnect_without_newline_fails_immediately() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-disconnect",
        bin: Some(current_exe),
        bin_error: None,
        serve_args: Vec::new(),
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-disconnect".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    };

    // `initialize` 呼び出しの時点で fake MCP が切断するため、fixture
    // サーバーへ実際に接続するところまでは進まないが、本物のサーバーを
    // 使い、テスト用のダミーポートに頼らない構成にする。
    let fixture_server = measure::FixtureServer::start().expect("fixture server should start");

    let start = Instant::now();
    let outcome = measure_token_reduction(&target, fixture_server.port);
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

/// ケース 4（PERF-1/PERF-3/PERF-6/AISNAP-1）: 対象バイナリが未設定
/// （`bin: None`）の場合、4 項目すべてが `skipped` になり、終了コードが
/// `0` になることを確認する。
fn unconfigured_target_is_skipped_with_exit_code_zero() {
    let target = Target {
        name: "unconfigured",
        bin: None,
        bin_error: None,
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

/// ケース 5（AISNAP-1）: fake MCP（`mcp-stale`）が `goto` を受けてもマーカーを
/// 更新せず、
/// 常に 1 件目のページ（`/article.html`）の内容を返し続ける場合、`goto` の
/// JSON-RPC 応答自体は形式上妥当でも、2 件目以降の fixture
/// （`/listing.html`・`/form.html`）については `html`/`tree` にその
/// fixture 自身のマーカーが含まれないため、内容ベースの検証
/// によって計測失敗（`Outcome::Error`）になることを確認する。
fn stale_mcp_response_after_second_goto_is_error() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-stale",
        bin: Some(current_exe),
        bin_error: None,
        serve_args: Vec::new(),
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-stale".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    };

    // fake MCP（`mcp-stale`）は 1 件目の `goto` で実際に fixture サーバーへ
    // HTTP GET するため、本物のサーバーを起動して使う。
    let fixture_server = measure::FixtureServer::start().expect("fixture server should start");

    let outcome = measure_token_reduction(&target, fixture_server.port);
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

/// `Target` の共通フィールドを埋めた雛形を作る（fake CDP 系のテストが
/// `serve_args` だけを変えて使う）。
fn fake_cdp_only_target(name: &'static str, cdp_role: &str) -> Target {
    let current_exe = std::env::current_exe().expect("current_exe");
    Target {
        name,
        bin: Some(current_exe),
        bin_error: None,
        serve_args: vec![
            FAKE_ROLE_FLAG.to_string(),
            cdp_role.to_string(),
            "--port".to_string(),
            "{port}".to_string(),
        ],
        mcp_args: Vec::new(),
        serve_args_error: None,
        mcp_args_error: None,
    }
}

/// PERF-3: readiness probe が `Transfer-Encoding: chunked` の応答を正しく
/// デコードし、cold start を計測できることを確認する
/// （`measure::probe_once`/`read_chunked_body` の成功経路）。
fn cdp_chunked_readiness_response_is_measured() {
    let target = fake_cdp_only_target("fake-browser-chunked", "cdp-chunked");
    match measure_cold_start(&target, 1) {
        Outcome::Value(v) => assert!(v > 0.0, "cold start should be a positive duration: {v}"),
        other => panic!(
            "expected Outcome::Value for a chunked readiness response (PERF-3), got {other:?}"
        ),
    }
}

/// PERF-3: readiness probe が `Content-Length` も `Transfer-Encoding` も無い
/// 応答を EOF まで読んで解釈できることを確認する（`probe_once` の EOF
/// フォールバック経路）。
fn cdp_no_content_length_readiness_response_is_measured() {
    let target = fake_cdp_only_target("fake-browser-no-cl", "cdp-no-cl");
    match measure_cold_start(&target, 1) {
        Outcome::Value(v) => assert!(v > 0.0, "cold start should be a positive duration: {v}"),
        other => panic!(
            "expected Outcome::Value for a Content-Length-less readiness response (PERF-3), got {other:?}"
        ),
    }
}

/// PERF-3: readiness probe が不正な `Transfer-Encoding: chunked` 応答
/// （16 進数として不正なチャンクサイズ行）を送られ続けると、readiness に
/// 到達できず 10 秒のタイムアウトで `Outcome::Error` になり、その理由に
/// 最後の probe 失敗（チャンクサイズが不正である旨）が残ることを確認する
/// （`spawn_and_wait_ready` の「最後の失敗理由を保持する」契約）。
fn cdp_malformed_chunked_readiness_response_is_error_with_last_error_reason() {
    let target = fake_cdp_only_target("fake-browser-malformed-chunked", "cdp-malformed-chunked");
    match measure_cold_start(&target, 1) {
        Outcome::Error(reason) => {
            assert!(
                reason.contains("invalid chunk size"),
                "error reason should retain the last probe failure (invalid chunk size) (PERF-3): {reason}"
            );
        }
        other => panic!(
            "expected Outcome::Error for a malformed chunked readiness response (PERF-3), got {other:?}"
        ),
    }
}

/// AISNAP-1: fake MCP（`mcp-empty-goto`）が `goto` の `tools/call` 応答で
/// `result.content` を空配列にしても（MCP 2025-06-18 は許容する）、続く
/// `html`/`tree` が正しい内容を返せば計測が成功することを確認する
/// （`validate_mcp_result_shape` の非空要求撤廃の回帰防止）。
fn mcp_empty_goto_content_still_succeeds() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-empty-goto",
        bin: Some(current_exe),
        bin_error: None,
        serve_args: Vec::new(),
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-empty-goto".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    };
    let fixture_server = measure::FixtureServer::start().expect("fixture server should start");
    let expected = expected_token_reduction_pct_from_fixture_table();

    match measure_token_reduction(&target, fixture_server.port) {
        Outcome::Value(v) => assert_eq!(
            v, expected,
            "tokenReductionPct should match FIXTURE_TABLE's content even when goto's content array is empty (AISNAP-1)"
        ),
        other => panic!(
            "expected Outcome::Value when goto returns an empty content array but html/tree succeed (AISNAP-1), got {other:?}"
        ),
    }
}

/// AISNAP-1: fake MCP（`mcp-non-json`）が stdout に JSON として解析できない
/// 行を書いた場合、`initialize` 呼び出し（20 秒のタイムアウト）が即時に
/// `Outcome::Error` になり、20 秒を待たないことを経過時間で確認する
/// （MCP stdout の非 JSON 行を読み飛ばさず即エラーにする契約）。
fn mcp_non_json_stdout_line_fails_immediately() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let target = Target {
        name: "fake-browser-non-json",
        bin: Some(current_exe),
        bin_error: None,
        serve_args: Vec::new(),
        mcp_args: vec![FAKE_ROLE_FLAG.to_string(), "mcp-non-json".to_string()],
        serve_args_error: None,
        mcp_args_error: None,
    };
    let fixture_server = measure::FixtureServer::start().expect("fixture server should start");

    let start = Instant::now();
    let outcome = measure_token_reduction(&target, fixture_server.port);
    let elapsed = start.elapsed();

    match outcome {
        Outcome::Error(reason) => {
            assert!(
                reason.contains("not valid JSON"),
                "error reason should mention the non-JSON stdout line: {reason}"
            );
        }
        other => panic!(
            "expected Outcome::Error when the fake MCP writes a non-JSON stdout line, got {other:?}"
        ),
    }
    assert!(
        elapsed < Duration::from_secs(10),
        "a non-JSON stdout line should fail immediately, not wait for the 20s call timeout: {elapsed:?}"
    );
}

/// `request` をそのまま TCP で送り、応答のステータス行を読んで返す
/// （fixture サーバーへの直接リクエストを送るテストが使う最小限の
/// クライアント。応答が無い・接続が切れる等は `None` にする）。
fn send_raw_request_and_read_status(
    port: u16,
    request: &[u8],
    read_timeout: Duration,
) -> Option<u16> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(read_timeout)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    if !request.is_empty() {
        stream.write_all(request).ok()?;
    }
    let mut reader = std::io::BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).ok()?;
    support::parse_http_status(&status_line)
}

/// PERF-1/PERF-3/PERF-6/AISNAP-1 が使う fixture 配信サーバー
/// （`measure::FixtureServer`）へ直接リクエストを送り、400/404/405
/// （HEAD・POST）/408・ヘッダ行数上限の応答契約を確認する（結合テスト。
/// 対象バイナリを介さず、サーバー自身の HTTP 実装を検証する）。
fn fixture_server_direct_requests_behave_as_documented() {
    let server = measure::FixtureServer::start().expect("fixture server should start");
    let port = server.port;
    let short_timeout = Duration::from_secs(5);

    // 200: 既知のパスへの GET は成功する（他のケースの前提が壊れていない
    // ことの確認）。
    assert_eq!(
        send_raw_request_and_read_status(
            port,
            b"GET /article.html HTTP/1.1\r\nHost: x\r\n\r\n",
            short_timeout
        ),
        Some(200),
        "GET to a known fixture path should succeed"
    );

    // 400: HTTP バージョントークンを欠いた不正なリクエスト行。
    assert_eq!(
        send_raw_request_and_read_status(port, b"GET /article.html\r\n\r\n", short_timeout),
        Some(400),
        "a malformed request line should be rejected with 400"
    );

    // 404: 未知のパス。
    assert_eq!(
        send_raw_request_and_read_status(
            port,
            b"GET /does-not-exist.html HTTP/1.1\r\nHost: x\r\n\r\n",
            short_timeout
        ),
        Some(404),
        "an unknown path should be rejected with 404"
    );

    // 405: HEAD・POST はいずれも拒否する（HEAD 対応は削除済み。GET のみ
    // 受理する）。
    assert_eq!(
        send_raw_request_and_read_status(
            port,
            b"HEAD /article.html HTTP/1.1\r\nHost: x\r\n\r\n",
            short_timeout
        ),
        Some(405),
        "HEAD should be rejected with 405 (HEAD support was removed)"
    );
    assert_eq!(
        send_raw_request_and_read_status(
            port,
            b"POST /article.html HTTP/1.1\r\nHost: x\r\n\r\n",
            short_timeout
        ),
        Some(405),
        "POST should be rejected with 405"
    );

    // 408: リクエスト行を一切送らないまま接続だけ確立する。サーバー側の
    // 接続全体の絶対期限（`FIXTURE_IO_TIMEOUT`）超過で 408 になる
    // （クライアント側の読み取り期限はそれより十分長く取る）。
    assert_eq!(
        send_raw_request_and_read_status(port, b"", Duration::from_secs(15)),
        Some(408),
        "a connection that never sends a request line should be rejected with 408"
    );

    // ヘッダ行数の上限（`FIXTURE_MAX_HEADER_LINES`）超過は 400。
    let mut over_limit_request = String::from("GET /article.html HTTP/1.1\r\n");
    for i in 0..=(measure::FIXTURE_MAX_HEADER_LINES as usize) {
        over_limit_request.push_str(&format!("X-Filler-{i}: 1\r\n"));
    }
    over_limit_request.push_str("\r\n");
    assert_eq!(
        send_raw_request_and_read_status(port, over_limit_request.as_bytes(), short_timeout),
        Some(400),
        "exceeding the header line limit should be rejected with 400"
    );

    // slow-loris: リクエスト行を 1 バイトずつ、サーバーの接続期限を超える
    // 間隔で送り続けても、テスト自体がハングせず応答が返る（または接続が
    // 切れる）ことを確認する。
    slow_loris_request_does_not_hang(port);
}

/// PERF-3/AISNAP-1: リクエスト行を 1 バイトずつ間隔を空けて送り続け
/// （slow-loris）、fixture サーバーの接続全体の絶対期限
/// （`DeadlineReader`）で確実に打ち切られる（テスト自体が無期限に
/// ブロックしない）ことを確認する。
fn slow_loris_request_does_not_hang(port: u16) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    // クライアント側の読み取り期限は、サーバーの接続絶対期限
    // （`FIXTURE_IO_TIMEOUT`）を大きく超えて余裕を持たせる（応答を待つ側が
    // 先にタイムアウトして「ハングしなかった」ことを誤検知しないため）。
    let client_read_timeout = measure::FIXTURE_IO_TIMEOUT + Duration::from_secs(10);
    stream
        .set_read_timeout(Some(client_read_timeout))
        .expect("set_read_timeout");
    let bytes = b"GET /article.html HTTP/1.1\r\n\r\n";
    // 1 バイトずつの合計送信時間が `FIXTURE_IO_TIMEOUT` を確実に超える間隔
    // にする（送り切れてしまうと、サーバー側の接続絶対期限を検証した
    // ことにならない）。
    let interval = (measure::FIXTURE_IO_TIMEOUT / bytes.len() as u32) + Duration::from_millis(50);
    let start = Instant::now();
    let mut write_failed = false;
    for &b in bytes {
        // 書き込み失敗（サーバーが期限で接続を閉じた）はここで検知でき、
        // その時点でループを終える。
        if stream.write_all(&[b]).is_err() {
            write_failed = true;
            break;
        }
        std::thread::sleep(interval);
    }
    let mut reader = std::io::BufReader::new(stream);
    let mut status_line = String::new();
    let read_result = reader.read_line(&mut status_line);
    let elapsed = start.elapsed();

    // 「ハングしなかった」だけでなく、実際にサーバー側の接続絶対期限で
    // 打ち切られたことまで確認する。期待する結果は次のいずれか:
    // (1) 送信完了前に書き込みが失敗した（サーバーが先に接続を閉じた）、
    // (2) 応答が読めた場合は、それが `408 Request Timeout` であること
    //     （送信を完遂できてしまった場合でも、期限切れとして扱われている
    //     ことを status line で確認する）、
    // (3) 応答本体を読む前に接続が閉じられた（`Ok(0)`）。
    match read_result {
        Ok(0) => {
            // 接続が閉じられた（応答なし、または応答済みで close された）。
        }
        Ok(_) => {
            assert_eq!(
                support::parse_http_status(&status_line),
                Some(408),
                "a slow-loris request should be rejected with 408 once the server's deadline elapses, got status line {status_line:?}"
            );
        }
        Err(e) => {
            assert!(
                write_failed || e.kind() != std::io::ErrorKind::TimedOut,
                "the client should not need to hit its own read timeout; the server should have closed the connection well before that: {e}"
            );
        }
    }

    // 経過時間が `FIXTURE_IO_TIMEOUT` に妥当な猶予（`DeadlineReader::
    // extend_deadline` のエラー応答用猶予・スケジューリング遅延分）を
    // 足した範囲に収まることを確認する（サーバーが期限を大幅に超えて
    // 接続を保持し続けていないこと）。
    let max_elapsed = measure::FIXTURE_IO_TIMEOUT + Duration::from_secs(5);
    assert!(
        elapsed < max_elapsed,
        "a slow-loris connection should be cut off around the server's absolute connection deadline ({:?}), took {elapsed:?}",
        measure::FIXTURE_IO_TIMEOUT
    );
}

/// PERF-3: `Transfer-Encoding: chunked` のチャンク拡張（`;` 以降）を無視して
/// 本文を正しく復元することを確認する（RFC 9112 §7.1.1）。
fn read_chunked_body_decodes_chunk_extension() {
    let input = b"5;ext=ignored\r\nhello\r\n0\r\n\r\n";
    let mut reader = std::io::BufReader::new(&input[..]);
    let body = measure::read_chunked_body(&mut reader, 1024).expect("should decode");
    assert_eq!(body, b"hello");
}

/// PERF-3: 最終チャンク（サイズ 0）の後のトレーラー部を空行まで読み飛ばし、
/// 本文には影響しないことを確認する。
fn read_chunked_body_skips_trailer_headers() {
    let input = b"5\r\nhello\r\n0\r\nX-Trailer: value\r\n\r\n";
    let mut reader = std::io::BufReader::new(&input[..]);
    let body = measure::read_chunked_body(&mut reader, 1024).expect("should decode");
    assert_eq!(body, b"hello");
}

/// PERF-3: 累積本文サイズが上限を超えるチャンクは、読み取りを試みず失敗に
/// する（黙って上限まで切り詰めて成功として扱わない）。
fn read_chunked_body_rejects_size_exceeding_limit() {
    let input = b"a\r\n0123456789\r\n0\r\n\r\n"; // 10 バイトのチャンク。
    let mut reader = std::io::BufReader::new(&input[..]);
    let result = measure::read_chunked_body(&mut reader, 5);
    match result {
        Err(e) => assert!(
            e.contains("exceeds"),
            "error should mention the size limit: {e}"
        ),
        Ok(body) => panic!("expected an error for a chunk exceeding the limit, got {body:?}"),
    }
}

/// PERF-3: チャンクデータ直後に CRLF/LF が続かない場合は失敗にする（部分的に
/// 読めたデータを成功として使わない）。
fn read_chunked_body_rejects_missing_terminator_after_chunk_data() {
    let input = b"5\r\nhelloXX0\r\n\r\n";
    let mut reader = std::io::BufReader::new(&input[..]);
    let result = measure::read_chunked_body(&mut reader, 1024);
    match result {
        Err(e) => assert!(
            e.contains("not terminated"),
            "error should mention the missing terminator: {e}"
        ),
        Ok(body) => panic!("expected an error for a missing chunk terminator, got {body:?}"),
    }
}

/// PERF-3: チャンクサイズ行の裸の LF 終端（CRLF ではなく LF のみ）を
/// 受理することを確認する（`read_complete_line` と統一した寛容さ）。
fn read_chunked_body_accepts_bare_lf_terminators() {
    let input = b"5\nhello\n0\n\n";
    let mut reader = std::io::BufReader::new(&input[..]);
    let body = measure::read_chunked_body(&mut reader, 1024).expect("should decode with bare LF");
    assert_eq!(body, b"hello");
}

/// PERF-3: `+` 接頭辞付きのチャンクサイズ（16 進数字以外を含む）を拒否する
/// （`usize::from_str_radix` が符号を許容してしまう経路を明示的な文字種
/// チェックで塞ぐ）。
fn read_chunked_body_rejects_plus_prefixed_size() {
    let input = b"+1a\r\nx";
    let mut reader = std::io::BufReader::new(&input[..]);
    let result = measure::read_chunked_body(&mut reader, 1024);
    match result {
        Err(e) => assert!(
            e.contains("invalid chunk size"),
            "error should mention the invalid chunk size: {e}"
        ),
        Ok(body) => panic!("expected an error for a '+'-prefixed chunk size, got {body:?}"),
    }
}

/// PERF-3: チャンクサイズ行の BWS（オプションの空白。例:
/// `5 ;ext=1`）を許容し、拡張の前後の空白がサイズ解釈を壊さないことを
/// 確認する。
fn read_chunked_body_accepts_bws_around_extension_separator() {
    let input = b"5 ;ext=1\r\nhello\r\n0\r\n\r\n";
    let mut reader = std::io::BufReader::new(&input[..]);
    let body = measure::read_chunked_body(&mut reader, 1024).expect("should decode with BWS");
    assert_eq!(body, b"hello");
}

/// 1 回だけ接続を受け付け、リクエストを読み捨てて `response` をそのまま
/// 書き込んで閉じる使い捨てのサーバーを起動する（`probe_once` の
/// `Content-Length`/`Transfer-Encoding` の境界ケースを直接検証するために
/// 使う）。
fn spawn_one_shot_response_server(response: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut discard = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut discard);
            let _ = stream.write_all(response);
            let _ = stream.flush();
        }
    });
    port
}

/// PERF-3: `Content-Length` と `Transfer-Encoding: chunked` が両方存在する
/// 応答は、本文の実際の境界をどちらの基準で解釈すべきか判断できないため
/// probe 失敗にする（RFC 9112 §6.1 の勧告どおり無視して進めるのではなく、
/// このベンチでは fail-closed にする）。
fn probe_once_rejects_content_length_and_transfer_encoding_together() {
    let response: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let port = spawn_one_shot_response_server(response);
    let result = measure::probe_once(
        port,
        "GET /json/version HTTP/1.1\r\n\r\n",
        Instant::now() + Duration::from_secs(5),
    );
    match result {
        Err(e) => assert!(
            e.contains("both Content-Length and Transfer-Encoding"),
            "error should mention the conflicting headers: {e}"
        ),
        Ok((status, body)) => panic!(
            "expected an error for Content-Length + Transfer-Encoding together, got status={status} body={body:?}"
        ),
    }
}

/// PERF-3: 同じ `Content-Length` ヘッダーが複数回現れて値が食い違う場合、
/// どちらの値を信じるべきか判断できないため probe 失敗にする。
fn probe_once_rejects_conflicting_content_length_values() {
    let response: &'static [u8] =
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!";
    let port = spawn_one_shot_response_server(response);
    let result = measure::probe_once(
        port,
        "GET /json/version HTTP/1.1\r\n\r\n",
        Instant::now() + Duration::from_secs(5),
    );
    match result {
        Err(e) => assert!(
            e.contains("conflicting Content-Length"),
            "error should mention the conflicting Content-Length values: {e}"
        ),
        Ok((status, body)) => panic!(
            "expected an error for conflicting Content-Length values, got status={status} body={body:?}"
        ),
    }
}

/// ループバック TCP 接続の両端（サーバー側・クライアント側）を返す
/// （`read_line_bounded` の単体テストが実ソケット越しに検証するために使う）。
fn loopback_pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    let client = std::net::TcpStream::connect(addr).expect("connect loopback client");
    let (server, _) = listener.accept().expect("accept loopback connection");
    (server, client)
}

/// PERF-3/AISNAP-1: `read_line_bounded` は `MAX_LINE_BYTES` を超えて改行に
/// 達しない行を打ち切りエラーにする（`fixture_accept_loop`・`probe_once`・
/// MCP stdout 読み取りのすべての呼び出し元が依存する契約）。
fn read_line_bounded_rejects_over_limit_lines() {
    let (server, mut client) = loopback_pair();
    let sender = std::thread::spawn(move || {
        let chunk = vec![b'a'; measure::MAX_LINE_BYTES + 1];
        let _ = client.write_all(&chunk);
        let _ = client.flush();
        drop(client);
    });
    let mut reader = std::io::BufReader::new(server);
    let mut out = String::new();
    let result = measure::read_line_bounded(&mut reader, &mut out);
    match &result {
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::InvalidData,
            "expected InvalidData for a line exceeding MAX_LINE_BYTES, got {result:?}"
        ),
        Ok(_) => panic!("expected an error for a line exceeding MAX_LINE_BYTES, got {result:?}"),
    }
    let _ = sender.join();
}

/// PERF-3/AISNAP-1: `read_line_bounded` は不正な UTF-8 バイト列を含む行を
/// （`from_utf8_lossy` で置換せず）エラーにする。
fn read_line_bounded_rejects_invalid_utf8() {
    let (server, mut client) = loopback_pair();
    let sender = std::thread::spawn(move || {
        let _ = client.write_all(&[0xff, 0xfe, b'\n']);
        let _ = client.flush();
        drop(client);
    });
    let mut reader = std::io::BufReader::new(server);
    let mut out = String::new();
    let result = measure::read_line_bounded(&mut reader, &mut out);
    match &result {
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::InvalidData,
            "expected InvalidData for invalid UTF-8 bytes, got {result:?}"
        ),
        Ok(_) => panic!("expected an error for invalid UTF-8 bytes, got {result:?}"),
    }
    let _ = sender.join();
}

/// PERF-3/AISNAP-1: `read_line_bounded` は、1 バイト以上読んだ後に改行へ
/// 達する前に接続が閉じた（行が途中で切れた）場合、
/// `ErrorKind::UnexpectedEof` にする（改行なしの断片を「1 行読めた」として
/// 扱わない）。
fn read_line_bounded_rejects_mid_line_eof() {
    let (server, mut client) = loopback_pair();
    let sender = std::thread::spawn(move || {
        let _ = client.write_all(b"partial line without a trailing newline");
        let _ = client.flush();
        drop(client);
    });
    let mut reader = std::io::BufReader::new(server);
    let mut out = String::new();
    let result = measure::read_line_bounded(&mut reader, &mut out);
    match &result {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        other => panic!(
            "expected ErrorKind::UnexpectedEof for a line cut off before a newline, got {other:?}"
        ),
    }
    let _ = sender.join();
}

/// [`spawn_pipe_reader`] がバックグラウンドスレッドから送るイベント。
#[derive(Debug)]
enum PipeEvent {
    /// 最初の行（`run_fake_sleep_parent` が書く孫の PID）。
    Line(String),
    /// パイプの書き込み側が全て閉じた（＝子・孫の両方が終了した）。
    Eof,
}

/// 子プロセスの標準出力パイプを読み、最初の 1 行を [`PipeEvent::Line`] で
/// 送ったあとは、以後の出力を読み捨てながら EOF を監視して
/// [`PipeEvent::Eof`] を送るバックグラウンドスレッドを起動する。
///
/// `kill_process_group_kills_descendant_process_cross_platform` が、外部
/// コマンド（`kill -0`・`tasklist` 等）に頼らず 3 OS 共通で「子・孫の両方が
/// 終了したか」を判定するために使う（[`run_fake_sleep_parent`] のドキュメント
/// 参照。孫にもこのパイプの書き込み側を継承させているため、子だけが終了
/// しても孫が生きていれば EOF に達しない）。
fn spawn_pipe_reader(stdout: std::process::ChildStdout) -> mpsc::Receiver<PipeEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(PipeEvent::Eof);
                return;
            }
            Ok(_) => {
                let _ = tx.send(PipeEvent::Line(line.trim().to_string()));
            }
        }
        // 孫の PID 行を読んだ後、`run_fake_sleep_parent`/
        // `run_fake_sleep_grandchild` はこのパイプへそれ以上書き込まない
        // 前提のため、以後は EOF（子・孫の両方が終了し、書き込み側の
        // 複製が全て閉じたこと）だけを監視する。
        let mut buf = [0u8; 64];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(PipeEvent::Eof);
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    let _ = tx.send(PipeEvent::Eof);
                    break;
                }
            }
        }
    });
    rx
}

/// `rx` から `wait` の間に届いたイベントを見て、パイプが EOF に達した
/// （＝死んだ）とみなせるかを判定する。読み取りスレッドの終了に伴う
/// 送信側の drop（`Disconnected`）も EOF と同義に扱う。タイムアウト
/// （まだ何も届いていない）は「まだ生きている」ことを意味し、`false`
/// にする（外部コマンドの失敗・タイムアウトを安易に「死んでいる」と
/// 判定しない契約）。
fn pipe_indicates_dead(rx: &mpsc::Receiver<PipeEvent>, wait: Duration) -> bool {
    matches!(
        rx.recv_timeout(wait),
        Ok(PipeEvent::Eof) | Err(mpsc::RecvTimeoutError::Disconnected)
    )
}

/// PERF-3/PERF-6: `kill_process_group` が対象プロセスの子だけでなく孫も
/// 終了させることを、テストバイナリ自身の再実行によって 3 OS 共通の方法で
/// 確認する（OS シェルの構文差異に依存しない。`cfg(unix)` に限定しない）。
///
/// 生死判定は外部コマンド（`kill -0`・`tasklist` 等）に頼らない。孫にも
/// 子の標準出力パイプの書き込み側を継承させ（[`run_fake_sleep_parent`]
/// 参照）、そのパイプが EOF に達するかどうかで判定する
/// （[`spawn_pipe_reader`]・[`pipe_indicates_dead`]）。外部コマンドの
/// spawn 失敗・タイムアウトを「死んでいる」と誤判定する経路が構造的に
/// 無くなる。
fn kill_process_group_kills_descendant_process_cross_platform() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let mut command = std::process::Command::new(&current_exe);
    support::apply_new_process_group(&mut command);
    command
        .arg(FAKE_ROLE_FLAG)
        .arg("sleep-parent")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().expect("spawn sleep-parent");
    let child_pid = child.id();

    let stdout = child.stdout.take().expect("child stdout");
    let rx = spawn_pipe_reader(stdout);
    let grandchild_pid: u32 = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(PipeEvent::Line(pid_str)) => pid_str.parse().expect("parse grandchild pid"),
        other => panic!("expected the grandchild pid line, got a different event: {other:?}"),
    };
    let _ = grandchild_pid; // ログ・デバッグ用途以外では未使用。

    assert!(
        !pipe_indicates_dead(&rx, Duration::from_millis(300)),
        "the pipe should not be at EOF yet; the child and grandchild should both be alive before kill_process_group"
    );

    let kill_result = support::kill_process_group(child_pid);
    let _ = child.wait();
    assert!(
        kill_result.is_ok(),
        "kill_process_group should succeed for a process group we just spawned: {kill_result:?}"
    );

    // シグナル配送／プロセスツリー終了から実際の終了までの短い遅延を許容する。
    let mut dead = false;
    for _ in 0..50 {
        if pipe_indicates_dead(&rx, Duration::from_millis(100)) {
            dead = true;
            break;
        }
    }
    assert!(
        dead,
        "the pipe should reach EOF after kill_process_group, meaning both the child and the grandchild have exited (not just the direct child)"
    );
}

/// PERF-3/PERF-6: `wait_with_deadline` が期限を大幅に超えて生存し続ける
/// 子プロセスを kill することを、OS シェルに依存しないテストバイナリ自身の
/// 再実行で確認する（`cfg(unix)` に限定しない。既存の `support.rs` の
/// `wait_with_deadline_kills_process_that_outlives_the_deadline` は `sh` を
/// 使う unix 限定テストであり、これはその 3 OS 版）。
fn wait_with_deadline_kills_descendant_process_cross_platform() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let mut command = std::process::Command::new(&current_exe);
    command
        .arg(FAKE_ROLE_FLAG)
        .arg("sleep-grandchild")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().expect("spawn sleep-grandchild");

    let start = Instant::now();
    let result = support::wait_with_deadline(&mut child, Duration::from_millis(300));
    let elapsed = start.elapsed();

    match &result {
        Err(e) => assert!(
            e.contains("did not exit"),
            "expected a message mentioning the process did not exit in time, got {result:?}"
        ),
        Ok(status) => panic!(
            "expected a timeout error for a process sleeping much longer than the deadline, got Ok({status:?})"
        ),
    }
    assert!(
        elapsed < Duration::from_secs(10),
        "wait_with_deadline should return soon after the deadline, took {elapsed:?}"
    );
}
