// js_shim/events.js: 汎用 Event / EventTarget・document / window のリスナー保持・ライフサイクル通知の shim
// （TASK-109・Issue #781・#1177・ビヘイビア JS-4・JS-5・JS-6・SEC-2・REPAIR-3。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle が `document.addEventListener('DOMContentLoaded', ...)` /
// `window.addEventListener('load', ...)` で登録したリスナーを保持し、ページランナー
// （core の page_runner）が全スクリプトの実行後に呼ぶディスパッチャー `__fandheLifecycle` で
// 順に呼び出す。DOM やページ状態の実体は持たない（readyState は core 側の bridge op）。
// 呼び出し元: core の `js_shim::install`（dom.js・window.js・timers.js・abort.js の後に評価。
// リスナー例外の報告に abort.js の `DOMException` と timers.js の `queueMicrotask` を install 時に捕捉するため）と、
// ランナーの発火処理。history.js が `window.dispatchEvent` で popstate を配る。
//
// 設計上の要点:
// - ディスパッチャーは決して throw しない。リスナーの例外は try/catch で捕捉し、
//   `lifecycleListenerError` op で種類の名前・メッセージ（4097 UTF-16 単位で打ち切り）・
//   stack（先頭 1024 UTF-16 単位。core が opt-in 時のみ保持。Issue #1172）を core に渡して
//   後続のリスナーを続行する。
// - `__fandheLifecycle` は non-writable / non-configurable で固定する（__dom と同じ作法）。
//   再 install 時（同一 global への再評価）は窓口関数を残し、実体とリスナー状態だけを新しい評価のものへ差し替える。
//   差し替え（setImpl）は core が shim 注入中だけ立てるフラグ（`lifecycleInstallOpen` op）が true のときだけ有効で、
//   ページ JS が呼んでも dispatcher は変わらない。
// - DOMContentLoaded は document → window の順（バブリングの近似）、load は window のみ。
// - 汎用の `Event` / `EventTarget`（#1177・JS-5）: 内部状態は WeakMap とクロージャに置き、getter・メソッドは
//   brand check する。`dispatchEvent` は対象上で capture → 非 capture の順に登録順で呼び、once・passive・
//   stopImmediatePropagation を扱う。リスナーの例外は後続を止めず `dispatchEvent` の呼び出し元にも伝えず、
//   install 時に捕捉した `queueMicrotask` に throw する関数を積んで報告する（timers.js が `timerCallbackError`
//   〔source microtask〕として診断に残す）。ライフサイクル用の `lifecycleListenerError` op は Rust 側が
//   DOMContentLoaded / load しか受けないため使わない。配送中の再 dispatch は InvalidStateError（DOMException）。
// - ページが生成したイベントの `isTrusted` は常に false（信頼済みを装わない。SEC-2）。
// - `window.onpopstate` は HTML 仕様どおり、最初に非 null を代入した位置で呼ばれる内部エントリで実現する。
//   `PopStateEvent` / `onpopstate` は主要ブラウザすべてにある標準 API で、UA・navigator 等の識別面を
//   変えず、未実装を実装済みに見せる機能検出の偽装にもならない（SEC-2 の確認結果。#1177）。
// - ES2015 の範囲に留める（V8 と boa で同一のソースを使うため）。
//
// 未実装（REPAIR-3）:
// - 要素の dispatchEvent と、DOM の木に沿った capture / bubble の伝播。要素レベルの addEventListener /
//   removeEventListener は「保持のみ」のスタブ（#1151。React createRoot が container に約 80 種を登録するため、
//   TypeError で止まらないよう受け付ける）。ネイティブのイベント源が無く配送はしない。保持件数は 1 要素 1024 件まで。
// - `window` / `document` / `AbortSignal` は `EventTarget` を継承しない（`instanceof EventTarget` は false）。
//   `AbortSignal` は abort.js の自前の保持を使い、`dispatchEvent` も持たない（中途半端な継承で保持先が割れるのを避ける）。
// - `document.dispatchEvent` は bubbles が true なら window へも配るが、capture フェーズは近似しない。
// - `addEventListener` の `signal` オプション、`Event.timeStamp` / `composedPath` / `cancelBubble` / `returnValue` / `srcElement`、
//   CustomEvent。
// - onload / onreadystatechange 等の他のイベントハンドラ属性。
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

    function isObjectLike(v) {
        return v !== null && (typeof v === 'object' || typeof v === 'function');
    }

    // リスナーのオプション。boolean は capture、オブジェクトは capture・once・passive（signal は未実装。REPAIR-3）。
    function parseOptions(options) {
        var o = { capture: false, once: false, passive: false };
        if (isObjectLike(options)) {
            o.capture = !!options.capture;
            o.once = !!options.once;
            o.passive = !!options.passive;
        } else {
            o.capture = !!options;
        }
        return o;
    }

    function add(target, type, listener, options) {
        if (typeof type !== 'string' || !isListener(listener)) {
            return;
        }
        var opts = parseOptions(options);
        var list = target.items;
        for (var i = 0; i < target.count; i++) {
            if (list[i].type === type && list[i].listener === listener && list[i].capture === opts.capture) {
                return;
            }
        }
        if (target.count >= MAX_LISTENERS_PER_TARGET) {
            try {
                callOp('lifecycleListenerLimit');
            } catch (e) {
                // 通知に失敗しても登録は拒否したままにする。
            }
            return;
        }
        list[target.count] = {
            type: type,
            listener: listener,
            capture: opts.capture,
            once: opts.once,
            passive: opts.passive
        };
        target.count += 1;
    }

    // 内部エントリ（onpopstate 用）は件数上限の対象外で 1 件だけ足す。
    function addInternal(target, type, fn) {
        var entry = { type: type, listener: fn, capture: false, once: false, passive: false };
        target.items[target.count] = entry;
        target.count += 1;
        return entry;
    }

    function removeEntry(target, entry) {
        var list = target.items;
        for (var i = 0; i < target.count; i++) {
            if (list[i] === entry) {
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

    function remove(target, type, listener, options) {
        var capture = isObjectLike(options) ? !!options.capture : !!options;
        var list = target.items;
        for (var i = 0; i < target.count; i++) {
            if (list[i].type === type && list[i].listener === listener && list[i].capture === capture) {
                removeEntry(target, list[i]);
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

    var NativeTypeError = TypeError;
    var NativeDOMException = global.DOMException;
    var nativeQueueMicrotask = global.queueMicrotask;
    var strCtor = String;
    var symTag = Symbol.toStringTag;
    if (typeof NativeDOMException !== 'function') {
        throw new NativeTypeError('abort.js must be evaluated before events.js');
    }

    // 要素・汎用 EventTarget の保持先は WeakMap に遅延生成し、ラッパーと一緒に回収される。
    // window / document だけは上の固定の保持先を使う（EventTarget.prototype のメソッドを call されても同じ保持先に届く）。
    var objectTargets = new WeakMap();
    var weakGet = WeakMap.prototype.get;
    var weakSet = WeakMap.prototype.set;
    var weakHas = WeakMap.prototype.has;
    function recordFor(obj, create) {
        if (!isObjectLike(obj)) {
            throw new NativeTypeError('Illegal invocation');
        }
        if (obj === global) {
            return windowTarget;
        }
        if (obj === global.document) {
            return documentTarget;
        }
        if (fnCall(weakHas, objectTargets, obj)) {
            return fnCall(weakGet, objectTargets, obj);
        }
        if (!create) {
            return null;
        }
        var rec = newTarget();
        fnCall(weakSet, objectTargets, obj, rec);
        return rec;
    }

    function addEventListenerImpl(self, type, listener, options) {
        add(recordFor(self, true), type, listener, options);
    }
    function removeEventListenerImpl(self, type, listener, options) {
        var rec = recordFor(self, false);
        if (rec !== null) {
            remove(rec, type, listener, options);
        }
    }

    // ---- 汎用 Event（#1177・JS-5） ----
    var eventStates = new WeakMap();

    function reportLater(error) {
        if (typeof nativeQueueMicrotask !== 'function') {
            return;
        }
        try {
            nativeQueueMicrotask(function () {
                throw error;
            });
        } catch (e) {
            // 報告に失敗しても配送は続ける。
        }
    }

    function eventState(self) {
        var st = isObjectLike(self) && fnCall(weakHas, eventStates, self) ? fnCall(weakGet, eventStates, self) : undefined;
        if (st === undefined) {
            throw new NativeTypeError('Illegal invocation');
        }
        return st;
    }

    function Event(type, init) {
        if (!(this instanceof Event)) {
            throw new NativeTypeError("Failed to construct 'Event': Please use the 'new' operator");
        }
        if (arguments.length < 1) {
            throw new NativeTypeError("Failed to construct 'Event': 1 argument required");
        }
        if (typeof type === 'symbol') {
            throw new NativeTypeError('Cannot convert a Symbol value to a string');
        }
        var st = {
            type: strCtor(type),
            bubbles: false,
            cancelable: false,
            composed: false,
            canceled: false,
            target: null,
            current: null,
            phase: 0,
            dispatching: false,
            stopped: false,
            immediateStopped: false,
            inPassive: false
        };
        if (init !== undefined && init !== null) {
            if (!isObjectLike(init)) {
                throw new NativeTypeError("Failed to construct 'Event': parameter 2 is not of type 'EventInit'");
            }
            st.bubbles = !!init.bubbles;
            st.cancelable = !!init.cancelable;
            st.composed = !!init.composed;
        }
        fnCall(weakSet, eventStates, this, st);
    }

    function defineGetter(proto, name, fn) {
        var d = createObject(null);
        d.get = fn;
        d.configurable = true;
        d.enumerable = true;
        defineProperty(proto, name, d);
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

    defineGetter(Event.prototype, 'type', function () { return eventState(this).type; });
    defineGetter(Event.prototype, 'target', function () { return eventState(this).target; });
    defineGetter(Event.prototype, 'currentTarget', function () { return eventState(this).current; });
    defineGetter(Event.prototype, 'eventPhase', function () { return eventState(this).phase; });
    defineGetter(Event.prototype, 'bubbles', function () { return eventState(this).bubbles; });
    defineGetter(Event.prototype, 'cancelable', function () { return eventState(this).cancelable; });
    defineGetter(Event.prototype, 'defaultPrevented', function () { return eventState(this).canceled; });
    defineGetter(Event.prototype, 'composed', function () { return eventState(this).composed; });
    // ページが生成したイベントは信頼済みではない（SEC-2）。
    defineGetter(Event.prototype, 'isTrusted', function () { eventState(this); return false; });
    defineMethod(Event.prototype, 'preventDefault', function preventDefault() {
        var st = eventState(this);
        if (st.cancelable && !st.inPassive) {
            st.canceled = true;
        }
    });
    defineMethod(Event.prototype, 'stopPropagation', function stopPropagation() {
        eventState(this).stopped = true;
    });
    defineMethod(Event.prototype, 'stopImmediatePropagation', function stopImmediatePropagation() {
        var st = eventState(this);
        st.stopped = true;
        st.immediateStopped = true;
    });
    var PHASE_NAMES = ['NONE', 'CAPTURING_PHASE', 'AT_TARGET', 'BUBBLING_PHASE'];
    for (var pi = 0; pi < 4; pi++) {
        defineConst(Event, PHASE_NAMES[pi], pi);
        defineConst(Event.prototype, PHASE_NAMES[pi], pi);
    }
    defineTag(Event.prototype, 'Event');
    defineMethod(Event.prototype, 'constructor', Event);

    // 1 リスナーの呼び出し。例外は microtask 経由で報告し、呼び出し元へ伝えない。
    function invokeEntry(entry, thisArg, event, st) {
        st.inPassive = entry.passive;
        try {
            var l = entry.listener;
            if (typeof l === 'function') {
                fnCall(l, thisArg, event);
            } else {
                var h = l.handleEvent;
                if (typeof h === 'function') {
                    fnCall(h, l, event);
                }
            }
        } catch (e) {
            reportLater(e);
        } finally {
            st.inPassive = false;
        }
    }

    // mode: 1 = capture のみ・2 = 非 capture のみ。スナップショットに対し、走査中に外されたものは呼ばない。
    function runListeners(rec, currentObj, event, st, mode) {
        var source = rec.items;
        var sourceCount = rec.count;
        var snapshot = createObject(null);
        for (var k = 0; k < sourceCount; k++) {
            snapshot[k] = source[k];
        }
        for (var i = 0; i < sourceCount; i++) {
            if (st.immediateStopped) {
                return;
            }
            var entry = snapshot[i];
            if (entry.type !== st.type || entry.capture !== (mode === 1)) {
                continue;
            }
            var alive = false;
            for (var m = 0; m < rec.count; m++) {
                if (source[m] === entry) {
                    alive = true;
                    break;
                }
            }
            if (!alive) {
                continue;
            }
            if (entry.once) {
                removeEntry(rec, entry);
            }
            st.current = currentObj;
            invokeEntry(entry, currentObj, event, st);
        }
    }

    // parentObj / parentRec は bubbles のときだけ届ける先（document → window）。
    function dispatchImpl(targetObj, rec, event, parentObj, parentRec) {
        var st = eventState(event);
        if (st.dispatching) {
            throw new NativeDOMException('The event is already being dispatched.', 'InvalidStateError');
        }
        st.dispatching = true;
        st.target = targetObj;
        st.stopped = false;
        st.immediateStopped = false;
        try {
            st.phase = 2;
            runListeners(rec, targetObj, event, st, 1);
            runListeners(rec, targetObj, event, st, 2);
            if (st.bubbles && !st.stopped && parentRec !== null) {
                st.phase = 3;
                runListeners(parentRec, parentObj, event, st, 2);
            }
        } finally {
            st.phase = 0;
            st.current = null;
            st.dispatching = false;
            st.stopped = false;
            st.immediateStopped = false;
        }
        return !st.canceled;
    }

    function dispatchEventImpl(self, event, argc) {
        if (argc < 1 || !isObjectLike(event) || !fnCall(weakHas, eventStates, event)) {
            throw new NativeTypeError("Failed to execute 'dispatchEvent' on 'EventTarget': parameter 1 is not of type 'Event'");
        }
        if (self === global.document) {
            return dispatchImpl(self, documentTarget, event, global, windowTarget);
        }
        return dispatchImpl(self, recordFor(self, true), event, null, null);
    }

    function EventTarget() {
        if (!(this instanceof EventTarget)) {
            throw new NativeTypeError("Failed to construct 'EventTarget': Please use the 'new' operator");
        }
    }
    defineMethod(EventTarget.prototype, 'addEventListener', function addEventListener(type, listener, options) {
        addEventListenerImpl(this, type, listener, options);
    });
    defineMethod(EventTarget.prototype, 'removeEventListener', function removeEventListener(type, listener, options) {
        removeEventListenerImpl(this, type, listener, options);
    });
    defineMethod(EventTarget.prototype, 'dispatchEvent', function dispatchEvent(event) {
        return dispatchEventImpl(this, event, arguments.length);
    });
    defineMethod(EventTarget.prototype, 'constructor', EventTarget);
    defineTag(EventTarget.prototype, 'EventTarget');

    defineProperty(global, 'Event', { value: Event, writable: true, configurable: true, enumerable: false });
    defineProperty(global, 'EventTarget', { value: EventTarget, writable: true, configurable: true, enumerable: false });

    // window / document は own メソッド（EventTarget は継承しない。REPAIR-3）。
    function installTarget(obj) {
        defineMethod(obj, 'addEventListener', function addEventListener(type, listener, options) {
            add(recordFor(obj, true), type, listener, options);
        });
        defineMethod(obj, 'removeEventListener', function removeEventListener(type, listener, options) {
            remove(recordFor(obj, true), type, listener, options);
        });
        defineMethod(obj, 'dispatchEvent', function dispatchEvent(event) {
            return dispatchEventImpl(obj, event, arguments.length);
        });
    }
    installTarget(global.document);
    installTarget(global);

    // onpopstate: 最初に非 null を代入した位置で呼ぶ内部エントリ（HTML 仕様のイベントハンドラの順序）。
    var popstateHandler = null;
    var popstateRegistered = false;
    var popstateAccessor = createObject(null);
    popstateAccessor.get = function () {
        return popstateHandler;
    };
    var popstateEntry = null;
    popstateAccessor.set = function (v) {
        popstateHandler = typeof v === 'function' ? v : null;
        if (popstateHandler === null) {
            // 解除時は内部エントリも外し、再設定時に末尾へ追加し直す（HTML 仕様のハンドラ登録位置）。
            if (popstateRegistered) {
                removeEntry(windowTarget, popstateEntry);
                popstateEntry = null;
                popstateRegistered = false;
            }
            return;
        }
        if (!popstateRegistered) {
            popstateRegistered = true;
            popstateEntry = addInternal(windowTarget, 'popstate', function (e) {
                var h = popstateHandler;
                if (typeof h === 'function') {
                    fnCall(h, global, e);
                }
            });
        }
    };
    popstateAccessor.configurable = true;
    popstateAccessor.enumerable = true;
    defineProperty(global, 'onpopstate', popstateAccessor);

    // 要素レベルは Node.prototype に保持のみのスタブとして定義する（document は上の own プロパティが優先）。
    // dispatchEvent は意図して定義しない（REPAIR-3）。
    if (typeof global.Node === 'function') {
        defineMethod(global.Node.prototype, 'addEventListener', function addEventListener(type, listener, options) {
            addEventListenerImpl(this, type, listener, options);
        });
        defineMethod(global.Node.prototype, 'removeEventListener', function removeEventListener(type, listener, options) {
            removeEventListenerImpl(this, type, listener, options);
        });
    }

    function report(eventName, error) {
        try {
            var text;
            var name = '';
            try {
                // 例外の種類の名前（TypeError 等）。ページが書き換え得るため文字列のときだけ渡す。
                name = error && typeof error.name === 'string' ? error.name.slice(0, 64) : '';
            } catch (e) {
                name = '';
            }
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
            var stack = '';
            try {
                // error.stack はページが偽造できる。文字列のときだけ、サロゲートを割らずに切って渡す。
                if (error && typeof error.stack === 'string') {
                    stack = error.stack.slice(0, 1024);
                    var sl = stack.charCodeAt(stack.length - 1);
                    if (sl >= 0xd800 && sl <= 0xdbff) {
                        stack = stack.slice(0, -1);
                    }
                }
            } catch (e) {
                stack = '';
            }
            callOp('lifecycleListenerError', eventName, name, text, stack);
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
                removeEntry(target, entry);
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
            // 記述子は null プロトタイプで作る（Object.prototype.value 等の継承汚染で
            // get と value が混在して TypeError になるのを防ぐ。JS-4）。
            var descriptor = createObject(null);
            descriptor.get = function () {
                return state.current;
            };
            descriptor.enumerable = true;
            descriptor.configurable = false;
            defineProperty(event, 'currentTarget', descriptor);
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
