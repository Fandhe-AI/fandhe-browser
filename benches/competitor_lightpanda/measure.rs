//! `competitor_lightpanda` の計測ロジック本体（fixture サーバー・readiness
//! probe・cold start / idle RSS / MCP トークン削減率の各計測・結果の JSON 化・
//! 終了コード判定）。
//!
//! AGENTS.md「ユニットテストと
//! 結合テストの併置」に基づき、fixture サーバーの起動から対象プロセスとの
//! 通信・計測結果・終了コードまでを対象バイナリを模したローカルプロセスで
//! 通す結合テスト（`measure_tests.rs`）を追加するにあたり、計測ロジックを
//! `[[bench]]` ターゲット（`../competitor_lightpanda.rs`。`cargo test` の対象
//! 外）から `#[path]` で取り込める本モジュールへ切り出した。`../competitor_lightpanda.rs`
//! の `main` は環境変数の解析・[`run_all`] の呼び出し・出力だけを行う薄い
//! ラッパーになる。
//!
//! 対応ビヘイビア: バイナリサイズ計測が使う `PERF-1`、cold start 計測が使う
//! `PERF-3`、アイドル RSS 計測が使う `PERF-6`、MCP トークン削減率計測が使う
//! `AISNAP-1`（TASK-84（84.1）・Issue #211）。
//!
//! このファイルは `[[bench]]`（`competitor_lightpanda`）・`[[test]]`
//! （`competitor_lightpanda_measure`。`harness = false`）の両方の crate root から
//! `#[path]` で `mod measure;` として取り込まれる。どちらの root でも `--cfg test`
//! 付きでコンパイルされる（`[[test]]` 側は `harness = false` のため通常の
//! bin 同様 `main` を持つ）が、`--test` は渡されないため `#[test]` 属性の
//! 有無に関わらず「両方の root から実際に呼ばれる」経路だけが dead_code
//! 警告を免れる。そのため本モジュールの公開項目はすべて [`run_all`]
//! （両方の root の `main` が呼ぶ）経由で、または結合テストが直接呼ぶ
//! 関数（[`measure_cold_start`]・[`measure_token_reduction`]。テスト側の
//! 個別ケースが `run_all` 全体を待たずに単一の計測だけを検証するために
//! 直接呼ぶ）として到達可能にしてある。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::support::{
    DeadlineReader, JsonValue, Outcome, apply_new_process_group, approx_tokens, arg_error_gate,
    bench_exit_code, content_length_exceeds_limit, expand_args, fixture_kind,
    http_status_for_io_error, json_escape, kill_process_group,
    looks_like_browser_readiness_response, lookup_fixture, median, parse_content_length,
    parse_http_request_line, parse_http_status, parse_json, port_conflict_error, reduction_pct,
    require_bin, token_reduction_gate, validate_mcp_response, wait_with_deadline,
};
// `parse_pgrep_pids` は unix 専用の `pgrep_children`（下記 `#[cfg(unix)]`）が
// 呼ぶ。`parse_tasklist_mem_kb` は windows 専用の `sample_rss_kb`
// （下記 `#[cfg(windows)]`）が呼ぶ。無条件 import のままだと反対側の OS
// ビルドで未使用になり `unused_imports` 警告が `-D warnings`
// （ci.md「3 OS CI」）で fail するため cfg で分離する
// （TASK-84.2・TASK-84.5・PERF-6・Issue #212）。
#[cfg(unix)]
use crate::support::parse_pgrep_pids;
#[cfg(windows)]
use crate::support::parse_tasklist_mem_kb;

/// fixture 配信サーバーの各種上限（coding-rust.md「長さ・件数を上限検証」）。
/// ローカル専用のベンチ補助サーバーだが、想定外の大量・低速接続で
/// スレッドが専有され続けないよう防御的に上限を設ける
/// （security.md「不安全な設計」）。リクエスト行・ヘッダ行のサイズ上限は
/// 既存の `MAX_LINE_BYTES`（`read_line_bounded` が使う）を共用する。
/// fixture 本体はコンパイル時に埋め込み済み（`support::FIXTURE_TABLE`）で
/// 実行時のファイル読み込みがないため、ファイルサイズの上限は不要になった。
const FIXTURE_MAX_CONNECTIONS: u64 = 10_000;
/// 読み捨てるヘッダ行の総数上限。無制限だと大量のヘッダ行を送るクライアント
/// がサーバースレッドを専有し続け得る。実ブラウザが送る一般的なヘッダ数は数十件に
/// 収まるため、大きめに倍取って `64` とする。
// `pub(crate)`: 結合テスト（`measure_tests.rs`）がヘッダ行数上限を超える
// リクエストを実際に送るテストで、上限値そのものを参照する。
pub(crate) const FIXTURE_MAX_HEADER_LINES: u32 = 64;
// `pub(crate)`: 結合テスト（`measure_tests.rs`）が slow-loris テストの
// 送信間隔・アサーション上限を、この値を基準に決めるために参照する。
pub(crate) const FIXTURE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const FIXTURE_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// readiness probe・MCP 呼び出しそれぞれの外部入力（応答行）に許す最大長。
/// 相手プロセスが不正・悪意ある出力を送り続けても無制限にバッファへ
/// 蓄積しないための上限（coding-rust.md）。
///
/// `pub(crate)`: 結合テスト（`measure_tests.rs`）が `read_line_bounded` の
/// 上限超過ケースを検証する際に参照する。
pub(crate) const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 計測対象 1 つ分の設定（Lightpanda / fandhe-browser）。
///
/// `bin` が `None` のときはバイナリ未提供として全計測を `Outcome::Skipped` にする
/// （`fandhe-browser-cli` は TASK-41.5 未着手のため、本 PR 時点では常にこの経路）。
pub struct Target {
    pub name: &'static str,
    pub bin: Option<PathBuf>,
    /// `<PREFIX>_BIN` が設定されているが、既存の通常ファイルとして解決
    /// できなかった場合の理由。`Some` のときは `bin` を常に `None` にし
    /// （未設定と同じ「起動を試みない」経路を通すが、`Outcome::Skipped` では
    /// なく `Outcome::Error` にするため `bin` だけでなくこの理由も見る
    /// 必要がある）、[`require_bin`] が全計測を即座に `Outcome::Error` へ
    /// 倒す。`bin` の解決は [`Target`] 構築時（環境変数からは 1 回だけ）に
    /// 行い、以後の起動（`Command::new`）・サイズ計測（`fs::metadata`）は
    /// 同じ検証済みパスを再利用する（相対パス・PATH 検索の有無による
    /// 起動先とサイズ計測先の食い違いを防ぐ）。
    pub bin_error: Option<String>,
    pub serve_args: Vec<String>,
    pub mcp_args: Vec<String>,
    /// `serve_args` の元となった `<PREFIX>_SERVE_ARGS` がサイズ・個数上限を
    /// 超えていた場合の理由。`Some` のときは `serve_args` を使う計測
    /// （cold start・アイドル RSS）だけが起動を試みず即座に `Outcome::Error`
    /// を返す。
    pub serve_args_error: Option<String>,
    /// `mcp_args` の元となった `<PREFIX>_MCP_ARGS` がサイズ・個数上限を
    /// 超えていた場合の理由。`Some` のときは `mcp_args` を使う計測
    /// （`AISNAP-1` トークン削減率）だけが起動を試みず即座に
    /// `Outcome::Error` を返す。
    pub mcp_args_error: Option<String>,
}

/// `competitor_lightpanda/fixtures/` を配信するローカル静的サーバー。
///
/// 事前の URL 検証をいくら積み増しても、計測対象ブラウザが実際に接続する
/// 宛先（名前再解決・リダイレクト追従込み）は保証できない。この問題は
/// 「ベンチが外部ネットワークへ一切出ない構成にする」ことでのみ解消できる
/// ため、`127.0.0.1` の空きポートへ bind した最小限の HTTP/1.1 静的サーバーを
/// 自前で立て、fixture ディレクトリ配下のファイルだけを返す（Lightpanda 本家
/// のベンチがローカルで配信するデモサイトを対象にするのと同じ構成）。
///
/// 接続受付はバックグラウンドスレッドで行い、`Drop` で `stop` フラグを立てて
/// スレッドの終了を待つ（プロセス終了時にリスナーを残さない）。
///
/// このサーバーの実際のクライアントは計測対象のブラウザ（`goto` で HTTP
/// GET する側）であり、ベンチ自身（テストコード）ではない。対象ブラウザは
/// 複数の接続を同時に開き得るため、接続ごとにスレッドを立てて処理する
/// （[`fixture_accept_loop`] 参照）。何も送らない接続（preconnect 等）が
/// 1 本詰まっても、他の接続の配信を止めないようにするための構成である。
pub(crate) struct FixtureServer {
    pub(crate) port: u16,
    stop: Arc<AtomicBool>,
    /// 現在処理中の接続数。[`fixture_accept_loop`] が同時接続数の上限判定に
    /// 使い、`Drop` は個々の接続スレッドの `JoinHandle` を保持しない代わりに
    /// この値が 0 になるのを有界のポーリングで待つ。
    active_connections: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FixtureServer {
    /// サーバーを起動する。`support::FIXTURE_TABLE`（コンパイル時に埋め込み
    /// 済みの固定テーブル）に完全一致するパスだけを配信する。
    ///
    /// `pub(crate)`: `competitor_lightpanda_measure`（`measure_tests.rs`）の
    /// 結合テストが、fixture サーバーへ直接リクエストを送るテスト
    /// （400/404/405/408・ヘッダ行数上限・slow-loris）のために使う。
    pub(crate) fn start() -> Result<Self, String> {
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
        let active_connections = Arc::new(AtomicUsize::new(0));
        let active_connections_for_thread = Arc::clone(&active_connections);
        let handle = std::thread::spawn(move || {
            fixture_accept_loop(listener, stop_for_thread, active_connections_for_thread);
        });
        Ok(Self {
            port,
            stop,
            active_connections,
            handle: Some(handle),
        })
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // 各接続は [`fixture_accept_loop`] がスレッドごとに処理し、個々の
        // `JoinHandle` は保持していないため直接 join できない。各接続には
        // 接続ごとの絶対期限（`FIXTURE_IO_TIMEOUT` + エラー応答用の猶予）が
        // あるため、無期限に待つのではなく有界のポーリングで
        // `active_connections` が 0 になるのを待つ（時間切れなら諦めて返る。
        // 個々の接続スレッド自身の期限にはこの待機と無関係に到達する）。
        let wait_deadline = Instant::now() + FIXTURE_IO_TIMEOUT + Duration::from_secs(5);
        while self.active_connections.load(Ordering::SeqCst) > 0 && Instant::now() < wait_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// 同時に処理する fixture 接続数の上限。thread-per-connection にしても
/// スレッドを無制限に増やさないための上限（coding-rust.md「長さ・件数を
/// 上限検証」）。超過した接続は処理せず即座に閉じる。
const FIXTURE_MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// `listener.accept()` の一時的なエラー（fd の瞬間的な枯渇等）を許容する
/// 連続回数の上限。1 回のエラーで即座にループを止めると、一時的な障害でも
/// サーバー全体が停止してしまうため、上限に達するまでは継続する。
const FIXTURE_ACCEPT_ERROR_LIMIT: u32 = 100;

/// [`FixtureServer::start`] が生成する accept ループ本体。
///
/// `stop` が立つまで `listener.accept()` をポーリングし、接続ごとに
/// スレッドを立てて [`handle_fixture_connection`] を処理する（実際の
/// クライアントは計測対象のブラウザであり複数接続を同時に開き得るため、
/// シングルスレッド処理にはしない）。`active_connections` が
/// [`FIXTURE_MAX_CONCURRENT_CONNECTIONS`] に達している間は、新規接続を
/// 処理せず即座に閉じる。
fn fixture_accept_loop(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    active_connections: Arc<AtomicUsize>,
) {
    let mut served: u64 = 0;
    let mut consecutive_accept_errors: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                consecutive_accept_errors = 0;
                served += 1;
                // 接続数に上限を設け、想定外の大量接続でサーバーが無制限に
                // 処理し続けないようにする。
                if served > FIXTURE_MAX_CONNECTIONS {
                    break;
                }
                let previous = active_connections.fetch_add(1, Ordering::SeqCst);
                if previous >= FIXTURE_MAX_CONCURRENT_CONNECTIONS {
                    // 同時接続数の上限を超えた分はスレッドを増やさず、
                    // 応答せずに即座に閉じる（`drop(stream)`）。
                    active_connections.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                let active_connections_for_thread = Arc::clone(&active_connections);
                // `std::thread::spawn` はスレッド生成に失敗すると panic する
                // （coding-rust.md「ライブラリコードでは panic させない」）。
                // 外部入力（大量の同時接続）に応じて増減するスレッド生成の
                // 経路であるため、`Builder::spawn` で `Result` として扱い、
                // 失敗時はこの接続だけを諦めてループを継続する。
                let spawn_result = std::thread::Builder::new().spawn(move || {
                    handle_fixture_connection(stream);
                    active_connections_for_thread.fetch_sub(1, Ordering::SeqCst);
                });
                if spawn_result.is_err() {
                    active_connections.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(FIXTURE_ACCEPT_POLL_INTERVAL);
            }
            Err(_) => {
                // listener 自体の一時的なエラー（fd の瞬間的な枯渇等）では
                // 即座にループを止めず、連続回数の上限に達するまで継続する。
                // 上限に達したら、`stop` の検知漏れでプロセスが終了できなく
                // なることを避けるためループを止める。
                consecutive_accept_errors += 1;
                if consecutive_accept_errors >= FIXTURE_ACCEPT_ERROR_LIMIT {
                    break;
                }
                std::thread::sleep(FIXTURE_ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

/// リクエスト行・ヘッダ行を 1 行読み、`read_line_bounded` が返す行が
/// 改行で終わっていない（`Ok(0)` の完全 EOF、または `MAX_LINE_BYTES` 未満で
/// 接続が切れた不完全な行）場合を、まとめて「行を読み切れなかった」
/// エラーとして扱う。
///
/// `read_line_bounded` はバイト単位で読み、
/// 以前は相手が改行を送る前に接続を閉じても、それまでに読めた分だけを
/// `Ok(n)`（`n > 0`）で返していた。呼び出し側がこれを「1 行読めた」として
/// 扱うと、ヘッダ終端（空行）へ到達しないまま後続処理（`lookup_fixture`
/// による 200 応答）へ進み得た。
///
/// この「改行で終わらない行は
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
    // 読み取り側の絶対期限（`FIXTURE_IO_TIMEOUT`）はこの時点でちょうど
    // 使い切られているため、延長せずに書き込むと常に即座にタイムアウトし、
    // 400/408 応答自体が構造的にクライアントへ届かない
    // （`DeadlineReader::extend_deadline` のドキュメント参照）。この小さな
    // 固定長のエラー応答だけを送るための短い猶予を与える。
    const ERROR_RESPONSE_GRACE: Duration = Duration::from_secs(2);
    writer.extend_deadline(ERROR_RESPONSE_GRACE);
    let _ = write_fixture_response(writer, status, reason, PLAIN_TEXT, b"");
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
/// 以前はここでファイルシステムから
/// `metadata`/`read` していたため、(1) シンボリックリンクを辿ってしまい
/// fixture ディレクトリ外のファイルを配信し得た、(2) メタデータ確認後の
/// 読み込みまでの TOCTOU でサイズ上限を超えて確保し得た。`lookup_fixture`
/// はコンパイル時に埋め込んだ `&'static str` を返すだけでファイルシステムへ
/// 一切触れないため、両方とも構造的に起こらない。
///
/// リクエスト行・ヘッダ行の読み取りが
/// タイムアウト・サイズ超過・接続の早期切断のいずれで失敗しても、以前は
/// ループを抜けるだけで後続の 200 応答へ進み得た。読み取りが完了しなかった
/// 経路はすべて [`respond_with_io_error`] で 400/408 を返してから接続を
/// 終了する。
fn handle_fixture_connection(stream: TcpStream) {
    // macOS（XNU）・
    // Windows（Winsock）では accept したソケットが listener のノンブロッキング
    // 状態を継承する（Linux の accept4 は継承しない）。継承されたままだと
    // 後続の `read_line_bounded` の最初の `read` が即座に `WouldBlock` を
    // 返し、リクエスト到着前に 400 を返してしまう。ブロッキングへ明示的に
    // 戻すことで 3 OS で同じ挙動にする（coding-rust.md「クロスプラットフォーム」）。
    let _ = stream.set_nonblocking(false);

    // 接続 1 本ごとの絶対期限を
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
        let _ = write_fixture_response(&mut writer, 400, "Bad Request", PLAIN_TEXT, b"");
        return;
    };
    // `goto`（GET）だけを配信対象にする。GET 以外はすべて
    // `405 Method Not Allowed` として拒否する（このサーバーの唯一の
    // クライアントは計測対象ブラウザの `goto` であり、GET 以外を受理する
    // 必要がない）。
    if method != "GET" {
        let _ = write_fixture_response(&mut writer, 405, "Method Not Allowed", PLAIN_TEXT, b"");
        return;
    }

    // ヘッダ行は使わないが、クライアント（計測対象ブラウザ）が送り終える前に
    // 接続を切ると `RST` になり得るため、`Connection: close` 前提で読み捨てる
    // （1 行あたりは `read_line_bounded` の `MAX_LINE_BYTES` で上限済み）。
    // 行数自体には
    // 上限がなく、ヘッダ行を送り続けるクライアントがサーバースレッドを
    // 無期限に専有し得た。`FIXTURE_MAX_HEADER_LINES` で総行数にも上限を
    // 設け、超過時は 400 を返して打ち切る（coding-rust.md「長さ・件数を
    // 上限検証」）。終端判定は `\r\n`（CRLF）だけでなく裸の `\n`（LF のみ）も
    // 空行として扱う（一部クライアントは LF のみを送るため）。
    let mut header_lines_seen = 0u32;
    loop {
        if header_lines_seen >= FIXTURE_MAX_HEADER_LINES {
            let _ = write_fixture_response(&mut writer, 400, "Bad Request", PLAIN_TEXT, b"");
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
            let _ =
                write_fixture_response(&mut writer, 200, "OK", content_type, content.as_bytes());
        }
        None => {
            let _ = write_fixture_response(&mut writer, 404, "Not Found", PLAIN_TEXT, b"");
        }
    }
}

/// 400/404/405/408 応答の `Content-Type`。fixture 本体は
/// `support::FIXTURE_TABLE` が個別に持つ content-type を使う（パス →
/// (content-type, 内容) の固定テーブル）。
const PLAIN_TEXT: &str = "text/plain; charset=utf-8";

/// `handle_fixture_connection` が使う最小限の HTTP/1.1 レスポンス書き込み。
///
/// `Content-Length` は常に `body.len()` にし、本文も常に書く（このサーバー
/// が受理するのは GET のみで HEAD は 405 にするため、本文を省く分岐は
/// 持たない）。`writer`（[`DeadlineReader`]）経由でのみ書き込むことで、
/// 接続の絶対期限を守る。
fn write_fixture_response(
    writer: &mut DeadlineReader,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

/// 子プロセスを確実に終了させる guard。早期 `return`（`?`）経路でも
/// `Drop` で `kill` + `wait` する（`wait` を省くとゾンビプロセスが残る）。
/// [`kill_process_group`] も合わせて呼び、子孫プロセスが起動していても
/// 極力パイプを保持し続けないようにする。
struct ChildGuard(Child);

/// `ChildGuard::drop` が `wait` に許す期限。`kill_process_group`/
/// `Child::kill` の後は通常すぐに終了するはずだが、OS がシグナル配送・
/// プロセス終了を遅延させる異常系でも `Drop` が無期限にブロックしない
/// ようにする（コンストラクタは `Drop::drop` からしか呼ばれない想定の
/// 内部専用定数）。
const CHILD_GUARD_WAIT_DEADLINE: Duration = Duration::from_secs(10);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // `Drop` は戻り値を呼び出し元へ返せないため、ここに限り
        // `kill_process_group` の失敗を握りつぶしてよい。続く `Child::kill`（直接の子）は
        // 無条件に行う既存のフォールバックのままにする。
        let _ = kill_process_group(self.0.id());
        let _ = self.0.kill();
        // 無期限の `wait()` ではなく、期限付きの `wait_with_deadline` を
        // 使う（`kill` 済みのプロセスが何らかの理由で終了を検知できない
        // 異常系でも `Drop` 自体が無期限にブロックしないようにする）。
        let _ = wait_with_deadline(&mut self.0, CHILD_GUARD_WAIT_DEADLINE);
    }
}

/// 空きポートを 1 つ確保して返す（`127.0.0.1:0` に bind して即座に解放する）。
/// 固定ポートにすると並列実行される他ベンチ・他 issue の worktree と衝突するため
/// 使わない。
///
/// リスナーを解放してから子プロセスを
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
/// `reserve_port` の TOCTOU により、
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
    // タイムアウト時、最後の probe 失敗理由を報告に含める（毎回同じ
    // 汎用メッセージでは、readiness に失敗した実際の原因（接続拒否・
    // 不正な応答形式・chunked デコード失敗等）が分からない）。
    let mut last_error = "no probe attempt was made".to_string();
    // `last_error` が実際の probe 結果（成功しなかった応答・失敗理由）から
    // 得られたものかどうか。プレースホルダのままなら「有意な理由」とは
    // 扱わない。
    let mut has_probe_result = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "timeout waiting for readiness probe (last error: {last_error})"
            ));
        }
        // 残り時間が 1 回の probe 試行の期限（`PROBE_ATTEMPT_TIMEOUT`）に
        // 満たない場合、この後の `probe_once` はほぼ確実に「期限切れで
        // 読み書きに失敗した」という情報量の無いエラーを返し、それまでに
        // 記録した本来の失敗理由（不正なチャンク長等）を上書きしてしまう。
        // 既に有意な理由を記録済みなら、無駄な最終試行をせずここで
        // タイムアウトとして確定させる（テストで観測された、9 回中 2 回の
        // フレークの原因）。
        if remaining < PROBE_ATTEMPT_TIMEOUT && has_probe_result {
            return Err(format!(
                "timeout waiting for readiness probe (last error: {last_error})"
            ));
        }
        // 子プロセスが既に終了しているのにポートへの応答（＝別プロセスの
        // 応答の可能性）だけを見て readiness と誤認しないよう、まず生存を
        // 確認する。
        match guard.0.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("child exited before becoming ready: {status}"));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("try_wait failed: {e}")),
        }
        match probe_once(port, &request, deadline) {
            Ok((status, body))
                if (200..300).contains(&status) && looks_like_browser_readiness_response(&body) =>
            {
                return Ok((guard, port, start.elapsed()));
            }
            Ok((status, body)) => {
                // 応答は得られたが readiness の条件を満たさない（ステータス・
                // 本文形式のいずれか）。本文は上限を設けて表示する（巨大な
                // 応答をエラーメッセージへそのまま埋め込まない）。
                const MAX_DISPLAY_BODY_CHARS: usize = 200;
                let truncated: String = body.chars().take(MAX_DISPLAY_BODY_CHARS).collect();
                last_error = format!(
                    "response did not look like browser readiness (status {status}, body {truncated:?})"
                );
                has_probe_result = true;
            }
            Err(e) => {
                // 外側の期限をちょうど使い切ったタイミングでの失敗は、
                // 情報量の無い「期限切れ」である可能性が高い。既に有意な
                // 理由を記録済みなら、それを上書きしない（上のループ先頭の
                // チェックをすり抜けた残りわずかなケースの保険）。
                if Instant::now() < deadline || !has_probe_result {
                    last_error = e;
                    has_probe_result = true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// readiness probe の応答本文に許す上限バイト数。相手プロセス（別プロセスの
/// 誤応答も含む）が不正・大量の応答を返し続けても無制限に確保しないための
/// 上限（coding-rust.md「長さ・件数を上限検証」）。
///
/// `pub(crate)`: 結合テスト（`measure_tests.rs`）が `probe_once` の上限
/// 超過ケースを直接検証する際に参照する。
pub(crate) const PROBE_BODY_MAX_BYTES: usize = 64 * 1024;

/// readiness probe 1 回あたりに許す時間。`outer_deadline`（呼び出し元の
/// readiness 全体の期限）の残り時間がこれより短ければ、そちらを優先する。
///
/// `pub(crate)`: 結合テスト（`measure_tests.rs`）が `probe_once` を直接
/// 呼ぶ際の `outer_deadline` を組み立てるために参照する。
pub(crate) const PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(200);

/// [`reprobe_after_kill`] が同じポートへ再接続を試みる際の期限。
/// `TcpStream::connect_timeout` は接続拒否（誰も listen していない）なら
/// この期限を待たずに即座に返るため、実際にはこの値は「応答が無いことを
/// 確認するまでの上限」としてのみ働く。
const PORT_CONFLICT_REPROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// [`spawn_and_wait_ready`] が起動した対象を `kill_process_group`/
/// `Child::kill` で終了させ `wait` で確認した直後に、同じポートへまだ
/// 何か（TCP レベルで）応答するプロセスがいるかを確認する。
///
/// `reserve_port` はリスナーを解放してから子プロセスを起動するまでの間に
/// 別プロセスが同じポートを奪える TOCTOU が残り、readiness probe が
/// 対象自身ではなく別プロセスの応答を拾っていた可能性がある。std だけでは
/// ソケットの所有者（PID）を直接確認する手段が無いため、代わりに
/// 「対象を kill して `wait` 済みのはずなのに、まだそのポートへ接続
/// できるか」を事後確認する（post-hoc な所有権確認。判定ロジック自体は
/// `support::port_conflict_error` へ切り出し、そちらで単体テストする）。
///
/// この確認には以下の限界がある（`support::port_conflict_error` の
/// ドキュメントにも記載）:
/// - kill から再接続までの間に第三のプロセスが新たに同じポートを奪った
///   場合、それを「元から居た別プロセス」と区別できない
/// - 対象が孫プロセス以降を起動していた場合、`ChildGuard`/
///   `kill_process_group` の `wait` は直接の子までしか保証しない
/// - std だけではソケットの所有者を PID で直接確認できない
///
/// これらの限界があっても、「対象を殺したのに同じポートがまだ応答する」
/// という明白な矛盾を検出できることには意味があり、黙って誤った計測値を
/// 使うより安全である（best-effort な事後確認）。
fn reprobe_after_kill(port: u16) -> bool {
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, PORT_CONFLICT_REPROBE_TIMEOUT).is_ok()
}

/// readiness probe を 1 回だけ試す。ステータス行と本文を返す。接続失敗・
/// 応答不正（不正な UTF-8・不正な `Transfer-Encoding`/`Content-Length`
/// 併存等を含む）・タイムアウトはいずれも `Err`（理由付き）にする
/// （呼び出し側 [`spawn_and_wait_ready`] はポーリングを継続しつつ、
/// 最後の失敗理由をタイムアウト時の報告に使う。panic させない）。
///
/// 本文まで読み、呼び出し元が [`looks_like_browser_readiness_response`] で
/// 検証できるようにする。この 1 回の読み取りは [`DeadlineReader`] を通し、
/// `outer_deadline` を超えない絶対期限（`PROBE_ATTEMPT_TIMEOUT` とどちらか
/// 短い方）で統一的に区切る。宣言された長さ（`Content-Length` または
/// `Transfer-Encoding: chunked`）を読み切る前に EOF・エラー（タイムアウトを
/// 含む）が起きた場合は、読めた断片を検証に渡さず probe 失敗にする
/// （部分的な本文をそのまま使わない）。`Content-Length` も
/// `Transfer-Encoding: chunked` も無い場合は、読み取りエラーを失敗として
/// 扱いつつ EOF まで読む。
/// `pub(crate)`: `competitor_lightpanda_measure`（`measure_tests.rs`）の
/// 単体テストが、`Content-Length`/`Transfer-Encoding` の境界ケース
/// （併存・矛盾等）をループバックソケット越しに直接検証する。
pub(crate) fn probe_once(
    port: u16,
    request: &str,
    outer_deadline: Instant,
) -> Result<(u16, String), String> {
    let probe_timeout =
        PROBE_ATTEMPT_TIMEOUT.min(outer_deadline.saturating_duration_since(Instant::now()));
    if probe_timeout.is_zero() {
        return Err("outer deadline already elapsed".to_string());
    }
    let deadline = Instant::now() + probe_timeout;

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("invalid probe address: {e}"))?;
    let stream = TcpStream::connect_timeout(&addr, probe_timeout)
        .map_err(|e| format!("connect failed: {e}"))?;
    let mut deadline_stream = DeadlineReader::new(stream, deadline);
    // 書き込み・読み取りの両方を同じ `DeadlineReader`（`Read`・`Write` の
    // 両方を実装する）経由で行う。書き込み後、そのまま `BufReader` に
    // 包んで読み取りへ移る（`BufReader` はどんな `Read` 実装も受け付ける
    // ため、内部の `TcpStream` を取り出し直す必要が無い）。
    deadline_stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("request write failed: {e}"))?;
    let mut reader = BufReader::new(deadline_stream);
    let mut status_line = String::new();
    read_line_bounded(&mut reader, &mut status_line)
        .map_err(|e| format!("status line read failed: {e}"))?;
    let status = parse_http_status(&status_line)
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
    // ヘッダ行を空行（終端）まで読み飛ばし、`Content-Length`・
    // `Transfer-Encoding` があれば覚えておく。行数に上限を設け
    // （`fixture_accept_loop` の `FIXTURE_MAX_HEADER_LINES` と同じ理由。
    // ポートを奪った別プロセスがヘッダ行を送り続けて `probe_once` を長時間
    // 占有することを防ぐ）、上限超過は probe 失敗にする。
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut header_lines_seen = 0u32;
    loop {
        if header_lines_seen >= FIXTURE_MAX_HEADER_LINES {
            return Err(format!(
                "too many header lines (limit {FIXTURE_MAX_HEADER_LINES})"
            ));
        }
        header_lines_seen += 1;
        let mut header_line = String::new();
        read_complete_line(&mut reader, &mut header_line)
            .map_err(|e| format!("header line read failed: {e}"))?;
        if header_line == "\r\n" || header_line == "\n" {
            break;
        }
        let lower = header_line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            // `Content-Length` ヘッダー自体は存在するのに値が不正
            // （非数値・空・符号付き等。`support::parse_content_length`
            // 参照）な場合、「長さ指定なし」（EOF まで読む）へフォール
            // バックせず probe 失敗にする（ヘッダーが実在するのにその値を
            // 無視するのは、相手の応答を正しく解釈できていないことを
            // 意味するため）。同じヘッダーが複数回現れて値が食い違う場合
            // （重複して矛盾する `Content-Length`）も、どちらの値を
            // 信じるべきか判断できないため probe 失敗にする（同一値の
            // 重複は許容する）。
            let parsed = parse_content_length(value.trim())
                .ok_or_else(|| format!("invalid Content-Length: {value:?}"))?;
            match content_length {
                Some(existing) if existing != parsed => {
                    return Err(format!(
                        "conflicting Content-Length values: {existing} and {parsed}"
                    ));
                }
                _ => content_length = Some(parsed),
            }
        }
        if let Some(value) = lower.strip_prefix("transfer-encoding:") {
            let value = value.trim();
            // このベンチが対応するのは単一の `chunked` コーディングのみ。
            // それ以外（`gzip`・複数コーディングの連結等）は本文の実際の
            // 境界を正しく解釈できないため probe 失敗にする。
            if value != "chunked" {
                return Err(format!("unsupported Transfer-Encoding: {value:?}"));
            }
            chunked = true;
        }
    }
    // `Content-Length` と `Transfer-Encoding: chunked` が両方存在する応答は
    // HTTP/1.1 のセマンティクス上あいまい（RFC 9112 §6.1 は `Content-Length`
    // を無視するよう求めるが、この不一致自体がリクエストスマグリング等の
    // 攻撃の温床になり得るため、無視して進めるのではなく probe 失敗にする）。
    if chunked && content_length.is_some() {
        return Err("response has both Content-Length and Transfer-Encoding: chunked".to_string());
    }
    let body = if chunked {
        read_chunked_body(&mut reader, PROBE_BODY_MAX_BYTES)?
    } else {
        // `Content-Length` を無視して常に EOF まで（＝読み取りタイムアウト
        // まで）読むと、相手が `Connection: close` を要求されても接続を
        // 保持し続ける HTTP/1.1 実装の場合、毎回の readiness probe に
        // タイムアウト分の遅延が系統的に乗り、cold start（`PERF-3`）の
        // 計測値を実態より水増しし得る。`Content-Length` が分かれば
        // ちょうどその長さだけ読み、無ければ（ヘッダに含まれない応答向けの
        // フォールバックとして）EOF までの読み取りに戻す。
        //
        // `Content-Length` が `PROBE_BODY_MAX_BYTES` を超える場合は、
        // 読み取りを試みず即座に probe 失敗にする（サーバーが実際に宣言
        // した長さの応答を確認しないまま `len.min(...)` 等で黙って
        // 切り詰めて成功扱いにしない）。`Content-Length` が無い分岐
        // （EOF まで読む）でも、読めたバイト数が上限を超えたら同様に
        // probe 失敗にする。
        if content_length_exceeds_limit(content_length, PROBE_BODY_MAX_BYTES) {
            return Err(format!(
                "Content-Length exceeds probe body limit ({PROBE_BODY_MAX_BYTES} bytes)"
            ));
        }
        match content_length {
            Some(len) => {
                // `Read::read_exact` は指定したバッファをちょうど埋め切る
                // 前に EOF に達すると `ErrorKind::UnexpectedEof` を返す
                // （部分的に読めた分をそのまま `Ok` として返すことはない）
                // ため、宣言された `Content-Length` に届く前に応答が途中で
                // 切れたケースを取りこぼさず失敗にできる。読み取りエラー
                // （`DeadlineReader` 経由のタイムアウトを含む）も同様に
                // `Err` になる。
                let mut buf = vec![0u8; len];
                reader
                    .read_exact(&mut buf)
                    .map_err(|e| format!("body read failed (Content-Length={len}): {e}"))?;
                buf
            }
            None => {
                let mut body = Vec::new();
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
                                return Err(format!(
                                    "body exceeds probe body limit ({PROBE_BODY_MAX_BYTES} bytes)"
                                ));
                            }
                        }
                        // タイムアウトを含む読み取りエラーは失敗として扱う
                        // （それまでに読めた断片をそのまま使わない）。
                        Err(e) => return Err(format!("body read failed: {e}")),
                    }
                }
                body
            }
        }
    };
    // 本文も厳密な UTF-8 変換にする（`from_utf8_lossy` による文字化けが
    // 偶然妥当な JSON へ変わり、別プロセスの応答をブラウザらしいと誤判定
    // する経路を避ける）。
    let body_str = String::from_utf8(body).map_err(|e| format!("body is not valid UTF-8: {e}"))?;
    Ok((status, body_str))
}

/// `Transfer-Encoding: chunked` の本文を読む（[`probe_once`] からのみ呼ぶ）。
///
/// RFC 9112 §7.1 のチャンク形式（16 進のチャンクサイズ行。`;` 以降の
/// チャンク拡張は無視する。チャンクデータに続く改行・サイズ 0 のチャンクと
/// それに続くトレーラー部）を厳密に読む。不正なチャンクサイズ行・
/// チャンクデータ読み取り前の EOF・チャンクデータ直後の改行欠落・
/// `max_bytes` を超える累積本文サイズは、いずれも `Err` にする（部分的に
/// 読めた本文を成功として使わない）。トレーラー行数は
/// [`FIXTURE_MAX_HEADER_LINES`] で上限を設ける（無制限に送り続けられて
/// 占有され続けることを防ぐ。coding-rust.md「長さ・件数を上限検証」）。
///
/// 改行の基準: RFC 9112 は CRLF を要求するが、このファイル内の行読み取り
/// （[`read_complete_line`]。「一部クライアントは LF のみを送るため」）と
/// 統一し、チャンクサイズ行・トレーラー行・チャンクデータ直後の区切りの
/// いずれも CRLF と裸の LF の両方を許容する（[`read_chunk_terminator`]
/// 参照）。
///
/// `pub(crate)`: `competitor_lightpanda_measure`（`measure_tests.rs`）の
/// 単体テストがループバックソケット経由で境界ケースを直接検証する。
pub(crate) fn read_chunked_body<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        read_complete_line(reader, &mut size_line)
            .map_err(|e| format!("chunk size line read failed: {e}"))?;
        let size_line = size_line.trim_end_matches(['\r', '\n']);
        // チャンク拡張（`;` 以降。例: `1a;foo=bar`）はサイズの解釈に使わない
        // ため読み捨てる。BWS（オプションの空白）を許すため前後の空白を
        // 落としてから検証する。
        let size_field = size_line.split(';').next().unwrap_or("").trim();
        if size_field.is_empty() {
            return Err("empty chunk size".to_string());
        }
        // `usize::from_str_radix` は符号付き型と同じ構文解析を経由し `+`
        // 接頭辞を許してしまう（例: `"+1a"` が `Ok(26)` になる）ため、
        // 16 進数字のみで構成されることを明示的に確認してから解釈する。
        if !size_field.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("invalid chunk size: {size_field:?}"));
        }
        let size = usize::from_str_radix(size_field, 16)
            .map_err(|_| format!("invalid chunk size: {size_field:?}"))?;
        if size == 0 {
            // 最終チャンク。トレーラー部（0 行以上のヘッダ相当）を空行まで
            // 読み飛ばす。
            let mut trailer_lines_seen = 0u32;
            loop {
                if trailer_lines_seen >= FIXTURE_MAX_HEADER_LINES {
                    return Err(format!(
                        "too many chunked trailer lines (limit {FIXTURE_MAX_HEADER_LINES})"
                    ));
                }
                trailer_lines_seen += 1;
                let mut trailer_line = String::new();
                read_complete_line(reader, &mut trailer_line)
                    .map_err(|e| format!("chunk trailer line read failed: {e}"))?;
                if trailer_line == "\r\n" || trailer_line == "\n" {
                    break;
                }
            }
            break;
        }
        if body.len().saturating_add(size) > max_bytes {
            return Err(format!("chunked body exceeds {max_bytes} bytes"));
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| format!("chunk data read failed (size={size}): {e}"))?;
        body.extend_from_slice(&chunk);
        read_chunk_terminator(reader)?;
    }
    Ok(body)
}

/// チャンクデータ直後の改行を読む。CRLF（RFC 9112 準拠）・裸の LF
/// （[`read_chunked_body`] のドキュメント参照。このファイル内の行読み取り
/// と同じ寛容さで統一する）のいずれかだけを受理する。それ以外（`\r` の後に
/// `\n` が続かない等）は `Err` にする。
fn read_chunk_terminator<R: BufRead>(reader: &mut R) -> Result<(), String> {
    let mut first = [0u8; 1];
    reader
        .read_exact(&mut first)
        .map_err(|e| format!("chunk trailing newline read failed: {e}"))?;
    match first[0] {
        b'\n' => Ok(()),
        b'\r' => {
            let mut second = [0u8; 1];
            reader
                .read_exact(&mut second)
                .map_err(|e| format!("chunk trailing newline read failed: {e}"))?;
            if second[0] == b'\n' {
                Ok(())
            } else {
                Err(format!(
                    "chunk not terminated by CRLF or LF (got {:?} {:?})",
                    first[0], second[0]
                ))
            }
        }
        other => Err(format!(
            "chunk not terminated by CRLF or LF (got {other:?})"
        )),
    }
}

/// `BufRead::read_line` 相当だが、外部プロセスの応答を無制限に信用せず
/// `MAX_LINE_BYTES` を超えたら打ち切る（DoS を避ける。coding-rust.md）。
///
/// 以前は `MAX_LINE_BYTES` に達しても改行未検出の
/// まま `Ok` を返しており、呼び出し側（`probe_once`・MCP stdout 読み取り
/// スレッド）が切り詰められた不完全な行を正常な 1 行として扱い得た
/// （`parse_json` が偶然パース可能な断片を返す・`parse_http_status` が誤った
/// 値を拾う等）。改行を見ないまま上限へ達した場合は
/// `ErrorKind::InvalidData` で明示的に失敗させ、呼び出し側に「この行は
/// 読み取れなかった」ことを伝える（coding-rust.md「外部入力の経路では
/// 明示的に処理する」）。
///
/// 以前は `String::from_utf8_lossy` で
/// 不正なバイト列を置換文字（U+FFFD）へ書き換えていたため、置換後の
/// 文字列がたまたま妥当な JSON になると、元の応答とは異なる内容を
/// 正常なトークン削減率として出力し得た。`String::from_utf8` で厳密に
/// 変換し、失敗したら `ErrorKind::InvalidData` で計測エラーにする
/// （呼び出し元は他の `Err` 経路と同様に扱えばよく、個別対応は不要）。
///
/// 契約: 「改行で終わる行を 1 本読めた」場合
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
///
/// `pub(crate)`: `competitor_lightpanda_measure`（`measure_tests.rs`）の
/// 単体テストがループバックソケット経由で直接検証する。
pub(crate) fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    out: &mut String,
) -> std::io::Result<usize> {
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
///
/// Windows の既知の制約: readiness probe（`probe_once`）は
/// `TcpStream::connect_timeout` で対象プロセスが bind したポートへ接続する
/// が、Windows では「まだ何も listen していないポートへの接続」の失敗
/// （`WSAECONNREFUSED` 相当の即時拒否ではなく、TCP レベルの再試行を挟んで
/// 検出される場合がある）に 1 秒以上かかることがある。これは
/// `connect_timeout` の指定時間より短くても発生し得る OS 側の挙動であり、
/// このベンチのコードでは制御できない。対象プロセスの実際の起動が速くても、
/// 最初の 1〜数回の probe 試行がこの遅延の影響を受け、cold start の測定値が
/// 実態よりわずかに水増しされる可能性がある（`PROBE_ATTEMPT_TIMEOUT` の
/// 短い間隔でポーリングを繰り返すため、遅延は最大でも数回分に留まる）。
pub fn measure_cold_start(target: &Target, trials: usize) -> Outcome {
    let bin = match require_bin(
        target.bin.as_ref(),
        target.bin_error.as_deref(),
        target.name,
    ) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    // cold start は `serve_args` だけを使うため、`mcp_args_error` は無視する。
    if let Some(outcome) = arg_error_gate(target.serve_args_error.as_deref(), target.name) {
        return outcome;
    }
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        match spawn_and_wait_ready(bin, &target.serve_args) {
            Ok((guard, port, elapsed)) => {
                // `drop(guard)` は `kill_process_group`/`Child::kill` で
                // プロセス（グループ）を終了させ、`wait` で完全な終了を
                // 確認してから返る。その後に再 probe することで、
                // 「対象を殺したのに同じポートがまだ応答する」という
                // ポート競合を検出できる。
                drop(guard);
                if let Some(reason) =
                    port_conflict_error(target.name, port, reprobe_after_kill(port))
                {
                    return Outcome::Error(format!(
                        "{}: cold start trial failed: {reason}",
                        target.name
                    ));
                }
                samples.push(elapsed.as_secs_f64() * 1000.0);
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

/// プロセスツリーの pid 一覧構築（`sample_process_tree_rss_kb` が使う）で
/// 数え上げる pid 数の上限。`pgrep` 1 回あたりの出力バイト上限
/// （`PGREP_CHILDREN_MAX_OUTPUT_BYTES`）ともこの値を前提に決めてある
/// （coding-rust.md「長さ・件数を上限検証してからアロケーションに使う」）。
#[cfg(unix)]
const MAX_TREE_PIDS: usize = 256;

/// `pid` の直接の子プロセス一覧を `pgrep -P <pid>` で取得する
/// （`sample_process_tree_rss_kb` の BFS が使う）。
///
/// `pgrep -P` は一致する子プロセスが無いと終了コード 1 を返す（子がいない
/// リーフノードは正常な終端であり失敗ではないため空の一覧にする）。それ
/// 以外の非 0 終了・起動失敗は呼び出し元で `None` 扱いにする
/// （fail-closed。coding-rust.md「外部入力の経路では明示的に処理する」）。
#[cfg(unix)]
fn pgrep_children(pid: u32) -> Option<Vec<u32>> {
    // `pgrep -P <pid>` の出力は pid の一覧のみで通常数バイト〜数十バイト。
    const MAX_PGREP_OUTPUT_BYTES: usize = 4096;
    let mut command = Command::new("pgrep");
    command.args(["-P", &pid.to_string()]);
    let (status, stdout) = crate::support::run_capturing_output_with_deadline(
        command,
        crate::support::EXTERNAL_COMMAND_DEADLINE,
        MAX_PGREP_OUTPUT_BYTES,
    )
    .ok()?;
    if !status.success() {
        return match status.code() {
            Some(1) => Some(Vec::new()),
            _ => None,
        };
    }
    parse_pgrep_pids(&String::from_utf8_lossy(&stdout), MAX_TREE_PIDS)
}

/// `root_pid` を起点に `ppid` チェーンを幅優先で辿り、プロセスツリー全体
/// （自身 + 子・孫・…）の pid 一覧を返す（`sample_process_tree_rss_kb` が
/// 使う）。
///
/// TASK-84.2・Issue #212 のレビュー指摘対応: 以前は `pgrep -g <pgid>`
/// （`apply_new_process_group` が作る同一プロセスグループのメンバー）で
/// 代用していたが、対象の子孫が `setpgid` 等で別のプロセスグループへ
/// 移ると数え漏れ、実際には目標未達でも `PERF-6` 判定
/// （`support::perf6_comparison`）が `"met"` を誤って返し得た。`ppid` を
/// 直接辿る本方式はプロセスグループの変更に影響されない。`MAX_TREE_PIDS`
/// を超える異常なツリーサイズは打ち切って `None`（呼び出し元が Error 化）
/// にする。
#[cfg(unix)]
fn collect_process_tree_pids(root_pid: u32) -> Option<Vec<u32>> {
    let mut pids = vec![root_pid];
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        for child in pgrep_children(parent)? {
            if pids.contains(&child) {
                continue;
            }
            if pids.len() >= MAX_TREE_PIDS {
                return None;
            }
            pids.push(child);
            frontier.push(child);
        }
    }
    Some(pids)
}

/// アイドル RSS（`PERF-6`）: 起動して安定させたあと、対象プロセスの RSS
/// （KB）を読む。unix は対象プロセスのプロセスツリー全体
/// （自身 + 子孫。`collect_process_tree_pids` 参照）の RSS 合計、windows は
/// 対象プロセス単体のワーキングセット相当の値を読む（`measure_idle_rss`
/// 参照）。
///
/// TASK-84.2・Issue #212 のレビュー指摘対応（unix 側）: 以前は
/// `guard.0.id()`（直接の子プロセス 1 つ）だけを `ps -o rss= -p <pid>` で
/// 読んでいたため、対象がさらに子プロセスを使う実装だと、そのメモリを
/// 含めずに `PERF-6` 判定（`support::perf6_comparison`。基準値は Chromium の
/// **プロセスツリー全体**のアイドル RSS）へ渡してしまい、実際には目標未達
/// でも `"met"` を誤って返し得た。windows は `tasklist` にプロセスツリー
/// 全体を安全に数え上げる標準的な手段が無いため（`unsafe`・新規依存を
/// 避ける方針。TASK-84.5）、対象プロセス単体の計測
/// （`sample_rss_kb`・[`parse_tasklist_mem_kb`]）に留める。
#[cfg(unix)]
fn sample_process_tree_rss_kb(root_pid: u32) -> Option<u64> {
    let pids = collect_process_tree_pids(root_pid)?;
    if pids.is_empty() {
        return None;
    }
    // `ps -p` はカンマ区切りで複数 pid を受け付ける（POSIX・GNU・BSD 共通）。
    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // 1 pid あたり最大 10 桁 + カンマ想定で `MAX_TREE_PIDS` 件分の出力を
    // 十分収められる上限（`ps` 出力は数字列のみのため通常はこれよりずっと
    // 小さい）。
    const MAX_PS_OUTPUT_BYTES: usize = 8192;
    let mut ps_command = Command::new("ps");
    ps_command.args(["-o", "rss=", "-p", &pid_list]);
    let (ps_status, ps_stdout) = crate::support::run_capturing_output_with_deadline(
        ps_command,
        crate::support::EXTERNAL_COMMAND_DEADLINE,
        MAX_PS_OUTPUT_BYTES,
    )
    .ok()?;
    if !ps_status.success() {
        return None;
    }
    // `collect_process_tree_pids`（`pgrep`）で確定した pid が、この `ps`
    // 実行までに終了しているとその行が出力されない。`sum_ps_rss_kb_lines`
    // だけだと残りの行を「合計成功」として黙って返してしまう（レビュー
    // 指摘対応。TASK-84.2・Issue #212）ため、要求した pid 数と実際の行数の
    // 一致を検証する `sum_ps_rss_kb_lines_checked` を使う。`from_utf8_lossy`
    // で足りる理由は旧 `sample_process_group_rss_kb` と同じ（置換文字混入時
    // は `parse::<u64>()` が失敗して `None` になるだけ）。
    crate::support::sum_ps_rss_kb_lines_checked(&String::from_utf8_lossy(&ps_stdout), pids.len())
}

/// windows 版 `sample_rss_kb`。`tasklist` の標準出力は OEM コードページで
/// 出るが、[`parse_tasklist_mem_kb`] が使うのは ASCII 数字だけなので
/// （関数側のコメント参照）、unix 版と同様に `from_utf8_lossy` で十分。
#[cfg(windows)]
fn sample_rss_kb(pid: u32) -> Option<u64> {
    // `tasklist` の CSV 1 行は数百バイト程度に収まる。unix の `ps` 版と
    // 同じ考え方で無制限確保を避ける上限を設ける
    // （coding-rust.md「長さ・件数を上限検証」）。
    const MAX_TASKLIST_OUTPUT_BYTES: usize = 4096;
    let mut command = Command::new("tasklist");
    command.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
    let (status, stdout) = crate::support::run_capturing_output_with_deadline(
        command,
        crate::support::EXTERNAL_COMMAND_DEADLINE,
        MAX_TASKLIST_OUTPUT_BYTES,
    )
    .ok()?;
    if !status.success() {
        return None;
    }
    parse_tasklist_mem_kb(&String::from_utf8_lossy(&stdout), pid)
}

/// アイドル RSS（`PERF-6`）計測本体。`sample_rss_kb` の OS 差異
/// （unix: `ps`／Windows: `tasklist`）を吸収した先の共通フロー。
/// 以前は unix/windows で別関数に分けており、Windows 版は
/// `target.bin` を確認せず常に `Outcome::Unsupported` を返すバグを持って
/// いた。`require_bin` による `Skipped`/`Error` の判定を 1 か所に保つため
/// 1 本の関数へ統合した（TASK-84.5）。
fn measure_idle_rss(target: &Target, trials: usize) -> Outcome {
    let bin = match require_bin(
        target.bin.as_ref(),
        target.bin_error.as_deref(),
        target.name,
    ) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    // アイドル RSS も `serve_args` だけを使うため、`mcp_args_error` は
    // 無視する。
    if let Some(outcome) = arg_error_gate(target.serve_args_error.as_deref(), target.name) {
        return outcome;
    }
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        match spawn_and_wait_ready(bin, &target.serve_args) {
            Ok((guard, port, _elapsed)) => {
                std::thread::sleep(Duration::from_millis(500));
                // `sample_process_tree_rss_kb`/`sample_rss_kb` の失敗
                // （プロセス早期終了・パース不能出力）を黙って捨てず即座に
                // Error 化する。unix は `ppid` チェーンを辿ってプロセス
                // ツリー全体の RSS 合計（子孫を含む。PERF-6・Issue #212 の
                // レビュー指摘対応）を読む。windows は対象プロセス単体を
                // 読む（TASK-84.5）。
                let pid = guard.0.id();
                #[cfg(unix)]
                let rss = sample_process_tree_rss_kb(pid);
                #[cfg(windows)]
                let rss = sample_rss_kb(pid);
                // `drop(guard)` は `kill_process_group`/`Child::kill` で
                // プロセス（グループ）を終了させ、`wait` で完全な終了を
                // 確認してから返る。その後に再 probe することで、
                // 「対象を殺したのに同じポートがまだ応答する」という
                // ポート競合を検出できる。
                drop(guard);
                if let Some(reason) =
                    port_conflict_error(target.name, port, reprobe_after_kill(port))
                {
                    return Outcome::Error(format!(
                        "{}: idle RSS trial failed: {reason}",
                        target.name
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
                match rss {
                    Some(rss) => samples.push(rss as f64),
                    None => {
                        return Outcome::Error(format!(
                            "{}: idle RSS trial failed: RSS sampling failed for pid {pid}",
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

/// バイナリサイズ（`PERF-1`）: 対象実行ファイルの `fs::metadata` によるバイト数。
fn measure_binary_size(target: &Target) -> Outcome {
    let bin = match require_bin(
        target.bin.as_ref(),
        target.bin_error.as_deref(),
        target.name,
    ) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    match std::fs::metadata(bin) {
        Ok(meta) if meta.is_file() => Outcome::Value(meta.len() as f64),
        // `fs::metadata` はディレクトリにも成功し
        // `len()` がディレクトリエントリサイズ等の無意味な値を返すため、
        // `<PREFIX>_BIN` に誤ってディレクトリを指定した場合に異常値が
        // そのまま `binarySizeBytes` として出力され得た。通常ファイルで
        // ないことを検出したら計測失敗として扱う。
        Ok(_) => Outcome::Error(format!("{}: bin path is not a regular file", target.name)),
        Err(e) => Outcome::Error(format!("{}: metadata failed: {e}", target.name)),
    }
}

/// stdout 読み取りスレッドが呼び出し側との間に持つ行キューの合計バイト数の
/// 上限。`MAX_LINE_BYTES`（1 行あたりの上限）だけでは、外部プロセスが応答を
/// 読ませないまま大量の短い行を送り続けた場合にキュー全体のメモリ使用量を
/// 抑えられない（行数で上限を切っても、1 行あたりのサイズが小さければ
/// 合計バイト数は行数の上限とは無関係に膨らみ得る）ため、行数ではなく
/// 実際にキューに滞留しているバイト数の合計（`McpClient::queued_bytes`）で
/// 上限を判定する。溢れたら読み取りスレッドを止めて `overflowed` を立てる。
/// 64MiB は、正常系での `notifications/message` 等のログ行バーストを
/// 誤検知しない余裕を持たせつつ、無制限確保にはしない上限として選んだ値。
const MCP_STDOUT_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// `McpClient::notify` の書き込みタイムアウト。
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
    /// `Arc<Mutex<_>>` にする理由: `write_with_deadline`
    /// はタイムアウト時に書き込みスレッドを detach し（`join` しない）、
    /// 以後の呼び出しでも同じ `ChildStdin` を再利用できるようにするため、
    /// 所有権を一方的に奪う（`&mut ChildStdin`/`Option::take`）方式ではなく
    /// 共有できる `Arc<Mutex<ChildStdin>>` にする。detach したスレッドが
    /// 書き込みを続けている間に次の呼び出しが来ても、ロック待ちで
    /// `recv_timeout` が正しくタイムアウトする（無期限に隠れてブロックしない）。
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    rx: mpsc::Receiver<String>,
    /// キューに滞留している行の合計バイト数。読み取りスレッドが行を送信
    /// するたびに加算し、`call`/`notify` が `rx` から受け取るたびに
    /// （その行の長さ分）減算する（[`MCP_STDOUT_QUEUE_MAX_BYTES`] 参照）。
    queued_bytes: Arc<AtomicUsize>,
    next_id: u64,
    /// 読み取りスレッドがキュー容量超過を検知した際に立てるフラグ。
    /// `call` はこれを見て、無制限にキューを溜め込む代わりに
    /// 計測をエラー終了させる。
    overflowed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 読み取りスレッドが行の読み取り・解析に失敗した際にその理由を記録する
    /// スロット。対象は `read_line_bounded` からのエラー（`InvalidData`
    /// （`MAX_LINE_BYTES` 到達・改行未検出・不正な UTF-8）・`UnexpectedEof`
    /// を含む、あらゆる I/O エラー種別）に加え、行として読めても JSON として
    /// 解析できなかった場合（`spawn` 参照）も同じスロットに記録する。`call`
    /// はこれを見て、切り詰められた／文字化けした／未完了／非 JSON の行を
    /// 無視したまま待ち続けるのではなく明示的にエラー終了させる。
    ///
    /// `Mutex<Option<String>>` にすることで、エラーの種別を問わず実際の
    /// メッセージを記録する（`AtomicBool` で「エラーの有無」だけを見る
    /// 設計だと、種別ごとに個別の記録経路を用意し忘れた場合にスレッドが
    /// 記録なしで静かに終了し得る）。
    line_read_error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl McpClient {
    fn spawn(bin: &PathBuf, args: &[String]) -> Result<Self, String> {
        let mut command = Command::new(bin);
        apply_new_process_group(&mut command);
        let child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        // spawn 直後、他の操作（`stdin`/`stdout` の take 等）より先に
        // `ChildGuard` へ包む。`take()` が失敗する経路（`?`）があっても、
        // それ以降の早期 return で子プロセスをリークさせない
        // （`Stdio::piped()` を指定しているため通常は起こらないが、防御的に
        // 保証する）。
        let mut guard = ChildGuard(child);
        let stdin = Arc::new(Mutex::new(guard.0.stdin.take().ok_or("no stdin")?));
        let stdout = guard.0.stdout.take().ok_or("no stdout")?;
        // 行数ではなく合計バイト数でキューの上限を判定するため、チャンネル
        // 自体には容量を設けず（`mpsc::channel`）、`queued_bytes` で
        // バックプレッシャーをかける（`MCP_STDOUT_QUEUE_MAX_BYTES` 参照）。
        let (tx, rx) = mpsc::channel::<String>();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let queued_bytes_for_thread = Arc::clone(&queued_bytes);
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
                    Ok(_) => {
                        // MCP の stdio トランスポート仕様上、stdout に現れる
                        // 行はすべて 1 行 1 JSON 値の応答でなければならない
                        // （それ以外を書くのは対象プロセス側の契約違反）。
                        // 解析できない行を読み飛ばして次の行を待つと、
                        // 対象プロセスの不正な出力に気づかないまま応答待ちを
                        // 続け得るため、`line_read_error` に記録して読み取り
                        // スレッドを止め、`call`/`notify` 側へ明示的に
                        // 伝える（fail-closed。coding-rust.md「外部入力の
                        // 経路では明示的に処理する」）。
                        //
                        // `line` の末尾には `read_line_bounded` が読んだ改行
                        // （`\r\n` または `\n`）がそのまま残っている。
                        // `str::trim`（Unicode の空白全般を対象にする）では
                        // なく `trim_end_matches(['\r', '\n'])` で末尾の改行
                        // だけを取り除く。`trim` を使うと、`parse_json` 自身が
                        // 拒否する RFC 8259 外の空白（例: U+00A0）が行の
                        // 先頭・末尾にあった場合にそれを黙って取り除いて
                        // しまい、本来 `parse_json` が弾くべき不正な行を
                        // 通してしまう経路になり得る。空行（改行のみの行）は
                        // 取り除いた結果が空文字列になり、`parse_json` が
                        // `UnexpectedEnd` で拒否するため、他の非 JSON 行と
                        // 同じく即エラーとして扱う（読み飛ばさない）。
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if let Err(e) = parse_json(trimmed) {
                            if let Ok(mut reason) = line_read_error_writer.lock() {
                                *reason = Some(format!("mcp stdout line is not valid JSON: {e:?}"));
                            }
                            break;
                        }
                        let line_len = line.len();
                        let new_total = queued_bytes_for_thread
                            .fetch_add(line_len, Ordering::SeqCst)
                            + line_len;
                        if new_total > MCP_STDOUT_QUEUE_MAX_BYTES {
                            // キュー容量超過: 消費側が追いつけていない、
                            // または相手プロセスが応答を読ませず出力し
                            // 続けている。無制限に溜め込まず読み取りを
                            // 止め、呼び出し側へは `overflowed` 経由で
                            // エラーとして伝える。この行自体は送信しない
                            // （加算した分を戻す）。
                            queued_bytes_for_thread.fetch_sub(line_len, Ordering::SeqCst);
                            overflowed_writer.store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                        if tx.send(line).is_err() {
                            // 受信側（`McpClient`）が drop 済み（呼び出し元が
                            // 既に諦めている）。
                            break;
                        }
                    }
                    Err(e) => {
                        // `MAX_LINE_BYTES` に達し改行未検出のまま打ち切られた
                        // 行・不正な UTF-8・改行に達する前に子プロセスが
                        // 標準出力を閉じた（`UnexpectedEof`）・その他の I/O
                        // エラー（`ConnectionReset` 等）を、種別を問わず
                        // すべて記録する。切り詰められた／文字化けした／
                        // 未完了の断片を正常応答として扱わせず、読み取りを
                        // 止めて `call` 側へ明示的に伝える。
                        if let Ok(mut reason) = line_read_error_writer.lock() {
                            *reason = Some(e.to_string());
                        }
                        break;
                    }
                }
            }
            // 「reader スレッドが終了
            // したら（EOF の場合も含めて）、送信側を drop する」ことを
            // 明示する。`tx` はこのクロージャに move 済みのローカル変数
            // なので、`break` でループを抜けクロージャが終了する時点で
            // 自動的に drop される（Rust の通常のスコープ規則）。これに
            // より `call`/`notify` 側の `self.rx.recv_timeout` は
            // 待機中でも即座に `RecvTimeoutError::Disconnected` で
            // 返るようになる。
        });
        Ok(Self {
            guard,
            stdin,
            rx,
            queued_bytes,
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
    /// 以前は `std::thread::scope` を使い、
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
                // 終了させる床として残す。
                //
                // `kill_process_group` の
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
    /// （タイムアウト内に一致しなければ `Err`。他 id の応答（サーバーからの
    /// リクエスト・通知を含む）は読み捨てて継続する。JSON として解析
    /// できない行は、この関数に届く前に読み取りスレッド側で検知して
    /// 読み取り自体を止める（`spawn` 参照。パース不能行を読み飛ばして
    /// 待ち続けることはしない。外部プロセスの応答を untrusted として
    /// 扱う）。
    ///
    /// `id` が一致しても JSON-RPC の `error` フィールドが立っている、または
    /// `tools/call` 結果の `result.isError` が `true` の応答は失敗として
    /// `Err` を返す（呼び出し元 `measure_token_reduction` は、いずれかの
    /// サイトでこのエラーが起きると、そのサイトを `failed` に記録した上で
    /// 計測全体を `Outcome::Error` にする。一部失敗を隠した測定値を
    /// `Value` として返すことはない。ここで成功と誤判定すると、失敗
    /// レスポンスの本文がトークン削減率の計測に混入する）。
    ///
    /// `timeout` は書き込み（`write_with_deadline`）から応答待ちまでの
    /// 呼び出し全体に適用する（以前は書き込みに
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
                    "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_MAX_BYTES} bytes, aborting"
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
            // 読み取りスレッドが終了すると
            // `tx`（送信側）が drop され、`recv_timeout` は待機中でも即座に
            // `RecvTimeoutError::Disconnected` で返る。以前はこれを
            // `Timeout` と区別せず一括りに扱っていたため、実際には切断
            // （エラー・EOF）が起きているのに「timeout waiting」という
            // 紛らわしいメッセージになり得た。ここで明示的に区別し、
            // 「channel の切断を検知して即時に返す」ことを保証する。
            let line = match self.rx.recv_timeout(remaining) {
                Ok(line) => {
                    // 受信した分だけキューの合計バイト数を減らす
                    // （`MCP_STDOUT_QUEUE_MAX_BYTES` によるバックプレッシャーの
                    // 対になる減算。読み取りスレッド側の加算と対応する）。
                    self.queued_bytes.fetch_sub(line.len(), Ordering::SeqCst);
                    line
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if self.overflowed.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(format!(
                            "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_MAX_BYTES} bytes, aborting"
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
                            "{method}: mcp stdout queue exceeded {MCP_STDOUT_QUEUE_MAX_BYTES} bytes, aborting"
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
            // 読み取りスレッドが非 JSON 行をキューへ送る前に弾く（`spawn` の
            // 読み取りループ参照）ため、ここに来る行は必ず有効な JSON の
            // はずだが、その不変条件が崩れても panic はせず読み飛ばす
            // （fail-closed。coding-rust.md「外部入力の経路では unwrap を
            // 使わず明示的に処理する」）。
            // 読み取りスレッド側（`spawn`）と同じ理由で `trim` ではなく
            // `trim_end_matches(['\r', '\n'])` を使う。
            let Ok(value) = parse_json(line.trim_end_matches(['\r', '\n'])) else {
                continue;
            };
            // id が一致しない応答（他の呼び出しの応答等）は読み捨てて
            // 次の行を待つ。JSON-RPC 2.0 では、対象プロセスがサーバーから
            // クライアントへ向けたリクエスト（例: `ping`）を送ることも
            // あり、その場合も `id`・`method` の両方を持つ。そのような
            // メッセージは `method` を持つため「応答」ではなく「対象からの
            // リクエスト」であり、`id` の値がこちらの発行した id と
            // たまたま一致し得る（双方が独立に採番するため id 空間が
            // 分離されていない）。`method` を持つメッセージは応答として
            // 扱わず読み捨てる（対象からのリクエストへの応答自体は、この
            // ベンチが必要とする範囲では行わない）。
            let has_method = value.get("method").and_then(JsonValue::as_str).is_some();
            if has_method || value.get("id").and_then(JsonValue::as_f64) != Some(id as f64) {
                continue;
            }
            // `result` が `null`・`{}` でも `isError` さえ立っていなければ
            // 成功として扱ってしまうと、`goto` のように本文を確認しない
            // 呼び出しでは、ページ遷移に失敗した応答をそのまま成功と誤判定
            // し得る（続く `html`・`tree` が別ページの結果として計測される）。
            // JSON-RPC 2.0 の封筒（`jsonrpc`・`id`・`error`/`result` の排他）と、
            // `method` ごとに要求される `result` の最小限の形
            // （`tools/call` は `content` が配列であること・各要素の `type`・
            // `isError` の型など。`content` は MCP 2025-06-18 の仕様どおり
            // 空配列も許す）を `validate_mcp_response`（support.rs。純粋関数）
            // で検証してから、`error`/`isError` の実際の失敗判定へ進む。
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
    /// `write_with_deadline`で
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
/// 欠落時に空文字列を返してしまうと、`html` が非空で `tree` の抽出に
/// 失敗しただけのケースが `tree_tok=0` の正常計測（削減率 100%）として
/// 記録されてしまう。呼び出し元 `measure_token_reduction` は `None` を
/// 抽出失敗として扱い、そのサイトを成功サンプルに含めない。
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
/// 削減率を求める。`fixture_port` は呼び出し元（`main`）が起動した
/// [`FixtureServer`] のポート。`goto` する URL は [`crate::support::fixture_url`]
/// でのみ組み立て、`FIXTURE_TABLE` の固定パス以外を渡せない（外部 URL の
/// 指定経路自体を持たない）。
pub fn measure_token_reduction(target: &Target, fixture_port: u16) -> Outcome {
    let bin = match require_bin(
        target.bin.as_ref(),
        target.bin_error.as_deref(),
        target.name,
    ) {
        Ok(bin) => bin,
        Err(skipped) => return skipped,
    };
    // `AISNAP-1` は `mcp_args` だけを使うため、`serve_args_error` は無視する。
    if let Some(outcome) = arg_error_gate(target.mcp_args_error.as_deref(), target.name) {
        return outcome;
    }
    // `marker` は当該 fixture の `goto` が実際に成功したことを内容ベースで
    // 確認するための一意な識別子（[`crate::support::fixture_marker`] の
    // ドキュメント参照）。`FIXTURE_TABLE` の各パスには必ず対応するマーカーが
    // 登録されている（`support.rs` の
    // `fixture_table_and_markers_cover_the_same_paths` が保証する）ため、
    // ここで `unwrap_or("")` にせず明示的に内部不整合として扱う。
    let mut sites: Vec<(String, &'static str, &'static str)> =
        Vec::with_capacity(crate::support::FIXTURE_TABLE.len());
    for (page, _content_type, _content) in crate::support::FIXTURE_TABLE {
        let Some(marker) = crate::support::fixture_marker(page) else {
            return Outcome::Error(format!(
                "{}: internal error: no fixture marker registered for {page}",
                target.name
            ));
        };
        // `kind` は結果の `eprintln!`（サイトごとの削減率）に添える類型名
        // （`AISNAP-1`・TASK-84.4。`FIXTURE_KINDS` のドキュメント参照）。
        // `FIXTURE_TABLE` の全パスに対応する類型が必ず登録されていることは
        // `support.rs` の `fixture_table_and_kinds_list_the_same_paths_in_order`
        // が保証するため、ここでも内部不整合として明示的に扱う
        // （`unwrap_or` で黙って埋め合わせない）。
        let Some((kind, _poc13_site)) = fixture_kind(page) else {
            return Outcome::Error(format!(
                "{}: internal error: no fixture kind registered for {page}",
                target.name
            ));
        };
        sites.push((
            crate::support::fixture_url(fixture_port, page),
            marker,
            kind,
        ));
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
    // 送信失敗を握りつぶすと、初期化未完了のまま
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

    // goto・html・tree の失敗サイトを無言で除外し、残ったサイトだけの
    // 中央値を「代表値」として報告してしまうと、既定のサイトのうち 1 件
    // しか成功しなくても正常な計測であるかのように見えてしまう。失敗した
    // サイトと理由を `failed` に記録し、1 件でも完走できなければ
    // `Outcome::Error` にして「一部失敗を隠した測定値」を返さない
    // （fail-closed）。
    let mut reductions = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (url, marker, kind) in &sites {
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
        // `goto` の応答が JSON-RPC としての形式
        // （`validate_mcp_response`）を満たしているというだけでは、対象
        // ブラウザが実際にこの `url` へ遷移したことを意味しない（未実装の
        // `goto` が常に成功応答だけ返す・前のページに留まったまま等）。
        // MCP 側に「現在の URL」を取得する専用 API があるとは限らないため
        // それには依存せず、`html`・`tree` の応答内容そのものに、この
        // fixture 固有の一意なマーカー（`crate::support::fixture_marker`。
        // 各ページ `<body>` 内の可視見出し `<h1>` のテキスト）が含まれて
        // いるかを確認する。
        //
        // 当初は
        // `<title>` テキストをマーカーにしていたが、`<title>` は `<head>`
        // にしか存在せずアクセシビリティツリー（`tree`）に現れる保証が
        // 無いため、実ブラウザでは `tree` 側の検証が常に失敗し得た
        // （`support::FIXTURE_MARKERS` のドキュメント参照）。見出し要素
        // （heading ロール）はアクセシビリティツリーに確実に名前として
        // 表れるため、マーカーを `<h1>` のテキストへ変更した。
        //
        // html・tree のどちらか一方でも欠けていれば、直前のページの
        // ままである・遷移に失敗した等の疑いがあるため、その試行を計測
        // 失敗にする（黙って直前のページの内容を当該 fixture の成功
        // サンプルとして記録しない）。
        if !html.contains(marker) {
            failed.push(format!(
                "{url}: goto navigation could not be verified: html response does not contain the fixture marker {marker:?}"
            ));
            continue;
        }
        if !tree.contains(marker) {
            failed.push(format!(
                "{url}: goto navigation could not be verified: tree response does not contain the fixture marker {marker:?}"
            ));
            continue;
        }
        let html_tok = approx_tokens(html.chars().count());
        let tree_tok = approx_tokens(tree.chars().count());
        match reduction_pct(html_tok as f64, tree_tok as f64) {
            Some(pct) => {
                // 類型ごとの実測値を stderr へ出す（TASK-84.4・AISNAP-1）。
                // 結果 JSON の `sites`（`run_all`）は静的な構成情報のみで
                // 実測値を持たないため、実際の削減率を確認できる経路は
                // ここだけになる。
                eprintln!("token reduction [{kind}] ({url}): {pct}");
                reductions.push(pct);
            }
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

/// [`Target`] の一覧に対し、バイナリサイズ（`PERF-1`）・cold start（`PERF-3`）・
/// アイドル RSS（`PERF-6`）・MCP トークン削減率（`AISNAP-1`）の全計測を実行し、
/// 結果 JSON 本文（`../competitor_lightpanda.rs` が stdout へ出す形式そのもの）と
/// 終了コード（[`crate::support::bench_exit_code`] 契約）を返す。
///
/// `../competitor_lightpanda.rs` の `main`（環境変数から実対象を構築する）と、
/// `measure_tests.rs` の結合テスト（対象バイナリを模したローカルプロセスを
/// `targets` に指定する）の両方から呼ぶ、計測の唯一のエントリポイント。
///
/// `chromium_idle_rss_kb`: `CHROMIUM_IDLE_RSS_KB`（`main` が
/// [`crate::support::parse_chromium_baseline_kb`] で解析済み）由来の
/// Chromium ヘッドレス 1 インスタンス（プロセスツリー全体）のアイドル RSS
/// 基準値（KB）。`None`（未設定）なら各対象の結果 JSON の `"perf6"` は
/// `skipped` になる（TASK-84.2・Issue #212）。各対象の `idleRssKb` と
/// この基準値から [`crate::support::perf6_comparison`]（純粋関数。
/// `PERF-6` 目標値は [`crate::support::PERF6_TARGET_PCT`]）が判定した結果を
/// `"perf6"` として添える。`below_target`（目標未達）は計測結果の一種であり
/// 計測失敗ではないため、[`bench_exit_code`] の判定対象には含めない
/// （`../competitor_lightpanda.rs` モジュールドキュメントの終了コード契約参照）。
pub fn run_all(
    targets: &[Target],
    trials: usize,
    chromium_idle_rss_kb: Option<u64>,
) -> (String, u8) {
    // `AISNAP-1` 計測は外部
    // URL を一切使わず、ここで起動するローカル fixture サーバーだけを
    // 対象にする（モジュールドキュメント参照）。両方の `target` で
    // 同じサーバーを共有し、関数を抜けるときに `Drop` で停止する。
    // どちらの対象バイナリも未設定（`bin` が `None`）のときは
    // `measure_token_reduction` が最初の分岐で必ず `Outcome::Skipped` を
    // 返しサーバーを使わないため、起動自体を省く（起動を
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
        // 片方の対象だけに `_BIN` を設定した
        // 状態で fixture サーバーの起動が失敗すると、以前は未設定の対象にも
        // `Outcome::Error` を割り当てていた（「対象バイナリ未設定は常に
        // `Outcome::Skipped`」という契約に反する）。`token_reduction_gate`
        // （support.rs。純粋関数）へ対象ごとの `bin` 有無を渡して判定を
        // 対象単位にする。
        let server_start_error = fixture_server.as_ref().and_then(|r| r.as_ref().err());
        let token_reduction = match token_reduction_gate(
            target.bin.as_ref(),
            target.bin_error.as_deref(),
            server_start_error.map(String::as_str),
            target.name,
        ) {
            Some(outcome) => outcome,
            None => match &fixture_server {
                Some(Ok(server)) => measure_token_reduction(target, server.port),
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

        // `PERF-6` 目標（Chromium 比 85% 以上のアイドル RSS 削減。
        // `crate::support::PERF6_TARGET_PCT`）との比較（TASK-84.2・Issue #212）。
        // 判定対象は `idle_rss`（この対象のアイドル RSS 計測結果。unix は
        // `sample_process_tree_rss_kb` によりプロセスツリー全体の RSS 合計に
        // 揃えてある）そのものであり、`bench_exit_code` の判定対象には
        // 含めない（`perf6_comparison` ドキュメント参照）。
        //
        // windows は `idleRssKb` 自体は実測できる（TASK-84.5）が、対象
        // プロセス単体のワーキングセットしか読めない一方、
        // `chromium_idle_rss_kb`（基準値）は Chromium の**プロセスツリー
        // 全体**のアイドル RSS のため、両者は計測範囲が食い違い数値上の
        // 比較が成立しない（`idleRssKb` が小さく出るだけで「削減できた」
        // わけではない）。レビュー指摘対応（TASK-84.2・Issue #212）として、
        // Windows では実測値をそのまま `perf6_comparison` へ渡さず
        // `Outcome::Unsupported` に差し替えて「比較不能」を明示する
        // （`idleRssKb` フィールド自体の実測値はこの差し替えの影響を受け
        // ない。`Skipped`/`Unsupported`/`Error` はそのまま素通しする）。
        let perf6_idle_rss = if cfg!(windows) {
            match idle_rss.clone() {
                Outcome::Value(_) => Outcome::Unsupported(
                    "PERF-6 comparison is unsupported on Windows: idleRssKb measures only \
                     the direct child process while the baseline is a full process-tree total"
                        .to_string(),
                ),
                other => other,
            }
        } else {
            idle_rss.clone()
        };
        let perf6 = crate::support::perf6_comparison(
            chromium_idle_rss_kb,
            &perf6_idle_rss,
            crate::support::PERF6_TARGET_PCT,
        );
        eprintln!("PERF-6 comparison: {perf6}");

        // `sitesCount`・`sites`: `tokenReductionPct` の対象として構成
        // されている fixture の件数・類型一覧（`support::FIXTURE_TABLE`・
        // `support::FIXTURE_KINDS`。TASK-84.4 で PoC-13 相当の 5 類型へ
        // 拡張した）。いずれも計測の成否とは独立の静的な構成情報であり、
        // `tokenReductionPct` が `skipped`/`error` のときも出力する。
        // `docs/spec/03-poc/browser-landscape-2026`（PoC-13）と類型は
        // 揃えたが、fixture は自作の小規模静的コンテンツでありライブ
        // ページを転載していないため、`tokenReductionPct` を PoC-13 の
        // 実測値と直接比較しないことを結果からも判別できるようにする
        // （モジュールドキュメント・`support::FIXTURE_KINDS` 参照）。
        body.push_str(&format!(
            "  \"{}\": {{\"binarySizeBytes\":{},\"coldStartMs\":{},\"idleRssKb\":{},\"tokenReductionPct\":{},\"sitesCount\":{},\"sites\":{},\"perf6\":{}}}",
            json_escape(target.name),
            binary_size.to_json(),
            cold_start.to_json(),
            idle_rss.to_json(),
            token_reduction.to_json(),
            crate::support::FIXTURE_TABLE.len(),
            fixture_sites_json(),
            perf6,
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

    let outcome_refs: Vec<&Outcome> = outcomes.iter().collect();
    let exit_code = bench_exit_code(&outcome_refs);
    (body, exit_code)
}

/// [`crate::support::FIXTURE_KINDS`] を順に走査し、`run_all` が結果 JSON へ
/// 添える `sites` 配列の本文（`[{"path":...,"kind":...,"poc13Site":...}, ...]`）
/// を組み立てる。
///
/// `FIXTURE_KINDS` は `&'static str` の固定テーブルであり実行時入力を含まない
/// が、JSON 文字列として埋め込む際は他の文字列出力（`Outcome::to_json` 等）
/// と同じ経路（[`json_escape`]）に統一する（TASK-84.4・AISNAP-1）。
fn fixture_sites_json() -> String {
    let mut out = String::from("[");
    for (i, (path, kind, poc13_site)) in crate::support::FIXTURE_KINDS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"kind\":\"{}\",\"poc13Site\":\"{}\"}}",
            json_escape(path),
            json_escape(kind),
            json_escape(poc13_site),
        ));
    }
    out.push(']');
    out
}
