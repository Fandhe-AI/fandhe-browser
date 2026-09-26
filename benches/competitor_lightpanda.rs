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
//! `AISNAP-1`（MCP トークン削減率）計測が `goto` する対象（PR #442 再レビュー。
//! Codex P0）: 以前は `COMPETITOR_BENCH_SITES` で任意の外部 URL を指定できたが、
//! 計測対象ブラウザは接続時に名前を再解決でき、公開 URL からのリダイレクトにも
//! 追従し得るため、事前の URL 検証をいくら積み増しても実際の接続先
//! （内部アドレスに到達しないこと）を保証できなかった（SSRF・TOCTOU）。この
//! ため任意 URL の指定経路を廃止し、本ベンチが自ら起動するローカル静的
//! サーバー（`127.0.0.1` の空きポート。[`FixtureServer`]）がリポジトリ同梱の
//! 自作 fixture（外部参照を含まない。`support::FIXTURE_TABLE`）を配信し、
//! そこだけを `goto` する構成にした。これは Lightpanda 本家のベンチ
//! （ローカルで配信するデモサイトを対象にする）と同じ形であり、ベンチが
//! 外部ネットワークへ一切出ないため SSRF 経路が構造的になくなり、外部サイトの
//! 可用性・内容変化にも左右されない再現可能な計測になる（security.md
//! 「SSRF」）。
//!
//! fixture 配信（PR #442 再々レビュー。Codex P0・P1）: 以前は fixture を
//! ファイルシステムから実行時に読んでいたため、字面上の `..` 拒否だけでは
//! fixture 配下のシンボリックリンクがルート外を指す経路を防げず、また
//! メタデータ確認後の読み込みまでの間にファイルサイズが変わる TOCTOU も
//! あった。fixture は `include_str!` でコンパイル時にバイナリへ埋め込み
//! （`support::FIXTURE_TABLE`）、リクエストパスをそのテーブルの完全一致
//! だけで引く（`support::lookup_fixture`）ことで、実行時のファイル I/O
//! 自体をなくし、シンボリックリンク・TOCTOU・サイズ超過が構造的に
//! 起こらないようにした。
//!
//! 終了コード契約（レビュー指摘 P1。Codex。PR #442）: JSON は必ず stdout へ
//! 出力したうえで、いずれかの計測項目が `Outcome::Error`（対象バイナリの
//! 起動失敗・MCP 呼び出し失敗等）になった場合は終了コード `1` で終了する。
//! 対象バイナリ未設定による `Outcome::Skipped`・Windows 未対応による
//! `Outcome::Unsupported` のみの場合は `0` で終了する。自動計測（CI 等）の
//! 呼び出し側はこの終了コードで成功・失敗を判定できる。
//!
//! 新規外部依存は追加しない（`Cargo.toml` の `dependencies` に変更なし。
//! dependency-policy.md）。`support::validate_local_bench_url` の URL
//! 正規化のみ `reqwest::Url`（暫定ホスト `fandhe-browser-core` が既に依存し、
//! `Cargo.lock` に解決済みの推移依存）を再利用する（詳細は `support.rs` の
//! 同関数ドキュメント参照）。それ以外は `std` のみで完結する。

#[path = "competitor_lightpanda/support.rs"]
mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use support::{
    JsonValue, Outcome, approx_tokens, bench_exit_code, expand_args, json_escape, lookup_fixture,
    median, parse_http_request_line, parse_http_status, parse_json, reduction_pct, split_args,
    token_reduction_gate, validate_local_bench_url,
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

/// `<PREFIX>_SERVE_ARGS` / `<PREFIX>_MCP_ARGS`（外部入力）に許す上限。
/// `split_args` へ渡す前にバイト数・分割後の引数個数を検証し、巨大な
/// 環境変数によるメモリ過剰消費を防ぐ（レビュー指摘 P1: line 140。
/// coding-rust.md「長さ・件数を上限検証」）。
const MAX_ARGS_ENV_BYTES: usize = 4 * 1024;
const MAX_ARGS_COUNT: usize = 64;

/// fixture 配信サーバーの各種上限（coding-rust.md「長さ・件数を上限検証」）。
/// ローカル専用のベンチ補助サーバーだが、想定外の大量・低速接続で
/// スレッドが専有され続けないよう防御的に上限を設ける
/// （security.md「不安全な設計」）。リクエスト行・ヘッダ行のサイズ上限は
/// 既存の `MAX_LINE_BYTES`（`read_line_bounded` が使う）を共用する。
/// fixture 本体はコンパイル時に埋め込み済み（`support::FIXTURE_TABLE`）で
/// 実行時のファイル読み込みがないため、ファイルサイズの上限は不要になった。
const FIXTURE_MAX_CONNECTIONS: u64 = 10_000;
/// 読み捨てるヘッダ行の総数上限。無制限だと大量のヘッダ行を送るクライアント
/// がサーバースレッドを専有し続け得る（advisor 指摘。coding-rust.md
/// 「長さ・件数を上限検証」）。実ブラウザが送る一般的なヘッダ数は数十件に
/// 収まるため、大きめに倍取って `64` とする。
const FIXTURE_MAX_HEADER_LINES: u32 = 64;
const FIXTURE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const FIXTURE_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// readiness probe・MCP 呼び出しそれぞれの外部入力（応答行）に許す最大長。
/// 相手プロセスが不正・悪意ある出力を送り続けても無制限にバッファへ
/// 蓄積しないための上限（coding-rust.md）。
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 計測対象 1 つ分の設定（Lightpanda / fandhe-browser）。
///
/// `bin` が `None` のときはバイナリ未提供として全計測を `Outcome::Skipped` にする
/// （`fandhe-browser-cli` は TASK-41.5 未着手のため、本 PR 時点では常にこの経路）。
struct Target {
    name: &'static str,
    bin: Option<PathBuf>,
    serve_args: Vec<String>,
    mcp_args: Vec<String>,
    /// `serve_args` / `mcp_args` の元となった環境変数がサイズ・個数上限を
    /// 超えていた場合の理由。`Some` のときは各計測関数が起動を試みず
    /// 即座に `Outcome::Error` を返す（レビュー指摘 P1: line 140）。
    args_error: Option<String>,
}

/// `<PREFIX>_SERVE_ARGS` / `<PREFIX>_MCP_ARGS` を読み取り、上限検証してから
/// `split_args` へ渡す（外部入力。coding-rust.md「長さ・件数を上限検証」）。
fn args_from_env(env_prefix: &str, var_suffix: &str, default: &str) -> Result<Vec<String>, String> {
    match std::env::var(format!("{env_prefix}_{var_suffix}")) {
        Ok(raw) => {
            if raw.len() > MAX_ARGS_ENV_BYTES {
                return Err(format!(
                    "{env_prefix}_{var_suffix} exceeds {MAX_ARGS_ENV_BYTES} bytes"
                ));
            }
            let args = split_args(&raw);
            if args.len() > MAX_ARGS_COUNT {
                return Err(format!(
                    "{env_prefix}_{var_suffix} has more than {MAX_ARGS_COUNT} args"
                ));
            }
            Ok(args)
        }
        Err(_) => Ok(split_args(default)),
    }
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
        let serve_args = args_from_env(env_prefix, "SERVE_ARGS", default_serve);
        let mcp_args = args_from_env(env_prefix, "MCP_ARGS", default_mcp);
        let args_error = serve_args
            .as_ref()
            .err()
            .or(mcp_args.as_ref().err())
            .cloned();
        Self {
            name,
            bin,
            serve_args: serve_args.unwrap_or_default(),
            mcp_args: mcp_args.unwrap_or_default(),
            args_error,
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

/// `competitor_lightpanda/fixtures/` を配信するローカル静的サーバー。
///
/// レビュー指摘 P0（Codex。PR #442 再レビュー・competitor_lightpanda.rs:729/783）:
/// 事前の URL 検証をいくら積み増しても、計測対象ブラウザが実際に接続する
/// 宛先（名前再解決・リダイレクト追従込み）は保証できない。この問題は
/// 「ベンチが外部ネットワークへ一切出ない構成にする」ことでのみ解消できる
/// ため、`127.0.0.1` の空きポートへ bind した最小限の HTTP/1.1 静的サーバーを
/// 自前で立て、fixture ディレクトリ配下のファイルだけを返す（Lightpanda 本家
/// のベンチがローカルで配信するデモサイトを対象にするのと同じ構成）。
///
/// 接続受付はバックグラウンドスレッドで行い、`Drop` で `stop` フラグを立てて
/// スレッドの終了を待つ（プロセス終了時にリスナーを残さない）。
struct FixtureServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FixtureServer {
    /// サーバーを起動する。`support::FIXTURE_TABLE`（コンパイル時に埋め込み
    /// 済みの固定テーブル）に完全一致するパスだけを配信する。
    fn start() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind failed: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("local_addr failed: {e}"))?
            .port();
        // accept をノンブロッキングにし、`stop` を定期的に確認することで
        // `Drop` 時にスレッドを即座に終了させる（listener を閉じるまで
        // `accept` がブロックし続ける方式は使わない）。
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking failed: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            fixture_accept_loop(listener, stop_for_thread);
        });
        Ok(Self {
            port,
            stop,
            handle: Some(handle),
        })
    }

    /// このサーバーの `http://127.0.0.1:<port>/` 形式のベース URL。
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// [`FixtureServer::start`] が生成する accept ループ本体。
///
/// `stop` が立つまで `listener.accept()` をポーリングし、接続ごとに
/// [`handle_fixture_connection`] を同期的に処理する（ベンチ自身が唯一の
/// クライアントであるためシングルスレッドで十分。並行処理はしない）。
fn fixture_accept_loop(listener: TcpListener, stop: Arc<AtomicBool>) {
    let mut served: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                served += 1;
                // レビュー指摘（コーディネーター指示）: 接続数に上限を設け、
                // 想定外の大量接続でサーバーが無制限に処理し続けないようにする。
                if served > FIXTURE_MAX_CONNECTIONS {
                    break;
                }
                handle_fixture_connection(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(FIXTURE_ACCEPT_POLL_INTERVAL);
            }
            // listener 自体のエラー（fd 枯渇等）はループを止める。`stop` の
            // 検知漏れでプロセスが終了できなくなることを避ける。
            Err(_) => break,
        }
    }
}

/// fixture サーバーへの 1 接続を処理する。
///
/// リクエスト行のみを読み取り（ヘッダ・ボディは無視。fixture 配信に不要）、
/// [`lookup_fixture`]（`support::FIXTURE_TABLE` の完全一致検索。実行時の
/// ファイル I/O を行わない）で本文を引き、`200`（成功）・`404`（未検出）・
/// `400`（リクエスト行が不正）のいずれかを返す。読み書きに
/// `FIXTURE_IO_TIMEOUT` を設定し、低速・応答なしクライアントでスレッドが
/// 無期限にブロックしないようにする（coding-rust.md「不安全な設計」）。
///
/// レビュー指摘 P0・P1（Codex。PR #442 再々レビュー・
/// competitor_lightpanda.rs:338/352）: 以前はここでファイルシステムから
/// `metadata`/`read` していたため、(1) シンボリックリンクを辿ってしまい
/// fixture ディレクトリ外のファイルを配信し得た、(2) メタデータ確認後の
/// 読み込みまでの TOCTOU でサイズ上限を超えて確保し得た。`lookup_fixture`
/// はコンパイル時に埋め込んだ `&'static str` を返すだけでファイルシステムへ
/// 一切触れないため、両方とも構造的に起こらない。
fn handle_fixture_connection(mut stream: TcpStream) {
    // レビュー指摘（advisor。PR #442 再レビュー後の追加指摘）: macOS（XNU）・
    // Windows（Winsock）では accept したソケットが listener のノンブロッキング
    // 状態を継承する（Linux の accept4 は継承しない）。継承されたままだと
    // 後続の `read_line_bounded` の最初の `read` が即座に `WouldBlock` を
    // 返し、リクエスト到着前に 400 を返してしまう。ブロッキングへ明示的に
    // 戻すことで 3 OS で同じ挙動にする（coding-rust.md「クロスプラットフォーム」）。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(FIXTURE_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(FIXTURE_IO_TIMEOUT));

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if read_line_bounded(&mut reader, &mut line).is_err() {
        let _ = write_fixture_response(&mut stream, 400, "Bad Request", PLAIN_TEXT, b"");
        return;
    }

    let Some((method, path)) = parse_http_request_line(&line) else {
        let _ = write_fixture_response(&mut stream, 400, "Bad Request", PLAIN_TEXT, b"");
        return;
    };
    if method != "GET" {
        let _ = write_fixture_response(&mut stream, 400, "Bad Request", PLAIN_TEXT, b"");
        return;
    }

    // ヘッダ行は使わないが、クライアント（計測対象ブラウザ）が送り終える前に
    // 接続を切ると `RST` になり得るため、`Connection: close` 前提で読み捨てる
    // （1 行あたりは `read_line_bounded` の `MAX_LINE_BYTES` で上限済み）。
    // レビュー指摘（advisor。PR #442 再レビュー後の追加指摘）: 行数自体には
    // 上限がなく、ヘッダ行を送り続けるクライアントがサーバースレッドを
    // 無期限に専有し得た。`FIXTURE_MAX_HEADER_LINES` で総行数にも上限を
    // 設け、超過時は 400 を返して打ち切る（coding-rust.md「長さ・件数を
    // 上限検証」）。終端判定は `\r\n`（CRLF）だけでなく裸の `\n`（LF のみ）も
    // 空行として扱う（一部クライアントは LF のみを送るため。LF のみを
    // 見逃すと `\r\n` を待ち続けて `FIXTURE_IO_TIMEOUT` 分停止していた）。
    let mut header_lines_seen = 0u32;
    loop {
        if header_lines_seen >= FIXTURE_MAX_HEADER_LINES {
            let _ = write_fixture_response(&mut stream, 400, "Bad Request", PLAIN_TEXT, b"");
            return;
        }
        header_lines_seen += 1;
        let mut header_line = String::new();
        match read_line_bounded(&mut reader, &mut header_line) {
            Ok(0) => break,
            Ok(_) if header_line == "\r\n" || header_line == "\n" || header_line.is_empty() => {
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    match lookup_fixture(&path) {
        Some((content_type, content)) => {
            let _ =
                write_fixture_response(&mut stream, 200, "OK", content_type, content.as_bytes());
        }
        None => {
            let _ = write_fixture_response(&mut stream, 404, "Not Found", PLAIN_TEXT, b"");
        }
    }
}

/// 400/404 応答の `Content-Type`。fixture 本体は `support::FIXTURE_TABLE`
/// が個別に持つ content-type を使う（レビュー指摘。PR #442 再々レビュー:
/// パス → (content-type, 内容) の固定テーブルにする）。
const PLAIN_TEXT: &str = "text/plain; charset=utf-8";

/// `handle_fixture_connection` が使う最小限の HTTP/1.1 レスポンス書き込み。
fn write_fixture_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
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
///
/// レビュー指摘 P1（PR #442）: 以前は `MAX_LINE_BYTES` に達しても改行未検出の
/// まま `Ok` を返しており、呼び出し側（`probe_once`・MCP stdout 読み取り
/// スレッド）が切り詰められた不完全な行を正常な 1 行として扱い得た
/// （`parse_json` が偶然パース可能な断片を返す・`parse_http_status` が誤った
/// 値を拾う等）。改行を見ないまま上限へ達した場合は
/// `ErrorKind::InvalidData` で明示的に失敗させ、呼び出し側に「この行は
/// 読み取れなかった」ことを伝える（coding-rust.md「外部入力の経路では
/// 明示的に処理する」）。
fn read_line_bounded<R: BufRead>(reader: &mut R, out: &mut String) -> std::io::Result<usize> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if reader.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() >= MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("line exceeded MAX_LINE_BYTES ({MAX_LINE_BYTES}) without newline"),
            ));
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
    if let Some(reason) = &target.args_error {
        return Outcome::Error(format!("{}: {reason}", target.name));
    }
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
    if let Some(reason) = &target.args_error {
        return Outcome::Error(format!("{}: {reason}", target.name));
    }
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
        Ok(meta) if meta.is_file() => Outcome::Value(meta.len() as f64),
        // レビュー指摘 P2: line 398。`fs::metadata` はディレクトリにも成功し
        // `len()` がディレクトリエントリサイズ等の無意味な値を返すため、
        // `<PREFIX>_BIN` に誤ってディレクトリを指定した場合に異常値が
        // そのまま `binarySizeBytes` として出力され得た。通常ファイルで
        // ないことを検出したら計測失敗として扱う。
        Ok(_) => Outcome::Error(format!("{}: bin path is not a regular file", target.name)),
        Err(e) => Outcome::Error(format!("{}: metadata failed: {e}", target.name)),
    }
}

/// stdout 読み取りスレッドが呼び出し側との間に持つ行キューの上限件数。
/// `MAX_LINE_BYTES`（1 行あたりの上限）だけでは、外部プロセスが応答を
/// 読ませないまま大量の行を送り続けた場合にキューが無制限に伸びメモリを
/// 枯渇させ得る（レビュー指摘 P0: line 386）。`mpsc::sync_channel` で
/// 容量を区切り、溢れたら読み取りスレッドを止めて `overflowed` を立てる。
/// 256 件（最悪 256 × `MAX_LINE_BYTES` = 256MiB）は、正常系での
/// `notifications/message` 等のログ行バーストを誤検知しない余裕を持たせつつ、
/// 無制限確保にはしない上限として選んだ値。
const MCP_STDOUT_QUEUE_CAPACITY: usize = 256;

/// `McpClient::notify` の書き込みタイムアウト（レビュー指摘 P1（PR #442））。
/// `notify` は応答を待たないが、書き込み自体（`write_with_deadline`）は
/// 相手プロセスが標準入力を読まない場合に無期限へブロックし得るため、
/// `call` と同様に期限を設ける。応答待ちが無い分 `call` の個別呼び出しより
/// 短い固定値で十分とみなす。
const NOTIFY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// MCP stdio JSON-RPC クライアント。stdout を読む別スレッドが行単位で
/// 容量付き `mpsc` チャンネルへ push し、呼び出し側は `id` 一致を待つ
/// （パイプに読み取りタイムアウトが無いため、スレッド + チャンネルで模す）。
struct McpClient {
    guard: ChildGuard,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
    /// 読み取りスレッドがキュー容量超過を検知した際に立てるフラグ。
    /// `call` はこれを見て、無制限にキューを溜め込む代わりに
    /// 計測をエラー終了させる。
    overflowed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 読み取りスレッドが `read_line_bounded` の `InvalidData`（`MAX_LINE_BYTES`
    /// 到達・改行未検出）を検知した際に立てるフラグ（レビュー指摘 P1）。
    /// `call` はこれを見て、切り詰められた行を無視したまま待ち続けるのではなく
    /// 明示的にエラー終了させる。
    line_too_long: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
        let (tx, rx) = mpsc::sync_channel::<String>(MCP_STDOUT_QUEUE_CAPACITY);
        let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflowed_writer = std::sync::Arc::clone(&overflowed);
        let line_too_long = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let line_too_long_writer = std::sync::Arc::clone(&line_too_long);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match read_line_bounded(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => match tx.try_send(line) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(_)) => {
                            // キュー容量超過: 消費側が追いつけていない、または
                            // 相手プロセスが応答を読ませず出力し続けている。
                            // 無制限に溜め込まず読み取りを止め、呼び出し側へは
                            // `overflowed` 経由でエラーとして伝える。
                            overflowed_writer.store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        // `MAX_LINE_BYTES` に達し改行未検出のまま打ち切られた行
                        // （レビュー指摘 P1）。切り詰められた断片を正常応答として
                        // 扱わせず、読み取りを止めて `call` 側へ明示的に伝える。
                        line_too_long_writer.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
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
            overflowed,
            line_too_long,
        })
    }

    /// 書き込み中に相手プロセスが標準入力を読まずパイプが満杯になっても
    /// 無期限にブロックしないよう、`deadline` までに `write_all` + `flush` が
    /// 終わらなければ子プロセスを `kill` して待ち構えているスレッドを
    /// 解放する（レビュー指摘 P1（PR #442）。子プロセスの標準入力（パイプ）
    /// には OS レベルの書き込みタイムアウトが無いため、実際の書き込みは
    /// `std::thread::scope` の子スレッドへ切り出し、`mpsc` の
    /// `recv_timeout` で待つ。タイムアウト時は `child.kill()` で
    /// 子プロセスの読み取り側を閉じ、ブロックしている `write_all` を
    /// `BrokenPipe` で解放してからスレッドの終了（送信）を待つ
    /// （`thread::scope` はスコープ終了時に生成した全スレッドの
    /// join を待つため、解放せずに抜けようとすると結局無期限に
    /// ブロックする）。
    fn write_with_deadline(
        stdin: &mut std::process::ChildStdin,
        child: &mut Child,
        data: &[u8],
        deadline: Instant,
    ) -> Result<(), String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("write timed out before starting (deadline already elapsed)".to_string());
        }
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let result = stdin.write_all(data).and_then(|()| stdin.flush());
                let _ = tx.send(result);
            });
            match rx.recv_timeout(remaining) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("write failed: {e}")),
                Err(_) => {
                    let _ = child.kill();
                    // 書き込みスレッドが `BrokenPipe` 等で終わり `tx.send` する
                    // のを待つ（join のためにスレッドの終了を確定させる）。
                    let _ = rx.recv();
                    Err("write timed out, child process killed".to_string())
                }
            }
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
    ///
    /// `timeout` は書き込み（`write_with_deadline`）から応答待ちまでの
    /// 呼び出し全体に適用する（レビュー指摘 P1（PR #442）: 以前は書き込みに
    /// 期限が無く、書き込みが無期限にブロックし得た）。
    fn call(&mut self, method: &str, params: &str, timeout: Duration) -> Result<JsonValue, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}\n"
        );
        let deadline = Instant::now() + timeout;
        Self::write_with_deadline(
            &mut self.stdin,
            &mut self.guard.0,
            request.as_bytes(),
            deadline,
        )
        .map_err(|e| format!("{method}: {e}"))?;

        loop {
            if self.overflowed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(format!(
                    "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_CAPACITY} lines, aborting"
                ));
            }
            if self.line_too_long.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(format!(
                    "{method}: mcp response line exceeded {MAX_LINE_BYTES} bytes without newline, aborting"
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timeout waiting for response to {method}"));
            }
            let line = match self.rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(_) => {
                    if self.overflowed.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(format!(
                            "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_CAPACITY} lines, aborting"
                        ));
                    }
                    if self.line_too_long.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(format!(
                            "{method}: mcp response line exceeded {MAX_LINE_BYTES} bytes without newline, aborting"
                        ));
                    }
                    return Err(format!("timeout waiting for response to {method}"));
                }
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
                let Some(result) = value.get("result") else {
                    // レビュー指摘 P1: line 535。`error` も `result.isError` も
                    // 無いが `result` 自体が欠けている応答（プロトコル逸脱・
                    // 実質失敗）を、id 一致のみで成功扱いにしていた。
                    // `result` 欠如は不正な応答として `Err` にし、
                    // 呼び出し元 `measure_token_reduction` の
                    // 失敗サイト集計（`failed` への記録）へ回す。
                    return Err(format!("{method}: response missing result field"));
                };
                let is_error = result
                    .get("isError")
                    .is_some_and(|v| matches!(v, JsonValue::Bool(true)));
                if is_error {
                    return Err(format!("{method}: tool call reported isError"));
                }
                return Ok(value);
            }
        }
    }

    /// 応答を待たない通知（`initialize` 後の `notifications/initialized` 等）。
    /// 応答を待たないだけで、書き込み自体には `call` 同様
    /// `write_with_deadline`（レビュー指摘 P1（PR #442））で
    /// `NOTIFY_WRITE_TIMEOUT` を適用し、相手プロセスが標準入力を読まない
    /// 場合の無期限ブロックを避ける。
    fn notify(&mut self, method: &str, params: &str) -> Result<(), String> {
        let line = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{params}}}\n");
        let deadline = Instant::now() + NOTIFY_WRITE_TIMEOUT;
        Self::write_with_deadline(
            &mut self.stdin,
            &mut self.guard.0,
            line.as_bytes(),
            deadline,
        )
        .map_err(|e| format!("{method}: {e}"))
    }
}

/// MCP 応答の `result.content[].text` を連結して取り出す（PoC-13 の
/// `extractText` 相当）。`result.content` が欠如している、または
/// `content` 内のどの項目にも `text` が無く連結結果が空文字列になる場合は
/// 抽出失敗として `None` を返す（panic はさせない）。
///
/// レビュー指摘 P1: line 569。以前は欠落時に空文字列を返しており、
/// `html` が非空で `tree` の抽出に失敗しただけのケースが
/// `tree_tok=0` の正常計測（削減率 100%）として記録されてしまっていた。
/// 呼び出し元 `measure_token_reduction` は `None` を抽出失敗として扱い、
/// そのサイトを成功サンプルに含めない。
fn extract_text(value: &JsonValue) -> Option<String> {
    let content = value.get("result").and_then(|r| r.get("content"))?;
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
    if out.is_empty() { None } else { Some(out) }
}

/// AISNAP-1: `support::FIXTURE_TABLE`（ベンチが起動したローカル fixture サーバーが
/// 配信する自作ページ）へ `goto` → `html` / `tree` を呼び、近似トークン数の
/// 削減率を求める。`fixture_base_url`・`fixture_port` は呼び出し元
/// （`main`）が起動した [`FixtureServer`] のもの（モジュールドキュメント
/// 参照。PR #442 再レビューで外部 URL の指定経路を廃止した）。
fn measure_token_reduction(target: &Target, fixture_base_url: &str, fixture_port: u16) -> Outcome {
    let Some(bin) = &target.bin else {
        return Outcome::Skipped(format!("{}: binary path not configured", target.name));
    };
    if let Some(reason) = &target.args_error {
        return Outcome::Error(format!("{}: {reason}", target.name));
    }
    let sites: Vec<String> = support::FIXTURE_TABLE
        .iter()
        .map(|(page, _content_type, _content)| format!("{fixture_base_url}{page}"))
        .collect();
    // レビュー指摘 P0（Codex。PR #442 再レビュー）: 生成した URL が本当に
    // このベンチが起動した fixture サーバー（127.0.0.1・起動したポート）
    // だけを指すことを最後にもう一度確認する（モジュールドキュメント参照。
    // ここで拒否されるのは実装バグの場合のみで、通常経路では常に成功する）。
    for url in &sites {
        if let Err(reason) = validate_local_bench_url(url, fixture_port) {
            return Outcome::Error(format!("{}: {reason}", target.name));
        }
    }

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
    // レビュー指摘 P1: line 747。送信失敗を握りつぶすと、初期化未完了のまま
    // 後続の `goto` 等の計測へ進み、真因（初期化通知の送信失敗）とは別の
    // エラーとして誤って報告され得る。送信失敗は直ちに `Outcome::Error` で
    // 返す（fail-closed。coding-rust.md「外部入力の経路では unwrap を使わず
    // 明示的に処理する」）。
    if let Err(e) = client.notify("notifications/initialized", "{}") {
        drop(client.guard);
        return Outcome::Error(format!(
            "{}: mcp notifications/initialized failed: {e}",
            target.name
        ));
    }

    // レビュー指摘 P1: line 555。以前は goto・html・tree の失敗サイトを
    // 無言で `continue` して除外し、残ったサイトだけの中央値を
    // `measured` として返していたため、既定 5 サイト中 1 サイトしか
    // 成功しなくても代表値であるかのように報告され得た。失敗したサイトと
    // 理由を `failed` に記録し、1 件でも完走できなければ `Outcome::Error`
    // にして「一部失敗を隠した測定値」を返さない（fail-closed）。
    let mut reductions = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for url in &sites {
        let goto_params = format!(
            "{{\"name\":\"goto\",\"arguments\":{{\"url\":\"{}\"}}}}",
            json_escape(url)
        );
        if let Err(e) = client.call("tools/call", &goto_params, Duration::from_secs(20)) {
            failed.push(format!("{url}: goto failed: {e}"));
            continue;
        }
        let html = match client.call(
            "tools/call",
            "{\"name\":\"html\",\"arguments\":{}}",
            Duration::from_secs(20),
        ) {
            Ok(v) => match extract_text(&v) {
                Some(t) => t,
                None => {
                    failed.push(format!("{url}: html extraction failed"));
                    continue;
                }
            },
            Err(e) => {
                failed.push(format!("{url}: html call failed: {e}"));
                continue;
            }
        };
        let tree = match client.call(
            "tools/call",
            "{\"name\":\"tree\",\"arguments\":{}}",
            Duration::from_secs(20),
        ) {
            Ok(v) => match extract_text(&v) {
                Some(t) => t,
                None => {
                    failed.push(format!("{url}: tree extraction failed"));
                    continue;
                }
            },
            Err(e) => {
                failed.push(format!("{url}: tree call failed: {e}"));
                continue;
            }
        };
        let html_tok = approx_tokens(html.chars().count());
        let tree_tok = approx_tokens(tree.chars().count());
        match reduction_pct(html_tok as f64, tree_tok as f64) {
            Some(pct) => reductions.push(pct),
            None => failed.push(format!("{url}: reduction_pct undefined (html_tok=0)")),
        }
    }
    drop(client.guard);

    if !failed.is_empty() {
        return Outcome::Error(format!(
            "{}: {}/{} sites failed: {}",
            target.name,
            failed.len(),
            sites.len(),
            failed.join("; ")
        ));
    }

    match median(&reductions) {
        Some(v) => Outcome::Value(v),
        None => Outcome::Error(format!(
            "{}: no site yielded a token reduction sample",
            target.name
        )),
    }
}

fn main() -> std::process::ExitCode {
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

    // レビュー指摘 P0（Codex。PR #442 再レビュー）: `AISNAP-1` 計測は外部
    // URL を一切使わず、ここで起動するローカル fixture サーバーだけを
    // 対象にする（モジュールドキュメント参照）。両方の `target` で
    // 同じサーバーを共有し、`main` を抜けるときに `Drop` で停止する。
    // どちらの対象バイナリも未設定（`bin` が `None`）のときは
    // `measure_token_reduction` が最初の分岐で必ず `Outcome::Skipped` を
    // 返しサーバーを使わないため、起動自体を省く（advisor 指摘: 起動を
    // 無条件にすると、サンドボックス等で `bind` が失敗した場合に
    // 「対象未設定で Skipped のみ→終了コード 0」の契約が崩れ、実際には
    // 使わないはずのサーバー起動失敗で `Outcome::Error`（終了コード 1）に
    // なってしまう）。
    let any_target_configured = targets.iter().any(|t| t.bin.is_some());
    let fixture_server = if any_target_configured {
        Some(FixtureServer::start())
    } else {
        None
    };

    let mut body = String::from("{\n");
    // レビュー指摘 P1（Codex。PR #442・competitor_lightpanda.rs:893）:
    // 全計測項目の `Outcome` をここへ集め、`bench_exit_code` へ一括で渡す
    // （`Outcome::is_error` は support.rs 内限定のため、判定自体は
    // `bench_exit_code` 側に閉じる）。
    let mut outcomes: Vec<Outcome> = Vec::new();
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
        // レビュー指摘 P1（Codex。PR #442 再々レビュー・
        // competitor_lightpanda.rs:1058）: 片方の対象だけに `_BIN` を設定した
        // 状態で fixture サーバーの起動が失敗すると、以前は未設定の対象にも
        // `Outcome::Error` を割り当てていた（「対象バイナリ未設定は常に
        // `Outcome::Skipped`」という契約に反する）。`token_reduction_gate`
        // （support.rs。純粋関数）へ対象ごとの `bin` 有無を渡して判定を
        // 対象単位にする。
        let server_start_error = fixture_server.as_ref().and_then(|r| r.as_ref().err());
        let token_reduction = match token_reduction_gate(
            target.bin.is_some(),
            server_start_error.map(String::as_str),
            target.name,
        ) {
            Some(outcome) => outcome,
            None => match &fixture_server {
                Some(Ok(server)) => {
                    measure_token_reduction(target, &server.base_url(), server.port)
                }
                // `token_reduction_gate` が `None` を返すのは
                // `bin_configured && server_start_error.is_none()` の場合
                // だけであり、`bin_configured` なら `any_target_configured`
                // も真なので `fixture_server` は必ず `Some(Ok(_))` のはず。
                // この不変条件が崩れた場合でも panic はせず fail-closed に
                // `Error` を返す（coding-rust.md「外部入力の経路では
                // unwrap を使わず明示的に処理する」の精神を内部不変条件にも
                // 適用する）。
                _ => Outcome::Error(format!(
                    "{}: internal error: fixture server unavailable",
                    target.name
                )),
            },
        };
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

        outcomes.push(binary_size);
        outcomes.push(cold_start);
        outcomes.push(idle_rss);
        outcomes.push(token_reduction);
    }
    body.push_str("}\n");

    // 終了コードを決める前に必ず JSON を出力する（モジュールドキュメントの
    // 終了コード契約参照。呼び出し側が失敗時も結果 JSON を取得できるようにする）。
    print!("{body}");

    let outcome_refs: Vec<&Outcome> = outcomes.iter().collect();
    std::process::ExitCode::from(bench_exit_code(&outcome_refs))
}
