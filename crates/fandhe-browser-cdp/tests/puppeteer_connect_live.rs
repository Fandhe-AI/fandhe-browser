//! 実 Puppeteer で `puppeteer.connect({browserWSEndpoint})` を実行し、結果を構造化回収する
//! 明示実行ターゲット（TASK-45.1・#480、ビヘイビア `CDP-3`・MS-4）。
//!
//! `Cargo.toml` で `test = false` のため `cargo test --workspace` では走らない。
//! `make test-puppeteer-connect`（= `cargo test -p fandhe-browser-cdp --test
//! puppeteer_connect_live`）で実行する。Puppeteer 未導入なら skip せず **失敗** する。
//!
//! 本テストが assert するのは「構造化結果を回収できたこと」だけで、接続の成否は評価しない
//! （成否の到達確認は #481（TASK-45.2）、レポートは #482（TASK-45.3））。#481 はここへ
//! ステップを足す。

#![cfg(unix)]

mod script_harness;

use script_harness::{
    DEFAULT_DEADLINE, ScriptOutcome, browser_ws_endpoint, preflight_puppeteer,
    puppeteer_harness_dir, run_script, start_server,
};

#[test]
fn puppeteer_connect_result_is_collected() {
    let cmd = match preflight_puppeteer(&puppeteer_harness_dir()) {
        Ok(c) => c,
        Err(e) => panic!("Puppeteer harness is unavailable: {e:?}"),
    };
    let server = start_server();
    let ep = browser_ws_endpoint(server.addr);
    match run_script(&cmd, &ep, DEFAULT_DEADLINE) {
        ScriptOutcome::Completed { ok, step, error } => {
            eprintln!("puppeteer result: ok={ok} step={step} error={error:?}");
            assert_eq!(step, "connect");
        }
        other => panic!("no structured result collected: {other:?}"),
    }
}
