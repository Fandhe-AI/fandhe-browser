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
//   再 install 時（同一 global への再評価）は既存を残して何もしない。
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
    var MAX_TEXT_UNITS = 4097;

    var documentTarget = { listeners: [] };
    var windowTarget = { listeners: [] };

    function isListener(l) {
        return typeof l === 'function' || (l !== null && typeof l === 'object');
    }

    function add(target, type, listener, options) {
        if (typeof type !== 'string' || !isListener(listener)) {
            return;
        }
        var list = target.listeners;
        for (var i = 0; i < list.length; i++) {
            if (list[i].type === type && list[i].listener === listener) {
                return;
            }
        }
        var once = options !== null && typeof options === 'object' && !!options.once;
        list.push({ type: type, listener: listener, once: once });
    }

    function remove(target, type, listener) {
        var list = target.listeners;
        for (var i = 0; i < list.length; i++) {
            if (list[i].type === type && list[i].listener === listener) {
                list.splice(i, 1);
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
            nativeOp.call(bridge, 'lifecycleListenerError', eventName, text);
        } catch (e) {
            // 報告に失敗してもディスパッチは続ける。
        }
    }

    function fire(target, currentTarget, event, eventName, state) {
        // 呼び出し中の登録・削除の影響を受けないよう複製して走査する。
        var snapshot = target.listeners.slice();
        for (var i = 0; i < snapshot.length; i++) {
            if (state.immediateStopped) {
                return;
            }
            var entry = snapshot[i];
            if (entry.type !== eventName) {
                continue;
            }
            // 走査中に removeEventListener されたものは呼ばない。
            if (target.listeners.indexOf(entry) < 0) {
                continue;
            }
            if (entry.once) {
                remove(target, entry.type, entry.listener);
            }
            event.currentTarget = currentTarget;
            try {
                if (typeof entry.listener === 'function') {
                    entry.listener.call(currentTarget, event);
                } else if (typeof entry.listener.handleEvent === 'function') {
                    entry.listener.handleEvent(event);
                }
            } catch (e) {
                report(eventName, e);
            }
        }
    }

    function dispatch(eventName) {
        try {
            var state = { propagationStopped: false, immediateStopped: false };
            var event = {
                type: eventName,
                target: eventName === 'load' ? global : global.document,
                currentTarget: null,
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
        }
    }

    if (!Object.prototype.hasOwnProperty.call(global, '__fandheLifecycle')) {
        Object.defineProperty(global, '__fandheLifecycle', {
            value: dispatch,
            writable: false,
            configurable: false,
            enumerable: false
        });
    }
})(globalThis);
