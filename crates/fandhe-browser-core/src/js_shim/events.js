// js_shim/events.js: document / window の最小 EventTarget とライフサイクル通知の shim
// （TASK-109・Issue #781・ビヘイビア JS-4・JS-6。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle が `document.addEventListener('DOMContentLoaded', ...)` /
// `window.addEventListener('load', ...)` で登録したリスナーを保持し、ページランナー
// （core の page_runner）が全スクリプトの実行後に呼ぶディスパッチャー `__fandheLifecycle` で
// 順に呼び出す。DOM やページ状態の実体は持たない（readyState は core 側の bridge op）。
// 呼び出し元: core の `js_shim::install`（dom.js・window.js の後に評価）と、ランナーの発火処理。
//
// 設計上の要点:
// - ディスパッチャーは決して throw しない。リスナーの例外は try/catch で捕捉し、
//   `lifecycleListenerError` op でメッセージ（4097 UTF-16 単位で打ち切り）だけ core に渡して
//   後続のリスナーを続行する。
// - `__fandheLifecycle` は non-writable / non-configurable で固定する（__dom と同じ作法）。
//   再 install 時（同一 global への再評価）は窓口関数を残し、実体とリスナー状態だけを新しい評価のものへ差し替える。
//   差し替え（setImpl）は core が shim 注入中だけ立てるフラグ（`lifecycleInstallOpen` op）が true のときだけ有効で、
//   ページ JS が呼んでも dispatcher は変わらない。
// - DOMContentLoaded は document → window の順（バブリングの近似）、load は window のみ。
// - ES2015 の範囲に留める（V8 と boa で同一のソースを使うため）。
//
// 未実装（REPAIR-3）:
// - 要素レベルの addEventListener、onload / onreadystatechange 等のイベントハンドラ属性。
// - キャプチャ・完全なバブリング、passive / signal オプション、CustomEvent / dispatchEvent。
// - readystatechange イベント。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var nativeOp = bridge.op;
    // ページ JS による Function.prototype.call / Object.defineProperty の差し替えや、
    // nativeOp への own プロパティ `call` 追加の影響を受けないよう、install 時に束縛・退避する。
    var callOp = Function.prototype.call.bind(nativeOp, bridge);
    var defineProperty = Object.defineProperty;
    // 発火時にページが書き換え得る組み込みメソッドも、install 時に退避して使う（JS-4）。
    // 配列の push / splice / slice / indexOf は使わない（ページが差し替えたり、splice / slice が
    // 参照する Array[Symbol.species] を改変したりしても通知が止まらないよう、添字演算だけで操作する）。
    var fnCall = Function.prototype.call.bind(Function.prototype.call);
    var MAX_TEXT_UNITS = 4097;
    // document / window それぞれの保持件数の上限。確保前に検証し、超過した登録は無視して
    // `lifecycleListenerLimit` op でランナーへ通知する（ランナーはリソース上限として打ち切る。JS-6）。
    var MAX_LISTENERS_PER_TARGET = 1024;

    // リスナー保持は配列ではなく「プロトタイプを持たないコンテナ＋件数」で行う。配列の添字代入は
    // Array.prototype / Object.prototype の '0' 等に定義された setter や書き込み不可プロパティの影響を
    // 受け、例外でリスナーが呼ばれないまま Dispatched になり得るため（JS-4）。
    // null プロトタイプのオブジェクトへの代入は継承された setter を経由しない。
    var createObject = Object.create;
    function newTarget() {
        return { items: createObject(null), count: 0 };
    }
    var documentTarget = newTarget();
    var windowTarget = newTarget();

    function isListener(l) {
        return typeof l === 'function' || (l !== null && typeof l === 'object');
    }

    function add(target, type, listener, options) {
        if (typeof type !== 'string' || !isListener(listener)) {
            return;
        }
        var list = target.items;
        for (var i = 0; i < target.count; i++) {
            if (list[i].type === type && list[i].listener === listener) {
                return;
            }
        }
        var once = options !== null && typeof options === 'object' && !!options.once;
        if (target.count >= MAX_LISTENERS_PER_TARGET) {
            try {
                callOp('lifecycleListenerLimit');
            } catch (e) {
                // 通知に失敗しても登録は拒否したままにする。
            }
            return;
        }
        list[target.count] = { type: type, listener: listener, once: once };
        target.count += 1;
    }

    function remove(target, type, listener) {
        var list = target.items;
        for (var i = 0; i < target.count; i++) {
            if (list[i].type === type && list[i].listener === listener) {
                var len = target.count;
                for (var j = i; j < len - 1; j++) {
                    list[j] = list[j + 1];
                }
                delete list[len - 1];
                target.count = len - 1;
                return;
            }
        }
    }

    function defineMethod(obj, name, fn) {
        Object.defineProperty(obj, name, {
            value: fn,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    function install(obj, target) {
        defineMethod(obj, 'addEventListener', function addEventListener(type, listener, options) {
            add(target, type, listener, options);
        });
        defineMethod(obj, 'removeEventListener', function removeEventListener(type, listener) {
            remove(target, type, listener);
        });
    }

    install(global.document, documentTarget);
    install(global, windowTarget);

    function report(eventName, error) {
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
            callOp('lifecycleListenerError', eventName, text);
        } catch (e) {
            // 報告に失敗してもディスパッチは続ける。
        }
    }

    function fire(target, currentTarget, event, eventName, state) {
        // 呼び出し中の登録・削除の影響を受けないよう複製して走査する。
        var source = target.items;
        var sourceCount = target.count;
        var snapshot = createObject(null);
        for (var k = 0; k < sourceCount; k++) {
            snapshot[k] = source[k];
        }
        for (var i = 0; i < sourceCount; i++) {
            if (state.immediateStopped) {
                return;
            }
            var entry = snapshot[i];
            if (entry.type !== eventName) {
                continue;
            }
            // 走査中に removeEventListener されたものは呼ばない。
            var alive = false;
            for (var m = 0; m < target.count; m++) {
                if (source[m] === entry) {
                    alive = true;
                    break;
                }
            }
            if (!alive) {
                continue;
            }
            if (entry.once) {
                remove(target, entry.type, entry.listener);
            }
            // currentTarget は getter 経由（ページ側が event を freeze しても代入で TypeError にならない）。
            state.current = currentTarget;
            try {
                if (typeof entry.listener === 'function') {
                    fnCall(entry.listener, currentTarget, event);
                } else if (typeof entry.listener.handleEvent === 'function') {
                    entry.listener.handleEvent(event);
                }
            } catch (e) {
                report(eventName, e);
            }
        }
    }

    function dispatch(eventName) {
        var state = null;
        try {
            state = { propagationStopped: false, immediateStopped: false, current: null };
            var event = {
                type: eventName,
                target: eventName === 'load' ? global : global.document,
                bubbles: eventName === 'DOMContentLoaded',
                cancelable: false,
                defaultPrevented: false,
                preventDefault: function () {},
                stopPropagation: function () {
                    state.propagationStopped = true;
                },
                stopImmediatePropagation: function () {
                    state.propagationStopped = true;
                    state.immediateStopped = true;
                }
            };
            defineProperty(event, 'currentTarget', {
                get: function () {
                    return state.current;
                },
                enumerable: true,
                configurable: false
            });
            if (eventName === 'DOMContentLoaded') {
                fire(documentTarget, global.document, event, eventName, state);
                if (!state.propagationStopped) {
                    fire(windowTarget, global, event, eventName, state);
                }
            } else if (eventName === 'load') {
                fire(windowTarget, global, event, eventName, state);
            }
        } catch (e) {
            // ディスパッチャーは throw しない。
        } finally {
            // 発火終了後は保存されたイベントの currentTarget を null に戻す。
            if (state !== null) {
                state.current = null;
            }
        }
    }

    // 固定の窓口関数は初回だけ定義し、実体（dispatch）は再 install のたびに差し替える。
    // これで addEventListener の登録先（今回の評価の listeners）と発火先が常に一致し、
    // ページ間でリスナーを持ち越さない。
    var existing = Object.prototype.hasOwnProperty.call(global, '__fandheLifecycle')
        ? global.__fandheLifecycle
        : null;
    if (existing !== null && typeof existing === 'function' && existing.__fandheImpl === true) {
        existing.setImpl(dispatch);
    } else if (existing === null) {
        var currentImpl = dispatch;
        var entry = function __fandheLifecycle(eventName) {
            try {
                currentImpl(eventName);
            } catch (e) {
                // ディスパッチャーは throw しない。
            }
        };
        defineProperty(entry, '__fandheImpl', { value: true });
        defineProperty(entry, 'setImpl', {
            // 差し替えは Rust 側が shim 注入中だけ立てるフラグが true のときに限る。
            // ページ JS から呼んでも何も起きない（ディスパッチャーの無効化を防ぐ）。
            value: function (impl) {
                var open = false;
                try {
                    open = callOp('lifecycleInstallOpen') === true;
                } catch (e) {
                    open = false;
                }
                if (open) {
                    currentImpl = impl;
                }
            }
        });
        defineProperty(global, '__fandheLifecycle', {
            value: entry,
            writable: false,
            configurable: false,
            enumerable: false
        });
    }
})(globalThis);
