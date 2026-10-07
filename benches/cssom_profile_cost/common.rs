//! CSSOM プロファイル計測用プローブの共通部（TASK-100.6・Issue #270・`PLUG-8`）。
//!
//! `control.rs` / `profiled.rs` が `#[path]` で取り込む。固定の小さな HTML を
//! core の実コードに通し、処理後の常駐サイズ（アイドル RSS）を自己報告する。
//! 呼び出し元は `benches/cssom_profile_cost.sh`。計測専用で出荷物ではない。
//! 入力はソース埋め込みの固定文字列のみで、URL・パス・引数は受け取らない。

use fandhe_browser_core::cssom::collect_document_styles;
use fandhe_browser_core::{ParseOptions, parse_document};

const HTML: &str = "<!doctype html><html><head><style>p{color:red;margin:0}\
.a{display:block}</style></head><body><p class=\"a\">probe</p></body></html>";

/// 共通の最小処理。収集した styleSheet 数を返す（core の実コードを control にもリンクさせる）。
pub fn run_minimal_workload() -> Result<usize, String> {
    let parsed = parse_document(HTML, &ParseOptions::default()).map_err(|e| e.to_string())?;
    let styles = collect_document_styles(&parsed.document).map_err(|e| e.to_string())?;
    Ok(std::hint::black_box(styles.style_sheets().len()))
}

/// `/proc/self/status` の `VmRSS`（KiB）を返す。Linux 以外は偽の値を出さずエラーにする。
#[cfg(target_os = "linux")]
pub fn current_rss_kib() -> Result<u64, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open("/proc/self/status")
        .and_then(|f| f.take(65536).read_to_end(&mut buf))
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().strip_suffix("kB"))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or_else(|| "VmRSS not found".to_string())
}

/// Linux 以外では RSS を測れないため、偽の値を出さずエラーにする（REPAIR-3）。
#[cfg(not(target_os = "linux"))]
pub fn current_rss_kib() -> Result<u64, String> {
    Err("RSS measurement is only supported on Linux".to_string())
}

/// 1 行の固定形式で結果を出力する。
pub fn report(variant: &str, profiles: u32) -> std::process::ExitCode {
    match current_rss_kib() {
        Ok(rss) => {
            println!("cssom-profile-cost: variant={variant} rss_kib={rss} profiles={profiles}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::from(2)
        }
    }
}
