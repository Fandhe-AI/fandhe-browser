//! 親側の RSS 監視（設計 §2.7。RENDER-4。TASK-38・#994）。
//!
//! [`super::HostProcess`] がホスト起動ごとに [`RssMonitor`] を 1 本起動し、上限超過で
//! 子を kill する。js crate の `read_child_rss_bytes` 方式を踏襲するが、render は js に
//! 依存しないため再実装している。
//!
//! - Linux: `/proc/<pid>/status` の `VmRSS`
//! - macOS: `/bin/ps -o rss= -p <pid>`（絶対パス・500 ms 上限）
//! - その他（Windows を含む）: **未実装**。`windows-sys` への依存辺と新規 `unsafe` が要るため
//!   承認事項として #994 で報告済み。[`probe_supported`] が false を返し、監視自体を起動しない
//!   （REPAIR-3）。プローブが無い OS では「読めない」ことを理由に kill しない。

use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// プローブ間隔。
pub const RSS_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 読み取り連続失敗でこの回数に達したら kill する（プローブがある OS のみ。fail-closed）。
const MAX_CONSECUTIVE_PROBE_FAILURES: u32 = 10;

/// この OS に RSS プローブがあるか。
pub const fn probe_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

/// 子プロセスの RSS（バイト）。読めなければ `None`。
pub fn probe_rss(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kib.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        probe_macos(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "macos")]
fn probe_macos(pid: u32) -> Option<u64> {
    use std::io::Read;
    use std::time::Instant;
    let mut child = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    let kib: u64 = text.trim().parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// 監視スレッドを起動できなかった（スレッド資源不足など）。
/// 「この OS に RSS プローブが無い」こと（`Ok(None)`）とは区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RssStartError;

/// 監視対象の pid の出所。
///
/// bwrap で包むと子プロセスの pid は bwrap になる。`--unshare-pid` のため、外から見ると
/// `bwrap(起動した子) -> bwrap(ns 内 pid 1) -> ホスト` の 2 段で、ホストは孫になる（macOS の
/// sandbox-exec は exec するので pid は変わらない。RENDER-3・RENDER-4・#1436）。bwrap 自身の RSS は
/// ホストの使用量を表さないため代用しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidSource {
    /// 起動した子そのもの。
    Direct(u32),
    /// 起動した子（ラッパー）から子を 2 段、各段ちょうど 1 件ずつ辿った先（ホスト）。
    /// 辿れるまで「計測不能」として扱う。
    WrappedChildOf(u32),
}

/// bwrap の pid 名前空間の段数（ラッパー -> ns 内 pid 1 の bwrap -> ホスト）。
const WRAPPER_DEPTH: usize = 2;

/// ラッパー（bwrap）配下のホストの pid を、外側の pid 名前空間での pid で辿る。
///
/// `/proc/<p>/task/<p>/children` を [`WRAPPER_DEPTH`] 段たどり、各段で空白区切りの pid が
/// **ちょうど 1 件**のときだけ `Some`。0 件・2 件以上・形式不正・読めない場合は `None`
/// （曖昧なまま別プロセスの RSS を測らない）。`read` は `/proc` の読み取りを差し替えられる
/// ようにするための注入点（本番は `std::fs::read_to_string`）。`CONFIG_PROC_CHILDREN` の無い
/// カーネルでは読めず、`None` が続いて連続失敗の kill に至る（fail-closed）。
///
/// 前提（#1451）: servo-host は Servo を `multiprocess: false`（`servo-host/src/engine.rs`）で起動し、
/// `servo-host/src` に子プロセスを作る箇所が無いので、ns 内 pid 1 の子はホスト 1 件だけとみなす。
/// この前提はコード読みによるもので、実ホストで子プロセス数を実測してはいない（Servo 本体や
/// 依存が内部で子を作る可能性は排除できていない。実測は #1437 の範囲）。ホストが
/// ダブルフォークで孤児の子を作ると 2 段目が 2 件になり、解決前に `MAX_CONSECUTIVE_PROBE_FAILURES`
/// 回失敗すると kill される（解決後は pid が固定されるので影響しない）。前提が崩れた場合
/// （Servo のメディア系が子を作る等）は cmdline・exe で選ぶ方式に変える。
pub fn resolve_wrapped_pid(wrapper: u32, read: &dyn Fn(&str) -> Option<String>) -> Option<u32> {
    let mut pid = wrapper;
    for _ in 0..WRAPPER_DEPTH {
        let text = read(&format!("/proc/{pid}/task/{pid}/children"))?;
        let mut pids = text.split_whitespace();
        let first = pids.next()?.parse::<u32>().ok()?;
        if pids.next().is_some() {
            return None;
        }
        pid = first;
    }
    Some(pid)
}

/// 1 回分の監視状態（pid の解決・連続失敗の計数）。スレッドと単体テストが共有する。
struct Watch {
    source: PidSource,
    resolved: Option<u32>,
    failures: u32,
}

impl Watch {
    fn new(source: PidSource) -> Self {
        let resolved = match source {
            PidSource::Direct(p) => Some(p),
            PidSource::WrappedChildOf(_) => None,
        };
        Self {
            source,
            resolved,
            failures: 0,
        }
    }

    /// 1 tick 分。上限超過、または連続失敗が上限に達したら true（kill すべき）。
    /// pid が未解決のあいだは失敗に数える。
    fn step(
        &mut self,
        limit_bytes: u64,
        read: &dyn Fn(&str) -> Option<String>,
        probe: &dyn Fn(u32) -> Option<u64>,
    ) -> bool {
        if self.resolved.is_none()
            && let PidSource::WrappedChildOf(w) = self.source
        {
            self.resolved = resolve_wrapped_pid(w, read);
        }
        match self.resolved.and_then(probe) {
            Some(rss) => {
                self.failures = 0;
                rss > limit_bytes
            }
            None => {
                self.failures += 1;
                self.failures >= MAX_CONSECUTIVE_PROBE_FAILURES
            }
        }
    }
}

/// 監視スレッドのハンドル。Drop で停止を通知する（スレッドは detach され、次の待機で終わる）。
#[derive(Debug)]
pub struct RssMonitor {
    _stop: Sender<()>,
    /// 解決済みの監視対象 pid（未解決は 0。診断・テスト用）。
    resolved: Arc<AtomicU32>,
}

impl RssMonitor {
    /// 監視を始める。プローブが無い OS では `Ok(None)`（監視しない）。
    /// スレッドを起動できなければ `Err`（呼び出し側が子を kill・wait して失敗を返す）。
    /// 上限超過または連続読み取り失敗で `exceeded` を立てて `child` を kill する。
    pub fn start(
        source: PidSource,
        limit_bytes: u64,
        interval: Duration,
        child: Arc<Mutex<Child>>,
        exceeded: Arc<AtomicBool>,
    ) -> Result<Option<Self>, RssStartError> {
        if !probe_supported() {
            return Ok(None);
        }
        let (tx, rx) = mpsc::channel::<()>();
        let resolved = Arc::new(AtomicU32::new(match source {
            PidSource::Direct(p) => p,
            PidSource::WrappedChildOf(_) => 0,
        }));
        let resolved_out = Arc::clone(&resolved);
        std::thread::Builder::new()
            .name("render-host-rss".into())
            .spawn(move || {
                let mut watch = Watch::new(source);
                let read = |path: &str| std::fs::read_to_string(path).ok();
                loop {
                    match rx.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                    let over = watch.step(limit_bytes, &read, &probe_rss);
                    resolved_out.store(watch.resolved.unwrap_or(0), Ordering::SeqCst);
                    if over {
                        exceeded.store(true, Ordering::SeqCst);
                        if let Ok(mut c) = child.lock() {
                            // pid 名前空間のため、ラッパーを kill すれば ns 内のホストも落ちる
                            // （`isolation` のモジュール doc）。名指しの kill はしない。
                            let _ = c.kill();
                        }
                        return;
                    }
                }
            })
            .map_err(|_| RssStartError)?;
        Ok(Some(Self {
            _stop: tx,
            resolved,
        }))
    }

    /// 監視対象として解決できた pid。未解決は `None`。
    pub fn monitored_pid(&self) -> Option<u32> {
        match self.resolved.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn render4_probe_reads_own_rss() {
        let rss = probe_rss(std::process::id()).expect("rss");
        assert!(rss > 0);
    }

    fn children_of<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |path: &str| {
            map.iter()
                .find(|(k, _)| *k == path)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// RENDER-3・RENDER-4（#1436）: ラッパー -> ns 内 pid 1 の bwrap -> ホストの 2 段を、各段 1 件の
    /// ときだけ辿る。読む先は task/<pid>/children。
    #[test]
    fn render4_resolve_wrapped_pid_walks_two_levels() {
        let tree = [
            ("/proc/77/task/77/children", "1234 "),
            ("/proc/1234/task/1234/children", "1235\n"),
        ];
        assert_eq!(resolve_wrapped_pid(77, &children_of(&tree)), Some(1235));
        // 1 段目しか無い（ホスト未起動）・ホストが複数・ラッパーが複数は曖昧なので None。
        let first_only = [("/proc/77/task/77/children", "1234")];
        assert_eq!(resolve_wrapped_pid(77, &children_of(&first_only)), None);
        let two_hosts = [
            ("/proc/77/task/77/children", "1234"),
            ("/proc/1234/task/1234/children", "1235 1236"),
        ];
        assert_eq!(resolve_wrapped_pid(77, &children_of(&two_hosts)), None);
        let two_inits = [
            ("/proc/77/task/77/children", "1234 1240"),
            ("/proc/1234/task/1234/children", "1235"),
        ];
        assert_eq!(resolve_wrapped_pid(77, &children_of(&two_inits)), None);
        for (text, why) in [("", "empty"), ("abc", "garbage"), ("-5", "negative")] {
            let m = [
                ("/proc/77/task/77/children", "1234"),
                ("/proc/1234/task/1234/children", text),
            ];
            assert_eq!(resolve_wrapped_pid(77, &children_of(&m)), None, "{why}");
        }
        // 別 pid のファイルしか無い（読み取り失敗）。
        assert_eq!(resolve_wrapped_pid(78, &children_of(&tree)), None);
    }

    /// RENDER-4（#1436）: 未解決のあいだは失敗に数え、10 回で kill 判定になる。解決後は子の RSS で判定する。
    #[test]
    fn render4_watch_counts_unresolved_as_failure_and_never_uses_wrapper_rss() {
        let none = |_: &str| None;
        // ラッパー自身 (77) の RSS は大きいが、使ってはならない。
        let probe = |pid: u32| if pid == 77 { Some(u64::MAX) } else { None };
        let mut w = Watch::new(PidSource::WrappedChildOf(77));
        for i in 1..MAX_CONSECUTIVE_PROBE_FAILURES {
            assert!(!w.step(100, &none, &probe), "tick {i}");
        }
        assert!(w.step(100, &none, &probe), "10th failure must kill");
        assert_eq!(w.resolved, None);

        // 解決後は子の RSS で判定し、固定する。
        let m = [
            ("/proc/77/task/77/children", "400"),
            ("/proc/400/task/400/children", "500"),
        ];
        let read = children_of(&m);
        let probe = |pid: u32| if pid == 500 { Some(50) } else { Some(u64::MAX) };
        let mut w = Watch::new(PidSource::WrappedChildOf(77));
        assert!(!w.step(100, &read, &probe));
        assert_eq!(w.resolved, Some(500));
        assert!(!w.step(100, &none, &probe), "resolved pid is kept");
        let over = |pid: u32| (pid == 500).then_some(101);
        assert!(w.step(100, &none, &over));
    }

    /// RENDER-4: 直接の子は即座に解決済みで、読めなければ従来どおり 10 回で kill 判定。
    #[test]
    fn render4_watch_direct_pid_is_resolved_immediately() {
        let none = |_: &str| None;
        let mut w = Watch::new(PidSource::Direct(9));
        assert_eq!(w.resolved, Some(9));
        assert!(!w.step(100, &none, &|_| Some(100)));
        assert!(w.step(100, &none, &|_| Some(101)));
        let mut w = Watch::new(PidSource::Direct(9));
        for _ in 1..MAX_CONSECUTIVE_PROBE_FAILURES {
            assert!(!w.step(100, &none, &|_| None));
        }
        assert!(w.step(100, &none, &|_| None));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn render4_probe_is_unsupported_here() {
        assert!(!probe_supported());
        assert_eq!(probe_rss(std::process::id()), None);
    }
}
