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
//! - 起動後の実アドレスの機械可読通知（`CLI-3`）: `--port <PORT>`・ポート 0 は #823 で実装済み
//!   （待ち受け IP は 127.0.0.1 固定。SEC-4）。実ポートは stderr の
//!   `DevTools listening on ...` 1 行で通知する暫定形式で、出力形式は CLI-3 の確定時に見直す
//! - `launch --help`: ハンドラは出力先を受け取らないため未対応（`--help` は未知のオプション扱い。
//!   トップレベルの `--help` に `launch` が載る）

use fandhe_browser_core::ScriptContextHandle;
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

/// 起動オプション。`--port`（#823・`CLI-3`）は本モジュール、`--profile-dir`（#287）は [`profile`] が解釈する。
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

/// ポート指定オプション名。
const PORT_OPTION: &str = "--port";
/// ポート値の名前（欠落エラーの表示用）。
const PORT_VALUE_NAME: &str = "PORT";

/// `--port` の値を解析する（副作用なし・外部入力）。ASCII 数字だけの 1〜5 文字で `u16` に収まるもの。
/// `0`（OS 割り当て）以外の先頭ゼロは 8 進誤読を避けるため拒否する。`+`・空白・非 ASCII 数字も拒否する。
fn parse_port(value: &str) -> Result<u16, UsageError> {
    let invalid = UsageError::InvalidOptionValue {
        option: PORT_OPTION,
    };
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 5 || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid);
    }
    if bytes.len() > 1 && bytes.first() == Some(&b'0') {
        return Err(invalid);
    }
    value.parse::<u16>().map_err(|_| invalid)
}

/// `--port <PORT>` を 1 つ消費する。該当しなければ `Ok(false)`。重複指定・値の欠落は拒否する。
fn take_port<'a>(
    arg: &str,
    iter: &mut impl Iterator<Item = &'a String>,
    current: &mut Option<u16>,
) -> Result<bool, UsageError> {
    if arg != PORT_OPTION {
        return Ok(false);
    }
    if current.is_some() {
        return Err(UsageError::UnexpectedArgument(arg.to_string()));
    }
    let value = iter
        .next()
        .ok_or(UsageError::MissingArgument(PORT_VALUE_NAME))?;
    if value.starts_with('-') {
        // 後続のオプションを値として飲み込まない。
        return Err(UsageError::MissingArgument(PORT_VALUE_NAME));
    }
    *current = Some(parse_port(value)?);
    Ok(true)
}

/// `launch` の引数を解析する（副作用なし）。受け付けるのは `--profile-dir <PATH>` と `--port <PORT>`。
/// 待ち受け IP は 127.0.0.1 固定で、ホストやアドレスを指定する手段は設けない（SEC-4）。
pub(crate) fn parse_launch_args(args: &CommandArgs) -> Result<LaunchOptions, UsageError> {
    let mut options = LaunchOptions::default();
    let mut port = None;
    let mut iter = args.as_slice().iter();
    while let Some(arg) = iter.next() {
        if take_port(arg, &mut iter, &mut port)? {
            continue;
        }
        if profile::take_profile_dir(arg, &mut iter, &mut options.profile)? {
            continue;
        }
        return Err(if arg.starts_with('-') {
            UsageError::UnknownOption(arg.clone())
        } else {
            UsageError::UnexpectedArgument(arg.clone())
        });
    }
    if let Some(port) = port {
        options.addr = std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port);
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
    // 評価コンテキスト（`Page.createIsolatedWorld`・`Runtime.*` のゲート）も同じ `[js] engine` の
    // 解決結果から作る。エンジンなしのビルドでは無効のまま（TASK-44.9・#1220・`CDP-2`）。
    // 構築だけではスレッドも子プロセスも作らない（遅延起動。`PERF-7`）。新しい設定キーは足さない。
    let script_contexts = ScriptContextHandle::new(config.js());
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
        script_contexts,
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

    #[test]
    fn cli3_launch_port_sets_loopback_addr() {
        let o = parse_launch_args(&args(&["--port", "9444"])).expect("ok");
        assert_eq!(o.addr.to_string(), "127.0.0.1:9444");
        let o = parse_launch_args(&args(&["--port", "0"])).expect("ok");
        assert_eq!(o.addr.to_string(), "127.0.0.1:0");
        let o = parse_launch_args(&args(&["--port", "1"])).expect("ok");
        assert_eq!(o.addr.to_string(), "127.0.0.1:1");
        let o = parse_launch_args(&args(&["--port", "65535"])).expect("ok");
        assert_eq!(o.addr.to_string(), "127.0.0.1:65535");
    }

    #[test]
    fn cli3_launch_port_combines_with_profile_dir() {
        for a in [
            ["--port", "0", "--profile-dir", "p"],
            ["--profile-dir", "p", "--port", "0"],
        ] {
            let o = parse_launch_args(&args(&a)).expect("ok");
            assert_eq!(o.addr.to_string(), "127.0.0.1:0");
            assert_eq!(o.profile, ProfileSelection::Explicit("p".into()));
        }
    }

    #[test]
    fn cli3_launch_rejects_invalid_port_values() {
        for bad in [
            "",
            "65536",
            "99999",
            "123456",
            "abc",
            "9333a",
            "+9333",
            " 9333",
            "9333 ",
            "0x10",
            "9333.0",
            "09333",
            "00",
            "\u{661}\u{662}\u{663}",
        ] {
            let e = parse_launch_args(&args(&["--port", bad])).expect_err(bad);
            assert_eq!(
                e,
                UsageError::InvalidOptionValue { option: "--port" },
                "{bad:?}"
            );
            let ce = CommandError::from(e);
            assert_eq!(
                ce.to_string(),
                "invalid value for option '--port'; run 'fandhe-browser --help' for usage"
            );
            assert_eq!(
                format!("{:?}", ce.exit_code()),
                format!("{:?}", std::process::ExitCode::from(2))
            );
        }
    }

    #[test]
    fn cli3_launch_port_missing_value() {
        for a in [
            vec!["--port"],
            vec!["--port", "--profile-dir", "p"],
            vec!["--port", "-1"],
        ] {
            assert_eq!(
                parse_launch_args(&args(&a)),
                Err(UsageError::MissingArgument("PORT"))
            );
        }
    }

    #[test]
    fn cli3_launch_port_duplicate_is_rejected() {
        assert_eq!(
            parse_launch_args(&args(&["--port", "1", "--port", "2"])),
            Err(UsageError::UnexpectedArgument("--port".into()))
        );
    }

    #[test]
    fn sec4_launch_has_no_host_option() {
        for (opt, v) in [
            ("--host", "0.0.0.0"),
            ("--bind", "0.0.0.0:9333"),
            ("--address", "0.0.0.0"),
        ] {
            assert_eq!(
                parse_launch_args(&args(&[opt, v])),
                Err(UsageError::UnknownOption(opt.into()))
            );
        }
        let o = parse_launch_args(&args(&["--port", "8080"])).expect("ok");
        assert_eq!(o.addr.ip(), std::net::Ipv4Addr::LOCALHOST);
    }
}
