// js_shim/timers.js: setTimeout / setInterval / queueMicrotask の shim
// （TASK-110・MS-6・Issue #782・ビヘイビア JS-7・JS-6。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle が登録したタイマーのコールバックを保持し、タイマーの期限・順序・上限の
// 管理は core 側（event_loop.rs / dom_bridge の timerSet・timerClear op）に任せる。ランナー
// （core の page_runner）が仮想時間の順に `__fandheRunTimer(id)` を評価してコールバックを呼ぶ。
// 実時間は待たない。呼び出し元: core の `js_shim::install`（events.js の後に評価）と、
// ランナーのイベントループ段および各 <script> / ライフサイクルイベント後の microtask checkpoint。
//
// 設計上の要点:
// - 期限・発火回数・保留中の件数の実体は Rust 側にあり、ページ JS がこの shim を改竄しても
//   上限は回避できない。遅延値は Number に変換して渡すだけで、丸めは Rust 側で行う。
// - 文字列ハンドラ（setTimeout("code", ...)）は eval 相当のため評価しない。0 を返し、
//   `timerStringHandlerIgnored` op で件数だけ通知する（インジェクション面を作らない）。
// - __fandheRunTimer は `timerConsume` op（Rust が選んだ発火の一回限りの許可）が true のときだけ実行する。
// - ディスパッチャー（__fandheRunTimer / __fandheRunMicrotasks）は決して throw しない。
//   コールバックの例外は try/catch で捕捉し、`timerCallbackError` op でメッセージだけ通知して
//   後続を続行する。両者は non-writable / non-configurable で固定し、再 install 時の実体の
//   差し替えは events.js と同じく `lifecycleInstallOpen` op が true（shim 注入中）のときだけ許す。
// - microtask は shim 内の JS キューで実装し、エンジンに依存しない。drain は各タイマーの発火の
//   末尾（JS 内）と、ランナーが評価する `__fandheRunMicrotasks()` で行う。
// - 配列メソッドは使わず（ページによる Array.prototype の改変の影響を避ける）、null プロトタイプの
//   コンテナと添字演算で状態を持つ。ES2015 の範囲に留める（V8 と boa で同一ソース）。
//
// 未実装（REPAIR-3）:
// - ネイティブ Promise の反応ジョブとの統合（エンジン依存。TASK-30）。
// - HTML のネスト時 4ms クランプ、文字列ハンドラの評価。
// - setTimeout が返す ID は正の整数のみ（登録を拒否したときは 0）。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var nativeOp = bridge.op;
    var callOp = Function.prototype.call.bind(nativeOp, bridge);
    var defineProperty = Object.defineProperty;
    var createObject = Object.create;
    var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    var fnCall = Function.prototype.call.bind(Function.prototype.call);
    var fnApply = Function.prototype.call.bind(Function.prototype.apply);
    var toNumber = Number;
    var floor = Math.floor;
    var isFinite_ = Number.isFinite;
    var MAX_TEXT_UNITS = 4097;
    // 保留中の microtask 数の上限。超過分は破棄して Rust 側へ通知し、ランナーが打ち切る（JS-6）。
    var MAX_PENDING_MICROTASKS = 10000;

    // id → { fn, args, repeat }。null プロトタイプ（継承された setter / 'toString' 等の影響を避ける）。
    var timers = createObject(null);
    // timers の保持件数と、次に Rust 側の生存確認で掃除する閾値（JS-6）。
    var retained = 0;
    var SWEEP_MIN = 64;
    var sweepAt = SWEEP_MIN;
    // microtask キュー（head 以上 tail 未満が保留中）。
    var queue = createObject(null);
    var head = 0;
    var tail = 0;

    function report(source, error) {
        try {
            var text;
            try {
                text = String(error && error.message !== undefined ? error.message : error);
            } catch (e) {
                text = '[unprintable]';
            }
            if (text.length > MAX_TEXT_UNITS) {
                var end = MAX_TEXT_UNITS;
                var last = text.charCodeAt(end - 1);
                // サロゲートペアの途中で切らない。
                if (last >= 0xd800 && last <= 0xdbff) {
                    end -= 1;
                }
                text = text.slice(0, end);
            }
            callOp('timerCallbackError', source, text);
        } catch (e) {
            // 報告に失敗しても続行する。
        }
    }

    function toDelay(delay) {
        try {
            return toNumber(delay);
        } catch (e) {
            return 0;
        }
    }

    // Rust 側で既に無効な ID（__dom.op の直接 timerClear 等）の保持を解放する。生存件数の倍で
    // 次の掃除を行い、償却コストを一定に保つ。保持件数は Rust 側の登録上限の高々 2 倍に収まる。
    function sweep() {
        var kept = 0;
        for (var key in timers) {
            var alive = false;
            try {
                alive = callOp('timerPending', toNumber(key)) === true;
            } catch (e) {
                alive = false;
            }
            if (alive) {
                kept += 1;
            } else {
                delete timers[key];
            }
        }
        retained = kept;
        sweepAt = kept * 2 > SWEEP_MIN ? kept * 2 : SWEEP_MIN;
    }

    function schedule(handler, delay, repeat, extra, extraStart) {
        if (typeof handler !== 'function') {
            try {
                callOp('timerStringHandlerIgnored');
            } catch (e) {
                // 通知に失敗しても評価はしない。
            }
            return 0;
        }
        var id = callOp('timerSet', toDelay(delay), repeat);
        if (typeof id !== 'number' || !(id > 0)) {
            return 0;
        }
        var args = createObject(null);
        var n = 0;
        for (var i = extraStart; i < extra.length; i++) {
            args[n] = extra[i];
            n += 1;
        }
        args.length = n;
        if (retained >= sweepAt) {
            sweep();
        }
        timers[id] = { fn: handler, args: args, repeat: repeat };
        retained += 1;
        return id;
    }

    function clearTimer(id) {
        var n;
        try {
            n = toNumber(id);
        } catch (e) {
            return;
        }
        if (!isFinite_(n) || n !== floor(n) || n < 1) {
            return;
        }
        if (hasOwn(timers, n)) {
            delete timers[n];
            retained -= 1;
        }
        try {
            callOp('timerClear', n);
        } catch (e) {
            // 取り消しの通知に失敗しても JS 側の保持は削除済み（発火しても何も呼ばれない）。
        }
    }

    function drain() {
        while (head < tail) {
            var fn = queue[head];
            delete queue[head];
            head += 1;
            try {
                fnCall(fn, global);
            } catch (e) {
                report('microtask', e);
            }
        }
        head = 0;
        tail = 0;
    }

    function runTimer(id) {
        // Rust 側が選んだ発火を一度だけ消費できたときだけ実行する。ページが直接呼んでも
        // 発火回数・仮想時間の上限を回避できない（JS-6）。
        var allowed = false;
        try {
            allowed = callOp('timerConsume', id) === true;
        } catch (e) {
            allowed = false;
        }
        if (!allowed) {
            return;
        }
        var entry = timers[id];
        if (entry !== undefined) {
            if (!entry.repeat) {
                delete timers[id];
                retained -= 1;
            }
            try {
                fnApply(entry.fn, global, entry.args);
            } catch (e) {
                report(entry.repeat ? 'interval' : 'timeout', e);
            }
        }
        drain();
    }

    function defineMethod(name, fn) {
        defineProperty(global, name, {
            value: fn,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    defineMethod('setTimeout', function setTimeout(handler, delay) {
        return schedule(handler, delay, false, arguments, 2);
    });
    defineMethod('setInterval', function setInterval(handler, delay) {
        return schedule(handler, delay, true, arguments, 2);
    });
    defineMethod('clearTimeout', function clearTimeout(id) {
        clearTimer(id);
    });
    defineMethod('clearInterval', function clearInterval(id) {
        clearTimer(id);
    });
    defineMethod('queueMicrotask', function queueMicrotask(callback) {
        if (typeof callback !== 'function') {
            throw new TypeError('queueMicrotask: callback must be a function');
        }
        if (tail - head >= MAX_PENDING_MICROTASKS) {
            try {
                callOp('microtaskLimit');
            } catch (e) {
                // 通知に失敗しても破棄は維持する。
            }
            return;
        }
        queue[tail] = callback;
        tail += 1;
    });

    // 固定の窓口関数は初回だけ定義し、実体は再 install のたびに差し替える（events.js と同じ作法）。
    function defineDispatcher(name, impl) {
        var existing = hasOwn(global, name) ? global[name] : null;
        if (existing !== null && typeof existing === 'function' && existing.__fandheImpl === true) {
            existing.setImpl(impl);
            return;
        }
        if (existing !== null) {
            return;
        }
        var currentImpl = impl;
        var entry = function (arg) {
            try {
                currentImpl(arg);
            } catch (e) {
                // ディスパッチャーは throw しない。
            }
        };
        defineProperty(entry, '__fandheImpl', { value: true });
        defineProperty(entry, 'setImpl', {
            // 差し替えは Rust 側が shim 注入中だけ立てるフラグが true のときに限る。
            value: function (next) {
                var open = false;
                try {
                    open = callOp('lifecycleInstallOpen') === true;
                } catch (e) {
                    open = false;
                }
                if (open) {
                    currentImpl = next;
                }
            }
        });
        defineProperty(global, name, {
            value: entry,
            writable: false,
            configurable: false,
            enumerable: false
        });
    }

    defineDispatcher('__fandheRunTimer', runTimer);
    defineDispatcher('__fandheRunMicrotasks', drain);
})(globalThis);
