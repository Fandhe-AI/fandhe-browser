//! profiled プローブ（TASK-100.6・Issue #270・`PLUG-8`）。
//!
//! control と同じ処理に加え、Chrome / Safari の両プロファイルを読み込み
//! （`load_profile`）、各 1 回 `property_support` を照会してから RSS を報告する。
//! 「読み込み」のコストだけを測るため cascade / apply は行わない。
//! 呼び出し元は `benches/cssom_profile_cost.sh`。計測専用で出荷物ではない。

#[path = "common.rs"]
mod common;

use fandhe_browser_core::cssom_profile::{BrowserProfile, load_profile};

fn main() -> std::process::ExitCode {
    if let Err(e) = common::run_minimal_workload() {
        eprintln!("error: {e}");
        return std::process::ExitCode::from(2);
    }
    for profile in BrowserProfile::ALL {
        match load_profile(profile) {
            Ok(table) => {
                std::hint::black_box(table.property_support("speak"));
            }
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    common::report("profiled", 2)
}
