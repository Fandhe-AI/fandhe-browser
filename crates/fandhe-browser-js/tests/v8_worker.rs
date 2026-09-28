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
    eprintln!("case: js_1_oversized_test_heap_limit_is_clamped_to_the_production_default");
    js_1_oversized_test_heap_limit_is_clamped_to_the_production_default();
    eprintln!("case: js_1_undersized_test_heap_limit_is_raised_to_a_working_floor");
    js_1_undersized_test_heap_limit_is_raised_to_a_working_floor();
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

/// codex レビュー指摘 #503 P0「ヒープ外メモリが無制限」の回帰テスト
/// 観点「大きな `ArrayBuffer` を確保して触ると `ResourceLimitExceeded`
/// になり、次の評価が新しい Context で成功すること」: `Uint8Array` の
/// backing store は V8 のヒープ外（[`v8_engine`] の `MAX_ISOLATE_HEAP_BYTES`。
/// 本テストは既定値の 128 MiB のまま）に確保されるため、`chunks` 配列
/// 自体が保持する参照は小さく、V8 の Isolate ヒープ上限には触れない。
/// それでも際限のない確保は、**3 OS 共通で**親側の RSS 監視
/// （`resource_limits::MAX_CHILD_RSS_BYTES`。320 MiB）による強制終了と
/// して検出され、`JsEngineError::ResourceLimitExceeded` になる
/// （メッセージに "context was discarded" を含む）。Linux の
/// `RLIMIT_DATA`（`resource_limits::enforce_via_rlimit_data`。2 GiB）は
/// V8 の `CodeRange` 仮想アドレス予約だけで数百 MiB を要するため小さい
/// 値には設定できず（`resource_limits` のドキュメントコメント参照）、
/// RSS 監視のしきい値よりはるかに大きい。したがって本テストの範囲では
/// RSS 監視が先に発動し、`RLIMIT_DATA` はここでは働かない
/// （際限のない確保をどこまでも許さないための、より緩い最終防衛線に
/// 留まる）。
fn js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script("var before_oom = 555;", &EvaluateOptions::default())
        .unwrap_or_else(|err| panic!("marker declaration must evaluate successfully: {err}"));

    // 1 チャンク 10 MiB の `Uint8Array` を確保して実際に書き込み（`fill`
    // でページをタッチし、仮想アドレス空間の予約だけで済ませない）、
    // 参照を保持し続ける。ヒープ外の RSS を素早く積み上げ、親の RSS
    // 監視しきい値（`MAX_CHILD_RSS_BYTES`。320 MiB）に
    // `SCRIPT_EXECUTION_TIMEOUT`（2 秒）の実行時間内で到達させることを
    // 狙う（`RLIMIT_DATA`＝2 GiB はこのテストの範囲では発動しない。
    // 上のドキュメントコメント参照）。
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

/// codex・Cursor Bugbot レビュー指摘 #503 P0「短い評価を繰り返して
/// `ArrayBuffer` をグローバル変数に溜め込むと、上限を超えても検出され
/// ない」の回帰テスト: 1 回 1 回は小さくすぐ終わる評価
/// （`chunks.push(new Uint8Array(1e7).fill(1))`。無限ループを含まない）
/// を `evaluate_script` 呼び出しごとに繰り返すと、継続的なメモリ監視
/// スレッド（応答直後の同期確認・次回呼び出し前の確認を含む）が
/// どこかの呼び出しで `ResourceLimitExceeded` を返し、その次の評価は
/// 新しい Context で成功すること。
fn js_1_repeated_short_evaluations_accumulating_array_buffers_hit_resource_limit() {
    let mut engine = V8ProcessEngine::new();
    engine
        .evaluate_script(
            "globalThis.chunks = []; chunks.length",
            &EvaluateOptions::default(),
        )
        .unwrap_or_else(|err| panic!("initializing the accumulator must succeed: {err}"));

    // 320 MiB のしきい値に対し、1 回 10 MiB。80 回（800 MiB 相当）まで
    // 繰り返せば、しきい値超過が検出されないままでは通らないはずである。
    const MAX_ITERATIONS: usize = 80;
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
/// ではコミット量を制限できない」の回帰テスト観点「Windows の
/// `ProcessMemoryLimit` で、大きな `ArrayBuffer` を確保すると
/// `ResourceLimitExceeded` になり、次の評価が新しい Context で成功する
/// こと」。
///
/// 本テストはローカル（開発環境が Windows ではない）では実行されず、
/// CI の Windows ランナーでの確認を前提とする（`#[cfg(target_os =
/// "windows")]` かつ `main` から Windows でだけ呼ばれる）。子は
/// `enforce_child_memory_limit` により起動直後に Job Object の
/// `ProcessMemoryLimit`（`resource_limits::WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`。
/// 384 MiB）を自身へ設定済みである。ただし `JOB_OBJECT_LIMIT_PROCESS_MEMORY`
/// は Linux の `kill` ベースの強制とは異なり、**上限到達時にプロセスを
/// 終了させるのではなく、その先の確保を失敗させるだけ**である
/// （V8 は確保失敗を GC 付きで再試行したうえ、最終的に catchable な
/// `RangeError` を投げる。詳細は `resource_limits` の
/// `WINDOWS_PROCESS_MEMORY_LIMIT_BYTES` のドキュメントコメント参照）。
/// 際限のない確保はこの OS 側の天井で頭打ちになり、コミット量が
/// `MAX_CHILD_RSS_BYTES`（320 MiB）を上回った状態が続くため、親側の
/// `PrivateUsage` 監視（応答直後の同期確認・継続監視のいずれか）が
/// 最終的に検出して `ResourceLimitExceeded` へ分類し、子を作り直す
/// （観測可能な結果としては、他 OS 向けの
/// `js_1_array_buffer_backing_store_exhaustion_is_caught_as_resource_limit_exceeded`
/// と同じ `ResourceLimitExceeded` になる）。
#[cfg(target_os = "windows")]
fn js_1_windows_process_memory_limit_kills_oversized_array_buffer_allocation() {
    let mut engine = V8ProcessEngine::new();
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
