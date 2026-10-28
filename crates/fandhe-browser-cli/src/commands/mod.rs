//! CLI サブコマンドの登録・ディスパッチの共通基盤（TASK-47.1・`CLI-1`・MS-5）。
//!
//! `main` が `std::env::args_os()` の引数列（プログラム名を除く）をここへ渡し、本モジュールが
//! 「既定ハンドラ（引数なし）」「help」「登録済みコマンド」「usage エラー」のいずれかへ振り分ける。
//! 引数パーサは #281 の決定どおり新規依存なしの手書き（`args_os` 起点。非 UTF-8 でも panic しない）。
//!
//! `query --file PATH`（#824・`PERF-7`）はネットワークを使わずローカル HTML を照会する（cold start 計測用）。
//!
//! 拡張点: 後続の TASK-47.2〜47.5（#283〜#287）は `commands/<name>.rs` を追加し、
//! [`COMMANDS`] へ [`CommandSpec`] を 1 行足す。全コマンド共通のプロファイル指定
//! `--profile-dir <PATH>`（TASK-47.6・#287）はサブコマンド名の後ろに置き、各コマンドが
//! [`profile::take_profile_dir`] で解釈する（サブコマンド名より前の `-` 始まりは従来どおり未知のオプション）。
//!
//! 解析（[`parse`]）と実行（[`dispatch`]）を分けるのは、プロセスを起動せずに単体テストするため。
//! `cfg(target_os)` 分岐は置かず 3 OS で同一挙動とする（`CLI-1`）。

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

use crate::server::StartupError;

pub(crate) mod launch;
pub(crate) mod navigate;
pub(crate) mod profile;
pub(crate) mod query;
pub(crate) mod screenshot;
pub(crate) mod setup;
#[cfg(test)]
pub(crate) mod test_support;

/// usage エラー時の終了コード（`StartupError` 系の 1 と区別する）。
const USAGE_EXIT_CODE: u8 = 2;
/// エラー表示に載せる利用者入力の最大文字数（巨大引数・制御文字の端末出力を避ける）。
const MAX_SHOWN_CHARS: usize = 64;

/// ハンドラへ渡す検証済みの残り引数（サブコマンド名を除く、UTF-8 化済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandArgs {
    rest: Vec<String>,
}

impl CommandArgs {
    /// 解析済みの引数列を組み立てる（ハンドラ単体テスト用）。
    #[cfg(test)]
    pub(crate) fn new(rest: Vec<String>) -> Self {
        Self { rest }
    }

    /// サブコマンド名より後ろの引数列。
    pub(crate) fn as_slice(&self) -> &[String] {
        &self.rest
    }
}

/// コマンド実行の成功結果。将来の拡張に備え真偽値・`()` にしない（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum CommandOutcome {
    /// コマンドが正常に完了した。
    Completed,
    /// help を表示した。
    HelpShown,
}

/// サブコマンドのハンドラ型。
pub(crate) type Handler = fn(CommandArgs) -> Result<CommandOutcome, CommandError>;
/// 引数なし起動のハンドラ型。
pub(crate) type DefaultHandler = fn() -> Result<CommandOutcome, CommandError>;

/// 登録コマンド 1 件（静的な関数ポインタ表。動的ディスパッチ・依存は使わない）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandSpec {
    /// サブコマンド名（`-` で始めない）。
    pub(crate) name: &'static str,
    /// help に載せる 1 行説明（英語）。
    pub(crate) summary: &'static str,
    /// 実行本体。
    pub(crate) run: Handler,
}

/// 本番の登録表。`launch`（#283）・`navigate`（#284）・`query`（#285）・`screenshot`（#286）・`setup`（#290。セットアップ専用）を登録済み。
pub(crate) const COMMANDS: &[CommandSpec] = &[
    launch::SPEC,
    navigate::SPEC,
    query::SPEC,
    screenshot::SPEC,
    setup::SPEC,
];

/// ディスパッチ先の一式。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Registry<'a> {
    /// 引数なしで起動したときのハンドラ（従来のサーバー起動。`main` が注入する）。
    pub(crate) default: DefaultHandler,
    /// 登録済みサブコマンド。
    pub(crate) commands: &'a [CommandSpec],
}

/// 引数列の解析結果。
#[derive(Debug)]
enum Invocation<'a> {
    Default,
    Help,
    Command {
        spec: &'a CommandSpec,
        args: CommandArgs,
    },
}

/// 引数の誤り（呼び出し側の使い方の問題。終了コード 2）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum UsageError {
    /// UTF-8 でない引数（0 始まりの位置、プログラム名を除く）。
    NonUtf8Argument { index: usize },
    /// 未登録のサブコマンド名。
    UnknownCommand(String),
    /// 未知のオプション。
    UnknownOption(String),
    /// 余分な位置引数。
    UnexpectedArgument(String),
    /// 必須の位置引数が無い（引数名）。
    MissingArgument(&'static str),
    /// 同時に指定できないオプションの組（先に現れた方・後に現れた方）。
    ConflictingOptions {
        first: &'static str,
        second: &'static str,
    },
    /// オプションの値が不正（オプション名）。
    InvalidOptionValue { option: &'static str },
    /// 認識はするが現時点では未対応のオプション（オプション名。成功を装わず拒否する。REPAIR-3）。
    UnsupportedOption(&'static str),
}

/// 利用者入力を端末へ安全に出すため、エスケープして長さを切り詰める。
fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(MAX_SHOWN_CHARS)
        .flat_map(char::escape_debug)
        .collect();
    if s.chars().count() > MAX_SHOWN_CHARS {
        out.push_str("...");
    }
    out
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUtf8Argument { index } => {
                write!(f, "argument {index} is not valid UTF-8")
            }
            Self::UnknownCommand(name) => write!(f, "unknown command '{}'", sanitize(name)),
            Self::UnknownOption(name) => write!(f, "unknown option '{}'", sanitize(name)),
            Self::UnexpectedArgument(a) => write!(f, "unexpected argument '{}'", sanitize(a)),
            Self::MissingArgument(name) => write!(f, "missing required argument '{name}'"),
            Self::ConflictingOptions { first, second } => {
                write!(
                    f,
                    "options '{first}' and '{second}' cannot be used together"
                )
            }
            Self::InvalidOptionValue { option } => {
                write!(f, "invalid value for option '{option}'")
            }
            Self::UnsupportedOption(option) => {
                write!(f, "option '{option}' is not supported yet")
            }
        }
    }
}

impl std::error::Error for UsageError {}

/// コマンド実行の共通エラー。ハンドラの失敗は panic させずこの型で `main` まで伝搬する。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum CommandError {
    /// 引数の誤り。
    Usage(UsageError),
    /// サーバー起動系の失敗（従来の `error: ...` 文言を維持するため委譲する）。
    Startup(StartupError),
    /// `navigate` の失敗。
    Navigate(navigate::NavigateError),
    /// `query` の失敗。
    Query(query::QueryError),
    /// プロファイル指定（`--profile-dir`）の失敗。
    Profile(profile::ProfileOptionError),
    /// `screenshot` の失敗。
    Screenshot(screenshot::ScreenshotError),
    /// `setup` の失敗。
    Setup(setup::SetupError),
    /// 標準出力への書き込み失敗。
    Output(io::Error),
}

impl CommandError {
    /// プロセス終了コード。usage は 2、それ以外は 1。
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Usage(_) => ExitCode::from(USAGE_EXIT_CODE),
            Self::Profile(e) if e.is_validation() => ExitCode::from(USAGE_EXIT_CODE),
            _ => ExitCode::FAILURE,
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(e) => write!(f, "{e}; run 'fandhe-browser --help' for usage"),
            Self::Startup(e) => write!(f, "{e}"),
            Self::Navigate(e) => write!(f, "{e}"),
            Self::Query(e) => write!(f, "{e}"),
            Self::Profile(e) => write!(f, "{e}"),
            Self::Screenshot(e) => write!(f, "{e}"),
            Self::Setup(e) => write!(f, "{e}"),
            Self::Output(_) => f.write_str("failed to write output"),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Usage(e) => Some(e),
            Self::Startup(e) => Some(e),
            Self::Navigate(e) => Some(e),
            Self::Query(e) => Some(e),
            Self::Profile(e) => Some(e),
            Self::Screenshot(e) => Some(e),
            Self::Setup(e) => Some(e),
            Self::Output(e) => Some(e),
        }
    }
}

impl From<UsageError> for CommandError {
    fn from(e: UsageError) -> Self {
        Self::Usage(e)
    }
}

impl From<StartupError> for CommandError {
    fn from(e: StartupError) -> Self {
        Self::Startup(e)
    }
}

impl From<navigate::NavigateError> for CommandError {
    fn from(e: navigate::NavigateError) -> Self {
        Self::Navigate(e)
    }
}

impl From<query::QueryError> for CommandError {
    fn from(e: query::QueryError) -> Self {
        Self::Query(e)
    }
}

impl From<profile::ProfileOptionError> for CommandError {
    fn from(e: profile::ProfileOptionError) -> Self {
        Self::Profile(e)
    }
}

impl From<screenshot::ScreenshotError> for CommandError {
    fn from(e: screenshot::ScreenshotError) -> Self {
        Self::Screenshot(e)
    }
}

impl From<setup::SetupError> for CommandError {
    fn from(e: setup::SetupError) -> Self {
        Self::Setup(e)
    }
}

/// 引数列（プログラム名を除く）を解析する。副作用なし。
fn parse<'a>(
    args: impl IntoIterator<Item = OsString>,
    registry: &Registry<'a>,
) -> Result<Invocation<'a>, UsageError> {
    let mut texts = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        texts.push(
            arg.into_string()
                .map_err(|_| UsageError::NonUtf8Argument { index })?,
        );
    }
    let mut iter = texts.into_iter();
    let Some(first) = iter.next() else {
        return Ok(Invocation::Default);
    };
    if matches!(first.as_str(), "-h" | "--help" | "help") {
        return Ok(Invocation::Help);
    }
    if first.starts_with('-') {
        return Err(UsageError::UnknownOption(first));
    }
    match registry.commands.iter().find(|c| c.name == first) {
        Some(spec) => Ok(Invocation::Command {
            spec,
            args: CommandArgs {
                rest: iter.collect(),
            },
        }),
        None => Err(UsageError::UnknownCommand(first)),
    }
}

/// help を書き出す。
fn write_help(out: &mut impl Write, registry: &Registry<'_>) -> io::Result<()> {
    writeln!(out, "Usage: fandhe-browser [COMMAND] [ARGS]...")?;
    writeln!(out)?;
    writeln!(out, "With no command, behaves like 'launch'.")?;
    writeln!(out)?;
    writeln!(out, "Commands:")?;
    for c in registry.commands {
        writeln!(out, "  {:<12} {}", c.name, c.summary)?;
    }
    writeln!(out, "  {:<12} Show this help", "help")?;
    writeln!(out)?;
    writeln!(out, "Options (all commands, after the command name):")?;
    writeln!(
        out,
        "  {:<18} Use PATH as the profile storage directory instead of the OS default",
        "--profile-dir PATH"
    )?;
    writeln!(out)?;
    writeln!(out, "Launch options:")?;
    writeln!(
        out,
        "  {:<18} Listen on 127.0.0.1:PORT (default 9333; 0 lets the OS choose)",
        "--port PORT"
    )?;
    writeln!(out)?;
    writeln!(out, "Screenshot options:")?;
    writeln!(
        out,
        "  {:<18} Page load limit in seconds, 1-60 (default 30; screenshot step is fixed)",
        "--timeout SECONDS"
    )?;
    writeln!(out)?;
    writeln!(out, "Query options:")?;
    writeln!(
        out,
        "  {:<18} Read HTML from a local file instead of fetching a URL",
        "--file PATH"
    )
}

/// 引数列を解析して該当ハンドラを実行する。`main` から呼ばれる唯一の入口。
///
/// 失敗は [`CommandError`] で返し、終了コードへの写像は呼び出し元が [`CommandError::exit_code`] で行う。
pub(crate) fn dispatch(
    args: impl IntoIterator<Item = OsString>,
    registry: &Registry<'_>,
    out: &mut impl Write,
) -> Result<CommandOutcome, CommandError> {
    match parse(args, registry)? {
        Invocation::Default => (registry.default)(),
        Invocation::Help => {
            write_help(out, registry).map_err(CommandError::Output)?;
            Ok(CommandOutcome::HelpShown)
        }
        Invocation::Command { spec, args } => (spec.run)(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn default_ok() -> Result<CommandOutcome, CommandError> {
        Ok(CommandOutcome::Completed)
    }

    fn echo(args: CommandArgs) -> Result<CommandOutcome, CommandError> {
        assert_eq!(args.as_slice(), ["a".to_string(), "--b".to_string()]);
        Err(CommandError::Startup(StartupError::NonLoopbackAddr))
    }

    const TEST_COMMANDS: &[CommandSpec] = &[CommandSpec {
        name: "demo",
        summary: "Demo command",
        run: echo,
    }];

    fn registry() -> Registry<'static> {
        Registry {
            default: default_ok,
            commands: TEST_COMMANDS,
        }
    }

    #[test]
    fn cli1_empty_args_dispatch_to_default_handler() {
        let mut out = Vec::new();
        let r = dispatch(os(&[]), &registry(), &mut out).expect("default handler succeeds");
        assert_eq!(r, CommandOutcome::Completed);
        assert!(out.is_empty());
    }

    #[test]
    fn cli1_registered_command_receives_remaining_args() {
        let mut out = Vec::new();
        let err = dispatch(os(&["demo", "a", "--b"]), &registry(), &mut out)
            .expect_err("handler error propagates");
        assert!(matches!(
            err,
            CommandError::Startup(StartupError::NonLoopbackAddr)
        ));
    }

    #[test]
    fn cli1_handler_error_propagates_without_panic() {
        let err = dispatch(os(&["demo", "a", "--b"]), &registry(), &mut Vec::new())
            .expect_err("error expected");
        assert_eq!(err.to_string(), StartupError::NonLoopbackAddr.to_string());
        assert_eq!(
            format!("{:?}", err.exit_code()),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    #[test]
    fn cli1_unknown_command_is_usage_error() {
        let err = dispatch(os(&["nope"]), &registry(), &mut Vec::new()).expect_err("usage error");
        assert!(matches!(
            err,
            CommandError::Usage(UsageError::UnknownCommand(ref n)) if n == "nope"
        ));
        assert_eq!(
            err.to_string(),
            "unknown command 'nope'; run 'fandhe-browser --help' for usage"
        );
        assert_eq!(
            format!("{:?}", err.exit_code()),
            format!("{:?}", ExitCode::from(2))
        );
    }

    #[test]
    fn cli1_unknown_option_is_usage_error() {
        let err = dispatch(os(&["--bogus"]), &registry(), &mut Vec::new()).expect_err("usage");
        assert!(matches!(
            err,
            CommandError::Usage(UsageError::UnknownOption(ref n)) if n == "--bogus"
        ));
    }

    #[test]
    fn cli1_help_lists_registered_commands() {
        for flag in ["--help", "-h", "help"] {
            let mut out = Vec::new();
            let r = dispatch(os(&[flag]), &registry(), &mut out).expect("help succeeds");
            assert_eq!(r, CommandOutcome::HelpShown);
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.starts_with("Usage: fandhe-browser"), "{text}");
            assert!(text.contains("demo"), "{text}");
            assert!(text.contains("Demo command"), "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn cli1_non_utf8_argument_is_usage_error() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![OsString::from("demo"), OsString::from_vec(vec![0xff])];
        let err = dispatch(args, &registry(), &mut Vec::new()).expect_err("usage");
        assert!(matches!(
            err,
            CommandError::Usage(UsageError::NonUtf8Argument { index: 1 })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn cli1_non_utf8_argument_is_usage_error() {
        use std::os::windows::ffi::OsStringExt;
        let args = vec![OsString::from("demo"), OsString::from_wide(&[0xD800])];
        let err = dispatch(args, &registry(), &mut Vec::new()).expect_err("usage");
        assert!(matches!(
            err,
            CommandError::Usage(UsageError::NonUtf8Argument { index: 1 })
        ));
    }

    #[test]
    fn cli1_help_lists_launch() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "launch       Start the CDP/AI API server with the default or specified profile"
            ),
            "{text}"
        );
    }

    #[test]
    fn cli1_help_lists_navigate() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("navigate     Fetch a URL once and print the final URL and status"),
            "{text}"
        );
    }

    #[test]
    fn cli1_help_lists_query() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "query        Fetch a URL once and print text or an attribute of elements matching a CSS selector"
            ),
            "{text}"
        );
    }

    #[test]
    fn cli1_help_lists_query_file_option() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "Query options:\n  --file PATH        Read HTML from a local file instead of fetching a URL"
            ),
            "{text}"
        );
    }

    #[test]
    fn cli1_help_lists_screenshot() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("screenshot"), "{text}");
        assert!(
            text.contains("Render a URL once and save a PNG screenshot to a file"),
            "{text}"
        );
    }

    #[test]
    fn cli1_help_lists_screenshot_timeout_option() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "Screenshot options:\n  --timeout SECONDS  Page load limit in seconds, 1-60 (default 30; screenshot step is fixed)"
            ),
            "{text}"
        );
    }

    #[test]
    fn cli1_help_lists_setup() {
        let registry = Registry {
            default: default_ok,
            commands: COMMANDS,
        };
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry, &mut out).expect("help succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "setup        Check OS-specific prerequisites for building the rendering host"
            ),
            "{text}"
        );
    }

    #[test]
    fn cli1_profile_error_exit_codes() {
        use fandhe_browser_profile::ProfileError;
        use profile::ProfileOptionError;
        let invalid = CommandError::from(ProfileOptionError::InvalidPath { reason: "x" });
        assert_eq!(
            format!("{:?}", invalid.exit_code()),
            format!("{:?}", ExitCode::from(2))
        );
        assert_eq!(invalid.to_string(), "invalid profile directory: x");
        let open = CommandError::from(ProfileOptionError::Open(ProfileError::Unsupported {
            reason: "x",
        }));
        assert_eq!(
            format!("{:?}", open.exit_code()),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    #[test]
    fn cli1_help_lists_profile_dir_option() {
        let mut out = Vec::new();
        dispatch(os(&["--help"]), &registry(), &mut out).expect("help");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("--profile-dir PATH"), "{text}");
    }

    #[test]
    fn cli1_conflicting_and_invalid_option_display() {
        let c = UsageError::ConflictingOptions {
            first: "--all",
            second: "--attr",
        };
        assert_eq!(
            c.to_string(),
            "options '--all' and '--attr' cannot be used together"
        );
        let i = UsageError::InvalidOptionValue { option: "--attr" };
        assert_eq!(i.to_string(), "invalid value for option '--attr'");
        let u = UsageError::UnsupportedOption("--full-page");
        assert_eq!(u.to_string(), "option '--full-page' is not supported yet");
    }

    #[test]
    fn cli1_unexpected_argument_display_is_escaped_and_truncated() {
        let long = format!("\x1b[31m{}", "x".repeat(200));
        let shown = UsageError::UnexpectedArgument(long).to_string();
        assert!(
            shown.starts_with("unexpected argument '\\u{1b}[31m"),
            "{shown}"
        );
        assert!(shown.ends_with("...'"), "{shown}");
    }

    #[test]
    fn cli1_unknown_command_display_is_escaped_and_truncated() {
        let long = format!("\x1b[31m{}", "x".repeat(200));
        let shown = UsageError::UnknownCommand(long).to_string();
        assert!(shown.starts_with("unknown command '\\u{1b}[31m"), "{shown}");
        assert!(shown.ends_with("...'"), "{shown}");
        assert!(shown.len() < 140, "{}", shown.len());
    }
}
