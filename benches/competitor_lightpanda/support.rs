//! `competitor_lightpanda` ベンチ本体（`../competitor_lightpanda.rs`）が使う
//! 純粋関数群。副作用（プロセス起動・ソケット通信）を持たないロジックだけを
//! ここへ切り出し、`[[test]]` ターゲット（`competitor_lightpanda_support`）として
//! 3 OS で unit test する（ci.md「3 OS CI」・coding-rust.md「テスト」）。
//!
//! TASK-84（84.1）・Issue #211。対応ビヘイビア: バイナリサイズ計測が使う
//! `PERF-1`、cold start 計測が使う `PERF-3`、アイドル RSS 計測が使う `PERF-6`、
//! MCP トークン削減率計測が使う `AISNAP-1`。
//!
//! この crate root は `[[test]]` ターゲット（`cargo test` 用の bin-like root）
//! としてもコンパイルされるため、`#[cfg(test)]` 外の関数もすべて `cfg(test)` の
//! テストから到達させる必要がある（到達しない関数・フィールドは dead_code エラーに
//! なる。unused であっても `pub` は免除されない）。
//!
//! 例外的に [`DeadlineReader`] だけは実ソケット I/O を行う（`TcpStream` を
//! 包む）。外部プロセス（対象バイナリ）を起動せずローカルの loopback
//! ソケットだけで動作を検証できるため、単体テスト（slow-loris 相手の
//! 全体期限打ち切り等）もこのファイルに置く（レビュー指摘。コーディネーター
//! 指示。PR #442 再々々々々々レビュー: fixture サーバー・readiness probe の
//! 両方で場当たり的に `set_read_timeout` を設定するのではなく、共通の
//! プリミティブへ統一する）。
//!
//! `AISNAP-1`（MCP トークン削減率）計測は、任意の外部 URL
//! （`COMPETITOR_BENCH_SITES`）ではなく、リポジトリに同梱した静的 fixture
//! （外部参照を含まない自作コンテンツ。[`FIXTURE_TABLE`]）だけを対象にする
//! （PR #442 再レビュー。Codex P0: 計測対象ブラウザは接続時に名前を
//! 再解決し、公開 URL からのリダイレクトにも追従し得るため、事前の URL
//! 検証をいくら積み増しても実際の接続先を保証できない）。fixture は
//! `include_str!` でコンパイル時にバイナリへ埋め込み（実行時のファイル
//! I/O を行わない。PR #442 再々レビュー。Codex P0/P1: ファイルシステムから
//! 都度読む方式はシンボリックリンク追従・メタデータ確認後の TOCTOU
//! サイズ超過の経路になり得た）、`competitor_lightpanda.rs` が起動する
//! ローカル静的サーバー（`127.0.0.1` の空きポート）が [`lookup_fixture`]
//! の完全一致検索で配信する。`goto` する URL がそのサーバーだけを指す
//! ことを [`validate_local_bench_url`] で確認する。これによりベンチが
//! 外部ネットワークへ一切出ない構成になり、SSRF 経路を構造的になくすと
//! 同時に、外部サイトの可用性・変化に左右されない再現可能な計測になる
//! （security.md「SSRF」）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Instant;

// `reqwest`（`fandhe-browser-core` の既存依存。ホストする crate の
// `Cargo.toml` 参照）は `url::Url` を `reqwest::Url` として re-export している。
// 新規依存を追加せず（dependency-policy.md）、既存の推移依存から WHATWG URL
// 準拠のパーサーを使うため、ここでは `reqwest::Url` を経由して取り込む
// （レビュー指摘 P0/Medium。PR #442。再レビューでローカル URL 検証
// （`validate_local_bench_url`）へ用途を変更した）。
use reqwest::Url;

/// 複数回試行した計測値（ミリ秒・キロバイト等）から中央値を求める。
///
/// cold start（`PERF-3`）・アイドル RSS（`PERF-6`）はいずれも複数回試行して
/// 中央値を採用する方針（実装計画 3.2 節）のため、単位に依存しない `f64` で
/// 受け取る。空スライスは中央値を定義できないため `None` を返す。
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = sorted.len();
    let mid = len / 2;
    let value = if len.is_multiple_of(2) {
        let lower = *sorted.get(mid - 1)?;
        let upper = *sorted.get(mid)?;
        (lower + upper) / 2.0
    } else {
        *sorted.get(mid)?
    };
    Some(value)
}

/// 基準値（Chromium 実測値等）に対する削減率を百分率で返す。
///
/// `PERF-1`（バイナリサイズ）・`PERF-6`（アイドル RSS）の Chromium 比目標達成可否
/// 判定、`AISNAP-1`（トークン削減率）の算出で共通に使う。`baseline` が 0 以下の
/// ときは分母が意味を持たないため `None` を返す（fail-closed。coding-rust.md
/// 「外部入力の経路では unwrap を使わず明示的に処理する」に準じ、ここでは
/// 環境変数由来の基準値を外部入力として扱う）。
pub fn reduction_pct(baseline: f64, value: f64) -> Option<f64> {
    if baseline <= 0.0 {
        return None;
    }
    Some((1.0 - value / baseline) * 100.0)
}

/// 文字数から近似トークン数を見積もる（`ceil(chars / 4)`）。
///
/// `AISNAP-1` のトークン削減率計測で使う近似式（PoC-1 の `measure-chromium` と
/// 同じ近似。実装計画 3.2 節に明記の通り、cl100k 等の厳密なトークナイザ依存は
/// 導入しないため、この近似値と PoC-13 実測値（gpt-tokenizer 使用）は一致しない）。
pub fn approx_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

/// HTTP レスポンスの先頭行（例: `HTTP/1.1 200 OK`）からステータスコードを取り出す。
///
/// cold start 計測（`PERF-3`）の readiness probe が `TcpStream` で受け取った
/// レスポンスの先頭行を渡す想定。書式に合わない行・範囲外の数値は `None` にし、
/// 呼び出し側のポーリングを継続させる（panic させない。coding-rust.md）。
pub fn parse_http_status(line: &str) -> Option<u16> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    // レビュー指摘 P2（コーディネーター指示。PR #442 再々々々々々々々
    // レビュー・support.rs:109）: 以前は `version.starts_with("HTTP/")`
    // （`HTTP/potato` のような不正な値も通る）・`code.parse::<u16>()`
    // （桁数不問。`0`・`65535` のような HTTP のステータスコードとして
    // 無意味な値も通る）という緩い判定だった。ここでは
    // `HTTP/1.0`・`HTTP/1.1` のいずれかに続く単一の SP、ちょうど 3 桁の
    // 数字（100〜599）、その後に SP か行末が続く、という形式だけを
    // 受理する。
    let version_len = "HTTP/1.1".len();
    // `str::split_at` はバイト境界が UTF-8 文字境界からずれていると panic
    // する。外部プロセスからの未検証入力（マルチバイト文字を含み得る）を
    // 扱うため、`get` で境界を検証してから取り出す（panic させない。
    // coding-rust.md「外部入力の経路では unwrap を使わず明示的に処理する」）。
    let version = trimmed.get(..version_len)?;
    let rest = trimmed.get(version_len..)?;
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return None;
    }
    let rest = rest.strip_prefix(' ')?;
    let mut chars = rest.chars();
    let mut code_str = String::with_capacity(3);
    for _ in 0..3 {
        let c = chars.next()?;
        if !c.is_ascii_digit() {
            return None;
        }
        code_str.push(c);
    }
    match chars.next() {
        None | Some(' ') => {}
        Some(_) => return None,
    }
    let code: u16 = code_str.parse().ok()?;
    if (100..=599).contains(&code) {
        Some(code)
    } else {
        None
    }
}

/// readiness probe の応答本文に含まれていれば「CDP `/json/version` らしい」
/// と判定するフィールド名（大小文字を区別しない）。
///
/// `fandhe-browser-cdp` crate が対象とする CDP プロトコルの
/// `Browser`/`Protocol-Version` に加え、Chromium 系 CDP 実装が一般的に返す
/// `webSocketDebuggerUrl`・`User-Agent`・`V8-Version`・`WebKit-Version` も
/// 許容する（`fandhe-browser-cli` は未実装で確定した応答形が無いため、
/// 単一のフィールド名に絞らず既知の候補を並べる）。
const READINESS_RESPONSE_FIELDS: &[&str] = &[
    "browser",
    "protocol-version",
    "websocketdebuggerurl",
    "user-agent",
    "v8-version",
    "webkit-version",
];

/// readiness probe の応答本文が「起動した子プロセス（ブラウザ）からの
/// 応答らしいか」を判定する。
///
/// レビュー指摘 Medium（Cursor。PR #442 再々々々々レビュー・
/// competitor_lightpanda.rs:486-535）: `reserve_port` は bind したリスナーを
/// 即座に drop してから子プロセスを起動するため、その間に別プロセスが
/// 同じポートを奪える TOCTOU が残る。ポートが「空いているか」ではなく
/// 「応答している内容が期待するブラウザらしいか」を検証することで、
/// 無関係なプロセスの応答を子プロセスの readiness と誤認する可能性を
/// 下げる。CDP の `/json/version` は JSON オブジェクトで
/// [`READINESS_RESPONSE_FIELDS`] のいずれかのフィールドを持つのが
/// 一般的なため、本文が JSON オブジェクトであり、かつそのいずれかの
/// キー（大小文字を区別しない）を持つことを要求する（レビュー指摘
/// advisor 追加指摘: 「JSON オブジェクトでありさえすれば `{}` でも通る」
/// のは弱すぎるため、少なくとも 1 つの既知フィールドを要求するよう
/// 強化した）。ステータス行だけで判定していた以前の実装より強いが、
/// 別プロセスがたまたま同じ形の JSON を返す場合までは排除できない
/// （呼び出し元 `spawn_and_wait_ready` の `Child::try_wait` による生存
/// 確認と組み合わせた best-effort。完全な防止には子プロセスが実際に
/// そのポートを bind していることの確認が要るが、std だけでは OS
/// 非依存にソケットの所有プロセスを調べる手段が無く、`unsafe`・新規
/// 依存なしでは実装しない）。
pub fn looks_like_browser_readiness_response(body: &str) -> bool {
    let Ok(JsonValue::Object(fields)) = parse_json(body) else {
        return false;
    };
    fields
        .keys()
        .any(|key| READINESS_RESPONSE_FIELDS.contains(&key.to_ascii_lowercase().as_str()))
}

/// unix `ps -o rss= -p <pid>` の標準出力（キロバイト単位の数値のみ・前後に空白を
/// 含み得る）を解釈する。
///
/// アイドル RSS 計測（`PERF-6`）が unix でのみ呼ぶ（Windows は
/// `Unsupported`。実装計画 3.2 節）。空文字列・数値でない出力は `None` を返す。
///
/// 呼び出し元（`competitor_lightpanda.rs` の `sample_rss_kb`）が
/// `#[cfg(unix)]` 限定のため、この関数自体も `#[cfg(unix)]` にする。
/// 無条件公開のままだと Windows ネイティブビルドで到達不能になり
/// `dead_code` 警告が `-D warnings`（ci.md「3 OS CI」）で fail する。
#[cfg(unix)]
pub fn parse_ps_rss_kb(out: &str) -> Option<u64> {
    out.trim().parse::<u64>().ok()
}

/// テンプレート引数中の `{port}` プレースホルダを実ポート番号へ展開する。
///
/// `Target::serve_args`（実装計画 3.1 節）に対して起動直前に呼ぶ。プレースホルダを
/// 含まない要素はそのまま返す（トークン置換のみ・シェル展開はしない。
/// security.md「インジェクション」対策としてシェルを経由しないコマンド起動と組む）。
pub fn expand_args(template: &[String], port: u16) -> Vec<String> {
    template
        .iter()
        .map(|arg| arg.replace("{port}", &port.to_string()))
        .collect()
}

/// 環境変数の値（空白区切り）を引数ベクタへ分割する。
///
/// `FANDHE_BROWSER_SERVE_ARGS` 等の環境変数から起動引数を受け取る経路が使う。
/// クォート解釈はせず、素朴な空白分割に留める（シェル評価をしないことで
/// インジェクションを避ける方針。実装計画セクション 6）。
pub fn split_args(env: &str) -> Vec<String> {
    env.split_whitespace().map(str::to_string).collect()
}

/// 計測 1 件の結果。成功しなかった場合も理由を残し「実装済みを装わない」
/// （coding-rust.md「公開 API」・REPAIR-4 相当の方針をベンチ出力にも適用）。
///
/// `main`（`competitor_lightpanda.rs`）が構築し、[`bench_exit_code`] の入力
/// および JSON 出力（[`Outcome::to_json`]）に使う。純粋関数群と同じ
/// `support.rs` に置くことで、[`bench_exit_code`] のテスト（`Skipped`/
/// `Unsupported` のみでは `0`、`Error` を含むと非ゼロ）を副作用なしに書ける
/// （レビュー指摘 P1。Codex。PR #442・competitor_lightpanda.rs:893）。
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Value(f64),
    Skipped(String),
    // Windows（`cfg(windows)` 版の `measure_idle_rss`）でのみ構築される
    // バリアント。Linux/macOS ネイティブビルドではこのバリアントを構築する
    // コード経路が存在しないため dead_code 警告が出るが、3 OS CI
    // （ci.md）の各ネイティブランナーでは Windows ビルド時に使われる。
    #[allow(dead_code)]
    Unsupported(String),
    Error(String),
}

impl Outcome {
    /// この計測項目が失敗（`Outcome::Error`）かどうかを返す。
    ///
    /// `main` が全計測項目を走査して終了コード（[`bench_exit_code`]）を
    /// 決めるために使う。
    fn is_error(&self) -> bool {
        matches!(self, Outcome::Error(_))
    }

    /// `{"status":"...","value":...}` 形式の JSON 断片を返す（依存追加を避けるため
    /// 手書きシリアライズ。同ファイルの [`json_escape`] を使う）。
    pub fn to_json(&self) -> String {
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

/// 全計測結果から `main` の終了コードを決める。
///
/// レビュー指摘 P1（Codex。competitor_lightpanda.rs:893）: `Outcome::Error`
/// （対象バイナリの起動失敗・MCP 呼び出し失敗等の計測失敗）が発生しても
/// `main` が常に正常終了すると、自動計測が失敗した実行を成功と誤判定し得る。
/// `main` の終了コード契約: 1 件でも計測失敗（`Outcome::Error`）があれば
/// 非ゼロ（`1`）で終了する。対象バイナリ未設定による `Outcome::Skipped`・
/// Windows 未対応による `Outcome::Unsupported` のみの場合は `0` で終了する
/// （JSON は失敗の有無に関わらず必ず stdout へ出力する）。`main`（`[[bench]]`
/// ターゲット）は `std::process::ExitCode` を返す契約のため `u8` で返す。
pub fn bench_exit_code(outcomes: &[&Outcome]) -> u8 {
    if outcomes.iter().any(|o| o.is_error()) {
        1
    } else {
        0
    }
}

/// 出力 JSON へ埋め込む文字列をエスケープする。
///
/// 手書き JSON シリアライズ（実装計画 3.3 節。新規依存を避けるため `serde_json`
/// は使わない）で使う。制御文字は `\uXXXX` 形式にする。
pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `goto` へ渡す URL がベンチ自身の起動したローカル fixture サーバー
/// （127.0.0.1・固定ポートではなく起動時に確保したポート）だけを指すことを
/// 検証する。
///
/// レビュー指摘 P0（Codex。PR #442 再レビュー・competitor_lightpanda.rs:729/783）:
/// 計測対象ブラウザは接続時に名前を再解決でき、また `goto` 先が公開 URL
/// でもそこからのリダイレクトに追従し得るため、事前検証（DNS 解決結果の
/// チェックを含む）を積み増しても「ブラウザが実際に接続する宛先」を
/// 保証できない（TOCTOU）。この問題は「検証を強化する」のではなく
/// 「外部ネットワークに一切出ない構成にする」ことでしか解消できないため、
/// `COMPETITOR_BENCH_SITES` による任意外部 URL の指定を廃止し、ベンチが
/// 自ら起動したローカル静的サーバー（`competitor_lightpanda.rs` の
/// `FixtureServer`）が配信する fixture ページだけを `goto` する設計へ
/// 変更した。この関数はその最後の防波堤として、生成した URL が本当に
/// `http://127.0.0.1:<起動したポート>/...` の形であることを確認する
/// （scheme が `http`・ホストが `127.0.0.1`・ポートが一致することを要求する）。
pub fn validate_local_bench_url(url: &str, expected_port: u16) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|err| format!("{url}: invalid URL ({err})"))?;
    if parsed.scheme() != "http" {
        return Err(format!("{url}: only http is allowed for local fixtures"));
    }
    match parsed.host_str() {
        Some("127.0.0.1") => {}
        _ => return Err(format!("{url}: host must be 127.0.0.1")),
    }
    match parsed.port() {
        Some(port) if port == expected_port => {}
        _ => {
            return Err(format!(
                "{url}: port must match the fixture server port ({expected_port})"
            ));
        }
    }
    Ok(())
}

/// `AISNAP-1`（MCP トークン削減率）計測が `goto` する fixture ページの
/// パス → (content-type, 内容) テーブル。
///
/// レビュー指摘 P0・P1（Codex。PR #442 再々レビュー・
/// competitor_lightpanda.rs:338/352）: 以前は fixture をファイルシステムから
/// 実行時に読んでいたため、(1) `safe_join_fixture_path` が字面上の `..` を
/// 拒否しても `metadata`/`read` はシンボリックリンクを辿ってしまい、fixture
/// 配下にルート外を指すリンクを置かれるとそれを配信し得た、(2)
/// `metadata` でサイズを確認した後に `read` するまでの間にファイルが
/// 大きくなる TOCTOU で上限を超えて確保し得た。fixture をコンパイル時に
/// `include_str!` でバイナリへ埋め込み、パスからの検索をこの固定テーブルの
/// 完全一致だけにすることで、シンボリックリンク・TOCTOU・サイズ超過が
/// 構造的に起こらないようにする（実行時のファイル I/O 自体をなくす）。
/// fixture を追加・削除した場合は [`fixtures_contain_no_external_references`]
/// にも反映すること。
pub const FIXTURE_TABLE: &[(&str, &str, &str)] = &[
    (
        "/article.html",
        "text/html; charset=utf-8",
        include_str!("fixtures/article.html"),
    ),
    (
        "/listing.html",
        "text/html; charset=utf-8",
        include_str!("fixtures/listing.html"),
    ),
    (
        "/form.html",
        "text/html; charset=utf-8",
        include_str!("fixtures/form.html"),
    ),
];

/// リクエストパスに完全一致する fixture の `(content-type, 内容)` を返す。
///
/// [`FIXTURE_TABLE`] のキーとの完全一致だけで引く（`..`・クエリ文字列・
/// 末尾スラッシュの有無等を正規化しない）。一致しなければ `None`
/// （呼び出し側は 404 を返す。`competitor_lightpanda.rs` の
/// `handle_fixture_connection`）。
pub fn lookup_fixture(request_path: &str) -> Option<(&'static str, &'static str)> {
    FIXTURE_TABLE
        .iter()
        .find(|(path, _, _)| *path == request_path)
        .map(|(_, content_type, content)| (*content_type, *content))
}

/// 対象バイナリが設定済みならその参照を `Ok` で返し、未設定なら全計測項目で
/// 共通の `Outcome::Skipped` を `Err` で返す。
///
/// レビュー指摘 P1（Codex。PR #442 再々々レビュー・
/// competitor_lightpanda.rs:593）: `measure_idle_rss` の `#[cfg(windows)]`
/// 版が `target.bin` を確認せずに常に `Outcome::Unsupported` を返しており、
/// 「対象バイナリ未設定なら全計測項目が `Outcome::Skipped`」という契約に
/// 反していた（`<PREFIX>_BIN` 未設定でも `idleRssKb` だけ `unsupported` に
/// なる）。この判定は本来 OS に依存しない（`target.bin` の有無だけで決まる）
/// ため、`competitor_lightpanda.rs` の `measure_cold_start`・
/// `measure_binary_size`・`measure_token_reduction`・`#[cfg(unix)]`/
/// `#[cfg(windows)]` 両方の `measure_idle_rss`・[`token_reduction_gate`]
/// がすべてこの 1 関数を通して判定することで、個別に同じ分岐を書いて
/// 食い違いを生む余地をなくす。
/// `Result` にして `bin` そのものを返すことで、呼び出し側は
/// `let Some(bin) = &target.bin else { unreachable!() }` のような
/// 冗長かつ不変条件に依存する再チェックを書かずに済む。
pub fn require_bin<'a>(
    bin: Option<&'a PathBuf>,
    target_name: &str,
) -> Result<&'a PathBuf, Outcome> {
    bin.ok_or_else(|| Outcome::Skipped(format!("{target_name}: binary path not configured")))
}

/// `AISNAP-1` 計測（`competitor_lightpanda.rs` の `measure_token_reduction`
/// 呼び出し前）が、対象バイナリの有無と fixture サーバーの起動結果から
/// `Outcome` を確定できるかを判定する。
///
/// レビュー指摘 P1（Codex。PR #442 再々レビュー・
/// competitor_lightpanda.rs:1058）: 片方の対象だけに `_BIN` を設定した状態で
/// fixture サーバーの起動（`bind`）が失敗すると、以前は未設定の対象にも
/// `Outcome::Error` を割り当てていた（「対象バイナリ未設定は常に
/// `Outcome::Skipped`」という契約に反する）。バイナリ未設定の判定自体は
/// [`require_bin`] と共通化し、この関数はそれに加えて fixture サーバーの
/// 起動結果を見る（バイナリが設定済みでサーバー起動が失敗していた場合だけ
/// `Some(Outcome::Error)` を返す）。両方問題なければ `None`
/// （呼び出し側が実際に `measure_token_reduction` を呼ぶ）。
pub fn token_reduction_gate(
    bin: Option<&PathBuf>,
    server_start_error: Option<&str>,
    target_name: &str,
) -> Option<Outcome> {
    if let Err(skipped) = require_bin(bin, target_name) {
        return Some(skipped);
    }
    if let Some(err) = server_start_error {
        return Some(Outcome::Error(format!(
            "{target_name}: fixture server failed to start: {err}"
        )));
    }
    None
}

/// `<PREFIX>_SERVE_ARGS`/`<PREFIX>_MCP_ARGS` の検証エラーがあれば
/// `Outcome::Error` を返す。無ければ `None`（呼び出し側が実際の計測を
/// 続ける）。
///
/// レビュー指摘 P2（Codex。PR #442 再々々々レビュー・
/// competitor_lightpanda.rs:511/554）: 以前は `Target::args_error` が
/// `SERVE_ARGS`・`MCP_ARGS` 両方の検証エラーを 1 フィールドへまとめていた
/// ため、`MCP_ARGS` だけが上限超過でも、`MCP_ARGS` を使わない cold start
/// （`PERF-3`）・アイドル RSS（`PERF-6`）計測まで計測前に `Outcome::Error`
/// になっていた（逆に `SERVE_ARGS` だけの超過で `AISNAP-1` も巻き込まれる）。
/// `Target` 側を `serve_args_error`・`mcp_args_error` の 2 フィールドに
/// 分け、各計測関数（`measure_cold_start`・unix 版 `measure_idle_rss` は
/// `serve_args_error`、`measure_token_reduction` は `mcp_args_error`）が
/// 自分の使う引数のエラーだけをこの関数経由で確認するようにした。
pub fn arg_error_gate(arg_error: Option<&str>, target_name: &str) -> Option<Outcome> {
    arg_error.map(|reason| Outcome::Error(format!("{target_name}: {reason}")))
}

/// HTTP リクエストの先頭行（例: `GET /article.html HTTP/1.1`）から
/// メソッドとパスを取り出す。
///
/// fixture 配信サーバー（`competitor_lightpanda.rs` の
/// `handle_fixture_connection`）が使う。書式に合わない行は `None` にし、
/// 呼び出し側が 400 相当のエラー応答を返せるようにする（panic させない。
/// coding-rust.md）。
pub fn parse_http_request_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let version = parts.next()?;
    // 余分なトークンがあれば不正なリクエスト行として扱う。
    if parts.next().is_some() {
        return None;
    }
    if !is_valid_http_version(version) {
        return None;
    }
    Some((method.to_string(), path.to_string()))
}

/// `"HTTP/"` に続けて `<数字列>.<数字列>`（例: `HTTP/1.1`・`HTTP/1.0`）の
/// 形式かどうかを判定する。
///
/// レビュー指摘（コーディネーター指示。PR #442 再々々々レビュー）:
/// 以前は `version.starts_with("HTTP/")` だけで判定しており、
/// `HTTP/potato` のような不正な値も通っていた。バージョン番号部分が
/// 数字のみの 2 要素であることまで確認する。
pub fn is_valid_http_version(version: &str) -> bool {
    let Some(rest) = version.strip_prefix("HTTP/") else {
        return false;
    };
    let mut parts = rest.split('.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.chars().all(|c| c.is_ascii_digit())
        && minor.chars().all(|c| c.is_ascii_digit())
}

/// ソケット読み取りの `io::Error` から、fixture 配信サーバーが返すべき
/// HTTP ステータスコードを決める。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
/// competitor_lightpanda.rs:352）: リクエスト行・ヘッダ行の読み取りが
/// タイムアウト・サイズ超過・その他の I/O エラーで失敗した場合、以前は
/// ループを抜けるだけで後続の処理（`lookup_fixture` によるパス一致判定・
/// `200` 応答）へ進んでしまい、読み取り未完了のまま既知のパスなら成功応答を
/// 返し得た。読み取りが完了しなかった経路はすべてこの関数で判定した
/// ステータスで応答してから接続を終了する契約にする
/// （`ErrorKind::WouldBlock`/`TimedOut` はソケットの読み取りタイムアウト
/// （`FIXTURE_IO_TIMEOUT`）由来なので `408 Request Timeout`、それ以外
/// （接続の早期切断・`MAX_LINE_BYTES` 超過等）は `400 Bad Request`）。
pub fn http_status_for_io_error(kind: std::io::ErrorKind) -> (u16, &'static str) {
    match kind {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => (408, "Request Timeout"),
        _ => (400, "Bad Request"),
    }
}

/// 外部コマンド（`kill`/`taskkill`）の終了ステータスを、成功したかどうかの
/// 判定へ変換する。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々々々々レビュー・
/// competitor_lightpanda.rs:558）: `competitor_lightpanda.rs` の
/// `kill_process_group` は以前 `Command::status()` の結果を
/// `let _ = ...` で握りつぶしており、外部コマンドの非 0 終了（対象
/// プロセスが既に存在しない等）に気づけなかった。判定ロジック自体を
/// この純粋関数へ切り出し、`kill_process_group` はプロセス起動
/// （`unsafe`・実際の子プロセス操作を伴い、この crate root
/// （`[[test]]` ターゲット）では単体テストできない）と、この関数が行う
/// 判定とに分離する。
pub fn exit_status_to_result(status: std::process::ExitStatus) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}

/// `Content-Length` ヘッダーの値（コロンの後ろ、前後の空白を含み得る）を
/// 厳密に解釈する。
///
/// レビュー指摘 P2（コーディネーター指示。PR #442 再々々々々々々レビュー）:
/// 以前は `value.trim().parse::<usize>().ok()` で解釈しており、パースに
/// 失敗した場合は素通りして `content_length` を `None`（＝ヘッダーが
/// 無かった場合と同じ「長さ指定なし」）にしていた。しかし `Content-Length`
/// ヘッダー自体は存在しているので、値が不正（非数値・空・符号付き等）
/// なら「長さ指定なし」ではなく probe 失敗として扱うべきである
/// （呼び出し元 `probe_once` はこの関数が `None` を返したら `Content-Length`
/// が無かった場合とは区別して即座に probe 失敗にする契約とする）。
/// `usize::parse` は非負整数でも `"+200"` のような符号付き表記を受理して
/// しまうため、ここでは前後の空白を取り除いた後、ASCII 数字のみで構成
/// された非空文字列であることを明示的に検証してから `parse` する。
pub fn parse_content_length(value: &str) -> Option<usize> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<usize>().ok()
}

/// readiness probe が受け取った `Content-Length` が、読み取りに許す上限
/// （`competitor_lightpanda.rs` の `PROBE_BODY_MAX_BYTES`）を超えているかを
/// 判定する。
///
/// レビュー指摘 P2（Codex。PR #442 再々々々々々々レビュー・
/// competitor_lightpanda.rs:756）: 以前は `Content-Length` が上限を超えて
/// いても `len.min(max_bytes)` で黙って切り詰め、その範囲だけ読めれば
/// probe 成功として扱っていた（サーバーが実際に宣言した長さの応答を
/// 確認しないまま成功と判定し得た）。呼び出し元（`probe_once`）は、この
/// 関数が `true` を返したら読み取りを試みず即座に probe 失敗にする契約
/// とする（「上限で黙って切り詰めて成功扱いする」経路を無くす）。
pub fn content_length_exceeds_limit(content_length: Option<usize>, max_bytes: usize) -> bool {
    content_length.is_some_and(|len| len > max_bytes)
}

/// `TcpStream` を包み、`read`/`write` のたびに残り時間を
/// `set_read_timeout`/`set_write_timeout` へ反映することで、個々の
/// 呼び出しではなく「この接続・この probe」全体に対する絶対期限
/// （`Instant`）を守らせる `Read`/`Write` 実装。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
/// competitor_lightpanda.rs:364/682/711）: `set_read_timeout` は 1 回の
/// `read` 呼び出しにしか効かない。相手が 1 バイトずつ小分けに送り続ける
/// （slow-loris 型）と、個々の `read` は毎回タイムアウト内に完了して
/// しまうため、`BufRead::read_line` 相当の呼び出し全体としては無期限に
/// 時間を消費し得た。fixture サーバーの接続 1 本・readiness probe の
/// 1 回それぞれで場当たり的にタイムアウトを設定し直すのではなく、この
/// 1 つのプリミティブに統一する（`competitor_lightpanda.rs` の
/// `handle_fixture_connection`・`probe_once` の両方がこれ経由でのみ
/// ソケットを読み書きする）。`read`/`write` のたびに
/// `deadline.saturating_duration_since(Instant::now())` を計算し、
/// 残りが 0 なら実際には OS の `read`/`write` を呼ばず
/// `ErrorKind::TimedOut` を返す。`write_all`（`Write` トレイトの
/// デフォルト実装が `write` を繰り返し呼ぶ）もこの仕組みに自動的に
/// 従うため、書き込み側も同じプリミティブで期限を守る（レビュー指摘:
/// 「書き込み側も同様に確認し、必要なら `set_write_timeout` とあわせて
/// 期限を守らせる」）。
///
/// この型だけは実ソケット I/O を行う（モジュールドキュメント参照）。
/// 外部プロセスを起動せずローカルの loopback ソケットだけで
/// 動作を検証できるため、単体テストもこのファイルに置く。
pub struct DeadlineReader {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineReader {
    pub fn new(stream: TcpStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }
}

/// OS のソケットタイムアウトに由来する `io::Error` を、共通の
/// `ErrorKind::TimedOut` へ正規化する。
///
/// レビュー指摘（Cursor。PR #442 再々々々々々々レビュー・
/// competitor_lightpanda.rs:1893。macOS CI 失敗）: unix のブロッキング
/// ソケットは `set_read_timeout`/`set_write_timeout` の期限切れで
/// `ErrorKind::WouldBlock` を返す（Linux では `TimedOut` を返すため、
/// この違いに気づかれにくい）。`DeadlineReader` 呼び出し側（
/// `deadline_reader_cuts_off_slow_loris_sender` 等）が `TimedOut` だけを
/// 期待していると macOS で失敗する。`DeadlineReader::read`/`write` の
/// 時点でこの 2 つを区別なく `TimedOut` へ正規化し、以降の呼び出し元
/// （fixture サーバー・probe・`support::http_status_for_io_error` 等）が
/// OS 差異を意識せずに済むようにする。
fn normalize_timeout_error(err: std::io::Error) -> std::io::Error {
    match err.kind() {
        std::io::ErrorKind::WouldBlock => std::io::Error::new(std::io::ErrorKind::TimedOut, err),
        _ => err,
    }
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline exceeded while reading",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf).map_err(normalize_timeout_error)
    }
}

impl Write for DeadlineReader {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline exceeded while writing",
            ));
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buf).map_err(normalize_timeout_error)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// 手書き最小 JSON パーサーが返す値。
///
/// MCP（`mcp` サブコマンド）が stdio 経由で返す NDJSON 応答行から、呼び出し
/// `id` の照合と `result.content[].text` の抽出だけを行う用途に限定した最小限の
/// 値表現（`AISNAP-1`）。汎用 JSON ライブラリ（`serde_json`）は新規依存になる
/// ため使わない（dependency-policy.md）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

impl JsonValue {
    /// `Object` のときだけ指定キーの値を返す。MCP 応答の `id`・`result` 等の
    /// フィールド取り出しに使う（外部入力なので `get` 相当の非 panic API のみ提供）。
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// `Array` のときだけ指定 index の値を返す。
    pub fn index(&self, i: usize) -> Option<&JsonValue> {
        match self {
            JsonValue::Array(items) => items.get(i),
            _ => None,
        }
    }

    /// `String` のときだけ中身を借用する。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// `Number` のときだけ `f64` を返す。MCP 応答の `id`（数値）照合に使う。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// `Bool` のときだけ中身を返す。`result.isError` の型検証に使う
    /// （レビュー指摘 P1。Codex。PR #442 再々々々レビュー・
    /// competitor_lightpanda.rs:839）。
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// `Array` のときだけ要素列を借用する。`result.content` の形検証に使う。
    pub fn as_array(&self) -> Option<&Vec<JsonValue>> {
        match self {
            JsonValue::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// MCP サーバー（stdio 越しの JSON-RPC 2.0）からの応答 1 件が、要求した
/// `method` に対する妥当な応答の形をしているかを検証する。
///
/// レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
/// competitor_lightpanda.rs:839）: 以前は `id` が一致し `result.isError` が
/// `true` でなければ成功として扱っており、`result` が `null`・`{}` の
/// ような空応答でも `goto` の成功と誤判定し得た（`goto` は本文を見ない
/// ため、ページ遷移に失敗した応答をそのまま成功扱いにし、続く `html`・
/// `tree` を別ページの結果として計測する経路になり得た）。ここで
/// JSON-RPC 2.0 の封筒（`jsonrpc`・`id`・`error`/`result` の排他）と、
/// `method` ごとに要求される `result` の最小限の形を検証する
/// （`tools/call` は `content` が空でない配列で各要素に `type` を持ち、
/// `type == "text"` なら `text` が文字列であること・`isError` があるなら
/// bool であることまで。`initialize` は `result` がオブジェクトで、
/// `protocolVersion` が文字列・`capabilities` がオブジェクトであることまで
/// （`{}` のような空応答を成功とみなさない）。それ以外のメソッド
/// （`tools/list` 等。現状このベンチからは呼ばない）は `result` が
/// JSON オブジェクトであることまで）。`error` 応答は封筒として妥当なら
/// `Ok(())` を返す（`error.message` の取り出しは呼び出し元
/// `McpClient::call` の既存処理に委ねる）。
pub fn validate_mcp_response(
    value: &JsonValue,
    expected_id: f64,
    method: &str,
) -> Result<(), String> {
    match value.get("jsonrpc").and_then(JsonValue::as_str) {
        Some("2.0") => {}
        _ => return Err("response missing or invalid \"jsonrpc\":\"2.0\"".to_string()),
    }
    match value.get("id").and_then(JsonValue::as_f64) {
        Some(id) if id == expected_id => {}
        _ => return Err("response \"id\" does not match the request".to_string()),
    }
    let error = value.get("error");
    let result = value.get("result");
    match (error, result) {
        (Some(_), Some(_)) => Err("response has both \"error\" and \"result\"".to_string()),
        (None, None) => Err("response has neither \"error\" nor \"result\"".to_string()),
        (Some(_), None) => Ok(()),
        (None, Some(result)) => validate_mcp_result_shape(result, method),
    }
}

/// `method` ごとに要求される `result` の最小限の形を検証する
/// （[`validate_mcp_response`] のヘルパー）。
fn validate_mcp_result_shape(result: &JsonValue, method: &str) -> Result<(), String> {
    match method {
        "tools/call" => {}
        // レビュー指摘（コーディネーター指示。PR #442 再々々々レビュー・
        // competitor_lightpanda.rs:839）: 「`initialize` や `tools/list`
        // など、ほかの MCP 呼び出しの応答の形も同じ方針で確認する」ため、
        // `initialize` は MCP 2025-06-18 の `InitializeResult` が持つべき
        // 必須フィールド（`protocolVersion`・`capabilities`）まで確認する
        // （`result: {}` のような空応答をオブジェクトというだけで成功と
        // みなさない）。
        "initialize" => {
            if !matches!(result, JsonValue::Object(_)) {
                return Err("initialize \"result\" is not a JSON object".to_string());
            }
            if result
                .get("protocolVersion")
                .and_then(JsonValue::as_str)
                .is_none()
            {
                return Err("initialize \"result.protocolVersion\" is not a string".to_string());
            }
            return if matches!(result.get("capabilities"), Some(JsonValue::Object(_))) {
                Ok(())
            } else {
                Err("initialize \"result.capabilities\" is not an object".to_string())
            };
        }
        // 上記以外（`tools/list` 等。現状このベンチからは呼ばない）は、
        // 最低限 `result` が JSON オブジェクトであることまで確認する。
        _ => {
            return if matches!(result, JsonValue::Object(_)) {
                Ok(())
            } else {
                Err(format!("{method}: \"result\" is not a JSON object"))
            };
        }
    }
    if !matches!(result, JsonValue::Object(_)) {
        return Err("tools/call \"result\" is not a JSON object".to_string());
    }
    if let Some(is_error) = result.get("isError")
        && is_error.as_bool().is_none()
    {
        return Err("tools/call \"result.isError\" is not a bool".to_string());
    }
    let Some(content) = result.get("content") else {
        return Err("tools/call \"result\" is missing \"content\"".to_string());
    };
    let Some(items) = content.as_array() else {
        return Err("tools/call \"result.content\" is not an array".to_string());
    };
    if items.is_empty() {
        return Err("tools/call \"result.content\" is empty".to_string());
    }
    for (i, item) in items.iter().enumerate() {
        let Some(item_type) = item.get("type").and_then(JsonValue::as_str) else {
            return Err(format!(
                "tools/call \"result.content[{i}]\" is missing a string \"type\""
            ));
        };
        if item_type == "text" && item.get("text").and_then(JsonValue::as_str).is_none() {
            return Err(format!(
                "tools/call \"result.content[{i}]\" has type \"text\" but no string \"text\""
            ));
        }
    }
    Ok(())
}

/// JSON パース失敗の理由。呼び出し元（MCP 応答の逐次パース）は該当行を
/// `error` 扱いにして次のサイトの計測を継続する（実装計画 4 節）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonParseError {
    UnexpectedEnd,
    UnexpectedChar(char),
    InvalidNumber,
    InvalidEscape,
    InvalidSurrogate,
    DepthExceeded,
    TrailingData,
}

/// 再帰下降の深さ上限。MCP 応答は数階層のオブジェクト・配列のネストに留まる
/// 想定だが、外部プロセスからの入力を無制限に信用しないため上限を設ける
/// （coding-rust.md「長さ・件数を上限検証してからアロケーションに使う」）。
const MAX_JSON_DEPTH: usize = 64;

/// 最小 JSON パーサーのエントリポイント。末尾に空白以外のデータが残っていたら
/// `TrailingData` にする（NDJSON の 1 行 = 1 JSON 値という前提を守るため）。
pub fn parse_json(input: &str) -> Result<JsonValue, JsonParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    let value = parse_value(&chars, &mut pos, 0)?;
    skip_whitespace(&chars, &mut pos);
    if pos != chars.len() {
        return Err(JsonParseError::TrailingData);
    }
    Ok(value)
}

fn skip_whitespace(chars: &[char], pos: &mut usize) {
    while let Some(&c) = chars.get(*pos) {
        if c.is_whitespace() {
            *pos += 1;
        } else {
            break;
        }
    }
}

fn peek(chars: &[char], pos: usize) -> Option<char> {
    chars.get(pos).copied()
}

fn parse_value(chars: &[char], pos: &mut usize, depth: usize) -> Result<JsonValue, JsonParseError> {
    if depth > MAX_JSON_DEPTH {
        return Err(JsonParseError::DepthExceeded);
    }
    skip_whitespace(chars, pos);
    match peek(chars, *pos) {
        None => Err(JsonParseError::UnexpectedEnd),
        Some('"') => parse_string(chars, pos).map(JsonValue::String),
        Some('{') => parse_object(chars, pos, depth),
        Some('[') => parse_array(chars, pos, depth),
        Some('t') => parse_literal(chars, pos, "true", JsonValue::Bool(true)),
        Some('f') => parse_literal(chars, pos, "false", JsonValue::Bool(false)),
        Some('n') => parse_literal(chars, pos, "null", JsonValue::Null),
        Some(c) if c == '-' || c.is_ascii_digit() => parse_number(chars, pos),
        Some(c) => Err(JsonParseError::UnexpectedChar(c)),
    }
}

fn parse_literal(
    chars: &[char],
    pos: &mut usize,
    literal: &str,
    value: JsonValue,
) -> Result<JsonValue, JsonParseError> {
    for expected in literal.chars() {
        match peek(chars, *pos) {
            Some(c) if c == expected => *pos += 1,
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
    }
    Ok(value)
}

fn parse_number(chars: &[char], pos: &mut usize) -> Result<JsonValue, JsonParseError> {
    let start = *pos;
    if peek(chars, *pos) == Some('-') {
        *pos += 1;
    }
    let mut saw_digit = false;
    while let Some(c) = peek(chars, *pos) {
        if c.is_ascii_digit() {
            *pos += 1;
            saw_digit = true;
        } else {
            break;
        }
    }
    if !saw_digit {
        return Err(JsonParseError::InvalidNumber);
    }
    if peek(chars, *pos) == Some('.') {
        *pos += 1;
        let mut saw_frac_digit = false;
        while let Some(c) = peek(chars, *pos) {
            if c.is_ascii_digit() {
                *pos += 1;
                saw_frac_digit = true;
            } else {
                break;
            }
        }
        if !saw_frac_digit {
            return Err(JsonParseError::InvalidNumber);
        }
    }
    if matches!(peek(chars, *pos), Some('e') | Some('E')) {
        *pos += 1;
        if matches!(peek(chars, *pos), Some('+') | Some('-')) {
            *pos += 1;
        }
        let mut saw_exp_digit = false;
        while let Some(c) = peek(chars, *pos) {
            if c.is_ascii_digit() {
                *pos += 1;
                saw_exp_digit = true;
            } else {
                break;
            }
        }
        if !saw_exp_digit {
            return Err(JsonParseError::InvalidNumber);
        }
    }
    let text: String = chars
        .get(start..*pos)
        .ok_or(JsonParseError::InvalidNumber)?
        .iter()
        .collect();
    text.parse::<f64>()
        .map(JsonValue::Number)
        .map_err(|_| JsonParseError::InvalidNumber)
}

fn parse_string(chars: &[char], pos: &mut usize) -> Result<String, JsonParseError> {
    // 呼び出し元で `"` を確認済みの前提（`parse_value` の分岐から呼ばれる）。
    *pos += 1;
    let mut out = String::new();
    loop {
        match peek(chars, *pos) {
            None => return Err(JsonParseError::UnexpectedEnd),
            Some('"') => {
                *pos += 1;
                return Ok(out);
            }
            Some('\\') => {
                *pos += 1;
                let escaped = peek(chars, *pos).ok_or(JsonParseError::UnexpectedEnd)?;
                match escaped {
                    '"' => {
                        out.push('"');
                        *pos += 1;
                    }
                    '\\' => {
                        out.push('\\');
                        *pos += 1;
                    }
                    '/' => {
                        out.push('/');
                        *pos += 1;
                    }
                    'b' => {
                        out.push('\u{0008}');
                        *pos += 1;
                    }
                    'f' => {
                        out.push('\u{000C}');
                        *pos += 1;
                    }
                    'n' => {
                        out.push('\n');
                        *pos += 1;
                    }
                    'r' => {
                        out.push('\r');
                        *pos += 1;
                    }
                    't' => {
                        out.push('\t');
                        *pos += 1;
                    }
                    'u' => {
                        *pos += 1;
                        let high = parse_hex4(chars, pos)?;
                        if (0xD800..=0xDBFF).contains(&high) {
                            // サロゲートペアの上位。直後に下位サロゲート
                            // （`\uDC00`〜`\uDFFF`）が続く前提で結合する。
                            if peek(chars, *pos) != Some('\\') || peek(chars, *pos + 1) != Some('u')
                            {
                                return Err(JsonParseError::InvalidSurrogate);
                            }
                            *pos += 2;
                            let low = parse_hex4(chars, pos)?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(JsonParseError::InvalidSurrogate);
                            }
                            let combined = 0x10000
                                + (u32::from(high) - 0xD800) * 0x400
                                + (u32::from(low) - 0xDC00);
                            let c =
                                char::from_u32(combined).ok_or(JsonParseError::InvalidSurrogate)?;
                            out.push(c);
                        } else if (0xDC00..=0xDFFF).contains(&high) {
                            // 単独の下位サロゲートは不正な値として扱う。
                            return Err(JsonParseError::InvalidSurrogate);
                        } else {
                            let c = char::from_u32(u32::from(high))
                                .ok_or(JsonParseError::InvalidSurrogate)?;
                            out.push(c);
                        }
                    }
                    other => return Err(JsonParseError::UnexpectedChar(other)),
                }
            }
            // レビュー指摘 P2（PR #442）: 未エスケープの U+0000〜U+001F 制御
            // 文字（RFC 8259 の JSON 文法で文字列内に生で現れることを許さない
            // 範囲）を通常文字として受理していた。MCP 応答を untrusted な
            // 外部入力として扱う方針（coding-rust.md）に従い、ここで
            // `JsonParseError` として拒否する。
            Some(c) if (c as u32) < 0x20 => return Err(JsonParseError::UnexpectedChar(c)),
            Some(c) => {
                out.push(c);
                *pos += 1;
            }
        }
    }
}

fn parse_hex4(chars: &[char], pos: &mut usize) -> Result<u16, JsonParseError> {
    let mut value: u16 = 0;
    for _ in 0..4 {
        let c = peek(chars, *pos).ok_or(JsonParseError::UnexpectedEnd)?;
        let digit = c.to_digit(16).ok_or(JsonParseError::InvalidEscape)?;
        value = value
            .checked_mul(16)
            .and_then(|v| v.checked_add(digit as u16))
            .ok_or(JsonParseError::InvalidEscape)?;
        *pos += 1;
    }
    Ok(value)
}

fn parse_array(chars: &[char], pos: &mut usize, depth: usize) -> Result<JsonValue, JsonParseError> {
    // 呼び出し元で `[` を確認済み。
    *pos += 1;
    let mut items = Vec::new();
    skip_whitespace(chars, pos);
    if peek(chars, *pos) == Some(']') {
        *pos += 1;
        return Ok(JsonValue::Array(items));
    }
    loop {
        let value = parse_value(chars, pos, depth + 1)?;
        items.push(value);
        skip_whitespace(chars, pos);
        match peek(chars, *pos) {
            Some(',') => {
                *pos += 1;
            }
            Some(']') => {
                *pos += 1;
                return Ok(JsonValue::Array(items));
            }
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
    }
}

fn parse_object(
    chars: &[char],
    pos: &mut usize,
    depth: usize,
) -> Result<JsonValue, JsonParseError> {
    // 呼び出し元で `{` を確認済み。
    *pos += 1;
    let mut map = HashMap::new();
    skip_whitespace(chars, pos);
    if peek(chars, *pos) == Some('}') {
        *pos += 1;
        return Ok(JsonValue::Object(map));
    }
    loop {
        skip_whitespace(chars, pos);
        if peek(chars, *pos) != Some('"') {
            return Err(JsonParseError::UnexpectedChar(
                peek(chars, *pos).unwrap_or('\0'),
            ));
        }
        let key = parse_string(chars, pos)?;
        skip_whitespace(chars, pos);
        match peek(chars, *pos) {
            Some(':') => *pos += 1,
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
        let value = parse_value(chars, pos, depth + 1)?;
        map.insert(key, value);
        skip_whitespace(chars, pos);
        match peek(chars, *pos) {
            Some(',') => {
                *pos += 1;
            }
            Some('}') => {
                *pos += 1;
                return Ok(JsonValue::Object(map));
            }
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
    }
}

#[cfg(test)]
mod tests {
    // Cargo は `[[bench]]` ターゲット（本ファイルを `#[path]` で取り込む
    // `competitor_lightpanda.rs`）にも常に `--cfg test`（`--test` フラグ自体は
    // 付けない）を渡すため、この `mod tests` はベンチ側のビルドでも解析対象になる。
    // その場合 `--test` 抜きでは組み込みの `#[test]` 属性がアイテムを剥ぎ取り
    // 本体が空になるため、この glob import は「未使用」と判定される
    // （`[[test]]` ターゲット（`cargo test`）としての単独ビルドでは実際に使われる）。
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use std::time::Duration;

    // PERF-3・PERF-6: median は cold start / RSS の複数試行から代表値を出す。
    #[test]
    fn median_even_count_averages_middle_two() {
        assert_eq!(median(&[1.0, 3.0, 5.0, 7.0]), Some(4.0));
    }

    #[test]
    fn median_odd_count_returns_middle() {
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
    }

    #[test]
    fn median_empty_is_none() {
        assert_eq!(median(&[]), None);
    }

    // PERF-1・PERF-6: reduction_pct は Chromium 比の削減率算出に使う。
    #[test]
    fn reduction_pct_basic() {
        assert_eq!(reduction_pct(100.0, 20.0), Some(80.0));
    }

    #[test]
    fn reduction_pct_zero_baseline_is_none() {
        assert_eq!(reduction_pct(0.0, 20.0), None);
    }

    #[test]
    fn reduction_pct_negative_baseline_is_none() {
        assert_eq!(reduction_pct(-1.0, 20.0), None);
    }

    // AISNAP-1: approx_tokens はトークン削減率計測の近似式。
    #[test]
    fn approx_tokens_rounds_up() {
        assert_eq!(approx_tokens(0), 0);
        assert_eq!(approx_tokens(1), 1);
        assert_eq!(approx_tokens(4), 1);
        assert_eq!(approx_tokens(5), 2);
        assert_eq!(approx_tokens(8), 2);
    }

    // PERF-3: parse_http_status は cold start の readiness probe が使う。
    #[test]
    fn parse_http_status_ok() {
        assert_eq!(parse_http_status("HTTP/1.1 200 OK\r\n"), Some(200));
    }

    #[test]
    fn parse_http_status_malformed_is_none() {
        assert_eq!(parse_http_status("not an http status"), None);
        assert_eq!(parse_http_status(""), None);
        assert_eq!(parse_http_status("HTTP/1.1"), None);
        assert_eq!(parse_http_status("HTTP/1.1 abc"), None);
    }

    // レビュー指摘 P2（コーディネーター指示。PR #442 再々々々々々々
    // レビュー・support.rs:109）: `HTTP/1.0`・`HTTP/1.1` のいずれかに続く
    // 単一の SP、ちょうど 3 桁の数字（100〜599）、その後に SP か行末、
    // という形式だけを受理する。
    #[test]
    fn parse_http_status_accepts_http_1_0_and_1_1() {
        assert_eq!(parse_http_status("HTTP/1.0 200 OK\r\n"), Some(200));
        assert_eq!(parse_http_status("HTTP/1.1 200 OK\r\n"), Some(200));
    }

    #[test]
    fn parse_http_status_accepts_status_without_reason_phrase() {
        // 行末（SP なし）で終わる場合も受理する。
        assert_eq!(parse_http_status("HTTP/1.1 200"), Some(200));
        assert_eq!(parse_http_status("HTTP/1.1 200\r\n"), Some(200));
    }

    #[test]
    fn parse_http_status_accepts_boundary_codes() {
        assert_eq!(parse_http_status("HTTP/1.1 100 Continue\r\n"), Some(100));
        assert_eq!(parse_http_status("HTTP/1.1 599 Unassigned\r\n"), Some(599));
    }

    #[test]
    fn parse_http_status_rejects_unknown_version() {
        assert_eq!(parse_http_status("HTTP/potato 200 OK\r\n"), None);
        assert_eq!(parse_http_status("HTTP/2 200 OK\r\n"), None);
        assert_eq!(parse_http_status("HTTP/2.0 200 OK\r\n"), None);
        assert_eq!(parse_http_status("http/1.1 200 OK\r\n"), None);
    }

    #[test]
    fn parse_http_status_rejects_out_of_range_codes() {
        assert_eq!(parse_http_status("HTTP/1.1 099 OK\r\n"), None);
        assert_eq!(parse_http_status("HTTP/1.1 600 OK\r\n"), None);
        assert_eq!(parse_http_status("HTTP/1.1 000 OK\r\n"), None);
    }

    #[test]
    fn parse_http_status_rejects_wrong_digit_count() {
        assert_eq!(parse_http_status("HTTP/1.1 20 OK\r\n"), None);
        assert_eq!(parse_http_status("HTTP/1.1 2000 OK\r\n"), None);
        assert_eq!(parse_http_status("HTTP/1.1 20a OK\r\n"), None);
    }

    #[test]
    fn parse_http_status_rejects_missing_or_extra_separators() {
        // バージョンとコードの間に SP が無い。
        assert_eq!(parse_http_status("HTTP/1.1200 OK\r\n"), None);
        // SP が 2 つ（余分な空白）。
        assert_eq!(parse_http_status("HTTP/1.1  200 OK\r\n"), None);
    }

    // レビュー指摘 Medium（Cursor。PR #442 再々々々々レビュー・
    // competitor_lightpanda.rs:486-535）: readiness probe の応答本文が
    // ブラウザ（CDP `/json/version`）らしい形かどうかで、別プロセスの
    // 応答をポート再利用によって誤って readiness と判定しないようにする。
    #[test]
    fn looks_like_browser_readiness_response_accepts_known_cdp_fields() {
        assert!(looks_like_browser_readiness_response(
            r#"{"Browser":"Lightpanda/1.0","webSocketDebuggerUrl":"ws://x"}"#
        ));
        assert!(looks_like_browser_readiness_response(
            r#"{"Protocol-Version":"1.3"}"#
        ));
        // フィールド名の大小文字は区別しない。
        assert!(looks_like_browser_readiness_response(r#"{"browser":"x"}"#));
    }

    // レビュー指摘（advisor 追加指摘）: JSON オブジェクトでありさえすれば
    // `{}` でも通ってしまうのは弱すぎるため、既知フィールドを 1 つも
    // 持たないオブジェクトは拒否することを確認する。
    #[test]
    fn looks_like_browser_readiness_response_rejects_object_without_known_fields() {
        assert!(!looks_like_browser_readiness_response(r#"{}"#));
        assert!(!looks_like_browser_readiness_response(r#"{"status":"ok"}"#));
    }

    #[test]
    fn looks_like_browser_readiness_response_rejects_non_object() {
        assert!(!looks_like_browser_readiness_response("not json"));
        assert!(!looks_like_browser_readiness_response(""));
        assert!(!looks_like_browser_readiness_response("[]"));
        assert!(!looks_like_browser_readiness_response("42"));
        assert!(!looks_like_browser_readiness_response("null"));
        assert!(!looks_like_browser_readiness_response("\"just a string\""));
        assert!(!looks_like_browser_readiness_response(
            "<html>not json</html>"
        ));
    }

    // PERF-6: parse_ps_rss_kb は unix の `ps -o rss=` 出力を解釈する
    // （関数定義が `#[cfg(unix)]` のため、テストも合わせて限定する）。
    #[cfg(unix)]
    #[test]
    fn parse_ps_rss_kb_basic() {
        assert_eq!(parse_ps_rss_kb("  12345\n"), Some(12345));
    }

    #[cfg(unix)]
    #[test]
    fn parse_ps_rss_kb_invalid_is_none() {
        assert_eq!(parse_ps_rss_kb(""), None);
        assert_eq!(parse_ps_rss_kb("not a number"), None);
    }

    #[test]
    fn expand_args_replaces_port_placeholder() {
        let template = vec![
            "serve".to_string(),
            "--port".to_string(),
            "{port}".to_string(),
        ];
        assert_eq!(
            expand_args(&template, 9400),
            vec![
                "serve".to_string(),
                "--port".to_string(),
                "9400".to_string()
            ]
        );
    }

    #[test]
    fn expand_args_no_placeholder_is_unchanged() {
        let template = vec!["mcp".to_string()];
        assert_eq!(expand_args(&template, 9400), vec!["mcp".to_string()]);
    }

    #[test]
    fn split_args_splits_on_whitespace() {
        assert_eq!(
            split_args("serve  --port {port}"),
            vec![
                "serve".to_string(),
                "--port".to_string(),
                "{port}".to_string()
            ]
        );
    }

    #[test]
    fn split_args_empty_is_empty() {
        assert_eq!(split_args(""), Vec::<String>::new());
    }

    // レビュー指摘 P1（Codex。competitor_lightpanda.rs:893）: `Skipped`・
    // `Unsupported`（未設定・未対応。失敗ではない）のみでは終了コード 0、
    // `Error`（計測失敗）を 1 件でも含むと非ゼロになることを確認する。
    #[test]
    fn bench_exit_code_is_zero_for_skipped_and_unsupported_only() {
        let skipped = Outcome::Skipped("not configured".to_string());
        let unsupported = Outcome::Unsupported("not supported on this OS".to_string());
        let value = Outcome::Value(1.0);
        assert_eq!(bench_exit_code(&[&skipped, &unsupported, &value]), 0);
    }

    #[test]
    fn bench_exit_code_is_nonzero_when_any_error() {
        let skipped = Outcome::Skipped("not configured".to_string());
        let error = Outcome::Error("binary launch failed".to_string());
        assert_eq!(bench_exit_code(&[&skipped, &error]), 1);
    }

    #[test]
    fn bench_exit_code_is_zero_for_empty_outcomes() {
        assert_eq!(bench_exit_code(&[]), 0);
    }

    #[test]
    fn outcome_to_json_shapes() {
        assert_eq!(
            Outcome::Value(12.5).to_json(),
            r#"{"status":"measured","value":12.5}"#
        );
        assert_eq!(
            Outcome::Skipped("not configured".to_string()).to_json(),
            r#"{"status":"skipped","reason":"not configured"}"#
        );
        assert_eq!(
            Outcome::Unsupported("windows only".to_string()).to_json(),
            r#"{"status":"unsupported","reason":"windows only"}"#
        );
        assert_eq!(
            Outcome::Error("launch failed".to_string()).to_json(),
            r#"{"status":"error","reason":"launch failed"}"#
        );
    }

    #[test]
    fn json_escape_basic() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn json_escape_control_char() {
        assert_eq!(json_escape("\u{0001}"), "\\u0001");
    }

    // AISNAP-1: parse_json は MCP 応答（id 照合・content[].text 抽出）に使う。
    #[test]
    fn parse_json_object_and_get() {
        let value = parse_json(r#"{"id": 1, "result": {"ok": true}}"#).expect("valid json");
        assert_eq!(value.get("id").and_then(JsonValue::as_f64), Some(1.0));
        assert_eq!(
            value.get("result").and_then(|r| r.get("ok")),
            Some(&JsonValue::Bool(true))
        );
    }

    #[test]
    fn parse_json_array_and_index() {
        let value = parse_json(r#"[1, "two", null]"#).expect("valid json");
        assert_eq!(value.index(0).and_then(JsonValue::as_f64), Some(1.0));
        assert_eq!(value.index(1).and_then(JsonValue::as_str), Some("two"));
        assert_eq!(value.index(2), Some(&JsonValue::Null));
        assert_eq!(value.index(3), None);
    }

    #[test]
    fn parse_json_string_with_escapes_and_surrogate_pair() {
        // U+1F600 (😀) は UTF-16 サロゲートペア 😀 で表現される。
        let value = parse_json(r#""a\n\t\"😀""#).expect("valid json");
        assert_eq!(value.as_str(), Some("a\n\t\"\u{1F600}"));
    }

    #[test]
    fn parse_json_lone_surrogate_is_error() {
        let err = parse_json(r#""\uD83D""#).unwrap_err();
        assert_eq!(err, JsonParseError::InvalidSurrogate);
    }

    // レビュー指摘 P2（PR #442）: 未エスケープの制御文字（U+0000〜U+001F）を
    // 含む文字列は不正な JSON として拒否する（RFC 8259）。
    #[test]
    fn parse_json_unescaped_control_char_is_error() {
        let raw = "\"a\u{0001}b\"";
        let err = parse_json(raw).unwrap_err();
        assert_eq!(err, JsonParseError::UnexpectedChar('\u{0001}'));
    }

    #[test]
    fn parse_json_unescaped_newline_in_string_is_error() {
        let raw = "\"a\nb\"";
        let err = parse_json(raw).unwrap_err();
        assert_eq!(err, JsonParseError::UnexpectedChar('\n'));
    }

    #[test]
    fn parse_json_content_text_extraction_shape() {
        // MCP `tools/call` 応答の実際の形（実装計画 3.1 節・4 節）を模す。
        let raw = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"<html></html>"}]}}"#;
        let value = parse_json(raw).expect("valid json");
        let text = value
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.index(0))
            .and_then(|item| item.get("text"))
            .and_then(JsonValue::as_str);
        assert_eq!(text, Some("<html></html>"));
    }

    #[test]
    fn parse_json_empty_input_is_error() {
        assert_eq!(parse_json(""), Err(JsonParseError::UnexpectedEnd));
    }

    #[test]
    fn parse_json_malformed_is_error() {
        assert!(parse_json("{not json}").is_err());
        assert!(parse_json("[1, 2,]").is_err());
        assert!(parse_json("tru").is_err());
    }

    #[test]
    fn parse_json_trailing_data_is_error() {
        assert_eq!(parse_json("1 2"), Err(JsonParseError::TrailingData));
    }

    #[test]
    fn parse_json_depth_exceeded_is_error() {
        let deep = "[".repeat(MAX_JSON_DEPTH + 2) + &"]".repeat(MAX_JSON_DEPTH + 2);
        assert_eq!(parse_json(&deep), Err(JsonParseError::DepthExceeded));
    }

    #[test]
    fn parse_json_numbers() {
        assert_eq!(parse_json("0"), Ok(JsonValue::Number(0.0)));
        assert_eq!(parse_json("-1.5"), Ok(JsonValue::Number(-1.5)));
        assert_eq!(parse_json("1e3"), Ok(JsonValue::Number(1000.0)));
        assert!(parse_json("-").is_err());
        assert!(parse_json("1.").is_err());
    }

    // レビュー指摘 P0（Codex。PR #442 再レビュー）: `validate_local_bench_url`
    // が 127.0.0.1・指定ポートの http URL のみを許可し、それ以外
    // （別ホスト・別ポート・https・file 等）を拒否することを確認する。
    #[test]
    fn validate_local_bench_url_accepts_matching_local_url() {
        assert!(validate_local_bench_url("http://127.0.0.1:9400/article.html", 9400).is_ok());
    }

    #[test]
    fn validate_local_bench_url_rejects_non_http_scheme() {
        assert!(validate_local_bench_url("https://127.0.0.1:9400/", 9400).is_err());
        assert!(validate_local_bench_url("file:///etc/passwd", 9400).is_err());
    }

    #[test]
    fn validate_local_bench_url_rejects_other_host() {
        assert!(validate_local_bench_url("http://example.com:9400/", 9400).is_err());
        assert!(validate_local_bench_url("http://localhost:9400/", 9400).is_err());
        assert!(validate_local_bench_url("http://0.0.0.0:9400/", 9400).is_err());
    }

    // `reqwest::Url`（WHATWG URL 準拠）は `127.1`・`2130706433` のような
    // IPv4 の別表記を正規化して "127.0.0.1" にする。この関数は正規化後の
    // 文字列を見るため、これらの表記も結局は許可対象のホストと同じ
    // アドレスとして受理される（別表記を使っても迂回にはならない。
    // `goto` 先が変わるわけではないため安全側）。
    #[test]
    fn validate_local_bench_url_accepts_ipv4_alt_forms_that_normalize_to_loopback() {
        assert!(validate_local_bench_url("http://127.1:9400/", 9400).is_ok());
        assert!(validate_local_bench_url("http://2130706433:9400/", 9400).is_ok());
    }

    #[test]
    fn validate_local_bench_url_rejects_mismatched_port() {
        assert!(validate_local_bench_url("http://127.0.0.1:9401/", 9400).is_err());
        // ポート省略（80 番）は起動したポートと一致しない限り拒否される。
        assert!(validate_local_bench_url("http://127.0.0.1/", 9400).is_err());
    }

    // fixture 配信サーバー（competitor_lightpanda.rs の
    // handle_fixture_connection）が使うリクエスト行パーサー。
    #[test]
    fn parse_http_request_line_basic() {
        assert_eq!(
            parse_http_request_line("GET /article.html HTTP/1.1\r\n"),
            Some(("GET".to_string(), "/article.html".to_string()))
        );
    }

    #[test]
    fn parse_http_request_line_malformed_is_none() {
        assert_eq!(parse_http_request_line(""), None);
        assert_eq!(parse_http_request_line("GET /only-two-tokens"), None);
        assert_eq!(parse_http_request_line("GET /path NOT-HTTP/1.1"), None);
    }

    // レビュー指摘（コーディネーター指示。PR #442 再々々々レビュー）:
    // `HTTP/` で始まるだけの不正なバージョン表記（`HTTP/potato` 等）を
    // 許してしまわないことを確認する。
    #[test]
    fn parse_http_request_line_rejects_malformed_version() {
        assert_eq!(
            parse_http_request_line("GET /article.html HTTP/potato\r\n"),
            None
        );
        assert_eq!(
            parse_http_request_line("GET /article.html HTTPS/1.1\r\n"),
            None
        );
        assert_eq!(
            parse_http_request_line("GET /article.html HTTP/1\r\n"),
            None
        );
    }

    #[test]
    fn parse_http_request_line_rejects_extra_tokens() {
        assert_eq!(
            parse_http_request_line("GET /article.html HTTP/1.1 extra\r\n"),
            None
        );
    }

    #[test]
    fn is_valid_http_version_accepts_common_forms() {
        assert!(is_valid_http_version("HTTP/1.1"));
        assert!(is_valid_http_version("HTTP/1.0"));
        assert!(is_valid_http_version("HTTP/2.0"));
    }

    #[test]
    fn is_valid_http_version_rejects_malformed_forms() {
        assert!(!is_valid_http_version("HTTP/potato"));
        assert!(!is_valid_http_version("HTTP/1"));
        assert!(!is_valid_http_version("HTTP/1.1.1"));
        assert!(!is_valid_http_version("HTTPS/1.1"));
        assert!(!is_valid_http_version("http/1.1"));
        assert!(!is_valid_http_version(""));
    }

    // レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
    // competitor_lightpanda.rs:352）: リクエスト行・ヘッダ行の読み取りが
    // タイムアウト／サイズ超過／その他の I/O エラーで失敗した経路は、
    // すべて 400（タイムアウトなら 408）で終了しなければならない。
    #[test]
    fn http_status_for_io_error_timeout_is_408() {
        assert_eq!(
            http_status_for_io_error(std::io::ErrorKind::WouldBlock),
            (408, "Request Timeout")
        );
        assert_eq!(
            http_status_for_io_error(std::io::ErrorKind::TimedOut),
            (408, "Request Timeout")
        );
    }

    #[test]
    fn http_status_for_io_error_other_is_400() {
        assert_eq!(
            http_status_for_io_error(std::io::ErrorKind::InvalidData),
            (400, "Bad Request")
        );
        assert_eq!(
            http_status_for_io_error(std::io::ErrorKind::UnexpectedEof),
            (400, "Bad Request")
        );
        assert_eq!(
            http_status_for_io_error(std::io::ErrorKind::ConnectionReset),
            (400, "Bad Request")
        );
    }

    // レビュー指摘 P1（Codex。PR #442 再々々々々々々レビュー・
    // competitor_lightpanda.rs:558）: `kill_process_group`（`unsafe`・実際の
    // プロセス操作を伴うため、この crate root では単体テストできない）が
    // 使う「終了ステータスから成功／失敗を判定する」ロジック自体は
    // 実プロセスを起動して検証できる。
    #[test]
    fn exit_status_to_result_success_is_ok() {
        #[cfg(unix)]
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .expect("spawn sh");
        #[cfg(windows)]
        let status = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .status()
            .expect("spawn cmd");
        assert_eq!(exit_status_to_result(status), Ok(()));
    }

    #[test]
    fn exit_status_to_result_failure_is_err() {
        #[cfg(unix)]
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 1"])
            .status()
            .expect("spawn sh");
        #[cfg(windows)]
        let status = std::process::Command::new("cmd")
            .args(["/C", "exit 1"])
            .status()
            .expect("spawn cmd");
        assert!(exit_status_to_result(status).is_err());
    }

    // レビュー指摘 P2（Codex。PR #442 再々々々々々レビュー・
    // competitor_lightpanda.rs:756）: `Content-Length` が読み取り上限を
    // 超える場合は `min` で黙って切り詰めず probe 失敗にする契約を、
    // `probe_once` が呼ぶこの純粋関数の単体テストとして確認する。
    #[test]
    fn content_length_exceeds_limit_true_when_over() {
        assert!(content_length_exceeds_limit(Some(65), 64));
    }

    #[test]
    fn content_length_exceeds_limit_false_when_within_or_equal() {
        assert!(!content_length_exceeds_limit(Some(64), 64));
        assert!(!content_length_exceeds_limit(Some(1), 64));
        assert!(!content_length_exceeds_limit(Some(0), 64));
    }

    #[test]
    fn content_length_exceeds_limit_false_when_absent() {
        assert!(!content_length_exceeds_limit(None, 64));
    }

    // レビュー指摘 P2（コーディネーター指示。PR #442 再々々々々々々
    // レビュー）: `Content-Length` ヘッダーが存在するのに値が不正
    // （非数値・空・符号付き等）な場合は「長さ指定なし」ではなく
    // `None`（呼び出し元はこれを probe 失敗として扱う）にする。
    #[test]
    fn parse_content_length_accepts_plain_digits() {
        assert_eq!(parse_content_length("123"), Some(123));
        assert_eq!(parse_content_length(" 123 "), Some(123));
        assert_eq!(parse_content_length("0"), Some(0));
        // 先頭ゼロは HTTP のグラマー上許容される（DIGIT の連続）。
        assert_eq!(parse_content_length("007"), Some(7));
    }

    #[test]
    fn parse_content_length_rejects_empty() {
        assert_eq!(parse_content_length(""), None);
        assert_eq!(parse_content_length("   "), None);
    }

    #[test]
    fn parse_content_length_rejects_non_numeric() {
        assert_eq!(parse_content_length("abc"), None);
        assert_eq!(parse_content_length("12abc"), None);
        assert_eq!(parse_content_length("12.5"), None);
    }

    #[test]
    fn parse_content_length_rejects_signed() {
        // `usize::parse` は `"+123"` を受理してしまうため、明示的に
        // 拒否できていることを確認する。
        assert_eq!(parse_content_length("+123"), None);
        assert_eq!(parse_content_length("-123"), None);
    }

    // レビュー指摘 P0・P1（Codex。PR #442 再々レビュー・
    // competitor_lightpanda.rs:338/352）: fixture はコンパイル時に埋め込んだ
    // 固定テーブルの完全一致だけで引くため、シンボリックリンク・TOCTOU・
    // サイズ超過が構造的に起こらない（実行時のファイル I/O 自体がない）。
    #[test]
    fn lookup_fixture_returns_known_pages() {
        for (path, content_type, content) in FIXTURE_TABLE {
            assert_eq!(lookup_fixture(path), Some((*content_type, *content)));
        }
    }

    #[test]
    fn lookup_fixture_unknown_path_is_none() {
        assert_eq!(lookup_fixture("/does-not-exist.html"), None);
    }

    #[test]
    fn lookup_fixture_parent_traversal_path_is_none() {
        // テーブルには `..` を含むキーが存在しないため、完全一致の時点で
        // 自然に拒否される（ファイルシステムへ触れないため辿りようがない）。
        assert_eq!(lookup_fixture("/../Cargo.toml"), None);
        assert_eq!(lookup_fixture("/../../etc/passwd"), None);
        assert_eq!(lookup_fixture("/.."), None);
    }

    #[test]
    fn lookup_fixture_query_or_trailing_slash_is_none() {
        // 完全一致のみを許可する契約（クエリ文字列・末尾スラッシュの正規化は
        // 行わない）ことを確認する。
        assert_eq!(lookup_fixture("/article.html?x=1"), None);
        assert_eq!(lookup_fixture("/article.html/"), None);
        assert_eq!(lookup_fixture(""), None);
    }

    // レビュー指摘（コーディネーター指示。PR #442 再レビュー）: fixture は
    // 自作の静的コンテンツであり、外部サイトへのサブリソース参照
    // （`http://`・`https://`・プロトコル相対の `//`）を含まないことを
    // 確認する。埋め込み済みの `FIXTURE_TABLE` に対して検査するため、
    // fixture を追加した場合もテーブルへ追加するだけで自動的に対象になる。
    #[test]
    fn fixtures_contain_no_external_references() {
        for (name, _content_type, content) in FIXTURE_TABLE {
            let lower = content.to_ascii_lowercase();
            assert!(
                !lower.contains("http://"),
                "{name}: must not reference http:// URLs"
            );
            assert!(
                !lower.contains("https://"),
                "{name}: must not reference https:// URLs"
            );
            // プロトコル相対参照（`src="//..."`・`src='//...'` の引用符付き、
            // および HTML が許す `src=//...` の無引用形。レビュー指摘
            // advisor: 無引用属性値は素通りしていた）の簡易検出。
            assert!(
                !lower.contains("=\"//") && !lower.contains("='//") && !lower.contains("=//"),
                "{name}: must not reference protocol-relative (//) URLs"
            );
            // インライン CSS の `url(//...)`（プロトコル相対）も対象にする。
            assert!(
                !lower.contains("url(//"),
                "{name}: must not reference protocol-relative (//) CSS url()"
            );
        }
    }

    // レビュー指摘 P1（Codex。PR #442 再々レビュー・
    // competitor_lightpanda.rs:1058）: 対象バイナリ未設定（`bin: None`）は、
    // fixture サーバーの起動結果に関わらず常に `Skipped` でなければならない
    // （`Error` になってはいけない）。
    #[test]
    fn token_reduction_gate_unconfigured_is_skipped_even_if_server_failed() {
        let outcome = token_reduction_gate(None, Some("bind failed"), "fandhe-browser");
        match outcome {
            Some(Outcome::Skipped(reason)) => {
                assert!(reason.contains("binary path not configured"));
            }
            other => panic!("expected Some(Outcome::Skipped(_)), got {other:?}"),
        }
    }

    #[test]
    fn token_reduction_gate_unconfigured_without_server_error_is_skipped() {
        let outcome = token_reduction_gate(None, None, "fandhe-browser");
        assert!(matches!(outcome, Some(Outcome::Skipped(_))));
    }

    #[test]
    fn token_reduction_gate_configured_with_server_error_is_error() {
        let bin = PathBuf::from("lightpanda-bin");
        let outcome = token_reduction_gate(Some(&bin), Some("bind failed"), "lightpanda");
        match outcome {
            Some(Outcome::Error(reason)) => {
                assert!(reason.contains("bind failed"));
            }
            other => panic!("expected Some(Outcome::Error(_)), got {other:?}"),
        }
    }

    #[test]
    fn token_reduction_gate_configured_without_server_error_is_none() {
        let bin = PathBuf::from("lightpanda-bin");
        assert_eq!(token_reduction_gate(Some(&bin), None, "lightpanda"), None);
    }

    // レビュー指摘 P1（Codex。PR #442 再々々レビュー・
    // competitor_lightpanda.rs:593）: `measure_idle_rss` の Windows 版が
    // `target.bin` を確認せず常に `Outcome::Unsupported` を返し、「対象
    // バイナリ未設定なら全計測項目が Skipped」という契約に反していた。
    // OS に依存しないこの判定を共通関数として検証する
    // （`measure_cold_start`・`measure_binary_size`・unix/windows 両方の
    // `measure_idle_rss`・[`token_reduction_gate`] がすべてこの関数を通す）。
    #[test]
    fn require_bin_none_is_skipped() {
        match require_bin(None, "fandhe-browser") {
            Err(Outcome::Skipped(reason)) => {
                assert!(reason.contains("binary path not configured"));
            }
            other => panic!("expected Err(Outcome::Skipped(_)), got {other:?}"),
        }
    }

    #[test]
    fn require_bin_some_returns_the_path() {
        let bin = PathBuf::from("lightpanda-bin");
        assert_eq!(require_bin(Some(&bin), "lightpanda"), Ok(&bin));
    }

    // レビュー指摘 P2（Codex。PR #442 再々々々レビュー・
    // competitor_lightpanda.rs:511/554）: `SERVE_ARGS`・`MCP_ARGS` の検証
    // エラーは、それぞれを使う計測だけに影響しなければならない。
    #[test]
    fn arg_error_gate_none_is_none() {
        assert_eq!(arg_error_gate(None, "lightpanda"), None);
    }

    #[test]
    fn arg_error_gate_some_is_error_with_target_name() {
        match arg_error_gate(Some("too many args"), "lightpanda") {
            Some(Outcome::Error(reason)) => {
                assert!(reason.contains("lightpanda"));
                assert!(reason.contains("too many args"));
            }
            other => panic!("expected Some(Outcome::Error(_)), got {other:?}"),
        }
    }

    // レビュー指摘 P1（Codex。PR #442 再々々々レビュー・
    // competitor_lightpanda.rs:839）: `result` が `null`・`{}` でも
    // `isError` が無ければ成功として扱っていた。JSON-RPC の封筒と
    // `tools/call` の `result.content` の形を厳密に検証する。
    #[test]
    fn validate_mcp_response_accepts_valid_tools_call() {
        let value = parse_json(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"<html></html>"}]}}"#,
        )
        .expect("valid json");
        assert_eq!(validate_mcp_response(&value, 1.0, "tools/call"), Ok(()));
    }

    #[test]
    fn validate_mcp_response_accepts_error_envelope() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":1,"error":{"message":"boom"}}"#)
            .expect("valid json");
        assert_eq!(validate_mcp_response(&value, 1.0, "tools/call"), Ok(()));
    }

    #[test]
    fn validate_mcp_response_rejects_missing_jsonrpc() {
        let value = parse_json(r#"{"id":1,"result":{}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_wrong_jsonrpc_version() {
        let value = parse_json(r#"{"jsonrpc":"1.0","id":1,"result":{}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_id_mismatch() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_both_error_and_result() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":1,"error":{"message":"x"},"result":{}}"#)
            .expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_neither_error_nor_result() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":1}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_accepts_valid_initialize_result() {
        let value = parse_json(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
        )
        .expect("valid json");
        assert_eq!(validate_mcp_response(&value, 1.0, "initialize"), Ok(()));
    }

    // レビュー指摘（コーディネーター指示。PR #442 再々々々レビュー）:
    // 「initialize や tools/list など、ほかの MCP 呼び出しの応答の形も
    // 同じ方針で確認する」ため、`initialize` は空の `result: {}` を
    // 成功と誤判定してはいけない（`tools/call` の `result: {}` を拒否する
    // のと同じ理由付け）。
    #[test]
    fn validate_mcp_response_rejects_initialize_empty_result() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_initialize_missing_protocol_version() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#)
            .expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_initialize_non_object_capabilities() {
        let value = parse_json(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":[]}}"#,
        )
        .expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "initialize").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_non_object_result_for_non_tools_call() {
        for raw in [
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":42}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":[]}"#,
        ] {
            let value = parse_json(raw).expect("valid json");
            assert!(
                validate_mcp_response(&value, 1.0, "initialize").is_err(),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn validate_mcp_response_rejects_tools_call_null_or_empty_result() {
        // レビュー指摘 P1 の核心事例: `goto` の応答が `result: null` や
        // `result: {}`（`content` 欠如）でも、以前は `isError` が無いという
        // だけで成功扱いにしていた。
        for raw in [
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        ] {
            let value = parse_json(raw).expect("valid json");
            assert!(
                validate_mcp_response(&value, 1.0, "tools/call").is_err(),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn validate_mcp_response_rejects_tools_call_content_not_array() {
        let value =
            parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"content":"x"}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "tools/call").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_tools_call_empty_content_array() {
        let value =
            parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "tools/call").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_tools_call_content_item_missing_type() {
        let value = parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"text":"x"}]}}"#)
            .expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "tools/call").is_err());
    }

    #[test]
    fn validate_mcp_response_rejects_tools_call_text_item_missing_text_field() {
        let value =
            parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text"}]}}"#)
                .expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "tools/call").is_err());
    }

    #[test]
    fn validate_mcp_response_accepts_non_text_content_item_without_text() {
        // `type` が `"text"` でなければ `text` フィールドは必須ではない。
        let value =
            parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image"}]}}"#)
                .expect("valid json");
        assert_eq!(validate_mcp_response(&value, 1.0, "tools/call"), Ok(()));
    }

    #[test]
    fn validate_mcp_response_rejects_tools_call_non_bool_is_error() {
        let value = parse_json(
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":"true","content":[{"type":"text","text":"x"}]}}"#,
        )
        .expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "tools/call").is_err());
    }

    #[test]
    fn validate_mcp_response_accepts_tools_call_bool_is_error() {
        let value = parse_json(
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":false,"content":[{"type":"text","text":"x"}]}}"#,
        )
        .expect("valid json");
        assert_eq!(validate_mcp_response(&value, 1.0, "tools/call"), Ok(()));
    }

    // レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
    // competitor_lightpanda.rs:364/682/711）: `set_read_timeout` は 1 回の
    // `read` にしか効かないため、1 バイトずつ送る相手（slow-loris）は
    // 個々の `read` を毎回タイムアウト直前に完了させることで、呼び出し
    // 全体を無期限に専有し得た。`DeadlineReader` が接続全体の絶対期限で
    // 打ち切ることを、実際の loopback ソケットで確認する。
    #[test]
    fn deadline_reader_cuts_off_slow_loris_sender() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            // 1 バイトずつ、`DeadlineReader` の期限より長い間隔で送り続ける
            // （slow-loris）。テスト側の `DeadlineReader` が期限で打ち切る
            // ことを検証するため、送信側は本テストの期限（200ms）よりも
            // 十分長く粘る。
            for _ in 0..50 {
                if socket.write_all(b"A").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let stream = TcpStream::connect(addr).expect("connect");
        let deadline_duration = Duration::from_millis(200);
        let deadline = Instant::now() + deadline_duration;
        let mut reader = DeadlineReader::new(stream, deadline);

        let start = Instant::now();
        let mut buf = [0u8; 4096];
        // 個々の `read` は（`DeadlineReader` により毎回残り時間へ短縮
        // されるとはいえ）データが来れば成功し得るため、`TimedOut` に
        // 達するまでループする。
        //
        // レビュー指摘（Cursor。PR #442 再々々々々々々レビュー）: 送信側
        // （slow-loris スレッド）は接続を close しない前提のため、
        // `Ok(0)`（相手の正常な close）に達することは無いはずである。
        // 万一到達した場合はテストの前提が崩れている（実装の変更漏れ等）
        // ことを示すので、無限ループにせず `panic!` で明示的に失敗させる
        // （`Ok(0)` を単に無視して回り続けると、slow-loris の送信が尽きた
        // 後もタイムアウトへ到達せず無限ループし得た）。
        let last_result = loop {
            match reader.read(&mut buf) {
                Ok(0) => panic!(
                    "slow-loris sender must not close the connection; got EOF instead of a timeout"
                ),
                Ok(_) => continue,
                Err(e) => break e.kind(),
            }
        };
        let elapsed = start.elapsed();

        assert_eq!(last_result, std::io::ErrorKind::TimedOut);
        // 送信側は 50 回 × 50ms = 2.5 秒粘るが、`DeadlineReader` は
        // 接続全体の期限（200ms）で打ち切るため、実測時間がそれを大幅に
        // 超えないことを確認する（多少のオーバーヘッドは許容する）。
        assert!(
            elapsed < deadline_duration * 5,
            "DeadlineReader should cut off around the deadline, took {elapsed:?}"
        );

        // ソケットを閉じてから送信側スレッドの終了を待つ。閉じないと
        // 送信側は（誰も読んでいなくても）OS の送信バッファへ書き込み
        // 続けてしまい、全 50 回分（2.5 秒）の `sleep` を律儀に消化して
        // からでないと `join` が返らず、テストが不必要に長くなる。
        drop(reader);
        let _ = handle.join();
    }

    // レビュー指摘 P1（Codex。PR #442 再々々々々々レビュー・
    // competitor_lightpanda.rs:711）: 宣言された長さに届く前に応答が
    // 途中で切れた場合、それまでに読めた断片を成功として扱ってはいけない。
    // `probe_once`（competitor_lightpanda.rs）はこの検証を
    // `Read::read_exact` に委ねているため、ここでは `DeadlineReader` 越しの
    // `read_exact` が早期 EOF を確実に `Err` にすることを確認する。
    #[test]
    fn deadline_reader_read_exact_fails_on_truncated_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            // 宣言された長さ（10 バイト）より短い 3 バイトだけ送って閉じる。
            let _ = socket.write_all(b"abc");
            drop(socket);
        });

        let stream = TcpStream::connect(addr).expect("connect");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reader = DeadlineReader::new(stream, deadline);

        let mut buf = [0u8; 10];
        let result = reader.read_exact(&mut buf);
        assert!(
            result.is_err(),
            "truncated body must not be treated as a complete read"
        );
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );

        let _ = handle.join();
    }

    // レビュー指摘（Cursor。PR #442 再々々々々々々レビュー・
    // competitor_lightpanda.rs:1893。macOS CI 失敗）: unix のブロッキング
    // ソケットはタイムアウト時に `WouldBlock` を返すことがある（macOS）が、
    // `DeadlineReader` の利用側は OS 差異を意識せず `TimedOut` だけを
    // 見ればよいようにする。`normalize_timeout_error` がその正規化を
    // 一貫して行うことを確認する。
    #[test]
    fn normalize_timeout_error_maps_would_block_to_timed_out() {
        let err = std::io::Error::new(std::io::ErrorKind::WouldBlock, "would block");
        assert_eq!(
            normalize_timeout_error(err).kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn normalize_timeout_error_preserves_already_timed_out() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(
            normalize_timeout_error(err).kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn normalize_timeout_error_preserves_unrelated_errors() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(
            normalize_timeout_error(err).kind(),
            std::io::ErrorKind::ConnectionReset
        );
    }
}
