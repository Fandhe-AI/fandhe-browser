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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// 監視スレッドのハンドル。Drop で停止を通知する（スレッドは detach され、次の待機で終わる）。
#[derive(Debug)]
pub struct RssMonitor {
    _stop: Sender<()>,
}

impl RssMonitor {
    /// 監視を始める。プローブが無い OS では `Ok(None)`（監視しない）。
    /// スレッドを起動できなければ `Err`（呼び出し側が子を kill・wait して失敗を返す）。
    /// 上限超過または連続読み取り失敗で `exceeded` を立てて `child` を kill する。
    pub fn start(
        pid: u32,
        limit_bytes: u64,
        interval: Duration,
        child: Arc<Mutex<Child>>,
        exceeded: Arc<AtomicBool>,
    ) -> Result<Option<Self>, RssStartError> {
        if !probe_supported() {
            return Ok(None);
        }
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("render-host-rss".into())
            .spawn(move || {
                let mut failures = 0u32;
                loop {
                    match rx.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                    let over = match probe_rss(pid) {
                        Some(rss) => {
                            failures = 0;
                            rss > limit_bytes
                        }
                        None => {
                            failures += 1;
                            failures >= MAX_CONSECUTIVE_PROBE_FAILURES
                        }
                    };
                    if over {
                        exceeded.store(true, Ordering::SeqCst);
                        if let Ok(mut c) = child.lock() {
                            let _ = c.kill();
                        }
                        return;
                    }
                }
            })
            .map_err(|_| RssStartError)?;
        Ok(Some(Self { _stop: tx }))
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn render4_probe_is_unsupported_here() {
        assert!(!probe_supported());
        assert_eq!(probe_rss(std::process::id()), None);
    }
}
