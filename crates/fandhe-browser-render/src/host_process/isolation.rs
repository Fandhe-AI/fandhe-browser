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
//!   --tmpfs /run --tmpfs /tmp [--tmpfs <$HOME>] --bind <WorkDir> <WorkDir> [--ro-bind <ホスト>
//!   <ホスト>] -- <ホスト> --stdio --egress-relay-uds <UDS>`（`--ro-bind` はホストが `/tmp` か
//!   `$HOME` の配下にあるときだけ。`--tmpfs` の後に bind を置く順序が意味を持つ）。netns に出口が無くなり、ホストは
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
//! - ファイルシステム全体の読み書きは隔離しない（`--dev-bind / /`）。隠すのは `/run`・`/tmp`・
//!   本体の `$HOME`（tmpfs。#1485）で、戻すのはこのインスタンスの WorkDir（rw）と、隠れる場所に
//!   あるホストの実行ファイル（ro）だけ。
//! - 隔離中は WebSocket が使えない（Servo の `websocket_loader` はプロキシを使わず直接接続する
//!   ため、遮断されて失敗する）。
//! - macOS の loopback プロキシは同一マシンの全ユーザー・全プロセスから届く（認証なし。Seatbelt
//!   はホスト側の送信だけを絞る）。宛先は許可リスト（公開 IP）に限られるので SSRF にはならないが、
//!   本体の送信元 IP を踏み台にされうる。UDS 化や接続トークンは、Servo のプロキシ設定が TCP
//!   （`http://127.0.0.1:<port>`）しか扱えず、macOS には netns が無く中継を置いても loopback の
//!   待ち受けが残るため、別途検討する（RENDER-3・#1451）。
//! - Linux で残る FS 上の UDS の迂回路: `/run`・`/tmp`・`$HOME` 以外の場所（`/var/tmp`・`/dev/shm`・
//!   独自の場所）の UDS。また `temp_root` が `/tmp` と `$HOME` の外にあると、兄弟 WorkDir（同じ uid の
//!   他インスタンスのプロキシ UDS）は見えたままになる。抽象名前空間のソケットは netns ごとに分かれる
//!   ので対象外（#1457・#1485）。
//! - `$HOME` と `/tmp` が隠れるため、ユーザーのフォント（`~/.local/share/fonts`・`~/.fonts`）や
//!   `$HOME` の設定は見えない（`FONTCONFIG_FILE` 等が `$HOME` 配下を指していても同様）。
//! - `$HOME` が未設定・相対・`/`・存在しない・ディレクトリでないときは隠せないため
//!   [`EgressIsolationUnavailable::HomeNotHideable`]（`required` は拒否、`best-effort` は警告つきで
//!   隔離なし）。`$HOME` はプローブ時に 1 回だけ読みキャッシュする。本体の `HOME` も子へ強制で渡す
//!   `HOME` も書き換えない（SEC-2）。`$HOME` がシステムのディレクトリを指すとプローブか起動が失敗する
//!   が fail-closed で、fail-open にはならない。
//! - 子の cwd は正規化前の WorkDir パス。`/run` 配下にある実行ファイルは戻さない。
//! - macOS の Seatbelt は `(allow default)` で、絞るのは IP の送信だけ。mach/XPC 経由の通信
//!   （他のデーモンを介した通信）は止めない。DNS 経路（mDNSResponder 経由の名前解決・DNS
//!   トンネリング）も同じ理由で観測・遮断していない。
//! - WorkDir（`HostProcessConfig::temp_root`。既定は `TMPDIR`）が正規化後に `/run` 配下だと
//!   （例: `TMPDIR=/run/user/<uid>`）`--tmpfs /run` に隠れて UDS に届かないため、隔離できないと
//!   判定する（`required` は起動を拒否し、`best-effort` は警告つきで隔離なしで起動する）。これは
//!   意図した fail-closed で変えない（#1457・#1459）。回避策は `/run` の外を `temp_root` または
//!   `TMPDIR` に指定すること。
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
/// bwrap の以後の probe・exec はその実体パスを使う（判定と exec のパスを一致させる。bwrap は
/// multicall ではない）。試走用の `readlink` だけは候補パスで exec する（[`READLINK_CANDIDATES`]）。
///
/// プローブは、サンドボックス内の `readlink /proc/self/ns/net`・`/proc/self/ns/pid` の出力を本体の
/// 値と比べ、ネットワーク名前空間と pid 名前空間がどちらも**異なるときだけ**成功とする（pid ns は #1466）（RENDER-3・#1459。終了コードだけを見ると、引数を
/// 無視して exit 0 する root 所有コマンドを隔離できたと誤認する）。exit 0 だけの偽物は出力が空、
/// ns を作らず中身だけ実行する偽物は inode が同じなので、どちらも失敗する。`--unshare-net` で
/// 作った netns には `lo` しかないため、別の netns にいることが「外部への出口が無い」ことの
/// 十分条件になる。ただしそれは、その netns が**本物の** bwrap の `--unshare-net` で作られた
/// という前提に依存する（プローブが確かめるのは別の netns にいることまで）。本物であることは
/// root 所有の信頼判定で担保する。`/sys/class/net` は `--dev-bind / /` でホストの sysfs が見えるため判定に使えない。
///
/// 残る限界: root 所有の候補パスを差し替えられる攻撃者（それらしい別の inode を出力する悪意ある
/// バイナリを置ける者）は脅威モデルの外にある（その権限があれば隔離そのものを外せる）。
/// bwrap が Nix 等の root 所有でない場所にある環境は
/// `SandboxToolUntrusted` となり、`required` では起動できない（`best-effort` は警告つきで隔離なし）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BWRAP_CANDIDATES: [&str; 1] = ["/usr/bin/bwrap"];
/// sandbox-exec の絶対パス。
const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";
/// プローブで走らせる無害なコマンドの候補。ホストバイナリは試走に使わない。
///
/// bwrap と違い、exec には実体パスではなく**候補パスのまま**を渡す（[`select_trusted_exec`]）。
/// uutils の `coreutils`・busybox のような multicall バイナリは argv[0] で動作を決めるため、
/// 実体パス（例: `/usr/lib/cargo/bin/coreutils`）で exec すると `true` として動かず、
/// `required` で起動できなくなる（fail-closed だが可用性の問題。#1458）。信頼判定は実体の
/// チェーンと、候補パス側の symlink チェーンの両方で行う。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const TRUE_CANDIDATES: [&str; 2] = ["/usr/bin/true", "/bin/true"];
/// Linux のプローブでサンドボックス内から netns を読ませる `readlink` の候補（#1459）。
///
/// [`TRUE_CANDIDATES`] と同じく、exec するのは**候補パスのまま**（multicall の argv[0] 対策。
/// #1458）で、信頼判定は [`select_trusted_exec`] で行う。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const READLINK_CANDIDATES: [&str; 2] = ["/usr/bin/readlink", "/bin/readlink"];
/// プローブで本体と比べる名前空間の種類（RENDER-3・#1459・#1466）。`--unshare-net` の netns に加え、
/// 本番が渡す `--unshare-pid` の pid ns も分離できているかを確かめる（rss 監視が ns 内 pid 1 と
/// ラッパーの kill で ns 内のホストも落ちることを前提にするため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Namespace {
    Net,
    Pid,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Namespace {
    /// 自プロセス（サンドボックス内では内側のプロセス）の名前空間を指す symlink。
    fn link(self) -> &'static str {
        match self {
            Self::Net => "/proc/self/ns/net",
            Self::Pid => "/proc/self/ns/pid",
        }
    }

    /// `readlink` の出力の接頭辞（`<種別>:[<inode>]`）。
    fn prefix(self) -> &'static str {
        match self {
            Self::Net => "net:[",
            Self::Pid => "pid:[",
        }
    }
}
/// プローブの stdout を読む上限（`net:[<u64>]` は 30 バイト未満。超過は fail-closed）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PROBE_STDOUT_MAX: u64 = 256;
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
    /// 作業ディレクトリが `/run` 配下で、`--tmpfs /run` に隠れて UDS に届かない。意図した
    /// fail-closed で、`required` は起動を拒否する。回避策は `/run` の外を `temp_root`
    /// （`with_temp_root`）または `TMPDIR` に指定すること（#1459）。
    WorkDirHiddenBySandbox,
    /// 中継用 UDS のパスが servo-host の規則（絶対・正規化済み・100 バイト以下）に反する。
    SocketPathInvalid,
    /// 本体の `$HOME` を tmpfs で隠せない（未設定・相対パス・`/`・存在しない・ディレクトリでない。
    /// RENDER-3・#1485）。`required` は起動を拒否し、`best-effort` は警告つきで隔離なしで起動する。
    /// 回避策は有効な絶対パスのディレクトリを `HOME` に設定すること（最小構成のコンテナで起きやすい）。
    HomeNotHideable,
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
            Self::HomeNotHideable => "home directory cannot be hidden by the sandbox",
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
    Bwrap {
        /// 信頼判定済みの bwrap の実体パス。
        bwrap: PathBuf,
        /// プローブで使ったのと同じ隠し対象（起動時にもそのまま使い、両者の一致を構造で保証する）。
        hidden: HiddenPaths,
    },
    // 構築は macOS の probe のみ（他 OS では未使用）。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    SandboxExec,
}

impl Sandbox {
    pub(super) fn backend(&self) -> IsolationBackend {
        match self {
            Self::Bwrap { .. } => IsolationBackend::Bwrap,
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

/// bwrap が隠す対象（`--tmpfs`）。`/run` と `/tmp` は固定で、`$HOME` はプローブ時に 1 回だけ
/// 解決した正規化済みパス（RENDER-3・SEC-2・#1485）。FS 上の UDS（`/tmp/ssh-*/agent.*`・
/// `/tmp/.X11-unix`・`~/.gnupg/S.*`・同じ uid の他インスタンスのプロキシ UDS）が netns の外への
/// 迂回路になるのを塞ぐ。本体の `HOME` 環境変数は書き換えない（子へ強制で渡す `HOME` も `env.rs` のまま）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HiddenPaths {
    /// 正規化済みの `$HOME`。`/run`・`/tmp` の配下なら既に隠れているので `None`（tmpfs の重複を省く）。
    home: Option<PathBuf>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl HiddenPaths {
    /// 固定で隠すパス（`--tmpfs` の先頭 2 つ）。
    const FIXED: [&'static str; 2] = ["/run", "/tmp"];

    /// 正規化済みの `$HOME`（[`resolve_home_to_hide`] の結果）から作る。
    pub(super) fn new(home_canonical: PathBuf) -> Self {
        let covered = Self::FIXED.iter().any(|f| home_canonical.starts_with(f));
        Self {
            home: if covered { None } else { Some(home_canonical) },
        }
    }

    /// `--tmpfs` を並べる順の対象（`/run`・`/tmp`・必要なら `$HOME`）。
    fn tmpfs_targets(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = Self::FIXED.iter().map(Path::new).collect();
        v.extend(self.home.as_deref());
        v
    }

    /// 正規化済みの `canonical` が `/tmp` か `$HOME` の配下で隠れるか。ホストの実行ファイルを
    /// `--ro-bind` で戻す要否の判定に使う。`/run` 配下は含めない（従来どおり。#1485 のスコープ外）。
    pub(super) fn hides(&self, canonical: &Path) -> bool {
        canonical.starts_with("/tmp")
            || self
                .home
                .as_deref()
                .is_some_and(|h| canonical.starts_with(h))
    }
}

/// `$HOME` の生の値を、tmpfs で隠せる正規化済みパスへ解決する（RENDER-3・#1485）。
///
/// 絶対パスで、正規化でき、`/` ではなく、ディレクトリのときだけ `Ok`。それ以外（未設定・相対・
/// 存在しない・`/`〔`//`・`/.`・`/` を指す symlink を含む〕・ファイル）は
/// [`EgressIsolationUnavailable::HomeNotHideable`]。環境変数を書き換えずにテストできるよう、
/// 値は引数で受け取る（edition 2024 の `set_var` は `unsafe`）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn resolve_home_to_hide(
    raw: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, EgressIsolationUnavailable> {
    let bad = EgressIsolationUnavailable::HomeNotHideable;
    let path = Path::new(raw.ok_or(bad)?);
    if !path.is_absolute() {
        return Err(bad);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| bad)?;
    if canonical == Path::new("/") || !canonical.is_dir() {
        return Err(bad);
    }
    Ok(canonical)
}

/// `--bind` / `--ro-bind` の種別（tmpfs で隠した後に戻すもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) enum BindKind {
    ReadWrite,
    ReadOnly,
}

/// bwrap を包む引数（`--` まで）。プローブと本番で名前空間・`--dev-bind`・`--proc`・`--tmpfs` を
/// 同じにする（RENDER-3）。`--proc /proc` は `--dev-bind / /` の後ろに、`binds`（隠した後に戻す
/// 自分の WorkDir とホストの実行ファイル）は `--tmpfs` の**後ろ**に置く（順序が意味を持つ）。
/// bind は起動ごとのものなのでプローブには付けない。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_wrapper_args(hidden: &HiddenPaths, binds: &[(BindKind, &Path)]) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--die-with-parent",
        "--unshare-net",
        "--unshare-pid",
        "--dev-bind",
        "/",
        "/",
        "--proc",
        "/proc",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    for t in hidden.tmpfs_targets() {
        args.push(OsString::from("--tmpfs"));
        args.push(t.as_os_str().to_owned());
    }
    for (kind, p) in binds {
        args.push(OsString::from(match kind {
            BindKind::ReadWrite => "--bind",
            BindKind::ReadOnly => "--ro-bind",
        }));
        args.push(p.as_os_str().to_owned());
        args.push(p.as_os_str().to_owned());
    }
    args.push(OsString::from("--"));
    args
}

/// Linux の起動コマンド（プログラムと引数）を組み立てる（FS には触れない純関数）。
///
/// `workdir` は正規化済みの WorkDir（rw で戻す）、`host` は正規化済みのホスト実行ファイル（隠れる
/// 場所にあるときだけ ro で戻す）。`relay_uds` が `workdir` の外だと隠れて届かないため `None`
/// （呼び出し側が起動失敗にする）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn linux_argv(
    bwrap: &Path,
    hidden: &HiddenPaths,
    workdir: &Path,
    host: &Path,
    relay_uds: &Path,
) -> Option<(PathBuf, Vec<OsString>)> {
    if !relay_uds.starts_with(workdir) {
        return None;
    }
    let mut binds = vec![(BindKind::ReadWrite, workdir)];
    if hidden.hides(host) {
        binds.push((BindKind::ReadOnly, host));
    }
    let mut args = linux_wrapper_args(hidden, &binds);
    args.push(host.as_os_str().to_owned());
    args.extend(HOST_ARGS.iter().map(OsString::from));
    args.push(OsString::from("--egress-relay-uds"));
    args.push(relay_uds.as_os_str().to_owned());
    Some((bwrap.to_path_buf(), args))
}

/// プローブ用の bwrap 起動コマンド。本番と同じフラグ（[`linux_wrapper_args`]。tmpfs も同じ）の後ろに、
/// `readlink_exec`（候補パスのまま。multicall の argv[0] 対策。#1458）と `link`（[`Namespace::link`]）
/// を置く（内側の名前空間を標準出力へ出させる。RENDER-3・#1459・#1466）。busybox の `readlink` は
/// オペランドを 1 つしか取らないため、名前空間ごとに別の起動にする。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_probe_argv(
    bwrap: &Path,
    hidden: &HiddenPaths,
    readlink_exec: &Path,
    link: &str,
) -> (PathBuf, Vec<OsString>) {
    // readlink は root 所有の `/usr/bin`・`/bin` にあり隠れないため、bind は要らない。
    let mut args = linux_wrapper_args(hidden, &[]);
    args.push(readlink_exec.as_os_str().to_owned());
    args.push(OsString::from(link));
    (bwrap.to_path_buf(), args)
}

/// `<種別>:[<10 進数>]`（末尾の改行は 1 個まで）だけを名前空間の inode 番号として読む。それ以外
/// （空・種別違い・前後のゴミ・桁あふれ）は `None`（fail-closed）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_ns_id(ns: Namespace, s: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(s).ok()?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let digits = text.strip_prefix(ns.prefix())?.strip_suffix(']')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// 外側と内側の名前空間がどちらも読め、かつ**異なる**ときだけ真（RENDER-3・#1459・#1466）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn ns_separated(ns: Namespace, outside: &[u8], inside_stdout: &[u8]) -> bool {
    match (parse_ns_id(ns, outside), parse_ns_id(ns, inside_stdout)) {
        (Some(o), Some(i)) => o != i,
        _ => false,
    }
}

/// 名前空間プローブの結果（テストで具体値を assert するための内部用。公開エラーには
/// `Separated` 以外をすべて `SandboxProbeFailed` として写す）。
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NsProbe {
    SpawnFailed,
    Timeout,
    ExitFailure,
    OutputUnreadable,
    NotSeparated,
    Separated,
}

/// プローブ用の sandbox-exec 起動コマンド（profile は何も絞らない。`true_exec` は候補パスのまま）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_probe_argv(tool: &Path, true_exec: &Path) -> (PathBuf, Vec<OsString>) {
    let args = vec![
        OsString::from("-p"),
        OsString::from("(version 1)(allow default)"),
        true_exec.as_os_str().to_owned(),
    ];
    (tool.to_path_buf(), args)
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
/// `render_3_sec_2_uds_dir_is_created_by_host_process_0700_and_owned`（`host_process.rs`）が、
/// `spawn` と同じ部品を直接呼んで固定する（`spawn` 自体は通さない範囲は同テストの doc を参照）。
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
/// bwrap・sandbox-exec の選定（[`select_trusted`]）と、`true` の実体側の判定
/// （[`select_trusted_exec`]）が使う。
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

/// symlink を辿る段数の上限（超えたら信頼しない。ループ・過大なチェーンの fail-closed）。
#[cfg(unix)]
const MAX_SYMLINK_HOPS: usize = 40;

/// 候補パス側の symlink チェーンの信頼判定（純関数）。各段の祖先ディレクトリのメタデータ
/// `(is_dir, uid, mode)` の列を受け取り、すべてが [`trusted_dir_metadata`] を満たすときだけ
/// 真。段が 1 つも無い（証拠が無い）場合は偽（fail-closed。RENDER-3・#1458）。
#[cfg(unix)]
fn trusted_hops(hops: &[Vec<(bool, u32, u32)>]) -> bool {
    !hops.is_empty()
        && hops.iter().all(|hop| {
            hop.iter()
                .all(|&(is_dir, uid, mode)| trusted_dir_metadata(is_dir, uid, mode))
        })
}

/// 候補パスから symlink を 1 段ずつ辿り、各段で参照されるパスの字句上の祖先（symlink は辿った
/// 状態の属性）と、親を `canonicalize` した実体の祖先のメタデータを集める。exec のときに
/// カーネルが解決する symlink の置き場所（ユーザー所有の `/usr/local/bin` 等）を信頼判定に含めるため。
/// 上限超過・I/O エラーは `None`（呼び出し側で Untrusted に倒す）。
#[cfg(unix)]
fn collect_hops(path: &Path) -> Option<Vec<Vec<(bool, u32, u32)>>> {
    use std::os::unix::fs::MetadataExt;
    let dir_meta = |dir: &Path| {
        std::fs::metadata(dir)
            .ok()
            .map(|m| (m.is_dir(), m.uid(), m.mode()))
    };
    let mut cur = path.to_path_buf();
    let mut hops = Vec::new();
    for _ in 0..MAX_SYMLINK_HOPS {
        let parent = cur.parent()?.to_path_buf();
        let mut metas = Vec::new();
        for dir in cur.ancestors().skip(1) {
            metas.push(dir_meta(dir)?);
        }
        let canonical_parent = std::fs::canonicalize(&parent).ok()?;
        for dir in canonical_parent.ancestors() {
            metas.push(dir_meta(dir)?);
        }
        hops.push(metas);
        if !std::fs::symlink_metadata(&cur)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return Some(hops);
        }
        let target = std::fs::read_link(&cur).ok()?;
        cur = if target.is_absolute() {
            target
        } else {
            parent.join(target)
        };
    }
    None
}

/// 試走用 `true` の選定。[`select_trusted`] と違い、返すのは exec に使う**候補パス**そのもの
/// （multicall の argv[0] 対策。#1458）。信頼判定は、実体のチェーン（[`trusted_realpath`]）と、
/// 候補パス側の symlink チェーン（[`collect_hops`]・[`trusted_hops`]）の両方を満たすこと。
/// 存在しなければ `SandboxToolNotFound`、信頼できなければ `SandboxToolUntrusted`。
#[cfg(unix)]
fn select_trusted_exec(candidates: &[&str]) -> Result<PathBuf, EgressIsolationUnavailable> {
    let mut found_untrusted = false;
    for c in candidates {
        let path = Path::new(c);
        match trusted_realpath(path) {
            TrustCheck::Missing => {}
            TrustCheck::Untrusted => found_untrusted = true,
            TrustCheck::Trusted(_) => match collect_hops(path) {
                Some(hops) if trusted_hops(&hops) => return Ok(path.to_path_buf()),
                _ => found_untrusted = true,
            },
        }
    }
    Err(if found_untrusted {
        EgressIsolationUnavailable::SandboxToolUntrusted
    } else {
        EgressIsolationUnavailable::SandboxToolNotFound
    })
}

/// `cmd` を stdio なしで走らせ、`timeout` 内に成功終了したかを返す。超過したら kill して失敗。
/// macOS のプローブ用（Seatbelt には netns が無く、終了コードで判定する）。
#[cfg(target_os = "macos")]
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

/// `bwrap` で `readlink_exec <ns のリンク>` を本番と同じフラグで試走し、内側の名前空間 `ns` が
/// 本体と別かを判定する（RENDER-3・#1459・#1466）。信頼判定は呼び出し側（[`probe_uncached`]）が行い、
/// 本関数は渡されたパスをそのまま使う（テストが偽物の bwrap を渡せる）。
///
/// 試走は `env_clear` で走るが、本番の起動は許可リストの環境変数で走る。差が出るのは起動できない側
/// （失敗側）だけで、本番で隔離なしに起動できてしまう fail-open にはならない（設計 §9.4）。stdout は
/// [`PROBE_STDOUT_MAX`] バイトまで、終了後に読む（出力が小さくパイプの容量に収まるため
/// デッドロックしない。大量出力で詰まる場合は timeout で kill して失敗にする）。
#[cfg(target_os = "linux")]
fn probe_ns_with(
    bwrap: &Path,
    hidden: &HiddenPaths,
    readlink_exec: &Path,
    ns: Namespace,
    timeout: Duration,
) -> NsProbe {
    use std::io::Read;
    let Ok(outside) = std::fs::read_link(ns.link()) else {
        return NsProbe::OutputUnreadable;
    };
    let outside = outside.into_os_string().into_encoded_bytes();
    let (prog, args) = linux_probe_argv(bwrap, hidden, readlink_exec, ns.link());
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return NsProbe::SpawnFailed;
    };
    let deadline = Instant::now().checked_add(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if deadline.is_some_and(|d| Instant::now() < d) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return NsProbe::Timeout;
            }
        }
    };
    if !status.success() {
        return NsProbe::ExitFailure;
    }
    let mut buf = Vec::new();
    let Some(out) = child.stdout.take() else {
        return NsProbe::OutputUnreadable;
    };
    if out
        .take(PROBE_STDOUT_MAX.saturating_add(1))
        .read_to_end(&mut buf)
        .is_err()
        || u64::try_from(buf.len()).map_or(true, |n| n > PROBE_STDOUT_MAX)
    {
        return NsProbe::OutputUnreadable;
    }
    if ns_separated(ns, &outside, &buf) {
        NsProbe::Separated
    } else {
        NsProbe::NotSeparated
    }
}

/// 本番と同じフラグで、netns -> pid ns の順に**別々の bwrap 起動**で分離を確かめる（RENDER-3・#1466）。
/// 最初に分離できなかった名前空間とその結果を `Err` で返す。
///
/// 確かめるのは「別の名前空間にいる」ことまで。外部への出口が無いことは、その netns が本物の
/// bwrap の `--unshare-net` で作られ `lo` しか持たないという前提に依存し、本物であることは
/// root 所有の信頼判定（[`select_trusted`]）で担保する。
#[cfg(target_os = "linux")]
fn probe_bwrap_with(
    bwrap: &Path,
    hidden: &HiddenPaths,
    readlink_exec: &Path,
    timeout: Duration,
) -> Result<(), (Namespace, NsProbe)> {
    for ns in [Namespace::Net, Namespace::Pid] {
        match probe_ns_with(bwrap, hidden, readlink_exec, ns, timeout) {
            NsProbe::Separated => {}
            other => return Err((ns, other)),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn probe_uncached() -> Result<Sandbox, EgressIsolationUnavailable> {
    let bwrap = select_trusted(&BWRAP_CANDIDATES)?;
    let readlink_exec = select_trusted_exec(&READLINK_CANDIDATES)?;
    // `$HOME` はプローブ時に 1 回だけ読み、結果と一緒にキャッシュする（プロセスの生存中は変わらない前提。
    // 本体の環境変数は書き換えない。SEC-2）。エラーの優先順位は ツールの有無・信頼 → HOME → 試走。
    let hidden = HiddenPaths::new(resolve_home_to_hide(std::env::var_os("HOME").as_deref())?);
    // 実起動（`build_command`）は、ここでキャッシュした実体パスを `select_trusted` を通し直さず使う。
    // 実体は root 所有で祖先まで書き込み不可のまま、プロセスの生存中は変わらない前提に立つ
    // （差し替えには root が要り、脅威モデルの外。設計 §9.3）。
    match probe_bwrap_with(&bwrap, &hidden, &readlink_exec, PROBE_TIMEOUT) {
        Ok(()) => Ok(Sandbox::Bwrap { bwrap, hidden }),
        Err(_) => Err(EgressIsolationUnavailable::SandboxProbeFailed),
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
    let true_exec = select_trusted_exec(&TRUE_CANDIDATES)?;
    let (prog, args) = macos_probe_argv(&tool, &true_exec);
    let mut cmd = Command::new(prog);
    cmd.args(args);
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

    /// 0755 の通常ファイルを作り、root で実行している場合（作成直後の所有者が 0）は所有者を
    /// 非 root（nobody 相当の 65534）にして、`(is_file, uid, mode)` を返す。どの実行ユーザーでも
    /// 「root 以外が所有する 0755」を実ファイルで再現するため（失敗は panic。skip にしない）。
    #[cfg(unix)]
    fn make_non_root_owned_executable(f: &Path) -> (bool, u32, u32) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::write(f, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
        if std::fs::metadata(f).unwrap().uid() == 0 {
            std::os::unix::fs::chown(f, Some(65534), None).unwrap();
        }
        let m = std::fs::metadata(f).unwrap();
        assert_ne!(m.uid(), 0, "fixture must be owned by a non-root user");
        (m.is_file(), m.uid(), m.mode())
    }

    /// RENDER-3（#1451・#1458）: 実ファイルでも、root 以外が所有する 0755 のファイルは拒否される。
    /// `/tmp`（1777）の祖先が常に Untrusted の理由になるため、`trusted_realpath` だけでは所有者の
    /// 分岐を単独で確かめられない。そこで実ファイルのメタデータから `trusted_metadata` を直接評価し、
    /// uid だけを 0 に置き換えると受理されること（所有者が決め手であること）を示す
    /// （純関数 `render3_trusted_metadata_rejects_non_root_owner` が所有者の分岐の正本）。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_realpath_rejects_user_owned_file() {
        let dir = std::env::temp_dir().join(format!("fb-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fake-bwrap");
        let (is_file, uid, mode) = make_non_root_owned_executable(&f);
        assert!(!trusted_metadata(is_file, uid, mode));
        assert!(trusted_metadata(is_file, 0, mode));
        assert_eq!(trusted_realpath(&f), TrustCheck::Untrusted);
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

    /// CI（GitHub Actions の Linux）では標準配置であること自体を必須にする（`trusted_chain` が
    /// 壊れて常に false になる退行や、Err 分岐への空振りを CI で検出するため。大文字小文字を
    /// 区別する完全一致は `tests/host_process.rs` の `ci_requires_sandbox` と同じ）。
    #[cfg(unix)]
    fn ci_requires_ok() -> bool {
        cfg!(target_os = "linux") && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
    }

    /// 存在する最初の `TRUE_CANDIDATES` の実体パスと、その実体が標準配置（実体と全祖先が root 所有
    /// 0755 相当）かを返す。どれも無ければ各候補が Missing であることを assert して `None`
    /// （環境の事実を assert する。skip にしない）。
    #[cfg(unix)]
    fn existing_true_candidate() -> Option<(PathBuf, bool)> {
        use std::os::unix::fs::MetadataExt;
        for c in TRUE_CANDIDATES {
            if let Ok(real) = std::fs::canonicalize(c) {
                let m = std::fs::metadata(&real).unwrap();
                let ancestors: Vec<(bool, u32, u32)> = real
                    .ancestors()
                    .skip(1)
                    .map(|d| {
                        let dm = std::fs::metadata(d).unwrap();
                        (dm.is_dir(), dm.uid(), dm.mode())
                    })
                    .collect();
                let standard = trusted_chain((m.is_file(), m.uid(), m.mode()), &ancestors);
                if ci_requires_ok() {
                    assert!(
                        standard,
                        "CI must have a standard (root-owned) true: {real:?}"
                    );
                }
                return Some((real, standard));
            }
        }
        for c in TRUE_CANDIDATES {
            assert_eq!(trusted_realpath(Path::new(c)), TrustCheck::Missing, "{c}");
        }
        None
    }

    /// RENDER-3: ユーザー所有ディレクトリ内の symlink は、リンクの置き場所ではなく実体で判定され、
    /// 信頼できれば返るパスは実体になる。実体が信頼できないリンク（自作ファイル宛）は拒否される。
    /// 実体は存在する最初の `TRUE_CANDIDATES` を使う。標準配置（root 所有の祖先）でない環境
    /// （Nix 等）では、前提が崩れていることと Untrusted になることを assert する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_realpath_judges_symlink_by_target() {
        let dir = std::env::temp_dir().join(format!("fb-trust-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some((real_true, standard)) = existing_true_candidate() {
            let link = dir.join("bwrap");
            std::os::unix::fs::symlink(&real_true, &link).unwrap();
            if standard {
                assert_eq!(trusted_realpath(&link), TrustCheck::Trusted(real_true));
            } else {
                assert_eq!(trusted_realpath(&link), TrustCheck::Untrusted);
            }
        }

        let own = dir.join("own");
        std::fs::write(&own, b"#!/bin/sh\n").unwrap();
        let bad = dir.join("bad-link");
        std::os::unix::fs::symlink(&own, &bad).unwrap();
        assert_eq!(trusted_realpath(&bad), TrustCheck::Untrusted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RENDER-3: 候補が全て無ければ NotFound、あるが信頼できなければ Untrusted。後者は所有者の
    /// 分岐（root 以外所有の 0755）から来ていることを、`trusted_metadata` で示す。
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
        let (is_file, uid, mode) = make_non_root_owned_executable(&f);
        assert!(!trusted_metadata(is_file, uid, mode));
        assert!(trusted_metadata(is_file, 0, mode));
        assert_eq!(
            select_trusted(&[f.to_str().unwrap()]),
            Err(EgressIsolationUnavailable::SandboxToolUntrusted)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RENDER-3（#1458）: 候補パス側の symlink チェーンは、各段の祖先がすべて root 所有・
    /// group / other 書き込み不可のときだけ受理する。段が無い（証拠が無い）場合は拒否する。
    #[cfg(unix)]
    #[test]
    fn render3_trusted_hops_checks_every_hop_and_ancestor() {
        let dir = (true, 0, 0o755);
        assert!(trusted_hops(&[vec![dir, dir], vec![dir]]));
        // 空の列は fail-closed
        assert!(!trusted_hops(&[]));
        // 非 root 所有の祖先（中間の段）
        assert!(!trusted_hops(&[vec![dir], vec![dir, (true, 1000, 0o755)]]));
        // group / other 書き込み可
        assert!(!trusted_hops(&[vec![(true, 0, 0o775)]]));
        assert!(!trusted_hops(&[vec![dir], vec![(true, 0, 0o757)]]));
        // sticky つきの 1777（/tmp 相当）
        assert!(!trusted_hops(&[vec![(true, 0, 0o1777)]]));
    }

    /// RENDER-3（#1458）: 候補パスが「ユーザー所有ではない一時ディレクトリ内の symlink」のとき、
    /// 実体（`trusted_realpath`）が標準配置でも、置き場所の祖先（`/tmp` は 1777）が理由で
    /// `select_trusted_exec` は `SandboxToolUntrusted` にする。root でも非 root でも成り立つ
    /// （実体が標準配置でない環境でも Untrusted）。
    #[cfg(unix)]
    #[test]
    fn render3_select_trusted_exec_rejects_symlink_in_untrusted_dir() {
        let dir = std::env::temp_dir().join(format!("fb-trust-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some((real_true, _)) = existing_true_candidate() {
            let link = dir.join("true");
            std::os::unix::fs::symlink(&real_true, &link).unwrap();
            assert_eq!(
                select_trusted_exec(&[link.to_str().unwrap()]),
                Err(EgressIsolationUnavailable::SandboxToolUntrusted)
            );
        }
        assert_eq!(
            select_trusted_exec(&["/nonexistent-fb-dir/true"]),
            Err(EgressIsolationUnavailable::SandboxToolNotFound)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RENDER-3（#1458）: 標準配置の環境では、候補パスのまま（実体へ解決せず）返す。
    /// `/usr/bin/true` が symlink（`gnutrue`・multicall の `coreutils` 等）でも、返すのは候補パス。
    #[cfg(unix)]
    #[test]
    fn render3_select_trusted_exec_returns_candidate_path_not_realpath() {
        // Nix 等の非標準配置では Untrusted、候補が全く無ければ NotFound。Err でも空振りさせない
        for cands in [&TRUE_CANDIDATES[..], &READLINK_CANDIDATES[..]] {
            match select_trusted_exec(cands) {
                Ok(path) => assert!(
                    cands.iter().any(|c| Path::new(c) == path),
                    "must return a candidate path as-is: {path:?}"
                ),
                Err(e) => {
                    // CI では Ok を必須にする（Err 分岐は CI 以外だけ）
                    assert!(!ci_requires_ok(), "CI must trust {cands:?}: {e:?}");
                    assert!(
                        matches!(
                            e,
                            EgressIsolationUnavailable::SandboxToolUntrusted
                                | EgressIsolationUnavailable::SandboxToolNotFound
                        ),
                        "unexpected error: {e:?}"
                    );
                }
            }
        }
        // 存在する候補ごとに単独で判定し、両方が存在する環境でも各 symlink 判定を検証する
        for c in TRUE_CANDIDATES {
            if std::fs::symlink_metadata(c).is_err() {
                continue;
            }
            match select_trusted_exec(&[c]) {
                Ok(path) => assert_eq!(path, PathBuf::from(c)),
                Err(e) => assert_eq!(e, EgressIsolationUnavailable::SandboxToolUntrusted, "{c}"),
            }
        }
    }

    /// RENDER-3（#1458・#1459）: Linux のプローブ argv は本番と同じフラグ + 候補パスのまま（実体
    /// パスでない）+ netns のリンク。multicall では実体で exec すると argv[0] が変わり動かない。
    #[test]
    fn render3_linux_probe_argv_uses_candidate_path_as_argv0() {
        for ns in [Namespace::Net, Namespace::Pid] {
            let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
            let (prog, args) = linux_probe_argv(
                Path::new("/usr/bin/bwrap"),
                &hidden,
                Path::new("/usr/bin/readlink"),
                ns.link(),
            );
            assert_eq!(prog, PathBuf::from("/usr/bin/bwrap"));
            let got: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
            // 本番と同じ tmpfs を含み、bind は無い。
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
                    "--tmpfs",
                    "/tmp",
                    "--tmpfs",
                    "/home/u",
                    "--",
                    "/usr/bin/readlink",
                    ns.link(),
                ]
            );
        }
        assert_eq!(Namespace::Net.link(), "/proc/self/ns/net");
        assert_eq!(Namespace::Pid.link(), "/proc/self/ns/pid");
    }

    /// RENDER-3（#1459・#1466）: 名前空間の読み取りは `<種別>:[<数字>]`（改行 1 個まで）だけを受け付ける。
    /// 種別の取り違え（net を期待して pid が来る等）も `None`。
    #[test]
    fn render3_parse_ns_id_strict() {
        assert_eq!(
            parse_ns_id(Namespace::Net, b"net:[4026531833]\n"),
            Some(4026531833)
        );
        assert_eq!(parse_ns_id(Namespace::Net, b"net:[1]"), Some(1));
        assert_eq!(
            parse_ns_id(Namespace::Pid, b"pid:[4026531836]\n"),
            Some(4026531836)
        );
        assert_eq!(parse_ns_id(Namespace::Pid, b"pid:[7]"), Some(7));
        assert_eq!(parse_ns_id(Namespace::Net, b"pid:[1]"), None);
        assert_eq!(parse_ns_id(Namespace::Pid, b"net:[1]"), None);
        for ns in [Namespace::Net, Namespace::Pid] {
            let p = ns.prefix();
            let with = |tail: &str| format!("{p}{tail}").into_bytes();
            let bads: Vec<Vec<u8>> = vec![
                Vec::new(),
                with("]"),
                with("abc]"),
                b"mnt:[1]".to_vec(),
                format!(" {p}1]").into_bytes(),
                with("1]\n\n"),
                with("1] "),
                with("99999999999999999999999]"),
                with("+1]"),
                vec![0xff],
                vec![b'1'; 300],
            ];
            for bad in &bads {
                assert_eq!(parse_ns_id(ns, bad), None, "{ns:?} {bad:?}");
            }
        }
    }

    /// RENDER-3（#1459・#1466）: 外と中が異なる値のときだけ分離とみなす（net・pid とも）。
    #[test]
    fn render3_ns_separated_requires_different_ids() {
        assert!(ns_separated(Namespace::Net, b"net:[1]", b"net:[2]\n"));
        assert!(!ns_separated(Namespace::Net, b"net:[1]", b"net:[1]\n"));
        assert!(!ns_separated(Namespace::Net, b"net:[1]", b""));
        assert!(!ns_separated(Namespace::Net, b"garbage", b"net:[2]"));
        assert!(ns_separated(Namespace::Pid, b"pid:[1]", b"pid:[2]\n"));
        assert!(!ns_separated(Namespace::Pid, b"pid:[1]", b"pid:[1]\n"));
        assert!(!ns_separated(Namespace::Pid, b"pid:[1]", b"net:[2]"));
    }

    /// 0755 のシェルスクリプトを一時ディレクトリに作る。ETXTBSY（並列テストの fork が書き込み fd を
    /// 継ぐ既知の問題）は呼び出し側が `SpawnFailed` のやり直しで吸収する。
    #[cfg(target_os = "linux")]
    fn fake_script(tag: &str, body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        // 予測しにくい名前（pid・連番・ナノ秒）にし、`create_dir` で作る。既存（他者が先に作った
        // ディレクトリや symlink）ならエラーにして失敗させる（fail-closed。#1466）。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "fb-netns-{tag}-{}-{}-{nanos}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let f = dir.join("fake-bwrap");
        std::fs::write(&f, body).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, f)
    }

    #[cfg(target_os = "linux")]
    fn probe_retrying(bwrap: &Path, readlink: &Path) -> Result<(), (Namespace, NsProbe)> {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        let mut r = Err((Namespace::Net, NsProbe::SpawnFailed));
        for _ in 0..20 {
            r = probe_bwrap_with(bwrap, &hidden, readlink, Duration::from_secs(5));
            if !matches!(r, Err((_, NsProbe::SpawnFailed))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        r
    }

    /// RENDER-3（#1459）: 引数を無視して exit 0 するだけの偽 bwrap は、出力が無いので失敗する。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_probe_rejects_fake_bwrap_that_only_exits_zero() {
        let (dir, fake) = fake_script("exit0", "#!/bin/sh\nexit 0\n");
        let r = probe_retrying(&fake, Path::new("/usr/bin/readlink"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r, Err((Namespace::Net, NsProbe::NotSeparated)));
    }

    /// RENDER-3（#1459）: ns を作らず、内側のコマンドだけを実行する偽 bwrap は inode が同じで失敗する。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_probe_rejects_fake_bwrap_without_new_netns() {
        let Ok(readlink) = select_trusted_exec(&READLINK_CANDIDATES) else {
            return assert!(!ci_requires_ok(), "CI must trust readlink");
        };
        // 本番のフラグ列（`--` まで）を読み飛ばし、残り（readlink と引数）をそのまま exec する
        let body = "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n";
        let (dir, fake) = fake_script("nons", body);
        let r = probe_retrying(&fake, &readlink);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r, Err((Namespace::Net, NsProbe::NotSeparated)));
    }

    /// RENDER-3・RENDER-4（#1466）: netns だけ新しく見せ、pid ns を作らない偽 bwrap は Pid で失敗する。
    /// 本番のフラグ列を読み飛ばし、最後の引数が netns のリンクなら別の値を出し、それ以外は exec する。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_probe_rejects_fake_bwrap_without_new_pidns() {
        let Ok(readlink) = select_trusted_exec(&READLINK_CANDIDATES) else {
            return assert!(!ci_requires_ok(), "CI must trust readlink");
        };
        let body = "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nfor a in \"$@\"; do last=\"$a\"; done\nif [ \"$last\" = /proc/self/ns/net ]; then echo 'net:[1]'; exit 0; fi\nexec \"$@\"\n";
        let (dir, fake) = fake_script("nopid", body);
        let r = probe_retrying(&fake, &readlink);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r, Err((Namespace::Pid, NsProbe::NotSeparated)));
    }

    /// RENDER-3（#1459）: 本物の bwrap では netns が分離され、成功する。CI の Linux では bwrap と
    /// readlink が信頼判定を通ること自体を必須にする。bwrap が無い環境では NotFound を確かめて終える。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_probe_accepts_real_bwrap_with_separated_netns() {
        match (
            select_trusted(&BWRAP_CANDIDATES),
            select_trusted_exec(&READLINK_CANDIDATES),
        ) {
            (Ok(bwrap), Ok(readlink)) => {
                // 本番（probe_uncached）と同じく、実際の `$HOME` を隠した状態で分離を確かめる。
                let home = resolve_home_to_hide(std::env::var_os("HOME").as_deref());
                let Ok(home) = home else {
                    return assert!(!ci_requires_ok(), "CI must have a hideable HOME: {home:?}");
                };
                let hidden = HiddenPaths::new(home);
                let mut r = Err((Namespace::Net, NsProbe::SpawnFailed));
                for _ in 0..20 {
                    r = probe_bwrap_with(&bwrap, &hidden, &readlink, Duration::from_secs(5));
                    if !matches!(r, Err((_, NsProbe::SpawnFailed))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert_eq!(r, Ok(()));
            }
            (b, r) => {
                assert!(
                    !ci_requires_ok(),
                    "CI must have bwrap and readlink: {b:?} {r:?}"
                );
                for e in [b.err(), r.err()].into_iter().flatten() {
                    assert!(matches!(
                        e,
                        EgressIsolationUnavailable::SandboxToolNotFound
                            | EgressIsolationUnavailable::SandboxToolUntrusted
                    ));
                }
            }
        }
    }

    /// RENDER-3（#1458）: macOS のプローブ argv も候補パスのまま。
    #[test]
    fn render3_macos_probe_argv_uses_candidate_path_as_argv0() {
        let (prog, args) = macos_probe_argv(Path::new(SANDBOX_EXEC_PATH), Path::new("/bin/true"));
        assert_eq!(prog, PathBuf::from("/usr/bin/sandbox-exec"));
        let got: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(got, ["-p", "(version 1)(allow default)", "/bin/true"]);
    }

    fn argv_strs(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|a| a.to_str().unwrap()).collect()
    }

    /// RENDER-3・SEC-2（#1436・#1485）: Linux の argv は具体値と順序まで完全一致する。
    /// (a) ホストが `$HOME` 配下: tmpfs 3 つ -> `--bind` -> `--ro-bind`。
    #[test]
    fn render3_linux_argv_is_exact_with_exe_under_home() {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        let (prog, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            Path::new("/home/u/t/servo-host"),
            Path::new("/tmp/w/egress-proxy.sock"),
        )
        .unwrap();
        assert_eq!(prog, PathBuf::from("/usr/bin/bwrap"));
        let got = argv_strs(&args);
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
                "--tmpfs",
                "/tmp",
                "--tmpfs",
                "/home/u",
                "--bind",
                "/tmp/w",
                "/tmp/w",
                "--ro-bind",
                "/home/u/t/servo-host",
                "/home/u/t/servo-host",
                "--",
                "/home/u/t/servo-host",
                "--stdio",
                "--egress-relay-uds",
                "/tmp/w/egress-proxy.sock",
            ]
        );
        assert_bind_order(&got);
    }

    /// bind は最後の `--tmpfs` より後ろ、`--` より前に置く（順序が意味を持つ）。
    fn assert_bind_order(got: &[&str]) {
        let last_tmpfs = got.iter().rposition(|a| *a == "--tmpfs").unwrap();
        let sep = got.iter().position(|a| *a == "--").unwrap();
        for (i, a) in got.iter().enumerate() {
            if *a == "--bind" || *a == "--ro-bind" {
                assert!(i > last_tmpfs && i < sep, "{a} at {i}");
            }
        }
    }

    /// (b) ホストが隠れない場所（`/opt`）なら `--ro-bind` は付けない。
    #[test]
    fn render3_linux_argv_has_no_ro_bind_for_visible_exe() {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            Path::new("/opt/host/servo-host"),
            Path::new("/tmp/w/egress-proxy.sock"),
        )
        .unwrap();
        let got = argv_strs(&args);
        assert_eq!(
            &got[8..],
            [
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/tmp",
                "--tmpfs",
                "/home/u",
                "--bind",
                "/tmp/w",
                "/tmp/w",
                "--",
                "/opt/host/servo-host",
                "--stdio",
                "--egress-relay-uds",
                "/tmp/w/egress-proxy.sock",
            ]
        );
        assert!(!got.contains(&"--ro-bind"));
        assert_bind_order(&got);
    }

    /// (c) `$HOME` が `/tmp` 配下なら `--tmpfs <home>` を省き、`--tmpfs /tmp` は 1 回だけ。
    #[test]
    fn render3_linux_argv_omits_home_tmpfs_when_home_under_tmp() {
        let hidden = HiddenPaths::new(PathBuf::from("/tmp/h"));
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            Path::new("/tmp/h/servo-host"),
            Path::new("/tmp/w/egress-proxy.sock"),
        )
        .unwrap();
        let got = argv_strs(&args);
        assert_eq!(
            &got[8..],
            [
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/tmp",
                "--bind",
                "/tmp/w",
                "/tmp/w",
                "--ro-bind",
                "/tmp/h/servo-host",
                "/tmp/h/servo-host",
                "--",
                "/tmp/h/servo-host",
                "--stdio",
                "--egress-relay-uds",
                "/tmp/w/egress-proxy.sock",
            ]
        );
        assert_eq!(got.iter().filter(|a| **a == "/tmp").count(), 1);
        assert_bind_order(&got);
    }

    /// UDS が WorkDir の外だと隠れて届かないので argv を作らない（呼び出し側が起動失敗にする）。
    #[test]
    fn render3_linux_argv_rejects_uds_outside_workdir() {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        assert!(
            linux_argv(
                Path::new("/usr/bin/bwrap"),
                &hidden,
                Path::new("/tmp/w"),
                Path::new("/opt/host"),
                Path::new("/tmp/other/egress-proxy.sock"),
            )
            .is_none()
        );
    }

    /// RENDER-3（#1485）: `$HOME` の解決。未設定・相対・`/`・存在しない・ファイルは拒否する。
    #[cfg(unix)]
    #[test]
    fn render3_resolve_home_cases() {
        use std::ffi::OsStr;
        let bad = Err(EgressIsolationUnavailable::HomeNotHideable);
        assert_eq!(resolve_home_to_hide(None), bad);
        assert_eq!(resolve_home_to_hide(Some(OsStr::new("rel/x"))), bad);
        assert_eq!(resolve_home_to_hide(Some(OsStr::new(""))), bad);
        for root in ["/", "//", "/."] {
            assert_eq!(resolve_home_to_hide(Some(OsStr::new(root))), bad, "{root}");
        }

        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = base.join(format!("fb-home-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let link = dir.join("rootlink");
        std::os::unix::fs::symlink("/", &link).unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();

        assert_eq!(
            resolve_home_to_hide(Some(dir.join("absent").as_os_str())),
            bad
        );
        assert_eq!(resolve_home_to_hide(Some(file.as_os_str())), bad);
        assert_eq!(resolve_home_to_hide(Some(link.as_os_str())), bad);
        // `/tmp` 配下（macOS では /private/var 配下）でも Ok。正規化後の値が返る。
        assert_eq!(resolve_home_to_hide(Some(sub.as_os_str())), Ok(sub.clone()));
        // `/tmp` 配下の HOME は tmpfs が重複しない。
        if sub.starts_with("/tmp") {
            let h = HiddenPaths::new(sub.clone());
            assert_eq!(h.home, None);
            assert!(h.hides(&sub.join("child")));
        }
        let _ = std::fs::remove_dir_all(&dir);

        // 通常のディレクトリ（`/tmp` の外）は正規化されて Ok。
        let manifest = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        assert!(!manifest.starts_with("/tmp"), "fixture assumption");
        assert_eq!(
            resolve_home_to_hide(Some(manifest.as_os_str())),
            Ok(manifest.clone())
        );
        let h = HiddenPaths::new(manifest.clone());
        assert_eq!(h.home, Some(manifest));
    }

    /// RENDER-3（#1485）: 隠れる場所の判定は成分単位（`/tmpx` や `/home/ux` は隠れない）。
    #[test]
    fn render3_hidden_paths_hides() {
        let h = HiddenPaths::new(PathBuf::from("/home/u"));
        assert!(h.hides(Path::new("/tmp/x")));
        assert!(!h.hides(Path::new("/tmpx")));
        assert!(h.hides(Path::new("/home/u/a")));
        assert!(!h.hides(Path::new("/home/ux")));
        assert!(!h.hides(Path::new("/opt/x")));
        assert!(!h.hides(Path::new("/run/x")));
    }

    /// 試走用に `bwrap <wrapper> test <flag> <path>` を走らせ、終了コードを返す（失敗は `None`）。
    #[cfg(target_os = "linux")]
    fn run_bwrap_test(
        bwrap: &Path,
        hidden: &HiddenPaths,
        binds: &[(BindKind, &Path)],
        test_exec: &Path,
        flag: &str,
        target: &Path,
    ) -> Option<i32> {
        for _ in 0..20 {
            let mut args = linux_wrapper_args(hidden, binds);
            args.push(test_exec.as_os_str().to_owned());
            args.push(OsString::from(flag));
            args.push(target.as_os_str().to_owned());
            let mut cmd = Command::new(bwrap);
            cmd.args(args)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let Ok(mut child) = cmd.spawn() else {
                // ETXTBSY 等の一過性の起動失敗はやり直す。
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => return st.code(),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            }
        }
        None
    }

    /// RENDER-3・SEC-2（#1485）: 本物の bwrap で、`/tmp` の兄弟と `$HOME` の既存エントリが見えず、
    /// 戻した WorkDir（rw）とホスト実行ファイル（ro）だけが見える。CI の Linux では bwrap・readlink・
    /// test・`$HOME` が揃うこと自体を必須にする。揃わない環境では Err の種別を確かめて終える。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_bwrap_hides_tmp_and_home_and_restores_workdir_and_exe() {
        use std::os::unix::net::UnixListener;
        const TEST_CANDIDATES: [&str; 2] = ["/usr/bin/test", "/bin/test"];
        const TOUCH_CANDIDATES: [&str; 2] = ["/usr/bin/touch", "/bin/touch"];
        let tools = (
            select_trusted(&BWRAP_CANDIDATES),
            select_trusted_exec(&TEST_CANDIDATES),
            select_trusted_exec(&TOUCH_CANDIDATES),
            resolve_home_to_hide(std::env::var_os("HOME").as_deref()),
        );
        let (Ok(bwrap), Ok(test_exec), Ok(touch_exec), Ok(home)) = tools.clone() else {
            assert!(
                !ci_requires_ok(),
                "CI must have bwrap, test, touch and HOME: {tools:?}"
            );
            return;
        };
        let hidden = HiddenPaths::new(home.clone());

        // 兄弟ディレクトリ（`/tmp`）と WorkDir（`temp_dir` 配下。通常は `/tmp`）。
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let pid = std::process::id();
        let sibling = base.join(format!("fb-hide-sib-{pid}"));
        let work = base.join(format!("fb-hide-work-{pid}"));
        for d in [&sibling, &work] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir(d).unwrap();
        }
        let sib_file = sibling.join("f");
        let work_file = work.join("f");
        std::fs::write(&sib_file, b"x").unwrap();
        std::fs::write(&work_file, b"x").unwrap();
        let sib_sock = sibling.join("s.sock");
        let work_sock = work.join("s.sock");
        let _l1 = UnixListener::bind(&sib_sock).unwrap();
        let _l2 = UnixListener::bind(&work_sock).unwrap();

        let rw = [(BindKind::ReadWrite, work.as_path())];
        let run = |binds: &[(BindKind, &Path)], flag: &str, t: &Path| {
            run_bwrap_test(&bwrap, &hidden, binds, &test_exec, flag, t)
        };
        let mut checks = Vec::new();
        // `/tmp` が隠れる環境（既定）でだけ、兄弟が消えることを確かめる。
        let tmp_hidden = sibling.starts_with("/tmp") || hidden.hides(&sibling);
        if tmp_hidden {
            checks.push(("sibling file", run(&rw, "-e", &sib_file), Some(1)));
            checks.push(("sibling uds", run(&rw, "-S", &sib_sock), Some(1)));
        }
        checks.push(("workdir file", run(&rw, "-e", &work_file), Some(0)));
        checks.push(("workdir uds", run(&rw, "-S", &work_sock), Some(0)));
        // `$HOME` の既存エントリ（WorkDir と実行ファイルの祖先でないもの）は見えない。
        let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let entry = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| !exe.starts_with(p) && !work.starts_with(p));
        if let (Some(entry), true) = (entry.as_ref(), !home.starts_with("/tmp")) {
            checks.push(("home entry", run(&rw, "-e", entry), Some(1)));
        }
        // ホスト実行ファイル: 隠れる場所なら ro で戻り、書き込めない。
        let ro = [
            (BindKind::ReadWrite, work.as_path()),
            (BindKind::ReadOnly, exe.as_path()),
        ];
        let exe_hidden = hidden.hides(&exe);
        if exe_hidden {
            checks.push(("exe visible", run(&ro, "-e", &exe), Some(0)));
            // uutils の `test -w` は権限ビットしか見ず ro マウントを検出できないため、`touch -c`
            // （既存ファイルの時刻更新。ro マウントでは EROFS で失敗する）で読み取り専用を確かめる。
            let touch = run_bwrap_test(&bwrap, &hidden, &ro, &touch_exec, "-c", &exe);
            checks.push(("exe read-only", touch, Some(1)));
            checks.push(("exe hidden without bind", run(&rw, "-e", &exe), Some(1)));
        }

        for d in [&sibling, &work] {
            let _ = std::fs::remove_dir_all(d);
        }
        for (name, got, want) in checks {
            assert_eq!(got, want, "{name}");
        }
        if ci_requires_ok() {
            assert!(exe_hidden, "CI runs tests from under $HOME");
        }
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
            EgressIsolationUnavailable::HomeNotHideable,
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
        let h = isolation_warning_message(EgressIsolationUnavailable::HomeNotHideable);
        assert_eq!(
            h,
            "render host egress isolation is unavailable (home directory cannot be hidden by \
             the sandbox); starting render host without egress isolation"
        );
        assert!(!h.contains('/'));
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
