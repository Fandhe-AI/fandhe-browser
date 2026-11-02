//! Servo ホストの egress 隔離: OS のサンドボックスで包む起動引数の組み立てと可否判定
//! （RENDER-3・RENDER-4・SEC-2・XOS-1。TASK-38・#1436・設計
//! `docs/design/servo-host-egress-isolation.md`）。
//!
//! [`super::HostProcess`] の `spawn()` が呼ぶ。許可リスト付き CONNECT プロキシ（`egress_proxy`・
//! #1434）の待ち受けと、ホスト側の引数（servo-host の `args.rs`・#1435）は部品として main に
//! あり、本モジュールはそれらを OS のコマンドで包む部分を担う。依存追加も `unsafe` もなく、
//! `pre_exec`・FFI は使わず外部コマンドの引数配列だけで隔離する（シェルを介さない）。
//!
//! - Linux: `bwrap --die-with-parent --unshare-net --dev-bind / / --tmpfs /run -- <ホスト>
//!   --stdio --egress-relay-uds <UDS>`。netns に出口が無くなり、ホストは netns 内の中継を
//!   経由して WorkDir 配下の UDS（本体側のプロキシ）にだけ届く。
//! - macOS: `sandbox-exec -p <profile> <ホスト> --stdio --egress-proxy-port <port>`。
//!   profile は loopback の 1 ポート以外への送信を拒否する（`harness/egress-isolation/seatbelt.py`
//!   と同じ形）。
//! - Windows ほか: 隔離できない（[`EgressIsolationUnavailable::UnsupportedPlatform`]。既知の制約）
//!
//! # 範囲・既知の制約
//!
//! - ファイルシステムは隔離しない（v1。`--dev-bind / /`）。隔離するのはネットワーク送信だけ。
//! - 隔離中は WebSocket が使えない（Servo の `websocket_loader` はプロキシを使わず直接接続する
//!   ため、遮断されて失敗する）。
//! - macOS の loopback プロキシは同一ユーザーの他プロセスからも届く（Seatbelt はホスト側の
//!   送信だけを絞る）。DNS 経路（mDNSResponder 経由の名前解決）は観測・遮断していない。
//! - 識別面（UA・ヘッダ・環境変数）は変えない（SEC-2）。プロキシはバイト列を改変しない。
//! - 外部コマンドは固定の絶対パス候補から選ぶ（PATH は使わない）。
//! - bwrap の `--die-with-parent` は bwrap 自身を親の死で落とすが、bwrap だけを kill しても子の
//!   ホストは残る（実測）。終了時・RSS 超過時はホストを先に名指しで落としてからラッパーを kill する
//!   （[`kill_process`]）。本体プロセス自体が SIGKILL された場合は bwrap だけが落ち、ホストは stdin の
//!   EOF で終わるのを待つ（既知の制約）。bwrap は呼び出しスレッドの終了でも落ちるため、
//!   `HostProcess` は専用の親スレッドで起動する。

use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU16;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use fandhe_browser_core::EgressIsolationMode;

use super::{HOST_ARGS, HostProcessError};

/// UDS パスの最大バイト長。servo-host の `args.rs`（`UDS_PATH_MAX_BYTES`）と同じ値で、
/// `sun_path`（Linux 108・macOS 104、NUL 込み）の両方に収まる。別 workspace のため複製している。
pub const RELAY_UDS_PATH_MAX_BYTES: usize = 100;

/// bwrap の候補（絶対パス。PATH は使わない）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BWRAP_CANDIDATES: [&str; 2] = ["/usr/bin/bwrap", "/usr/local/bin/bwrap"];
/// sandbox-exec の絶対パス。
const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";
/// プローブで走らせる無害なコマンドの候補。ホストバイナリは試走に使わない。
#[cfg_attr(not(unix), allow(dead_code))]
const TRUE_CANDIDATES: [&str; 2] = ["/usr/bin/true", "/bin/true"];
/// プローブ 1 回の上限時間。
#[cfg_attr(not(unix), allow(dead_code))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 隔離できない理由。`Display` は固定の英語文言で、パスを含めない（診断・警告用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EgressIsolationUnavailable {
    /// この OS には隔離の手段が無い（Windows ほか。既知の制約）。
    UnsupportedPlatform,
    /// bwrap・sandbox-exec が無い、または検証に失敗した。
    SandboxToolNotFound,
    /// サンドボックスの試走に失敗した（`--unshare-net` が許されない環境・timeout を含む）。
    SandboxProbeFailed,
    /// 作業ディレクトリが `/run` 配下で、`--tmpfs /run` に隠れて UDS に届かない。
    WorkDirHiddenBySandbox,
    /// 中継用 UDS のパスが servo-host の規則（絶対・正規化済み・100 バイト以下）に反する。
    SocketPathInvalid,
}

impl EgressIsolationUnavailable {
    /// 固定の英語文言（警告・`Display` 用。パスや環境値を含めない）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "no sandbox is available on this OS",
            Self::SandboxToolNotFound => "sandbox tool (bwrap or sandbox-exec) was not found",
            Self::SandboxProbeFailed => "sandbox tool could not start an isolated process",
            Self::WorkDirHiddenBySandbox => "work directory is hidden by the sandbox",
            Self::SocketPathInvalid => "relay socket path is not usable",
        }
    }
}

impl fmt::Display for EgressIsolationUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 隔離に使う仕組み。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IsolationBackend {
    /// Linux の bwrap（`--unshare-net`）。
    Bwrap,
    /// macOS の sandbox-exec（Seatbelt）。
    SandboxExec,
}

/// 現在のホストの隔離状態（診断・テスト用。REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EgressIsolationStatus {
    /// サンドボックスで包み、通信はプロキシに限っている。
    Isolated(IsolationBackend),
    /// 隔離していない（`best-effort` で隔離できなかった）。
    Unisolated(EgressIsolationUnavailable),
}

/// 隔離可否の判定元。本番は [`IsolationProbe::System`]。
///
/// `Unavailable` は**テスト用**の注入点（偽ホストを使う結合テストが、実行環境に bwrap が
/// あるかどうかで挙動を変えないための固定。先例は `EgressProxyConfig::with_resolver`）。
/// 本番（cli）は `System` のまま使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum IsolationProbe {
    /// OS のコマンドを実際に試走して判定する（結果はプロセス内でキャッシュする）。
    #[default]
    System,
    /// 常に隔離できないとする（テスト用）。
    Unavailable(EgressIsolationUnavailable),
}

/// 利用できると確認したサンドボックス。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Sandbox {
    // 構築は Linux の probe のみ（macOS・Windows では未使用で、-D warnings の dead_code になるため許可する）。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Bwrap(PathBuf),
    // 構築は macOS の probe のみ（他 OS では未使用）。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    SandboxExec,
}

impl Sandbox {
    pub(super) fn backend(&self) -> IsolationBackend {
        match self {
            Self::Bwrap(_) => IsolationBackend::Bwrap,
            Self::SandboxExec => IsolationBackend::SandboxExec,
        }
    }
}

/// 起動の方針。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
    Isolated(Sandbox),
    Unisolated(EgressIsolationUnavailable),
}

/// 可否の結果とモードから方針を決める。`Required` で隔離できなければ起動しない（fail-closed）。
pub(super) fn decide(
    mode: EgressIsolationMode,
    availability: Result<Sandbox, EgressIsolationUnavailable>,
) -> Result<Plan, HostProcessError> {
    match (availability, mode) {
        (Ok(s), _) => Ok(Plan::Isolated(s)),
        (Err(reason), EgressIsolationMode::Required) => {
            Err(HostProcessError::EgressIsolationUnavailable(reason))
        }
        (Err(reason), _) => Ok(Plan::Unisolated(reason)),
    }
}

/// `best-effort` で隔離なしに落とすときの警告文（英語・理由つき・パスを含めない）。
pub fn isolation_warning_message(reason: EgressIsolationUnavailable) -> String {
    format!(
        "render host egress isolation is unavailable ({reason}); \
         starting render host without egress isolation"
    )
}

/// bwrap を包む引数（`--` まで）。プローブと本番で同じフラグを使う。
fn linux_wrapper_args() -> Vec<OsString> {
    [
        "--die-with-parent",
        "--unshare-net",
        "--dev-bind",
        "/",
        "/",
        "--tmpfs",
        "/run",
        "--",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// Linux の起動コマンド（プログラムと引数）を組み立てる。
pub(super) fn linux_argv(bwrap: &Path, host: &Path, relay_uds: &Path) -> (PathBuf, Vec<OsString>) {
    let mut args = linux_wrapper_args();
    args.push(host.as_os_str().to_owned());
    args.extend(HOST_ARGS.iter().map(OsString::from));
    args.push(OsString::from("--egress-relay-uds"));
    args.push(relay_uds.as_os_str().to_owned());
    (bwrap.to_path_buf(), args)
}

/// Seatbelt の profile。loopback の `port` 以外への送信を拒否する。
/// 埋め込むのは検証済みの整数だけで、文字列の外部入力は入らない。
pub(super) fn seatbelt_profile(port: NonZeroU16) -> String {
    format!(
        "(version 1)(allow default)(deny network-outbound (remote ip))\
         (allow network-outbound (remote ip \"localhost:{port}\"))"
    )
}

/// macOS の起動コマンド（プログラムと引数）を組み立てる。
pub(super) fn macos_argv(host: &Path, port: NonZeroU16) -> (PathBuf, Vec<OsString>) {
    let mut args = vec![OsString::from("-p"), OsString::from(seatbelt_profile(port))];
    args.push(host.as_os_str().to_owned());
    args.extend(HOST_ARGS.iter().map(OsString::from));
    args.push(OsString::from("--egress-proxy-port"));
    args.push(OsString::from(port.to_string()));
    (PathBuf::from(SANDBOX_EXEC_PATH), args)
}

/// 中継 UDS のパスが servo-host の `args.rs` の規則を満たすか（絶対・NUL なし・空要素や `.`・`..`
/// なし・100 バイト以下）。ホストに拒否されて起動後に失敗するのを避けるため、起動前に検査する。
pub(super) fn relay_socket_path_ok(path: &Path) -> Result<(), EgressIsolationUnavailable> {
    let bad = EgressIsolationUnavailable::SocketPathInvalid;
    let value = path.to_str().ok_or(bad)?;
    if value.contains('\0') || !path.is_absolute() || value.len() > RELAY_UDS_PATH_MAX_BYTES {
        return Err(bad);
    }
    let clean = value.strip_prefix('/').is_some_and(|rest| {
        rest.split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
    });
    let components_ok = path
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    if clean && components_ok {
        Ok(())
    } else {
        Err(bad)
    }
}

/// 作業ディレクトリ（正規化済み）が `--tmpfs /run` に隠れるか。
pub(super) fn workdir_hidden_by_sandbox(canonical: &Path) -> bool {
    canonical.starts_with("/run")
}

/// プロキシが WorkDir 配下に作る UDS のファイル名（`egress_proxy` の固定名と一致させる。
/// 結合テストが bind 済みの実パスと突き合わせる）。
pub(super) const EGRESS_SOCKET_NAME: &str = "egress-proxy.sock";

/// ラッパー（bwrap）の子として動くホストを SIGKILL する（best-effort）。
///
/// 実測（bwrap 0.11.1・`--die-with-parent`・pid namespace なし）で、bwrap だけを kill しても
/// 子のホストは残った（init に reparent される）。そのためラッパーを kill する前に、ホストを
/// 名指しで落とす。std には pid 指定の kill が無く `unsafe`・libc を足せないため、絶対パスの
/// `kill` コマンドに整数の pid だけを渡す（シェルを介さない）。コマンドが無い環境ではホストが
/// 残りうる（stdin の EOF で終了するのを待つしかない。既知の制約）。呼び出し側は、ラッパーが
/// 生きていてホストがその子である間にだけ呼ぶこと（pid 再利用の取り違えを避ける）。
#[cfg(target_os = "linux")]
pub(super) fn kill_process(pid: u32) {
    let Some(kill) = ["/bin/kill", "/usr/bin/kill"]
        .iter()
        .map(Path::new)
        .find(|p| is_trusted_executable(p))
    else {
        return;
    };
    let mut cmd = Command::new(kill);
    cmd.args(["-s", "KILL", "--"]).arg(pid.to_string());
    let _ = run_probe(cmd, PROBE_TIMEOUT);
}

/// Linux 以外ではラッパーが exec するため（macOS）または隔離しないため、何もしない。
#[cfg(not(target_os = "linux"))]
pub(super) fn kill_process(_pid: u32) {}

/// bwrap 用に、WorkDir を正規化してソケット用ディレクトリとして返す。`--tmpfs /run` に隠れる、
/// または UDS のパスが規則に反する場合は隔離できない理由を返す（fail-closed）。
pub(super) fn socket_dir_for(workdir: &Path) -> Result<PathBuf, EgressIsolationUnavailable> {
    let canonical = std::fs::canonicalize(workdir)
        .map_err(|_| EgressIsolationUnavailable::SocketPathInvalid)?;
    if workdir_hidden_by_sandbox(&canonical) {
        return Err(EgressIsolationUnavailable::WorkDirHiddenBySandbox);
    }
    relay_socket_path_ok(&canonical.join(EGRESS_SOCKET_NAME))?;
    Ok(canonical)
}

/// 外部コマンドとして信頼できる実行ファイルか（通常ファイル・実行可能・他者書き込み不可）。
#[cfg(unix)]
fn is_trusted_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            m.is_file()
                && m.permissions().mode() & 0o111 != 0
                && m.permissions().mode() & 0o002 == 0
        }
        Err(_) => false,
    }
}

/// `cmd` を stdio なしで走らせ、`timeout` 内に成功終了したかを返す。超過したら kill して失敗。
#[cfg(unix)]
fn run_probe(mut cmd: Command, timeout: Duration) -> bool {
    cmd.env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let deadline = Instant::now().checked_add(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if deadline.is_some_and(|d| Instant::now() < d) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_uncached() -> Result<Sandbox, EgressIsolationUnavailable> {
    let bwrap = BWRAP_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| is_trusted_executable(p))
        .ok_or(EgressIsolationUnavailable::SandboxToolNotFound)?;
    let true_bin = TRUE_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| is_trusted_executable(p))
        .ok_or(EgressIsolationUnavailable::SandboxToolNotFound)?;
    let mut cmd = Command::new(bwrap);
    cmd.args(linux_wrapper_args()).arg(true_bin);
    if run_probe(cmd, PROBE_TIMEOUT) {
        Ok(Sandbox::Bwrap(bwrap.to_path_buf()))
    } else {
        Err(EgressIsolationUnavailable::SandboxProbeFailed)
    }
}

#[cfg(target_os = "macos")]
fn probe_uncached() -> Result<Sandbox, EgressIsolationUnavailable> {
    let tool = Path::new(SANDBOX_EXEC_PATH);
    if !is_trusted_executable(tool) {
        return Err(EgressIsolationUnavailable::SandboxToolNotFound);
    }
    let true_bin = TRUE_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| is_trusted_executable(p))
        .ok_or(EgressIsolationUnavailable::SandboxToolNotFound)?;
    let mut cmd = Command::new(tool);
    cmd.args(["-p", "(version 1)(allow default)"]).arg(true_bin);
    if run_probe(cmd, PROBE_TIMEOUT) {
        Ok(Sandbox::SandboxExec)
    } else {
        Err(EgressIsolationUnavailable::SandboxProbeFailed)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe_uncached() -> Result<Sandbox, EgressIsolationUnavailable> {
    Err(EgressIsolationUnavailable::UnsupportedPlatform)
}

/// この環境で隔離できるかを判定する。結果はプロセス内でキャッシュする（コマンドの有無は
/// 実行中に変わらない前提）。cli が `required` の前倒し判定にも使う。
pub fn probe_system() -> Result<(), EgressIsolationUnavailable> {
    probe_sandbox().map(|_| ())
}

pub(super) fn probe_sandbox() -> Result<Sandbox, EgressIsolationUnavailable> {
    static CACHE: OnceLock<Result<Sandbox, EgressIsolationUnavailable>> = OnceLock::new();
    CACHE.get_or_init(probe_uncached).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u16) -> NonZeroU16 {
        NonZeroU16::new(n).unwrap()
    }

    /// RENDER-3（#1436）: Linux の argv は bwrap の固定フラグ + ホスト + 中継 UDS と完全一致する。
    #[test]
    fn render3_linux_argv_is_exact() {
        let (prog, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            Path::new("/opt/host/servo-host"),
            Path::new("/tmp/w/egress-proxy.sock"),
        );
        assert_eq!(prog, PathBuf::from("/usr/bin/bwrap"));
        let got: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            got,
            [
                "--die-with-parent",
                "--unshare-net",
                "--dev-bind",
                "/",
                "/",
                "--tmpfs",
                "/run",
                "--",
                "/opt/host/servo-host",
                "--stdio",
                "--egress-relay-uds",
                "/tmp/w/egress-proxy.sock",
            ]
        );
    }

    /// RENDER-3（#1436）: Seatbelt の profile は整数ポートだけを埋め込んだ固定形。
    #[test]
    fn render3_seatbelt_profile_is_exact() {
        assert_eq!(
            seatbelt_profile(p(8123)),
            "(version 1)(allow default)(deny network-outbound (remote ip))\
             (allow network-outbound (remote ip \"localhost:8123\"))"
        );
    }

    /// RENDER-3（#1436）: macOS の argv は sandbox-exec + profile + ホスト + ポート。
    #[test]
    fn render3_macos_argv_is_exact() {
        let (prog, args) = macos_argv(Path::new("/opt/host/servo-host"), p(65535));
        assert_eq!(prog, PathBuf::from("/usr/bin/sandbox-exec"));
        let got: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            got,
            [
                "-p",
                "(version 1)(allow default)(deny network-outbound (remote ip))\
                 (allow network-outbound (remote ip \"localhost:65535\"))",
                "/opt/host/servo-host",
                "--stdio",
                "--egress-proxy-port",
                "65535",
            ]
        );
    }

    /// RENDER-3（#1436）: UDS パスの規則は servo-host の `args.rs` と同じ。
    #[test]
    fn render3_relay_socket_path_rules() {
        let exact = format!("/{}", "a".repeat(RELAY_UDS_PATH_MAX_BYTES - 1));
        assert_eq!(exact.len(), 100);
        assert!(relay_socket_path_ok(Path::new(&exact)).is_ok());
        let over = format!("/{}", "a".repeat(RELAY_UDS_PATH_MAX_BYTES));
        assert_eq!(over.len(), 101);
        let bad = Err(EgressIsolationUnavailable::SocketPathInvalid);
        assert_eq!(relay_socket_path_ok(Path::new(&over)), bad);
        for v in [
            "rel/x.sock",
            "/a//b.sock",
            "/a/./b.sock",
            "/a/../b.sock",
            "/a/b/",
            "/a\0b",
        ] {
            assert_eq!(relay_socket_path_ok(Path::new(v)), bad, "{v:?}");
        }
        assert!(relay_socket_path_ok(Path::new("/tmp/w/egress-proxy.sock")).is_ok());
    }

    /// RENDER-3（#1436）: `/run` 配下だけが `--tmpfs /run` に隠れる。
    #[test]
    fn render3_workdir_hidden_only_under_run() {
        assert!(workdir_hidden_by_sandbox(Path::new("/run")));
        assert!(workdir_hidden_by_sandbox(Path::new("/run/user/1000/x")));
        assert!(!workdir_hidden_by_sandbox(Path::new("/tmp/x")));
        assert!(!workdir_hidden_by_sandbox(Path::new("/running/x")));
    }

    /// RENDER-3（#1436）: モード × 可否の真理値表。`required` は隔離できなければ起動しない。
    #[test]
    fn render3_decide_truth_table() {
        use EgressIsolationMode::{BestEffort, Required};
        let ok = || Ok(Sandbox::SandboxExec);
        assert_eq!(
            decide(BestEffort, ok()).unwrap(),
            Plan::Isolated(Sandbox::SandboxExec)
        );
        assert_eq!(
            decide(Required, ok()).unwrap(),
            Plan::Isolated(Sandbox::SandboxExec)
        );
        for reason in [
            EgressIsolationUnavailable::UnsupportedPlatform,
            EgressIsolationUnavailable::SandboxToolNotFound,
            EgressIsolationUnavailable::SandboxProbeFailed,
            EgressIsolationUnavailable::WorkDirHiddenBySandbox,
            EgressIsolationUnavailable::SocketPathInvalid,
        ] {
            assert_eq!(
                decide(BestEffort, Err(reason)).unwrap(),
                Plan::Unisolated(reason)
            );
            match decide(Required, Err(reason)) {
                Err(HostProcessError::EgressIsolationUnavailable(r)) => assert_eq!(r, reason),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// RENDER-3（#1436）: 警告は英語で理由を含み、パスを含まない。
    #[test]
    fn render3_warning_message_has_reason_without_paths() {
        let m = isolation_warning_message(EgressIsolationUnavailable::SandboxToolNotFound);
        assert_eq!(
            m,
            "render host egress isolation is unavailable (sandbox tool (bwrap or sandbox-exec) \
             was not found); starting render host without egress isolation"
        );
        assert!(!m.contains('/'));
    }

    /// RENDER-3（#1436）: Windows ほかは隔離できない（既知の制約）。
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn render3_unsupported_platform_cannot_isolate() {
        assert_eq!(
            probe_system(),
            Err(EgressIsolationUnavailable::UnsupportedPlatform)
        );
    }
}
