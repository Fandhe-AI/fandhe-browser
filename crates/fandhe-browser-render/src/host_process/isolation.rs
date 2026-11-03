//! Servo ホストの egress 隔離: OS のサンドボックスで包む起動引数の組み立てと可否判定
//! （RENDER-3・RENDER-4・SEC-2・XOS-1。TASK-38・#1436・設計
//! `docs/design/servo-host-egress-isolation.md`）。
//!
//! [`super::HostProcess`] の `spawn()` が呼ぶ。許可リスト付き CONNECT プロキシ（`egress_proxy`・
//! #1434）の待ち受けと、ホスト側の引数（servo-host の `args.rs`・#1435）は部品として main に
//! あり、本モジュールはそれらを OS のコマンドで包む部分を担う。依存追加も `unsafe` もなく、
//! `pre_exec`・FFI は使わず外部コマンドの引数配列だけで隔離する（シェルを介さない）。
//!
//! - Linux: `bwrap --die-with-parent --unshare-net --unshare-pid --dev-bind / / --proc /proc
//!   --tmpfs /run -- <ホスト> --stdio --egress-relay-uds <UDS>`。netns に出口が無くなり、ホストは
//!   netns 内の中継を経由して WorkDir 配下の UDS（本体側のプロキシ）にだけ届く。`--unshare-pid` と
//!   新しい `/proc` により、ホストから本体の `/proc/<ppid>`（`mem`・`fd`・`environ`・ptrace）が見えず、
//!   乗っ取られたホストが本体へ介入して隔離を破る経路を塞ぐ。`--proc` は `--dev-bind / /` が
//!   ホストの `/proc` を持ち込んだ後に置く（先に置くと上書きされ、本体の pid が見えてしまう）。
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
//! - macOS の loopback プロキシは同一マシンの全ユーザー・全プロセスから届く（認証なし。Seatbelt
//!   はホスト側の送信だけを絞る）。宛先は許可リスト（公開 IP）に限られるので SSRF にはならないが、
//!   本体の送信元 IP を踏み台にされうる。UDS 化や接続トークンは、Servo のプロキシ設定が TCP
//!   （`http://127.0.0.1:<port>`）しか扱えず、macOS には netns が無く中継を置いても loopback の
//!   待ち受けが残るため、別途検討する（RENDER-3・#1451）。
//! - Linux でも FS は隔離しない（`--dev-bind / /`）ため、FS 上の UDS が迂回路として残る。`/run` は
//!   tmpfs で隠すが、`/tmp/ssh-*/agent.*`・`/tmp/.X11-unix`・`~/.gnupg/S.*`・同じ uid の他
//!   インスタンスのプロキシ UDS には届く。`/tmp`・`$HOME` を分けるかは別途判断する（残余リスク）。
//! - macOS の Seatbelt は `(allow default)` で、絞るのは IP の送信だけ。mach/XPC 経由の通信
//!   （他のデーモンを介した通信）は止めない。DNS 経路（mDNSResponder 経由の名前解決・DNS
//!   トンネリング）も同じ理由で観測・遮断していない。
//! - 識別面（UA・ヘッダ・環境変数）は変えない（SEC-2）。プロキシはバイト列を改変しない。
//! - 外部コマンドは固定の絶対パス候補から選ぶ（PATH は使わない）。
//! - pid 名前空間の中の pid 1 は bwrap の init 役で、外から見たプロセスツリーは
//!   `bwrap(起動した子) -> bwrap(ns 内 pid 1) -> ホスト` の 2 段になる。起動した子を kill すると
//!   `--die-with-parent` で ns 内 pid 1 も落ち、pid 1 の死で ns 内の全プロセスが SIGKILL される
//!   （bwrap 0.11.1 で実測）。ns 内の全滅は bwrap の `--die-with-parent` に依存しており、確かめた
//!   のは 0.11.1 だけで、それより古い版は未検証（最低版は決めていない）。そのためホストを名指しで kill する必要はなく、pid の再利用による
//!   取り違えもない。本体が SIGKILL されても同じ連鎖でホストまで落ちる。bwrap は呼び出し
//!   スレッドの終了でも落ちるため、`HostProcess` は専用の親スレッドで起動する。

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
///
/// ディストリビューション標準の `/usr/bin/bwrap` だけを候補にする。`/usr/local/bin` は管理者以外が
/// 書き込める運用（Linuxbrew 等）があるため含めない。候補は [`trusted_realpath`] で実体パスに解決し、
/// 実体ファイルと `/` までの全祖先が root 所有・group / other 書き込み不可であることを確かめる。
/// 以後の probe・exec はその実体パスを使う（判定と exec のパスを一致させる）。
///
/// 残余リスク（RENDER-3。未対応）: 信頼判定を通った実行ファイルが bwrap 互換かは、プローブ
/// （本番と同じフラグでの試走の終了コード）でしか確かめない。引数を無視して exit 0 する
/// root 所有コマンドは成功してしまう。プローブで「外へ出られないこと」まで実測する対策は
/// #1437（実ホスト検証）に委ねる。bwrap が Nix 等の root 所有でない場所にある環境は
/// `SandboxToolUntrusted` となり、`required` では起動できない（`best-effort` は警告つきで隔離なし）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BWRAP_CANDIDATES: [&str; 1] = ["/usr/bin/bwrap"];
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
    /// bwrap・sandbox-exec（または試走用の `true`）が候補のパスに見つからない。
    SandboxToolNotFound,
    /// 候補は見つかったが、信頼判定（実体と全祖先が root 所有・group / other 書き込み不可）に
    /// 通らない。`required` ではこの環境で起動できない（Nix・ユーザー所有の bwrap など）。
    SandboxToolUntrusted,
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
            Self::SandboxToolUntrusted => {
                "sandbox tool found but not trusted (must be root-owned and not group/other writable)"
            }
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

/// bwrap を包む引数（`--` まで）。プローブと本番で同じフラグを使う（RENDER-3）。
/// `--proc /proc` は `--dev-bind / /` の後ろに置く（順序が意味を持つ）。
fn linux_wrapper_args() -> Vec<OsString> {
    [
        "--die-with-parent",
        "--unshare-net",
        "--unshare-pid",
        "--dev-bind",
        "/",
        "/",
        "--proc",
        "/proc",
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

/// なし・100 バイト以下）。ホストに拒否されて起動後に失敗するのを避けるため、起動前に検査する。
/// 絶対判定は `/` 始まりの文字列で行う（UDS は unix 専用で、Windows の `is_absolute` はドライブ指定を要するため）。
/// なし・100 バイト以下）。ホストに拒否されて起動後に失敗するのを避けるため、起動前に検査する。
pub(super) fn relay_socket_path_ok(path: &Path) -> Result<(), EgressIsolationUnavailable> {
    let bad = EgressIsolationUnavailable::SocketPathInvalid;
    let value = path.to_str().ok_or(bad)?;
    if value.contains('\0') || !value.starts_with('/') || value.len() > RELAY_UDS_PATH_MAX_BYTES {
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

/// bwrap 用に、WorkDir を正規化してソケット用ディレクトリとして返す。`--tmpfs /run` に隠れる、
/// または UDS のパスが規則に反する場合は隔離できない理由を返す（fail-closed）。
///
/// 返すのは `HostProcess` がこの起動のために排他的に作った 0700 の WorkDir ルートで、
/// `EgressEndpoint::UnixSocketIn` の呼び出し側の契約を満たす（#1444・#1450）。契約はテスト
/// `render_3_sec_2_uds_dir_is_created_by_host_process_0700_and_owned`（`host_process.rs`）で固定する。
pub(super) fn socket_dir_for(workdir: &Path) -> Result<PathBuf, EgressIsolationUnavailable> {
    let canonical = std::fs::canonicalize(workdir)
        .map_err(|_| EgressIsolationUnavailable::SocketPathInvalid)?;
    if workdir_hidden_by_sandbox(&canonical) {
        return Err(EgressIsolationUnavailable::WorkDirHiddenBySandbox);
    }
    relay_socket_path_ok(&canonical.join(EGRESS_SOCKET_NAME))?;
    Ok(canonical)
}

/// メタデータが外部コマンドとして信頼できるか（RENDER-3・#1451）。通常ファイル・実行ビットあり・
/// 所有者が root・group / other に書き込み権限なし。root 所有のファイルはテストで作れないため
/// 純関数に分けている。
#[cfg(unix)]
fn trusted_metadata(is_file: bool, uid: u32, mode: u32) -> bool {
    is_file && uid == 0 && mode & 0o111 != 0 && mode & 0o022 == 0
}

/// 祖先ディレクトリのメタデータが信頼できるか（RENDER-3）。ディレクトリ・root 所有・
/// group / other 書き込み不可。sticky bit があっても group / other 書き込み可は拒否する
/// （他ユーザーが自分のエントリを差し込める場所を信頼しない）。
#[cfg(unix)]
fn trusted_dir_metadata(is_dir: bool, uid: u32, mode: u32) -> bool {
    is_dir && uid == 0 && mode & 0o022 == 0
}

/// 実体ファイルと祖先（親から `/` まで）の `(is_dir_or_file, uid, mode)` から信頼を判定する純関数
/// （RENDER-3・メタデータを差し替えてテストできる形）。
#[cfg(unix)]
fn trusted_chain(file: (bool, u32, u32), ancestors: &[(bool, u32, u32)]) -> bool {
    trusted_metadata(file.0, file.1, file.2)
        && ancestors
            .iter()
            .all(|&(is_dir, uid, mode)| trusted_dir_metadata(is_dir, uid, mode))
}

/// 候補パスの信頼判定の結果。
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TrustCheck {
    /// 存在しない。
    Missing,
    /// 存在するが信頼できない（所有者・書き込み権限・種別・解決不能）。
    Untrusted,
    /// 信頼できる。値は canonicalize 済みの実体パスで、以後の probe・exec に使う。
    Trusted(PathBuf),
}

/// 候補を `canonicalize` した実体パスで判定する（RENDER-3）。シンボリックリンクは解決し、
/// 実体ファイルと `/` までの全祖先を見る。リンクの置き場所（ユーザー所有の `/usr/local/bin` 等）
/// ではなく実体の側で判定するため、リンク先の実体が信頼できなければ拒否される。
/// `resolve_sandbox_plan` が bwrap・sandbox-exec・true の選定に使う。
#[cfg(unix)]
fn trusted_realpath(path: &Path) -> TrustCheck {
    use std::os::unix::fs::MetadataExt;
    let real = match std::fs::canonicalize(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TrustCheck::Missing,
        Err(_) => return TrustCheck::Untrusted,
    };
    let Ok(m) = std::fs::metadata(&real) else {
        return TrustCheck::Untrusted;
    };
    let mut ancestors = Vec::new();
    for dir in real.ancestors().skip(1) {
        match std::fs::metadata(dir) {
            Ok(dm) => ancestors.push((dm.is_dir(), dm.uid(), dm.mode())),
            Err(_) => return TrustCheck::Untrusted,
        }
    }
    if trusted_chain((m.is_file(), m.uid(), m.mode()), &ancestors) {
        TrustCheck::Trusted(real)
    } else {
        TrustCheck::Untrusted
    }
}

/// 候補から最初に信頼できるものの実体パスを返す。信頼できるものが無ければ、候補が 1 つでも
/// 存在した（信頼できなかった）なら `SandboxToolUntrusted`、全て無ければ `SandboxToolNotFound`。
#[cfg(unix)]
fn select_trusted(candidates: &[&str]) -> Result<PathBuf, EgressIsolationUnavailable> {
    let mut found_untrusted = false;
    for c in candidates {
        match trusted_realpath(Path::new(c)) {
            TrustCheck::Trusted(real) => return Ok(real),
            TrustCheck::Untrusted => found_untrusted = true,
            TrustCheck::Missing => {}
        }
    }
    Err(if found_untrusted {
        EgressIsolationUnavailable::SandboxToolUntrusted
    } else {
        EgressIsolationUnavailable::SandboxToolNotFound
    })
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
    let bwrap = select_trusted(&BWRAP_CANDIDATES)?;
    let true_bin = select_trusted(&TRUE_CANDIDATES)?;
    let mut cmd = Command::new(&bwrap);
    cmd.args(linux_wrapper_args()).arg(&true_bin);
    if run_probe(cmd, PROBE_TIMEOUT) {
        Ok(Sandbox::Bwrap(bwrap))
    } else {
        Err(EgressIsolationUnavailable::SandboxProbeFailed)
    }
}

#[cfg(target_os = "macos")]
fn probe_uncached() -> Result<Sandbox, EgressIsolationUnavailable> {
    // 実起動（`macos_argv`）は SANDBOX_EXEC_PATH を exec するため、実体パスが一致しないものは
    // 判定と exec がずれるので信頼しない。
    let tool = select_trusted(&[SANDBOX_EXEC_PATH])?;
    if tool != Path::new(SANDBOX_EXEC_PATH) {
        return Err(EgressIsolationUnavailable::SandboxToolUntrusted);
    }
    let true_bin = select_trusted(&TRUE_CANDIDATES)?;
    let mut cmd = Command::new(&tool);
    cmd.args(["-p", "(version 1)(allow default)"])
        .arg(&true_bin);
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

    /// RENDER-3（#1451）: root 所有で group / other 書き込み不可の実行ファイルは信頼する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_metadata_accepts_root_owned_executable() {
        assert!(trusted_metadata(true, 0, 0o755));
        assert!(trusted_metadata(true, 0, 0o555));
        // 古い Debian の setuid bwrap。
        assert!(trusted_metadata(true, 0, 0o4755));
    }

    /// RENDER-3（#1451）: root 以外が所有するファイルは拒否する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_metadata_rejects_non_root_owner() {
        assert!(!trusted_metadata(true, 1000, 0o755));
        assert!(!trusted_metadata(true, 1, 0o755));
    }

    /// RENDER-3（#1451）: group / other が書き込めるファイルは拒否する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_metadata_rejects_group_or_other_writable() {
        assert!(!trusted_metadata(true, 0, 0o775));
        assert!(!trusted_metadata(true, 0, 0o757));
        assert!(!trusted_metadata(true, 0, 0o777));
    }

    /// RENDER-3（#1451）: 実行ビットが無いもの・通常ファイルでないものは拒否する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_metadata_rejects_non_executable_or_non_file() {
        assert!(!trusted_metadata(true, 0, 0o644));
        assert!(!trusted_metadata(false, 0, 0o755));
    }

    /// RENDER-3（#1451）: 実ファイルでも、自分が所有する 0755 のファイルは拒否される
    /// （root で実行する環境では所有者が 0 になるため、group 書き込み可の 0775 で確かめる）。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_realpath_rejects_user_owned_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("fb-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fake-bwrap");
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        let owner = std::fs::metadata(&f).unwrap().uid();
        if owner != 0 {
            assert_eq!(trusted_realpath(&f), TrustCheck::Untrusted);
        } else {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o775)).unwrap();
            assert_eq!(trusted_realpath(&f), TrustCheck::Untrusted);
        }
        assert_eq!(trusted_realpath(&dir.join("absent")), TrustCheck::Missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RENDER-3: 実体と全祖先が root 所有 0755 なら受理、祖先に非 root 所有・group / other 書き込み可
    /// （sticky の有無によらず）があれば拒否、実体が信頼できなければ祖先が良くても拒否する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_chain_checks_every_ancestor() {
        let file = (true, 0, 0o755);
        let dir = (true, 0, 0o755);
        assert!(trusted_chain(file, &[dir, dir, dir]));
        assert!(trusted_chain(file, &[]));
        // 非 root 所有の祖先
        assert!(!trusted_chain(file, &[dir, (true, 1000, 0o755), dir]));
        // group / other 書き込み可の祖先
        assert!(!trusted_chain(file, &[(true, 0, 0o775), dir]));
        assert!(!trusted_chain(file, &[dir, (true, 0, 0o757)]));
        // sticky つきの 1777（/tmp 相当）も拒否
        assert!(!trusted_chain(file, &[(true, 0, 0o1777)]));
        // 祖先がディレクトリでない
        assert!(!trusted_chain(file, &[(false, 0, 0o755)]));
        // 実体が不正
        assert!(!trusted_chain((true, 1000, 0o755), &[dir]));
        assert!(!trusted_chain((true, 0, 0o775), &[dir]));
    }

    /// RENDER-3: ユーザー所有ディレクトリ内の symlink は、リンクの置き場所ではなく実体で判定され、
    /// 信頼できれば返るパスは実体（exec に使うパス）になる。`/usr/bin/true` は root 所有・祖先も
    /// root 所有の標準配置を前提とする。実体が信頼できないリンク（自作ファイル宛）は拒否される。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_realpath_judges_symlink_by_target() {
        let dir = std::env::temp_dir().join(format!("fb-trust-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real_true = std::fs::canonicalize("/usr/bin/true").unwrap();
        let link = dir.join("bwrap");
        std::os::unix::fs::symlink(&real_true, &link).unwrap();
        assert_eq!(trusted_realpath(&link), TrustCheck::Trusted(real_true));

        let own = dir.join("own");
        std::fs::write(&own, b"#!/bin/sh\n").unwrap();
        let bad = dir.join("bad-link");
        std::os::unix::fs::symlink(&own, &bad).unwrap();
        assert_eq!(trusted_realpath(&bad), TrustCheck::Untrusted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RENDER-3: 候補が全て無ければ NotFound、あるが信頼できなければ Untrusted。
    #[cfg(unix)]
    #[test]
    fn render3_select_trusted_distinguishes_missing_and_untrusted() {
        assert_eq!(
            select_trusted(&["/nonexistent-fb-dir/bwrap"]),
            Err(EgressIsolationUnavailable::SandboxToolNotFound)
        );
        let dir = std::env::temp_dir().join(format!("fb-trust-sel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("tool");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(
            select_trusted(&[f.to_str().unwrap()]),
            Err(EgressIsolationUnavailable::SandboxToolUntrusted)
        );
        let _ = std::fs::remove_dir_all(&dir);
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
                "--unshare-pid",
                "--dev-bind",
                "/",
                "/",
                "--proc",
                "/proc",
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
            EgressIsolationUnavailable::SandboxToolUntrusted,
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
        let u = isolation_warning_message(EgressIsolationUnavailable::SandboxToolUntrusted);
        assert_eq!(
            u,
            "render host egress isolation is unavailable (sandbox tool found but not trusted \
             (must be root-owned and not group/other writable)); starting render host without \
             egress isolation"
        );
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
