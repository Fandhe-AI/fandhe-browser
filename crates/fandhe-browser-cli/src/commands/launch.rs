//! 起動コマンド `launch`（TASK-47.2・Issue #283・`CLI-1`・MS-5）。
//!
//! ブラウザインスタンス（CDP 互換サーバー + AI API）を起動する。`main` の引数なし起動
//! （[`run_default`]）と `launch` サブコマンドは同じ [`start_server`] を通り、出力も同一になる。
//! `cfg(target_os)` 分岐は置かず 3 OS で同一の引数体系・挙動とする（`CLI-1`）。
//!
//! # スタブについて
//!
//! 以下は未実装で、実装済みを装わない（REPAIR-3）。
//!
//! - `--port`・ポート 0・起動後の実アドレスの機械可読通知（`CLI-3`）: #823 で
//!   [`LaunchOptions::addr`] に追加する。現状は [`server::DEFAULT_ADDR`] 固定
//! - `launch --help`: ハンドラは出力先を受け取らないため未対応（`--help` は未知のオプション扱い。
//!   トップレベルの `--help` に `launch` が載る）

use fandhe_browser_profile::ProfileStore;

use super::profile::{self, ProfileSelection};
use super::{CommandArgs, CommandError, CommandOutcome, CommandSpec, UsageError};
use crate::server::{self, StartupError};
use crate::{page_scripts, render_host, startup_config};

/// 登録表（[`super::COMMANDS`]）へ載せる仕様。
pub(crate) const SPEC: CommandSpec = CommandSpec {
    name: "launch",
    summary: "Start the CDP/AI API server with the default or specified profile",
    run,
};

/// 起動オプション。後続の `--port`（#823）の拡張点。`--profile-dir`（#287）は [`profile`] が解釈する。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct LaunchOptions {
    /// 待ち受けアドレス（loopback のみ。`server::bind` が検査する）。
    pub(crate) addr: std::net::SocketAddr,
    /// プロファイルの選択。
    pub(crate) profile: ProfileSelection,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            addr: server::DEFAULT_ADDR,
            profile: ProfileSelection::OsDefault,
        }
    }
}

/// `launch` の引数を解析する（副作用なし）。受け付けるのは `--profile-dir <PATH>` だけ。
pub(crate) fn parse_launch_args(args: &CommandArgs) -> Result<LaunchOptions, UsageError> {
    let mut options = LaunchOptions::default();
    let mut iter = args.as_slice().iter();
    while let Some(arg) = iter.next() {
        if profile::take_profile_dir(arg, &mut iter, &mut options.profile)? {
            continue;
        }
        return Err(if arg.starts_with('-') {
            UsageError::UnknownOption(arg.clone())
        } else {
            UsageError::UnexpectedArgument(arg.clone())
        });
    }
    Ok(options)
}

/// サブコマンド `launch` のハンドラ。
fn run(args: CommandArgs) -> Result<CommandOutcome, CommandError> {
    let options = parse_launch_args(&args)?;
    // パス検証は設定読み込み・bind より前に確定させる（失敗時はサーバーを起動しない）。
    let store = profile::build_store(&options.profile)?;
    start_server(&options, &store)?;
    Ok(CommandOutcome::Completed)
}

/// 引数なし起動のハンドラ（`main` が注入する）。`launch` と同じ既定オプションで起動する。
pub(crate) fn run_default() -> Result<CommandOutcome, CommandError> {
    let options = LaunchOptions::default();
    let store = profile::build_store(&options.profile)?;
    start_server(&options, &store)?;
    Ok(CommandOutcome::Completed)
}

/// 設定読み込み → 描画ホスト解決 → current_thread ランタイム構築 → サーバー起動。
/// 構築失敗は panic させず [`StartupError`] として返す（release は `panic = "abort"`）。
pub(crate) fn start_server(
    options: &LaunchOptions,
    store: &dyn ProfileStore,
) -> Result<(), StartupError> {
    // 設定エラーはプロファイル open・bind より前に確定させる（サーバーを起動しない）。
    // `[js] execute_page_scripts` からページスクリプト実行アクターのハンドルを作る（TASK-112・
    // `JS-1`・`JS-4`）。ハンドルの構築だけではスレッドも JS 子プロセスも作らない（遅延起動）。
    let config = startup_config::load_startup_config()?;
    let page_scripts = page_scripts::handle_from_config(config.js());
    // 描画ホストの解決も bind より前に行う。不正なパスはサーバーを起動せず非 0 終了させる（fail-closed）。
    // 設定キーは core の `Config` に未定義のため `None` 固定（REPAIR-3。後続課題で config から渡す）。
    // 解決できてもホストはここでは起動しない（`HostRenderer` は初回の描画要求で遅延起動する）。
    let renderer = render_host::renderer_from_process(None)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    // AI API ルータ（`/ai/*`）は既定の追加ルータとして CDP と同一リスナーへ合成する（TASK-19.3）。
    runtime.block_on(server::run(
        store,
        options.addr,
        server::default_router_factories(),
        page_scripts,
        renderer,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::COMMANDS;

    fn args(v: &[&str]) -> CommandArgs {
        CommandArgs::new(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn cli1_launch_is_registered() {
        assert_eq!(COMMANDS.iter().filter(|c| c.name == "launch").count(), 1);
    }

    #[test]
    fn cli1_launch_without_args_uses_defaults() {
        let o = parse_launch_args(&args(&[])).expect("no args is valid");
        assert_eq!(o.addr.to_string(), "127.0.0.1:9333");
        assert_eq!(o.profile, ProfileSelection::OsDefault);
    }

    #[test]
    fn cli1_launch_accepts_profile_dir() {
        let o = parse_launch_args(&args(&["--profile-dir", "p"])).expect("ok");
        assert_eq!(o.profile, ProfileSelection::Explicit("p".into()));
        assert_eq!(
            parse_launch_args(&args(&["--profile-dir"])),
            Err(UsageError::MissingArgument("PATH"))
        );
    }

    #[test]
    fn cli1_launch_profile_is_still_unknown_option() {
        assert_eq!(
            parse_launch_args(&args(&["--profile", "chrome"])),
            Err(UsageError::UnknownOption("--profile".into()))
        );
    }

    #[test]
    fn cli1_launch_rejects_unknown_option() {
        assert_eq!(
            parse_launch_args(&args(&["--bogus"])),
            Err(UsageError::UnknownOption("--bogus".into()))
        );
    }

    #[test]
    fn cli1_launch_rejects_positional_argument() {
        let e = parse_launch_args(&args(&["extra"])).expect_err("usage error");
        assert_eq!(e, UsageError::UnexpectedArgument("extra".into()));
        let ce = CommandError::from(e);
        assert_eq!(
            ce.to_string(),
            "unexpected argument 'extra'; run 'fandhe-browser --help' for usage"
        );
        assert_eq!(
            format!("{:?}", ce.exit_code()),
            format!("{:?}", std::process::ExitCode::from(2))
        );
    }
}
