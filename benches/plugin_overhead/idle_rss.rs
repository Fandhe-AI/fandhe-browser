//! アイドル RSS 増分の計測手順（TASK-95.1・Issue #375・`PLUG-3`・`MS-9`）。
//!
//! `plugin_overhead.rs` の main から呼ばれる。ベンチ自身（in-process の ai ルータ = 本体役）を根とする
//! プロセスツリーの RSS を `ps` で採取し、mcp 起動前 (a) と起動後 (b) の差を求める。mcp は子プロセスなので
//! (b) の木に含まれる。PoC-15 の手法（静置後に一定間隔で複数回採取して中央値）を移植している。
//! `ps` は固定の絶対パス候補だけを使い（PATH を探索しない）、出力量と実行時間に上限を設ける。

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::app_host::AppHost;
use crate::mcp_client::McpSession;
use crate::options::Options;
use crate::rss::{
    IdleRssMeasurement, PsRow, contains_in_tree, parse_ps_table, read_limited, tree_rss,
};

const PS_CANDIDATES: [&str; 2] = ["/bin/ps", "/usr/bin/ps"];
const PS_MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// `ps` を 1 回実行し、(全プロセス表, 起動した `ps` 自身の pid) を返す。
/// `ps` 自身はベンチの子として表に載るため、呼び出し側が合算から除外できるよう pid を返す。
fn snapshot() -> Result<(Vec<PsRow>, u32), String> {
    let ps = PS_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .ok_or_else(|| "ps(1) not found".to_string())?;
    let mut child = Command::new(ps)
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "rss="])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "failed to spawn ps".to_string())?;
    let ps_pid = child.id();
    let out = child.stdout.take();
    // 出力は別スレッドで上限付きに読む（パイプ満杯で ps が止まらないように）。
    let reader = std::thread::spawn(move || match out {
        Some(o) => read_limited(o, PS_MAX_OUTPUT_BYTES),
        None => Err("ps stdout is unavailable".to_string()),
    });
    let deadline = Instant::now() + PS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ps timed out".to_string());
            }
        }
    };
    let read_result = reader.join().map_err(|_| "ps reader failed".to_string())?;
    if !status.success() {
        return Err("ps exited with failure".to_string());
    }
    // 読み取り途中の I/O エラーは部分的な表を成功扱いにしないため、ここで採取失敗として伝播する。
    let buf = read_result?;
    if buf.len() as u64 > PS_MAX_OUTPUT_BYTES {
        return Err("ps output exceeds 4 MiB".to_string());
    }
    let text = String::from_utf8(buf).map_err(|_| "ps output is not UTF-8".to_string())?;
    let rows = parse_ps_table(&text).map_err(|e| e.to_string())?;
    Ok((rows, ps_pid))
}

/// (a) 本体単体と (b) 本体＋プラグインのアイドル RSS を `opts.samples` 回ずつ採取する。
pub fn measure(bin: &Path, opts: &Options) -> Result<IdleRssMeasurement, String> {
    let me = std::process::id();
    let settle = Duration::from_millis(opts.settle_ms);
    let interval = Duration::from_millis(opts.interval_ms);

    eprintln!("starting bench host");
    let host = AppHost::start()?;
    // 採取機構（ps 実行用スレッド・バッファ）の確保分を (a) の基準へ含めるため、捨て打ちを 1 回行う。
    snapshot()?;
    std::thread::sleep(settle);
    let mut host_only = Vec::with_capacity(opts.samples);
    for i in 0..opts.samples {
        if i > 0 {
            std::thread::sleep(interval);
        }
        let (rows, ps_pid) = snapshot()?;
        let t = tree_rss(&rows, me, Some(ps_pid)).map_err(|e| e.to_string())?;
        eprintln!(
            "host-only sample {i}: {} KiB ({} processes)",
            t.rss_kib, t.processes
        );
        host_only.push(t.rss_kib);
    }

    eprintln!("starting plugin");
    let session = McpSession::start(bin, host.addr())?;
    let mcp_pid = session.pid();
    std::thread::sleep(settle);
    let mut both = Vec::with_capacity(opts.samples);
    let mut plugin = Vec::with_capacity(opts.samples);
    for i in 0..opts.samples {
        if i > 0 {
            std::thread::sleep(interval);
        }
        let (rows, ps_pid) = snapshot()?;
        if !contains_in_tree(&rows, me, mcp_pid) {
            return Err("plugin process is not in the bench process tree".to_string());
        }
        let b = tree_rss(&rows, me, Some(ps_pid)).map_err(|e| e.to_string())?;
        let p = tree_rss(&rows, mcp_pid, Some(ps_pid)).map_err(|e| e.to_string())?;
        eprintln!(
            "host+plugin sample {i}: {} KiB, plugin alone: {} KiB",
            b.rss_kib, p.rss_kib
        );
        both.push(b.rss_kib);
        plugin.push(p.rss_kib);
    }
    drop(session);
    drop(host);
    Ok(IdleRssMeasurement {
        host_only_kib: host_only,
        host_plus_plugin_kib: both,
        plugin_kib: plugin,
    })
}
