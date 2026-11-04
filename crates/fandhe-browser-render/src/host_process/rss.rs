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

use std::collections::HashSet;
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
/// この前提はコード読みによるもので、Servo 本体や依存が内部で子を作る可能性は排除できていない。
/// 実ホストでの記録は `real_host` の `EGRESS-PROCTREE` 行（[`walk_wrapped_proc_tree_live`]・#1460）、
/// 結果の追跡は #1457。ホストが
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

/// プロセスツリー走査の深さの上限（循環・暴走の打ち切り）。
const PROC_TREE_MAX_DEPTH: usize = 32;
/// プロセスツリー走査で訪問するノード数の上限。
const PROC_TREE_MAX_NODES: usize = 4096;
/// 1 プロセスあたりの tid 数の上限（Servo のスレッド数より十分大きく、暴走だけを止める値。#1466）。
const PROC_TREE_MAX_TIDS_PER_NODE: usize = 8192;
/// 1 プロセスあたりの children（全 tid の和集合）の件数の上限（#1466）。
const PROC_TREE_MAX_CHILDREN_PER_NODE: usize = 1024;
/// 主スレッドの children が読めなかったとき、読み直す前に待つ時間（ゾンビ化との競合対策。#1466）。
const PROC_TREE_RETRY_DELAY: Duration = Duration::from_millis(20);

/// [`walk_wrapped_proc_tree_live`] の失敗。`/proc` が読めない・形式不正・上限超過のとき推測で埋めず返す。
///
/// 診断用（`real_host` の `{e:?}` 表示）にだけ使うため `non_exhaustive`（#1466）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcTreeError {
    /// `/proc/<pid>/task` の一覧が読めない。
    NoTasks { pid: u32 },
    /// `children` ファイルが読めない。
    Unreadable { path: String },
    /// `children` の中身が空白区切りの非負整数でない。
    Malformed { path: String },
    /// 1 プロセスの tid 数が [`PROC_TREE_MAX_TIDS_PER_NODE`] を超えた（#1466）。
    TooManyTasks { pid: u32 },
    /// 1 プロセスの children が [`PROC_TREE_MAX_CHILDREN_PER_NODE`] を超えた（#1466）。
    TooManyChildren { path: String },
}

/// ラッパー配下のプロセスツリーの記録（RENDER-3・RENDER-4・#1460）。
///
/// `real_host` の隔離下結合テストが、`resolve_wrapped_pid` の前提（ns 内 pid 1 の子がホスト 1 件）
/// を実ホストで確かめるために使う診断用の値。children は全スレッド（tid）の和集合で数える
/// （`/proc/<pid>/task/<tid>/children` はその tid が作った子だけを返すため）。
///
/// `host_process` が `#[doc(hidden)]` で再公開する診断用の型で、安定 API ではない。フィールドの追加を
/// 破壊的変更にしないため `non_exhaustive`（外部 crate からは構築できず、読み取りだけができる。#1473）。
///
/// ```
/// use fandhe_browser_render::host_process::{ProcTreeError, WrappedProcTree, walk_wrapped_proc_tree_live};
/// // 存在しない pid は Err（推測で埋めない）。型とパスの存在確認も兼ねる。
/// let r: Result<WrappedProcTree, ProcTreeError> = walk_wrapped_proc_tree_live(u32::MAX);
/// assert!(r.is_err());
/// ```
///
/// 外部 crate からのフィールド指定による構築はコンパイルエラー（`non_exhaustive`）:
///
/// ```compile_fail,E0639
/// use fandhe_browser_render::host_process::WrappedProcTree;
/// let _ = WrappedProcTree {
///     wrapper_children: vec![],
///     init_children: vec![],
///     init_children_main_thread: vec![],
///     host: None,
///     host_descendants: vec![],
///     host_threads: 0,
///     truncated: false,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WrappedProcTree {
    /// ラッパーの子（全 tid）。期待値は ns 内 pid 1 の 1 件。
    pub wrapper_children: Vec<u32>,
    /// ns 内 pid 1 の子（全 tid）。期待値はホスト 1 件。
    pub init_children: Vec<u32>,
    /// `resolve_wrapped_pid` が見る主スレッドだけの、ns 内 pid 1 の子。
    pub init_children_main_thread: Vec<u32>,
    /// `init_children` がちょうど 1 件のときのそれ。
    pub host: Option<u32>,
    /// ホストの子孫（全 tid を幅優先で辿った順）。
    pub host_descendants: Vec<u32>,
    /// ホストのスレッド数（参考値）。
    pub host_threads: usize,
    /// 深さ・ノード数の上限で打ち切ったか。true は異常として扱う。
    pub truncated: bool,
}

impl WrappedProcTree {
    /// 1 行の `key=value` 形式（ASCII の数値と真偽値だけ。プロセス由来の文字列は載せない）。
    pub fn summary_line(&self) -> String {
        format!(
            "wrapper_children={} init_children={} init_children_main_thread={} host_descendants={} host_threads={} truncated={}",
            self.wrapper_children.len(),
            self.init_children.len(),
            self.init_children_main_thread.len(),
            self.host_descendants.len(),
            self.host_threads,
            self.truncated
        )
    }
}

/// children の本文を pid 列へ解析する（`resolve_wrapped_pid` と同じ規則: 数値以外・負数は不正）。
/// 件数が [`PROC_TREE_MAX_CHILDREN_PER_NODE`] を超えた時点で打ち切る（全件を確保してから数えない）。
fn parse_children(text: &str, path: &str) -> Result<Vec<u32>, ProcTreeError> {
    let mut out = Vec::new();
    for t in text.split_whitespace() {
        let pid = t.parse::<u32>().map_err(|_| ProcTreeError::Malformed {
            path: path.to_string(),
        })?;
        if out.len() >= PROC_TREE_MAX_CHILDREN_PER_NODE {
            return Err(ProcTreeError::TooManyChildren {
                path: path.to_string(),
            });
        }
        out.push(pid);
    }
    Ok(out)
}

/// `/proc/<pid>/task/<tid>/children` のパス。診断が読む `/proc` のパスはここで組み立てるものだけ（#1466）。
fn children_path(pid: u32, tid: u32) -> String {
    format!("/proc/{pid}/task/{tid}/children")
}

/// 本番の読み取り。モジュール内で組み立てた `/proc/.../children` のパスだけを渡す前提の
/// private なアダプタ（任意パスを読める関数は公開しない。#1466）。読めなければ `None`。
fn read_children_path(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 本番の `/proc/<pid>/task` の tid 一覧。列挙は上限 + 1 件で打ち切る（超過は `children_all` が
/// `TooManyTasks` にする。確保量に上限を設ける。#1466）。
fn list_proc_tasks(pid: u32) -> Option<Vec<u32>> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut tids: Vec<u32> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .take(PROC_TREE_MAX_TIDS_PER_NODE + 1)
        .collect();
    tids.sort_unstable();
    Some(tids)
}

/// 主スレッドの children を読む。1 回目が `None` なら短く待って 1 回だけ読み直す。
/// 走査中のプロセスがゾンビ化・終了する競合で一瞬読めない場合のフレーク抑制（#1466）。
/// 存続中のプロセスが 2 回とも読めなければ `None` のままで、呼び出し側が Err にする（fail-closed）。
///
/// 待ち時間の上限: 1 回の呼び出しで読み取りは最大 2 回、[`PROC_TREE_RETRY_DELAY`]（20ms）の sleep は最大 1 回。
/// 呼ばれるのは各ノードの主スレッド（tid == pid）の読み取りと init の主スレッドだけなので、1 回の走査の sleep の
/// 合計は最大 `20ms * (PROC_TREE_MAX_NODES + 3)`。実際に読めないのは消えたプロセスとの競合のときだけ。
/// 診断用の経路（`real_host` の `EGRESS-PROCTREE`）だけで使い、本番の rss 監視からは呼ばれない。
/// 呼び出し側の期限には従わない（必要になったら別 issue。#1473）。
fn read_children_retry(read: &dyn Fn(&str) -> Option<String>, path: &str) -> Option<String> {
    read(path).or_else(|| {
        std::thread::sleep(PROC_TREE_RETRY_DELAY);
        read(path)
    })
}

/// 全 tid の children の和集合（重複除去・出現順）と tid 数。
/// tid 数・children 数の上限を超えたら Err（無制限確保の防止。#1466）。
fn children_all(
    pid: u32,
    read: &dyn Fn(&str) -> Option<String>,
    list_tasks: &dyn Fn(u32) -> Option<Vec<u32>>,
) -> Result<(Vec<u32>, usize), ProcTreeError> {
    let tids = list_tasks(pid).ok_or(ProcTreeError::NoTasks { pid })?;
    if tids.len() > PROC_TREE_MAX_TIDS_PER_NODE {
        return Err(ProcTreeError::TooManyTasks { pid });
    }
    let mut out: Vec<u32> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    for tid in &tids {
        let path = children_path(pid, *tid);
        let text = if *tid == pid {
            read_children_retry(read, &path)
        } else {
            read(&path)
        };
        match text {
            Some(text) => {
                for c in parse_children(&text, &path)? {
                    if seen.insert(c) {
                        if out.len() >= PROC_TREE_MAX_CHILDREN_PER_NODE {
                            return Err(ProcTreeError::TooManyChildren { path });
                        }
                        out.push(c);
                    }
                }
            }
            // 主スレッドが読めないのは異常。他スレッドは列挙後に終了した競合として読み飛ばす。
            None if *tid == pid => return Err(ProcTreeError::Unreadable { path }),
            None => {}
        }
    }
    Ok((out, tids.len()))
}

/// ラッパー -> ns 内 pid 1 -> ホスト -> 子孫のプロセスツリーを辿って記録する（RENDER-3・#1460）。
///
/// [`walk_wrapped_proc_tree_live`] の実体。`read`・`list_tasks` は `/proc` の差し替え点で、
/// 単体テストだけが差し替える（任意パスを読む本番の読み取りは公開しない。#1466）。
/// ラッパーの子がちょうど 1 件でなければ、それ以降は辿らず空のまま返す（呼び出し側が件数で判定する）。
/// 深さ・ノード数に上限があり、超えたら `truncated` にして打ち切る。
fn walk_wrapped_proc_tree(
    wrapper: u32,
    read: &dyn Fn(&str) -> Option<String>,
    list_tasks: &dyn Fn(u32) -> Option<Vec<u32>>,
) -> Result<WrappedProcTree, ProcTreeError> {
    let (wrapper_children, _) = children_all(wrapper, read, list_tasks)?;
    let mut tree = WrappedProcTree {
        wrapper_children,
        init_children: Vec::new(),
        init_children_main_thread: Vec::new(),
        host: None,
        host_descendants: Vec::new(),
        host_threads: 0,
        truncated: false,
    };
    let [init] = tree.wrapper_children[..] else {
        return Ok(tree);
    };
    tree.init_children = children_all(init, read, list_tasks)?.0;
    let main_path = children_path(init, init);
    tree.init_children_main_thread = match read_children_retry(read, &main_path) {
        Some(t) => parse_children(&t, &main_path)?,
        None => return Err(ProcTreeError::Unreadable { path: main_path }),
    };
    let [host] = tree.init_children[..] else {
        return Ok(tree);
    };
    tree.host = Some(host);
    let mut visited: HashSet<u32> = HashSet::from([host]);
    let mut queue: std::collections::VecDeque<(u32, usize)> = std::collections::VecDeque::new();
    queue.push_back((host, 0));
    let mut first = true;
    while let Some((pid, depth)) = queue.pop_front() {
        let (kids, threads) = match children_all(pid, read, list_tasks) {
            Ok(v) => v,
            // 子孫が列挙後に終了した競合は子なし扱い。ホスト自身が読めないのは異常。
            Err(ProcTreeError::NoTasks { .. }) if !first => (Vec::new(), 0),
            // children の読み取り失敗も、task 一覧が再取得できない（消滅した）ときだけ競合とみなす。
            // 存続中のプロセスの読み取り異常は Unreadable のまま返す。
            Err(ProcTreeError::Unreadable { .. }) if !first && list_tasks(pid).is_none() => {
                (Vec::new(), 0)
            }
            Err(e) => return Err(e),
        };
        if first {
            tree.host_threads = threads;
            first = false;
        }
        for k in kids {
            if visited.contains(&k) {
                continue;
            }
            if depth + 1 > PROC_TREE_MAX_DEPTH || visited.len() >= PROC_TREE_MAX_NODES {
                tree.truncated = true;
                return Ok(tree);
            }
            visited.insert(k);
            tree.host_descendants.push(k);
            queue.push_back((k, depth + 1));
        }
    }
    Ok(tree)
}

/// ラッパー配下のプロセスツリーを本番の `/proc` から辿って記録する（RENDER-3・RENDER-4・#1460・#1466）。
///
/// `real_host` の隔離下結合テストが `EGRESS-PROCTREE` 行を出すための診断用の入口。引数は pid だけで、
/// 読むのは `/proc/<pid>/task` と `/proc/<pid>/task/<tid>/children` に限る（任意パスの読み取りは
/// 公開しない）。結合テスト crate からは `cfg(test)` の項目が見えず feature の追加もしないため、
/// `host_process` から `#[doc(hidden)]` で再公開している。
pub fn walk_wrapped_proc_tree_live(wrapper: u32) -> Result<WrappedProcTree, ProcTreeError> {
    walk_wrapped_proc_tree(wrapper, &read_children_path, &list_proc_tasks)
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
                let read = read_children_path;
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

    type Files = Vec<(String, String)>;
    type Tasks = Vec<(u32, Vec<u32>)>;

    type ReadFn = Box<dyn Fn(&str) -> Option<String>>;
    type TasksFn = Box<dyn Fn(u32) -> Option<Vec<u32>>>;

    fn tree_fixture(files: Files, tasks: Tasks) -> (ReadFn, TasksFn) {
        (
            Box::new(move |p: &str| files.iter().find(|(k, _)| k == p).map(|(_, v)| v.clone())),
            Box::new(move |pid: u32| {
                tasks
                    .iter()
                    .find(|(k, _)| *k == pid)
                    .map(|(_, v)| v.clone())
            }),
        )
    }

    fn f(pid: u32, tid: u32, kids: &str) -> (String, String) {
        (format!("/proc/{pid}/task/{tid}/children"), kids.to_string())
    }

    /// RENDER-3・RENDER-4（#1460）: 正常なツリー。ワーカースレッドの子も子孫に数える。
    #[test]
    fn render3_walk_wrapped_proc_tree_counts_host_descendants_over_all_threads() {
        let (r, l) = tree_fixture(
            vec![
                f(77, 77, "1234"),
                f(1234, 1234, "1235"),
                f(1235, 1235, ""),
                f(1235, 1240, "2000"),
                f(2000, 2000, "2001"),
                f(2001, 2001, ""),
            ],
            vec![
                (77, vec![77]),
                (1234, vec![1234]),
                (1235, vec![1235, 1240]),
                (2000, vec![2000]),
                (2001, vec![2001]),
            ],
        );
        let t = walk_wrapped_proc_tree(77, &*r, &*l).expect("tree");
        assert_eq!(t.wrapper_children, vec![1234]);
        assert_eq!(t.init_children, vec![1235]);
        assert_eq!(t.init_children_main_thread, vec![1235]);
        assert_eq!(t.host, Some(1235));
        assert_eq!(t.host_descendants, vec![2000, 2001]);
        assert_eq!(t.host_threads, 2);
        assert!(!t.truncated);
        assert_eq!(
            t.summary_line(),
            "wrapper_children=1 init_children=1 init_children_main_thread=1 host_descendants=2 host_threads=2 truncated=false"
        );
    }

    /// RENDER-3（#1460）: 孤児の付け替え（ns 内 pid 1 の子が 2 件）は host なし。
    /// 主スレッド以外にだけ子がいる場合は主スレッドの数と全 tid の数が食い違う。
    #[test]
    fn render3_walk_wrapped_proc_tree_detects_broken_premise() {
        let (r, l) = tree_fixture(
            vec![f(77, 77, "1234"), f(1234, 1234, "1235 1300")],
            vec![(77, vec![77]), (1234, vec![1234])],
        );
        let t = walk_wrapped_proc_tree(77, &*r, &*l).expect("tree");
        assert_eq!(t.init_children, vec![1235, 1300]);
        assert_eq!(t.host, None);
        assert!(t.host_descendants.is_empty());

        let (r, l) = tree_fixture(
            vec![
                f(77, 77, "1234"),
                f(1234, 1234, ""),
                f(1234, 1250, "1235"),
                f(1235, 1235, ""),
            ],
            vec![(77, vec![77]), (1234, vec![1234, 1250]), (1235, vec![1235])],
        );
        let t = walk_wrapped_proc_tree(77, &*r, &*l).expect("tree");
        assert_eq!(t.init_children, vec![1235]);
        assert!(t.init_children_main_thread.is_empty());
    }

    /// RENDER-3（#1460）: 読めない・形式不正は Err（推測で埋めない）。
    #[test]
    fn render3_walk_wrapped_proc_tree_rejects_unreadable_and_malformed() {
        let (r, l) = tree_fixture(vec![], vec![]);
        assert_eq!(
            walk_wrapped_proc_tree(77, &*r, &*l),
            Err(ProcTreeError::NoTasks { pid: 77 })
        );
        let (r, l) = tree_fixture(vec![], vec![(77, vec![77])]);
        assert_eq!(
            walk_wrapped_proc_tree(77, &*r, &*l),
            Err(ProcTreeError::Unreadable {
                path: "/proc/77/task/77/children".to_string()
            })
        );
        for bad in ["abc", "-5"] {
            let (r, l) = tree_fixture(vec![f(77, 77, bad)], vec![(77, vec![77])]);
            assert_eq!(
                walk_wrapped_proc_tree(77, &*r, &*l),
                Err(ProcTreeError::Malformed {
                    path: "/proc/77/task/77/children".to_string()
                }),
                "{bad}"
            );
        }
    }

    /// RENDER-3（#1460）: 子孫が task 列挙後・children 読み取り前に消えても子なしで続行する。
    /// 存続中の子孫の読み取り失敗は Unreadable のまま。
    #[test]
    fn render3_walk_wrapped_proc_tree_tolerates_descendant_vanishing_before_children_read() {
        let files = [
            f(77, 77, "1234"),
            f(1234, 1234, "1235"),
            f(1235, 1235, "2000"),
        ];
        let calls = std::cell::Cell::new(0u32);
        let read = move |p: &str| files.iter().find(|(k, _)| k == p).map(|(_, v)| v.clone());
        let list = |pid: u32| match pid {
            77 => Some(vec![77]),
            1234 => Some(vec![1234]),
            1235 => Some(vec![1235]),
            // 2000 は 1 回目の列挙だけ成功し、その後は消滅（children ファイルも無い）。
            2000 => {
                calls.set(calls.get() + 1);
                (calls.get() == 1).then(|| vec![2000])
            }
            _ => None,
        };
        let t = walk_wrapped_proc_tree(77, &read, &list).expect("tree");
        assert_eq!(t.host_descendants, vec![2000]);
        assert!(!t.truncated);

        // 存続中（再列挙できる）なのに children が読めないのは異常。
        let list_alive = |pid: u32| match pid {
            77 => Some(vec![77]),
            1234 => Some(vec![1234]),
            1235 => Some(vec![1235]),
            2000 => Some(vec![2000]),
            _ => None,
        };
        assert_eq!(
            walk_wrapped_proc_tree(77, &read, &list_alive),
            Err(ProcTreeError::Unreadable {
                path: "/proc/2000/task/2000/children".to_string()
            })
        );
    }

    /// RENDER-3（#1466）: 読む `/proc` のパスは children の形式に限られる。
    #[test]
    fn render3_children_path_is_proc_task_children() {
        assert_eq!(children_path(12, 34), "/proc/12/task/34/children");
    }

    /// RENDER-3（#1466）: 存在しない pid は決定的に NoTasks（本番の入口が pid だけで動く）。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_walk_live_rejects_missing_pid() {
        assert_eq!(
            walk_wrapped_proc_tree_live(u32::MAX),
            Err(ProcTreeError::NoTasks { pid: u32::MAX })
        );
    }

    /// RENDER-3（#1466）: tid 数が上限 + 1 なら TooManyTasks（上限ちょうどは通る）。
    #[test]
    fn render3_walk_wrapped_proc_tree_rejects_too_many_tasks() {
        let read = |p: &str| (p == "/proc/77/task/77/children").then(String::new);
        let over = |pid: u32| {
            (pid == 77).then(|| {
                (0..=PROC_TREE_MAX_TIDS_PER_NODE as u32)
                    .map(|i| 77 + i)
                    .collect()
            })
        };
        assert_eq!(
            walk_wrapped_proc_tree(77, &read, &over),
            Err(ProcTreeError::TooManyTasks { pid: 77 })
        );
        let exact = |pid: u32| {
            (pid == 77).then(|| {
                (0..PROC_TREE_MAX_TIDS_PER_NODE as u32)
                    .map(|i| 77 + i)
                    .collect()
            })
        };
        let t = walk_wrapped_proc_tree(77, &read, &exact).expect("tree");
        assert!(t.wrapper_children.is_empty());
    }

    /// RENDER-3（#1466）: children が上限 + 1 件なら TooManyChildren（上限ちょうどは通る）。
    #[test]
    fn render3_walk_wrapped_proc_tree_rejects_too_many_children() {
        let body = |n: usize| {
            (1000..1000 + n as u32)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let list = |pid: u32| (pid == 77).then(|| vec![77]);
        let over = body(PROC_TREE_MAX_CHILDREN_PER_NODE + 1);
        let read = |p: &str| (p == "/proc/77/task/77/children").then(|| over.clone());
        assert_eq!(
            walk_wrapped_proc_tree(77, &read, &list),
            Err(ProcTreeError::TooManyChildren {
                path: "/proc/77/task/77/children".to_string()
            })
        );
        let exact = body(PROC_TREE_MAX_CHILDREN_PER_NODE);
        let read = |p: &str| (p == "/proc/77/task/77/children").then(|| exact.clone());
        let t = walk_wrapped_proc_tree(77, &read, &list).expect("tree");
        assert_eq!(t.wrapper_children.len(), PROC_TREE_MAX_CHILDREN_PER_NODE);
    }

    /// RENDER-3（#1466）: 主スレッドの children は 1 回だけ読み直す。常に読めなければ従来どおり Unreadable。
    #[test]
    fn render3_walk_wrapped_proc_tree_retries_main_thread_read_once() {
        let calls = std::cell::Cell::new(0u32);
        let flaky = |p: &str| {
            if p != "/proc/77/task/77/children" {
                return None;
            }
            calls.set(calls.get() + 1);
            (calls.get() >= 2).then(|| "1234".to_string())
        };
        let list = |pid: u32| (pid == 77).then(|| vec![77]);
        // ラッパーの子 1234 の task 一覧は無い（NoTasks）ので、1 回目の読み直しの成否だけ見る。
        assert_eq!(
            walk_wrapped_proc_tree(77, &flaky, &list),
            Err(ProcTreeError::NoTasks { pid: 1234 })
        );
        calls.set(0);
        let list = |pid: u32| matches!(pid, 77 | 1234).then(|| vec![pid]);
        let flaky2 = |p: &str| match p {
            "/proc/77/task/77/children" => {
                calls.set(calls.get() + 1);
                (calls.get() >= 2).then(|| "1234".to_string())
            }
            "/proc/1234/task/1234/children" => Some(String::new()),
            _ => None,
        };
        let t = walk_wrapped_proc_tree(77, &flaky2, &list).expect("tree");
        assert_eq!(t.wrapper_children, vec![1234]);
        assert_eq!(calls.get(), 2);

        let never = |_: &str| None;
        assert_eq!(
            walk_wrapped_proc_tree(77, &never, &list),
            Err(ProcTreeError::Unreadable {
                path: "/proc/77/task/77/children".to_string()
            })
        );
    }

    /// RENDER-3（#1473）: 読み直しは最大 1 回（読み取り 2 回）。常に読めない path でも 3 回目は読まない。
    #[test]
    fn render3_read_children_retry_is_bounded_to_two_reads() {
        let calls = std::cell::Cell::new(0u32);
        let never = |_: &str| {
            calls.set(calls.get() + 1);
            None
        };
        assert_eq!(read_children_retry(&never, "/proc/1/task/1/children"), None);
        assert_eq!(calls.get(), 2);
        // 1 回目で読めれば読み直さない
        calls.set(0);
        let ok = |_: &str| {
            calls.set(calls.get() + 1);
            Some("5".to_string())
        };
        assert_eq!(
            read_children_retry(&ok, "/proc/1/task/1/children"),
            Some("5".to_string())
        );
        assert_eq!(calls.get(), 1);
    }

    /// RENDER-3（#1460）: 循環は二重に数えず、深すぎるツリーは truncated で打ち切る。
    #[test]
    fn render3_walk_wrapped_proc_tree_handles_cycle_and_depth_limit() {
        let (r, l) = tree_fixture(
            vec![
                f(77, 77, "1234"),
                f(1234, 1234, "1235"),
                f(1235, 1235, "2000"),
                f(2000, 2000, "2001"),
                f(2001, 2001, "2000 1235"),
            ],
            vec![
                (77, vec![77]),
                (1234, vec![1234]),
                (1235, vec![1235]),
                (2000, vec![2000]),
                (2001, vec![2001]),
            ],
        );
        let t = walk_wrapped_proc_tree(77, &*r, &*l).expect("tree");
        assert_eq!(t.host_descendants, vec![2000, 2001]);
        assert!(!t.truncated);

        // ホスト 1235 から一直線に続く（pid n の子は n + 1）。
        let read = |p: &str| -> Option<String> {
            let pid: u32 = p.split('/').nth(2)?.parse().ok()?;
            Some(match pid {
                77 => "1234".to_string(),
                1234 => "1235".to_string(),
                n => (n + 1).to_string(),
            })
        };
        let tasks = |pid: u32| Some(vec![pid]);
        let t = walk_wrapped_proc_tree(77, &read, &tasks).expect("tree");
        assert!(t.truncated);
        assert_eq!(t.host_descendants.len(), PROC_TREE_MAX_DEPTH);
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
