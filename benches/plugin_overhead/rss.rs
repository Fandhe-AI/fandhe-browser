//! アイドル RSS 増分計測の純関数群（TASK-95.1・Issue #375・`PLUG-3`・`MS-9`）。
//!
//! `idle_rss.rs` が `ps` を実行して得た表を、ここで解析・ツリー合算・中央値・MB 換算・TSV 整形する。
//! I/O を持たないため全 OS でユニットテストできる（`rss_tests.rs`）。結果は `IdleRssMeasurement` として
//! 構造のまま返し、整形は別関数に分ける（TASK-95.4 が同じ値を TASK-10 形式へ流せるように。REPAIR-4）。
//! 外部コマンドの出力は untrusted として扱い、添字アクセス・`unwrap` を使わない。

use std::collections::{HashMap, HashSet};

/// `ps` 1 行分（pid・親 pid・RSS KiB）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsRow {
    pub pid: u32,
    pub ppid: u32,
    pub rss_kib: u64,
}

/// プロセスツリー合算の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRss {
    pub rss_kib: u64,
    pub processes: usize,
}

/// 解析・合算の失敗。メッセージは固定の英語で、外部出力の本文を含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RssError {
    MalformedRow,
    RootMissing,
    Overflow,
    NoSamples,
}

impl std::fmt::Display for RssError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::MalformedRow => "malformed ps output row",
            Self::RootMissing => "root process is not in the ps table",
            Self::Overflow => "rss sum overflowed",
            Self::NoSamples => "no samples collected",
        };
        f.write_str(s)
    }
}

/// `ps -A -o pid= -o ppid= -o rss=` の出力を解析する。空行は無視し、3 列の非負整数でない行は失敗にする。
pub fn parse_ps_table(text: &str) -> Result<Vec<PsRow>, RssError> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(pid) = it.next() else { continue };
        let ppid = it.next().ok_or(RssError::MalformedRow)?;
        let rss = it.next().ok_or(RssError::MalformedRow)?;
        if it.next().is_some() {
            return Err(RssError::MalformedRow);
        }
        rows.push(PsRow {
            pid: pid.parse().map_err(|_| RssError::MalformedRow)?,
            ppid: ppid.parse().map_err(|_| RssError::MalformedRow)?,
            rss_kib: rss.parse().map_err(|_| RssError::MalformedRow)?,
        });
    }
    Ok(rows)
}

/// `root` を根とする子孫（根を含む）の RSS 合計。`exclude` の pid（採取に使った `ps` 自身）は除く。
/// 根が表に無ければ失敗（プラグインが死んでいる場合に 0 を成功として返さない）。ppid の循環は visited で止める。
pub fn tree_rss(rows: &[PsRow], root: u32, exclude: Option<u32>) -> Result<TreeRss, RssError> {
    if !rows.iter().any(|r| r.pid == root) {
        return Err(RssError::RootMissing);
    }
    let mut children: HashMap<u32, Vec<&PsRow>> = HashMap::new();
    for r in rows {
        children.entry(r.ppid).or_default().push(r);
    }
    let by_pid: HashMap<u32, &PsRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let mut visited: HashSet<u32> = HashSet::new();
    let mut stack = vec![root];
    let mut sum: u64 = 0;
    let mut processes = 0usize;
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) || Some(pid) == exclude {
            continue;
        }
        if let Some(r) = by_pid.get(&pid) {
            sum = sum.checked_add(r.rss_kib).ok_or(RssError::Overflow)?;
            processes += 1;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().map(|k| k.pid));
        }
    }
    Ok(TreeRss {
        rss_kib: sum,
        processes,
    })
}

/// `pid` が `root` のツリーに含まれるか。
pub fn contains_in_tree(rows: &[PsRow], root: u32, pid: u32) -> bool {
    let by_pid: HashMap<u32, &PsRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut cur = pid;
    while seen.insert(cur) {
        if cur == root {
            return true;
        }
        match by_pid.get(&cur) {
            Some(r) => cur = r.ppid,
            None => return false,
        }
    }
    false
}

/// 中央値（偶数件は中央 2 件の平均）。空なら `None`。
pub fn median(values: &[u64]) -> Option<f64> {
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return None;
    }
    let mid = n / 2;
    if n % 2 == 1 {
        v.get(mid).map(|x| *x as f64)
    } else {
        let lo = v.get(mid.checked_sub(1)?)?;
        let hi = v.get(mid)?;
        Some((*lo as f64 + *hi as f64) / 2.0)
    }
}

/// KiB を MB へ換算する（PoC-15・`PLUG3_TARGETS` と同じ KiB ÷ 1024）。
pub fn kib_to_mb(kib: f64) -> f64 {
    kib / 1024.0
}

/// アイドル RSS 増分の採取結果（各系列はサンプルごとの KiB）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleRssMeasurement {
    /// (a) 本体（ホスト）単体。
    pub host_only_kib: Vec<u64>,
    /// (b) 本体＋プラグイン。
    pub host_plus_plugin_kib: Vec<u64>,
    /// プラグイン（mcp）単体の内訳。
    pub plugin_kib: Vec<u64>,
}

impl IdleRssMeasurement {
    /// 中央値(b) − 中央値(a)（KiB・符号付き。負でも丸めない）。
    pub fn increment_kib(&self) -> Option<f64> {
        Some(median(&self.host_plus_plugin_kib)? - median(&self.host_only_kib)?)
    }

    /// 増分の MB 換算。
    pub fn increment_mb(&self) -> Option<f64> {
        self.increment_kib().map(kib_to_mb)
    }
}

/// 出力 TSV の列見出し行。
pub const IDLE_RSS_HEADER: &str =
    "metric\tsamples\thost_only_kib\thost_plus_plugin_kib\tplugin_kib\tincrement_kib\tincrement_mb";

/// 3 系列の中央値と増分を TSV（コメント行つき）に整形する。`target_bytes` は参考値で、判定はしない。
pub fn render_idle_rss(m: &IdleRssMeasurement, target_bytes: u64) -> Result<String, RssError> {
    let a = median(&m.host_only_kib).ok_or(RssError::NoSamples)?;
    let b = median(&m.host_plus_plugin_kib).ok_or(RssError::NoSamples)?;
    let p = median(&m.plugin_kib).ok_or(RssError::NoSamples)?;
    let inc = m.increment_kib().ok_or(RssError::NoSamples)?;
    let inc_mb = m.increment_mb().ok_or(RssError::NoSamples)?;
    let target_mb = target_bytes as f64 / (1024.0 * 1024.0);
    Ok(format!(
        "# plugin overhead: idle RSS increment (PLUG-3)\n\
         # rss is KiB reported by ps(1); mb = KiB / 1024\n\
         {IDLE_RSS_HEADER}\n\
         idle_rss\t{}\t{a}\t{b}\t{p}\t{inc}\t{:.2}\n\
         # reference target (PLUG-3, official tier): increment_mb <= {target_mb:.2} (not judged here)",
        m.host_plus_plugin_kib.len(),
        inc_mb,
    ))
}
