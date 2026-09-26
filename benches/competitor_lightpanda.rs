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

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::{
    DeadlineReader, JsonValue, Outcome, approx_tokens, arg_error_gate, bench_exit_code,
    content_length_exceeds_limit, exit_status_to_result, expand_args, http_status_for_io_error,
    json_escape, looks_like_browser_readiness_response, lookup_fixture, median,
    parse_content_length, parse_http_request_line, parse_http_status, parse_json, reduction_pct,
    require_bin, split_args, token_reduction_gate, validate_local_bench_url, validate_mcp_response,
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
    /// `serve_args` の元となった `<PREFIX>_SERVE_ARGS` がサイズ・個数上限を
    /// 超えていた場合の理由。`Some` のときは `serve_args` を使う計測
    /// （cold start・アイドル RSS）だけが起動を試みず即座に `Outcome::Error`
    /// を返す。
    ///
    /// レビュー指摘 P2（Codex。PR #442 再々々々レビュー・
    /// competitor_lightpanda.rs:511/554）: 以前は `serve_args`・`mcp_args`
    /// 両方の検証エラーを 1 つの `args_error` フィールドにまとめていたため、
    /// `MCP_ARGS` だけが上限超過でも、`MCP_ARGS` を使わない cold start・
    /// アイドル RSS 計測まで計測前に `Outcome::Error` になっていた
    /// （逆方向も同様）。用途ごとに独立したフィールドへ分け、各計測関数が
    /// 自分の使う引数のエラーだけを [`support::arg_error_gate`] 経由で
    /// 確認するようにした。
    serve_args_error: Option<String>,
    /// `mcp_args` の元となった `<PREFIX>_MCP_ARGS` がサイズ・個数上限を
    /// 超えていた場合の理由。`Some` のときは `mcp_args` を使う計測
    /// （`AISNAP-1` トークン削減率）だけが起動を試みず即座に
    /// `Outcome::Error` を返す。
    mcp_args_error: Option<String>,
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
        let serve_args_error = serve_args.as_ref().err().cloned();
        let mcp_args_error = mcp_args.as_ref().err().cloned();
        Self {
            name,
            bin,
            serve_args: serve_args.unwrap_or_default(),
            mcp_args: mcp_args.unwrap_or_default(),
            serve_args_error,
            mcp_args_error,
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

/// リクエスト行・ヘッダ行を 1 行読み、`read_line_bounded` が返す行が
/// 改行で終わっていない（`Ok(0)` の完全 EOF、または `MAX_LINE_BYTES` 未満で
/// 接続が切れた不完全な行）場合を、まとめて「行を読み切れなかった」
/// エラーとして扱う。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
/// competitor_lightpanda.rs:352）: `read_line_bounded` はバイト単位で読み、
/// 以前は相手が改行を送る前に接続を閉じても、それまでに読めた分だけを
/// `Ok(n)`（`n > 0`）で返していた。呼び出し側がこれを「1 行読めた」として
/// 扱うと、ヘッダ終端（空行）へ到達しないまま後続処理（`lookup_fixture`
/// による 200 応答）へ進み得た。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
/// competitor_lightpanda.rs:1039）: この「改行で終わらない行は
/// `UnexpectedEof`」という契約自体は `read_line_bounded` 側へ集約した
/// （fixture サーバー・probe・MCP のすべての呼び出し元で一貫させるため。
/// 同関数のドキュメント参照）。この `read_complete_line` は
/// fixture サーバー・probe が使う薄いラッパーで、`read_line_bounded` が
/// 返す `Ok(0)`（＝ストリームの正常終端。MCP はこれを「もう応答が無い」
/// という正常なシグナルとして扱うが、fixture サーバー・probe は
/// 「リクエスト行・ヘッダ行を 1 行も受け取れなかった」ことを意味するため
/// 常に失敗として扱う）を `Err` へ変換する。
fn read_complete_line<R: BufRead>(reader: &mut R, out: &mut String) -> std::io::Result<()> {
    match read_line_bounded(reader, out) {
        Ok(0) => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed before a line was received",
        )),
        Ok(_) => {
            // `read_line_bounded` の契約により、ここに到達する `Ok(_)` は
            // 必ず改行で終わる完全な行である（改行前に EOF に達した場合は
            // 既に `Err(UnexpectedEof)` になっている）。
            debug_assert!(out.ends_with('\n'));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// `handle_fixture_connection` から、読み取りエラーを
/// `support::http_status_for_io_error` で分類して応答し接続を終える
/// ための小さなヘルパー。
fn respond_with_io_error(writer: &mut DeadlineReader, err: &std::io::Error) {
    let (status, reason) = http_status_for_io_error(err.kind());
    let _ = write_fixture_response(writer, status, reason, PLAIN_TEXT, b"", true);
}

/// fixture サーバーへの 1 接続を処理する。
///
/// リクエスト行とヘッダ行を読み取り（ボディは無視。fixture 配信に不要）、
/// [`lookup_fixture`]（`support::FIXTURE_TABLE` の完全一致検索。実行時の
/// ファイル I/O を行わない）で本文を引き、`200`（成功）・`404`（未検出）・
/// `405`（許可しないメソッド）・`400`（リクエスト行/ヘッダが不正・読み取り
/// 未完了）・`408`（読み取りタイムアウト）のいずれかを返す。読み書きに
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
///
/// レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
/// competitor_lightpanda.rs:352）: リクエスト行・ヘッダ行の読み取りが
/// タイムアウト・サイズ超過・接続の早期切断のいずれで失敗しても、以前は
/// ループを抜けるだけで後続の 200 応答へ進み得た。読み取りが完了しなかった
/// 経路はすべて [`respond_with_io_error`] で 400/408 を返してから接続を
/// 終了する。
fn handle_fixture_connection(stream: TcpStream) {
    // レビュー指摘（advisor。PR #442 再レビュー後の追加指摘）: macOS（XNU）・
    // Windows（Winsock）では accept したソケットが listener のノンブロッキング
    // 状態を継承する（Linux の accept4 は継承しない）。継承されたままだと
    // 後続の `read_line_bounded` の最初の `read` が即座に `WouldBlock` を
    // 返し、リクエスト到着前に 400 を返してしまう。ブロッキングへ明示的に
    // 戻すことで 3 OS で同じ挙動にする（coding-rust.md「クロスプラットフォーム」）。
    let _ = stream.set_nonblocking(false);

    // レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
    // competitor_lightpanda.rs:364）: 接続 1 本ごとの絶対期限を
    // [`DeadlineReader`]（読み取り・書き込みの両方をこの 1 つのプリミティブ
    // 経由でのみ行う）へ通し、1 バイトずつ送る相手（slow-loris）でも
    // 接続全体の処理が `FIXTURE_IO_TIMEOUT` を超えないようにする。
    let deadline = Instant::now() + FIXTURE_IO_TIMEOUT;

    // 読み取り用（`BufReader` で包む）と書き込み用で、同じソケットの
    // 別ハンドル（`try_clone`）をそれぞれ独立した `DeadlineReader` として
    // 使う（`BufReader` に包むと内部の型へ書き込み目的で直接アクセスできない
    // ため）。どちらも同じ `deadline`（`Instant`。`Copy`）を共有する。
    let read_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(DeadlineReader::new(read_half, deadline));
    let mut writer = DeadlineReader::new(stream, deadline);

    let mut line = String::new();
    if let Err(e) = read_complete_line(&mut reader, &mut line) {
        respond_with_io_error(&mut writer, &e);
        return;
    }

    let Some((method, path)) = parse_http_request_line(&line) else {
        let _ = write_fixture_response(&mut writer, 400, "Bad Request", PLAIN_TEXT, b"", true);
        return;
    };
    // レビュー指摘（コーディネーター指示。PR #442 再々々々レビュー）:
    // GET 以外（HEAD を除く）は `405 Method Not Allowed` として拒否する。
    // HEAD はヘッダのみを返し（本文は書かない）、`Content-Length` は
    // GET したときと同じ値にする（HTTP のセマンティクス。実際に使うのは
    // `goto`（GET）のみだが、fixture サーバーとしての最小限の正しさを保つ）。
    let is_head = method == "HEAD";
    if method != "GET" && !is_head {
        let _ = write_fixture_response(
            &mut writer,
            405,
            "Method Not Allowed",
            PLAIN_TEXT,
            b"",
            true,
        );
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
    // 空行として扱う（一部クライアントは LF のみを送るため）。
    let mut header_lines_seen = 0u32;
    loop {
        if header_lines_seen >= FIXTURE_MAX_HEADER_LINES {
            let _ = write_fixture_response(&mut writer, 400, "Bad Request", PLAIN_TEXT, b"", true);
            return;
        }
        header_lines_seen += 1;
        let mut header_line = String::new();
        match read_complete_line(&mut reader, &mut header_line) {
            Ok(()) if header_line == "\r\n" || header_line == "\n" => break,
            Ok(()) => continue,
            Err(e) => {
                respond_with_io_error(&mut writer, &e);
                return;
            }
        }
    }

    match lookup_fixture(&path) {
        Some((content_type, content)) => {
            let _ = write_fixture_response(
                &mut writer,
                200,
                "OK",
                content_type,
                content.as_bytes(),
                !is_head,
            );
        }
        None => {
            let _ = write_fixture_response(&mut writer, 404, "Not Found", PLAIN_TEXT, b"", true);
        }
    }
}

/// 400/404/405/408 応答の `Content-Type`。fixture 本体は
/// `support::FIXTURE_TABLE` が個別に持つ content-type を使う
/// （レビュー指摘。PR #442 再々レビュー: パス → (content-type, 内容) の
/// 固定テーブルにする）。
const PLAIN_TEXT: &str = "text/plain; charset=utf-8";

/// `handle_fixture_connection` が使う最小限の HTTP/1.1 レスポンス書き込み。
///
/// `write_body` が `false`（`HEAD` リクエストへの応答）でも
/// `Content-Length` は `body.len()`（`GET` したときの実際の長さ）にする
/// （HTTP のセマンティクス）。`writer`（[`DeadlineReader`]）経由でのみ
/// 書き込むことで、接続の絶対期限を守る（レビュー指摘 P1。Codex。
/// PR #442 再々々々々々レビュー・competitor_lightpanda.rs:364）。
fn write_fixture_response(
    writer: &mut DeadlineReader,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    write_body: bool,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes())?;
    if write_body {
        writer.write_all(body)?;
    }
    writer.flush()
}

/// 起動する `Command` に「新しいプロセスグループ」を設定する。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々々レビュー・
/// competitor_lightpanda.rs:827）: 対象プロセス（ブラウザ）が起動した
/// 子孫プロセスが stdin パイプの読み取り側を継承していると、直接の子だけを
/// `kill` してもパイプが閉じずベンチ全体が止まり得る。プロセスグループ
/// 単位で起動しておき、タイムアウト時に [`kill_process_group`] でグループ
/// ごと終了させることで、子孫がパイプを保持し続ける経路を塞ぐ（#435 と
/// 同じ方針）。unix は `process_group(0)`（`setpgid(0,0)` 相当。std 1.64 で
/// 安定化。子プロセス自身を新しいプロセスグループのリーダーにする。
/// これによりプロセスグループ ID は子プロセスの PID と一致する）、
/// Windows は `CREATE_NEW_PROCESS_GROUP` を使う。いずれも `unsafe`・新規
/// 依存なしで `std::os::{unix,windows}::process::CommandExt` の安定 API
/// だけで実装する。
#[cfg(unix)]
fn apply_new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn apply_new_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // Win32 `CREATE_NEW_PROCESS_GROUP`（0x00000200）。`windows-sys` 等の
    // FFI 依存を新規に増やさないため、広く安定した定数値を直接埋め込む。
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// `pid` が属するプロセスグループ全体を強制終了する。
///
/// [`apply_new_process_group`] で子プロセス自身をグループリーダーにして
/// いるため（unix はグループ ID が `pid` と一致する）、グループへ
/// シグナルを送れば子孫プロセスも含めて終了できる。ただし std には
/// unix の `killpg`・Windows のプロセスツリー終了に相当する API が無く、
/// `unsafe`・新規依存（`libc`/`windows-sys` 等）も使えないため、実際の
/// 終了は OS 標準の外部コマンド（unix: `kill -KILL -- -<pid>`、Windows:
/// `taskkill /T /F`）に委ねる。これらのコマンドが使えない環境（spawn 自体の
/// 失敗）・非 0 終了（対象が既に存在しない等）では `Err` を返す。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々々々々々レビュー・
/// competitor_lightpanda.rs:558）: 以前は `Command::status()` の結果を
/// `let _ = ...` で握りつぶしており、外部コマンドの spawn 失敗・非 0
/// 終了に気づけなかった。`Result` にして呼び出し元へ返し、呼び出し元は
/// 失敗時に `Child::kill`（直接の子のみ）へのフォールバックを行った旨を
/// エラーメッセージへ含める（`Drop`（`ChildGuard::drop`）内で呼ぶ経路は
/// 戻り値を返せないため、そこに限り結果を握りつぶしてよい）。
#[cfg(unix)]
fn kill_process_group(pid: u32) -> Result<(), String> {
    // レビュー指摘 P1: `-<pid>`（プロセスグループ宛て）はハイフンで
    // 始まるため、`kill` 実装によってはオプションの一部と誤解釈され得る。
    // `--` でオプションの終端を明示し、後続の `-<pid>` を確実に引数として
    // 扱わせる。
    let status = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to spawn kill: {e}"))?;
    exit_status_to_result(status)
}

#[cfg(windows)]
fn kill_process_group(pid: u32) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to spawn taskkill: {e}"))?;
    exit_status_to_result(status)
}

/// 子プロセスを確実に終了させる guard。早期 `return`（`?`）経路でも
/// `Drop` で `kill` + `wait` する（`wait` を省くとゾンビプロセスが残る）。
/// [`kill_process_group`] も合わせて呼び、子孫プロセスが起動していても
/// 極力パイプを保持し続けないようにする（レビュー指摘 P1。PR #442
/// 再々々々々レビュー・competitor_lightpanda.rs:827）。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // `Drop` は戻り値を呼び出し元へ返せないため、ここに限り
        // `kill_process_group` の失敗を握りつぶしてよい（レビュー指摘 P1。
        // Codex。PR #442 再々々々々々々レビュー・
        // competitor_lightpanda.rs:558）。続く `Child::kill`（直接の子）は
        // 無条件に行う既存のフォールバックのままにする。
        let _ = kill_process_group(self.0.id());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 空きポートを 1 つ確保して返す（`127.0.0.1:0` に bind して即座に解放する）。
/// 固定ポートにすると並列実行される他ベンチ・他 issue の worktree と衝突するため
/// 使わない。
///
/// レビュー指摘 Medium（Cursor。PR #442 再々々々々レビュー・
/// competitor_lightpanda.rs:486-535）: リスナーを解放してから子プロセスを
/// 起動するまでの間に別プロセスが同じポートを奪える TOCTOU が残る
/// （bind して保持したまま子へ引き継ぐには、子プロセス側がソケット
/// 継承（`SO_REUSEPORT`・fd 引き渡し等）に対応している必要があり、対象は
/// 任意の外部バイナリのため前提にできない）。この関数自体では対処せず、
/// 呼び出し元 `spawn_and_wait_ready` 側で「ポートが空いているか」ではなく
/// 「応答しているのが自分の子プロセスらしいか」を検証する
/// （[`looks_like_browser_readiness_response`]・`Child::try_wait` 参照）。
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
///
/// レビュー指摘 Medium（Cursor。PR #442 再々々々々レビュー・
/// competitor_lightpanda.rs:486-535）: `reserve_port` の TOCTOU により、
/// 別プロセスが同じポートで先に応答し得る。ここでは (1) 毎回のポーリングで
/// `Child::try_wait` により子プロセスがまだ生きていることを確認し、
/// 既に終了していれば別プロセスの応答を拾う前に打ち切る、(2) 応答本文が
/// ブラウザらしい形（JSON オブジェクト）であることを
/// [`looks_like_browser_readiness_response`] で確認する、の 2 点を追加した。
/// 子プロセスが実際にそのポートを bind していることまでは確認できていない
/// （std だけでは OS 非依存にソケットの所有プロセスを調べる手段が無い。
/// `looks_like_browser_readiness_response` のドキュメント参照）。
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
    let mut command = Command::new(bin);
    apply_new_process_group(&mut command);
    let child = command
        .args(&expanded)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let mut guard = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    let request = format!(
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    loop {
        if Instant::now() >= deadline {
            return Err("timeout waiting for readiness probe".to_string());
        }
        // レビュー指摘 Medium（Cursor）: 子プロセスが既に終了しているのに
        // ポートへの応答（＝別プロセスの応答の可能性）だけを見て readiness
        // と誤認しないよう、まず生存を確認する。
        match guard.0.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("child exited before becoming ready: {status}"));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("try_wait failed: {e}")),
        }
        match probe_once(port, &request, deadline) {
            Some((status, body))
                if (200..300).contains(&status) && looks_like_browser_readiness_response(&body) =>
            {
                return Ok((guard, port, start.elapsed()));
            }
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

/// readiness probe の応答本文に許す上限バイト数。相手プロセス（別プロセスの
/// 誤応答も含む）が不正・大量の応答を返し続けても無制限に確保しないための
/// 上限（coding-rust.md「長さ・件数を上限検証」）。
const PROBE_BODY_MAX_BYTES: usize = 64 * 1024;

/// readiness probe 1 回あたりに許す時間。`outer_deadline`（呼び出し元の
/// readiness 全体の期限）の残り時間がこれより短ければ、そちらを優先する
/// （レビュー指摘: 「probe の期限は外側の readiness 期限の残り時間を
/// 超えないようにする」）。
const PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(200);

/// readiness probe を 1 回だけ試す。ステータス行と本文を返す。接続失敗・
/// 応答不正（不正な UTF-8 を含む）・タイムアウトはいずれも `None`
/// （呼び出し側がポーリングを継続する。panic させない）。
///
/// レビュー指摘 Medium（Cursor。PR #442 再々々々々レビュー・
/// competitor_lightpanda.rs:486-535）: 以前はステータス行のみを見ており、
/// ポート再利用で別プロセスが応答してもステータスが 2xx なら readiness と
/// 誤認し得た。本文まで読み、呼び出し元が
/// [`looks_like_browser_readiness_response`] で検証できるようにする。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
/// competitor_lightpanda.rs:682/711）: この probe 1 回の読み取りは
/// [`DeadlineReader`] を通し、`outer_deadline` を超えない絶対期限
/// （`PROBE_ATTEMPT_TIMEOUT` とどちらか短い方）で統一的に区切る。
/// `Content-Length` 分を読み切る前に EOF・エラー（タイムアウトを含む）が
/// 起きた場合は、読めた断片を [`looks_like_browser_readiness_response`]
/// に渡さず probe 失敗（`None`）として扱う（以前は `break` して部分的な
/// 本文をそのまま検証に回していた）。`Content-Length` が無い場合も、
/// 読み取りエラー（タイムアウトを含む）は失敗として扱い、正常な EOF
/// （`Ok(0)`）だけを本文終端とみなす。
fn probe_once(port: u16, request: &str, outer_deadline: Instant) -> Option<(u16, String)> {
    let probe_timeout =
        PROBE_ATTEMPT_TIMEOUT.min(outer_deadline.saturating_duration_since(Instant::now()));
    if probe_timeout.is_zero() {
        return None;
    }
    let deadline = Instant::now() + probe_timeout;

    let stream =
        TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().ok()?, probe_timeout)
            .ok()?;
    let mut deadline_stream = DeadlineReader::new(stream, deadline);
    // 書き込み・読み取りの両方を同じ `DeadlineReader`（`Read`・`Write` の
    // 両方を実装する）経由で行う。書き込み後、そのまま `BufReader` に
    // 包んで読み取りへ移る（`BufReader` はどんな `Read` 実装も受け付ける
    // ため、内部の `TcpStream` を取り出し直す必要が無い）。
    deadline_stream.write_all(request.as_bytes()).ok()?;
    let mut reader = BufReader::new(deadline_stream);
    let mut status_line = String::new();
    read_line_bounded(&mut reader, &mut status_line).ok()?;
    let status = parse_http_status(&status_line)?;
    // ヘッダ行を空行（終端）まで読み飛ばし、`Content-Length` があれば覚えて
    // おく。読み取りが完了しなかった（タイムアウト・接続断・不正な行）
    // 場合は、本文の検証まで進めないため `None`（呼び出し元はポーリングを
    // 継続する）。
    //
    // レビュー指摘（advisor。PR #442 再々々々々レビュー後の追加指摘）:
    // 行数に上限が無いと、ポートを奪った別プロセスがヘッダ行を送り続ける
    // ことで `probe_once` を長時間占有し得る（`fixture_accept_loop` の
    // `FIXTURE_MAX_HEADER_LINES` と同じ理由）。同じ上限を再利用する。
    let mut content_length: Option<usize> = None;
    let mut header_lines_seen = 0u32;
    loop {
        if header_lines_seen >= FIXTURE_MAX_HEADER_LINES {
            return None;
        }
        header_lines_seen += 1;
        let mut header_line = String::new();
        read_complete_line(&mut reader, &mut header_line).ok()?;
        if header_line == "\r\n" || header_line == "\n" {
            break;
        }
        if let Some(value) = header_line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            // レビュー指摘 P2（コーディネーター指示。PR #442
            // 再々々々々々々レビュー）: `Content-Length` ヘッダー自体は
            // 存在するのに値が不正（非数値・空・符号付き等。
            // `support::parse_content_length` 参照）な場合、以前は
            // `.ok()` で握りつぶして「長さ指定なし」（EOF まで読む
            // フォールバック）扱いにしていた。ヘッダーが実在するのに
            // その値を無視するのは、相手の応答を正しく解釈できていない
            // ことを意味するため、ここで probe 失敗にする（フォール
            // バックしない）。同様に、同じヘッダーが複数回現れて値が
            // 食い違う場合（重複して矛盾する `Content-Length`）も、
            // どちらの値を信じるべきか判断できないため probe 失敗にする
            // （同一値の重複は許容する）。
            let parsed = parse_content_length(value)?;
            match content_length {
                Some(existing) if existing != parsed => return None,
                _ => content_length = Some(parsed),
            }
        }
    }
    // レビュー指摘（advisor。PR #442 再々々々々レビュー後の追加指摘）:
    // `Content-Length` を無視して常に EOF まで（＝読み取りタイムアウトまで）
    // 読んでいたため、相手が `Connection: close` を要求されても接続を
    // 保持し続ける HTTP/1.1 実装だと、毎回の readiness probe に
    // タイムアウト分の遅延が系統的に乗り、cold start（`PERF-3`）の
    // 計測値を実態より水増しし得た。`Content-Length` が分かればちょうど
    // その長さだけ読み、無ければ（ヘッダに含まれない応答向けの
    // フォールバックとして）以前と同じ EOF までの読み取りに戻す。
    //
    // レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
    // competitor_lightpanda.rs:711）: `Content-Length` 分を読み切る前に
    // EOF・エラーが起きた場合は `break` で打ち切って部分的な本文を
    // そのまま使っていた（本文が途中で切れた応答を成功として扱い得た）。
    // ここではどちらも probe 失敗（`None`）にする。
    //
    // レビュー指摘 P2（Codex。PR #442 再々々々々々々レビュー・
    // competitor_lightpanda.rs:756）: `Content-Length` が
    // `PROBE_BODY_MAX_BYTES` を超える場合、以前は `len.min(...)` で
    // 上限まで黙って切り詰め、その範囲だけ読めれば成功として扱っていた
    // （＝サーバーが実際に宣言した長さの応答を確認しないまま probe
    // 成功と判定し得た）。ここでは `len` が上限を超えた時点で読み取りを
    // 試みず即座に probe 失敗（`None`）にする。`Content-Length` が無い
    // 分岐（EOF まで読む）でも、読めたバイト数が上限を超えたら「切り詰めて
    // 使う」のではなく probe 失敗にする（同種の「上限で黙って切り詰めて
    // 成功扱い」をここでも避ける）。
    if content_length_exceeds_limit(content_length, PROBE_BODY_MAX_BYTES) {
        return None;
    }
    let mut body = Vec::new();
    match content_length {
        Some(len) => {
            // `Read::read_exact` は指定したバッファをちょうど埋め切る前に
            // EOF に達すると `ErrorKind::UnexpectedEof` を返す（部分的に
            // 読めた分をそのまま `Ok` として返すことはない）ため、宣言された
            // `Content-Length` に届く前に応答が途中で切れたケースを
            // 取りこぼさず失敗にできる。読み取りエラー（`DeadlineReader`
            // 経由のタイムアウトを含む）も同様に `Err` になる。
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).ok()?;
            body = buf;
        }
        None => {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    // 正常な EOF（`Ok(0)`）だけを本文終端とみなす。
                    Ok(0) => break,
                    Ok(n) => {
                        body.extend_from_slice(&buf[..n]);
                        if body.len() > PROBE_BODY_MAX_BYTES {
                            // 上限超過分をそのまま「読めた分だけ」の
                            // 成功として使わず、probe 失敗にする。
                            return None;
                        }
                    }
                    // タイムアウトを含む読み取りエラーは失敗として扱う
                    // （以前は `break` して、それまでに読めた断片を
                    // そのまま使っていた）。
                    Err(_) => return None,
                }
            }
        }
    }
    // レビュー指摘 P2（Codex。PR #442 再々々々レビュー・
    // competitor_lightpanda.rs:585）と同じ理由付けで、本文も厳密な UTF-8
    // 変換にする（`from_utf8_lossy` による文字化けが偶然妥当な JSON へ
    // 変わり、別プロセスの応答をブラウザらしいと誤判定する経路を避ける）。
    let body_str = String::from_utf8(body).ok()?;
    Some((status, body_str))
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
///
/// レビュー指摘 P2（Codex。PR #442 再々々々々レビュー・
/// competitor_lightpanda.rs:585）: 以前は `String::from_utf8_lossy` で
/// 不正なバイト列を置換文字（U+FFFD）へ書き換えていたため、置換後の
/// 文字列がたまたま妥当な JSON になると、元の応答とは異なる内容を
/// 正常なトークン削減率として出力し得た。`String::from_utf8` で厳密に
/// 変換し、失敗したら `ErrorKind::InvalidData` で計測エラーにする
/// （呼び出し元は他の `Err` 経路と同様に扱えばよく、個別対応は不要）。
///
/// 契約（レビュー指摘 P1。Codex。PR #442 再々々々々々レビュー・
/// competitor_lightpanda.rs:1039）: 「改行で終わる行を 1 本読めた」場合
/// だけ `Ok(len)`（`len > 0`）を返す。ストリームが 1 バイトも読まずに
/// 終端した場合（前の呼び出しまでに完結した行を読み終え、次の行が
/// 存在しないことを示す正常な終端）だけ `Ok(0)` を返す。それ以外
/// （1 バイト以上読んだが改行に達する前に EOF に達した = 行が途中で
/// 切れた）は `ErrorKind::UnexpectedEof` で `Err` にする。以前はこの
/// ケースを `Ok(len)`（改行なしの断片をそのまま返す）としていたため、
/// この関数を直接呼ぶ MCP stdout 読み取りスレッドが、途中で切れた行を
/// 完全な応答として読み取りキューへ送り得た（`read_complete_line` を
/// 経由する呼び出し元――fixture サーバー・probe――は既にこの区別を
/// 呼び出し側で行っていたが、直接呼ぶ呼び出し元と食い違っていた）。
/// この契約をここ 1 箇所に集約することで、fixture サーバー・probe・MCP
/// のすべての呼び出し元で一貫させる。
fn read_line_bounded<R: BufRead>(reader: &mut R, out: &mut String) -> std::io::Result<usize> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if reader.read(&mut byte)? == 0 {
            if buf.is_empty() {
                // 前の行までで完結しており、次の行が無いままストリームが
                // 正常終端した（呼び出し元にとって「もう読むものが無い」
                // ことを示す）。
                *out = String::new();
                return Ok(0);
            }
            // 1 バイト以上読んだが改行に達する前に接続が切れた。
            // 「行が完結した」とはみなさず明示的に失敗させる。
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stream ended before the line was terminated by a newline",
            ));
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
    let len = buf.len();
    match String::from_utf8(buf) {
        Ok(s) => {
            *out = s;
            Ok(len)
        }
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("line contains invalid UTF-8: {e}"),
        )),
    }
}

/// cold start（`PERF-3`）: `trials` 回起動し、readiness までの時間の中央値（ms）を返す。
fn measure_cold_start(target: &Target, trials: usize) -> Outcome {
    let bin = match require_bin(target.bin.as_ref(), target.name) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    // cold start は `serve_args` だけを使うため、`mcp_args_error` は無視する
    // （レビュー指摘 P2。Codex。PR #442 再々々々レビュー）。
    if let Some(outcome) = arg_error_gate(target.serve_args_error.as_deref(), target.name) {
        return outcome;
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
    // レビュー指摘 P2（Codex。PR #442 再々々々々レビュー・
    // competitor_lightpanda.rs:585）: 他の `from_utf8_lossy` 使用箇所も
    // 確認した。ここは `parse_ps_rss_kb`（`out.trim().parse::<u64>()`）で
    // 数字列としてのみ解釈するため、`from_utf8_lossy` の置換文字（U+FFFD）
    // が混入しても `parse::<u64>()` が失敗して `None` になるだけで、
    // 「文字化けが偶然別の妥当な値に化ける」経路にはならない（`read_line_bounded`
    // の JSON 応答のように、置換後の文字列が別の意味を持つ妥当な値として
    // 解釈され得るケースとは異なる）。そのため、ここは厳密な UTF-8 変換に
    // 変える必要はないと判断した。
    parse_ps_rss_kb(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(unix)]
fn measure_idle_rss(target: &Target, trials: usize) -> Outcome {
    let bin = match require_bin(target.bin.as_ref(), target.name) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    // アイドル RSS も `serve_args` だけを使うため、`mcp_args_error` は
    // 無視する（レビュー指摘 P2。Codex。PR #442 再々々々レビュー）。
    if let Some(outcome) = arg_error_gate(target.serve_args_error.as_deref(), target.name) {
        return outcome;
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
    // レビュー指摘 P1（Codex。PR #442 再々々レビュー・
    // competitor_lightpanda.rs:593）: `target.bin` を確認せず常に
    // `Outcome::Unsupported` を返していたため、`<PREFIX>_BIN` が未設定でも
    // `idleRssKb` だけ `unsupported` になり「対象バイナリ未設定なら全計測が
    // Skipped」という契約に反していた。他の計測関数（`measure_cold_start`・
    // `measure_binary_size`・unix 版 `measure_idle_rss`）と同じ判定を
    // `require_bin`（support.rs。OS に依存しない純粋関数）で先に行う。
    if let Err(skipped) = require_bin(target.bin.as_ref(), target.name) {
        return skipped;
    }
    Outcome::Unsupported(format!(
        "{}: idle RSS measurement uses unix `ps`, unsupported on Windows (PERF-6)",
        target.name
    ))
}

/// バイナリサイズ（`PERF-1`）: 対象実行ファイルの `fs::metadata` によるバイト数。
fn measure_binary_size(target: &Target) -> Outcome {
    let bin = match require_bin(target.bin.as_ref(), target.name) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
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
    /// `Arc<Mutex<_>>` にする理由（レビュー指摘 P1。Codex。PR #442
    /// 再々々々々レビュー・competitor_lightpanda.rs:827）: `write_with_deadline`
    /// はタイムアウト時に書き込みスレッドを detach し（`join` しない）、
    /// 以後の呼び出しでも同じ `ChildStdin` を再利用できるようにするため、
    /// 所有権を一方的に奪う（`&mut ChildStdin`/`Option::take`）方式ではなく
    /// 共有できる `Arc<Mutex<ChildStdin>>` にする。detach したスレッドが
    /// 書き込みを続けている間に次の呼び出しが来ても、ロック待ちで
    /// `recv_timeout` が正しくタイムアウトする（無期限に隠れてブロックしない）。
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    rx: mpsc::Receiver<String>,
    next_id: u64,
    /// 読み取りスレッドがキュー容量超過を検知した際に立てるフラグ。
    /// `call` はこれを見て、無制限にキューを溜め込む代わりに
    /// 計測をエラー終了させる。
    overflowed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 読み取りスレッドが `read_line_bounded` からエラー（`InvalidData`
    /// （`MAX_LINE_BYTES` 到達・改行未検出、または不正な UTF-8。レビュー
    /// 指摘 P1・P2）・`UnexpectedEof`（改行に達する前に子プロセスが標準
    /// 出力を閉じた。レビュー指摘 P1。Codex。PR #442 再々々々々々レビュー・
    /// competitor_lightpanda.rs:1039）を含む、あらゆる種別）を受け取った際に
    /// その内容を記録するスロット。`call` はこれを見て、切り詰められた／
    /// 文字化けした／未完了の行を無視したまま待ち続けるのではなく明示的に
    /// エラー終了させる。
    ///
    /// レビュー指摘 P2（Codex。PR #442 再々々々々々々々レビュー・
    /// competitor_lightpanda.rs:1147）: 以前は `AtomicBool` で「エラーが
    /// あったかどうか」だけを記録し、`InvalidData`・`UnexpectedEof` 以外の
    /// I/O エラー種別（`ConnectionReset` 等）は無条件の `Err(_) => break`
    /// に落ちて記録されずスレッドが静かに終了していた。`Mutex<Option<String>>`
    /// にして、どの種別のエラーであっても実際のメッセージを記録するように
    /// した。
    line_read_error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl McpClient {
    fn spawn(bin: &PathBuf, args: &[String]) -> Result<Self, String> {
        let mut command = Command::new(bin);
        apply_new_process_group(&mut command);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or("no stdin")?));
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::sync_channel::<String>(MCP_STDOUT_QUEUE_CAPACITY);
        let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflowed_writer = std::sync::Arc::clone(&overflowed);
        let line_read_error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let line_read_error_writer = std::sync::Arc::clone(&line_read_error);
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
                    Err(e) => {
                        // `MAX_LINE_BYTES` に達し改行未検出のまま打ち切られた行
                        // （レビュー指摘 P1）、不正な UTF-8（レビュー指摘 P2。
                        // Codex。PR #442 再々々々々レビュー・
                        // competitor_lightpanda.rs:585: `from_utf8_lossy` の
                        // 置換で偶然妥当な JSON に化ける経路を避けるため、
                        // `read_line_bounded` を厳密な UTF-8 変換にした）、
                        // 改行に達する前に子プロセスが標準出力を閉じた
                        // （`UnexpectedEof`。レビュー指摘 P1。Codex。PR #442
                        // 再々々々々々レビュー・competitor_lightpanda.rs:1039）、
                        // またはそれ以外の I/O エラー（`ConnectionReset` 等。
                        // レビュー指摘 P2。Codex。PR #442 再々々々々々々々
                        // レビュー・competitor_lightpanda.rs:1147: 以前は
                        // 種別を絞った `if` ガード付きの分岐でしか記録して
                        // おらず、それ以外の種別は無条件の `Err(_) => break`
                        // に落ちて記録されずスレッドが静かに終了していた）。
                        // 種別を問わずすべてのエラーを記録し、切り詰められた／
                        // 文字化けした／未完了の断片を正常応答として扱わせず、
                        // 読み取りを止めて `call` 側へ明示的に伝える。
                        if let Ok(mut reason) = line_read_error_writer.lock() {
                            *reason = Some(e.to_string());
                        }
                        break;
                    }
                }
            }
            // レビュー指摘 P2（Codex。PR #442 再々々々々々々々レビュー・
            // competitor_lightpanda.rs:1147）: 「reader スレッドが終了
            // したら（EOF の場合も含めて）、送信側を drop する」ことを
            // 明示する。`tx` はこのクロージャに move 済みのローカル変数
            // なので、`break` でループを抜けクロージャが終了する時点で
            // 自動的に drop される（Rust の通常のスコープ規則）。これに
            // より `call`/`notify` 側の `self.rx.recv_timeout` は
            // 待機中でも即座に `RecvTimeoutError::Disconnected` で
            // 返るようになる。
        });
        Ok(Self {
            guard: ChildGuard(child),
            stdin,
            rx,
            next_id: 1,
            overflowed,
            line_read_error,
        })
    }

    /// `line_read_error` に読み取りスレッドが記録したエラー内容があれば
    /// 取り出す（`Mutex` のロックが取得できない＝毒された場合は、記録が
    /// 取れなかったものとして `None` を返す。呼び出し元は他の経路
    /// （`overflowed`・`recv_timeout` のタイムアウト等）で fail-closed に
    /// 倒れるため、ここでの panic は避ける）。
    fn line_read_error_reason(
        line_read_error: &std::sync::Mutex<Option<String>>,
    ) -> Option<String> {
        line_read_error.lock().ok().and_then(|guard| guard.clone())
    }

    /// 書き込み中に相手プロセスが標準入力を読まずパイプが満杯になっても
    /// 無期限にブロックしないよう、`deadline` までに `write_all` + `flush` が
    /// 終わらなければタイムアウトとして扱う。子プロセスの標準入力
    /// （パイプ）には OS レベルの書き込みタイムアウトが無いため、実際の
    /// 書き込みは別スレッドへ切り出し、`mpsc` の `recv_timeout` で待つ。
    ///
    /// レビュー指摘 P1（Codex。PR #442 再々々々々レビュー・
    /// competitor_lightpanda.rs:827）: 以前は `std::thread::scope` を使い、
    /// タイムアウト時に直接の子プロセスだけを `kill` したあと、書き込み
    /// スレッドからの送信を `rx.recv()`（期限なし）で待っていた。対象
    /// プロセス（ブラウザ）が起動した子孫プロセスが stdin パイプの
    /// 読み取り側を継承していると、直接の子を終了してもパイプが閉じず、
    /// `write_all` が解放されないままベンチ全体が無期限に止まり得た
    /// （`thread::scope` はスコープ終了時に生成した全スレッドの join を
    /// 待つ仕組みのため、この `rx.recv()` を省いても結局スコープを抜ける
    /// ところで同じだけ待たされる）。
    ///
    /// 対処: (1) [`apply_new_process_group`] で子プロセスをプロセスグループの
    /// リーダーとして起動しておき、タイムアウト時は [`kill_process_group`]
    /// でグループ全体（子孫を含む）の終了を試み、直接の子は std の
    /// `Child::kill`（外部コマンドに依存しない、確実に効く床）でも
    /// 終了させる。`kill_process_group` は外部コマンド依存で、環境によっては
    /// 効かない（子孫が生き残る）ことがあり得るため、`Child::kill` を
    /// 省略しない（そのドキュメント参照。この 2 段構えにより、少なくとも
    /// 直接の子プロセスは確実に終了する）。(2) 書き込みは `thread::scope`
    /// ではなく `'static` な `thread::spawn`（detach 可能）へ切り替え、
    /// タイムアウト時はスレッドの終了を待たずに `Err` を返す。パイプが
    /// 閉じれば detach したスレッドの `write_all` も `BrokenPipe` 等で
    /// 自然に終了するが、閉じ切らない環境ではそのスレッドがプロセス終了
    /// までリークし得る。`stdin` を `Arc<Mutex<_>>` で共有するのは、
    /// detach したスレッドが書き込みを続けていても、次回の呼び出しが
    /// ロック取得待ちで正しく `recv_timeout` によりタイムアウトできる
    /// ようにするため（`McpClient::stdin` のドキュメント参照）。
    fn write_with_deadline(
        stdin: &Arc<Mutex<std::process::ChildStdin>>,
        child: &mut Child,
        data: Vec<u8>,
        deadline: Instant,
    ) -> Result<(), String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("write timed out before starting (deadline already elapsed)".to_string());
        }
        let (tx, rx) = mpsc::channel();
        let stdin_for_thread = Arc::clone(stdin);
        std::thread::spawn(move || {
            let mut guard = match stdin_for_thread.lock() {
                Ok(guard) => guard,
                // 他スレッド（前回タイムアウトした detach 済みスレッド）が
                // panic しつつロックを保持したまま終了した場合。書き込み
                // 自体は続行を試みる（fail-closed に倒すよりは、通常経路の
                // 継続を優先する）。
                Err(poisoned) => poisoned.into_inner(),
            };
            let result = guard.write_all(&data).and_then(|()| guard.flush());
            let _ = tx.send(result);
        });
        match rx.recv_timeout(remaining) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("write failed: {e}")),
            Err(_) => {
                // `kill_process_group` は外部コマンド（`kill`/`taskkill`）に
                // 依存し、実行環境によっては失敗し得る（ドキュメント参照）。
                // それだけに頼らず、std の `Child::kill`（直接の子）も必ず
                // 呼ぶ。子孫プロセスがパイプを保持していると直接の子だけの
                // `kill` では解放されないが、少なくとも直接の子は確実に
                // 終了させる床として残す（レビュー指摘。PR #442
                // 再々々々々レビュー・advisor 追加指摘）。
                //
                // レビュー指摘 P1（Codex。PR #442 再々々々々々々レビュー・
                // competitor_lightpanda.rs:558）: `kill_process_group` の
                // 結果を握りつぶさず、失敗時は `Child::kill` へ
                // フォールバックした旨をエラーメッセージへ含めて呼び出し元
                // （`call`/`notify`）へ返す。
                let group_kill_result = kill_process_group(child.id());
                let _ = child.kill();
                match group_kill_result {
                    Ok(()) => Err("write timed out, process group killed".to_string()),
                    Err(e) => Err(format!(
                        "write timed out; failed to kill process group ({e}), fell back to killing the direct child process only"
                    )),
                }
            }
        }
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
            &self.stdin,
            &mut self.guard.0,
            request.into_bytes(),
            deadline,
        )
        .map_err(|e| format!("{method}: {e}"))?;

        loop {
            if self.overflowed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(format!(
                    "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_CAPACITY} lines, aborting"
                ));
            }
            if let Some(reason) = Self::line_read_error_reason(&self.line_read_error) {
                return Err(format!(
                    "{method}: mcp response line could not be decoded ({reason}), aborting"
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timeout waiting for response to {method}"));
            }
            // レビュー指摘 P2（Codex。PR #442 再々々々々々々々レビュー・
            // competitor_lightpanda.rs:1147）: 読み取りスレッドが終了すると
            // `tx`（送信側）が drop され、`recv_timeout` は待機中でも即座に
            // `RecvTimeoutError::Disconnected` で返る。以前はこれを
            // `Timeout` と区別せず一括りに扱っていたため、実際には切断
            // （エラー・EOF）が起きているのに「timeout waiting」という
            // 紛らわしいメッセージになり得た。ここで明示的に区別し、
            // 「channel の切断を検知して即時に返す」ことを保証する。
            let line = match self.rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if self.overflowed.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(format!(
                            "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_CAPACITY} lines, aborting"
                        ));
                    }
                    if let Some(reason) = Self::line_read_error_reason(&self.line_read_error) {
                        return Err(format!(
                            "{method}: mcp response line could not be decoded ({reason}), aborting"
                        ));
                    }
                    // 読み取りスレッドが `Ok(0)`（正常な EOF。エラーではない）
                    // で終了した場合はここへ来る。
                    return Err(format!(
                        "{method}: mcp stdout closed before a response with matching id was received"
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.overflowed.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(format!(
                            "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_CAPACITY} lines, aborting"
                        ));
                    }
                    if let Some(reason) = Self::line_read_error_reason(&self.line_read_error) {
                        return Err(format!(
                            "{method}: mcp response line could not be decoded ({reason}), aborting"
                        ));
                    }
                    return Err(format!("timeout waiting for response to {method}"));
                }
            };
            let Ok(value) = parse_json(line.trim()) else {
                continue;
            };
            // id が一致しない応答（他の呼び出しの応答等）は読み捨てて
            // 次の行を待つ。
            if value.get("id").and_then(JsonValue::as_f64) != Some(id as f64) {
                continue;
            }
            // レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
            // competitor_lightpanda.rs:839）: `result` が `null`・`{}` でも
            // `isError` さえ立っていなければ成功として扱っていたため、
            // `goto` のように本文を確認しない呼び出しでは、ページ遷移に
            // 失敗した応答をそのまま成功と誤判定し得た（続く `html`・
            // `tree` が別ページの結果として計測される）。JSON-RPC 2.0 の
            // 封筒（`jsonrpc`・`id`・`error`/`result` の排他）と、
            // `method` ごとに要求される `result` の最小限の形
            // （`tools/call` は空でない `content` 配列・各要素の `type`・
            // `isError` の型など）を `validate_mcp_response`（support.rs。
            // 純粋関数）で検証してから、`error`/`isError` の実際の失敗判定へ
            // 進む。
            if let Err(reason) = validate_mcp_response(&value, id as f64, method) {
                return Err(format!("{method}: invalid response: {reason}"));
            }
            if let Some(err) = value.get("error") {
                let message = err
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown error");
                return Err(format!("{method}: rpc error: {message}"));
            }
            // `validate_mcp_response` が「`error`/`result` のどちらか一方が
            // 存在する」ことを確認済みであり、上で `error` 分岐を処理した
            // ため、ここに到達する時点で `result` は必ず存在する。
            let Some(result) = value.get("result") else {
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

    /// 応答を待たない通知（`initialize` 後の `notifications/initialized` 等）。
    /// 応答を待たないだけで、書き込み自体には `call` 同様
    /// `write_with_deadline`（レビュー指摘 P1（PR #442））で
    /// `NOTIFY_WRITE_TIMEOUT` を適用し、相手プロセスが標準入力を読まない
    /// 場合の無期限ブロックを避ける。
    fn notify(&mut self, method: &str, params: &str) -> Result<(), String> {
        let line = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{params}}}\n");
        let deadline = Instant::now() + NOTIFY_WRITE_TIMEOUT;
        Self::write_with_deadline(&self.stdin, &mut self.guard.0, line.into_bytes(), deadline)
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
    // `require_bin`（support.rs）で他の計測関数と同じ判定に揃える
    // （レビュー指摘。コーディネーター指示。PR #442 再々々々レビュー:
    // 「cfg で分かれている計測関数に同じ食い違いがないか確認する」の
    // 一環で洗い出した、`require_bin` に未集約だった最後の 1 箇所）。
    // `main` 側の `token_reduction_gate` が既に同じ判定を行った後で
    // しかこの関数を呼ばないため実質到達しないが、単独呼び出しでも
    // 安全なようにここでも確認する。
    let bin = match require_bin(target.bin.as_ref(), target.name) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    // `AISNAP-1` は `mcp_args` だけを使うため、`serve_args_error` は無視する
    // （レビュー指摘 P2。Codex。PR #442 再々々々レビュー）。
    if let Some(outcome) = arg_error_gate(target.mcp_args_error.as_deref(), target.name) {
        return outcome;
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
            target.bin.as_ref(),
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
