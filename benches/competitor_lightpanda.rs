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
//! `AISNAP-1`（MCP トークン削減率）計測が `goto` する対象: 計測対象ブラウザは
//! 接続時に名前を再解決でき、公開 URL からのリダイレクトにも追従し得るため、
//! 事前の URL 検証をいくら積み増しても実際の接続先（内部アドレスに到達しない
//! こと）を保証できない（SSRF・TOCTOU）。そのため任意 URL を指定する経路は
//! 持たず、本ベンチが自ら起動するローカル静的サーバー（`127.0.0.1` の
//! 空きポート。`measure::FixtureServer`）がリポジトリ同梱の自作 fixture
//! （外部参照を含まない。`support::FIXTURE_TABLE`）だけを配信し、`goto` は
//! `support::fixture_url` でそのサーバーの URL だけを組み立てて渡す（外部
//! URL を渡せる経路自体がない）。これは Lightpanda 本家のベンチ（ローカルで
//! 配信するデモサイトを対象にする）と同じ形である。「外部ネットワークに
//! 出ない」の範囲は `goto` の宛先（計測対象ブラウザがページ取得のために
//! 接続する先）に限る。計測対象プロセス自体が別の目的で行う通信（起動時の
//! 自己診断・アップデート確認等、本ベンチが把握・制御できないもの）は
//! この保証の対象外である。
//!
//! fixture の内容: 記事・一覧・フォームの 3 類型のみ（`support::FIXTURE_TABLE`）。
//! `docs/spec/03-poc/browser-landscape-2026`（PoC-13）の実測（5 類型・
//! トークン削減率 75.1%）とは対象ページの種類・件数が異なるため、本ベンチの
//! `tokenReductionPct` を PoC-13 の数値と直接比較しない。結果 JSON には
//! `tokenReductionPct` を計測しようとした対象として構成されている fixture の
//! 件数（`sitesCount`。計測の成否とは独立。`skipped`/`error` のときも
//! `FIXTURE_TABLE` の件数を返す）も出力する。
//!
//! アイドル RSS（`PERF-6`）: `ps -o rss=` で読むのは起動した直接の子
//! プロセスの RSS のみ。`<PREFIX>_BIN` が実体をラップして別プロセスとして
//! 起動するラッパースクリプト等の場合、実際にメモリを使う実体プロセスの
//! RSS を捕捉できない。
//!
//! `AISNAP-1` の `goto` 成功判定（`support::FIXTURE_MARKERS`）: 各 fixture の
//! `<body>` 内見出し（`<h1>`）のテキストが `html`・`tree` の両方に含まれる
//! ことを確認する。`tree`（アクセシビリティツリー）側の判定は「見出し要素の
//! アクセシブルネームがツリーに現れる」という WAI-ARIA のロールマッピングに
//! 基づく前提に立っており、実際の MCP 実装（Lightpanda 等）でこの前提が
//! 成り立つことは実機で未確認である。
//!
//! 終了コード契約: JSON は必ず stdout へ出力したうえで、いずれかの計測項目が
//! `Outcome::Error`（対象バイナリの起動失敗・MCP 呼び出し失敗等）になった
//! 場合は終了コード `1` で終了する。対象バイナリ未設定による
//! `Outcome::Skipped`・Windows 未対応による `Outcome::Unsupported` のみの
//! 場合は `0` で終了する。試行回数（`COMPETITOR_BENCH_TRIALS`）が不正な
//! 場合は、計測を一切試みずに JSON を出力しないまま終了コード `1` で終了する
//! （[`trial_count`] 参照）。自動計測（CI 等）の呼び出し側はこの終了コードで
//! 成功・失敗を判定できる。
//!
//! 新規外部依存は追加しない（`Cargo.toml` の `dependencies` に変更なし。
//! dependency-policy.md）。`std` のみで完結する。
//!
//! 計測ロジックの配置: fixture サーバーの起動・対象プロセスとの通信・
//! 計測本体は `competitor_lightpanda/measure.rs`（`measure` モジュール）へ
//! 切り出した。`measure_tests.rs`（`[[test]]` ターゲット。`harness = false`）
//! が同じモジュールを `#[path]` で取り込み、対象バイナリを模したローカル
//! プロセス（テストバイナリ自身の再実行）で fixture サーバーの起動から
//! 計測結果・終了コードまでを結合テストする（AGENTS.md「ユニットテストと
//! 結合テストの併置」）。この `main` は環境変数の解析（[`Target::from_env`]・
//! [`trial_count`]）と `measure::run_all` の呼び出し・出力だけを行う薄い
//! ラッパーにする。

#[path = "competitor_lightpanda/measure.rs"]
mod measure;
#[path = "competitor_lightpanda/support.rs"]
mod support;

use std::path::PathBuf;

use measure::Target;

/// 試行回数の既定値・上限（無制限ループ・無制限プロセス起動を避ける。
/// coding-rust.md「長さ・件数を上限検証」）。
const DEFAULT_TRIALS: usize = 5;
const MAX_TRIALS: usize = 20;

/// `<PREFIX>_SERVE_ARGS` / `<PREFIX>_MCP_ARGS`（外部入力）に許す上限。
/// `split_args` へ渡す前にバイト数・分割後の引数個数を検証し、巨大な
/// 環境変数によるメモリ過剰消費を防ぐ。
const MAX_ARGS_ENV_BYTES: usize = 4 * 1024;
const MAX_ARGS_COUNT: usize = 64;

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
            let args = support::split_args(&raw);
            if args.len() > MAX_ARGS_COUNT {
                return Err(format!(
                    "{env_prefix}_{var_suffix} has more than {MAX_ARGS_COUNT} args"
                ));
            }
            Ok(args)
        }
        Err(_) => Ok(support::split_args(default)),
    }
}

/// `<PREFIX>_BIN` を読み取り、[`support::resolve_bin_value`]（純粋関数。
/// 単体テストは `support.rs` 側に置く）へ委譲する薄いラッパー。
fn resolve_bin_env(env_prefix: &str) -> (Option<PathBuf>, Option<String>) {
    let var_name = format!("{env_prefix}_BIN");
    let raw = std::env::var_os(&var_name);
    support::resolve_bin_value(&var_name, raw.as_deref())
}

impl Target {
    /// 環境変数から 1 対象分の設定を構築する（`measure::Target` の唯一の
    /// コンストラクタ。テスト側は対象バイナリを模したローカルプロセス用に
    /// フィールドを直接組み立てるため、この関数を経由しない）。
    fn from_env(
        name: &'static str,
        env_prefix: &str,
        default_serve: &str,
        default_mcp: &str,
    ) -> Self {
        let (bin, bin_error) = resolve_bin_env(env_prefix);
        let serve_args = args_from_env(env_prefix, "SERVE_ARGS", default_serve);
        let mcp_args = args_from_env(env_prefix, "MCP_ARGS", default_mcp);
        let serve_args_error = serve_args.as_ref().err().cloned();
        let mcp_args_error = mcp_args.as_ref().err().cloned();
        Self {
            name,
            bin,
            bin_error,
            serve_args: serve_args.unwrap_or_default(),
            mcp_args: mcp_args.unwrap_or_default(),
            serve_args_error,
            mcp_args_error,
        }
    }
}

/// 試行回数を環境変数から読み取る（`COMPETITOR_BENCH_TRIALS`）。
///
/// 未設定なら [`DEFAULT_TRIALS`]。範囲外・不正値は黙って既定値へ
/// フォールバックせず `Err` にする（[`support::parse_trial_count`]
/// 参照。誤設定に気づけないまま意図しない試行回数で計測しないことが、
/// 「外部入力を fail-closed に扱う」（coding-rust.md）の趣旨に合う）。
fn trial_count() -> Result<usize, String> {
    support::parse_trial_count(
        std::env::var_os("COMPETITOR_BENCH_TRIALS").as_deref(),
        DEFAULT_TRIALS,
        MAX_TRIALS,
    )
}

fn main() -> std::process::ExitCode {
    let trials = match trial_count() {
        Ok(trials) => trials,
        Err(e) => {
            // 計測を一切試みる前の設定エラーであり、`measure::run_all` が
            // 生成する計測結果 JSON の対象がない。モジュールドキュメントの
            // 終了コード契約（計測結果 JSON を伴う 0/1）とは別に、起動時の
            // 設定エラーとして stderr へ出し非ゼロで終了する。
            eprintln!("competitor_lightpanda: invalid configuration: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
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

    let (body, exit_code) = measure::run_all(&targets, trials);

    // 終了コードを決める前に必ず JSON を出力する（モジュールドキュメントの
    // 終了コード契約参照。呼び出し側が失敗時も結果 JSON を取得できるようにする）。
    print!("{body}");

    std::process::ExitCode::from(exit_code)
}
