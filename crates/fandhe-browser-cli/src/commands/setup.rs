//! セットアップ専用コマンド `setup`（TASK-48.1・Issue #290・`CLI-2`・MS-5）。
//!
//! `setup` は OS 固有のビルド前提（Windows の SDK・MSVC v143・ATL・Python・`uv`）を
//! [`crate::setup`] で検出して 1 項目 1 行で報告する。`CLI-2` のとおり、この検出は `setup` を
//! 明示したときだけ走り、launch・navigate・query・screenshot・`--profile-dir` の経路からは呼ばない。
//! 引数は取らない（`--profile-dir` も無意味なため黙って受け入れず usage エラーにする。REPAIR-3）。
//! 不足が 1 件でもあれば [`SetupError::MissingPrerequisites`] で終了コード 1。
//! `cfg(target_os)` 分岐はここには置かない（`crate::setup` に局所化。TASK-48.2・#291 が検証する）。

use std::fmt;
use std::io::{self, Write};

use super::{CommandArgs, CommandError, CommandOutcome, CommandSpec, UsageError};
use crate::setup::{PrerequisiteId, PrerequisiteStatus, SetupReport, detect_current};

/// 登録表（[`super::COMMANDS`]）へ載せる仕様。
pub(crate) const SPEC: CommandSpec = CommandSpec {
    name: "setup",
    summary: "Check OS-specific prerequisites for building the rendering host (Windows SDK, MSVC v143, ATL, Python, uv)",
    run,
};

/// `setup` の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum SetupError {
    /// 不足している前提がある。
    MissingPrerequisites { missing: Vec<PrerequisiteId> },
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrerequisites { missing } => {
                let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
                write!(
                    f,
                    "missing OS-specific build prerequisites: {}; see docs/setup/windows.md",
                    names.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for SetupError {}

fn run(args: CommandArgs) -> Result<CommandOutcome, CommandError> {
    run_with(&args, detect_current(), &mut io::stdout().lock())
}

/// 検出結果と出力先を注入できる本体（単体テスト用に分離）。
pub(crate) fn run_with(
    args: &CommandArgs,
    report: SetupReport,
    out: &mut impl Write,
) -> Result<CommandOutcome, CommandError> {
    if let Some(arg) = args.as_slice().first() {
        let usage = if arg.starts_with('-') {
            UsageError::UnknownOption(arg.clone())
        } else {
            UsageError::UnexpectedArgument(arg.clone())
        };
        return Err(usage.into());
    }
    if report.checks.is_empty() {
        writeln!(
            out,
            "no OS-specific prerequisites are checked on {}",
            report.os
        )
        .map_err(CommandError::Output)?;
        return Ok(CommandOutcome::Completed);
    }
    for c in &report.checks {
        match &c.status {
            PrerequisiteStatus::Satisfied { found } => writeln!(out, "ok      {}: {found}", c.id),
            PrerequisiteStatus::Missing { expected } => {
                writeln!(out, "missing {}: expected {expected}; {}", c.id, c.hint)
            }
            PrerequisiteStatus::NotChecked { reason } => {
                writeln!(out, "skipped {}: {reason}", c.id)
            }
        }
        .map_err(CommandError::Output)?;
    }
    let missing: Vec<PrerequisiteId> = report.missing().collect();
    if missing.is_empty() {
        Ok(CommandOutcome::Completed)
    } else {
        Err(SetupError::MissingPrerequisites { missing }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::windows_report;
    use std::process::ExitCode;

    fn args(a: &[&str]) -> CommandArgs {
        CommandArgs::new(a.iter().map(|s| s.to_string()).collect())
    }

    fn empty_report() -> SetupReport {
        SetupReport {
            os: "linux",
            checks: vec![],
        }
    }

    #[test]
    fn cli2_empty_report_prints_not_checked_line() {
        let mut out = Vec::new();
        let r = run_with(&args(&[]), empty_report(), &mut out).expect("ok");
        assert_eq!(r, CommandOutcome::Completed);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "no OS-specific prerequisites are checked on linux\n"
        );
    }

    #[test]
    fn cli2_missing_prerequisites_error_and_exit_code() {
        let mut out = Vec::new();
        let err = run_with(&args(&[]), windows_report(None, None, &[], &[]), &mut out)
            .expect_err("missing");
        assert_eq!(
            err.to_string(),
            "missing OS-specific build prerequisites: windows-sdk, msvc-v143, python, uv; see docs/setup/windows.md"
        );
        assert_eq!(
            format!("{:?}", err.exit_code()),
            format!("{:?}", ExitCode::FAILURE)
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.starts_with("missing windows-sdk: expected Windows SDK 10.0.19041.0 or newer; "),
            "{text}"
        );
        assert!(
            text.contains("skipped atl-v143: MSVC v143 toolset not found\n"),
            "{text}"
        );
    }

    #[test]
    fn cli2_extra_arguments_are_usage_errors() {
        let e = run_with(&args(&["extra"]), empty_report(), &mut Vec::new()).expect_err("usage");
        assert_eq!(
            e.to_string(),
            "unexpected argument 'extra'; run 'fandhe-browser --help' for usage"
        );
        let e = run_with(
            &args(&["--profile-dir", "x"]),
            empty_report(),
            &mut Vec::new(),
        )
        .expect_err("usage");
        assert_eq!(
            format!("{:?}", e.exit_code()),
            format!("{:?}", ExitCode::from(2))
        );
    }

    /// 受入基準 2: 日常操作コマンドのソースが検出処理を参照しない（`CLI-2`）。
    #[test]
    fn cli2_daily_command_sources_do_not_reference_setup_detection() {
        let sources = [
            ("launch", include_str!("launch.rs")),
            ("navigate", include_str!("navigate.rs")),
            ("query", include_str!("query.rs")),
            ("screenshot", include_str!("screenshot.rs")),
            ("profile", include_str!("profile.rs")),
            ("server", include_str!("../server.rs")),
            ("render_host", include_str!("../render_host.rs")),
            ("startup_config", include_str!("../startup_config.rs")),
            ("page_scripts", include_str!("../page_scripts.rs")),
        ];
        for (name, src) in sources {
            assert!(!src.contains("crate::setup"), "{name} references setup");
            assert!(!src.contains("detect_current"), "{name} calls detection");
        }
    }
}
