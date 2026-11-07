//! CLI と Servo ホストの版の一致検査（`RENDER-3`・`SEC-2`・`REPAIR-4`・TASK-38・#1540）。
//!
//! [`super::select_renderer_checked`] が、ホストの解決・検証と egress 隔離の判定が済んだ後に 1 回だけ呼ぶ。
//! ホストの `--version`（`servo-host/src/main.rs`。Servo を初期化せずネットワークにも出ない）を
//! 上限付きで実行し、CLI 自身の版（`CARGO_PKG_VERSION`）と完全一致するか比べる。
//! crates.io では CLI とホストを別々に `cargo install` するため、版のずれを描画の前に検出するための検査。
//!
//! # 位置づけ
//!
//! - 利便のための整合確認であり、信頼の境界ではない（`--version` は自己申告）。バイナリ自体の検証は
//!   `resolve_host_binary` が担う。
//! - `--version` は `HostProcess` と同じ OS サンドボックスの下で実行する（`required` で隔離できなければ実行しない。
//!   プロキシなし・送信先なし・cwd は専用 WorkDir。#1553）。ページ読み込み経路（`--stdio`）は変えない。
//! - ホストの出力は untrusted。文法を通った版トークンだけを表示し、それ以外の生出力は一切出さない。

use fandhe_browser_render::host_process::{
    EgressIsolationUnavailable, HOST_BINARY_STEM, HostBinary, HostVersionRunConfig,
    HostVersionRunError, run_host_version,
};
use std::fmt;
use std::io;

/// 版トークンの最大バイト数。
const VERSION_TOKEN_MAX: usize = 64;

/// 版の検査の失敗。表示は固定の英語で、パス・URL・ホストの生出力を含めない（`Mismatch` の版は文法検証済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum HostVersionError {
    /// 版が CLI と異なる。`host` は文法を通った版トークン。
    Mismatch { host: String },
    /// `required` で egress 隔離できず、ホストを実行していない（`StartupError::RenderIsolation` へ写す）。
    IsolationUnavailable(EgressIsolationUnavailable),
    /// 専用の作業ディレクトリを用意できなかった。
    WorkDirFailed,
    /// `--version` を起動できなかった。
    SpawnFailed(io::ErrorKind),
    /// 上限時間内に終わらなかった。
    Timeout,
    /// 非 0 で終了した。
    ExitFailure,
    /// 出力を読めなかった。
    ReadFailed,
    /// 出力が上限を超えた。
    OutputTooLarge,
    /// 出力が `fandhe-browser-servo-host <version>` の形式でなかった。
    UnexpectedOutput,
}

/// 揃え方の案内。`commands/screenshot.rs` の `hello` 前終了の文言と語彙を揃える。
fn guidance(cli: &str) -> String {
    format!(
        "Install the same version: on macOS/Linux run 'cargo install fandhe-browser-servo-host --version {cli} --locked --force'; if FANDHE_BROWSER_RENDER_HOST is set, point it at the new binary"
    )
}

impl HostVersionError {
    fn reason(&self) -> &'static str {
        match self {
            Self::IsolationUnavailable(_) => "egress isolation is unavailable",
            Self::WorkDirFailed => "failed to prepare its working directory",
            Self::SpawnFailed(_) => "failed to start",
            Self::Timeout => "timed out",
            Self::ExitFailure => "exited with an error",
            Self::ReadFailed => "failed to read its output",
            Self::OutputTooLarge => "output too large",
            Self::UnexpectedOutput | Self::Mismatch { .. } => "unexpected output",
        }
    }
}

impl fmt::Display for HostVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cli = env!("CARGO_PKG_VERSION");
        match self {
            Self::Mismatch { host } => write!(
                f,
                "render host version mismatch: this CLI is {cli} but the render host is {host}. {}",
                guidance(cli)
            ),
            other => write!(
                f,
                "could not verify the render host version ({}); this CLI is {cli}. {}",
                other.reason(),
                guidance(cli)
            ),
        }
    }
}

impl std::error::Error for HostVersionError {}

impl From<HostVersionRunError> for HostVersionError {
    fn from(e: HostVersionRunError) -> Self {
        match e {
            HostVersionRunError::IsolationUnavailable(r) => Self::IsolationUnavailable(r),
            HostVersionRunError::WorkDirFailed => Self::WorkDirFailed,
            HostVersionRunError::SpawnFailed(k) => Self::SpawnFailed(k),
            HostVersionRunError::Timeout => Self::Timeout,
            HostVersionRunError::ExitFailure => Self::ExitFailure,
            HostVersionRunError::ReadFailed => Self::ReadFailed,
            HostVersionRunError::OutputTooLarge => Self::OutputTooLarge,
            // 将来のバリアントは検証不能として扱う（fail-closed）。
            _ => Self::ReadFailed,
        }
    }
}

/// 解決済みホストの版を CLI と比べる本番の入口。
///
/// `--version` は render の `run_host_version` が `HostProcess` と同じ egress 隔離の下で実行する
/// （プロキシなし・送信先なし。#1553）。`config` は隔離の水準と、`best-effort` で隔離なしに落ちたときの
/// 警告の通知先を持つ（`render_host` が `HostProcess` と共有する通知先を渡す）。
pub(crate) fn check_host_version(
    binary: &HostBinary,
    config: &HostVersionRunConfig,
) -> Result<(), HostVersionError> {
    check_host_version_with(binary, config, env!("CARGO_PKG_VERSION"))
}

/// [`check_host_version`] の本体。テストが隔離の判定元などを差し込めるよう設定を受ける。
pub(crate) fn check_host_version_with(
    binary: &HostBinary,
    config: &HostVersionRunConfig,
    expected: &str,
) -> Result<(), HostVersionError> {
    let out = run_host_version(binary, config)?;
    let host = parse_host_version(&out.stdout)?;
    if host == expected {
        Ok(())
    } else {
        Err(HostVersionError::Mismatch {
            host: host.to_owned(),
        })
    }
}

/// `fandhe-browser-servo-host <version>\n` を解析して版トークンを返す（純関数）。
///
/// 末尾の `\n` を 1 つだけ外し、残りに改行を許さない（`\r\n` は受けない）。版は先頭が ASCII 数字で、
/// `[0-9A-Za-z.+-]` のみの 64 バイト以下。表示に使う値はこの文法を通ったものに限る。
fn parse_host_version(stdout: &[u8]) -> Result<&str, HostVersionError> {
    let text = std::str::from_utf8(stdout).map_err(|_| HostVersionError::UnexpectedOutput)?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.contains('\n') || line.contains('\r') {
        return Err(HostVersionError::UnexpectedOutput);
    }
    let token = line
        .strip_prefix(HOST_BINARY_STEM)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or(HostVersionError::UnexpectedOutput)?;
    let valid = !token.is_empty()
        && token.len() <= VERSION_TOKEN_MAX
        && token.starts_with(|c: char| c.is_ascii_digit())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
    if valid {
        Ok(token)
    } else {
        Err(HostVersionError::UnexpectedOutput)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fandhe_browser_core::EgressIsolationMode;
    use std::sync::Arc;

    const CLI: &str = env!("CARGO_PKG_VERSION");

    /// `RENDER-3`・#1540: 正常な出力から版トークンを取り出す。
    #[test]
    fn render3_parse_accepts_well_formed_output() {
        assert_eq!(
            parse_host_version(b"fandhe-browser-servo-host 0.1.0\n"),
            Ok("0.1.0")
        );
        assert_eq!(
            parse_host_version(b"fandhe-browser-servo-host 1.2.3-rc.1+b5"),
            Ok("1.2.3-rc.1+b5")
        );
    }

    /// `RENDER-3`・#1540: 形式違いはすべて `UnexpectedOutput`（生出力を表示に使わない）。
    #[test]
    fn render3_parse_rejects_malformed_output() {
        let long = format!("fandhe-browser-servo-host 1{}\n", "0".repeat(64));
        let cases: Vec<&[u8]> = vec![
            b"",
            b"\n",
            b"fandhe-browser-servo-host 0.1.0\r\n",
            b"fandhe-browser-servo-host 0.1.0\nextra\n",
            b"other-binary 0.1.0\n",
            b"fandhe-browser-servo-host\n",
            b"fandhe-browser-servo-host \n",
            b"fandhe-browser-servo-host  0.1.0\n",
            b"fandhe-browser-servo-host \xff\xfe\n",
            b"fandhe-browser-servo-host 0.1.0\x1b[31m\n",
            b"fandhe-browser-servo-host v0.1.0\n",
            long.as_bytes(),
        ];
        for c in cases {
            assert_eq!(
                parse_host_version(c),
                Err(HostVersionError::UnexpectedOutput),
                "{c:?}"
            );
        }
    }

    /// `RENDER-3`・#1540: 不一致は両方の版と揃え方を示す。
    #[test]
    fn render3_mismatch_message_names_both_versions_and_remedy() {
        let shown = HostVersionError::Mismatch {
            host: "9.9.9".into(),
        }
        .to_string();
        assert_eq!(
            shown,
            format!(
                "render host version mismatch: this CLI is {CLI} but the render host is 9.9.9. Install the same version: on macOS/Linux run 'cargo install fandhe-browser-servo-host --version {CLI} --locked --force'; if FANDHE_BROWSER_RENDER_HOST is set, point it at the new binary"
            )
        );
    }

    /// `RENDER-3`・#1540: 読めない系の文言は CLI の版と案内を含み、パスや生出力を含まない。
    #[test]
    fn render3_unverifiable_message_has_cli_version_and_no_paths() {
        let cases = [
            (
                HostVersionError::SpawnFailed(io::ErrorKind::NotFound),
                "failed to start",
            ),
            (
                HostVersionError::IsolationUnavailable(
                    EgressIsolationUnavailable::SandboxToolNotFound,
                ),
                "egress isolation is unavailable",
            ),
            (
                HostVersionError::WorkDirFailed,
                "failed to prepare its working directory",
            ),
            (HostVersionError::Timeout, "timed out"),
            (HostVersionError::ExitFailure, "exited with an error"),
            (HostVersionError::ReadFailed, "failed to read its output"),
            (HostVersionError::OutputTooLarge, "output too large"),
            (HostVersionError::UnexpectedOutput, "unexpected output"),
        ];
        for (err, reason) in cases {
            assert_eq!(
                err.to_string(),
                format!(
                    "could not verify the render host version ({reason}); this CLI is {CLI}. Install the same version: on macOS/Linux run 'cargo install fandhe-browser-servo-host --version {CLI} --locked --force'; if FANDHE_BROWSER_RENDER_HOST is set, point it at the new binary"
                )
            );
        }
    }

    /// 隔離なしに固定した設定（実環境の bwrap の有無に左右されない。隔離下の観測は render 側のテストが持つ）。
    fn unisolated(mode: EgressIsolationMode) -> HostVersionRunConfig {
        HostVersionRunConfig::new(mode)
            .with_isolation_probe(
                fandhe_browser_render::host_process::IsolationProbe::Unavailable(
                    EgressIsolationUnavailable::SandboxToolNotFound,
                ),
            )
            .with_timeout(std::time::Duration::from_secs(10))
            .with_isolation_warning(Arc::new(|_| {}))
    }

    fn binary_at(path: &std::path::Path) -> HostBinary {
        fandhe_browser_render::host_process::resolve_host_binary(Some(path), None, None)
            .expect("absolute existing path resolves")
    }

    /// `RENDER-3`・#1540: `--version` を理解しない実バイナリ（テストバイナリ自身）は失敗として扱う。
    #[test]
    fn render3_binary_that_rejects_version_flag_is_failure() {
        let exe = std::env::current_exe().expect("current exe");
        let err = check_host_version_with(
            &binary_at(&exe),
            &unisolated(EgressIsolationMode::BestEffort),
            CLI,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                HostVersionError::ExitFailure | HostVersionError::UnexpectedOutput
            ),
            "{err:?}"
        );
    }

    /// `RENDER-3`・#1553: 隔離できない `required` は検査の失敗（ホスト未実行）として返り、写像は隔離エラー用。
    #[test]
    fn render3_required_without_isolation_does_not_run_host_version() {
        let exe = std::env::current_exe().expect("current exe");
        let err = check_host_version_with(
            &binary_at(&exe),
            &unisolated(EgressIsolationMode::Required),
            CLI,
        )
        .unwrap_err();
        assert_eq!(
            err,
            HostVersionError::IsolationUnavailable(EgressIsolationUnavailable::SandboxToolNotFound)
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::time::{Duration, Instant};

        struct Dir(PathBuf);
        impl Dir {
            fn new(tag: &str) -> Self {
                let p = std::env::temp_dir()
                    .join(format!("fandhe-hostver-{}-{tag}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).expect("dir");
                Self(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn fake_host(dir: &Dir, body: &str) -> PathBuf {
            let path = dir.0.join("fake-host");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        }

        /// 書き込み直後の実行は ETXTBSY になり得るため、その失敗に限ってやり直す。
        fn run(path: &Path, timeout: Duration, max: u64) -> Result<(), HostVersionError> {
            let binary = binary_at(path);
            let config = unisolated(EgressIsolationMode::BestEffort)
                .with_timeout(timeout)
                .with_stdout_max(max);
            for _ in 0..20 {
                match check_host_version_with(&binary, &config, CLI) {
                    Err(HostVersionError::SpawnFailed(io::ErrorKind::ExecutableFileBusy)) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    other => return other,
                }
            }
            check_host_version_with(&binary, &config, CLI)
        }

        const T: Duration = Duration::from_secs(5);

        #[test]
        fn render3_matching_version_passes() {
            let d = Dir::new("ok");
            let h = fake_host(&d, &format!("echo 'fandhe-browser-servo-host {CLI}'"));
            assert_eq!(run(&h, T, 256), Ok(()));
        }

        #[test]
        fn render3_different_version_is_mismatch() {
            let d = Dir::new("mismatch");
            let h = fake_host(&d, "echo 'fandhe-browser-servo-host 99.0.0'");
            assert_eq!(
                run(&h, T, 256),
                Err(HostVersionError::Mismatch {
                    host: "99.0.0".into()
                })
            );
        }

        #[test]
        fn render3_unresponsive_host_times_out() {
            let d = Dir::new("timeout");
            let h = fake_host(&d, "exec sleep 30");
            let start = Instant::now();
            assert_eq!(
                run(&h, Duration::from_millis(200), 256),
                Err(HostVersionError::Timeout)
            );
            assert!(start.elapsed() < Duration::from_secs(5));
        }

        /// 孫プロセスが stdout を握ったままでも時間の上限を守る。
        #[test]
        fn render3_grandchild_holding_stdout_still_times_out() {
            let d = Dir::new("grandchild");
            let h = fake_host(&d, "sleep 5\nexit 0");
            let start = Instant::now();
            assert_eq!(
                run(&h, Duration::from_millis(200), 256),
                Err(HostVersionError::Timeout)
            );
            assert!(start.elapsed() < Duration::from_secs(3));
        }

        #[test]
        fn render3_silent_success_is_unexpected_output() {
            let d = Dir::new("silent");
            let h = fake_host(&d, "exit 0");
            assert_eq!(run(&h, T, 256), Err(HostVersionError::UnexpectedOutput));
        }

        #[test]
        fn render3_nonzero_exit_is_exit_failure() {
            let d = Dir::new("exit1");
            let h = fake_host(&d, "exit 1");
            assert_eq!(run(&h, T, 256), Err(HostVersionError::ExitFailure));
        }

        #[test]
        fn render3_garbage_is_unexpected_output() {
            let d = Dir::new("garbage");
            let h = fake_host(&d, "echo garbage");
            assert_eq!(run(&h, T, 256), Err(HostVersionError::UnexpectedOutput));
        }

        #[test]
        fn render3_oversized_output_is_rejected() {
            let d = Dir::new("big");
            let h = fake_host(&d, "head -c 1024 /dev/zero | tr '\\0' 'a'; echo");
            assert_eq!(run(&h, T, 256), Err(HostVersionError::OutputTooLarge));
        }

        /// 無限に出力するホストでも timeout を待たず上限超過で止める。
        #[test]
        fn render3_endless_output_is_rejected_without_waiting_for_timeout() {
            let d = Dir::new("endless");
            let h = fake_host(&d, "exec yes");
            let start = Instant::now();
            assert_eq!(
                run(&h, Duration::from_secs(20), 256),
                Err(HostVersionError::OutputTooLarge)
            );
            assert!(start.elapsed() < Duration::from_secs(10));
        }

        /// 引数は `--version` のみで、親の環境変数は子へ渡らない。
        #[test]
        fn render3_passes_only_version_arg_and_clean_env() {
            let d = Dir::new("env");
            let log = d.0.join("log");
            let h = fake_host(
                &d,
                &format!(
                    "echo \"$@\" > '{0}'; env >> '{0}'; echo 'fandhe-browser-servo-host {CLI}'",
                    log.display()
                ),
            );
            assert_eq!(run(&h, T, 256), Ok(()));
            let text = std::fs::read_to_string(&log).expect("log");
            assert_eq!(text.lines().next(), Some("--version"));
            assert!(!text.contains("CARGO_MANIFEST_DIR"), "{text}");
            // HOME は親の値ではなく、専用 WorkDir 配下（`fandhe-render-*/home`）を指す（#1553）。
            let parent_home = std::env::var("HOME").unwrap_or_default();
            let home = text
                .lines()
                .find_map(|l| l.strip_prefix("HOME="))
                .expect("HOME is set to the work dir");
            assert_ne!(home, parent_home, "{text}");
            assert!(
                home.contains("fandhe-render-") && home.ends_with("/home"),
                "{home}"
            );
        }
    }
}
