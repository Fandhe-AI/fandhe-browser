//! セットアップ専用コマンド `setup`（TASK-48.1・Issue #290・`CLI-2`・MS-5）。
//!
//! `setup` は OS 固有のビルド前提（Windows の SDK・MSVC v143・ATL・Python・`uv`）を
//! [`crate::setup`] で検出して 1 項目 1 行で報告する。`CLI-2` のとおり、この検出は `setup` を
//! 明示したときだけ走り、launch・navigate・query・screenshot・`--profile-dir` の経路からは呼ばない。
//! 引数は取らない（`--profile-dir` も無意味なため黙って受け入れず usage エラーにする。REPAIR-3）。
//! 不足が 1 件でもあれば [`SetupError::MissingPrerequisites`] で終了コード 1。
//! `cfg(target_os)` 分岐はここには置かない（`crate::setup` に局所化。検証は `cli2_daily_command_sources_have_no_os_specific_branches`。TASK-48.2・#291）。

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

    /// 日常操作コマンドのソース 1 本から OS 固有分岐の疑いがある行を集める（`CLI-2`・TASK-48.2）。
    ///
    /// `#[cfg(test)]` の直後に `mod tests {` が来る境界より前（本番コード）だけを行単位で調べ、
    /// 境界が無ければ全体を調べる。`mod tests` がファイル末尾の最後の item でない場合は
    /// 本番コードが検査から漏れるため、その旨も違反として返す（fail-closed）。
    fn os_specific_branch_lines(name: &str, src: &str) -> Vec<String> {
        let lines: Vec<&str> = src.lines().collect();
        let boundary = lines.windows(2).position(|w| {
            w[0].trim() == "#[cfg(test)]" && w[1].trim_start().starts_with("mod tests {")
        });
        let mut found = Vec::new();
        let production = match boundary {
            Some(b) => {
                let tail = lines.get(b + 2..).unwrap_or(&[]);
                let closers: Vec<usize> = tail
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| **l == "}")
                    .map(|(i, _)| i)
                    .collect();
                let last_nonempty = tail.iter().rposition(|l| !l.trim().is_empty());
                if closers.len() != 1 || closers.first().copied() != last_nonempty {
                    found.push(format!("{name}: tests module is not the last item"));
                }
                lines.get(..b).unwrap_or(&[])
            }
            None => &lines[..],
        };
        for (i, raw) in production.iter().enumerate() {
            let line = raw.trim();
            if line.starts_with("//") {
                continue;
            }
            if line_has_os_branch(line) {
                found.push(format!("{name}:{}: {line}", i + 1));
            }
        }
        found
    }

    /// 1 行（trim 済み・コメント行除外済み）が OS 固有分岐を含むか。`cfg(test)` だけは許可する。
    fn line_has_os_branch(line: &str) -> bool {
        for pat in ["cfg(", "cfg!(", "cfg_attr("] {
            for (idx, _) in line.match_indices(pat) {
                let rest = &line[idx + pat.len()..];
                let inner = rest.split(')').next().unwrap_or("").trim();
                if inner != "test" {
                    return true;
                }
            }
        }
        [
            "target_os",
            "target_family",
            "target_vendor",
            "target_env",
            "std::os::",
            "consts::OS",
            "consts::FAMILY",
        ]
        .iter()
        .any(|p| line.contains(p))
    }

    /// 受入基準 1・2: 日常操作コマンド（launch・navigate・query・screenshot・profile）と
    /// 共通ディスパッチ（`mod.rs`）の本番コードに OS 固有分岐が無い（`CLI-2`・TASK-48.2・#291・MS-5）。
    /// `server.rs`（本番コードに `cfg(windows)` を持つ既定サーバー）と setup 系（OS 分岐を置く場所）は
    /// 日常操作コマンドではないため対象外。テストモジュールは挙動ではないので除外する。
    #[test]
    fn cli2_daily_command_sources_have_no_os_specific_branches() {
        let sources = [
            ("launch", include_str!("launch.rs")),
            ("navigate", include_str!("navigate.rs")),
            ("query", include_str!("query.rs")),
            ("screenshot", include_str!("screenshot.rs")),
            ("profile", include_str!("profile.rs")),
            ("dispatch", include_str!("mod.rs")),
        ];
        let mut violations = Vec::new();
        for (name, src) in sources {
            assert!(src.contains("fn "), "{name} source looks empty");
            violations.extend(os_specific_branch_lines(name, src));
        }
        assert_eq!(violations, Vec::<String>::new());
    }

    /// 検査ヘルパ自体の陰性テスト（検出すべきものを検出し、許可すべきものを通す）。
    #[test]
    fn cli2_os_branch_checker_reports_violations_with_location() {
        let bad = "fn a() {}\n\n#[cfg(target_os = \"windows\")]\nfn b() {}\n";
        assert_eq!(
            os_specific_branch_lines("fixture", bad),
            vec!["fixture:3: #[cfg(target_os = \"windows\")]".to_string()]
        );
        for line in [
            "if cfg!(windows) {}",
            "#[cfg_attr(unix, allow(dead_code))]",
            "use std::os::unix::fs::symlink;",
            "let o = std::env::consts::OS;",
        ] {
            assert!(line_has_os_branch(line), "{line}");
        }
        let ok = "//! cfg(target_os) 分岐は置かない\n#[cfg(test)]\nfn new() {}\nfn x() {}\n\n#[cfg(test)]\nmod tests {\n    #[cfg(unix)]\n    fn t() {}\n}\n";
        assert_eq!(
            os_specific_branch_lines("fixture", ok),
            Vec::<String>::new()
        );
        let broken = "fn a() {}\n#[cfg(test)]\nmod tests {\n}\nfn late() {}\n";
        assert_eq!(
            os_specific_branch_lines("fixture", broken),
            vec!["fixture: tests module is not the last item".to_string()]
        );
    }
}
