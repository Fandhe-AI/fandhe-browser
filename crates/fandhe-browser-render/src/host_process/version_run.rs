//! ホストの `--version` を egress 隔離の下で 1 回だけ実行する（`RENDER-3`・`SEC-2`・`REPAIR-4`・
//! TASK-38・#1553。設計 `docs/design/servo-host-design.md` §2.6）。
//!
//! cli の版の検査（`render_host/version.rs`・#1540）が呼ぶ。以前は解決したホストを素の `Command` で
//! 隔離の外に実行していたため、`required` でも差し替えられたバイナリが隔離の前に利用者の権限で走り得た。
//! 本モジュールは [`super::HostProcess`] と同じ判定（probe -> `decide`）と同じ包み方
//! （`isolation::linux_version_argv`・`macos_version_argv`。引数の組み立ては `isolation` だけが持つ）で
//! 包む。プロキシは起動せず、到達できる送信先を持たせない（Linux は `--unshare-net` のまま中継も UDS も
//! 渡さない。macOS はすべての送信を拒否する Seatbelt）。
//!
//! # モードごとの挙動
//!
//! - `required`: 隔離できなければホストを一切実行せず [`HostVersionRunError::IsolationUnavailable`]。
//! - `best-effort`: 隔離できれば隔離の下で実行する。できなければ警告して隔離なしで実行する。
//! - Windows は隔離できない（`UnsupportedPlatform`）ため `best-effort` で隔離なしのまま。
//!
//! # 子プロセスの扱い
//!
//! cwd・`HOME`・`TMPDIR` は [`super::HostProcess`] と同じ専用 WorkDir（0700・終了時に削除）で、環境変数は
//! `env_clear` のうえ許可リストだけ。利用者の cwd は継承しない。時間・出力の上限は fail-closed。
//!
//! bwrap 下では `--die-with-parent` と pid 名前空間により、直接の子を kill すると ns 内の全プロセス
//! （孫を含む）が落ちる（`isolation` のモジュール doc の bwrap 0.11.1 の実測）。隔離なしの経路と
//! macOS（sandbox-exec はホストへ exec する）では直接の子だけを止める。孫が残り得るが、時間上限と
//! fail-closed は保つ（既知の制約）。bwrap の `--die-with-parent` は親**スレッド**の終了で発火するため、
//! spawn と待機は呼び出しスレッド上で同期的に行う（別スレッドで spawn しない）。

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fandhe_browser_core::EgressIsolationMode;

use super::isolation::{self, Plan, Sandbox};
use super::{
    EgressIsolationStatus, EgressIsolationUnavailable, EgressIsolationWarning, HostBinary,
    HostProcessError, IsolationProbe, IsolationWarning, TmpfsUnlimitedReason, WorkDir, child_paths,
    env, report_isolation_warning,
};

/// 版の検査でホストへ渡す固定の引数。
pub const HOST_VERSION_ARGS: &[&str] = &["--version"];
/// `--version` の完了を待つ既定の上限。大きな未署名バイナリの初回起動を見込む。
pub const DEFAULT_HOST_VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// `--version` の stdout の既定の上限バイト数。
pub const HOST_VERSION_STDOUT_MAX: u64 = 256;
/// 終了待ちのポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 版の検査の実行設定。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostVersionRunConfig {
    /// egress 隔離の要求水準（`[rendering] egress_isolation`）。
    pub egress_isolation: EgressIsolationMode,
    /// 隔離可否の判定元（既定 [`IsolationProbe::System`]。差し替えはテスト用）。
    pub isolation_probe: IsolationProbe,
    /// WorkDir を作る親（絶対パス。既定は OS の一時ディレクトリ）。
    pub temp_root: std::path::PathBuf,
    /// 完了を待つ上限。
    pub timeout: Duration,
    /// stdout の上限バイト数。
    pub stdout_max: u64,
    isolation_warning: IsolationWarning,
}

impl HostVersionRunConfig {
    /// 既定値で作る。
    pub fn new(egress_isolation: EgressIsolationMode) -> Self {
        Self {
            egress_isolation,
            isolation_probe: IsolationProbe::default(),
            temp_root: std::env::temp_dir(),
            timeout: DEFAULT_HOST_VERSION_TIMEOUT,
            stdout_max: HOST_VERSION_STDOUT_MAX,
            isolation_warning: IsolationWarning::default(),
        }
    }

    /// 隔離可否の判定元を差し替える（テスト用）。
    #[must_use]
    pub fn with_isolation_probe(mut self, probe: IsolationProbe) -> Self {
        self.isolation_probe = probe;
        self
    }

    /// WorkDir を作る親を差し替える。
    #[must_use]
    pub fn with_temp_root(mut self, root: std::path::PathBuf) -> Self {
        self.temp_root = root;
        self
    }

    /// 完了を待つ上限を差し替える。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// stdout の上限を差し替える。
    #[must_use]
    pub fn with_stdout_max(mut self, max: u64) -> Self {
        self.stdout_max = max;
        self
    }

    /// 隔離なしに落ちたときの警告の通知先（未設定なら stderr に 1 行）。
    #[must_use]
    pub fn with_isolation_warning(
        mut self,
        f: Arc<dyn Fn(EgressIsolationWarning) + Send + Sync>,
    ) -> Self {
        self.isolation_warning = IsolationWarning(Some(f));
        self
    }
}

/// 版の検査の成功結果。どの経路で走ったかを診断・テストが確かめられるよう構造で返す（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostVersionOutput {
    /// ホストの stdout（untrusted。上限内であることだけ保証する）。
    pub stdout: Vec<u8>,
    /// 実行時の隔離状態。
    pub isolation: EgressIsolationStatus,
}

/// 版の検査の失敗。表示は固定の英語で、パスと生出力を含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostVersionRunError {
    /// `required` で隔離できない。ホストは実行していない。
    IsolationUnavailable(EgressIsolationUnavailable),
    /// 専用の作業ディレクトリを用意できなかった。
    WorkDirFailed,
    /// 起動できなかった。
    SpawnFailed(io::ErrorKind),
    /// 上限時間内に終わらなかった。
    Timeout,
    /// 非 0 で終了した。
    ExitFailure,
    /// 出力を読めなかった。
    ReadFailed,
    /// 出力が上限を超えた。
    OutputTooLarge,
}

impl fmt::Display for HostVersionRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IsolationUnavailable(_) => "render host egress isolation is unavailable",
            Self::WorkDirFailed => "failed to prepare the working directory",
            Self::SpawnFailed(_) => "failed to start the render host",
            Self::Timeout => "the render host timed out",
            Self::ExitFailure => "the render host exited with an error",
            Self::ReadFailed => "failed to read the render host output",
            Self::OutputTooLarge => "the render host output was too large",
        })
    }
}

impl std::error::Error for HostVersionRunError {}

/// 解決済みホストを隔離の下で `--version` 付きで実行し、stdout を返す（cli の版の検査から呼ぶ）。
pub fn run_host_version(
    binary: &HostBinary,
    config: &HostVersionRunConfig,
) -> Result<HostVersionOutput, HostVersionRunError> {
    run_host_version_at(binary.path(), config)
}

/// [`run_host_version`] の本体。存在しないパスの失敗をテストするため、パスを直接受ける。
fn run_host_version_at(
    host: &Path,
    config: &HostVersionRunConfig,
) -> Result<HostVersionOutput, HostVersionRunError> {
    let workdir =
        WorkDir::create(&config.temp_root).map_err(|_| HostVersionRunError::WorkDirFailed)?;
    let (plan, canonical) = plan(config, &workdir)?;
    let canonical_root = canonical.as_ref().map(|c| c.dir.as_path());
    let (child_root, child_home, child_tmp) = child_paths(canonical_root, &workdir);

    let (program, args, isolation) = match &plan {
        Plan::Unisolated(reason) => {
            report_isolation_warning(
                &config.isolation_warning,
                EgressIsolationWarning::Unisolated(*reason),
                &mut io::stderr().lock(),
            );
            (
                host.to_path_buf(),
                HOST_VERSION_ARGS.iter().map(OsString::from).collect(),
                EgressIsolationStatus::Unisolated(*reason),
            )
        }
        Plan::Isolated(sandbox) => {
            warn_tmpfs_unlimited(sandbox, &config.isolation_warning);
            let (program, args) = match (sandbox, &canonical) {
                (Sandbox::Bwrap { bwrap, hidden }, Some(dir)) => isolation::linux_version_argv(
                    bwrap,
                    hidden,
                    &dir.dir,
                    dir.parent_to_hide.as_deref(),
                    host,
                ),
                (Sandbox::SandboxExec, _) => isolation::macos_version_argv(host),
                // plan が Bwrap に必ず dir を付ける。到達しない防御（隔離なしへは落とさない）。
                (Sandbox::Bwrap { .. }, None) => {
                    return Err(HostVersionRunError::IsolationUnavailable(
                        EgressIsolationUnavailable::SocketPathInvalid,
                    ));
                }
            };
            (
                program,
                args,
                EgressIsolationStatus::Isolated(sandbox.backend()),
            )
        }
    };

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .env_clear()
        .current_dir(&child_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    env::apply(&mut cmd, &child_root, &child_home, &child_tmp);
    let stdout = read_limited(cmd, config.timeout, config.stdout_max)?;
    Ok(HostVersionOutput { stdout, isolation })
}

/// bwrap の tmpfs に上限を付けられないとき、`HostProcess` と同じ理由で警告を 1 回通知する
/// （「黙って緩めない」。RENDER-3・#1498・#1502。版の検査は呼び出しごとに 1 回しか走らない）。
fn warn_tmpfs_unlimited(sandbox: &Sandbox, warning: &IsolationWarning) {
    let Sandbox::Bwrap { hidden, .. } = sandbox else {
        return;
    };
    let reason = match hidden.tmpfs_limit() {
        isolation::TmpfsLimit::Capped(_) => return,
        isolation::TmpfsLimit::Unsupported => TmpfsUnlimitedReason::SizeUnsupported,
        isolation::TmpfsLimit::Undetected(_) => TmpfsUnlimitedReason::DetectionFailed,
    };
    report_isolation_warning(
        warning,
        EgressIsolationWarning::TmpfsUnlimited(reason),
        &mut io::stderr().lock(),
    );
}

/// 隔離の方針を決める。`required` で隔離できなければここで失敗し、ホストは実行されない。
/// bwrap のときは WorkDir を隔離用に判定し（`/run` 配下・親を隠せない場合は降格または拒否）、
/// 正規化済みの WorkDir を返す。
fn plan(
    config: &HostVersionRunConfig,
    workdir: &WorkDir,
) -> Result<(Plan, Option<isolation::SocketDir>), HostVersionRunError> {
    let mode = config.egress_isolation;
    let availability = match config.isolation_probe {
        IsolationProbe::System => isolation::probe_sandbox(),
        IsolationProbe::Unavailable(r) => Err(r),
    };
    let map = |e: HostProcessError| match e {
        HostProcessError::EgressIsolationUnavailable(r) => {
            HostVersionRunError::IsolationUnavailable(r)
        }
        _ => HostVersionRunError::WorkDirFailed,
    };
    let plan = isolation::decide(mode, availability).map_err(map)?;
    let Plan::Isolated(Sandbox::Bwrap { hidden, .. }) = &plan else {
        return Ok((plan, None));
    };
    match isolation::sandbox_dir_for(workdir.path(), hidden, false) {
        Ok(dir) => Ok((plan, Some(dir))),
        Err(reason) => Ok((isolation::decide(mode, Err(reason)).map_err(map)?, None)),
    }
}

/// 子を起動し、stdout を上限付きで読む。終了後にまとめて読む方式だと孫が stdout を握り続けたとき
/// 時間の上限が破れるため、別スレッドで読む。spawn と待機は呼び出しスレッドで行う（モジュール doc）。
fn read_limited(
    mut cmd: Command,
    timeout: Duration,
    stdout_max: u64,
) -> Result<Vec<u8>, HostVersionRunError> {
    let mut child = cmd
        .spawn()
        .map_err(|e| HostVersionRunError::SpawnFailed(e.kind()))?;
    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err(HostVersionRunError::ReadFailed);
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
        return Err(HostVersionRunError::ReadFailed);
    }
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        kill_and_reap(&mut child);
        return Err(HostVersionRunError::Timeout);
    };
    let out = match rx.recv_timeout(timeout) {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            kill_and_reap(&mut child);
            return Err(HostVersionRunError::ReadFailed);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_and_reap(&mut child);
            return Err(HostVersionRunError::Timeout);
        }
    };
    if u64::try_from(out.len()).map_or(true, |n| n > stdout_max) {
        kill_and_reap(&mut child);
        return Err(HostVersionRunError::OutputTooLarge);
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(out)
                } else {
                    Err(HostVersionRunError::ExitFailure)
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(&mut child);
                return Err(HostVersionRunError::Timeout);
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => {
                kill_and_reap(&mut child);
                return Err(HostVersionRunError::ReadFailed);
            }
        }
    }
}

fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable() -> IsolationProbe {
        IsolationProbe::Unavailable(EgressIsolationUnavailable::SandboxToolNotFound)
    }

    /// 隔離なしに固定した設定（実環境の bwrap の有無に左右されない）。警告は捨てる。
    fn unisolated_config(temp_root: &Path) -> HostVersionRunConfig {
        HostVersionRunConfig::new(EgressIsolationMode::BestEffort)
            .with_isolation_probe(unavailable())
            .with_temp_root(temp_root.to_path_buf())
            .with_timeout(Duration::from_secs(5))
            .with_isolation_warning(Arc::new(|_| {}))
    }

    /// RENDER-3（#1553）: 存在しないパスは `SpawnFailed`（3 OS 共通の実プロセス経路）。
    #[test]
    fn render3_version_run_missing_binary_is_spawn_failed() {
        let missing = std::env::temp_dir().join("fandhe-no-such-host-1553");
        let err =
            run_host_version_at(&missing, &unisolated_config(&std::env::temp_dir())).unwrap_err();
        assert!(
            matches!(err, HostVersionRunError::SpawnFailed(_)),
            "{err:?}"
        );
    }

    /// RENDER-3（#1553）: `required` で隔離できないときはホストを実行しない。
    #[test]
    fn render3_version_run_required_unavailable_never_spawns_host() {
        let tmp = std::env::temp_dir().join(format!("fandhe-vr-req-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // 存在しないパス: spawn に到達すれば SpawnFailed になるので、IsolationUnavailable なら未実行。
        let config = HostVersionRunConfig::new(EgressIsolationMode::Required)
            .with_isolation_probe(unavailable())
            .with_temp_root(tmp.clone());
        let err = run_host_version_at(&tmp.join("nope"), &config).unwrap_err();
        assert_eq!(
            err,
            HostVersionRunError::IsolationUnavailable(
                EgressIsolationUnavailable::SandboxToolNotFound
            )
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// RENDER-3（#1553・#1502）: bwrap の tmpfs 上限なしは理由ごとに警告し、上限ありと macOS は黙る。
    #[test]
    fn render3_version_run_tmpfs_unlimited_warns_by_reason() {
        use std::path::PathBuf;
        use std::sync::Mutex;
        let bwrap = |limit| Sandbox::Bwrap {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            hidden: isolation::HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(limit),
        };
        let collect = |sandbox: &Sandbox| {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            let w = IsolationWarning(Some(Arc::new(move |e| sink.lock().unwrap().push(e))));
            warn_tmpfs_unlimited(sandbox, &w);
            seen.lock().unwrap().clone()
        };
        assert_eq!(
            collect(&bwrap(isolation::TmpfsLimit::Unsupported)),
            vec![EgressIsolationWarning::TmpfsUnlimited(
                TmpfsUnlimitedReason::SizeUnsupported
            )]
        );
        assert_eq!(collect(&bwrap(isolation::TmpfsLimit::Capped(1024))), vec![]);
        assert_eq!(collect(&Sandbox::SandboxExec), vec![]);
    }

    /// 偽ホストを使う実プロセスのテスト（unix）。
    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::sync::Mutex;

        struct Dir(PathBuf);
        impl Dir {
            fn new(tag: &str) -> Self {
                let p =
                    std::env::temp_dir().join(format!("fandhe-vrun-{}-{tag}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).expect("dir");
                Self(p)
            }
            fn work(&self) -> PathBuf {
                let w = self.0.join("work");
                std::fs::create_dir_all(&w).expect("work");
                w
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
        fn run(
            host: &Path,
            config: &HostVersionRunConfig,
        ) -> Result<HostVersionOutput, HostVersionRunError> {
            for _ in 0..20 {
                match run_host_version_at(host, config) {
                    Err(HostVersionRunError::SpawnFailed(io::ErrorKind::ExecutableFileBusy)) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    other => return other,
                }
            }
            run_host_version_at(host, config)
        }

        fn run_unisolated(
            d: &Dir,
            host: &Path,
            timeout: Duration,
            max: u64,
        ) -> Result<HostVersionOutput, HostVersionRunError> {
            let config = unisolated_config(&d.work())
                .with_timeout(timeout)
                .with_stdout_max(max);
            run(host, &config)
        }

        const T: Duration = Duration::from_secs(5);

        /// RENDER-3（#1553）: `best-effort` で隔離できないときは警告を 1 回通知し、隔離なしで実行する。
        #[test]
        fn render3_version_run_best_effort_unavailable_warns_once_and_runs_unisolated() {
            let d = Dir::new("warn");
            let h = fake_host(&d, "echo 'fandhe-browser-servo-host 1.2.3'");
            let seen = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            let config = unisolated_config(&d.work())
                .with_isolation_warning(Arc::new(move |w| sink.lock().unwrap().push(w)));
            let out = run(&h, &config).expect("runs");
            assert_eq!(out.stdout, b"fandhe-browser-servo-host 1.2.3\n");
            assert_eq!(
                out.isolation,
                EgressIsolationStatus::Unisolated(EgressIsolationUnavailable::SandboxToolNotFound)
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec![EgressIsolationWarning::Unisolated(
                    EgressIsolationUnavailable::SandboxToolNotFound
                )]
            );
        }

        #[test]
        fn render3_version_run_unresponsive_host_times_out() {
            let d = Dir::new("timeout");
            let h = fake_host(&d, "exec sleep 30");
            let start = Instant::now();
            assert_eq!(
                run_unisolated(&d, &h, Duration::from_millis(200), 256),
                Err(HostVersionRunError::Timeout)
            );
            assert!(start.elapsed() < Duration::from_secs(5));
        }

        /// 孫プロセスが stdout を握ったままでも時間の上限を守る。
        #[test]
        fn render3_version_run_grandchild_holding_stdout_still_times_out() {
            let d = Dir::new("grandchild");
            let h = fake_host(&d, "sleep 5\nexit 0");
            let start = Instant::now();
            assert_eq!(
                run_unisolated(&d, &h, Duration::from_millis(200), 256),
                Err(HostVersionRunError::Timeout)
            );
            assert!(start.elapsed() < Duration::from_secs(3));
        }

        #[test]
        fn render3_version_run_silent_success_returns_empty_stdout() {
            let d = Dir::new("silent");
            let h = fake_host(&d, "exit 0");
            let out = run_unisolated(&d, &h, T, 256).expect("ok");
            assert_eq!(out.stdout, b"");
        }

        #[test]
        fn render3_version_run_nonzero_exit_is_exit_failure() {
            let d = Dir::new("exit1");
            let h = fake_host(&d, "exit 1");
            assert_eq!(
                run_unisolated(&d, &h, T, 256),
                Err(HostVersionRunError::ExitFailure)
            );
        }

        #[test]
        fn render3_version_run_oversized_output_is_rejected() {
            let d = Dir::new("big");
            let h = fake_host(&d, "head -c 1024 /dev/zero | tr '\\0' 'a'; echo");
            assert_eq!(
                run_unisolated(&d, &h, T, 256),
                Err(HostVersionRunError::OutputTooLarge)
            );
        }

        /// 無限に出力するホストでも timeout を待たず上限超過で止める。
        #[test]
        fn render3_version_run_endless_output_is_rejected_without_waiting_for_timeout() {
            let d = Dir::new("endless");
            let h = fake_host(&d, "exec yes");
            let start = Instant::now();
            assert_eq!(
                run_unisolated(&d, &h, Duration::from_secs(20), 256),
                Err(HostVersionRunError::OutputTooLarge)
            );
            assert!(start.elapsed() < Duration::from_secs(10));
        }

        /// 引数は `--version` のみ。親の環境は渡らず、`HOME` は WorkDir 配下を指す。
        #[test]
        fn render3_version_run_passes_only_version_arg_and_workdir_env() {
            let d = Dir::new("env");
            let log = d.0.join("log");
            let h = fake_host(
                &d,
                &format!(
                    "echo \"$@\" > '{0}'; env >> '{0}'; echo 'fandhe-browser-servo-host 1.0.0'",
                    log.display()
                ),
            );
            run_unisolated(&d, &h, T, 256).expect("ok");
            let text = std::fs::read_to_string(&log).expect("log");
            assert_eq!(text.lines().next(), Some("--version"));
            assert!(!text.contains("CARGO_MANIFEST_DIR"), "{text}");
            let work = d.work();
            let home = text
                .lines()
                .find_map(|l| l.strip_prefix("HOME="))
                .expect("HOME set");
            assert!(
                Path::new(home).starts_with(&work) && home.ends_with("/home"),
                "{home}"
            );
        }

        /// RENDER-3（#1553・P3）: cwd は `temp_root` 配下の専用 WorkDir で、利用者の cwd ではない。
        /// 実行後に WorkDir は消える。
        #[test]
        fn render3_version_run_cwd_is_dedicated_workdir() {
            let d = Dir::new("cwd");
            let log = d.0.join("pwd.log");
            let h = fake_host(
                &d,
                &format!(
                    "pwd > '{}'; echo 'fandhe-browser-servo-host 1.0.0'",
                    log.display()
                ),
            );
            let work = d.work();
            run_unisolated(&d, &h, T, 256).expect("ok");
            let cwd = PathBuf::from(std::fs::read_to_string(&log).expect("log").trim());
            let work_canon = std::fs::canonicalize(&work).expect("canon");
            assert_eq!(cwd.parent(), Some(work_canon.as_path()), "{cwd:?}");
            assert!(
                cwd.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("fandhe-render-")),
                "{cwd:?}"
            );
            assert_ne!(cwd, std::env::current_dir().unwrap());
            assert!(!cwd.exists(), "WorkDir must be removed after the run");
        }

        /// RENDER-3・SEC-2（#1553）: 実 bwrap の下で版の検査が別 netns で走る（送信先を持たない）。
        /// bwrap が使えない環境ではその理由を確かめる（CI では必ず使える）。
        #[cfg(target_os = "linux")]
        #[test]
        fn render3_version_run_under_bwrap_is_in_separate_netns() {
            let d = Dir::new("netns");
            let h = fake_host(&d, "readlink /proc/self/ns/net");
            let config = HostVersionRunConfig::new(EgressIsolationMode::Required)
                .with_temp_root(d.work())
                .with_timeout(Duration::from_secs(10))
                .with_isolation_warning(Arc::new(|_| {}));
            match run(&h, &config) {
                Ok(out) => {
                    assert_eq!(
                        out.isolation,
                        EgressIsolationStatus::Isolated(
                            crate::host_process::IsolationBackend::Bwrap
                        )
                    );
                    let inside = String::from_utf8(out.stdout).expect("utf8");
                    let outside = std::fs::read_link("/proc/self/ns/net").expect("link");
                    assert!(inside.trim().starts_with("net:["), "{inside}");
                    assert_ne!(inside.trim(), outside.to_string_lossy());
                }
                Err(HostVersionRunError::IsolationUnavailable(r)) => {
                    let ci = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
                    assert!(!ci, "bwrap must work in CI: {r:?}");
                }
                Err(e) => panic!("{e:?}"),
            }
        }

        /// 偽ホスト: 版を出し、loopback の listener へ `nc` で接続を試みて終了コードを出す。
        /// env が空（PATH なし）のため絶対パスを使う。出力は stdout 上限 256 バイトに収まる。
        #[cfg(target_os = "macos")]
        fn nc_probe_host(d: &Dir, port: u16) -> PathBuf {
            fake_host(
                d,
                &format!(
                    "echo 'fandhe-browser-servo-host 1.0.0'; /usr/bin/nc -z -w 2 127.0.0.1 {port}; echo \"nc=$?\""
                ),
            )
        }

        /// RENDER-3・SEC-2（#1556）: 実 sandbox-exec（Seatbelt）の下で版の検査が通り、外向きの接続
        /// （loopback を含む）が拒否される。listener に接続が届かないことも確かめる。
        /// sandbox-exec が使えない環境ではその理由を確かめる（CI では必ず使える）。
        #[cfg(target_os = "macos")]
        #[test]
        fn render3_version_run_under_seatbelt_denies_outbound() {
            let d = Dir::new("seatbelt");
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let port = listener.local_addr().expect("addr").port();
            let h = nc_probe_host(&d, port);
            let config = HostVersionRunConfig::new(EgressIsolationMode::Required)
                .with_temp_root(d.work())
                .with_timeout(Duration::from_secs(10))
                .with_isolation_warning(Arc::new(|_| {}));
            match run(&h, &config) {
                Ok(out) => {
                    assert_eq!(
                        out.isolation,
                        EgressIsolationStatus::Isolated(
                            crate::host_process::IsolationBackend::SandboxExec
                        )
                    );
                    let text = String::from_utf8(out.stdout).expect("utf8");
                    assert!(text.contains("fandhe-browser-servo-host 1.0.0"), "{text}");
                    assert!(text.contains("nc="), "nc must have run: {text}");
                    assert!(!text.contains("nc=0"), "outbound must be denied: {text}");
                    let accepted = listener.accept();
                    assert!(
                        matches!(&accepted, Err(e) if e.kind() == io::ErrorKind::WouldBlock),
                        "no connection must reach the listener: {accepted:?}"
                    );
                }
                Err(HostVersionRunError::IsolationUnavailable(r)) => {
                    let ci = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
                    assert!(!ci, "sandbox-exec must work in CI: {r:?}");
                    eprintln!("skipped: isolation unavailable: {r:?}");
                }
                Err(e) => panic!("{e:?}"),
            }
        }

        /// RENDER-3・SEC-2（#1556）: 陽性対照。同じ偽ホストを隔離なしで走らせると `nc` が成功し、
        /// listener に接続が届く（`nc` が存在し、拒否の検証が空振りでないことの証明）。
        #[cfg(target_os = "macos")]
        #[test]
        fn render3_version_run_unisolated_nc_reaches_listener_positive_control() {
            let d = Dir::new("seatbelt-ctl");
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let port = listener.local_addr().expect("addr").port();
            let h = nc_probe_host(&d, port);
            let out = run_unisolated(&d, &h, Duration::from_secs(10), 256).expect("ok");
            let text = String::from_utf8(out.stdout).expect("utf8");
            assert!(text.contains("nc=0"), "nc must connect: {text}");
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "no connection reached listener");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => panic!("{e:?}"),
                }
            }
        }
    }
}
