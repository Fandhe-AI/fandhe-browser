// js_shim/abort.js: DOMException / AbortSignal / AbortController の shim
// （TASK-108・TASK-111・Issue #1150・ビヘイビア JS-5・JS-6・JS-7・JS-8・REPAIR-3・SEC-2。
// SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle が使うグローバルの `AbortController` / `AbortSignal` / `DOMException` が
// `ReferenceError` で止まらないようにし、`fetch(url, { signal })` の中止（net.js）と
// `AbortSignal.timeout`（timers.js の `setTimeout` 経由）を成立させる。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の timers.js の後・net.js の前に評価）。
// install 時に op は呼ばない。timers.js の `setTimeout` / `queueMicrotask` は install 時に捕捉して使い、
// 実行時にグローバルを引き直さない（ページによる差し替えの影響を避ける。JS-4）。
//
// 設計上の要点:
// - 内部状態（中止済みか・reason・リスナー・onabort・内部の abort algorithm）はクロージャ内の WeakMap で持ち、
//   ページから見えるプロパティにしない。getter とメソッドは受け手を brand check し、AbortSignal でなければ
//   TypeError にする。
// - リスナー保持は events.js と同じく「プロトタイプを持たないコンテナ＋件数」で、1 シグナル 1024 件まで
//   （JS-6）。超えた登録は無視して既存の `lifecycleListenerLimit` op で通知する。
// - リスナーの例外は後続のリスナーを止めず、`abort()` の呼び出し元にも伝えない。捕捉した例外は install 時に
//   捕捉した `queueMicrotask` に「throw する関数」を積んで報告する（timers.js の drain が既存の
//   `timerCallbackError`〔source microtask〕として診断に残す）。
// - net.js の fetch が中止に追従するため、窓口関数 `__fandheAbortFollow` を non-writable / non-configurable で
//   固定する。受け手が本物の AbortSignal のときだけ動き、できるのは「そのシグナルへの追従の登録・解除と状態の
//   問い合わせ」で、`addEventListener` でできる範囲に収まる。再 install 時の実体の差し替えは events.js と同じく
//   `lifecycleInstallOpen` op が true のときだけ許す。
// - 文言は固定の英文で、入力（reason・URL）を反響しない。ES2015 の範囲に留める（V8 と boa で同一ソース）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - 汎用の `EventTarget` / `Event` / `dispatchEvent` は定義しない（`typeof` は 'undefined' のまま。機能検出で
//   対応を装わない。SEC-2）。AbortSignal は `abort` イベントだけを自前で配る。将来仕様: 汎用 EventTarget（JS-5）。
// - `onabort` はリスナー一覧の設定位置ではなく最初に呼ぶ。addEventListener の `signal` / `passive` / capture は無視する。
// - `DOMException` は最小実装（`stack` なし、旧来の code 定数は主要なもの）。既存のブリッジエラーの DOMException
//   への写像は未実装。
// - `AbortSignal.timeout` は timers.js の `setTimeout` の登録に失敗した（id が 0）場合、中止されないシグナルを返す。
//   `queueMicrotask` が使えない環境ではリスナーの例外を捨てる。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var callOp = Function.prototype.call.bind(bridge.op, bridge);
    var defineProperty = Object.defineProperty;
    var createObject = Object.create;
    var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    var fnCall = Function.prototype.call.bind(Function.prototype.call);
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var NativeTypeError = TypeError;
    var NativeError = Error;
    var toStr = String;
    var toNumber = Number;
    var isFinite_ = Number.isFinite;
    var floor = Math.floor;
    var symIter = Symbol.iterator;
    var symTag = Symbol.toStringTag;
    var nativeSetTimeout = global.setTimeout;
    var nativeQueueMicrotask = global.queueMicrotask;
    // 1 シグナルあたりのリスナー・内部 algorithm の件数と、any の入力件数の上限（JS-6）。
    var MAX_ENTRIES = 1024;

    var domStates = new WeakMap();
    var signalStates = new WeakMap();
    var controllerSignals = new WeakMap();

    function defineMethod(obj, name, fn) {
        defineProperty(obj, name, { value: fn, writable: true, configurable: true, enumerable: false });
    }

    function defineAccessor(obj, name, getter, setter) {
        var d = createObject(null);
        d.get = getter;
        if (setter !== undefined) {
            d.set = setter;
        }
        d.configurable = true;
        d.enumerable = true;
        defineProperty(obj, name, d);
    }

    function defineConst(obj, name, value) {
        var d = createObject(null);
        d.value = value;
        d.enumerable = true;
        defineProperty(obj, name, d);
    }

    function defineTag(proto, tag) {
        var d = createObject(null);
        d.value = tag;
        d.configurable = true;
        defineProperty(proto, symTag, d);
    }

    // ---- DOMException（最小） ----

    var CODE_NAMES = ['IndexSizeError', 'HierarchyRequestError', 'WrongDocumentError',
        'InvalidCharacterError', 'NoModificationAllowedError', 'NotFoundError', 'NotSupportedError',
        'InUseAttributeError', 'InvalidStateError', 'SyntaxError', 'InvalidModificationError',
        'NamespaceError', 'InvalidAccessError', 'TypeMismatchError', 'SecurityError', 'NetworkError',
        'AbortError', 'URLMismatchError', 'QuotaExceededError', 'TimeoutError', 'InvalidNodeTypeError',
        'DataCloneError'];
    var CODE_VALUES = [1, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23, 24, 25];
    var CONST_NAMES = ['INDEX_SIZE_ERR', 'HIERARCHY_REQUEST_ERR', 'WRONG_DOCUMENT_ERR',
        'INVALID_CHARACTER_ERR', 'NO_MODIFICATION_ALLOWED_ERR', 'NOT_FOUND_ERR', 'NOT_SUPPORTED_ERR',
        'INUSE_ATTRIBUTE_ERR', 'INVALID_STATE_ERR', 'SYNTAX_ERR', 'INVALID_MODIFICATION_ERR',
        'NAMESPACE_ERR', 'INVALID_ACCESS_ERR', 'TYPE_MISMATCH_ERR', 'SECURITY_ERR', 'NETWORK_ERR',
        'ABORT_ERR', 'URL_MISMATCH_ERR', 'QUOTA_EXCEEDED_ERR', 'TIMEOUT_ERR', 'INVALID_NODE_TYPE_ERR',
        'DATA_CLONE_ERR'];

    function codeFor(name) {
        for (var i = 0; i < CODE_NAMES.length; i++) {
            if (CODE_NAMES[i] === name) {
                return CODE_VALUES[i];
            }
        }
        return 0;
    }

    function DOMException(message, name) {
        if (!(this instanceof DOMException)) {
            throw new NativeTypeError("Failed to construct 'DOMException': Please use the 'new' operator");
        }
        var n = name === undefined ? 'Error' : toStr(name);
        wmSet(domStates, this, {
            message: message === undefined ? '' : toStr(message),
            name: n,
            code: codeFor(n)
        });
    }
    DOMException.prototype = createObject(NativeError.prototype);
    defineMethod(DOMException.prototype, 'constructor', DOMException);

    function domGetter(key) {
        return function () {
            var st = wmGet(domStates, this);
            if (st === undefined) {
                throw new NativeTypeError('Illegal invocation');
            }
            return st[key];
        };
    }
    defineAccessor(DOMException.prototype, 'name', domGetter('name'));
    defineAccessor(DOMException.prototype, 'message', domGetter('message'));
    defineAccessor(DOMException.prototype, 'code', domGetter('code'));
    for (var ci = 0; ci < CONST_NAMES.length; ci++) {
        defineConst(DOMException, CONST_NAMES[ci], CODE_VALUES[ci]);
        defineConst(DOMException.prototype, CONST_NAMES[ci], CODE_VALUES[ci]);
    }
    defineTag(DOMException.prototype, 'DOMException');

    // ---- AbortSignal ----

    function newList() {
        return { items: createObject(null), count: 0 };
    }

    function notifyLimit() {
        try {
            callOp('lifecycleListenerLimit');
        } catch (e) {
            // 通知に失敗しても登録は拒否したままにする。
        }
    }

    function isListener(l) {
        return typeof l === 'function' || (l !== null && typeof l === 'object');
    }

    function stateOf(signal) {
        var st = wmGet(signalStates, signal);
        if (st === undefined) {
            throw new NativeTypeError('Illegal invocation');
        }
        return st;
    }

    function removeAt(list, i) {
        var len = list.count;
        for (var j = i; j < len - 1; j++) {
            list.items[j] = list.items[j + 1];
        }
        delete list.items[len - 1];
        list.count = len - 1;
    }

    function reportLater(error) {
        if (typeof nativeQueueMicrotask !== 'function') {
            return;
        }
        try {
            nativeQueueMicrotask(function () {
                throw error;
            });
        } catch (e) {
            // 報告に失敗しても abort の処理は続ける。
        }
    }

    function AbortSignal() {
        throw new NativeTypeError('Illegal constructor');
    }

    function createSignal() {
        var signal = createObject(AbortSignal.prototype);
        wmSet(signalStates, signal, {
            aborted: false,
            reason: undefined,
            listeners: newList(),
            algs: newList(),
            onabort: null
        });
        return signal;
    }

    function callListener(listener, signal, event) {
        if (typeof listener === 'function') {
            fnCall(listener, signal, event);
        } else if (typeof listener.handleEvent === 'function') {
            listener.handleEvent(event);
        }
    }

    function fireAbort(signal, st) {
        var flags = { stopped: false, current: signal };
        var event = {
            type: 'abort',
            target: signal,
            bubbles: false,
            cancelable: false,
            defaultPrevented: false,
            isTrusted: true,
            preventDefault: function () {},
            stopPropagation: function () {},
            stopImmediatePropagation: function () {
                flags.stopped = true;
            }
        };
        var d = createObject(null);
        d.get = function () {
            return flags.current;
        };
        d.enumerable = true;
        d.configurable = false;
        defineProperty(event, 'currentTarget', d);
        try {
            var handler = st.onabort;
            if (typeof handler === 'function') {
                try {
                    fnCall(handler, signal, event);
                } catch (e) {
                    reportLater(e);
                }
            }
            var list = st.listeners;
            var n = list.count;
            var snapshot = createObject(null);
            for (var k = 0; k < n; k++) {
                snapshot[k] = list.items[k];
            }
            for (var i = 0; i < n; i++) {
                if (flags.stopped) {
                    break;
                }
                var entry = snapshot[i];
                if (entry.type !== 'abort') {
                    continue;
                }
                // 走査中に removeEventListener されたものは呼ばない。
                var at = -1;
                for (var m = 0; m < list.count; m++) {
                    if (list.items[m] === entry) {
                        at = m;
                        break;
                    }
                }
                if (at < 0) {
                    continue;
                }
                if (entry.once) {
                    removeAt(list, at);
                }
                try {
                    callListener(entry.listener, signal, event);
                } catch (e) {
                    reportLater(e);
                }
            }
        } finally {
            flags.current = null;
        }
    }

    // any の結果シグナルへの追従 algorithm（関数 → { target, detach }）。signalAbort が中身を見て反復で伝播する。
    var links = new WeakMap();

    // 仕様の "signal abort"。すでに中止済みなら何もしない。
    // 依存シグナル（any）への伝播は明示的なスタックによる反復で行い、依存の連鎖が深くてもスタックを消費しない。
    // 全依存の状態を先に更新してから、中止した順に abort イベントを配る。
    function signalAbort(signal, reason) {
        var pending = [signal];
        var fired = [];
        var rootReason = reason;
        while (pending.length > 0) {
            var cur = pending.pop();
            var st = wmGet(signalStates, cur);
            if (st === undefined || st.aborted) {
                continue;
            }
            st.aborted = true;
            st.reason = cur === signal
                ? (rootReason === undefined
                    ? new DOMException('signal is aborted without reason', 'AbortError')
                    : rootReason)
                : rootReason;
            if (cur === signal) {
                rootReason = st.reason;
            }
            fired.push(cur);
            var algs = st.algs;
            var n = algs.count;
            var snapshot = createObject(null);
            for (var k = 0; k < n; k++) {
                snapshot[k] = algs.items[k];
            }
            for (var j = 0; j < n; j++) {
                delete algs.items[j];
            }
            algs.count = 0;
            for (var i = 0; i < n; i++) {
                var link = wmGet(links, snapshot[i]);
                if (link !== undefined) {
                    link.detach();
                    pending.push(link.target);
                    continue;
                }
                try {
                    snapshot[i](st.reason);
                } catch (e) {
                    reportLater(e);
                }
            }
        }
        for (var f = 0; f < fired.length; f++) {
            fireAbort(fired[f], wmGet(signalStates, fired[f]));
        }
    }

    function addAlg(st, fn) {
        if (st.algs.count >= MAX_ENTRIES) {
            return false;
        }
        st.algs.items[st.algs.count] = fn;
        st.algs.count += 1;
        return true;
    }

    function removeAlg(st, fn) {
        for (var i = 0; i < st.algs.count; i++) {
            if (st.algs.items[i] === fn) {
                removeAt(st.algs, i);
                return;
            }
        }
    }

    defineAccessor(AbortSignal.prototype, 'aborted', function () {
        return stateOf(this).aborted;
    });
    defineAccessor(AbortSignal.prototype, 'reason', function () {
        return stateOf(this).reason;
    });
    defineAccessor(AbortSignal.prototype, 'onabort', function () {
        return stateOf(this).onabort;
    }, function (value) {
        stateOf(this).onabort = typeof value === 'function' ? value : null;
    });
    defineMethod(AbortSignal.prototype, 'throwIfAborted', function throwIfAborted() {
        var st = stateOf(this);
        if (st.aborted) {
            throw st.reason;
        }
    });
    defineMethod(AbortSignal.prototype, 'addEventListener', function addEventListener(type, listener, options) {
        var st = stateOf(this);
        if (typeof type !== 'string' || !isListener(listener)) {
            return;
        }
        // options の getter はページ定義コードなので、重複・件数の確認より前に 1 回だけ読む。
        // 確認から登録までの間にページのコードを走らせない（getter 内の再登録で上限を回避させない。JS-6）。
        var once = options !== null && typeof options === 'object' && !!options.once;
        var list = st.listeners;
        for (var i = 0; i < list.count; i++) {
            if (list.items[i].type === type && list.items[i].listener === listener) {
                return;
            }
        }
        if (list.count >= MAX_ENTRIES) {
            notifyLimit();
            return;
        }
        list.items[list.count] = { type: type, listener: listener, once: once };
        list.count += 1;
    });
    defineMethod(AbortSignal.prototype, 'removeEventListener', function removeEventListener(type, listener) {
        var list = stateOf(this).listeners;
        for (var i = 0; i < list.count; i++) {
            if (list.items[i].type === type && list.items[i].listener === listener) {
                removeAt(list, i);
                return;
            }
        }
    });
    defineTag(AbortSignal.prototype, 'AbortSignal');

    defineMethod(AbortSignal, 'abort', function abort(reason) {
        var signal = createSignal();
        signalAbort(signal, reason);
        return signal;
    });

    defineMethod(AbortSignal, 'timeout', function timeout(ms) {
        var n = toNumber(ms);
        if (!isFinite_(n) || n < 0) {
            throw new NativeTypeError("Failed to execute 'timeout' on 'AbortSignal': Value is not a valid delay");
        }
        var signal = createSignal();
        if (typeof nativeSetTimeout === 'function') {
            // 丸めと上限は core の timerSet が受け持つ。登録に失敗（id 0）しても中止されないシグナルのまま返す。
            nativeSetTimeout(function () {
                signalAbort(signal, new DOMException('signal timed out', 'TimeoutError'));
            }, floor(n));
        }
        return signal;
    });

    defineMethod(AbortSignal, 'any', function any(signals) {
        var inputs = createObject(null);
        var count = 0;
        var it = signals[symIter]();
        for (;;) {
            var step = it.next();
            if (step.done) {
                break;
            }
            if (count >= MAX_ENTRIES) {
                throw new NativeTypeError("Failed to execute 'any' on 'AbortSignal': Too many signals");
            }
            if (wmGet(signalStates, step.value) === undefined) {
                throw new NativeTypeError("Failed to execute 'any' on 'AbortSignal': Element is not an AbortSignal");
            }
            inputs[count] = step.value;
            count += 1;
        }
        var result = createSignal();
        var i;
        for (i = 0; i < count; i++) {
            var ist = wmGet(signalStates, inputs[i]);
            if (ist.aborted) {
                signalAbort(result, ist.reason);
                return result;
            }
        }
        // 結果が中止されたら、全入力に登録した follow を解除する（JS-6。長寿命の入力に残り続けて上限に達するのを防ぐ）。
        var detach = function () {
            for (var d = 0; d < count; d++) {
                removeAlg(wmGet(signalStates, inputs[d]), follow);
            }
        };
        // 中止時の伝播は signalAbort が links を見て反復で行う（深い連鎖でも再帰しない）。
        var follow = function () {};
        wmSet(links, follow, { target: result, detach: detach });
        for (i = 0; i < count; i++) {
            if (!addAlg(wmGet(signalStates, inputs[i]), follow)) {
                // 登録途中の失敗は、ここまでの登録を巻き戻してから投げる。
                detach();
                throw new NativeTypeError("Failed to execute 'any' on 'AbortSignal': Too many dependents");
            }
        }
        return result;
    });

    // ---- AbortController ----

    function AbortController() {
        if (!(this instanceof AbortController)) {
            throw new NativeTypeError("Failed to construct 'AbortController': Please use the 'new' operator");
        }
        wmSet(controllerSignals, this, createSignal());
    }
    defineAccessor(AbortController.prototype, 'signal', function () {
        var signal = wmGet(controllerSignals, this);
        if (signal === undefined) {
            throw new NativeTypeError('Illegal invocation');
        }
        return signal;
    });
    defineMethod(AbortController.prototype, 'abort', function abort(reason) {
        var signal = wmGet(controllerSignals, this);
        if (signal === undefined) {
            throw new NativeTypeError('Illegal invocation');
        }
        signalAbort(signal, reason);
    });
    defineTag(AbortController.prototype, 'AbortController');

    function defineGlobal(name, value) {
        defineProperty(global, name, { value: value, writable: true, configurable: true, enumerable: false });
    }
    defineGlobal('DOMException', DOMException);
    defineGlobal('AbortSignal', AbortSignal);
    defineGlobal('AbortController', AbortController);

    // ---- net.js 向けの窓口 ----
    // follow(signal) → 0: AbortSignal ではない / 1: 未中止 / 2: 中止済み。
    // follow(signal, fn) → fn を中止時の内部 algorithm として登録（reason を渡す）。登録できたかを真偽値で返す。
    // follow(signal, fn, true) → 登録済みの fn を解除する。
    function follow(signal, fn, remove) {
        var st = wmGet(signalStates, signal);
        if (st === undefined) {
            return fn === undefined ? 0 : false;
        }
        if (fn === undefined) {
            return st.aborted ? 2 : 1;
        }
        if (typeof fn !== 'function') {
            return false;
        }
        if (remove === true) {
            removeAlg(st, fn);
            return true;
        }
        if (st.aborted) {
            return false;
        }
        return addAlg(st, fn);
    }

    var existing = hasOwn(global, '__fandheAbortFollow') ? global.__fandheAbortFollow : null;
    if (existing !== null && typeof existing === 'function' && existing.__fandheImpl === true) {
        existing.setImpl(follow);
    } else if (existing === null) {
        var currentImpl = follow;
        var entry = function __fandheAbortFollow(signal, fn, remove) {
            try {
                return currentImpl(signal, fn, remove);
            } catch (e) {
                return fn === undefined ? 0 : false;
            }
        };
        defineProperty(entry, '__fandheImpl', { value: true });
        defineProperty(entry, 'setImpl', {
            // 差し替えは Rust 側が shim 注入中だけ立てるフラグが true のときに限る。
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
        defineProperty(global, '__fandheAbortFollow', {
            value: entry,
            writable: false,
            configurable: false,
            enumerable: false
        });
    }
})(globalThis);
