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
//!   [--size <N>] --tmpfs /run [--size <N>] --tmpfs /tmp [--size <N>] --tmpfs /var/tmp
//!   [--size <N>] --tmpfs /dev/shm [--size <N> --tmpfs <$HOME>]
//!   [--size <N> --tmpfs <WorkDir の親>] --bind <WorkDir> <WorkDir> [--ro-bind <ホスト> <ホスト>] -- <ホスト>
//!   --stdio --egress-relay-uds <UDS>`（`--ro-bind` はホストが `/run` 以外の tmpfs の配下にあるときだけ。
//!   `--tmpfs` の後に bind を置く順序が意味を持つ。`/tmp` 等は実体パスに解決し、存在しないものは外す）。
//!   netns に出口が無くなり、ホストは netns 内の中継を経由して WorkDir 配下の UDS（本体側の
//!   プロキシ）にだけ届く。`--unshare-pid` と
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
//!   `/var/tmp`・`/dev/shm`・本体の `$HOME`・WorkDir の親（tmpfs。#1485・#1492）で、戻すのはこの
//!   インスタンスの WorkDir（rw）と、隠れる場所にあるホストの実行ファイル（ro）だけ。WorkDir の親
//!   （`temp_root`）は兄弟 WorkDir（同じ uid の他インスタンスのプロキシ UDS）を見せないため常に隠す。
//!   既に隠れる場所の配下なら重複させず、`/` やシステムのディレクトリ（`/usr`・`/etc`・`/var` など。
//!   [`is_system_dir`]）なら覆えないので [`EgressIsolationUnavailable::WorkDirParentNotHideable`]。
//!   `/tmp` が `/var/tmp` への symlink などの環境では、隠す対象を実体パスで判定して重複を除く。
//! - tmpfs には容量の上限を付ける（#1498。[`TMPFS_SIZE_BYTES`] 1 本 64 MiB・最大 6 本で 384 MiB。
//!   根拠は定数の doc）。既定は 1 本あたり RAM の 50% で上限は mount ごとのため、侵害されたホストが
//!   本体マシンのメモリを消費しうるのを抑える。`--size`（upstream bubblewrap v0.7.0 で導入）が
//!   使えるかは版の文字列ではなく `bwrap --help` の機能検出で、プローブ時に決め
//!   （[`TmpfsLimit`]）、試走と本番が同じ引数になるよう [`HiddenPaths`] に持たせる。非対応の版（確定）は
//!   固定するが、検出の失敗のうち一過性のもの（起動の失敗・時間切れ）は固定せず、上限つき
//!   （[`TMPFS_REPROBE_MAX`]）で次のプローブ時に検出し直す（#1502）。上限は時間でなく回数で数え、
//!   `Err` に終わった再検出も 1 回に数える。使い切るとそのプロセスの間は上限なしに固定する（#1508）。
//!   非対応・検出失敗のどちらでも
//!   隔離をやめず上限なしで続け（黙って緩めない）、`HostProcess` が警告を理由ごとの文言で 1 回出す
//!   （`with_isolation_warning` のコールバック経由。未設定なら stderr）。残余リスク:
//!   `--size` は容量しか抑えず inode の数（`nr_inodes`。bwrap に指定手段が無い）は抑えないため、
//!   空ファイルを大量に作るとカーネルのメモリを使いうる。
//! - 隔離中は WebSocket が使えない（Servo の `websocket_loader` はプロキシを使わず直接接続する
//!   ため、遮断されて失敗する）。
//! - macOS の loopback プロキシは同一マシンの全ユーザー・全プロセスから届く（認証なし。Seatbelt
//!   はホスト側の送信だけを絞る）。宛先は許可リスト（公開 IP）に限られるので SSRF にはならないが、
//!   本体の送信元 IP を踏み台にされうる。UDS 化や接続トークンは、Servo のプロキシ設定が TCP
//!   （`http://127.0.0.1:<port>`）しか扱えず、macOS には netns が無く中継を置いても loopback の
//!   待ち受けが残るため、別途検討する（RENDER-3・#1451）。
//! - Linux で残る FS 上の UDS の迂回路: 隠している場所以外（`/dev/log`・`/var/lib/*`・`/var/snap/*` など
//!   の既知のソケット）。拒否リスト型の限界で、長期策（`--ro-bind` / Landlock）は保留中。抽象名前空間の
//!   ソケットは netns ごとに分かれるので対象外（#1457・#1485・#1492）。
//! - `$HOME` と `/tmp` が隠れるため、ユーザーのフォント（`~/.local/share/fonts`・`~/.fonts`）や
//!   `$HOME` の設定は見えない（`FONTCONFIG_FILE` 等が `$HOME` 配下を指していても同様）。
//! - `$HOME` が未設定・相対・システムのディレクトリ（`/` を含む）・存在しない・ディレクトリでないときは隠せないため
//!   [`EgressIsolationUnavailable::HomeNotHideable`]（`required` は拒否、`best-effort` は警告つきで
//!   隔離なし）。`$HOME` はプローブ時に 1 回だけ読みキャッシュする。本体の `HOME` も子へ強制で渡す
//!   `HOME` も書き換えない（SEC-2）。`$HOME` がシステムのディレクトリを指すとプローブか起動が失敗する
//!   が fail-closed で、fail-open にはならない。
//! - 隔離時の子の cwd と `HOME`・`TMPDIR`・`XDG_*` は正規化済みの WorkDir から作る（symlink 経由の `temp_root` でも戻したパスを指す）。`/run` 配下にある実行ファイルは戻さない。
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
use std::sync::{Mutex, PoisonError};
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
/// `bwrap --help` の stdout を読む上限（実物は約 5.4KB。超過は `--size` 非対応とみなす）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const HELP_STDOUT_MAX: u64 = 64 * 1024;
/// tmpfs 1 本あたりの容量上限（バイト。bwrap の `--size`。RENDER-3・SEC-2・#1498）。
///
/// 根拠: 子の `TMPDIR`・`HOME`・`XDG_*` は WorkDir（実ディスクを `--bind` で戻したもの）を指すため、
/// 正当な書き込みは tmpfs に入らない。入りうるのは `/dev/shm` の POSIX shm 程度と迷い込んだ
/// ファイルだけなので 64 MiB で足りる。最悪の合計は「tmpfs の本数（最大 6: `/run`・`/tmp`・
/// `/var/tmp`・`/dev/shm`・`$HOME`・WorkDir の親）x 64 MiB = 384 MiB」で、RSS の上限
/// （`DEFAULT_RSS_LIMIT_BYTES` 2 GiB）の約 19%。既定の上限（1 本あたり RAM の 50%）と違い RAM の量に
/// よらない。tmpfs のページは RSS の監視に計上されない（推測）ため、本数 x 上限で別に抑える。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const TMPFS_SIZE_BYTES: u64 = 64 * 1024 * 1024;

/// tmpfs の容量上限の扱い（プローブで決め、[`HiddenPaths`] に持たせる）。
///
/// 判定をプローブに集約し結果を `Sandbox::Bwrap` の中に持つので、プローブと本番の引数は構造上
/// 一致する（版の文字列は解析せず、`bwrap --help` に `--size` があるかという機能で検出する）。
/// 「版が非対応」は確定として固定し、「検出の失敗」は一過性なら後で検出し直す（#1502。
/// [`cached_probe`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) enum TmpfsLimit {
    /// 各 `--tmpfs` の直前に `--size <バイト数>` を付ける。
    Capped(u64),
    /// `--help` を読めて、そこに `--size`（upstream v0.7.0 で導入）が無い（版が非対応）。上限なし
    /// （既定の RAM の 50%）で隔離を続ける。隔離そのものを外す理由にはしない（メモリ上限が無い
    /// ことより悪くなるため）。確定した結果なので固定する。
    Unsupported,
    /// 検出に失敗した（`--help` を読めていない）。`--size` を付けない上限なしで隔離を続ける
    /// （付けると `--size` を知らない bwrap で試走が失敗し、隔離なしに落ちるため。#1502）。
    /// 一過性の失敗（[`TmpfsDetectFailure::is_transient`]）は上限つきで検出し直す。
    Undetected(TmpfsDetectFailure),
}

/// `bwrap --help` による `--size` 検出の失敗の種類（#1502）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) enum TmpfsDetectFailure {
    /// 起動できなかった（負荷・ETXTBSY など。一過性）。
    SpawnFailed(std::io::ErrorKind),
    /// 時間切れ（一過性）。
    TimedOut,
    /// 非 0 で終了した（繰り返しても変わらない）。
    ExitFailure,
    /// 出力が [`HELP_STDOUT_MAX`] を超えた（繰り返しても変わらない）。
    OutputTooLarge,
    /// stdout を取れない・読み取りに失敗した。
    OutputUnreadable,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl TmpfsDetectFailure {
    /// 検出し直す価値のある一過性の失敗か（起動の失敗・時間切れ）。
    pub(super) fn is_transient(self) -> bool {
        matches!(self, Self::SpawnFailed(_) | Self::TimedOut)
    }
}

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
    /// 作業ディレクトリの親（`temp_root` を正規化したもの）を tmpfs で隠せない。親が `/tmp`・
    /// `/var/tmp`・`/dev/shm`・`$HOME` の配下なら既に隠れているが、それ以外（例: `/mnt/x`）は親を
    /// tmpfs で覆って兄弟 WorkDir の UDS を塞ぐ。親が `/` やシステムのディレクトリ（`/usr`・`/etc`
    /// など）だと覆えないため、`required` は起動を拒否し、`best-effort` は警告つきで隔離なしで
    /// 起動する（RENDER-3・SEC-2・#1492）。回避策は `/tmp`・`/var/tmp`・`$HOME` の配下などを
    /// `temp_root`（`with_temp_root`）または `TMPDIR` に指定すること。
    WorkDirParentNotHideable,
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
            Self::WorkDirParentNotHideable => {
                "work directory's parent cannot be hidden by the sandbox"
            }
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

/// 固定で隠す場所の候補（`--tmpfs`。RENDER-3・#1492）。`/run` はリテラルのまま扱い
/// （[`workdir_hidden_by_sandbox`] と対応）、残りは [`resolve_fixed_targets`] で実体パスに解決する。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const FIXED_CANDIDATES: [&str; 4] = ["/run", "/tmp", "/var/tmp", "/dev/shm"];

/// 並べた `--tmpfs` の対象から、既に採用した対象の配下（成分単位）にあるものを捨て、後から来た
/// 祖先が先の子孫を覆うときは子孫を取り除く（祖先を残す）。順序は保つ。tmpfs の重複と、祖先の
/// tmpfs が子孫の tmpfs を消してしまう順序問題を避ける（RENDER-3・#1492）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn merge_tmpfs_targets(targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = Vec::new();
    for t in targets {
        if kept.iter().any(|k| t.starts_with(k)) {
            continue;
        }
        kept.retain(|k| !k.starts_with(t));
        kept.push(t.clone());
    }
    kept
}

/// 隠せないシステムのディレクトリか（正規化済みのパスで判定。RENDER-3・#1492・#1498）。
///
/// 配下も含めて真: `/proc`・`/sys`・`/usr`・`/etc`・`/bin`・`/sbin`・`/boot` と、先頭成分が `lib` で
/// 始まるもの（`/lib`・`/lib64` など。安全側に倒して `/libertine` も真）、`/dev` 配下（`/dev/pts`・
/// `/dev/mqueue` など。例外は固定の tmpfs 対象 `/dev/shm` とその配下で偽。`/devx` は成分が違うので
/// 対象外）。それ自身だけ真: `/`・`/var`（`/var/tmp` や `/var/lib/<service>` のサービスユーザーの
/// HOME を壊さないため。`/var/lib` 自体も `/var/lib/<service>` も偽のまま。`/var/lib` そのものを
/// 覆って壊れる場合は起動の失敗として表に出て、隔離なしには落ちない）。
///
/// `/private` の先頭成分は OS によらず外して判定する（macOS の `/etc` -> `/private/etc` 対応）。
/// 本番で効くのは Linux の bwrap の経路だけで、Linux で外しても `/private/usr` を真にする方向
/// （過剰に拒否する安全側）にしか働かないため、`cfg` では分けない。
fn is_system_dir(canonical: &Path) -> bool {
    let mut normals = canonical.components().filter_map(|c| match c {
        Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
        _ => None,
    });
    let mut first_opt = normals.next();
    // macOS では `/etc`・`/var` が `/private/etc`・`/private/var` への symlink で、正規化すると
    // 先頭に `private` が付く。判定では `/private` を外して Linux と同じ表で見る。
    if first_opt.as_deref() == Some("private") {
        first_opt = normals.next();
    }
    let Some(first) = first_opt else {
        return true; // `/`
    };
    let second = normals.next();
    if matches!(
        first.as_str(),
        "proc" | "sys" | "usr" | "etc" | "bin" | "sbin" | "boot"
    ) || first.starts_with("lib")
    {
        return true;
    }
    match first.as_str() {
        // `/dev/shm` は固定の tmpfs 対象なので、それ以外の `/dev` 配下だけを真にする。
        "dev" => second.as_deref() != Some("shm"),
        "var" => second.is_none(),
        _ => false,
    }
}

/// [`FIXED_CANDIDATES`] を実体パスへ解決する（`canon` は canonicalize の注入点。RENDER-3・#1492）。
///
/// `/run` はリテラルのまま。存在しない候補（`NotFound`）は UDS が無いので外す（`--dev-bind / /` の下で
/// 存在しない場所に `--tmpfs` を張ると bwrap が失敗するため）。それ以外のエラーと、システムの
/// ディレクトリに解決されるものは [`EgressIsolationUnavailable::SandboxProbeFailed`]（fail-closed）。
/// symlink 経由の重複（`/tmp -> /var/tmp` など）と `/run` 配下は [`merge_tmpfs_targets`] で除く。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn resolve_fixed_targets(
    canon: impl Fn(&Path) -> std::io::Result<PathBuf>,
) -> Result<Vec<PathBuf>, EgressIsolationUnavailable> {
    let mut out = Vec::new();
    for c in FIXED_CANDIDATES {
        let path = Path::new(c);
        if c == "/run" {
            out.push(path.to_path_buf());
            continue;
        }
        match canon(path) {
            Ok(real) if is_system_dir(&real) => {
                return Err(EgressIsolationUnavailable::SandboxProbeFailed);
            }
            Ok(real) => out.push(real),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(EgressIsolationUnavailable::SandboxProbeFailed),
        }
    }
    Ok(merge_tmpfs_targets(&out))
}

/// 実ファイルシステムで [`resolve_fixed_targets`] を解くときの canonicalize（ディレクトリでなければ
/// エラーにする）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn canonicalize_dir(path: &Path) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(path)?;
    if real.is_dir() {
        Ok(real)
    } else {
        Err(std::io::Error::other("not a directory"))
    }
}

/// bwrap が隠す対象（`--tmpfs`）。固定の `/run`・`/tmp`・`/var/tmp`・`/dev/shm`（正規化・重複除去済み）
/// と、プローブ時に 1 回だけ解決した正規化済みの `$HOME`（RENDER-3・SEC-2・#1485・#1492）。FS 上の
/// UDS（`/tmp/ssh-*/agent.*`・`/tmp/.X11-unix`・`~/.gnupg/S.*`・同じ uid の他インスタンスのプロキシ
/// UDS）が netns の外への迂回路になるのを塞ぐ。本体の `HOME` 環境変数は書き換えない（子へ強制で渡す
/// `HOME` も `env.rs` のまま）。WorkDir の親の tmpfs は起動ごとに決まるのでここには含めない
/// （[`socket_dir_for`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HiddenPaths {
    /// 解決済みの固定対象（先頭は `/run`）。
    fixed: Vec<PathBuf>,
    /// 正規化済みの `$HOME`。固定対象の配下なら既に隠れているので `None`（tmpfs の重複を省く）。
    home: Option<PathBuf>,
    /// 各 tmpfs の容量上限（プローブで検出。#1498）。
    tmpfs_limit: TmpfsLimit,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl HiddenPaths {
    /// 解決済みの固定対象（[`resolve_fixed_targets`]）と正規化済みの `$HOME`
    /// （[`resolve_home_to_hide`]）から作る。
    pub(super) fn from_resolved(
        fixed: Vec<PathBuf>,
        home_canonical: PathBuf,
        tmpfs_limit: TmpfsLimit,
    ) -> Self {
        let covered = fixed.iter().any(|f| home_canonical.starts_with(f));
        Self {
            fixed,
            home: if covered { None } else { Some(home_canonical) },
            tmpfs_limit,
        }
    }

    /// 各 tmpfs の容量上限。
    pub(super) fn tmpfs_limit(&self) -> TmpfsLimit {
        self.tmpfs_limit
    }

    /// テスト用: 容量上限を差し替える。
    #[cfg(test)]
    pub(super) fn with_tmpfs_limit(mut self, limit: TmpfsLimit) -> Self {
        self.tmpfs_limit = limit;
        self
    }

    /// テスト用: 固定対象を `/run`・`/tmp`・`/var/tmp`・`/dev/shm` として作る。
    #[cfg(test)]
    pub(super) fn new(home_canonical: PathBuf) -> Self {
        let fixed = FIXED_CANDIDATES.iter().map(PathBuf::from).collect();
        Self::from_resolved(fixed, home_canonical, TmpfsLimit::Capped(TMPFS_SIZE_BYTES))
    }

    /// `--tmpfs` を並べる順の対象（固定 -> `$HOME` -> WorkDir の親。祖先を残して重複除去）。
    pub(super) fn tmpfs_targets(&self, parent: Option<&Path>) -> Vec<PathBuf> {
        let mut v = self.fixed.clone();
        v.extend(self.home.clone());
        v.extend(parent.map(Path::to_path_buf));
        merge_tmpfs_targets(&v)
    }

    /// 正規化済みの `canonical` が（`/run` 以外の）tmpfs 対象の配下で隠れるか。ホストの実行ファイルを
    /// `--ro-bind` で戻す要否の判定に使う。`/run` 配下は含めない（従来どおり。#1485 のスコープ外）。
    pub(super) fn hides(&self, canonical: &Path, parent: Option<&Path>) -> bool {
        self.tmpfs_targets(parent)
            .iter()
            .filter(|t| t.as_path() != Path::new("/run"))
            .any(|t| canonical.starts_with(t))
    }
}

/// tmpfs に容量上限を付けられない理由（#1502）。隔離は続いている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TmpfsUnlimitedReason {
    /// bwrap が `--size` に対応していない版。
    SizeUnsupported,
    /// `--size` への対応を検出できなかった（一過性の失敗など）。
    DetectionFailed,
}

/// 隔離まわりの警告（`with_isolation_warning` のコールバックに届く。RENDER-3・#1502）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EgressIsolationWarning {
    /// `best-effort` で隔離なしに落ちた（従来の通知）。
    Unisolated(EgressIsolationUnavailable),
    /// 隔離は続くが tmpfs に容量上限を付けられない。
    TmpfsUnlimited(TmpfsUnlimitedReason),
}

/// 警告の文言（固定の英語。パスを含めない。RENDER-3・SEC-2・#1498・#1502）。
pub fn egress_isolation_warning_message(warning: EgressIsolationWarning) -> String {
    match warning {
        EgressIsolationWarning::Unisolated(reason) => isolation_warning_message(reason),
        EgressIsolationWarning::TmpfsUnlimited(reason) => tmpfs_limit_warning_message(reason),
    }
}

/// tmpfs に容量上限を付けられないときの警告文言（固定の英語。パスを含めない）。
pub(super) fn tmpfs_limit_warning_message(reason: TmpfsUnlimitedReason) -> String {
    match reason {
        TmpfsUnlimitedReason::SizeUnsupported => {
            "render host sandbox (bwrap) does not support --size; tmpfs mounts are not size-limited"
        }
        TmpfsUnlimitedReason::DetectionFailed => {
            "render host sandbox (bwrap) --size support could not be detected; tmpfs mounts are not size-limited"
        }
    }
    .to_string()
}

/// `$HOME` の生の値を、tmpfs で隠せる正規化済みパスへ解決する（RENDER-3・#1485・#1492）。
///
/// 絶対パスで、正規化でき、システムのディレクトリ（[`is_system_dir`]。`/` を含む）ではなく、
/// ディレクトリのときだけ `Ok`。それ以外（未設定・相対・存在しない・`/`〔`//`・`/.`・`/` を指す
/// symlink を含む〕・`/usr` など・ファイル）は [`EgressIsolationUnavailable::HomeNotHideable`]。
/// 環境変数を書き換えずにテストできるよう、値は引数で受け取る（edition 2024 の `set_var` は `unsafe`）。
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
    // 正規化の前後の両方で判定する（macOS の `/etc` -> `/private/etc` のように、symlink 越しで
    // 拒否リストから外れるのを防ぐ）。
    if is_system_dir(path) || is_system_dir(&canonical) || !canonical.is_dir() {
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
/// 同じにする（RENDER-3）。ただし WorkDir の親の tmpfs（`tmpfs` に含める）は起動ごとに決まるため
/// 本番だけが持つ。プローブに含めなくても、親を覆えない場合は起動の失敗として表に出る
/// （fail-closed。隔離なしで起動する経路は増えない。#1492）。
///
/// `--proc /proc` は `--dev-bind / /` の後ろに、`binds`（隠した後に戻す自分の WorkDir と
/// ホストの実行ファイル）は `--tmpfs` の**後ろ**に置く（順序が意味を持つ）。bind は起動ごとの
/// ものなのでプローブには付けない。`limit` が `Capped` なら各 `--tmpfs` の直前に `--size` を付ける。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_wrapper_args(
    tmpfs: &[PathBuf],
    limit: TmpfsLimit,
    binds: &[(BindKind, &Path)],
) -> Vec<OsString> {
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
    for t in tmpfs {
        // `--size` は直後の 1 本の `--tmpfs` にだけ効くため、対象ごとに並べる（#1498）。
        if let TmpfsLimit::Capped(n) = limit {
            args.push(OsString::from("--size"));
            args.push(OsString::from(n.to_string()));
        }
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
/// `workdir` は正規化済みの WorkDir（rw で戻す）、`parent_to_hide` は tmpfs で覆う WorkDir の親
/// （[`SocketDir::parent_to_hide`]。既に隠れているなら `None`。#1492）、`host` は正規化済みの
/// ホスト実行ファイル（隠れる場所にあるときだけ ro で戻す）。`relay_uds` が `workdir` の外だと隠れて届かないため `None`
/// （呼び出し側が起動失敗にする）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn linux_argv(
    bwrap: &Path,
    hidden: &HiddenPaths,
    workdir: &Path,
    parent_to_hide: Option<&Path>,
    host: &Path,
    relay_uds: &Path,
) -> Option<(PathBuf, Vec<OsString>)> {
    if !relay_uds.starts_with(workdir) {
        return None;
    }
    let mut binds = vec![(BindKind::ReadWrite, workdir)];
    if hidden.hides(host, parent_to_hide) {
        binds.push((BindKind::ReadOnly, host));
    }
    let mut args = linux_wrapper_args(
        &hidden.tmpfs_targets(parent_to_hide),
        hidden.tmpfs_limit(),
        &binds,
    );
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
    let mut args = linux_wrapper_args(&hidden.tmpfs_targets(None), hidden.tmpfs_limit(), &[]);
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

/// 中継用 UDS のパスが servo-host の規則（絶対・正規化済み・NUL なし・100 バイト以下）を満たすか。
/// ホストに拒否されて起動後に失敗するのを避けるため、起動前に検査する。絶対判定は `/` 始まりの
/// 文字列で行う（UDS は unix 専用で、Windows の `is_absolute` はドライブ指定を要するため）。
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

/// [`socket_dir_for`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SocketDir {
    /// 正規化済みの WorkDir（UDS を置くディレクトリ）。
    pub(super) dir: PathBuf,
    /// 起動時に tmpfs で覆う WorkDir の親。既に隠れる場所（`/tmp`・`/var/tmp`・`/dev/shm`・`$HOME` 配下）
    /// なら `None`。
    pub(super) parent_to_hide: Option<PathBuf>,
}

/// WorkDir の（正規化済みの）親について、tmpfs で覆う要否を決める（RENDER-3・SEC-2・#1492）。
///
/// 兄弟 WorkDir（同じ uid の他インスタンスのプロキシ UDS）を見えなくするため、親は常に隠れていなければ
/// ならない。親が既存の tmpfs 対象の配下なら `Ok(None)`。そうでなく、親が無い・`/`・システムの
/// ディレクトリなら覆えないので [`EgressIsolationUnavailable::WorkDirParentNotHideable`]。
/// それ以外は `Ok(Some(親))`。覆われているかの判定をシステムディレクトリの判定より先に行う
/// （`/var/tmp` などの正当な置き場所を拒否しないため）。
fn parent_to_hide(
    parent: Option<&Path>,
    hidden: &HiddenPaths,
) -> Result<Option<PathBuf>, EgressIsolationUnavailable> {
    let bad = EgressIsolationUnavailable::WorkDirParentNotHideable;
    let parent = parent.ok_or(bad)?;
    if hidden
        .tmpfs_targets(None)
        .iter()
        .any(|t| parent.starts_with(t))
    {
        return Ok(None);
    }
    if is_system_dir(parent) {
        return Err(bad);
    }
    Ok(Some(parent.to_path_buf()))
}

/// bwrap 用に、WorkDir を正規化してソケット用ディレクトリとして返す。`--tmpfs /run` に隠れる、
/// UDS のパスが規則に反する、または親を隠せない場合は隔離できない理由を返す（fail-closed）。
///
/// 返す `dir` は `HostProcess` がこの起動のために排他的に作った 0700 の WorkDir ルートで、
/// `EgressEndpoint::UnixSocketIn` の呼び出し側の契約を満たす（#1444・#1450）。契約はテスト
/// `render_3_sec_2_uds_dir_is_created_by_host_process_0700_and_owned`（`host_process.rs`）が、
/// `spawn` と同じ部品を直接呼んで固定する（`spawn` 自体は通さない範囲は同テストの doc を参照）。
/// `parent_to_hide` は `host_process.rs` の `build_command` が [`linux_argv`] へ渡す。
pub(super) fn socket_dir_for(
    workdir: &Path,
    hidden: &HiddenPaths,
) -> Result<SocketDir, EgressIsolationUnavailable> {
    let canonical = std::fs::canonicalize(workdir)
        .map_err(|_| EgressIsolationUnavailable::SocketPathInvalid)?;
    if workdir_hidden_by_sandbox(&canonical) {
        return Err(EgressIsolationUnavailable::WorkDirHiddenBySandbox);
    }
    relay_socket_path_ok(&canonical.join(EGRESS_SOCKET_NAME))?;
    let parent_to_hide = parent_to_hide(canonical.parent(), hidden)?;
    Ok(SocketDir {
        dir: canonical,
        parent_to_hide,
    })
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

/// `bwrap --help` の出力に `--size` オプションがあるか（空白区切りのトークンの完全一致。
/// `--sizes` などは不一致。RENDER-3・#1498）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn help_supports_tmpfs_size(stdout: &[u8]) -> bool {
    stdout
        .split(|b| b.is_ascii_whitespace())
        .any(|tok| tok == b"--size")
}

/// 時間切れで kill した後に読み取りスレッドの終了を待つ上限。孫が stdout を保持してパイプが閉じない
/// 場合に `join` で止まらないための値で、超えたらスレッドを detach する（#1508）。
#[cfg(target_os = "linux")]
const READER_GRACE: Duration = Duration::from_millis(200);

/// 信頼判定済みの `bwrap` が `--size` を持つかを `--help` で調べ、tmpfs の上限の扱いを決める。
/// 時間切れの後の読み取りスレッドは上限付きで待ち、孫がパイプを保持していれば detach する（#1508）。
/// `--help` を読めて `--size` が無ければ `Unsupported`（確定）。起動できない・非 0 終了・時間切れ・
/// 出力が [`HELP_STDOUT_MAX`] 超は `Undetected`（上限なしで隔離を続ける。隔離自体の可否はこの後の
/// 試走が判定する）。出力が上限を超えた時点で時間切れを待たずに kill する（#1502）。
#[cfg(target_os = "linux")]
fn detect_tmpfs_limit(bwrap: &Path, timeout: Duration) -> TmpfsLimit {
    use std::io::Read;
    let undetected = TmpfsLimit::Undetected;
    let mut cmd = Command::new(bwrap);
    cmd.arg("--help")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return undetected(TmpfsDetectFailure::SpawnFailed(e.kind())),
    };
    // 出力がパイプ容量を超えても bwrap が書き込みで止まらないよう、終了待ちと並行して別スレッドで
    // 上限付きに読む。読み取りスレッドが終わったら（上限超 or EOF）毎周回で結果を見て、上限超なら
    // 時間切れを待たずに kill する。
    let Some(out) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return undetected(TmpfsDetectFailure::OutputUnreadable);
    };
    let mut reader = Some(std::thread::spawn(move || {
        let mut buf = Vec::new();
        out.take(HELP_STDOUT_MAX.saturating_add(1))
            .read_to_end(&mut buf)
            .map(|_| buf)
    }));
    let mut buf: Option<Vec<u8>> = None;
    let deadline = Instant::now().checked_add(timeout);
    let status = loop {
        if reader.as_ref().is_some_and(|r| r.is_finished())
            && let Some(r) = reader.take()
        {
            match r.join() {
                Ok(Ok(b)) if u64::try_from(b.len()).is_ok_and(|n| n <= HELP_STDOUT_MAX) => {
                    buf = Some(b);
                }
                Ok(Ok(_)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return undetected(TmpfsDetectFailure::OutputTooLarge);
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return undetected(TmpfsDetectFailure::OutputUnreadable);
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if deadline.is_some_and(|d| Instant::now() < d) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                // 通常は kill でパイプが閉じ、読み取りスレッドは EOF で終わる。ただし子が stdout を
                // 孫へ受け継がせているとパイプが閉じず join が戻らないため、上限付きで終了を待ち、
                // 過ぎたら detach する（#1508）。detach したスレッドが持つのは読み端と上限付き
                // バッファ（`HELP_STDOUT_MAX + 1`）だけで、孫の終了で終わる。数は検出の回数
                // （初回＋`TMPFS_REPROBE_MAX`）で抑えられる。本物の bwrap の `--help` に孫はいない。
                if let Some(r) = reader.take() {
                    let grace_end = Instant::now() + READER_GRACE;
                    while !r.is_finished() && Instant::now() < grace_end {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if r.is_finished() {
                        let _ = r.join();
                    }
                    // 未終了なら `r` を drop して detach する。
                }
                return undetected(TmpfsDetectFailure::TimedOut);
            }
        }
    };
    let buf = match (buf, reader.take()) {
        (Some(b), _) => b,
        (None, Some(r)) => match r.join() {
            Ok(Ok(b)) => b,
            _ => return undetected(TmpfsDetectFailure::OutputUnreadable),
        },
        (None, None) => return undetected(TmpfsDetectFailure::OutputUnreadable),
    };
    if !status.success() {
        return undetected(TmpfsDetectFailure::ExitFailure);
    }
    if u64::try_from(buf.len()).map_or(true, |n| n > HELP_STDOUT_MAX) {
        return undetected(TmpfsDetectFailure::OutputTooLarge);
    }
    if help_supports_tmpfs_size(&buf) {
        TmpfsLimit::Capped(TMPFS_SIZE_BYTES)
    } else {
        TmpfsLimit::Unsupported
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
    // 本体の環境変数は書き換えない。SEC-2）。エラーの優先順位は ツールの有無・信頼 → HOME →
    // 固定対象（`/tmp` 等の解決）→ 試走。WorkDir の親の tmpfs は起動ごとなのでここには含めない（#1492）。
    let home = resolve_home_to_hide(std::env::var_os("HOME").as_deref())?;
    let fixed = resolve_fixed_targets(canonicalize_dir)?;
    // `--size` の対応はここで検出し、試走と本番が同じ引数になるよう `hidden` に持たせる。
    // 起動の失敗（負荷など。すぐ終わる）だけはその場で 1 回やり直す。時間切れは起動の遅延を増やさない
    // よう次の呼び出しでの再プローブ（[`cached_probe`]）に回す（#1502）。
    let mut limit = detect_tmpfs_limit(&bwrap, PROBE_TIMEOUT);
    if matches!(
        limit,
        TmpfsLimit::Undetected(TmpfsDetectFailure::SpawnFailed(_))
    ) {
        limit = detect_tmpfs_limit(&bwrap, PROBE_TIMEOUT);
    }
    let hidden = HiddenPaths::from_resolved(fixed, home, limit);
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

/// 検出の失敗（一過性）の後に `--size` の検出をやり直す回数の上限（初回を除く。#1502）。
/// 時間では区切らず回数で数える（`Err` に終わった再検出も消費する）。使い切った後は
/// `Undetected` のまま、そのプロセスの間は上限なしに固定する（警告は出す。#1508）。
const TMPFS_REPROBE_MAX: u32 = 3;

/// プローブ結果のキャッシュ（再プローブの回数つき）。
#[derive(Default)]
struct ProbeCache {
    result: Option<Result<Sandbox, EgressIsolationUnavailable>>,
    reprobes: u32,
}

/// tmpfs の上限の検出が一過性の失敗で終わった `Ok` か。
fn has_transient_tmpfs_failure(r: &Result<Sandbox, EgressIsolationUnavailable>) -> bool {
    matches!(
        r,
        Ok(Sandbox::Bwrap { hidden, .. })
            if matches!(hidden.tmpfs_limit(), TmpfsLimit::Undetected(f) if f.is_transient())
    )
}

/// キャッシュの判断（注入した `probe` で試験できるよう切り出す）。未取得なら `probe` の結果を保存
/// して返す。保存済みが一過性の検出失敗なら、上限（[`TMPFS_REPROBE_MAX`]）まで `probe`（試走を含む
/// 全体なので、`Capped` は `--size` つきの引数で試走を通したときだけ採用される）をやり直す。再プローブが
/// `Err` でも前の `Ok` を残し、隔離できていた状態を隔離なしへ落とさない。`$HOME` は再プローブのたびに
/// 読み直すが、試走と本番の `hidden` は同じ結果から作られるので整合は崩れない。
fn cached_probe(
    cache: &mut ProbeCache,
    mut probe: impl FnMut() -> Result<Sandbox, EgressIsolationUnavailable>,
) -> Result<Sandbox, EgressIsolationUnavailable> {
    match &cache.result {
        None => {
            let r = probe();
            cache.result = Some(r.clone());
            r
        }
        Some(prev) if has_transient_tmpfs_failure(prev) && cache.reprobes < TMPFS_REPROBE_MAX => {
            let prev = prev.clone();
            cache.reprobes += 1;
            let r = probe();
            if r.is_ok() {
                cache.result = Some(r.clone());
                r
            } else {
                prev
            }
        }
        Some(prev) => prev.clone(),
    }
}

pub(super) fn probe_sandbox() -> Result<Sandbox, EgressIsolationUnavailable> {
    static CACHE: Mutex<ProbeCache> = Mutex::new(ProbeCache {
        result: None,
        reprobes: 0,
    });
    let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cached_probe(&mut guard, probe_uncached)
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
            let hidden = HiddenPaths::new(PathBuf::from("/home/u"))
                .with_tmpfs_limit(TmpfsLimit::Unsupported);
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
                    "/var/tmp",
                    "--tmpfs",
                    "/dev/shm",
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
        probe_retrying_with(bwrap, readlink, &HiddenPaths::new(PathBuf::from("/home/u")))
    }

    /// `detect_tmpfs_limit` を、fake の書き込み直後の ETXTBSY（`ExecutableFileBusy`）に限って
    /// やり直して呼ぶ。他の失敗は即返す（`Capped` を隠さない。#1502）。
    #[cfg(target_os = "linux")]
    fn detect_retrying(bwrap: &Path, timeout: Duration) -> TmpfsLimit {
        let mut limit = detect_tmpfs_limit(bwrap, timeout);
        for _ in 0..20 {
            if limit
                != TmpfsLimit::Undetected(TmpfsDetectFailure::SpawnFailed(
                    std::io::ErrorKind::ExecutableFileBusy,
                ))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            limit = detect_tmpfs_limit(bwrap, timeout);
        }
        limit
    }

    #[cfg(target_os = "linux")]
    fn probe_retrying_with(
        bwrap: &Path,
        readlink: &Path,
        hidden: &HiddenPaths,
    ) -> Result<(), (Namespace, NsProbe)> {
        let mut r = Err((Namespace::Net, NsProbe::SpawnFailed));
        for _ in 0..20 {
            r = probe_bwrap_with(bwrap, hidden, readlink, Duration::from_secs(5));
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
                let hidden =
                    HiddenPaths::new(home).with_tmpfs_limit(detect_retrying(&bwrap, PROBE_TIMEOUT));
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
        let hidden =
            HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(TmpfsLimit::Unsupported);
        let (prog, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            None,
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
                "/var/tmp",
                "--tmpfs",
                "/dev/shm",
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
        let hidden =
            HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(TmpfsLimit::Unsupported);
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            None,
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
                "/var/tmp",
                "--tmpfs",
                "/dev/shm",
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
        let hidden =
            HiddenPaths::new(PathBuf::from("/tmp/h")).with_tmpfs_limit(TmpfsLimit::Unsupported);
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            None,
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
                "--tmpfs",
                "/var/tmp",
                "--tmpfs",
                "/dev/shm",
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
                None,
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
            assert!(h.hides(&sub.join("child"), None));
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
        assert!(h.hides(Path::new("/tmp/x"), None));
        assert!(!h.hides(Path::new("/tmpx"), None));
        assert!(h.hides(Path::new("/var/tmp/x"), None));
        assert!(h.hides(Path::new("/dev/shm/x"), None));
        assert!(!h.hides(Path::new("/var/tmpx"), None));
        assert!(h.hides(Path::new("/home/u/a"), None));
        assert!(!h.hides(Path::new("/home/ux"), None));
        assert!(!h.hides(Path::new("/opt/x"), None));
        assert!(!h.hides(Path::new("/run/x"), None));
        // WorkDir の親の配下も隠れる（#1492）。
        let parent = Path::new("/mnt/x");
        assert!(h.hides(Path::new("/mnt/x/host"), Some(parent)));
        assert!(!h.hides(Path::new("/mnt/x/host"), None));
        assert!(!h.hides(Path::new("/mnt/xy/host"), Some(parent)));
    }

    fn pb(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    /// RENDER-3（#1492）: 親を隠すと `--tmpfs` が固定 -> `$HOME` -> 親の順に出て、bind はその後ろ。
    /// WorkDir の親が `/mnt/x`（どの tmpfs にも覆われない）の場合。
    #[test]
    fn render3_linux_argv_hides_uncovered_workdir_parent() {
        let hidden =
            HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(TmpfsLimit::Unsupported);
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/mnt/x/w"),
            Some(Path::new("/mnt/x")),
            Path::new("/mnt/x/host"),
            Path::new("/mnt/x/w/egress-proxy.sock"),
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
                "/var/tmp",
                "--tmpfs",
                "/dev/shm",
                "--tmpfs",
                "/home/u",
                "--tmpfs",
                "/mnt/x",
                "--bind",
                "/mnt/x/w",
                "/mnt/x/w",
                // ホストも親の配下なので ro で戻す。
                "--ro-bind",
                "/mnt/x/host",
                "/mnt/x/host",
                "--",
                "/mnt/x/host",
                "--stdio",
                "--egress-relay-uds",
                "/mnt/x/w/egress-proxy.sock",
            ]
        );
        assert_bind_order(&got);
    }

    /// RENDER-3（#1492）: `/var/tmp`・`/dev/shm` 配下のホストは `--ro-bind` で戻し、`/opt` は戻さない。
    /// 親が既に隠れているなら（`parent_to_hide` が `None`）親の `--tmpfs` は出ない。
    #[test]
    fn render3_linux_argv_ro_binds_host_under_var_tmp_and_dev_shm() {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        for host in ["/var/tmp/h/servo-host", "/dev/shm/h/servo-host"] {
            let (_, args) = linux_argv(
                Path::new("/usr/bin/bwrap"),
                &hidden,
                Path::new("/var/tmp/w"),
                None,
                Path::new(host),
                Path::new("/var/tmp/w/egress-proxy.sock"),
            )
            .unwrap();
            let got = argv_strs(&args);
            assert!(got.contains(&"--ro-bind"), "{host}");
            assert_eq!(got.iter().filter(|a| **a == "/var/tmp").count(), 1);
            assert_eq!(got.iter().filter(|a| **a == "--tmpfs").count(), 5);
            // 既定（Capped）では --tmpfs ごとに --size が付く（#1498）。
            assert_eq!(got.iter().filter(|a| **a == "--size").count(), 5);
            assert_bind_order(&got);
        }
    }

    /// RENDER-3（#1492）: `temp_root=/home`・`HOME=/home/u` のように祖先が来るときは祖先だけを残す。
    #[test]
    fn render3_linux_argv_keeps_only_ancestor_tmpfs() {
        let hidden =
            HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(TmpfsLimit::Unsupported);
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/home/w"),
            Some(Path::new("/home")),
            Path::new("/opt/host"),
            Path::new("/home/w/egress-proxy.sock"),
        )
        .unwrap();
        let got = argv_strs(&args);
        assert_eq!(
            &got[8..16],
            [
                "--tmpfs", "/run", "--tmpfs", "/tmp", "--tmpfs", "/var/tmp", "--tmpfs", "/dev/shm",
            ]
        );
        assert_eq!(&got[16..18], ["--tmpfs", "/home"]);
        assert_eq!(&got[18..20], ["--bind", "/home/w"]);
        assert!(!got.contains(&"/home/u"));
    }

    /// RENDER-3（#1492）: tmpfs 対象の併合は、重複・子孫を捨て、後から来た祖先が先の子孫を消す。
    /// 判定は成分単位（`/tmpx` は `/tmp` の配下ではない）。
    #[test]
    fn render3_merge_tmpfs_targets_cases() {
        assert_eq!(
            merge_tmpfs_targets(&pb(&["/run", "/tmp", "/tmp", "/tmp/a", "/tmpx"])),
            pb(&["/run", "/tmp", "/tmpx"])
        );
        assert_eq!(
            merge_tmpfs_targets(&pb(&["/home/u", "/tmp", "/home"])),
            pb(&["/tmp", "/home"])
        );
        assert_eq!(merge_tmpfs_targets(&[]), Vec::<PathBuf>::new());
    }

    /// RENDER-3（#1492）: 固定対象の解決。`/tmp -> /var/tmp` は 1 本になり、`/dev/shm -> /run/shm` は
    /// `/run` に含まれて消え、存在しない候補は外す。それ以外のエラーとシステムのディレクトリへの
    /// 解決は `SandboxProbeFailed`。
    #[test]
    fn render3_resolve_fixed_targets_cases() {
        use std::io::{Error, ErrorKind};
        let ok = |m: &'static [(&'static str, &'static str)]| {
            move |p: &Path| {
                m.iter()
                    .find(|(k, _)| Path::new(k) == p)
                    .map(|(_, v)| PathBuf::from(v))
                    .ok_or_else(|| Error::from(ErrorKind::NotFound))
            }
        };
        assert_eq!(
            resolve_fixed_targets(ok(&[
                ("/tmp", "/tmp"),
                ("/var/tmp", "/var/tmp"),
                ("/dev/shm", "/dev/shm")
            ])),
            Ok(pb(&["/run", "/tmp", "/var/tmp", "/dev/shm"]))
        );
        assert_eq!(
            resolve_fixed_targets(ok(&[
                ("/tmp", "/var/tmp"),
                ("/var/tmp", "/var/tmp"),
                ("/dev/shm", "/run/shm")
            ])),
            Ok(pb(&["/run", "/var/tmp"]))
        );
        // 存在しない候補は外す。
        assert_eq!(
            resolve_fixed_targets(ok(&[("/tmp", "/tmp")])),
            Ok(pb(&["/run", "/tmp"]))
        );
        // NotFound 以外のエラー。
        let denied = |_: &Path| Err(Error::from(ErrorKind::PermissionDenied));
        assert_eq!(
            resolve_fixed_targets(denied),
            Err(EgressIsolationUnavailable::SandboxProbeFailed)
        );
        // システムのディレクトリへ解決される場合。
        assert_eq!(
            resolve_fixed_targets(ok(&[("/tmp", "/usr/tmp")])),
            Err(EgressIsolationUnavailable::SandboxProbeFailed)
        );
    }

    /// RENDER-3（#1492・#1498）: システムのディレクトリの判定。`/var` はそれ自身だけ真、`/dev` は
    /// `/dev/shm`（固定の tmpfs 対象）とその配下を除いて配下も真、`/private` の先頭成分は外す。
    #[test]
    fn render3_is_system_dir_cases() {
        for t in [
            "/",
            "/usr",
            "/usr/local/x",
            "/etc/x",
            "/bin",
            "/sbin/x",
            "/boot",
            "/lib",
            "/lib64",
            "/libx32/a",
            "/libertine",
            "/proc/1",
            "/sys",
            "/dev",
            "/dev/pts",
            "/dev/mqueue",
            "/dev/x/y",
            "/var",
            "/private/usr",
        ] {
            assert!(is_system_dir(Path::new(t)), "{t}");
        }
        for f in [
            "/var/tmp",
            "/var/lib",
            "/var/lib/svc",
            "/dev/shm",
            "/dev/shm/a",
            "/devx",
            "/private/x",
            "/home/u",
            "/usr2",
            "/mnt/x",
            "/opt",
            "/tmp",
        ] {
            assert!(!is_system_dir(Path::new(f)), "{f}");
        }
    }

    /// RENDER-3（#1492）: 親の判定。覆われていれば `None`、覆われていない通常の場所は `Some`、
    /// 親なし・`/`・システムのディレクトリは `WorkDirParentNotHideable`。覆われているかを先に見る。
    #[test]
    fn render3_parent_to_hide_cases() {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        for covered in ["/tmp", "/var/tmp", "/dev/shm", "/home/u", "/home/u/x/y"] {
            assert_eq!(
                parent_to_hide(Some(Path::new(covered)), &hidden),
                Ok(None),
                "{covered}"
            );
        }
        assert_eq!(
            parent_to_hide(Some(Path::new("/mnt/x")), &hidden),
            Ok(Some(PathBuf::from("/mnt/x")))
        );
        // `/home/ux` は `$HOME`（/home/u）の配下ではない。
        assert_eq!(
            parent_to_hide(Some(Path::new("/home/ux")), &hidden),
            Ok(Some(PathBuf::from("/home/ux")))
        );
        let bad = Err(EgressIsolationUnavailable::WorkDirParentNotHideable);
        assert_eq!(parent_to_hide(None, &hidden), bad);
        for sys in [
            "/",
            "/usr",
            "/usr/local",
            "/etc",
            "/dev",
            "/dev/pts",
            "/var",
            "/proc",
        ] {
            assert_eq!(parent_to_hide(Some(Path::new(sys)), &hidden), bad, "{sys}");
        }
    }

    /// RENDER-3（#1492）: 実 FS の WorkDir で `socket_dir_for` が親の要否を返す。`temp_dir` 配下は
    /// 既に隠れている（`None`）。`$HOME` にも固定対象にも覆われない場所では `Some(親)`。
    #[cfg(unix)]
    #[test]
    fn render3_socket_dir_for_reports_parent_to_hide() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = base.join(format!("fb-sd-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let work = dir.join("w");
        std::fs::create_dir(&work).unwrap();
        // 固定対象に `temp_dir` を含めない。`$HOME` も別の場所にして、覆われない状況を作る。
        let uncovered = HiddenPaths::from_resolved(
            pb(&["/run"]),
            PathBuf::from("/nonexistent-home"),
            TmpfsLimit::Capped(TMPFS_SIZE_BYTES),
        );
        let got = socket_dir_for(&work, &uncovered);
        // 覆われる場合: 親 `dir` 自体が固定対象。
        let covered = HiddenPaths::from_resolved(
            vec![PathBuf::from("/run"), dir.clone()],
            PathBuf::from("/nonexistent-home"),
            TmpfsLimit::Capped(TMPFS_SIZE_BYTES),
        );
        let got_covered = socket_dir_for(&work, &covered);
        let _ = std::fs::remove_dir_all(&dir);
        // パス長の制限で SocketPathInvalid になる環境（macOS の長い temp_dir）は対象外。
        if let Ok(sd) = got {
            assert_eq!(sd.dir, work);
            assert_eq!(sd.parent_to_hide, Some(dir.clone()));
            let sd2 = got_covered.unwrap();
            assert_eq!(sd2.parent_to_hide, None);
        } else {
            assert_eq!(got, Err(EgressIsolationUnavailable::SocketPathInvalid));
        }
    }

    /// RENDER-3（#1492）: `$HOME` がシステムのディレクトリなら `HomeNotHideable`。
    #[cfg(unix)]
    #[test]
    fn render3_resolve_home_rejects_system_dirs() {
        use std::ffi::OsStr;
        let bad = Err(EgressIsolationUnavailable::HomeNotHideable);
        for d in ["/usr", "/etc", "/proc", "/sys", "/dev", "/dev/pts", "/var"] {
            if Path::new(d).is_dir() {
                assert_eq!(resolve_home_to_hide(Some(OsStr::new(d))), bad, "{d}");
            }
        }
        // `/var/tmp` はシステムのディレクトリではなく、固定対象の配下として覆われる。
        if let Ok(canon) = std::fs::canonicalize("/var/tmp") {
            assert_eq!(
                resolve_home_to_hide(Some(OsStr::new("/var/tmp"))),
                Ok(canon.clone())
            );
            // macOS では `/var/tmp` が `/private/var/tmp` になるため、固定対象も実体パスで作る。
            let hidden = HiddenPaths::from_resolved(
                vec![canon.clone()],
                canon,
                TmpfsLimit::Capped(TMPFS_SIZE_BYTES),
            );
            assert_eq!(hidden.home, None);
        }
    }

    /// RENDER-3（#1492）: macOS の実体パス（`/private/etc`・`/private/var`）もシステムのディレクトリ。
    /// `/private/var/folders/..`（macOS の一時領域）は配下なので対象外。
    #[test]
    fn render3_is_system_dir_handles_macos_private_prefix() {
        assert!(is_system_dir(Path::new("/private/etc")));
        assert!(is_system_dir(Path::new("/private/etc/ssh")));
        assert!(is_system_dir(Path::new("/private/var")));
        assert!(!is_system_dir(Path::new("/private/var/folders/ab/cd")));
        assert!(!is_system_dir(Path::new("/private/tmp")));
        assert!(!is_system_dir(Path::new("/Users/alice")));
    }

    /// 試走用に `bwrap <wrapper> test <flag> <path>` を走らせ、終了コードを返す（失敗は `None`）。
    #[cfg(target_os = "linux")]
    fn run_bwrap_test(
        bwrap: &Path,
        tmpfs: &[PathBuf],
        binds: &[(BindKind, &Path)],
        test_exec: &Path,
        flag: &str,
        target: &Path,
    ) -> Option<i32> {
        for _ in 0..20 {
            // 実物の bwrap が対応していれば `--size` 付きで、非対応なら無しで走らせる（本番と同じ判定）。
            let mut args = linux_wrapper_args(tmpfs, detect_retrying(bwrap, PROBE_TIMEOUT), binds);
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
        // 本番（probe_uncached）と同じく、実環境の `/tmp`・`/var/tmp`・`/dev/shm` を解決して使う。
        let fixed = resolve_fixed_targets(canonicalize_dir).unwrap();
        let hidden =
            HiddenPaths::from_resolved(fixed, home.clone(), TmpfsLimit::Capped(TMPFS_SIZE_BYTES));
        let tmpfs = hidden.tmpfs_targets(None);

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
            run_bwrap_test(&bwrap, &tmpfs, binds, &test_exec, flag, t)
        };
        let mut checks = Vec::new();
        // `temp_dir` 配下の兄弟が消えること。CI では隠れることを必須にし、空振りさせない。
        let tmp_hidden = hidden.hides(&sibling, None);
        if ci_requires_ok() {
            assert!(tmp_hidden, "CI temp_dir must be under a hidden path");
        }
        if tmp_hidden {
            checks.push(("sibling file", run(&rw, "-e", &sib_file), Some(1)));
            checks.push(("sibling uds", run(&rw, "-S", &sib_sock), Some(1)));
        }
        // `/var/tmp`・`/dev/shm` の兄弟（ファイルと UDS）も消えること（#1492）。作れない環境は CI 以外のみ許す。
        let mut extra_dirs = Vec::new();
        let mut extra_listeners = Vec::new();
        for root in ["/var/tmp", "/dev/shm"] {
            let made = canonicalize_dir(Path::new(root)).ok().and_then(|b| {
                let d = b.join(format!("fb-hide-extra-{pid}"));
                let _ = std::fs::remove_dir_all(&d);
                std::fs::create_dir(&d).ok()?;
                let file = d.join("f");
                let sock = d.join("s.sock");
                std::fs::write(&file, b"x").ok()?;
                let l = UnixListener::bind(&sock).ok()?;
                Some((d, file, sock, l))
            });
            let Some((d, file, sock, l)) = made else {
                assert!(!ci_requires_ok(), "CI must have writable {root}");
                continue;
            };
            assert!(hidden.hides(&d, None), "{root} must be hidden");
            checks.push(("extra sibling file", run(&rw, "-e", &file), Some(1)));
            checks.push(("extra sibling uds", run(&rw, "-S", &sock), Some(1)));
            extra_dirs.push(d);
            extra_listeners.push(l);
        }
        checks.push(("workdir file", run(&rw, "-e", &work_file), Some(0)));
        checks.push(("workdir uds", run(&rw, "-S", &work_sock), Some(0)));
        // `$HOME` の既存エントリ（WorkDir と実行ファイルの祖先でないもの）は見えない。
        let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let entry = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| !exe.starts_with(p) && !work.starts_with(p) && (p.is_file() || p.is_dir()));
        let home_checked = entry.is_some() && !home.starts_with("/tmp");
        if let (Some(entry), true) = (entry.as_ref(), home_checked) {
            checks.push(("home entry", run(&rw, "-e", entry), Some(1)));
        }
        // ホスト実行ファイル: 隠れる場所なら ro で戻り、書き込めない。
        let ro = [
            (BindKind::ReadWrite, work.as_path()),
            (BindKind::ReadOnly, exe.as_path()),
        ];
        let ro_tmpfs = &tmpfs;
        let exe_hidden = hidden.hides(&exe, None);
        if exe_hidden {
            checks.push(("exe visible", run(&ro, "-e", &exe), Some(0)));
            // uutils の `test -w` は権限ビットしか見ず ro マウントを検出できないため、`touch -c`
            // （既存ファイルの時刻更新。ro マウントでは EROFS で失敗する）で読み取り専用を確かめる。
            let touch = run_bwrap_test(&bwrap, ro_tmpfs, &ro, &touch_exec, "-c", &exe);
            checks.push(("exe read-only", touch, Some(1)));
            checks.push(("exe hidden without bind", run(&rw, "-e", &exe), Some(1)));
        }

        for d in [&sibling, &work].into_iter().chain(extra_dirs.iter()) {
            let _ = std::fs::remove_dir_all(d);
        }
        for (name, got, want) in checks {
            assert_eq!(got, want, "{name}");
        }
        if ci_requires_ok() {
            assert!(exe_hidden, "CI runs tests from under $HOME");
            assert!(
                home_checked,
                "CI must have a visible-on-host entry in $HOME"
            );
        }
    }

    /// RENDER-3・SEC-2（#1492）: 本物の bwrap で、WorkDir の親を `--tmpfs` で覆うと親の下の兄弟
    /// （ファイルと UDS）が見えず、`--bind` で戻した WorkDir だけが見える。固定対象を `/run` だけにして
    /// `/tmp` 配下の専用ディレクトリを「どの tmpfs にも覆われない親」として使う。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_bwrap_hides_workdir_parent_siblings() {
        use std::os::unix::net::UnixListener;
        const TEST_CANDIDATES: [&str; 2] = ["/usr/bin/test", "/bin/test"];
        let (Ok(bwrap), Ok(test_exec)) = (
            select_trusted(&BWRAP_CANDIDATES),
            select_trusted_exec(&TEST_CANDIDATES),
        ) else {
            assert!(!ci_requires_ok(), "CI must have bwrap and test");
            return;
        };
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let parent = base.join(format!("fb-hide-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).unwrap();
        let sibling = parent.join("sib");
        let work = parent.join("work");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::create_dir(&work).unwrap();
        let sib_sock = sibling.join("s.sock");
        let work_sock = work.join("s.sock");
        let _l1 = UnixListener::bind(&sib_sock).unwrap();
        let _l2 = UnixListener::bind(&work_sock).unwrap();
        // `$HOME` は `/run` の配下（既に覆われる）として tmpfs を増やさない。
        let hidden = HiddenPaths::from_resolved(
            pb(&["/run"]),
            PathBuf::from("/run/home"),
            TmpfsLimit::Capped(TMPFS_SIZE_BYTES),
        );
        let tmpfs = hidden.tmpfs_targets(Some(&parent));
        assert_eq!(tmpfs, [PathBuf::from("/run"), parent.clone()]);
        let rw = [(BindKind::ReadWrite, work.as_path())];
        let checks = [
            (
                "sibling dir",
                run_bwrap_test(&bwrap, &tmpfs, &rw, &test_exec, "-e", &sibling),
                Some(1),
            ),
            (
                "sibling uds",
                run_bwrap_test(&bwrap, &tmpfs, &rw, &test_exec, "-S", &sib_sock),
                Some(1),
            ),
            (
                "workdir uds",
                run_bwrap_test(&bwrap, &tmpfs, &rw, &test_exec, "-S", &work_sock),
                Some(0),
            ),
        ];
        let _ = std::fs::remove_dir_all(&parent);
        for (name, got, want) in checks {
            assert_eq!(got, want, "{name}");
        }
    }

    /// RENDER-3・SEC-2（#1492・#1498・P3-b）: 親が既存の tmpfs に覆われない場合（`Some(親)`）を、
    /// 本物の WorkDir から `socket_dir_for` -> `linux_argv` -> 実 bwrap まで通す。内側の偽ホスト
    /// （親の下に置いた sh スクリプト。`--ro-bind` で戻る）が、UDS が見え・兄弟のファイルと UDS が見えず・
    /// WorkDir に書け・親が tmpfs（上限ありなら `size=65536k`）であることを自分で確かめ、終了コード 0 を返す。
    /// 実 FS の書き込み先はどれも `/tmp` か `$HOME` の配下で既に覆われるため、覆わない `HiddenPaths`
    /// を注入して `Some(親)` を作る。CI の Linux では bwrap が使えることを必須にする。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_socket_dir_for_to_real_bwrap_hides_uncovered_parent() {
        use super::super::workdir::WorkDir;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        let Ok(bwrap) = select_trusted(&BWRAP_CANDIDATES) else {
            assert!(!ci_requires_ok(), "CI must have bwrap");
            return;
        };
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let parent = base.join(format!("fb-p3b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).unwrap();
        let workdir = WorkDir::create(&parent).unwrap();
        let sibling = parent.join("sib");
        std::fs::create_dir(&sibling).unwrap();
        let sib_file = sibling.join("f");
        let sib_sock = sibling.join("s.sock");
        std::fs::write(&sib_file, b"x").unwrap();
        let _l = UnixListener::bind(&sib_sock).unwrap();
        // `/tmp` を覆わない固定対象。`$HOME` は `/run` の配下として tmpfs を増やさない。
        let limit = detect_retrying(&bwrap, PROBE_TIMEOUT);
        let hidden = HiddenPaths::from_resolved(pb(&["/run"]), PathBuf::from("/run/home"), limit);
        let sd = socket_dir_for(workdir.path(), &hidden).unwrap();
        assert_eq!(sd.parent_to_hide, Some(parent.clone()));
        let uds = sd.dir.join(EGRESS_SOCKET_NAME);
        let _proxy = UnixListener::bind(&uds).unwrap();
        let size_check = if matches!(limit, TmpfsLimit::Capped(_)) {
            "case \"$rest\" in *\" - tmpfs \"*size=65536k*) found=1;; esac"
        } else {
            "found=1"
        };
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\n\
             [ -S \"$last\" ] || exit 11\n\
             [ -e '{sf}' ] && exit 12\n\
             [ -e '{ss}' ] && exit 13\n\
             : > '{wd}/probe' || exit 14\n\
             found=0\n\
             while read -r id pid dev root mp rest; do\n\
               if [ \"$mp\" = '{pa}' ]; then {size_check}; fi\n\
             done < /proc/self/mountinfo\n\
             [ \"$found\" = 1 ] || exit 15\nexit 0\n",
            sf = sib_file.display(),
            ss = sib_sock.display(),
            wd = sd.dir.display(),
            pa = parent.display(),
        );
        let host = parent.join("host.sh");
        std::fs::write(&host, script).unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (prog, args) = linux_argv(
            &bwrap,
            &hidden,
            &sd.dir,
            sd.parent_to_hide.as_deref(),
            &host,
            &uds,
        )
        .unwrap();
        let mut code = None;
        for _ in 0..20 {
            let mut cmd = Command::new(&prog);
            cmd.args(&args)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let Ok(mut child) = cmd.spawn() else {
                // ETXTBSY 等の一過性の起動失敗はやり直す。
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            code = child.wait().ok().and_then(|st| st.code());
            break;
        }
        drop(workdir);
        let _ = std::fs::remove_dir_all(&parent);
        assert_eq!(code, Some(0));
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
            EgressIsolationUnavailable::WorkDirParentNotHideable,
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
        let w = isolation_warning_message(EgressIsolationUnavailable::WorkDirParentNotHideable);
        assert_eq!(
            w,
            "render host egress isolation is unavailable (work directory's parent cannot be \
             hidden by the sandbox); starting render host without egress isolation"
        );
        assert!(!w.contains('/'));
        let u = isolation_warning_message(EgressIsolationUnavailable::SandboxToolUntrusted);
        assert_eq!(
            u,
            "render host egress isolation is unavailable (sandbox tool found but not trusted \
             (must be root-owned and not group/other writable)); starting render host without \
             egress isolation"
        );
    }

    /// RENDER-3・SEC-2（#1498）: 上限ありのとき、各 `--tmpfs` の直前に `--size 67108864` が付く
    /// argv の完全一致（`--size` は直後の 1 本にだけ効くため対象ごとに並べる）。
    #[test]
    fn render3_linux_argv_is_exact_with_tmpfs_size() {
        let hidden = HiddenPaths::new(PathBuf::from("/home/u"));
        assert_eq!(hidden.tmpfs_limit(), TmpfsLimit::Capped(64 * 1024 * 1024));
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            None,
            Path::new("/opt/host/servo-host"),
            Path::new("/tmp/w/egress-proxy.sock"),
        )
        .unwrap();
        assert_eq!(
            argv_strs(&args),
            vec![
                "--die-with-parent",
                "--unshare-net",
                "--unshare-pid",
                "--dev-bind",
                "/",
                "/",
                "--proc",
                "/proc",
                "--size",
                "67108864",
                "--tmpfs",
                "/run",
                "--size",
                "67108864",
                "--tmpfs",
                "/tmp",
                "--size",
                "67108864",
                "--tmpfs",
                "/var/tmp",
                "--size",
                "67108864",
                "--tmpfs",
                "/dev/shm",
                "--size",
                "67108864",
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
    }

    /// RENDER-3・SEC-2（#1498）: bwrap が `--size` 非対応（`Unsupported`）なら従来どおり `--size` を
    /// 付けない argv の完全一致（隔離は続け、上限だけ無い）。
    #[test]
    fn render3_linux_argv_is_exact_without_tmpfs_size() {
        let hidden =
            HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(TmpfsLimit::Unsupported);
        let (_, args) = linux_argv(
            Path::new("/usr/bin/bwrap"),
            &hidden,
            Path::new("/tmp/w"),
            None,
            Path::new("/opt/host/servo-host"),
            Path::new("/tmp/w/egress-proxy.sock"),
        )
        .unwrap();
        assert_eq!(
            argv_strs(&args),
            vec![
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
                "/var/tmp",
                "--tmpfs",
                "/dev/shm",
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
    }

    /// RENDER-3（#1498）: `--help` からの `--size` 検出はトークンの完全一致。
    #[test]
    fn render3_help_supports_tmpfs_size_cases() {
        let real = b"Usage: bwrap [OPTION...] [--] COMMAND [ARGS...]\n\n    --tmpfs DEST                 Mount new tmpfs on DEST\n    --size BYTES                 Set size in bytes of next tmpfs\n";
        assert!(help_supports_tmpfs_size(real));
        assert!(!help_supports_tmpfs_size(
            b"    --tmpfs DEST   Mount new tmpfs on DEST\n    --sizes N\n"
        ));
        assert!(!help_supports_tmpfs_size(b"--size-limit\n"));
        assert!(!help_supports_tmpfs_size(b""));
        assert!(!help_supports_tmpfs_size(&[0xff, 0xfe, 0x00, 0x80]));
    }

    /// RENDER-3・SEC-2（#1498・#1502）: 警告文言は理由ごとに固定の英語で、互いに違い、パスを含まない。
    #[test]
    fn render3_tmpfs_warning_messages_differ_and_have_no_paths() {
        let unsupported = tmpfs_limit_warning_message(TmpfsUnlimitedReason::SizeUnsupported);
        let failed = tmpfs_limit_warning_message(TmpfsUnlimitedReason::DetectionFailed);
        assert_eq!(
            unsupported,
            "render host sandbox (bwrap) does not support --size; tmpfs mounts are not size-limited"
        );
        assert_eq!(
            failed,
            "render host sandbox (bwrap) --size support could not be detected; tmpfs mounts are not size-limited"
        );
        assert_ne!(unsupported, failed);
        assert!(!unsupported.contains('/') && !failed.contains('/'));
        // 公開の message 関数は、隔離なしの警告には従来の文言をそのまま返す。
        assert_eq!(
            egress_isolation_warning_message(EgressIsolationWarning::Unisolated(
                EgressIsolationUnavailable::SandboxToolNotFound
            )),
            isolation_warning_message(EgressIsolationUnavailable::SandboxToolNotFound)
        );
        assert_eq!(
            egress_isolation_warning_message(EgressIsolationWarning::TmpfsUnlimited(
                TmpfsUnlimitedReason::DetectionFailed
            )),
            failed
        );
    }

    /// RENDER-3・SEC-2（#1502）: 検出に失敗した（`Undetected`）ときも `--size` は付けない
    /// （`--size` を知らない bwrap で試走が失敗し、隔離なしに落ちるのを避ける）。
    #[test]
    fn render3_undetected_limit_adds_no_size_flag() {
        let args = linux_wrapper_args(
            &[PathBuf::from("/tmp")],
            TmpfsLimit::Undetected(TmpfsDetectFailure::TimedOut),
            &[],
        );
        assert!(!args.iter().any(|a| a == "--size"), "{args:?}");
        assert!(args.iter().any(|a| a == "--tmpfs"), "{args:?}");
    }

    /// RENDER-3（#1502）: 一過性なのは起動の失敗と時間切れだけ。
    #[test]
    fn render3_tmpfs_detect_failure_transience() {
        use std::io::ErrorKind;
        assert!(TmpfsDetectFailure::SpawnFailed(ErrorKind::ExecutableFileBusy).is_transient());
        assert!(TmpfsDetectFailure::TimedOut.is_transient());
        assert!(!TmpfsDetectFailure::ExitFailure.is_transient());
        assert!(!TmpfsDetectFailure::OutputTooLarge.is_transient());
        assert!(!TmpfsDetectFailure::OutputUnreadable.is_transient());
    }

    fn bwrap_with(limit: TmpfsLimit) -> Result<Sandbox, EgressIsolationUnavailable> {
        Ok(Sandbox::Bwrap {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            hidden: HiddenPaths::new(PathBuf::from("/home/u")).with_tmpfs_limit(limit),
        })
    }

    fn limit_of(r: &Result<Sandbox, EgressIsolationUnavailable>) -> Option<TmpfsLimit> {
        match r {
            Ok(Sandbox::Bwrap { hidden, .. }) => Some(hidden.tmpfs_limit()),
            _ => None,
        }
    }

    /// RENDER-3・SEC-2（#1502）: 一過性の検出失敗の後は再プローブし、`Capped` になったら固定する
    /// （3 回目は probe を呼ばない）。実時間に依存しない（probe を注入する）。
    #[test]
    fn render3_cached_probe_reprobes_after_transient_until_capped() {
        let mut seq = vec![
            TmpfsLimit::Undetected(TmpfsDetectFailure::TimedOut),
            TmpfsLimit::Capped(TMPFS_SIZE_BYTES),
        ]
        .into_iter();
        let mut calls = 0;
        let mut cache = ProbeCache::default();
        let mut results = Vec::new();
        for _ in 0..3 {
            results.push(cached_probe(&mut cache, || {
                calls += 1;
                bwrap_with(seq.next().expect("probe called too often"))
            }));
        }
        assert_eq!(
            limit_of(&results[0]),
            Some(TmpfsLimit::Undetected(TmpfsDetectFailure::TimedOut))
        );
        assert_eq!(
            limit_of(&results[1]),
            Some(TmpfsLimit::Capped(TMPFS_SIZE_BYTES))
        );
        assert_eq!(
            limit_of(&results[2]),
            Some(TmpfsLimit::Capped(TMPFS_SIZE_BYTES))
        );
        assert_eq!(calls, 2);
    }

    /// RENDER-3（#1502）: 確定した結果（`Unsupported`・一過性でない `Undetected`・`Err`・
    /// `SandboxExec`）は再プローブしない。
    #[test]
    fn render3_cached_probe_keeps_unsupported_and_non_transient() {
        let cases: Vec<Result<Sandbox, EgressIsolationUnavailable>> = vec![
            bwrap_with(TmpfsLimit::Unsupported),
            bwrap_with(TmpfsLimit::Undetected(TmpfsDetectFailure::ExitFailure)),
            bwrap_with(TmpfsLimit::Undetected(TmpfsDetectFailure::OutputTooLarge)),
            bwrap_with(TmpfsLimit::Capped(TMPFS_SIZE_BYTES)),
            Err(EgressIsolationUnavailable::SandboxProbeFailed),
            Ok(Sandbox::SandboxExec),
        ];
        for case in cases {
            let mut calls = 0;
            let mut cache = ProbeCache::default();
            let first = cached_probe(&mut cache, || {
                calls += 1;
                case.clone()
            });
            let second = cached_probe(&mut cache, || {
                calls += 1;
                case.clone()
            });
            assert_eq!(first, case);
            assert_eq!(second, case);
            assert_eq!(calls, 1, "{case:?}");
        }
    }

    /// RENDER-3・SEC-2（#1502）: 再プローブが `Err` でも、前の `Ok`（隔離できていた状態）を残す
    /// （`best-effort` を隔離なしへ降格させない）。
    #[test]
    fn render3_cached_probe_reprobe_error_keeps_previous_ok() {
        let transient = TmpfsLimit::Undetected(TmpfsDetectFailure::TimedOut);
        let mut cache = ProbeCache::default();
        let first = cached_probe(&mut cache, || bwrap_with(transient));
        let second = cached_probe(&mut cache, || {
            Err(EgressIsolationUnavailable::SandboxProbeFailed)
        });
        assert_eq!(limit_of(&first), Some(transient));
        assert_eq!(limit_of(&second), Some(transient));
    }

    /// RENDER-3（#1502）: 検出し直す回数には上限がある（初回 + [`TMPFS_REPROBE_MAX`] 回で止まる）。
    #[test]
    fn render3_cached_probe_reprobe_is_bounded() {
        let transient = TmpfsLimit::Undetected(TmpfsDetectFailure::SpawnFailed(
            std::io::ErrorKind::ExecutableFileBusy,
        ));
        let mut calls = 0;
        let mut cache = ProbeCache::default();
        for _ in 0..(TMPFS_REPROBE_MAX + 5) {
            let r = cached_probe(&mut cache, || {
                calls += 1;
                bwrap_with(transient)
            });
            assert_eq!(limit_of(&r), Some(transient));
        }
        assert_eq!(calls, 1 + TMPFS_REPROBE_MAX);
    }

    /// RENDER-3（#1498・#1502）: `--size` 非対応の bwrap（偽物。`--help` に `--size` が無く、`--size` 付きの
    /// 起動は失敗する）では `Unsupported` と判定し、その上限で本番と同じ試走が通る
    /// （隔離は外さず、上限だけ外す）。実物の bwrap と readlink が要る。CI では必須。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_unsupported_size_keeps_isolation_with_fake_old_bwrap() {
        let (real, readlink) = match (
            select_trusted(&BWRAP_CANDIDATES),
            select_trusted_exec(&READLINK_CANDIDATES),
        ) {
            (Ok(b), Ok(r)) => (b, r),
            (b, r) => {
                assert!(
                    !ci_requires_ok(),
                    "CI must have bwrap and readlink: {b:?} {r:?}"
                );
                return;
            }
        };
        let body = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then echo 'Usage: bwrap [OPTION...] COMMAND'; echo '    --tmpfs DEST  Mount new tmpfs'; exit 0; fi\nfor a in \"$@\"; do if [ \"$a\" = \"--size\" ]; then exit 1; fi; done\nexec {} \"$@\"\n",
            real.display()
        );
        let (dir, fake) = fake_script("oldbwrap", &body);
        // ETXTBSY（書き込み直後の起動失敗）以外はやり直さない。`Capped` になったらここで失敗する。
        let limit = detect_retrying(&fake, Duration::from_secs(5));
        assert_eq!(limit, TmpfsLimit::Unsupported);
        let home = resolve_home_to_hide(std::env::var_os("HOME").as_deref());
        let result = home.map(|home| {
            let hidden = HiddenPaths::new(home).with_tmpfs_limit(limit);
            probe_retrying_with(&fake, &readlink, &hidden)
        });
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(r) => assert_eq!(r, Ok(())),
            Err(e) => assert!(!ci_requires_ok(), "CI must have a hideable HOME: {e:?}"),
        }
    }

    /// RENDER-3・SEC-2（#1502）: 一過性の起動失敗（実行できない fake）は `Undetected(SpawnFailed)` で
    /// 固定されず、同じパスが実行可能になって `--size` を持てば次の検出で `Capped` になる。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_detect_tmpfs_limit_transient_spawn_failure_then_capped() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, fake) = fake_script("flaky", "#!/bin/sh\nexit 0\n");
        // 実行権限を外して起動を決定的に失敗させる。
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).unwrap();
        let first = detect_tmpfs_limit(&fake, Duration::from_secs(5));
        let TmpfsLimit::Undetected(failure) = first else {
            panic!("expected Undetected, got {first:?}");
        };
        assert!(
            matches!(
                failure,
                TmpfsDetectFailure::SpawnFailed(
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                )
            ),
            "{failure:?}"
        );
        assert!(failure.is_transient());
        // 回復: `--size` を持つ実行可能な bwrap になる。
        std::fs::write(
            &fake,
            "#!/bin/sh\necho '    --size BYTES  tmpfs size'\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let second = detect_retrying(&fake, Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(second, TmpfsLimit::Capped(TMPFS_SIZE_BYTES));
    }

    /// 偽 bwrap が書いた pid ファイル（`<script>.<ext>`）を読む。無い・数値でないなら panic。
    #[cfg(target_os = "linux")]
    fn read_pid(script: &Path, ext: &str) -> u32 {
        let mut name = script.as_os_str().to_owned();
        name.push(format!(".{ext}"));
        std::fs::read_to_string(PathBuf::from(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// `/proc/<pid>` が消える（回収済み）まで最大 2 秒待つ。残っていれば false。
    #[cfg(target_os = "linux")]
    fn process_gone(pid: u32) -> bool {
        let p = PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..200 {
            if !p.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// RENDER-3（#1502・#1508）: 時間切れは `TimedOut`、非 0 終了は `ExitFailure`、`--size` の無い help は
    /// `Unsupported` に分類する。時間切れの偽物は `exec` で pid を保ち、kill 後に残らない。stdout を
    /// 孫へ受け継がせる偽物でも読み取りスレッドの `join` で止まらない。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_detect_tmpfs_limit_classifies_timeout_and_exit_failure() {
        let (d1, slow) = fake_script("slow", "#!/bin/sh\necho $$ > \"$0.pid\"\nexec sleep 30\n");
        let (d2, failing) = fake_script("fail", "#!/bin/sh\nexit 3\n");
        let (d3, nosize) = fake_script("nosize", "#!/bin/sh\necho '    --tmpfs DEST'\nexit 0\n");
        let (d4, gc) = fake_script(
            "gc",
            "#!/bin/sh\nsleep 5 &\necho $! > \"$0.gc\"\necho $$ > \"$0.pid\"\nexec sleep 30\n",
        );
        let started = Instant::now();
        let timed_out = detect_retrying(&slow, Duration::from_millis(300));
        let slow_elapsed = started.elapsed();
        let exit_failure = detect_retrying(&failing, Duration::from_secs(5));
        let unsupported = detect_retrying(&nosize, Duration::from_secs(5));
        let started = Instant::now();
        let gc_limit = detect_retrying(&gc, Duration::from_millis(300));
        let gc_elapsed = started.elapsed();
        let slow_pid = read_pid(&slow, "pid");
        let gc_pid = read_pid(&gc, "pid");
        let grandchild = read_pid(&gc, "gc");
        // 孫を残さない（テスト自身が kill する。数値は自分で書いたファイル由来）。
        let _ = Command::new("kill").arg(grandchild.to_string()).status();
        let slow_gone = process_gone(slow_pid);
        let gc_gone = process_gone(gc_pid);
        let grandchild_gone = process_gone(grandchild);
        for d in [d1, d2, d3, d4] {
            let _ = std::fs::remove_dir_all(&d);
        }
        assert_eq!(
            timed_out,
            TmpfsLimit::Undetected(TmpfsDetectFailure::TimedOut)
        );
        assert_eq!(
            gc_limit,
            TmpfsLimit::Undetected(TmpfsDetectFailure::TimedOut)
        );
        assert!(slow_elapsed < Duration::from_secs(2), "{slow_elapsed:?}");
        assert!(gc_elapsed < Duration::from_secs(2), "{gc_elapsed:?}");
        assert!(slow_gone, "sleep 30 remained (pid {slow_pid})");
        assert!(gc_gone, "gc parent remained (pid {gc_pid})");
        assert!(grandchild_gone, "grandchild remained (pid {grandchild})");
        assert_eq!(
            exit_failure,
            TmpfsLimit::Undetected(TmpfsDetectFailure::ExitFailure)
        );
        assert_eq!(unsupported, TmpfsLimit::Unsupported);
    }

    /// RENDER-3（#1498・#1502・#1508）: `--help` の出力が [`HELP_STDOUT_MAX`] を超える偽物は、`--size` を含んでいても
    /// `Undetected(OutputTooLarge)`（上限超は信頼しない）。出力後に居座っても時間切れ（`TimedOut`）を待たず、
    /// 超過した時点で kill し、偽物のプロセスを残さない。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_detect_tmpfs_limit_oversized_help_kills_early() {
        let body = "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then echo $$ > \"$0.pid\"; echo '    --size BYTES  tmpfs size'; i=0; while [ $i -lt 4000 ]; do echo 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i+1)); done; exec sleep 600; fi\nexit 1\n";
        let (dir, fake) = fake_script("bigbwrap", body);
        let started = Instant::now();
        let limit = detect_retrying(&fake, Duration::from_secs(60));
        let elapsed = started.elapsed();
        let pid = read_pid(&fake, "pid");
        let gone = process_gone(pid);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            limit,
            TmpfsLimit::Undetected(TmpfsDetectFailure::OutputTooLarge)
        );
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert!(gone, "fake bwrap remained (pid {pid})");
    }

    /// RENDER-3（#1498）: 実物の bwrap の `--help` 検出が、`--size` ありの版で `Capped`、無い版で
    /// `Unsupported` を返す。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_detect_tmpfs_limit_matches_real_bwrap_help() {
        let Ok(bwrap) = select_trusted(&BWRAP_CANDIDATES) else {
            assert!(!ci_requires_ok(), "CI must have bwrap");
            return;
        };
        let help = Command::new(&bwrap)
            .arg("--help")
            .env_clear()
            .output()
            .unwrap();
        let expected = if help_supports_tmpfs_size(&help.stdout) {
            TmpfsLimit::Capped(TMPFS_SIZE_BYTES)
        } else {
            TmpfsLimit::Unsupported
        };
        assert_eq!(detect_retrying(&bwrap, PROBE_TIMEOUT), expected);
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
