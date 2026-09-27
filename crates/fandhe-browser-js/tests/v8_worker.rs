//! JS プロセス分離（`JS-1`・`TASK-29`・Issue #503「JS プロセス分離」設計書
//! §7 W6）の結合テスト。実際に子プロセスを起動し、`V8ProcessEngine`（親）
//! と `super::worker`（子。同じテストバイナリの自己再実行）の間で
//! ハンドシェイク・評価・タイムアウト・OOM・プロトコル違反までを通す。
//!
//! `harness = false`（`Cargo.toml` 参照）で独自の `main` を持つ。本 `main`
//! の**先頭**（他の何よりも前）で
//! [`fandhe_browser_js::run_js_worker_if_requested`] を呼ぶ。これにより、
//! このテストバイナリ自身が「子プロセス」役を兼ねる（設計書 §3.1
//! 「テストでは `current_exe()` が libtest のハーネスを指し、ハーネスは
//! フックを呼ばない」問題への対処。`crates/fandhe-browser-core` の
//! `competitor_lightpanda_measure` 結合テストと同じ発想: 対象プロセスを
//! 模したローカルプロセスを、自分自身の再実行で作る）。
//!
//! 子プロセス役のときの stdout は [`fandhe_browser_js`] のプロトコル
//! フレーム専用であり、通常の libtest ハーネスが書く "running N tests"
//! 等の文字列が混入すると親側のフレーム読み取りが壊れるため、
//! `harness = false` にしている。

use std::process::ExitCode;

use fandhe_browser_js::process_engine::{V8ProcessEngine, WorkerSpawnConfigForTest};
use fandhe_browser_js::{EvaluateOptions, JsEngineError, JsValue};

/// テスト用に小さいヒープ上限を渡す（Issue #503 設計書 §7 W6「テスト用に
/// 小さいヒープ上限を渡す経路（テスト専用。本番では無効）」）。本番の
/// 128 MiB では OOM の再現に長い時間がかかるため、高速化のため縮小する。
const TEST_HEAP_LIMIT_BYTES: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    // 最優先: このプロセスが子プロセスとして起動されたものであれば、
    // ワーカーとして動作してそのまま終了する（設計書 §3.1）。
    if let Some(code) = fandhe_browser_js::run_js_worker_if_requested() {
        return code;
    }

    eprintln!("case: js_1_normal_evaluation_returns_expected_value");
    js_1_normal_evaluation_returns_expected_value();
    eprintln!("case: js_1_state_persists_across_evaluations");
    js_1_state_persists_across_evaluations();
    eprintln!("case: js_1_infinite_loop_times_out_but_context_survives");
    js_1_infinite_loop_times_out_but_context_survives();
    eprintln!("case: js_1_oom_returns_resource_limit_exceeded_and_next_eval_uses_fresh_context");
    js_1_oom_returns_resource_limit_exceeded_and_next_eval_uses_fresh_context();
    eprintln!("case: js_1_handshake_failure_is_engine_unavailable");
    js_1_handshake_failure_is_engine_unavailable();
    eprintln!("case: js_1_protocol_violation_discards_context_and_recovers");
    js_1_protocol_violation_discards_context_and_recovers();
    eprintln!("v8_worker: all cases passed");
    ExitCode::SUCCESS
}

/// JS-1・Issue #503 W6 観点 1「通常の評価」: 単純な算術式が期待どおりの
/// [`JsValue::Number`] を返すこと。
fn js_1_normal_evaluation_returns_expected_value() {
    let mut engine = V8ProcessEngine::new();
    let result = engine
        .evaluate_script("1 + 2 * 3", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("arithmetic expression must evaluate successfully: {err}"));
    assert_eq!(result, JsValue::Number(7.0));
}

/// JS-1・Issue #503 W6 観点 2「評価をまたいだ状態の保持」: `var` で定義
/// した変数が、子プロセスへの分離後も評価呼び出しをまたいで参照できる
/// こと（永続 Context が子の寿命で保たれることの確認。設計書 §3.2）。
fn js_1_state_persists_across_evaluations() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("var counter = 10;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("variable declaration must evaluate successfully: {err}"));
    let result = engine
        .evaluate_script("counter + 5", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("previously declared variable must still be visible: {err}"));
    assert_eq!(result, JsValue::Number(15.0));
}

/// JS-1・Issue #503 W6 観点 3「無限ループがタイムアウトしても Context が
/// 残ること」: 子の監視スレッド（watchdog）による打ち切りでは、子
/// プロセス自体は生き続け、それ以前に定義したグローバル変数も残る
/// （設計書 §3.3 の対応表 1 行目。メッセージに "context was discarded"
/// を含まないことも確認する）。
fn js_1_infinite_loop_times_out_but_context_survives() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("var marker = 123;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("marker declaration must evaluate successfully: {err}"));

    match engine.evaluate_script("while (true) {}", &EvaluateOptions::default()) {
        Err(JsEngineError::Timeout(msg)) => {
            assert!(
                !msg.contains("context was discarded"),
                "watchdog-based timeout must not discard the context, got: {msg}"
            );
        }
        other => panic!("expected Timeout for an infinite loop, got: {other:?}"),
    }

    let result = engine
        .evaluate_script("marker", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!("marker must still be visible after a watchdog timeout: {err}")
        });
    assert_eq!(result, JsValue::Number(123.0));
}

/// JS-1・Issue #503 W6 観点 4「OOM で ResourceLimitExceeded になり、次の
/// 評価が新しい Context で成功すること」: ヒープ上限に達すると子
/// プロセスが V8 の既定の fatal OOM で終了し、親がこれを検出して
/// [`JsEngineError::ResourceLimitExceeded`] へ変換すること
/// （メッセージに "context was discarded" を含む）。その後の評価は
/// 新しい子プロセス・新しい Context で成功し、以前の Context にだけ
/// 存在した変数はもう見えないこと。
fn js_1_oom_returns_resource_limit_exceeded_and_next_eval_uses_fresh_context() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: Some(TEST_HEAP_LIMIT_BYTES),
        protocol_version_override: None,
    });
    engine
        .evaluate_script("var before_oom = 999;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("marker declaration must evaluate successfully: {err}"));

    let oom_script = "var chunks = []; while (true) { chunks.push(new Array(1e6).fill(0)); }";
    match engine.evaluate_script(oom_script, &EvaluateOptions::default()) {
        Err(JsEngineError::ResourceLimitExceeded(msg)) => {
            assert!(
                msg.contains("context was discarded"),
                "OOM must discard the context, got: {msg}"
            );
        }
        other => {
            panic!("expected ResourceLimitExceeded for a heap-exhausting script, got: {other:?}")
        }
    }

    let result = engine
        .evaluate_script("40 + 2", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("engine must recover with a fresh child after OOM: {err}"));
    assert_eq!(result, JsValue::Number(42.0));

    match engine.evaluate_script("before_oom", &EvaluateOptions::default()) {
        Err(JsEngineError::EvaluationFailed(msg)) => {
            assert!(
                msg.contains("before_oom") || msg.contains("ReferenceError"),
                "expected a ReferenceError for a variable from the discarded context, got: {msg}"
            );
        }
        other => {
            panic!("expected the pre-OOM variable to be gone in the fresh context, got: {other:?}")
        }
    }
}

/// JS-1・Issue #503 W6 観点 5「ハンドシェイクの失敗」: 子が親の期待する
/// プロトコルバージョンをサポートしない場合、子は `Hello` を送らずに
/// 終了し、親はこれを [`JsEngineError::EngineUnavailable`] として報告
/// すること（設計書 §3.1「ハンドシェイク」）。
fn js_1_handshake_failure_is_engine_unavailable() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: None,
        protocol_version_override: Some(9999),
    });

    match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
        Err(JsEngineError::EngineUnavailable(_)) => {}
        other => {
            panic!("expected EngineUnavailable for an unsupported protocol version, got: {other:?}")
        }
    }
}

/// JS-1・Issue #503 W6 観点 6「プロトコル違反」: 子の stdin へ未知の tag
/// を持つ生フレームを送り込むと、子はこれを検出して終了し、次の評価は
/// 親が EOF を検出して [`JsEngineError::EngineUnavailable`]（Context は
/// 破棄）を返すこと。さらにその次の評価は、新しい子プロセスで成功する
/// こと（自動での起動し直し）。
fn js_1_protocol_violation_discards_context_and_recovers() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("1 + 1", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("initial evaluation must succeed: {err}"));

    // `u32 LE len`（タグ 1 バイトのみ。ペイロード無し）＋ `u8 tag`（200。
    // `super::worker_protocol::tag` のどの値とも一致しない未知のタグ）。
    let mut malformed_frame = Vec::new();
    malformed_frame.extend_from_slice(&1u32.to_le_bytes());
    malformed_frame.push(200);
    engine
        .send_raw_frame_for_test(&malformed_frame)
        .unwrap_or_else(|err| panic!("writing the malformed frame must succeed: {err}"));

    match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
        Err(JsEngineError::EngineUnavailable(msg)) => {
            assert!(
                msg.contains("context was discarded"),
                "protocol violation must discard the context, got: {msg}"
            );
        }
        other => panic!(
            "expected EngineUnavailable after the child observed a protocol violation, got: {other:?}"
        ),
    }

    let result = engine
        .evaluate_script("40 + 2", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!("engine must recover with a fresh child after a protocol violation: {err}")
        });
    assert_eq!(result, JsValue::Number(42.0));
}
