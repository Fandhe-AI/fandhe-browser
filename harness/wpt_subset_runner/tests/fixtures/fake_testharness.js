// 結合テスト用の偽 testharness。WPT の testharness.js ではない（ライセンス上 WPT の
// コードは同梱しない。#273 の方針）。本リポで書いた最小のエミュレーションで、
// `self` に依存すること・同期 test() の結果を同期で通知すること・ステータス値が
// 実物と同じこと（PASS=0, FAIL=1 / OK=0）だけを模す。実物との互換確認は #554 で
// 固定リビジョンの testharness.js を使って行う。
(function (global_scope) {
    var result_callbacks = [];
    var completion_callbacks = [];
    var tests = [];

    function assert_true(actual, description) {
        if (actual !== true) {
            throw new Error('assert_true: ' + (description || 'expected true'));
        }
    }

    function test(func, name) {
        var result = { name: name, status: 0, message: null };
        try {
            func();
        } catch (e) {
            result.status = 1;
            result.message = String(e && e.message);
        }
        tests.push(result);
        result_callbacks.forEach(function (cb) { cb(result); });
    }

    function done() {
        var hs = { status: 0, message: null };
        completion_callbacks.forEach(function (cb) { cb(tests, hs); });
    }

    function add_result_callback(cb) { result_callbacks.push(cb); }
    function add_completion_callback(cb) { completion_callbacks.push(cb); }

    global_scope.assert_true = assert_true;
    global_scope.test = test;
    global_scope.done = done;
    global_scope.add_result_callback = add_result_callback;
    global_scope.add_completion_callback = add_completion_callback;
})(self);
