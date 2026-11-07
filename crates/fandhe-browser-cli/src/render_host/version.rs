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
//! - 隔離の外で動かすのはこの `--version` だけ。ページ読み込み経路（`HostProcess` の `--stdio`）は変えない。
//! - ホストの出力は untrusted。文法を通った版トークンだけを表示し、それ以外の生出力は一切出さない。

use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fandhe_browser_render::host_process::{HOST_BINARY_STEM, HostBinary, PASSTHROUGH_ENV_NAMES};

/// `--version` の完了を待つ上限。大きな未署名バイナリの初回起動を見込み、egress 判定の 5 秒より長くする。
pub(crate) const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// `--version` の stdout の上限バイト数。
pub(crate) const VERSION_STDOUT_MAX: u64 = 256;
/// 版トークンの最大バイト数。
const VERSION_TOKEN_MAX: usize = 64;
/// 終了待ちのポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 版の検査の失敗。表示は固定の英語で、パス・URL・ホストの生出力を含めない（`Mismatch` の版は文法検証済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum HostVersionError {
    /// 版が CLI と異なる。`host` は文法を通った版トークン。
    Mismatch { host: String },
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

/// 解決済みホストの版を CLI と比べる本番の入口。
pub(crate) fn check_host_version(binary: &HostBinary) -> Result<(), HostVersionError> {
    check_host_version_with(
        binary.path(),
        VERSION_TIMEOUT,
        VERSION_STDOUT_MAX,
        env!("CARGO_PKG_VERSION"),
    )
}

fn check_host_version_with(
    binary: &Path,
    timeout: Duration,
    stdout_max: u64,
    expected: &str,
) -> Result<(), HostVersionError> {
    let out = read_host_version_output(binary, timeout, stdout_max)?;
    let host = parse_host_version(&out)?;
    if host == expected {
        Ok(())
    } else {
        Err(HostVersionError::Mismatch {
            host: host.to_owned(),
        })
    }
}

/// 子プロセスの cwd に使える形へパスを整える。
///
/// Windows の `canonicalize` は verbatim 形式（`\\?\C:\...`）を返すが、`CreateProcessW` の
/// `lpCurrentDirectory` はこれを受け付けず spawn が失敗する。ドライブ形式の verbatim 接頭辞だけを
/// 外して通常のパスにする。UNC の verbatim（`\\?\UNC\...`）など安全に変換できない形は
/// `None`（cwd を設定しない）にする。Windows 以外では通常のパスをそのまま返す。
fn child_cwd(dir: &Path) -> Option<std::path::PathBuf> {
    let Some(text) = dir.to_str() else {
        return Some(dir.to_path_buf());
    };
    let rest = text
        .strip_prefix("\\\\?\\")
        .or_else(|| text.strip_prefix("//?/"));
    let Some(rest) = rest else {
        return Some(dir.to_path_buf());
    };
    let b = rest.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(std::path::PathBuf::from(rest))
    } else {
        None
    }
}

/// `--version` を実行して stdout を返す。
///
/// シェルなし・引数固定。環境は `env_clear` のうえ render の引き継ぎ許可リストだけを渡し、cwd は
/// バイナリの親ディレクトリにする（利用者の cwd で実行しない）。stdout は別スレッドで上限付きに読む。
/// 終了後にまとめて読む方式だと、孫プロセスが stdout を握り続けたとき時間の上限が破れるため。
/// 読み取りスレッドは孫が握る場合に限りプロセス終了まで残り得るが、起動時 1 回で害はない。
fn read_host_version_output(
    binary: &Path,
    timeout: Duration,
    stdout_max: u64,
) -> Result<Vec<u8>, HostVersionError> {
    let mut cmd = Command::new(binary);
    cmd.arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in PASSTHROUGH_ENV_NAMES {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    if let Some(dir) = binary.parent().and_then(child_cwd) {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| HostVersionError::SpawnFailed(e.kind()))?;
    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err(HostVersionError::ReadFailed);
    };
    let (tx, rx) = mpsc::channel::<io::Result<Vec<u8>>>();
    let spawned = std::thread::Builder::new()
        .name("host-version-read".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let r = stdout
                .take(stdout_max.saturating_add(1))
                .read_to_end(&mut buf)
                .map(|_| buf);
            let _ = tx.send(r);
        });
    if spawned.is_err() {
        kill_and_reap(&mut child);
        return Err(HostVersionError::ReadFailed);
    }
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        kill_and_reap(&mut child);
        return Err(HostVersionError::Timeout);
    };
    let out = match rx.recv_timeout(timeout) {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            kill_and_reap(&mut child);
            return Err(HostVersionError::ReadFailed);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_and_reap(&mut child);
            return Err(HostVersionError::Timeout);
        }
    };
    if u64::try_from(out.len()).map_or(true, |n| n > stdout_max) {
        kill_and_reap(&mut child);
        return Err(HostVersionError::OutputTooLarge);
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(out)
                } else {
                    Err(HostVersionError::ExitFailure)
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(&mut child);
                return Err(HostVersionError::Timeout);
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => {
                kill_and_reap(&mut child);
                return Err(HostVersionError::ReadFailed);
            }
        }
    }
}

fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
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

    const CLI: &str = env!("CARGO_PKG_VERSION");

    /// `RENDER-3`・#1540: verbatim 接頭辞は外し、UNC verbatim は cwd 指定なしにし、通常パスは保つ。
    #[test]
    fn render3_child_cwd_strips_verbatim_drive_prefix() {
        use std::path::{Path, PathBuf};
        assert_eq!(
            child_cwd(Path::new("\\\\?\\C:\\host\\bin")),
            Some(PathBuf::from("C:\\host\\bin"))
        );
        assert_eq!(
            child_cwd(Path::new("//?/C:/host/bin")),
            Some(PathBuf::from("C:/host/bin"))
        );
        assert_eq!(child_cwd(Path::new("\\\\?\\UNC\\srv\\share")), None);
        assert_eq!(
            child_cwd(Path::new("/usr/local/bin")),
            Some(PathBuf::from("/usr/local/bin"))
        );
    }

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

    /// `RENDER-3`・#1540: 存在しないパスは起動失敗（3 OS 共通の実プロセス経路）。
    #[test]
    fn render3_missing_binary_is_spawn_failed() {
        let missing = std::env::temp_dir().join("fandhe-no-such-host-1540");
        let err = check_host_version_with(&missing, Duration::from_secs(2), 256, CLI).unwrap_err();
        assert!(matches!(err, HostVersionError::SpawnFailed(_)), "{err:?}");
    }

    /// `RENDER-3`・#1540: `--version` を理解しない実バイナリ（テストバイナリ自身）は失敗として扱う。
    #[test]
    fn render3_binary_that_rejects_version_flag_is_failure() {
        let exe = std::env::current_exe().expect("current exe");
        let err = check_host_version_with(&exe, Duration::from_secs(10), 256, CLI).unwrap_err();
        assert!(
            matches!(
                err,
                HostVersionError::ExitFailure | HostVersionError::UnexpectedOutput
            ),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

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
            for _ in 0..20 {
                match check_host_version_with(path, timeout, max, CLI) {
                    Err(HostVersionError::SpawnFailed(io::ErrorKind::ExecutableFileBusy)) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    other => return other,
                }
            }
            check_host_version_with(path, timeout, max, CLI)
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
            assert!(!text.contains("HOME="), "{text}");
        }
    }
}
