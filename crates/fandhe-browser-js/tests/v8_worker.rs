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

/// テスト用に小さくした親側 RSS 監視のしきい値（バイト。48 MiB）。
///
/// codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ上限
/// がない」対応で、V8 の `ArrayBuffer` backing store 確保に独自の上限
/// （`resource_limits::MAX_ARRAY_BUFFER_ALLOCATION_BYTES`。128 MiB）を
/// 設けたことに伴い、この定数を導入する前に「`ArrayBuffer` を際限なく
/// 確保して本番の RSS 監視しきい値（320 MiB）に実際に到達させる」ことで
/// 親側の RSS 監視を検証していた既存のテストが、その手段を失った
/// （`ArrayBuffer` の確保が 128 MiB で `RangeError` になり、320 MiB には
/// 決して到達しなくなったため）。
///
/// RSS 監視という仕組みそのものの検証を保つため、`ArrayBuffer` の上限を
/// テストのために引き上げるのではなく（引き上げは、検証したい安全機構
/// 自体を回避する経路になり得るため避ける。`WorkerSpawnConfigForTest`
/// の他のテスト専用フィールドと同じ「下げることしかできない」原則を
/// 保つ）、RSS 監視のしきい値を本番の値より小さくする
/// （`WorkerSpawnConfigForTest::rss_threshold_bytes_override`）ことで、
/// `ArrayBuffer` の上限（128 MiB）に到達するよりずっと手前で RSS 監視が
/// 先に発動するようにする。本モジュールのドキュメントコメントが説明する
/// 起動直後の RSS 実測値（10〜25 MiB 程度）に対して十分な余裕を持たせ
/// （通常の評価がこのしきい値に誤って触れないように）、かつ数回の
/// 10 MiB チャンク確保で確実に超えられる値として 48 MiB を選んだ。
const TEST_RSS_THRESHOLD_BYTES: u64 = 48 * 1024 * 1024;

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
    eprintln!("case: js_1_handshake_rejects_hello_naming_a_different_engine");
    js_1_handshake_rejects_hello_naming_a_different_engine();
    eprintln!("case: js_1_handshake_rejects_hello_with_wrong_length");
    js_1_handshake_rejects_hello_with_wrong_length();
    eprintln!("case: js_1_oversized_script_is_rejected_without_spawning_a_child");
    js_1_oversized_script_is_rejected_without_spawning_a_child();
    eprintln!("case: js_1_send_raw_frame_for_test_rejects_oversized_input");
    js_1_send_raw_frame_for_test_rejects_oversized_input();
    eprintln!("case: js_1_protocol_violation_discards_context_and_recovers");
    js_1_protocol_violation_discards_context_and_recovers();
    eprintln!("case: js_1_oversized_test_heap_limit_is_clamped_to_the_production_default");
    js_1_oversized_test_heap_limit_is_clamped_to_the_production_default();
    eprintln!("case: js_1_undersized_test_heap_limit_is_raised_to_a_working_floor");
    js_1_undersized_test_heap_limit_is_raised_to_a_working_floor();
    eprintln!(
        "case: js_1_oversized_array_buffer_is_rejected_as_range_error_without_discarding_context"
    );
    js_1_oversized_array_buffer_is_rejected_as_range_error_without_discarding_context();
    eprintln!(
        "case: js_1_array_buffer_allocator_counter_does_not_leak_across_repeated_allocate_and_free"
    );
    js_1_array_buffer_allocator_counter_does_not_leak_across_repeated_allocate_and_free();
    eprintln!("case: js_1_web_assembly_global_is_undefined");
    js_1_web_assembly_global_is_undefined();
    eprintln!(
        "case: js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded"
    );
    js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded();
    eprintln!(
        "case: js_1_repeated_short_evaluations_accumulating_array_buffers_hit_resource_limit"
    );
    js_1_repeated_short_evaluations_accumulating_array_buffers_hit_resource_limit();
    #[cfg(target_os = "linux")]
    {
        eprintln!("case: js_1_linux_child_process_has_rlimit_data_set");
        js_1_linux_child_process_has_rlimit_data_set();
        eprintln!("case: js_1_linux_child_process_has_oom_score_adj_set");
        js_1_linux_child_process_has_oom_score_adj_set();
    }
    #[cfg(target_os = "windows")]
    {
        eprintln!(
            "case: js_1_windows_process_memory_limit_kills_oversized_array_buffer_allocation"
        );
        js_1_windows_process_memory_limit_kills_oversized_array_buffer_allocation();
    }
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
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: None,
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
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: None,
    });

    match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
        Err(JsEngineError::EngineUnavailable(_)) => {}
        other => {
            panic!("expected EngineUnavailable for an unsupported protocol version, got: {other:?}")
        }
    }
}

/// codex レビュー指摘 #503 P1「Hello のエンジン種別を無視している」の
/// 結合テスト: 実際の子プロセスに `EngineKind::Boa` を名乗らせると
/// （[`WorkerSpawnConfigForTest::hello_wrong_engine_for_test`]。子は
/// 本物の V8 の Hello・ハンドシェイク手順をそのまま踏んだ上でエンジン
/// バイトだけを差し替える）、親はこれをハンドシェイク失敗として拒否し、
/// [`JsEngineError::EngineUnavailable`] を返すこと。
fn js_1_handshake_rejects_hello_naming_a_different_engine() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: None,
        protocol_version_override: None,
        hello_wrong_engine_for_test: true,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: None,
    });

    match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
        Err(JsEngineError::EngineUnavailable(msg)) => {
            assert!(
                msg.contains("Boa"),
                "expected the message to mention the unexpected engine kind (Boa), got: {msg}"
            );
        }
        other => panic!(
            "expected EngineUnavailable for a Hello naming an unexpected engine kind, \
             got: {other:?}"
        ),
    }
}

/// codex レビュー指摘 #503 P1「decode_hello が末尾の余分なバイトを
/// 拒否していない」の結合テスト: 実際の子プロセスが送る `Hello` の末尾
/// に余分な 1 バイトを付け足させると（[`WorkerSpawnConfigForTest::hello_extra_byte_for_test`]）、
/// 親は黙って先頭 3 バイトだけを解釈せず、ハンドシェイク失敗として拒否
/// して [`JsEngineError::EngineUnavailable`] を返すこと。
fn js_1_handshake_rejects_hello_with_wrong_length() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: None,
        protocol_version_override: None,
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: true,
        rss_threshold_bytes_override: None,
    });

    match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
        Err(JsEngineError::EngineUnavailable(msg)) => {
            assert!(
                msg.contains("malformed Hello"),
                "expected the message to report a malformed Hello frame, got: {msg}"
            );
        }
        other => {
            panic!("expected EngineUnavailable for a Hello with trailing bytes, got: {other:?}")
        }
    }
}

/// codex レビュー指摘 #503 P1「親は MAX_FRAME_PAYLOAD_PARENT_TO_CHILD
/// （1 MiB+64 KiB）まで受け付けるが、子は MAX_SCRIPT_SOURCE_BYTES
/// （1 MiB）までしか受け付けない」の回帰テスト: 上限を 1 バイト超える
/// スクリプトは、子プロセスを一切起動せずに
/// [`JsEngineError::EvaluationFailed`] として拒否されること
/// （`worker_pid_for_test()` が `None` のままであることで、子が
/// 起動されていないことを確認する）。
fn js_1_oversized_script_is_rejected_without_spawning_a_child() {
    let mut engine = V8ProcessEngine::new();
    assert_eq!(
        engine.worker_pid_for_test(),
        None,
        "no child should be spawned before the first evaluate_script call"
    );

    let max_len = V8ProcessEngine::max_script_source_bytes_for_test();
    let oversized_script = "1".repeat(max_len + 1);

    match engine.evaluate_script(&oversized_script, &EvaluateOptions::default()) {
        Err(JsEngineError::EvaluationFailed(msg)) => {
            assert!(
                msg.contains(&max_len.to_string()),
                "expected the message to mention the maximum supported length ({max_len}), \
                 got: {msg}"
            );
        }
        other => panic!(
            "expected EvaluationFailed for a script exceeding the maximum supported length, \
             got: {other:?}"
        ),
    }

    assert_eq!(
        engine.worker_pid_for_test(),
        None,
        "the oversized script must be rejected before spawning a child process"
    );
}

/// codex レビュー指摘 #503 P0「`send_raw_frame_for_test` が `bytes` を
/// 長さ検証なしで `to_vec()` しており、無制限の確保経路になる」の回帰
/// テスト: 上限（[`V8ProcessEngine::max_raw_frame_bytes_for_test`]）を
/// 1 バイト超える入力は、複製（`to_vec()`）される前に拒否され、子
/// プロセスも起動されないこと。
fn js_1_send_raw_frame_for_test_rejects_oversized_input() {
    let mut engine = V8ProcessEngine::new();
    assert_eq!(
        engine.worker_pid_for_test(),
        None,
        "no child should be spawned before the first send_raw_frame_for_test call"
    );

    let max_len = V8ProcessEngine::max_raw_frame_bytes_for_test();
    let oversized_frame = vec![0u8; max_len + 1];

    match engine.send_raw_frame_for_test(&oversized_frame) {
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains(&max_len.to_string()),
                "expected the message to mention the maximum supported length ({max_len}), \
                 got: {msg}"
            );
        }
        Ok(()) => panic!(
            "expected an oversized raw frame ({} bytes, limit {max_len}) to be rejected",
            oversized_frame.len()
        ),
    }

    assert_eq!(
        engine.worker_pid_for_test(),
        None,
        "the oversized raw frame must be rejected before spawning a child process"
    );
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

/// codex レビュー指摘 #503 P0 の回帰テスト観点 1「既定値より大きい値を
/// 指定しても、実効の上限が本番の既定値（128 MiB）を超えないこと」:
/// テスト専用の `heap_limit_bytes` に本番の既定値を大きく上回る値
/// （`OVERSIZED_TEST_HEAP_LIMIT_BYTES`）を指定しても、
/// `WorkerSpawnConfigForTest::heap_limit_bytes` のドキュメントコメントが
/// 説明するクランプ（`fandhe_browser_js::process_engine::spawn_worker` が
/// 子へ渡す前に適用）により本番の既定値へ切り下げられ、本番と同じ
/// ヒープ枯渇スクリプトが `SCRIPT_EXECUTION_TIMEOUT`（2 秒）の実行時間内で
/// `ResourceLimitExceeded` になること（`Timeout` にならないこと）を確認
/// する。クランプが効いていなければ、要求どおりの巨大なヒープが Isolate
/// に設定され、同じスクリプトは 2 秒以内には枯渇せずに実行時間ベースの
/// `Timeout` になりうる。
///
/// **本テストは実時間ベースであり、`v8_engine::tests::
/// js_1_clamp_test_heap_limit_bytes_never_exceeds_production_default`
/// ほど決定的ではない**（実装済みを装わない。REPAIR-3）。128 MiB への
/// 到達が極端に遅い実行環境では `Timeout` になり誤って失敗しうる。
/// 1 チャンクを 1e7 要素（約 80 MiB 相当の書き込み）にすることで、
/// 128 MiB への到達に要する反復回数を数回に抑え、フレーク耐性を
/// 高めている。
fn js_1_oversized_test_heap_limit_is_clamped_to_the_production_default() {
    // 本番の既定値（128 MiB）を大きく超える要求値。クランプされていれば
    // 実効の上限は 128 MiB のまま変わらない。
    const OVERSIZED_TEST_HEAP_LIMIT_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4 GiB
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: Some(OVERSIZED_TEST_HEAP_LIMIT_BYTES),
        protocol_version_override: None,
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: None,
    });

    let oom_script = "var chunks = []; while (true) { chunks.push(new Array(1e7).fill(0)); }";
    match engine.evaluate_script(oom_script, &EvaluateOptions::default()) {
        Err(JsEngineError::ResourceLimitExceeded(msg)) => {
            assert!(
                msg.contains("context was discarded"),
                "clamped OOM must discard the context, got: {msg}"
            );
        }
        other => panic!(
            "expected ResourceLimitExceeded within the timeout because the requested heap \
             limit ({OVERSIZED_TEST_HEAP_LIMIT_BYTES} bytes) must be clamped down to the \
             production default (128 MiB); an unclamped 4 GiB heap would instead hit the \
             execution timeout first, got: {other:?}"
        ),
    }
}

/// codex レビュー指摘 #503 P0 の回帰テスト観点 2「`0` や極端に小さい値は
/// 下限まで引き上げられ、無効な値がそのまま子の V8 Isolate へ渡らない
/// こと」: `heap_limit_bytes: Some(0)` を要求しても、クランプにより
/// 実用上動作する最小値まで引き上げられ、トリビアルな評価が成功する
/// こと。クランプが無ければ `v8::CreateParams::heap_limits(0, 0)` に近い
/// 状態になり、Isolate 生成直後の永続 `Context` 生成の時点でヒープが
/// 尽きて子プロセスが起動時に fatal OOM で終了し、ハンドシェイクにすら
/// 到達できないはずである。
fn js_1_undersized_test_heap_limit_is_raised_to_a_working_floor() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: Some(0),
        protocol_version_override: None,
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: None,
    });

    let result = engine
        .evaluate_script("1 + 1", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!(
                "a heap_limit_bytes request of 0 must be clamped up to a working floor instead \
                 of being passed through as-is, but evaluation failed: {err}"
            )
        });
    assert_eq!(result, JsValue::Number(2.0));
}

/// codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ上限
/// がない」の回帰テスト観点「上限を超える `ArrayBuffer` は `RangeError`
/// （`EvaluationFailed`）になり、子プロセスは生きたまま Context も保た
/// れる（直後の評価が成功する）」:
/// `resource_limits::MAX_ARRAY_BUFFER_ALLOCATION_BYTES`（128 MiB）を
/// 1 MiB 超える単発の確保は、際限のない繰り返し（`js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded`
/// が検証する RSS 監視の経路）を経ずに、V8 自身のレベル
/// （`resource_limits::new_bounded_array_buffer_allocator`）で即座に
/// 拒否されること。`ResourceLimitExceeded`（子を破棄して作り直す）とは
/// 異なり、`EvaluationFailed` は子・Context を破棄しない契約であるため、
/// 直後の評価が同じ Context（`before_limit` が見える）で成功することも
/// あわせて確認する。
fn js_1_oversized_array_buffer_is_rejected_as_range_error_without_discarding_context() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("var before_limit = 111;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("marker declaration must evaluate successfully: {err}"));

    // 128 MiB + 1 MiB。`MAX_ARRAY_BUFFER_ALLOCATION_BYTES` を確実に
    // 超える単発の確保。
    const OVERSIZED_BYTES: u64 = 128 * 1024 * 1024 + 1024 * 1024;
    let oversized_script = format!("new ArrayBuffer({OVERSIZED_BYTES})");
    match engine.evaluate_script(&oversized_script, &EvaluateOptions::default()) {
        Err(JsEngineError::EvaluationFailed(msg)) => {
            assert!(
                msg.contains("RangeError"),
                "expected a RangeError for an allocation exceeding \
                 MAX_ARRAY_BUFFER_ALLOCATION_BYTES, got: {msg}"
            );
        }
        other => panic!(
            "expected EvaluationFailed(RangeError) for an oversized single ArrayBuffer \
             allocation, got: {other:?}"
        ),
    }

    // Context が保たれていること（`before_limit` がまだ見える。子プロセスも
    // 生きたまま同じ子を使い続ける）。
    let result = engine
        .evaluate_script("before_limit", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!(
                "the RangeError from an oversized ArrayBuffer must not discard the context: {err}"
            )
        });
    assert_eq!(result, JsValue::Number(111.0));
}

/// codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ上限
/// がない」の回帰テスト観点「上限以下の確保と解放（GC）を繰り返しても、
/// カウンタが漏れずに再び確保できる」: `resource_limits::
/// MAX_ARRAY_BUFFER_ALLOCATION_BYTES`（128 MiB）に対し、1 回 8 MiB の
/// `ArrayBuffer` を確保しては参照を手放す（`a = null`）ループを、
/// 単純合計で上限の何倍にもなる回数（40 回。8 MiB × 40 ＝ 320 MiB）
/// 繰り返す。カウンタが解放時に正しく減っていれば、V8 の外部メモリ
/// 圧力に基づく GC が不要になった `ArrayBuffer` を回収し続けるため、
/// 一度も `RangeError` にならずに完走する。カウンタが漏れていれば
/// （解放を数えていなければ）、40 回に到達するよりずっと前
/// （128 MiB ÷ 8 MiB ＝ 16 回目前後）に `RangeError` になるはずである。
fn js_1_array_buffer_allocator_counter_does_not_leak_across_repeated_allocate_and_free() {
    let mut engine = V8ProcessEngine::new();
    let script = "for (let i = 0; i < 40; i++) { let a = new ArrayBuffer(8 * 1024 * 1024); a = null; } \"done\"";
    let result = engine
        .evaluate_script(script, &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!(
                "40 iterations of allocate-then-drop (8 MiB each; 320 MiB cumulative if the \
                 allocator's counter leaked, well over the 128 MiB limit) must all succeed if \
                 `free` correctly returns bytes to the counter, got: {err}"
            )
        });
    assert_eq!(result, JsValue::String("done".to_string()));
}

/// codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ上限
/// がない」の回帰テスト観点「WebAssembly を無効化する」:
/// `typeof WebAssembly` が `"undefined"` になること
/// （`v8_engine::V8Engine::new_with_heap_limit` が Context 生成直後に
/// `WebAssembly` グローバルを上書きする。詳細は同関数の実装コメント
/// 参照）。
fn js_1_web_assembly_global_is_undefined() {
    let mut engine = V8ProcessEngine::new();
    let result = engine
        .evaluate_script("typeof WebAssembly", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("evaluating typeof WebAssembly must succeed: {err}"));
    assert_eq!(result, JsValue::String("undefined".to_string()));
}

/// codex レビュー指摘 #503 P0「ヒープ外メモリが無制限」の回帰テスト
/// 観点「大きな `ArrayBuffer` を確保して触ると `ResourceLimitExceeded`
/// になり、次の評価が新しい Context で成功すること」: `Uint8Array` の
/// backing store は V8 のヒープ外（[`v8_engine`] の `MAX_ISOLATE_HEAP_BYTES`。
/// 本テストは既定値の 128 MiB のまま）に確保されるため、`chunks` 配列
/// 自体が保持する参照は小さく、V8 の Isolate ヒープ上限には触れない。
///
/// **`WorkerSpawnConfigForTest::rss_threshold_bytes_override`
/// （[`TEST_RSS_THRESHOLD_BYTES`]。48 MiB）を使う**（同定数のドキュメント
/// コメント参照）: codex レビュー指摘 #503 P0「macOS の JS ワーカーに
/// 強制的なメモリ上限がない」対応で導入した
/// `resource_limits::new_bounded_array_buffer_allocator`（128 MiB）が、
/// 本番のしきい値（`resource_limits::MAX_CHILD_RSS_BYTES`。320 MiB）へ
/// 到達するよりずっと手前で `RangeError` を投げてしまうため、本番の
/// しきい値のままでは際限のない確保を続けさせられない。RSS 監視の
/// しきい値を下げることで、`ArrayBuffer` の上限（128 MiB）に到達する
/// よりずっと手前で親側の RSS 監視が先に発動するようにする。
fn js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: None,
        protocol_version_override: None,
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: Some(TEST_RSS_THRESHOLD_BYTES),
    });
    engine
        .evaluate_script("var before_oom = 555;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("marker declaration must evaluate successfully: {err}"));

    // 1 チャンク 10 MiB の `Uint8Array` を確保して実際に書き込み（`fill`
    // でページをタッチし、仮想アドレス空間の予約だけで済ませない）、
    // 参照を保持し続ける。ヒープ外の RSS を素早く積み上げ、下げた RSS
    // 監視しきい値（[`TEST_RSS_THRESHOLD_BYTES`]。48 MiB）に数チャンクで
    // 到達させる（`ArrayBuffer` の上限は 128 MiB のため、まだ十分な
    // 余裕がある段階で RSS 監視が先に発動する）。
    let oom_script = "var chunks = []; while (true) { chunks.push(new Uint8Array(1e7).fill(1)); }";
    match engine.evaluate_script(oom_script, &EvaluateOptions::default()) {
        Err(JsEngineError::ResourceLimitExceeded(msg)) => {
            assert!(
                msg.contains("context was discarded"),
                "heap-external OOM must discard the context, got: {msg}"
            );
        }
        other => panic!(
            "expected ResourceLimitExceeded for an ArrayBuffer backing-store exhaustion, got: \
             {other:?}"
        ),
    }

    let result = engine
        .evaluate_script("40 + 2", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!("engine must recover with a fresh child after heap-external OOM: {err}")
        });
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

/// codex レビュー指摘 #503 P0 の回帰テスト観点「Linux では `RLIMIT_DATA`
/// が実際に設定されていることを確認する」: `/proc/<pid>/limits` の
/// "Max data size" 行の soft/hard 値が、`resource_limits` モジュールが
/// 設定する上限（2 GiB＝2,147,483,648 バイト）と一致すること。
///
/// 子に `getrlimit` の結果を返させるテスト用の経路は設けず（設計書 §7
/// の指示どおり）、親から `/proc/<pid>/limits` を直接読む
/// （`V8ProcessEngine::worker_pid_for_test` 経由）。
#[cfg(target_os = "linux")]
fn js_1_linux_child_process_has_rlimit_data_set() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("1 + 1", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("a trivial evaluation must succeed: {err}"));

    let pid = engine
        .worker_pid_for_test()
        .expect("a worker must be running after a successful evaluation");
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits"))
        .unwrap_or_else(|err| panic!("failed to read /proc/{pid}/limits: {err}"));

    let data_size_line = limits
        .lines()
        .find(|line| line.starts_with("Max data size"))
        .unwrap_or_else(|| panic!("expected a \"Max data size\" line in:\n{limits}"));
    // 例: "Max data size             2147483648           2147483648           bytes"
    let fields: Vec<&str> = data_size_line.split_whitespace().collect();
    // ["Max", "data", "size", "<soft>", "<hard>", "bytes"]
    assert_eq!(
        fields.len(),
        6,
        "unexpected \"Max data size\" line format: {data_size_line:?}"
    );
    assert_eq!(
        fields[3], "2147483648",
        "soft RLIMIT_DATA mismatch: {data_size_line:?}"
    );
    assert_eq!(
        fields[4], "2147483648",
        "hard RLIMIT_DATA mismatch: {data_size_line:?}"
    );
}

/// `JS-1`・`TASK-29`・Issue #516: 子プロセスが起動直後・V8 初期化前に
/// `/proc/self/oom_score_adj` へ `1000` を書き込んでいること。親から
/// `/proc/<pid>/oom_score_adj` を読んで確認する
/// （`V8ProcessEngine::worker_pid_for_test` 経由。`js_1_linux_child_process_has_rlimit_data_set`
/// と同じ形）。
///
/// 親プロセス自身が既に `oom_score_adj=1000` で動いている環境（この値を
/// 継承しているだけの可能性がある）では、この結合テストだけでは
/// 「実際に書き込んだ」ことと「継承しただけ」を区別できない。書き込みの
/// 仕組みそのもの（既存のファイルへ値を書けること・存在しないパスでは
/// エラーになること）は `resource_limits` モジュールの単体テスト
/// （`js_1_write_oom_score_adj_writes_the_value_to_the_given_path` 等）が
/// 担う。本テストは「子プロセスの実行結果として値が 1000 になっている」
/// ことを、実際のプロセス起動を通して確認する回帰テストである。
///
/// 実際に OOM killer が発火してこの子が優先的に選ばれることの再現は
/// 環境依存のため対象外とする（`resource_limits` モジュールのドキュメント
/// コメント「既知の制限」参照）。
#[cfg(target_os = "linux")]
fn js_1_linux_child_process_has_oom_score_adj_set() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("1 + 1", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("a trivial evaluation must succeed: {err}"));

    let pid = engine
        .worker_pid_for_test()
        .expect("a worker must be running after a successful evaluation");
    let value = std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
        .unwrap_or_else(|err| panic!("failed to read /proc/{pid}/oom_score_adj: {err}"));

    assert_eq!(
        value.trim(),
        "1000",
        "expected the child's oom_score_adj to be set to 1000"
    );
}

/// codex・Cursor Bugbot レビュー指摘 #503 P0「短い評価を繰り返して
/// `ArrayBuffer` をグローバル変数に溜め込むと、上限を超えても検出され
/// ない」の回帰テスト: 1 回 1 回は小さくすぐ終わる評価
/// （`chunks.push(new Uint8Array(1e7).fill(1))`。無限ループを含まない）
/// を `evaluate_script` 呼び出しごとに繰り返すと、継続的なメモリ監視
/// スレッド（応答直後の同期確認・次回呼び出し前の確認を含む）が
/// どこかの呼び出しで `ResourceLimitExceeded` を返し、その次の評価は
/// 新しい Context で成功すること。
///
/// [`js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded`]
/// と同じ理由で `WorkerSpawnConfigForTest::rss_threshold_bytes_override`
/// （[`TEST_RSS_THRESHOLD_BYTES`]。48 MiB）を使う。
fn js_1_repeated_short_evaluations_accumulating_array_buffers_hit_resource_limit() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: None,
        protocol_version_override: None,
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: Some(TEST_RSS_THRESHOLD_BYTES),
    });
    engine
        .evaluate_script(
            "globalThis.chunks = []; chunks.length",
            &EvaluateOptions::default(),
        )
        .unwrap_or_else(|err| panic!("initializing the accumulator must succeed: {err}"));

    // [`TEST_RSS_THRESHOLD_BYTES`]（48 MiB）に対し、1 回 10 MiB。10 回
    // （100 MiB 相当。`ArrayBuffer` の上限である 128 MiB にはまだ余裕を
    // 残す）まで繰り返せば、しきい値超過が検出されないままでは通らない
    // はずである。
    const MAX_ITERATIONS: usize = 10;
    let mut hit_limit = false;
    for i in 0..MAX_ITERATIONS {
        match engine.evaluate_script(
            "chunks.push(new Uint8Array(1e7).fill(1)); chunks.length",
            &EvaluateOptions::default(),
        ) {
            Ok(_) => continue,
            Err(JsEngineError::ResourceLimitExceeded(msg)) => {
                assert!(
                    msg.contains("context was discarded"),
                    "repeated small allocations must discard the context, got: {msg}"
                );
                hit_limit = true;
                break;
            }
            other => panic!("iteration {i}: expected Ok or ResourceLimitExceeded, got: {other:?}"),
        }
    }
    assert!(
        hit_limit,
        "expected to hit the parent's RSS monitoring threshold within {MAX_ITERATIONS} \
         repeated 10 MiB allocations, but every evaluation succeeded"
    );

    // 次の評価は新しい子・新しい Context で成功し、以前の `chunks` は
    // もう見えないこと。
    let result = engine
        .evaluate_script("typeof chunks", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!("engine must recover with a fresh child after hitting the limit: {err}")
        });
    assert_eq!(result, JsValue::String("undefined".to_string()));
}

/// codex レビュー指摘 #503 P0「Windows では working set の上限と監視だけ
/// ではコミット量を制限できない」の回帰テスト観点「Windows で、大きな
/// `ArrayBuffer` を確保すると `ResourceLimitExceeded` になり、次の評価が
/// 新しい Context で成功すること」。
///
/// 本テストはローカル（開発環境が Windows ではない）では実行されず、
/// CI の Windows ランナーでの確認を前提とする（`#[cfg(target_os =
/// "windows")]` かつ `main` から Windows でだけ呼ばれる）。
///
/// [`js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded`]
/// と同じ理由で `WorkerSpawnConfigForTest::rss_threshold_bytes_override`
/// （[`TEST_RSS_THRESHOLD_BYTES`]。48 MiB）を使う: codex レビュー指摘
/// #503 P0「macOS の JS ワーカーに強制的なメモリ上限がない」対応で導入
/// した `resource_limits::new_bounded_array_buffer_allocator`（128 MiB）
/// が、Windows の Job Object の `ProcessMemoryLimit`（`resource_limits::
/// WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`。384 MiB）へ到達するよりずっと
/// 手前で `RangeError` を投げるようになったため、本番のしきい値のまま
/// では Job Object のコミット上限に基づく確保失敗（V8 が GC 付きで再試行
/// したうえで `RangeError` を投げる経路）を再現できない。RSS 監視の
/// しきい値を下げることで、`ArrayBuffer` の上限（128 MiB）に到達する
/// よりずっと手前で親側の RSS 監視（`PrivateUsage`）が先に発動するように
/// する（観測可能な結果としては、他 OS 向けの
/// `js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded`
/// と同じ `ResourceLimitExceeded` になる）。
#[cfg(target_os = "windows")]
fn js_1_windows_process_memory_limit_kills_oversized_array_buffer_allocation() {
    let mut engine = V8ProcessEngine::new_for_test(WorkerSpawnConfigForTest {
        heap_limit_bytes: None,
        protocol_version_override: None,
        hello_wrong_engine_for_test: false,
        hello_extra_byte_for_test: false,
        rss_threshold_bytes_override: Some(TEST_RSS_THRESHOLD_BYTES),
    });
    engine
        .evaluate_script("var before_oom = 777;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("marker declaration must evaluate successfully: {err}"));

    let oom_script = "var chunks = []; while (true) { chunks.push(new Uint8Array(1e7).fill(1)); }";
    match engine.evaluate_script(oom_script, &EvaluateOptions::default()) {
        Err(JsEngineError::ResourceLimitExceeded(msg)) => {
            assert!(
                msg.contains("context was discarded"),
                "heap-external OOM on Windows must discard the context, got: {msg}"
            );
        }
        other => panic!(
            "expected ResourceLimitExceeded for an ArrayBuffer backing-store exhaustion on \
             Windows, got: {other:?}"
        ),
    }

    let result = engine
        .evaluate_script("40 + 2", &EvaluateOptions::default())
        .unwrap_or_else(|err| {
            panic!("engine must recover with a fresh child after heap-external OOM: {err}")
        });
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
