//! `plugin_overhead` ベンチの引数解析（TASK-95.1・Issue #375・`PLUG-3`）。
//!
//! `plugin_overhead.rs` の main が `cargo bench ... -- <args>` の引数を渡す。純関数で I/O を持たず、
//! 資源枯渇を防ぐため回数・待ち時間に上限を設ける。`--metric` は TASK-95.2・95.3 が指標を足す拡張点。

/// 計測する指標。`idle-rss`（TASK-95.1）・`call-latency`（TASK-95.3）。cold start は TASK-95.2。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    IdleRss,
    CallLatency,
}

/// 解析済みの引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub metrics: Vec<Metric>,
    pub samples: usize,
    pub settle_ms: u64,
    pub interval_ms: u64,
    /// `call-latency` の計測呼び出し回数（既定 30 は `PLUG-3` の n=30）。
    pub calls: usize,
}

fn int_arg<T: std::str::FromStr + PartialOrd>(
    flag: &str,
    value: Option<&String>,
    min: T,
    max: T,
) -> Result<T, String> {
    let v: T = value
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{flag} requires an integer value"))?;
    if v < min || v > max {
        return Err(format!("{flag} is out of range"));
    }
    Ok(v)
}

impl Options {
    /// 既定: `idle-rss`・3 サンプル・静置 800ms・間隔 200ms（PoC-15 と同じ）。
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut metrics: Vec<Metric> = Vec::new();
        let mut samples = 3usize;
        let mut settle_ms = 800u64;
        let mut interval_ms = 200u64;
        let mut calls = 30usize;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                // cargo bench が付ける引数。
                "--bench" => {}
                "--metric" => match it.next().map(String::as_str) {
                    Some("idle-rss") => {
                        if !metrics.contains(&Metric::IdleRss) {
                            metrics.push(Metric::IdleRss);
                        }
                    }
                    Some("call-latency") => {
                        if !metrics.contains(&Metric::CallLatency) {
                            metrics.push(Metric::CallLatency);
                        }
                    }
                    Some(_) => return Err("unknown metric".to_string()),
                    None => return Err("--metric requires a value".to_string()),
                },
                "--samples" => samples = int_arg("--samples", it.next(), 1, 20)?,
                "--settle-ms" => settle_ms = int_arg("--settle-ms", it.next(), 0, 30_000)?,
                "--interval-ms" => interval_ms = int_arg("--interval-ms", it.next(), 0, 5_000)?,
                "--calls" => calls = int_arg("--calls", it.next(), 1, 1_000)?,
                _ => return Err("unknown argument".to_string()),
            }
        }
        if metrics.is_empty() {
            metrics.push(Metric::IdleRss);
        }
        Ok(Self {
            metrics,
            samples,
            settle_ms,
            interval_ms,
            calls,
        })
    }
}
