//! control プローブ（TASK-100.6・Issue #270・`PLUG-8`）。
//!
//! プロファイル機構を参照しない基準側。`profiled.rs` と同じ処理から
//! プロファイル読み込みだけを除いたもので、`benches/cssom_profile_cost.sh` が
//! 両者のサイズ・RSS 差を取る。計測専用で出荷物ではない。

#[path = "common.rs"]
mod common;

fn main() -> std::process::ExitCode {
    if let Err(e) = common::run_minimal_workload() {
        eprintln!("error: {e}");
        return std::process::ExitCode::from(2);
    }
    common::report("control", 0)
}
