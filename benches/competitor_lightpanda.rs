//! Lightpanda 比較ベンチスクリプト本体。
//!
//! TASK-84（84.1）・Issue #211。`docs/spec/03-poc/browser-landscape-2026`
//! （PoC-13）の Node.js 計測スクリプト（`measure-lp.mjs`・`mcp_snapshot.mjs`）
//! を移植し、Lightpanda と fandhe-browser（`fandhe-browser-cli` 未実装のため
//! 現状は計測対象なしで skip する）を対象に、バイナリサイズ（`PERF-1`）・
//! cold start（`PERF-3`）・アイドル RSS（`PERF-6`。Windows は未対応）・
//! MCP レスポンスのトークン削減率（`AISNAP-1`）を計測する。
//!
//! `cargo bench --bench competitor_lightpanda`（暫定ホストは
//! `fandhe-browser-core`。crate 側 `Cargo.toml` のコメント参照）で実行する。
//! 対象バイナリの有無は環境変数で与え、未設定の対象は panic させず skip し
//! 理由を stderr へ出す（coding-rust.md「外部入力の経路では unwrap を使わず
//! 明示的に処理する」）。結果は JSON として stdout へ出す（プログラム出力は
//! 英語。japanese-style.md）。
//!
//! 環境変数（対象ごとに `LIGHTPANDA_` / `FANDHE_BROWSER_` 接頭辞）:
//! - `<PREFIX>_BIN`: 実行ファイルパス（未設定なら該当対象を丸ごと skip）
//! - `<PREFIX>_SERVE_ARGS`: cold start / RSS 計測が起動する引数テンプレート
//!   （空白区切り。`{port}` プレースホルダを実ポートへ展開する。未設定時の
//!   既定値は Lightpanda の `serve --port {port} --log-level error` 相当）
//! - `<PREFIX>_MCP_ARGS`: MCP トークン計測が起動する引数テンプレート
//!   （未設定時の既定値は `mcp`）
//! - `COMPETITOR_BENCH_TRIALS`: cold start / RSS の試行回数（既定 5・上限 20）
//!
//! 新規外部依存は追加しない（`std` のみで完結。dependency-policy.md）。

#[path = "competitor_lightpanda/support.rs"]
mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use support::{
    JsonValue, approx_tokens, expand_args, json_escape, median, parse_http_status, parse_json,
    reduction_pct, split_args,
};
// `parse_ps_rss_kb` は unix 専用の `sample_rss_kb`（下記 `#[cfg(unix)]`）が
// 呼ぶ。無条件 import のままだと Windows ビルドで未使用になり `unused_imports`
// 警告が `-D warnings`（ci.md「3 OS CI」）で fail するため cfg で分離する。
#[cfg(unix)]
use support::parse_ps_rss_kb;

/// 試行回数の既定値・上限（無制限ループ・無制限プロセス起動を避ける。
/// coding-rust.md「長さ・件数を上限検証」）。
const DEFAULT_TRIALS: usize = 5;
const MAX_TRIALS: usize = 20;

/// `COMPETITOR_BENCH_SITES`（外部入力）に許す上限。分割・確保の前に検証し、
/// 巨大な値や大量 URL によるメモリ・実行時間の無制限消費を防ぐ
/// （coding-rust.md「長さ・件数を上限検証してからアロケーションに使う」）。
const MAX_SITES_ENV_BYTES: usize = 8 * 1024;
const MAX_SITES: usize = 20;

/// `COMPETITOR_BENCH_SITES` 未設定時の既定サイト群。PoC-13
/// （`docs/spec/03-poc/browser-landscape-2026/scripts/mcp_snapshot.mjs`）の
/// `sites` と同じ 5 サイトにし、既定実行時の `tokenReductionPct` が
/// 代表サイト群の中央値になるようにする（AISNAP-1）。
const DEFAULT_SITES: [&str; 5] = [
    "https://example.com",
    "https://en.wikipedia.org/wiki/Rust_(programming_language)",
    "https://news.ycombinator.com",
    "https://the-internet.herokuapp.com/login",
    "https://react.dev",
];

/// readiness probe・MCP 呼び出しそれぞれの外部入力（応答行）に許す最大長。
/// 相手プロセスが不正・悪意ある出力を送り続けても無制限にバッファへ
/// 蓄積しないための上限（coding-rust.md）。
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 計測 1 件の結果。成功しなかった場合も理由を残し「実装済みを装わない」
/// （coding-rust.md「公開 API」・REPAIR-4 相当の方針をベンチ出力にも適用）。
enum Outcome {
    Value(f64),
    Skipped(String),
    // Windows（`cfg(windows)` 版の `measure_idle_rss`）でのみ構築する
    // バリアント。Linux/macOS ネイティブビルドではこのバリアントを構築する
    // コード経路が存在しないため dead_code 警告が出るが、3 OS CI
    // （ci.md）の各ネイティブランナーでは Windows ビルド時に使われる。
    #[allow(dead_code)]
    Unsupported(String),
    Error(String),
}

impl Outcome {
    /// `{"status":"...","value":...}` 形式の JSON 断片を返す（依存追加を避けるため
    /// 手書きシリアライズ。support.rs の `json_escape` を使う）。
    fn to_json(&self) -> String {
        match self {
            Outcome::Value(v) => format!("{{\"status\":\"measured\",\"value\":{v}}}"),
            Outcome::Skipped(reason) => {
                format!(
                    "{{\"status\":\"skipped\",\"reason\":\"{}\"}}",
                    json_escape(reason)
                )
            }
            Outcome::Unsupported(reason) => {
                format!(
                    "{{\"status\":\"unsupported\",\"reason\":\"{}\"}}",
                    json_escape(reason)
                )
            }
            Outcome::Error(reason) => {
                format!(
                    "{{\"status\":\"error\",\"reason\":\"{}\"}}",
                    json_escape(reason)
                )
            }
        }
    }
}

/// 計測対象 1 つ分の設定（Lightpanda / fandhe-browser）。
///
/// `bin` が `None` のときはバイナリ未提供として全計測を `Outcome::Skipped` にする
/// （`fandhe-browser-cli` は TASK-41.5 未着手のため、本 PR 時点では常にこの経路）。
struct Target {
    name: &'static str,
    bin: Option<PathBuf>,
    serve_args: Vec<String>,
    mcp_args: Vec<String>,
}

impl Target {
    fn from_env(
        name: &'static str,
        env_prefix: &str,
        default_serve: &str,
        default_mcp: &str,
    ) -> Self {
        let bin = std::env::var(format!("{env_prefix}_BIN"))
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        let serve_args = std::env::var(format!("{env_prefix}_SERVE_ARGS"))
            .ok()
            .map(|s| split_args(&s))
            .unwrap_or_else(|| split_args(default_serve));
        let mcp_args = std::env::var(format!("{env_prefix}_MCP_ARGS"))
            .ok()
            .map(|s| split_args(&s))
            .unwrap_or_else(|| split_args(default_mcp));
        Self {
            name,
            bin,
            serve_args,
            mcp_args,
        }
    }
}

/// 試行回数を環境変数から読み取る（`COMPETITOR_BENCH_TRIALS`）。範囲外・不正値は
/// 既定値へフォールバックする（外部入力を fail-closed に扱う。coding-rust.md）。
fn trial_count() -> usize {
    std::env::var("COMPETITOR_BENCH_TRIALS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_TRIALS).contains(&n))
        .unwrap_or(DEFAULT_TRIALS)
}

/// 子プロセスを確実に終了させる guard。早期 `return`（`?`）経路でも
/// `Drop` で `kill` + `wait` する（`wait` を省くとゾンビプロセスが残る）。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 空きポートを 1 つ確保して返す（`127.0.0.1:0` に bind して即座に解放する）。
/// 固定ポートにすると並列実行される他ベンチ・他 issue の worktree と衝突するため
/// 使わない。
fn reserve_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind failed: {e}"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| format!("local_addr failed: {e}"))
}

/// `serve` 系プロセスを起動し、TCP readiness probe（`GET /json/version`）が
/// `HTTP/1.1 200` 相当を返すまでの経過時間をミリ秒で返す。
///
/// `PERF-3`（cold start）が使う。相手プロセスをシェル経由で起動しない
/// （`Command::new` + 分割済み引数。security.md「インジェクション」対策）。
fn spawn_and_wait_ready(
    bin: &PathBuf,
    args: &[String],
) -> Result<(ChildGuard, u16, Duration), String> {
    let port = reserve_port()?;
    let expanded = expand_args(args, port);
    // cold start（PERF-3）はプロセスのロード時間を含めて計測する。`spawn()`
    // 自体が fork/exec を伴いブロッキングするため、計測の起点はプロセス起動
    // 呼び出し（`Command::new` 実行時点）に置き、`spawn()` 完了後には置かない
    // （起点を後ろにずらすと実行ファイルのロード時間が計測から漏れ、
    // cold start が実態より系統的に短く出る）。
    let start = Instant::now();
    let child = Command::new(bin)
        .args(&expanded)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let guard = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    let request = format!(
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    loop {
        if Instant::now() >= deadline {
            return Err("timeout waiting for readiness probe".to_string());
        }
        match probe_once(port, &request) {
            Some(status) if (200..300).contains(&status) => {
                return Ok((guard, port, start.elapsed()));
            }
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

/// readiness probe を 1 回だけ試す。接続失敗・応答不正はいずれも `None`
/// （呼び出し側がポーリングを継続する。panic させない）。
fn probe_once(port: u16, request: &str) -> Option<u16> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().ok()?,
        Duration::from_millis(200),
    )
    .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    read_line_bounded(&mut reader, &mut line).ok()?;
    parse_http_status(&line)
}

/// `BufRead::read_line` 相当だが、外部プロセスの応答を無制限に信用せず
/// `MAX_LINE_BYTES` を超えたら打ち切る（DoS を避ける。coding-rust.md）。
fn read_line_bounded<R: BufRead>(reader: &mut R, out: &mut String) -> std::io::Result<usize> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if reader.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' || buf.len() >= MAX_LINE_BYTES {
            break;
        }
    }
    *out = String::from_utf8_lossy(&buf).into_owned();
    Ok(buf.len())
}

/// cold start（`PERF-3`）: `trials` 回起動し、readiness までの時間の中央値（ms）を返す。
fn measure_cold_start(target: &Target, trials: usize) -> Outcome {
    let Some(bin) = &target.bin else {
        return Outcome::Skipped(format!("{}: binary path not configured", target.name));
    };
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        match spawn_and_wait_ready(bin, &target.serve_args) {
            Ok((guard, _port, elapsed)) => {
                samples.push(elapsed.as_secs_f64() * 1000.0);
                drop(guard);
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                return Outcome::Error(format!("{}: cold start trial failed: {e}", target.name));
            }
        }
    }
    match median(&samples) {
        Some(v) => Outcome::Value(v),
        None => Outcome::Error(format!("{}: no cold start samples", target.name)),
    }
}

/// アイドル RSS（`PERF-6`）: 起動して安定させたあと `ps -o rss=` で単一プロセスの
/// RSS（KB）を読む。Windows は `ps` が無いため `Unsupported` を返す
/// （実装計画どおり。`parse_ps_rss_kb` は unix 専用経路でのみ呼ぶ）。
#[cfg(unix)]
fn sample_rss_kb(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps_rss_kb(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(unix)]
fn measure_idle_rss(target: &Target, trials: usize) -> Outcome {
    let Some(bin) = &target.bin else {
        return Outcome::Skipped(format!("{}: binary path not configured", target.name));
    };
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        match spawn_and_wait_ready(bin, &target.serve_args) {
            Ok((guard, _port, _elapsed)) => {
                std::thread::sleep(Duration::from_millis(500));
                // `ps` の失敗（プロセス早期終了・パース不能出力）を黙って
                // 捨てず即座に Error 化する（レビュー指摘: 失敗試行を除外した
                // まま残り試行だけで中央値を報告すると、trials 回分の計測値
                // であるかのように結果件数と意味が食い違う）。
                let pid = guard.0.id();
                let rss = sample_rss_kb(pid);
                drop(guard);
                std::thread::sleep(Duration::from_millis(200));
                match rss {
                    Some(rss) => samples.push(rss as f64),
                    None => {
                        return Outcome::Error(format!(
                            "{}: idle RSS trial failed: `ps` sampling failed for pid {pid}",
                            target.name
                        ));
                    }
                }
            }
            Err(e) => {
                return Outcome::Error(format!("{}: idle RSS trial failed: {e}", target.name));
            }
        }
    }
    match median(&samples) {
        Some(v) => Outcome::Value(v),
        None => Outcome::Error(format!("{}: no idle RSS samples", target.name)),
    }
}

#[cfg(windows)]
fn measure_idle_rss(target: &Target, _trials: usize) -> Outcome {
    Outcome::Unsupported(format!(
        "{}: idle RSS measurement uses unix `ps`, unsupported on Windows (PERF-6)",
        target.name
    ))
}

/// バイナリサイズ（`PERF-1`）: 対象実行ファイルの `fs::metadata` によるバイト数。
fn measure_binary_size(target: &Target) -> Outcome {
    let Some(bin) = &target.bin else {
        return Outcome::Skipped(format!("{}: binary path not configured", target.name));
    };
    match std::fs::metadata(bin) {
        Ok(meta) => Outcome::Value(meta.len() as f64),
        Err(e) => Outcome::Error(format!("{}: metadata failed: {e}", target.name)),
    }
}

/// MCP stdio JSON-RPC クライアント。stdout を読む別スレッドが行単位で
/// `mpsc` チャンネルへ push し、呼び出し側は `id` 一致を待つ
/// （パイプに読み取りタイムアウトが無いため、スレッド + チャンネルで模す）。
struct McpClient {
    guard: ChildGuard,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn spawn(bin: &PathBuf, args: &[String]) -> Result<Self, String> {
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match read_line_bounded(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            guard: ChildGuard(child),
            stdin,
            rx,
            next_id: 1,
        })
    }

    /// JSON-RPC 呼び出しを 1 件送り、`id` が一致する応答を待つ
    /// （タイムアウト内に一致しなければ `Err`。他 id の応答・パース不能行は
    /// 読み捨てて継続する。外部プロセスの応答を untrusted として扱う）。
    ///
    /// `id` が一致しても JSON-RPC の `error` フィールドが立っている、または
    /// `tools/call` 結果の `result.isError` が `true` の応答は失敗として
    /// `Err` を返す（呼び出し元 `measure_token_reduction` はエラー応答を
    /// そのサイトの skip として扱うため、ここで成功と誤判定すると
    /// トークン削減率の計測に失敗レスポンスの本文が混入する）。
    fn call(&mut self, method: &str, params: &str, timeout: Duration) -> Result<JsonValue, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}\n"
        );
        self.stdin
            .write_all(request.as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flush failed: {e}"))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timeout waiting for response to {method}"));
            }
            let line = match self.rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(_) => return Err(format!("timeout waiting for response to {method}")),
            };
            let Ok(value) = parse_json(line.trim()) else {
                continue;
            };
            if value.get("id").and_then(JsonValue::as_f64) == Some(id as f64) {
                if let Some(err) = value.get("error") {
                    let message = err
                        .get("message")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("unknown error");
                    return Err(format!("{method}: rpc error: {message}"));
                }
                let is_error = value
                    .get("result")
                    .and_then(|r| r.get("isError"))
                    .is_some_and(|v| matches!(v, JsonValue::Bool(true)));
                if is_error {
                    return Err(format!("{method}: tool call reported isError"));
                }
                return Ok(value);
            }
        }
    }

    /// 応答を待たない通知（`initialize` 後の `notifications/initialized` 等）。
    fn notify(&mut self, method: &str, params: &str) -> Result<(), String> {
        let line = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{params}}}\n");
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;
        self.stdin.flush().map_err(|e| format!("flush failed: {e}"))
    }
}

/// MCP 応答の `result.content[].text` を連結して取り出す（PoC-13 の
/// `extractText` 相当。フィールド欠落は空文字列にし panic させない）。
fn extract_text(value: &JsonValue) -> String {
    let Some(content) = value.get("result").and_then(|r| r.get("content")) else {
        return String::new();
    };
    let mut out = String::new();
    let mut i = 0;
    while let Some(item) = content.index(i) {
        if let Some(text) = item.get("text").and_then(JsonValue::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
        i += 1;
    }
    out
}

/// AISNAP-1: 代表サイト群で `goto` → `html` / `tree` を呼び、近似トークン数の
/// 削減率を求める。PoC-13 と同じ 5 サイトを既定にし、`COMPETITOR_BENCH_SITES`
/// （カンマ区切り URL）で上書き可能にする。
fn measure_token_reduction(target: &Target) -> Outcome {
    let Some(bin) = &target.bin else {
        return Outcome::Skipped(format!("{}: binary path not configured", target.name));
    };
    let sites: Vec<String> = match std::env::var("COMPETITOR_BENCH_SITES") {
        Ok(raw) => {
            // 分割・確保の前にバイト数を検証する（外部入力。P0 レビュー指摘:
            // 上限検証なしに split・収集すると巨大な値でメモリ・実行時間を
            // 無制限に消費し得る）。
            if raw.len() > MAX_SITES_ENV_BYTES {
                return Outcome::Error(format!(
                    "{}: COMPETITOR_BENCH_SITES exceeds {MAX_SITES_ENV_BYTES} bytes",
                    target.name
                ));
            }
            let parsed: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if parsed.len() > MAX_SITES {
                return Outcome::Error(format!(
                    "{}: COMPETITOR_BENCH_SITES has more than {MAX_SITES} sites",
                    target.name
                ));
            }
            if parsed.is_empty() {
                DEFAULT_SITES.iter().map(|s| s.to_string()).collect()
            } else {
                parsed
            }
        }
        Err(_) => DEFAULT_SITES.iter().map(|s| s.to_string()).collect(),
    };

    let mut client = match McpClient::spawn(bin, &target.mcp_args) {
        Ok(c) => c,
        Err(e) => return Outcome::Error(format!("{}: mcp spawn failed: {e}", target.name)),
    };

    let init = client.call(
        "initialize",
        "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"competitor-lightpanda-bench\",\"version\":\"0.1\"}}",
        Duration::from_secs(20),
    );
    if let Err(e) = init {
        drop(client.guard);
        return Outcome::Error(format!("{}: mcp initialize failed: {e}", target.name));
    }
    let _ = client.notify("notifications/initialized", "{}");

    let mut reductions = Vec::new();
    for url in &sites {
        let goto_params = format!(
            "{{\"name\":\"goto\",\"arguments\":{{\"url\":\"{}\"}}}}",
            json_escape(url)
        );
        if client
            .call("tools/call", &goto_params, Duration::from_secs(20))
            .is_err()
        {
            continue;
        }
        let html = match client.call(
            "tools/call",
            "{\"name\":\"html\",\"arguments\":{}}",
            Duration::from_secs(20),
        ) {
            Ok(v) => extract_text(&v),
            Err(_) => continue,
        };
        let tree = match client.call(
            "tools/call",
            "{\"name\":\"tree\",\"arguments\":{}}",
            Duration::from_secs(20),
        ) {
            Ok(v) => extract_text(&v),
            Err(_) => continue,
        };
        let html_tok = approx_tokens(html.chars().count());
        let tree_tok = approx_tokens(tree.chars().count());
        if let Some(pct) = reduction_pct(html_tok as f64, tree_tok as f64) {
            reductions.push(pct);
        }
    }
    drop(client.guard);

    match median(&reductions) {
        Some(v) => Outcome::Value(v),
        None => Outcome::Error(format!(
            "{}: no site yielded a token reduction sample",
            target.name
        )),
    }
}

fn main() {
    let trials = trial_count();
    let targets = [
        Target::from_env(
            "lightpanda",
            "LIGHTPANDA",
            "serve --port {port} --log-level error",
            "mcp",
        ),
        Target::from_env(
            "fandhe-browser",
            "FANDHE_BROWSER",
            "serve --port {port}",
            "mcp",
        ),
    ];

    let mut body = String::from("{\n");
    for (i, target) in targets.iter().enumerate() {
        eprintln!("=== {} ===", target.name);
        let binary_size = measure_binary_size(target);
        eprintln!("binary size: {}", binary_size.to_json());
        let cold_start = measure_cold_start(target, trials);
        eprintln!(
            "cold start (ms, median of {trials}): {}",
            cold_start.to_json()
        );
        let idle_rss = measure_idle_rss(target, trials);
        eprintln!("idle RSS (KB, median of {trials}): {}", idle_rss.to_json());
        let token_reduction = measure_token_reduction(target);
        eprintln!("token reduction (%): {}", token_reduction.to_json());

        body.push_str(&format!(
            "  \"{}\": {{\"binarySizeBytes\":{},\"coldStartMs\":{},\"idleRssKb\":{},\"tokenReductionPct\":{}}}",
            json_escape(target.name),
            binary_size.to_json(),
            cold_start.to_json(),
            idle_rss.to_json(),
            token_reduction.to_json(),
        ));
        if i + 1 < targets.len() {
            body.push(',');
        }
        body.push('\n');
    }
    body.push_str("}\n");

    print!("{body}");
}
