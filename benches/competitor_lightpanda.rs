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
//! サーバー（`127.0.0.1` の空きポート。`measure::FixtureServer`）がリポジトリ
//! 同梱の自作 fixture（外部参照を含まない。`support::FIXTURE_TABLE`）を配信し、
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
//!
//! 計測ロジックの配置（レビュー指摘。コーディネーター指示。PR #442
//! 再々々々々々々々々レビュー・`crates/fandhe-browser-core/Cargo.toml:57`）:
//! fixture サーバーの起動・対象プロセスとの通信・計測本体は
//! `competitor_lightpanda/measure.rs`（`measure` モジュール）へ切り出した。
//! `measure_tests.rs`（`[[test]]` ターゲット。`harness = false`）が同じ
//! モジュールを `#[path]` で取り込み、対象バイナリを模したローカルプロセス
//! （テストバイナリ自身の再実行）で fixture サーバーの起動から計測結果・
//! 終了コードまでを結合テストする（AGENTS.md「ユニットテストと結合テストの
//! 併置」）。この `main` は環境変数の解析（[`Target::from_env`]・
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
/// 環境変数によるメモリ過剰消費を防ぐ（レビュー指摘 P1: line 140。
/// coding-rust.md「長さ・件数を上限検証」）。
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

    let (body, exit_code) = measure::run_all(&targets, trials);

    // 終了コードを決める前に必ず JSON を出力する（モジュールドキュメントの
    // 終了コード契約参照。呼び出し側が失敗時も結果 JSON を取得できるようにする）。
    print!("{body}");

    std::process::ExitCode::from(exit_code)
}
