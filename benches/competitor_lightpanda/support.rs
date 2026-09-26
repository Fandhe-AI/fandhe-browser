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
//! 全体期限打ち切り等）もこのファイルに置く。
//!
//! 同様に [`apply_new_process_group`]・[`kill_process_group`] も実際に
//! プロセスを起動・終了させる副作用を持つ。プロセスグループ経由で子・孫
//! プロセスの両方を実際に終了できることを 3 OS CI で検証する単体テストが
//! 必要なため、`[[bench]]` ターゲット（`cargo test` の対象外）ではなく
//! この crate root に置く。テストは実際に `sh` で子・孫プロセスを起動して
//! 検証する（外部プロセス起動を伴うが、対象バイナリではなく OS 標準の
//! シェルのみを使うため、依存関係は増えない）。
//!
//! `AISNAP-1`（MCP トークン削減率）計測は、任意の外部 URL ではなく、
//! リポジトリに同梱した静的 fixture（外部参照を含まない自作コンテンツ。
//! [`FIXTURE_TABLE`]）だけを対象にする。計測対象ブラウザは接続時に名前を
//! 再解決し、公開 URL からのリダイレクトにも追従し得るため、事前の URL
//! 検証をいくら積み増しても実際の接続先を保証できない（SSRF・TOCTOU。
//! security.md「SSRF」）。fixture は `include_str!` でコンパイル時に
//! バイナリへ埋め込み（実行時のファイル I/O を行わないため、シンボリック
//! リンク追従やメタデータ確認後のサイズ変化 TOCTOU が構造的に起こらない）、
//! `measure.rs`（`FixtureServer`）が起動するローカル静的サーバー
//! （`127.0.0.1` の空きポート）が [`lookup_fixture`] の完全一致検索で配信する。`goto`
//! する URL は [`fixture_url`] だけで組み立て、`FIXTURE_TABLE` に列挙した
//! 固定パス以外を渡せない構造にすることで、ベンチが外部ネットワークへ
//! 一切出ない構成を保証する。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    // `HTTP/1.0`・`HTTP/1.1` のいずれかに続く単一の SP、ちょうど 3 桁の
    // 数字（100〜599）、その後に SP か行末が続く、という形式だけを受理する
    // （バージョン部分の判定は [`is_valid_http_version`] に統一し、
    // リクエスト行検証と基準を揃える）。
    let version_len = "HTTP/1.1".len();
    // `str::split_at` はバイト境界が UTF-8 文字境界からずれていると panic
    // する。外部プロセスからの未検証入力（マルチバイト文字を含み得る）を
    // 扱うため、`get` で境界を検証してから取り出す（panic させない。
    // coding-rust.md「外部入力の経路では unwrap を使わず明示的に処理する」）。
    let version = trimmed.get(..version_len)?;
    let rest = trimmed.get(version_len..)?;
    if !is_valid_http_version(version) {
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
/// `reserve_port` は bind したリスナーを
/// 即座に drop してから子プロセスを起動するため、その間に別プロセスが
/// 同じポートを奪える TOCTOU が残る。ポートが「空いているか」ではなく
/// 「応答している内容が期待するブラウザらしいか」を検証することで、
/// 無関係なプロセスの応答を子プロセスの readiness と誤認する可能性を
/// 下げる。CDP の `/json/version` は JSON オブジェクトで
/// [`READINESS_RESPONSE_FIELDS`] のいずれかのフィールドを持つのが
/// 一般的なため、本文が JSON オブジェクトであり、かつそのいずれかの
/// キー（大小文字を区別しない）を持つことを要求する。ステータス行だけで判定していた以前の実装より強いが、
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
/// 呼び出し元（`measure.rs` の `sample_rss_kb`）が
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
/// `FANDHE_BROWSER_SERVE_ARGS` 等の環境変数から起動引数を受け取る経路
/// （`competitor_lightpanda.rs` の `args_from_env`）が使う。クォート解釈は
/// せず、素朴な空白分割に留める（シェル評価をしないことでインジェクションを
/// 避ける方針。実装計画セクション 6）。
///
/// `#[allow(dead_code)]` の理由:
/// この crate root は 3 つの異なる `[[bench]]`/`[[test]]` ターゲットとして
/// コンパイルされる。`competitor_lightpanda`（`[[bench]]`）と
/// `competitor_lightpanda_support`（`harness = true` の `[[test]]`）では
/// それぞれ `args_from_env`（本番コード経路）・`#[test] fn`（`--test` 付きで
/// 実際に実行される）から到達するため dead_code にならないが、
/// `competitor_lightpanda_measure`（`harness = false` の `[[test]]`。
/// `measure_tests.rs` が独自の `main` を持ち、環境変数からの引数分割を
/// 使わない）は `--test` を渡されず `#[test] fn` を呼ばないため、この関数
/// だけを見ると到達不能になる（`kill_process_group_kills_child_and_grandchild`
/// テストのため `process_is_alive` をローカルクロージャへ変えた過去の
/// 対応と同種の制約だが、`split_args` は本番コード（`args_from_env`）からも
/// 使う公開関数のため、クロージャ化ではなくこの属性で対応する）。
#[allow(dead_code)]
pub fn split_args(env: &str) -> Vec<String> {
    env.split_whitespace().map(str::to_string).collect()
}

/// `<PREFIX>_SERVE_ARGS`/`<PREFIX>_MCP_ARGS` の生の値を起動引数へ解決する
/// 純粋関数（`competitor_lightpanda.rs` の `args_from_env` から呼ばれる。
/// [`resolve_bin_value`]・[`parse_trial_count`] と同じ理由で `std::env` に
/// 触れない形にする）。
///
/// 未設定（`raw` が `None`）は `default` を分割した結果を返す。設定
/// されているのに解決できない場合（非 UTF-8・バイト数上限超過・分割後の
/// 引数個数上限超過）は黙って既定値へフォールバックせず `Err` にする
/// （誤設定に気づけないまま意図しない引数で起動しないことを優先する。
/// coding-rust.md「外部入力の経路では明示的に処理する」）。
///
/// [`parse_trial_count`] と同じ理由で `#[allow(dead_code)]` を付ける
/// （`competitor_lightpanda_measure` からは到達不能）。
#[allow(dead_code)]
pub fn parse_args_env_value(
    var_name: &str,
    raw: Option<&std::ffi::OsStr>,
    default: &str,
    max_bytes: usize,
    max_count: usize,
) -> Result<Vec<String>, String> {
    let Some(raw) = raw else {
        return Ok(split_args(default));
    };
    let Some(raw) = raw.to_str() else {
        return Err(format!("{var_name} is not valid UTF-8"));
    };
    if raw.len() > max_bytes {
        return Err(format!("{var_name} exceeds {max_bytes} bytes"));
    }
    let args = split_args(raw);
    if args.len() > max_count {
        return Err(format!("{var_name} has more than {max_count} args"));
    }
    Ok(args)
}

/// 計測 1 件の結果。成功しなかった場合も理由を残し「実装済みを装わない」
/// （coding-rust.md「公開 API」・REPAIR-4 相当の方針をベンチ出力にも適用）。
///
/// `main`（`competitor_lightpanda.rs`）が構築し、[`bench_exit_code`] の入力
/// および JSON 出力（[`Outcome::to_json`]）に使う。純粋関数群と同じ
/// `support.rs` に置くことで、[`bench_exit_code`] のテスト（`Skipped`/
/// `Unsupported` のみでは `0`、`Error` を含むと非ゼロ）を副作用なしに書ける。
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
/// `Outcome::Error`
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

/// `AISNAP-1` が `goto` する fixture ページの URL を組み立てる型付き
/// コンストラクタ。
///
/// `path` は [`FIXTURE_TABLE`] に列挙された固定パス（`&'static str`。実行時
/// 入力を含まない）だけを渡す契約とし、常に
/// `http://127.0.0.1:<port><path>` の形になることを型で保証する。これに
/// より、文字列を組み立ててから `reqwest::Url` 等でパースし直して検証する
/// 経路自体をなくす（誤った scheme・host・port を持つ URL を返しようが
/// ない）。ベンチが起動するローカル fixture サーバー以外へは構造的に
/// `goto` できないため、SSRF 経路が生じない（security.md「SSRF」）。
pub fn fixture_url(port: u16, path: &'static str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// `AISNAP-1`（MCP トークン削減率）計測が `goto` する fixture ページの
/// パス → (content-type, 内容) テーブル。
///
/// 以前は fixture をファイルシステムから
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

/// [`FIXTURE_TABLE`] の各パスに対応する、`AISNAP-1` 計測（`measure.rs` の
/// `measure_token_reduction`）が「`goto` が実際にそのページへ遷移できたか」
/// を内容ベースで確認するための一意なマーカー文字列。
///
/// 以前は `goto` の JSON-RPC 応答が形式上妥当
/// （`validate_mcp_response`）でありさえすれば遷移成功とみなしており、
/// 対象ブラウザが実際にはそのページへ遷移していなくても（例えば直前の
/// ページに留まったまま）、直前のページの `html`/`tree` を当該 fixture の
/// 成功サンプルとして記録できてしまっていた。
///
/// 当初は各ページの `<title>`
/// テキストをマーカーに使っていたが、`<title>` は `<head>` にしか存在せず、
/// アクセシビリティツリー（`tree`）に現れる保証が無い（多くのアクセシビリ
/// ティツリー実装は文書の `accessible name` を `<title>` から採ることは
/// あるが、それは実装依存であり保証ではない）ため、実ブラウザでは
/// `tree` 側の検証が常に失敗し得た。マーカーは `<body>` 内の可視見出し
/// （各 fixture 既存の `<h1>`）のテキストに変更した。見出し要素は
/// アクセシビリティツリーで heading ロールのノード名として確実に表れる
/// （WAI-ARIA のロールマッピング）ため、`tree` 側でも見つかる可能性が
/// 高い。MCP 側に「現在の URL」を取得する専用 API があるとは限らないため
/// それには依存せず、`goto` 後に取得した `html`・`tree` の応答本文それぞれ
/// が、このマーカーを含んでいるかを確認する材料として使う。
///
/// マーカーが `<body>` 内の見出し要素のテキストとして実在すること（`<head>`
/// 内のみの出現は不可）は `fixture_markers_appear_as_body_heading_text` が、
/// マーカー同士が互いの部分文字列にならないこと（一意性）は
/// `fixture_markers_are_mutually_exclusive` が保証する。fixture を
/// 追加・削除した場合は、対応するマーカーもここへ追加・削除すること
/// （`fixture_table_and_markers_cover_the_same_paths` が両テーブルの
/// パス集合の一致を検証する）。
pub const FIXTURE_MARKERS: &[(&str, &str)] = &[
    ("/article.html", "A Short Note on Static Fixtures"),
    ("/listing.html", "Sample Item Listing"),
    ("/form.html", "Sample Sign-in Form"),
];

/// リクエストパスに完全一致する [`FIXTURE_MARKERS`] のマーカー文字列を返す。
///
/// `measure.rs` の `measure_token_reduction` が `goto` 後の内容検証に使う。
/// [`lookup_fixture`] と同じく完全一致だけで引く。
pub fn fixture_marker(request_path: &str) -> Option<&'static str> {
    FIXTURE_MARKERS
        .iter()
        .find(|(path, _)| *path == request_path)
        .map(|(_, marker)| *marker)
}

/// リクエストパスに完全一致する fixture の `(content-type, 内容)` を返す。
///
/// [`FIXTURE_TABLE`] のキーとの完全一致だけで引く（`..`・クエリ文字列・
/// 末尾スラッシュの有無等を正規化しない）。一致しなければ `None`
/// （呼び出し側は 404 を返す。`measure.rs` の
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
/// `<PREFIX>_BIN` が設定されているのに解決に失敗した場合（`bin_error`。
/// [`Target::bin_error`] 参照）は `Outcome::Skipped` ではなく
/// `Outcome::Error` にする（設定ミスと「そもそも未設定」を区別する。
/// coding-rust.md「外部入力の経路では明示的に処理する」）。この判定は
/// OS に依存しない（`bin`・`bin_error` の値だけで決まる）ため、
/// `measure_cold_start`・`measure_binary_size`・`measure_token_reduction`・
/// `#[cfg(unix)]`/`#[cfg(windows)]` 両方の `measure_idle_rss`・
/// [`token_reduction_gate`] がすべてこの 1 関数を通して判定することで、
/// 個別に同じ分岐を書いて食い違いを生む余地をなくす。`Result` にして
/// `bin` そのものを返すことで、呼び出し側は
/// `let Some(bin) = &target.bin else { unreachable!() }` のような
/// 冗長かつ不変条件に依存する再チェックを書かずに済む。
pub fn require_bin<'a>(
    bin: Option<&'a PathBuf>,
    bin_error: Option<&str>,
    target_name: &str,
) -> Result<&'a PathBuf, Outcome> {
    if let Some(reason) = bin_error {
        return Err(Outcome::Error(format!("{target_name}: {reason}")));
    }
    bin.ok_or_else(|| Outcome::Skipped(format!("{target_name}: binary path not configured")))
}

/// `AISNAP-1` 計測（`measure.rs` の `measure_token_reduction`
/// 呼び出し前）が、対象バイナリの有無と fixture サーバーの起動結果から
/// `Outcome` を確定できるかを判定する。
///
/// 片方の対象だけに `_BIN` を設定した状態で
/// fixture サーバーの起動（`bind`）が失敗すると、以前は未設定の対象にも
/// `Outcome::Error` を割り当てていた（「対象バイナリ未設定は常に
/// `Outcome::Skipped`」という契約に反する）。バイナリ未設定の判定自体は
/// [`require_bin`] と共通化し、この関数はそれに加えて fixture サーバーの
/// 起動結果を見る（バイナリが設定済みでサーバー起動が失敗していた場合だけ
/// `Some(Outcome::Error)` を返す）。両方問題なければ `None`
/// （呼び出し側が実際に `measure_token_reduction` を呼ぶ）。
pub fn token_reduction_gate(
    bin: Option<&PathBuf>,
    bin_error: Option<&str>,
    server_start_error: Option<&str>,
    target_name: &str,
) -> Option<Outcome> {
    if let Err(skipped) = require_bin(bin, bin_error, target_name) {
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
/// 以前は `Target::args_error` が
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
/// fixture 配信サーバー（`measure.rs` の
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

/// このベンチが唯一扱う HTTP バージョン表記（`HTTP/1.0`・`HTTP/1.1`）
/// かどうかを判定する。
///
/// fixture サーバーのリクエスト行検証（[`parse_http_request_line`]）と、
/// readiness probe のレスポンスステータス行検証（[`parse_http_status`]）の
/// 両方がこの 1 関数を通す（`HTTP/2.0`・`HTTP/potato` のような表記は
/// どちら側でも受理しない。両者で別々に判定すると基準が食い違い得るため
/// 統一する）。
pub fn is_valid_http_version(version: &str) -> bool {
    version == "HTTP/1.0" || version == "HTTP/1.1"
}

/// ソケット読み取りの `io::Error` から、fixture 配信サーバーが返すべき
/// HTTP ステータスコードを決める。
///
/// リクエスト行・ヘッダ行の読み取りが
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

/// `COMPETITOR_BENCH_TRIALS` の生の値を試行回数へ解決する純粋関数
/// （`competitor_lightpanda.rs` の `trial_count` から呼ばれる。
/// [`resolve_bin_value`] と同じ理由で `std::env` に触れない形にする）。
///
/// 未設定（`raw` が `None`）は `default` を返す。設定されているのに
/// 解決できない場合（非 UTF-8・非数値・`1..=max` の範囲外）は黙って
/// `default` へフォールバックせず `Err` にする（誤設定に気づけないまま
/// 意図しない試行回数で計測することを避ける。coding-rust.md「外部入力の
/// 経路では明示的に処理する」）。
///
/// [`split_args`] と同じ理由（本ファイル冒頭 `split_args` のドキュメント
/// 参照）で `#[allow(dead_code)]` を付ける。`competitor_lightpanda`
/// （`[[bench]]`）・`competitor_lightpanda_support`（`harness = true` の
/// `[[test]]`。単体テストから到達）では到達するが、
/// `competitor_lightpanda_measure`（`harness = false`。独自の `main` を持ち
/// 環境変数の解析を経由しない）からは到達不能になる。
#[allow(dead_code)]
pub fn parse_trial_count(
    raw: Option<&std::ffi::OsStr>,
    default: usize,
    max: usize,
) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let Some(raw) = raw.to_str() else {
        return Err("COMPETITOR_BENCH_TRIALS is not valid UTF-8".to_string());
    };
    let trimmed = raw.trim();
    let n: usize = trimmed
        .parse()
        .map_err(|_| format!("COMPETITOR_BENCH_TRIALS ({trimmed:?}) is not a valid integer"))?;
    if (1..=max).contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "COMPETITOR_BENCH_TRIALS ({n}) must be between 1 and {max}"
        ))
    }
}

/// `<PREFIX>_BIN` の生の値（`env::var_os` の結果）を、既存の通常ファイルの
/// パスへ 1 回だけ解決する純粋関数（`competitor_lightpanda.rs` の
/// `Target::from_env` から呼ばれる。`std::env` に触れない形にすることで
/// `std::env::set_var`（edition 2024 では `unsafe`。coding-rust.md
/// 「unsafe は原則禁止」）を使わずに単体テストできる）。
///
/// 戻り値は `(bin, bin_error)` の組。環境変数が未設定（`raw` が `None`）
/// なら `(None, None)`（全計測を `Outcome::Skipped` にする既存の契約）。
/// 設定されているのに解決できなかった場合（非 UTF-8・空白のみ・存在しない・
/// 通常ファイルでない）は `(None, Some(reason))` にし、[`require_bin`] が
/// `Outcome::Skipped` ではなく `Outcome::Error` として扱えるようにする
/// （誤設定を「未設定」と黙って同一視しない）。ここで検証したパスを、
/// 起動（`Command::new`）・サイズ計測（`fs::metadata`）の両方で再利用する
/// ことで、PATH 検索の有無や cwd の違いによる「起動できたのにサイズ計測
/// できない」といった食い違いを防ぐ。
///
/// [`parse_trial_count`] と同じ理由で `#[allow(dead_code)]` を付ける
/// （`competitor_lightpanda_measure` からは到達不能）。
#[allow(dead_code)]
pub fn resolve_bin_value(
    var_name: &str,
    raw: Option<&std::ffi::OsStr>,
) -> (Option<PathBuf>, Option<String>) {
    let Some(raw) = raw else {
        return (None, None);
    };
    let Some(raw) = raw.to_str() else {
        return (None, Some(format!("{var_name} is not valid UTF-8")));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (None, Some(format!("{var_name} is set but empty")));
    }
    let raw_path = PathBuf::from(trimmed);
    // 区切り文字を含まない名前（例: `lightpanda`）だと、`fs::metadata` は
    // cwd を基準に解決するが、`Command::new` はそのような名前を PATH 検索
    // する（`execvp`/Windows のサーチパス相当）ため、両者が異なるファイルを
    // 指し得る（起動できたのにサイズ計測できない、またはその逆）。
    // `std::path::absolute`（cwd との結合のみ行い、シンボリックリンク解決や
    // 存在確認はしない）で区切り文字を含む絶対パスに正規化し、以後は
    // 起動（`Command::new`）・サイズ計測（`fs::metadata`）の両方でこの
    // 同じ絶対パスを使うことで、`Command::new` が PATH 検索へフォール
    // バックする経路自体をなくす。
    let path = match std::path::absolute(&raw_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                None,
                Some(format!(
                    "{var_name} ({}) could not be made absolute: {e}",
                    raw_path.display()
                )),
            );
        }
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => (Some(path), None),
        Ok(_) => (
            None,
            Some(format!(
                "{var_name} ({}) is not a regular file",
                path.display()
            )),
        ),
        Err(e) => (
            None,
            Some(format!(
                "{var_name} ({}) could not be accessed: {e}",
                path.display()
            )),
        ),
    }
}

/// 外部コマンド（`kill`/`taskkill`）の終了ステータスを、成功したかどうかの
/// 判定へ変換する。判定ロジック自体をこの純粋関数へ切り出し、
/// [`kill_process_group`] はプロセス起動と、この関数が行う判定とに分離する。
pub fn exit_status_to_result(status: std::process::ExitStatus) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}

/// [`run_with_deadline`] が使う既定の期限とポーリング間隔。`kill`/`taskkill`/
/// `ps` はローカルの OS 標準コマンドで通常ミリ秒オーダーで完了するが、
/// 環境異常（応答しないシェル等）で無期限にブロックしないよう上限を設ける
/// （coding-rust.md「外部入力の経路」の精神を、信頼するローカルコマンドの
/// 待機にも適用する）。
pub const EXTERNAL_COMMAND_DEADLINE: Duration = Duration::from_secs(10);
const EXTERNAL_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// spawn 済みの子プロセスの終了を期限付きで待つ。期限超過時は `kill` して
/// から `wait` する（`wait` を省くとゾンビプロセスが残る）。
///
/// [`kill_process_group`]・`measure.rs` の `sample_rss_kb` が使う共通
/// プリミティブ。`try_wait` によるポーリングは、`Command::status()` の
/// ような無期限ブロッキング待機を避けるための唯一の手段（std には
/// 期限付き `wait` が無く、`unsafe`・新規依存も使えない）。
pub fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Result<std::process::ExitStatus, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "process did not exit within {deadline:?}; killed it"
                    ));
                }
                std::thread::sleep(EXTERNAL_COMMAND_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("try_wait failed: {e}")),
        }
    }
}

/// `command` を spawn し、期限付きで完了を待って終了ステータスを返す。
///
/// `command` の stdin/stdout/stderr は呼び出し元が設定済みであることを
/// 前提にする（`Stdio::piped()` を使う場合、本関数は完了を待つ間バッファを
/// 読み出さないため、大量出力を伴うコマンドには使わないこと。`kill`・
/// `taskkill` のような出力を持たない・ごく小さいコマンド専用）。
pub fn run_with_deadline(
    mut command: std::process::Command,
    deadline: Duration,
) -> Result<std::process::ExitStatus, String> {
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn: {e}"))?;
    wait_with_deadline(&mut child, deadline)
}

/// `command` の stdout を上限付きで採取しつつ、完了を期限付きで待つ。
///
/// `ps` のような出力の小さいローカルコマンド専用（`measure.rs` の
/// `sample_rss_kb` が使う）。`max_stdout_bytes` を超える出力は切り捨てて
/// 読み続ける（無制限確保を避ける。coding-rust.md「長さ・件数を上限検証
/// してからアロケーションに使う」）。stdin は `Stdio::null()` に固定する
/// （呼び出し元からの入力を渡す用途を持たない）。
///
/// `sample_rss_kb`（unix 専用。`ps` を使う）からのみ呼ばれるため
/// `#[cfg(unix)]` にする（Windows ビルドでは dead_code になる）。
#[cfg(unix)]
pub fn run_capturing_output_with_deadline(
    mut command: std::process::Command,
    deadline: Duration,
    max_stdout_bytes: usize,
) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn: {e}"))?;
    // `ps -o rss=` のような対象コマンドの出力は常に数バイト〜数十バイト
    // であり、パイプバッファ（多くの OS で 64KiB 以上）を埋めて書き込みが
    // ブロックする実運用上のリスクは無視できる。読み出し自体は `wait` の
    // 前ではなく後に行う（このプリミティブの想定用途では出力が極小で
    // あるため、待機中に相手が書き込みでブロックすることはない）。
    let status = wait_with_deadline(&mut child, deadline)?;
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let mut buf = [0u8; 4096];
        loop {
            let n = out
                .read(&mut buf)
                .map_err(|e| format!("read stdout: {e}"))?;
            if n == 0 {
                break;
            }
            let remaining = max_stdout_bytes.saturating_sub(stdout.len());
            let take = remaining.min(n);
            stdout.extend_from_slice(&buf[..take]);
            if stdout.len() >= max_stdout_bytes {
                break;
            }
        }
    }
    Ok((status, stdout))
}

/// 起動する `Command` に「新しいプロセスグループ」を設定する。
///
/// 対象プロセス（ブラウザ）が起動した
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
///
/// `kill_process_group` と合わせて、実際に子・孫プロセスの両方を
/// 終了させられることを 3 OS CI で検証する単体テストが必要なため、
/// `competitor_lightpanda.rs`（`[[bench]]` ターゲット。`cargo test` の
/// 対象外）ではなくこの crate root（`[[test]]` ターゲット
/// `competitor_lightpanda_support`）へ移した。
#[cfg(unix)]
pub fn apply_new_process_group(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
pub fn apply_new_process_group(command: &mut std::process::Command) {
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
/// 終了は OS 標準のシェル・外部コマンド（unix: POSIX シェルの組み込み
/// `kill`、Windows: `taskkill /T /F`）に委ねる。これらが使えない環境
/// （spawn 自体の失敗）・非 0 終了（対象が既に存在しない等）では `Err`
/// を返す。
///
/// 以前は `Command::status()` の結果を
/// `let _ = ...` で握りつぶしており、外部コマンドの spawn 失敗・非 0
/// 終了に気づけなかった。`Result` にして呼び出し元へ返し、呼び出し元は
/// 失敗時に `Child::kill`（直接の子のみ）へのフォールバックを行った旨を
/// エラーメッセージへ含める（`Drop`（`ChildGuard::drop`）内で呼ぶ経路は
/// 戻り値を返せないため、そこに限り結果を握りつぶしてよい）。
#[cfg(unix)]
pub fn kill_process_group(pid: u32) -> Result<(), String> {
    // `-<pid>`（プロセスグループ宛て）は
    // ハイフンで始まるため、GNU coreutils の `kill` はオプションの
    // 一部と誤解釈しないよう `--` を要求するが、
    // macOS の BSD `kill`（`/bin/kill`）は逆に `--` 自体を PID 引数として
    // 解釈し `illegal pid: --` で失敗する。外部の `kill` バイナリの
    // 引数解釈の違いに依存せず、POSIX シェル（`sh`）の組み込み `kill` を
    // 使うことで両方を満たす。bash（macOS の既定 `/bin/sh`）・dash
    // （多くの Linux ディストリビューションの既定 `/bin/sh`）はどちらも
    // 組み込み `kill` で `--` を正しく解釈することを確認済み。`pid` は
    // 自プロセスが起動した子プロセスの PID（`u32`）であり外部入力では
    // ない（数字以外を含み得ない）うえ、シェルスクリプト文字列へ埋め込む
    // のではなく別引数（`$1`）として渡すため、シェルへの注入の余地は無い。
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(r#"kill -s KILL -- "-$1""#)
        .arg("sh") // `$0`（スクリプト名。位置引数の 0 番目）のプレースホルダ。
        .arg(pid.to_string()) // `$1`。
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = run_with_deadline(command, EXTERNAL_COMMAND_DEADLINE)?;
    exit_status_to_result(status)
}

#[cfg(windows)]
pub fn kill_process_group(pid: u32) -> Result<(), String> {
    let mut command = std::process::Command::new("taskkill");
    command
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = run_with_deadline(command, EXTERNAL_COMMAND_DEADLINE)?;
    exit_status_to_result(status)
}

/// `spawn_and_wait_ready`（`measure.rs`）の 1 試行後、対象
/// プロセス（グループ）を kill し `wait` で終了を確認したあと、同じポート
/// へ短い期限で再接続を試みた結果から、その試行を計測値としてそのまま
/// 使ってよいか（別プロセスによるポート競合を検出したか）を判定する。
///
/// `reserve_port` はリスナーを解放してから子プロセスを起動するまでの間に
/// 別プロセスが同じポートを奪える TOCTOU が残るため、readiness probe が
/// 対象自身ではなく別プロセスの応答を拾っていた可能性がある。std だけでは
/// ソケットの所有者（PID）を直接確認する手段が無いため、代わりに
/// 「対象を kill して `wait` 済みのはずなのに、同じポートがまだ応答するか」
/// を事後確認する（post-hoc な所有権確認）。この関数はその確認結果
/// （`reprobe_still_responds`）だけを受け取り、`true`（まだ応答がある）
/// なら計測を `Error` として扱うべき理由文字列を返す。
///
/// 限界（呼び出し元 `measure.rs` の `reprobe_after_kill` の
/// ドキュメントにも記載）: (1) kill から再接続までの間に別のプロセスが
/// 新たに同じポートを奪った場合、それを「元から居た別プロセス」と区別
/// できない、(2) 対象が孫プロセスを起動していた場合、`wait` は直接の子
/// までしか保証しない、(3) ソケットの所有者を PID で直接確認できない。
/// これらの限界はあるが、少なくとも「対象を殺したのに同じポートがまだ
/// 応答する」という明白な矛盾を検出でき、黙って誤った計測値を使うより
/// 安全である。
pub fn port_conflict_error(
    target_name: &str,
    port: u16,
    reprobe_still_responds: bool,
) -> Option<String> {
    if reprobe_still_responds {
        Some(format!(
            "port {port} still responded after the measured process (and its process group) was \
             killed and waited on; another process likely owned this port during the trial \
             (readiness may have been observed from that other process instead of the measured \
             one), treating this trial as a port conflict for {target_name}"
        ))
    } else {
        None
    }
}

/// `Content-Length` ヘッダーの値（コロンの後ろ、前後の空白を含み得る）を
/// 厳密に解釈する。
///
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
/// （`measure.rs` の `PROBE_BODY_MAX_BYTES`）を超えているかを
/// 判定する。
///
/// 以前は `Content-Length` が上限を超えて
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
/// `set_read_timeout` は 1 回の
/// `read` 呼び出しにしか効かない。相手が 1 バイトずつ小分けに送り続ける
/// （slow-loris 型）と、個々の `read` は毎回タイムアウト内に完了して
/// しまうため、`BufRead::read_line` 相当の呼び出し全体としては無期限に
/// 時間を消費し得た。fixture サーバーの接続 1 本・readiness probe の
/// 1 回それぞれで場当たり的にタイムアウトを設定し直すのではなく、この
/// 1 つのプリミティブに統一する（`measure.rs` の
/// `handle_fixture_connection`・`probe_once` の両方がこれ経由でのみ
/// ソケットを読み書きする）。`read`/`write` のたびに
/// `deadline.saturating_duration_since(Instant::now())` を計算し、
/// 残りが 0 なら実際には OS の `read`/`write` を呼ばず
/// `ErrorKind::TimedOut` を返す。`write_all`（`Write` トレイトの
/// デフォルト実装が `write` を繰り返し呼ぶ）もこの仕組みに自動的に
/// 従うため、書き込み側も同じプリミティブで期限を守る。
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

    /// 期限を現在時刻から `extra` だけ先に延長する（縮めない。既存の期限が
    /// まだ先ならそのまま）。
    ///
    /// fixture サーバー（`measure.rs` の `respond_with_io_error`）が、読み
    /// 取り側の絶対期限をちょうど使い切った直後に、小さな固定長のエラー
    /// 応答（400/408）だけは送れるようにするための限定的な猶予に使う。
    /// 読み取り・書き込みで同じ 1 つの期限を共有する設計上、読み取り
    /// タイムアウトが発生した時点で残り時間は必ずゼロになっており、
    /// 延長しなければエラー応答の書き込み自体が常に即座に失敗し、
    /// クライアントに 400/408 が届かない（接続が無応答のまま閉じる）
    /// 構造的な問題になる。無制限な延長ではなく、呼び出し元が決めた短い
    /// 追加時間だけを許すため、接続全体の上限（`FIXTURE_IO_TIMEOUT` +
    /// この猶予）は変わらず有界のままである。
    pub(crate) fn extend_deadline(&mut self, extra: Duration) {
        let candidate = Instant::now() + extra;
        if candidate > self.deadline {
            self.deadline = candidate;
        }
    }
}

/// OS のソケットタイムアウトに由来する `io::Error` を、共通の
/// `ErrorKind::TimedOut` へ正規化する。
///
/// unix のブロッキング
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
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "deadline exceeded while reading",
                ));
            }
            self.stream.set_read_timeout(Some(remaining))?;
            match self.stream.read(buf).map_err(normalize_timeout_error) {
                // シグナル配送等による `EINTR` は失敗ではない（std の他の
                // I/O 実装が内部でリトライするのと同じ扱い）。期限は
                // ループ先頭で毎回再確認するため、無期限リトライにはならない。
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

impl Write for DeadlineReader {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "deadline exceeded while writing",
                ));
            }
            self.stream.set_write_timeout(Some(remaining))?;
            match self.stream.write(buf).map_err(normalize_timeout_error) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
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

    /// `Bool` のときだけ中身を返す。`result.isError` の型検証に使う。
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
/// 以前は `id` が一致し `result.isError` が
/// `true` でなければ成功として扱っており、`result` が `null`・`{}` の
/// ような空応答でも `goto` の成功と誤判定し得た（`goto` は本文を見ない
/// ため、ページ遷移に失敗した応答をそのまま成功扱いにし、続く `html`・
/// `tree` を別ページの結果として計測する経路になり得た）。ここで
/// JSON-RPC 2.0 の封筒（`jsonrpc`・`id`・`error`/`result` の排他）と、
/// `method` ごとに要求される `result` の最小限の形を検証する
/// （`tools/call` は `content` が配列（MCP 2025-06-18 の仕様どおり空配列も
/// 許す）で各要素に `type` を持ち、
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
        // 「`initialize` や `tools/list`
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
    // MCP 2025-06-18 の `CallToolResult.content` は空配列を許す（例えば
    // `goto` のように応答本体を持たないツールがあり得る）。空・欠けた
    // ページ内容から誤って「削減率 100%」等を導出しないための防御は、
    // 呼び出し側（`measure_token_reduction`）が `extract_text` の結果と
    // `FIXTURE_MARKERS` の内容ベース検証で行う（この関数はあくまで
    // JSON-RPC 2.0・MCP の形式契約だけを見る）。
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
    DuplicateKey(String),
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

/// RFC 8259 §2 が JSON の空白として定義する 4 種のみを空白として扱う。
/// `char::is_whitespace`（Unicode の広い空白定義）を使うと、RFC 8259 では
/// 非空白の Unicode 空白文字（例: U+00A0 NBSP）を暗黙に読み飛ばしてしまい、
/// MCP 応答（外部プロセスからの入力）に対して寛容すぎるパーサーになる。
fn is_json_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn skip_whitespace(chars: &[char], pos: &mut usize) {
    while let Some(&c) = chars.get(*pos) {
        if is_json_whitespace(c) {
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
    // RFC 8259 §6: 整数部の先頭が `0` の場合はそれ 1 桁のみで終わらなければ
    // ならない（`01`・`00` 等の先頭ゼロを許さない）。`-0` 自体は許容する。
    let int_start = *pos;
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
    let int_len = *pos - int_start;
    if int_len > 1 && chars.get(int_start) == Some(&'0') {
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
            // 未エスケープの U+0000〜U+001F 制御
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
        // RFC 8259 は重複キーの扱いを規定しないが、後勝ちで黙って上書きすると
        // MCP 応答（外部プロセスからの入力）で意図的な重複キーを使った
        // 値のすり替えに気づけない。ここでは明示的にエラーにする
        // （coding-rust.md「外部入力の経路」）。
        if map.contains_key(&key) {
            return Err(JsonParseError::DuplicateKey(key));
        }
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

    // `HTTP/1.0`・`HTTP/1.1` のいずれかに続く
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

    // readiness probe の応答本文が
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

    // JSON オブジェクトでありさえすれば
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

    // `Skipped`・
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

    // 未エスケープの制御文字（U+0000〜U+001F）を
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

    // RFC 8259 §6: 先頭ゼロ（`01`・`00`）は許さない。`0`・`-0`・`0.5` は
    // 許容する（`0` 単独・小数点付きは先頭ゼロ規則の対象外）。
    #[test]
    fn parse_json_rejects_leading_zero() {
        assert_eq!(parse_json("01"), Err(JsonParseError::InvalidNumber));
        assert_eq!(parse_json("-01"), Err(JsonParseError::InvalidNumber));
        assert_eq!(parse_json("00"), Err(JsonParseError::InvalidNumber));
    }

    #[test]
    fn parse_json_accepts_zero_and_negative_zero() {
        assert_eq!(parse_json("0"), Ok(JsonValue::Number(0.0)));
        assert_eq!(parse_json("-0"), Ok(JsonValue::Number(-0.0)));
        assert_eq!(parse_json("0.5"), Ok(JsonValue::Number(0.5)));
    }

    // RFC 8259 §2 の空白は ` `・`\t`・`\n`・`\r` の 4 種のみ。U+00A0 等の
    // 他の Unicode 空白は空白として読み飛ばさず、値の外側にある不正な文字
    // として扱う。
    #[test]
    fn parse_json_rejects_non_rfc8259_whitespace() {
        assert!(parse_json("\u{00A0}1").is_err());
    }

    #[test]
    fn parse_json_rejects_duplicate_object_keys() {
        assert_eq!(
            parse_json(r#"{"a":1,"a":2}"#),
            Err(JsonParseError::DuplicateKey("a".to_string()))
        );
    }

    // PERF-3/AISNAP-1: `fixture_url` は常に 127.0.0.1・指定ポート・渡した
    // パスの http URL だけを組み立てる（検証ではなく構造で保証する）。
    #[test]
    fn fixture_url_builds_local_http_url() {
        assert_eq!(
            fixture_url(9400, "/article.html"),
            "http://127.0.0.1:9400/article.html"
        );
    }

    #[test]
    fn fixture_url_reflects_the_given_port() {
        assert_eq!(fixture_url(1, "/"), "http://127.0.0.1:1/");
        assert_eq!(fixture_url(65535, "/"), "http://127.0.0.1:65535/");
    }

    // fixture 配信サーバー（measure.rs の handle_fixture_connection）が
    // 使うリクエスト行パーサー。
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
    fn is_valid_http_version_accepts_only_1_0_and_1_1() {
        assert!(is_valid_http_version("HTTP/1.1"));
        assert!(is_valid_http_version("HTTP/1.0"));
    }

    // このベンチが受理する HTTP バージョンは 1.0/1.1 のみ（`HTTP/2.0` も
    // 含めて他は拒否する）。リクエスト行・レスポンスステータス行の両方が
    // この関数を通るため、ここで拒否すれば両方に効く。
    #[test]
    fn is_valid_http_version_rejects_other_forms() {
        assert!(!is_valid_http_version("HTTP/2.0"));
        assert!(!is_valid_http_version("HTTP/potato"));
        assert!(!is_valid_http_version("HTTP/1"));
        assert!(!is_valid_http_version("HTTP/1.1.1"));
        assert!(!is_valid_http_version("HTTPS/1.1"));
        assert!(!is_valid_http_version("http/1.1"));
        assert!(!is_valid_http_version(""));
    }

    // リクエスト行・ヘッダ行の読み取りが
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

    #[test]
    fn parse_trial_count_absent_uses_default() {
        assert_eq!(parse_trial_count(None, 5, 20), Ok(5));
    }

    #[test]
    fn parse_trial_count_valid_value_in_range() {
        let raw = std::ffi::OsString::from("3");
        assert_eq!(parse_trial_count(Some(&raw), 5, 20), Ok(3));
    }

    // 不正値（非数値・範囲外・空白のみ）は黙って既定値へフォールバックせず
    // `Err` にする（誤設定に気づけないまま意図しない試行回数で計測しない）。
    #[test]
    fn parse_trial_count_non_numeric_is_an_error() {
        let raw = std::ffi::OsString::from("not-a-number");
        assert!(parse_trial_count(Some(&raw), 5, 20).is_err());
    }

    #[test]
    fn parse_trial_count_zero_is_an_error() {
        let raw = std::ffi::OsString::from("0");
        assert!(parse_trial_count(Some(&raw), 5, 20).is_err());
    }

    #[test]
    fn parse_trial_count_above_max_is_an_error() {
        let raw = std::ffi::OsString::from("21");
        assert!(parse_trial_count(Some(&raw), 5, 20).is_err());
    }

    #[test]
    fn parse_trial_count_whitespace_only_is_an_error() {
        let raw = std::ffi::OsString::from("   ");
        assert!(parse_trial_count(Some(&raw), 5, 20).is_err());
    }

    #[test]
    fn parse_args_env_value_absent_uses_default() {
        assert_eq!(
            parse_args_env_value(
                "LIGHTPANDA_SERVE_ARGS",
                None,
                "serve --port {port}",
                4096,
                64
            ),
            Ok(vec![
                "serve".to_string(),
                "--port".to_string(),
                "{port}".to_string()
            ])
        );
    }

    #[test]
    fn parse_args_env_value_valid_value_is_split() {
        let raw = std::ffi::OsString::from("mcp --flag");
        assert_eq!(
            parse_args_env_value("LIGHTPANDA_MCP_ARGS", Some(&raw), "mcp", 4096, 64),
            Ok(vec!["mcp".to_string(), "--flag".to_string()])
        );
    }

    // 非 UTF-8 は黙って既定値へフォールバックせず `Err` にする（誤設定を
    // 「未設定」と区別する）。
    #[cfg(unix)]
    #[test]
    fn parse_args_env_value_non_utf8_is_an_error() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(&[0xff, 0xfe]).to_os_string();
        let result = parse_args_env_value("LIGHTPANDA_SERVE_ARGS", Some(&raw), "serve", 4096, 64);
        assert!(
            result.is_err(),
            "non-UTF-8 SERVE_ARGS should be an error, not silently fall back to the default"
        );
    }

    #[test]
    fn parse_args_env_value_exceeds_byte_limit_is_an_error() {
        let raw = std::ffi::OsString::from("a".repeat(10));
        assert!(parse_args_env_value("LIGHTPANDA_SERVE_ARGS", Some(&raw), "serve", 4, 64).is_err());
    }

    #[test]
    fn parse_args_env_value_exceeds_count_limit_is_an_error() {
        let raw = std::ffi::OsString::from("a b c d e");
        assert!(
            parse_args_env_value("LIGHTPANDA_SERVE_ARGS", Some(&raw), "serve", 4096, 2).is_err()
        );
    }

    // `<PREFIX>_BIN` 未設定（`raw: None`）は既存の「バイナリ未設定は
    // Skipped」契約を保つため `(None, None)` にする。
    #[test]
    fn resolve_bin_value_absent_is_none_without_error() {
        assert_eq!(resolve_bin_value("LIGHTPANDA_BIN", None), (None, None));
    }

    #[test]
    fn resolve_bin_value_existing_file_resolves_to_that_path() {
        let path = std::env::temp_dir().join(format!(
            "fandhe-bench-resolve-bin-value-test-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"stub").expect("write temp file");
        let raw = std::ffi::OsString::from(path.as_os_str());
        let (bin, bin_error) = resolve_bin_value("LIGHTPANDA_BIN", Some(&raw));
        let _ = std::fs::remove_file(&path);
        assert_eq!(bin, Some(path));
        assert_eq!(bin_error, None);
    }

    #[test]
    fn resolve_bin_value_missing_path_is_an_error_not_skipped() {
        let missing = std::env::temp_dir().join("fandhe-bench-resolve-bin-value-missing-xyz");
        let raw = std::ffi::OsString::from(missing.as_os_str());
        let (bin, bin_error) = resolve_bin_value("LIGHTPANDA_BIN", Some(&raw));
        assert_eq!(bin, None);
        assert!(
            bin_error
                .expect("expected error")
                .contains("could not be accessed")
        );
    }

    // 区切り文字を含まない名前（例: `lightpanda`）は cwd 相対に解決され、
    // 返る `bin` は絶対パスになる（`Command::new` が PATH 検索へ
    // フォールバックしないよう、以後の起動・サイズ計測ですべて同じ絶対
    // パスを使うための契約）。
    #[test]
    fn resolve_bin_value_relative_name_is_resolved_to_an_absolute_path() {
        let dir = std::env::temp_dir().join(format!(
            "fandhe-bench-resolve-bin-value-relative-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file_name = "fandhe-bench-resolve-bin-value-relative-bin";
        std::fs::write(dir.join(file_name), b"stub").expect("write temp file");

        let original_cwd = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(&dir).expect("set_current_dir");
        let raw = std::ffi::OsString::from(file_name);
        let result = resolve_bin_value("LIGHTPANDA_BIN", Some(&raw));
        std::env::set_current_dir(&original_cwd).expect("restore current_dir");
        let _ = std::fs::remove_dir_all(&dir);

        let (bin, bin_error) = result;
        assert_eq!(bin_error, None);
        let bin = bin.expect("expected a resolved path");
        assert!(
            bin.is_absolute(),
            "a name without a path separator should resolve to an absolute path: {bin:?}"
        );
        assert!(
            bin.ends_with(file_name),
            "the absolute path should still end with the original file name: {bin:?}"
        );
    }

    #[test]
    fn resolve_bin_value_directory_is_an_error() {
        let dir = std::env::temp_dir();
        let raw = std::ffi::OsString::from(dir.as_os_str());
        let (bin, bin_error) = resolve_bin_value("LIGHTPANDA_BIN", Some(&raw));
        assert_eq!(bin, None);
        assert!(
            bin_error
                .expect("expected error")
                .contains("is not a regular file")
        );
    }

    #[test]
    fn resolve_bin_value_whitespace_only_is_an_error_not_skipped() {
        let raw = std::ffi::OsString::from("   ");
        let (bin, bin_error) = resolve_bin_value("LIGHTPANDA_BIN", Some(&raw));
        assert_eq!(bin, None);
        assert!(bin_error.expect("expected error").contains("set but empty"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bin_value_non_utf8_is_an_error() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(&[0xff, 0xfe]).to_os_string();
        let (bin, bin_error) = resolve_bin_value("LIGHTPANDA_BIN", Some(&raw));
        assert_eq!(bin, None);
        assert!(
            bin_error
                .expect("expected error")
                .contains("not valid UTF-8")
        );
    }

    // `kill_process_group` が使う「終了ステータスから成功／失敗を判定する」
    // ロジック（`unsafe` は使わない。`kill_process_group` 自体の単体テストは
    // 本ファイルの `kill_process_group_kills_child_and_grandchild` を参照）を
    // 実プロセスを起動して単独で検証する。
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

    // kill 後の再 probe でまだ応答があれば
    // 「その試行はポート競合」として `Error` にする判定ロジックの単体
    // テスト。
    #[test]
    fn port_conflict_error_none_when_no_response_after_kill() {
        assert_eq!(port_conflict_error("lightpanda", 9400, false), None);
    }

    #[test]
    fn port_conflict_error_some_when_still_responds_after_kill() {
        let err = port_conflict_error("lightpanda", 9400, true).expect("expected Some(reason)");
        assert!(err.contains("9400"));
        assert!(err.contains("lightpanda"));
        assert!(err.contains("port conflict"));
    }

    /// 新しいプロセスグループで子（`sh`）と孫（`sleep`）を
    /// 起動し、`kill_process_group`（子の PID）の後に子・孫の両方が
    /// 終了していることを 3 OS CI で実証する。孫の PID は子の標準出力
    /// から受け取る。
    ///
    /// 注記: 一部のサンドボックス環境では、プロセスグループへのシグナル
    /// 配送自体が制限されている場合がある（直接の子への `kill`（正の
    /// PID）は機能するが、プロセスグループ宛て（負の PID）のシグナル
    /// 配送だけがサンドボックスの制約で機能しない環境を確認済み）。この
    /// テストはアサーションを弱めず、実際に子・孫の両方が終了していることを
    /// 要求する。ローカルの
    /// サンドボックスで（環境起因で）失敗する場合は、その旨をそのまま
    /// 報告し、macOS・Linux の CI（`ci.md`「3 OS CI」）での結果に判断を
    /// 委ねる。
    #[cfg(unix)]
    #[test]
    fn kill_process_group_kills_child_and_grandchild() {
        // `kill -0 <pid>` で、指定した PID のプロセスが生存しているかを
        // 確認する（このテスト専用のローカルクロージャ。`kill -0` は
        // シグナルを送らず存在確認のみ行う POSIX の慣用手段）。
        let process_is_alive = |pid: u32| -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        let mut command = std::process::Command::new("sh");
        apply_new_process_group(&mut command);
        let mut child = command
            .arg("-c")
            // 孫（`sleep`）をバックグラウンドで起動し、その PID を
            // 標準出力へ書き出してから `wait` で孫の終了を待つ
            // （`sh` 自身は孫が生きている間ずっと生存する）。
            .arg("sleep 30 & echo $!; wait")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sh");
        let child_pid = child.id();

        let stdout = child.stdout.take().expect("child stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut line).expect("read grandchild pid");
        let grandchild_pid: u32 = line.trim().parse().expect("parse grandchild pid");

        // kill 前に子・孫がともに生存していることを確認する（テスト自体の
        // 前提が壊れていないことの確認）。
        assert!(
            process_is_alive(child_pid),
            "child (pid={child_pid}) should be alive before kill_process_group"
        );
        assert!(
            process_is_alive(grandchild_pid),
            "grandchild (pid={grandchild_pid}) should be alive before kill_process_group"
        );

        let kill_result = kill_process_group(child_pid);
        // `kill_process_group` はシグナル送信のみで、対象の実際の終了
        // （プロセステーブルからの除去）は非同期に起こり得るため、
        // `child.wait()`（直接の子。`sh` 自身）で確実に完了を待ってから
        // 判定する。
        let wait_result = child.wait();

        assert!(
            kill_result.is_ok(),
            "kill_process_group should succeed for a process we just spawned: {kill_result:?}"
        );
        assert!(
            wait_result.is_ok(),
            "waiting for the direct child to exit should succeed: {wait_result:?}"
        );

        // シグナル配送から実際のプロセス終了までの短い遅延を許容する。
        for _ in 0..50 {
            if !process_is_alive(child_pid) && !process_is_alive(grandchild_pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert!(
            !process_is_alive(child_pid),
            "child (pid={child_pid}) should be dead after kill_process_group"
        );
        assert!(
            !process_is_alive(grandchild_pid),
            "grandchild (pid={grandchild_pid}) should be dead after kill_process_group \
             (not just the direct child; this is the whole point of process-group kill)"
        );
    }

    /// PERF-3/PERF-6: `wait_with_deadline` は期限を超えて生存し続ける
    /// 子プロセスを kill し、期限を大きく超えて（テストでは 30 秒）
    /// ブロックしないことを確認する（[`kill_process_group`]・
    /// `measure.rs` の `sample_rss_kb` が使う共通プリミティブ）。
    #[cfg(unix)]
    #[test]
    fn wait_with_deadline_kills_process_that_outlives_the_deadline() {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().expect("spawn sh sleep 30");

        let start = Instant::now();
        let result = wait_with_deadline(&mut child, std::time::Duration::from_millis(300));
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "expected timeout error, got {result:?} (process should not exit on its own within 300ms of a 30s sleep)"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "wait_with_deadline should return soon after the deadline, took {elapsed:?}"
        );
    }

    /// 期限内に自発的に終了するプロセスは、期限を待たずに `Ok` で返る。
    #[cfg(unix)]
    #[test]
    fn wait_with_deadline_returns_ok_for_process_exiting_before_deadline() {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().expect("spawn sh exit 0");

        let result = wait_with_deadline(&mut child, std::time::Duration::from_secs(5));
        match result {
            Ok(status) => assert!(status.success()),
            Err(e) => panic!("expected Ok, got {e}"),
        }
    }

    /// `run_with_deadline` は spawn から完了待ちまでを一括で行う
    /// （`kill_process_group` が実際に使う経路）。
    #[cfg(unix)]
    #[test]
    fn run_with_deadline_reports_success_exit_status() {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let status =
            run_with_deadline(command, std::time::Duration::from_secs(5)).expect("run succeeds");
        assert!(status.success());
    }

    /// `run_capturing_output_with_deadline` は `sample_rss_kb` が使う経路。
    /// stdout を採取しつつ期限内に完了することを確認する。
    #[cfg(unix)]
    #[test]
    fn run_capturing_output_with_deadline_captures_stdout() {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("printf hello");
        let (status, stdout) =
            run_capturing_output_with_deadline(command, std::time::Duration::from_secs(5), 4096)
                .expect("run succeeds");
        assert!(status.success());
        assert_eq!(stdout, b"hello");
    }

    /// `max_stdout_bytes` を超える出力は切り詰め、無制限確保しない
    /// （coding-rust.md「長さ・件数を上限検証」）。
    #[cfg(unix)]
    #[test]
    fn run_capturing_output_with_deadline_caps_stdout_length() {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("printf '0123456789'");
        let (status, stdout) =
            run_capturing_output_with_deadline(command, std::time::Duration::from_secs(5), 4)
                .expect("run succeeds");
        assert!(status.success());
        assert_eq!(stdout.len(), 4);
    }

    // `Content-Length` が読み取り上限を
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

    // `Content-Length` ヘッダーが存在するのに値が不正
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

    // fixture はコンパイル時に埋め込んだ
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

    // `FIXTURE_MARKERS` は `goto` 後の内容検証に使うため、
    // `FIXTURE_TABLE` と対象パスの集合が一致していること・各マーカーが
    // 対応する fixture の実際のコンテンツに含まれていること・マーカー同士が
    // 互いの部分文字列にならない（一意性）ことを確認する。
    #[test]
    fn fixture_table_and_markers_cover_the_same_paths() {
        let table_paths: std::collections::BTreeSet<&str> =
            FIXTURE_TABLE.iter().map(|(path, _, _)| *path).collect();
        let marker_paths: std::collections::BTreeSet<&str> =
            FIXTURE_MARKERS.iter().map(|(path, _)| *path).collect();
        assert_eq!(
            table_paths, marker_paths,
            "FIXTURE_TABLE and FIXTURE_MARKERS must cover exactly the same paths"
        );
    }

    // マーカーが単に fixture の
    // どこかに出現するだけでは不十分（`<title>`（`<head>` 内）にしか無い
    // 文字列は、アクセシビリティツリーに現れる保証が無いため）。マーカーが
    // `<body>` 内の見出し要素（`<h1>`）のテキストとして実在すること、かつ
    // `<head>` 内には（見出しタグとしては）現れないことを確認する。
    #[test]
    fn fixture_markers_appear_as_body_heading_text() {
        for (path, _content_type, content) in FIXTURE_TABLE {
            let marker = fixture_marker(path).unwrap_or_else(|| {
                panic!("no marker registered for {path} (see fixture_table_and_markers_cover_the_same_paths)")
            });
            let heading_tag = format!("<h1>{marker}</h1>");

            let body_start = content
                .find("<body")
                .unwrap_or_else(|| panic!("fixture {path} has no <body> tag"));
            let body_end = content
                .find("</body>")
                .unwrap_or_else(|| panic!("fixture {path} has no </body> tag"));
            assert!(
                body_end > body_start,
                "fixture {path}: </body> appears before <body>"
            );
            let body = &content[body_start..body_end];
            assert!(
                body.contains(&heading_tag),
                "fixture {path}: marker {marker:?} must appear as the text of a heading \
                 element (expected to find {heading_tag:?}) inside <body>; a marker that only \
                 exists in <head> (e.g. <title>) is not guaranteed to appear in an \
                 accessibility tree"
            );

            if let (Some(head_start), Some(head_end)) =
                (content.find("<head>"), content.find("</head>"))
            {
                let head = &content[head_start..head_end];
                assert!(
                    !head.contains(&heading_tag),
                    "fixture {path}: the marker heading tag must live in <body>, not <head>"
                );
            }
        }
    }

    #[test]
    fn fixture_markers_are_mutually_exclusive() {
        for (i, (path_a, marker_a)) in FIXTURE_MARKERS.iter().enumerate() {
            for (path_b, marker_b) in FIXTURE_MARKERS.iter().skip(i + 1) {
                assert_ne!(
                    marker_a, marker_b,
                    "{path_a} and {path_b} must not share the same marker"
                );
                assert!(
                    !marker_a.contains(marker_b) && !marker_b.contains(marker_a),
                    "markers for {path_a} ({marker_a:?}) and {path_b} ({marker_b:?}) must not be \
                     substrings of one another, or a goto to one page could be mistaken for the other"
                );
            }
        }
    }

    #[test]
    fn fixture_marker_unknown_path_is_none() {
        assert_eq!(fixture_marker("/does-not-exist.html"), None);
    }

    // fixture は
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
            // プロトコル相対参照の簡易検出。
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

    // 対象バイナリ未設定（`bin: None`・`bin_error: None`）は、fixture
    // サーバーの起動結果に関わらず常に `Skipped` でなければならない
    // （`Error` になってはいけない）。
    #[test]
    fn token_reduction_gate_unconfigured_is_skipped_even_if_server_failed() {
        let outcome = token_reduction_gate(None, None, Some("bind failed"), "fandhe-browser");
        match outcome {
            Some(Outcome::Skipped(reason)) => {
                assert!(reason.contains("binary path not configured"));
            }
            other => panic!("expected Some(Outcome::Skipped(_)), got {other:?}"),
        }
    }

    #[test]
    fn token_reduction_gate_unconfigured_without_server_error_is_skipped() {
        let outcome = token_reduction_gate(None, None, None, "fandhe-browser");
        assert!(matches!(outcome, Some(Outcome::Skipped(_))));
    }

    #[test]
    fn token_reduction_gate_configured_with_server_error_is_error() {
        let bin = PathBuf::from("lightpanda-bin");
        let outcome = token_reduction_gate(Some(&bin), None, Some("bind failed"), "lightpanda");
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
        assert_eq!(
            token_reduction_gate(Some(&bin), None, None, "lightpanda"),
            None
        );
    }

    // `<PREFIX>_BIN` が設定されているのに解決できなかった（`bin_error`）
    // 場合は、`bin` が `None` でも `Skipped` ではなく `Error` にする
    // （設定ミスと「そもそも未設定」を区別する）。
    #[test]
    fn token_reduction_gate_bin_error_is_error_even_without_server_error() {
        let outcome = token_reduction_gate(None, Some("bin is not a file"), None, "lightpanda");
        match outcome {
            Some(Outcome::Error(reason)) => {
                assert!(reason.contains("bin is not a file"));
            }
            other => panic!("expected Some(Outcome::Error(_)), got {other:?}"),
        }
    }

    // `measure_idle_rss` の Windows 版が `target.bin` を確認せず常に
    // `Outcome::Unsupported` を返すと、「対象バイナリ未設定なら全計測項目が
    // Skipped」という契約に反する。OS に依存しないこの判定を共通関数として
    // 検証する（`measure_cold_start`・`measure_binary_size`・unix/windows
    // 両方の `measure_idle_rss`・[`token_reduction_gate`] がすべてこの関数を
    // 通す）。
    #[test]
    fn require_bin_none_is_skipped() {
        match require_bin(None, None, "fandhe-browser") {
            Err(Outcome::Skipped(reason)) => {
                assert!(reason.contains("binary path not configured"));
            }
            other => panic!("expected Err(Outcome::Skipped(_)), got {other:?}"),
        }
    }

    #[test]
    fn require_bin_some_returns_the_path() {
        let bin = PathBuf::from("lightpanda-bin");
        assert_eq!(require_bin(Some(&bin), None, "lightpanda"), Ok(&bin));
    }

    // `bin_error` が `Some` のときは `bin` の値に関わらず `Error` にする
    // （`bin` は解決失敗時に呼び出し側で常に `None` にする契約だが、この
    // 関数自体は不変条件に依存せず `bin_error` を優先する）。
    #[test]
    fn require_bin_error_takes_precedence_and_is_an_error() {
        match require_bin(None, Some("not a regular file"), "lightpanda") {
            Err(Outcome::Error(reason)) => {
                assert!(reason.contains("not a regular file"));
            }
            other => panic!("expected Err(Outcome::Error(_)), got {other:?}"),
        }
    }

    // `SERVE_ARGS`・`MCP_ARGS` の検証
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

    // `result` が `null`・`{}` でも
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
        // `goto` の応答が `result: null` や `result: {}`（`content` 欠如）
        // でも、`isError` が無いというだけで成功扱いにしてはならない。
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

    // MCP 2025-06-18 の `CallToolResult.content` は空配列を許す（`goto` の
    // ように応答本体を持たないツールがあり得るため）。ページ内容の欠如は
    // `measure_token_reduction` 側の `extract_text`・マーカー検証で検出する
    // （このレイヤは JSON-RPC/MCP の形式契約だけを見る）。
    #[test]
    fn validate_mcp_response_accepts_tools_call_empty_content_array() {
        let value =
            parse_json(r#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#).expect("valid json");
        assert!(validate_mcp_response(&value, 1.0, "tools/call").is_ok());
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

    // `set_read_timeout` は 1 回の
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
            // ことを検証するため、送信側は本テストの期限（300ms）よりも
            // 十分長く（合計 5 秒）粘る。CI の並行実行によるスケジューリング
            // 遅延を考慮し、期限とアサーションの上限に十分な余裕を持たせる。
            for _ in 0..100 {
                if socket.write_all(b"A").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let stream = TcpStream::connect(addr).expect("connect");
        let deadline_duration = Duration::from_millis(300);
        let deadline = Instant::now() + deadline_duration;
        let mut reader = DeadlineReader::new(stream, deadline);

        let start = Instant::now();
        let mut buf = [0u8; 4096];
        // 個々の `read` は（`DeadlineReader` により毎回残り時間へ短縮
        // されるとはいえ）データが来れば成功し得るため、`TimedOut` に
        // 達するまでループする。
        //
        // 送信側
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
        // 送信側は 100 回 × 50ms = 5 秒粘るが、`DeadlineReader` は
        // 接続全体の期限（300ms）で打ち切るため、実測時間がそれを大幅に
        // 超えないことを確認する。上限は送信側の総粘り時間（5 秒）より
        // 十分小さい絶対値にし、CI の並行実行による多少のスケジューリング
        // 遅延を吸収しつつ、「打ち切られず送信側が尽きるまで待った」という
        // 誤判定にはならないようにする。
        assert!(
            elapsed < Duration::from_secs(3),
            "DeadlineReader should cut off around the deadline, took {elapsed:?}"
        );

        // ソケットを閉じてから送信側スレッドの終了を待つ。閉じないと
        // 送信側は（誰も読んでいなくても）OS の送信バッファへ書き込み
        // 続けてしまい、全 100 回分（5 秒）の `sleep` を律儀に消化して
        // からでないと `join` が返らず、テストが不必要に長くなる。
        drop(reader);
        let _ = handle.join();
    }

    /// fixture サーバーが読み取り絶対期限をちょうど使い切った直後でも
    /// 400/408 応答を書き込めるようにする猶予（`measure.rs` の
    /// `respond_with_io_error` が使う）。期限切れ後に `extend_deadline` を
    /// 呼べば、それ以降の書き込みが（延長前の期限のままなら即座に
    /// `TimedOut` になるところを）成功することを、実際の loopback
    /// ソケットで確認する。
    #[test]
    fn deadline_reader_extend_deadline_allows_write_after_original_deadline_elapsed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        let deadline = Instant::now() + Duration::from_millis(50);
        let mut writer = DeadlineReader::new(server, deadline);
        // 元の期限を確実に過ぎさせる。
        std::thread::sleep(Duration::from_millis(150));

        // 延長前なら、この書き込みは期限切れで即座に `Err` になるはず。
        let before_extend = writer.write(b"x");
        assert!(
            before_extend.is_err(),
            "a write after the original (unextended) deadline should fail: {before_extend:?}"
        );

        writer.extend_deadline(Duration::from_secs(2));
        writer
            .write_all(b"hello")
            .expect("write should succeed after extend_deadline");
        drop(writer);

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set_read_timeout");
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).expect("read_exact");
        assert_eq!(&buf, b"hello");
    }

    // 宣言された長さに届く前に応答が
    // 途中で切れた場合、それまでに読めた断片を成功として扱ってはいけない。
    // `probe_once`（measure.rs）はこの検証を
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

    // unix のブロッキング
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
