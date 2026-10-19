// js_shim/observers.js: MutationObserver / MutationRecord の shim
// （TASK-108・Issue #1138・ビヘイビア JS-5・JS-7・JS-6。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle（Saucedemo・DemoQA の module script 等）が参照する `MutationObserver` を
// 提供し、DOM 変更の通知を microtask で配信する。変更の検出は dom.js のラッパー
// （appendChild / insertBefore / removeChild / setAttribute / removeAttribute / innerHTML /
// textContent）が呼ぶフックを介して行い、この shim が仲介する操作に限る。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の末尾。timers.js の後に評価）。
// 配信 microtask は timers.js の `queueMicrotask`（install 時に捕捉）で積むため、ランナーの
// microtask checkpoint（`__fandheRunMicrotasks`。classic / module の各 script 後・タイマー発火後）で実行される。
//
// 設計上の要点:
// - dom.js が install 中だけ公開する一時グローバル `__fandheDomMutationHandoff`
//   （setSink / contains）をここで受け取って直ちに削除する。ページ JS からは見えず、
//   ノード ID はラッパーの外へ出ない。ハンドオフが無ければ TypeError で fail-closed にする。
// - `queueMicrotask` / `Function.prototype.call` 等は install 時に捕捉する。ページが後から
//   差し替えても配信は影響を受けない（timers.js と同じ作法）。
// - 配信 microtask は全 observer で 1 本（フラグで重複を避ける）。同一 microtask 内の複数変更は
//   observer ごとに 1 回の callback 呼び出しにまとまる。callback の例外は observer ごとに捕捉し、
//   `timerCallbackError` op（source "microtask"）で種類の名前とメッセージだけ通知して残りへ配信を続ける。
// - 上限（JS-6）: 保留レコードの合計と登録数（observer × target）に上限を設ける。超過分は破棄し、
//   `mutationObserverLimit` op で一度だけ通知する（ランナーが ResourceLimit で打ち切る）。
// - `subtree` は変更先ノードの「現在の祖先関係」を `isInclusiveAncestor` op で判定する。登録先と
//   変更先が同じなら op を呼ばない。op の消費は 1 変更あたり subtree 登録数以下（登録数上限で抑える）。
// - 配列メソッドは使わず（Array.prototype の改変の影響を避ける）、添字演算と null プロトタイプの
//   コンテナで状態を持つ。ES2015 の範囲に留める（V8 と boa で同一ソース。class / Proxy は使わない）。
// - 偽装はしない（SEC-2）。`WebKitMutationObserver` 等のベンダー別名は定義しない。
//
// 未実装（REPAIR-3。黙って成功扱いにはせず、無視した options は `mutationOptionsIgnored` op の
// 診断件数で明示する）:
// - transient registered observer（削除されたノード配下の変更の追跡）。
// - oldValue（`attributeOldValue` / `characterDataOldValue` は受理するが無視し、診断に数える。
//   `MutationRecord.oldValue` は常に null）。
// - `innerHTML` / `textContent` 設定時の `addedNodes` / `removedNodes`（子の列挙 op が無いため空配列）。
// - `previousSibling`（常に null）。`nextSibling` は insertBefore の基準ノードのみで、appendChild では null。
// - 移動元の親での削除レコード（appendChild で既存ノードを別の親へ移すときの childList）。
// - shim を介さない変更（HTML パーサー・Rust 側の変更）の検出。
// - NodeList 互換（addedNodes / removedNodes は素の Array）。
// - 配信順序は「最初のレコードが積まれた順」で、DOM 仕様の observer 作成順ではない。
// - `attributeFilter` は配列・iterable（Set 等。iterator で読む）・array-like を受理し、64 要素までで、超過分は無視して診断に数える。名前の大文字小文字の正規化はしない。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var callOp = Function.prototype.call.bind(bridge.op, bridge);
    var fnCall = Function.prototype.call.bind(Function.prototype.call);
    var defineProperty = Object.defineProperty;
    var createObject = Object.create;
    var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    var toNumber = Number;
    var symIterator = typeof Symbol === 'function' ? Symbol.iterator : undefined;
    var toStr = String;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var MAX_TEXT_UNITS = 4097;
    var MAX_ATTRIBUTE_FILTER = 64;
    // 保留中のレコード数（全 observer の合計）と、登録数（observer × target）の上限（JS-6）。
    var MAX_PENDING_MUTATION_RECORDS = 10000;
    var MAX_MUTATION_OBSERVER_REGISTRATIONS = 1000;

    // dom.js からのハンドオフを受け取り、直ちに削除する。
    var handoff = global.__fandheDomMutationHandoff;
    if (!handoff || typeof handoff.setSink !== 'function' || typeof handoff.contains !== 'function') {
        throw new TypeError('MutationObserver hand-off from dom.js is not available');
    }
    var handoffSetSink = handoff.setSink;
    var handoffContains = handoff.contains;
    delete global.__fandheDomMutationHandoff;

    // timers.js の queueMicrotask を install 時に捕捉する。
    var enqueueMicrotask = global.queueMicrotask;
    if (typeof enqueueMicrotask !== 'function') {
        throw new TypeError('queueMicrotask is not available');
    }

    var TOKEN = {};
    // observer → 内部状態 { callback, observer, records, queued, stamp }。
    var states = new WeakMap();
    // 全登録 { state, target, opts }。
    var registrations = [];
    var pendingTotal = 0;
    var limitNotified = false;
    var deliveryQueued = false;
    var notifyList = [];
    var stampCounter = 0;

    function report(error) {
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
                text = toStr(error && error.message !== undefined ? error.message : error);
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
            callOp('timerCallbackError', 'microtask', name, text);
        } catch (e) {
            // 報告に失敗しても続行する。
        }
    }

    // 上限超過を Rust 側へ通知する。成功したら以後は op を省き、失敗したら次回に再試行する。
    function notifyLimit() {
        if (limitNotified) {
            return;
        }
        try {
            callOp('mutationObserverLimit');
            limitNotified = true;
        } catch (e) {
            // 通知に失敗しても破棄は維持する。
        }
    }

    function MutationRecord(token) {
        if (token !== TOKEN) {
            throw new TypeError('Illegal constructor');
        }
    }

    function makeRecord(type, target, info) {
        var r = createObject(MutationRecord.prototype);
        r.type = type;
        r.target = target;
        r.addedNodes = info !== null && info.added !== undefined ? copyList(info.added) : [];
        r.removedNodes = info !== null && info.removed !== undefined ? copyList(info.removed) : [];
        r.previousSibling = null;
        r.nextSibling = info !== null && info.nextSibling !== undefined ? info.nextSibling : null;
        r.attributeName = type === 'attributes' && info !== null ? info.attributeName : null;
        r.attributeNamespace = null;
        r.oldValue = null;
        return r;
    }

    // observer ごとに独立した配列を渡す（ページが 1 つを書き換えても他へ波及させない）。
    function copyList(list) {
        var out = [];
        for (var i = 0; i < list.length; i++) {
            out[i] = list[i];
        }
        return out;
    }

    function stateOf(observer, method) {
        var state = wmGet(states, observer);
        if (state === undefined) {
            throw new TypeError("Failed to execute '" + method + "' on 'MutationObserver': Illegal invocation");
        }
        return state;
    }

    function deliver() {
        deliveryQueued = false;
        var list = notifyList;
        notifyList = [];
        for (var i = 0; i < list.length; i++) {
            var state = list[i];
            state.queued = false;
            var recs = state.records;
            var n = recs.length;
            if (n === 0) {
                continue;
            }
            state.records = [];
            pendingTotal -= n;
            try {
                fnCall(state.callback, state.observer, recs, state.observer);
            } catch (e) {
                report(e);
            }
        }
    }

    // 配信待ちリストから state を除く。disconnect / takeRecords でレコードが空になった state を
    // 残すと、登録・変更・disconnect の繰り返しで observer と callback が解放されない（JS-6）。
    // これにより notifyList の長さは常に「レコードを持つ state 数」以下（<= pendingTotal 上限）に抑えられる。
    function unqueue(state) {
        if (!state.queued) {
            return;
        }
        state.queued = false;
        var kept = 0;
        for (var i = 0; i < notifyList.length; i++) {
            if (notifyList[i] !== state) {
                notifyList[kept] = notifyList[i];
                kept += 1;
            }
        }
        notifyList.length = kept;
    }

    function enqueue(state, record) {
        if (pendingTotal >= MAX_PENDING_MUTATION_RECORDS) {
            notifyLimit();
            return;
        }
        state.records[state.records.length] = record;
        pendingTotal += 1;
        if (!state.queued) {
            state.queued = true;
            notifyList[notifyList.length] = state;
        }
        if (!deliveryQueued) {
            deliveryQueued = true;
            fnCall(enqueueMicrotask, global, deliver);
        }
    }

    function removeRegistrations(state) {
        var kept = 0;
        for (var i = 0; i < registrations.length; i++) {
            var reg = registrations[i];
            if (reg.state !== state) {
                registrations[kept] = reg;
                kept += 1;
            }
        }
        registrations.length = kept;
    }

    // dom.js のラッパーが op 成功後に呼ぶフック。例外は dom.js 側で握りつぶされる。
    function onMutation(kind, target, info) {
        if (registrations.length === 0) {
            return;
        }
        stampCounter += 1;
        var stamp = stampCounter;
        // 走査中に callback が登録を変えることはない（配信は microtask）ため、そのまま走査できる。
        for (var i = 0; i < registrations.length; i++) {
            var reg = registrations[i];
            var opts = reg.opts;
            if (kind === 'childList' ? !opts.childList : kind === 'attributes' ? !opts.attributes : !opts.characterData) {
                continue;
            }
            if (kind === 'attributes' && opts.filter !== null && !hasOwn(opts.filter, info.attributeName)) {
                continue;
            }
            if (reg.target !== target) {
                if (!opts.subtree) {
                    continue;
                }
                var inside = false;
                try {
                    inside = fnCall(handoffContains, handoff, reg.target, target) === true;
                } catch (e) {
                    inside = false;
                }
                if (!inside) {
                    continue;
                }
            }
            if (reg.state.stamp === stamp) {
                continue;
            }
            reg.state.stamp = stamp;
            enqueue(reg.state, makeRecord(kind, target, info));
        }
    }

    // observe の options を検証・正規化する。無視するオプションの件数は ignored.count に積む。
    function normalizeOptions(o, ignored) {
        if (o === null || (typeof o !== 'object' && typeof o !== 'function')) {
            throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object must be an object.");
        }
        var attributes = o.attributes;
        var characterData = o.characterData;
        var attributeOldValue = o.attributeOldValue;
        var characterDataOldValue = o.characterDataOldValue;
        var filterArg = o.attributeFilter;
        if (attributes === undefined && (attributeOldValue !== undefined || filterArg !== undefined)) {
            attributes = true;
        }
        if (characterData === undefined && characterDataOldValue !== undefined) {
            characterData = true;
        }
        var opts = createObject(null);
        opts.childList = !!o.childList;
        opts.subtree = !!o.subtree;
        opts.attributes = !!attributes;
        opts.characterData = !!characterData;
        opts.filter = null;
        if (!opts.childList && !opts.attributes && !opts.characterData) {
            throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.");
        }
        // 矛盾の判定は値の真偽で行う（`attributeOldValue: false` は有効な指定）。attributeFilter は存在で判定する。
        if (attributes !== undefined && !opts.attributes && (attributeOldValue || filterArg !== undefined)) {
            throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'attributeOldValue' or 'attributeFilter' when 'attributes' is true or not present.");
        }
        if (characterData !== undefined && !opts.characterData && characterDataOldValue) {
            throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'characterDataOldValue' when 'characterData' is true or not present.");
        }
        if (filterArg !== undefined) {
            if (filterArg === null || typeof filterArg !== 'object') {
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The provided value cannot be converted to a sequence.");
            }
            var filter = createObject(null);
            var overflow = false;
            var iterFn = symIterator ? filterArg[symIterator] : undefined;
            if (typeof iterFn === 'function') {
                // Set 等の iterable は iterator で読む。上限 +1 件まで読んで超過を検出する（無限 iterator 対策）。
                var it = fnCall(iterFn, filterArg);
                var count = 0;
                while (true) {
                    var step = it.next();
                    if (step.done) {
                        break;
                    }
                    if (count >= MAX_ATTRIBUTE_FILTER) {
                        overflow = true;
                        if (typeof it['return'] === 'function') {
                            it['return']();
                        }
                        break;
                    }
                    filter[toStr(step.value)] = true;
                    count++;
                }
            } else {
                var len = toNumber(filterArg.length);
                if (!(len >= 0)) {
                    len = 0;
                }
                var take = len < MAX_ATTRIBUTE_FILTER ? len : MAX_ATTRIBUTE_FILTER;
                for (var i = 0; i < take; i++) {
                    filter[toStr(filterArg[i])] = true;
                }
                overflow = len > MAX_ATTRIBUTE_FILTER;
            }
            opts.filter = filter;
            if (overflow) {
                // 超過分はフィルターに入らない（通知対象外になる）ため、無視した件数として診断に出す。
                ignored.count += 1;
            }
        }
        // oldValue は未実装（REPAIR-3）。受理したうえで無視し、診断に数える。
        if (attributeOldValue) {
            ignored.count += 1;
        }
        if (characterDataOldValue) {
            ignored.count += 1;
        }
        return opts;
    }

    function MutationObserver(callback) {
        if (!(this instanceof MutationObserver)) {
            throw new TypeError("Failed to construct 'MutationObserver': Please use the 'new' operator.");
        }
        if (typeof callback !== 'function') {
            throw new TypeError("Failed to construct 'MutationObserver': The callback provided as parameter 1 is not a function.");
        }
        wmSet(states, this, {
            callback: callback,
            observer: this,
            records: [],
            queued: false,
            stamp: 0
        });
    }

    function define(proto, name, value) {
        defineProperty(proto, name, {
            value: value,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    define(MutationObserver.prototype, 'observe', function observe(target, options) {
        var state = stateOf(this, 'observe');
        // ノード ID は dom.js の内側にしか無いため、自己比較の contains（ID の解決に失敗すれば
        // throw）で target が shim のノードかを確かめる。
        var ok = false;
        try {
            ok = fnCall(handoffContains, handoff, target, target) === true;
        } catch (e) {
            ok = false;
        }
        if (!ok) {
            throw new TypeError("Failed to execute 'observe' on 'MutationObserver': parameter 1 is not of type 'Node'.");
        }
        var ignored = { count: 0 };
        var opts = normalizeOptions(options, ignored);
        if (ignored.count > 0) {
            try {
                callOp('mutationOptionsIgnored', ignored.count);
            } catch (e) {
                // 診断の通知に失敗しても登録は続ける。
            }
        }
        for (var i = 0; i < registrations.length; i++) {
            var existing = registrations[i];
            if (existing.state === state && existing.target === target) {
                // 同じ target への再 observe はオプションを置き換える。
                existing.opts = opts;
                return;
            }
        }
        if (registrations.length >= MAX_MUTATION_OBSERVER_REGISTRATIONS) {
            // throw せず無視し、Rust 側の ResourceLimit で打ち切りを確定させる（JS-6）。
            notifyLimit();
            return;
        }
        registrations[registrations.length] = { state: state, target: target, opts: opts };
    });
    define(MutationObserver.prototype, 'disconnect', function disconnect() {
        var state = stateOf(this, 'disconnect');
        removeRegistrations(state);
        pendingTotal -= state.records.length;
        state.records = [];
        unqueue(state);
    });
    define(MutationObserver.prototype, 'takeRecords', function takeRecords() {
        var state = stateOf(this, 'takeRecords');
        var recs = state.records;
        pendingTotal -= recs.length;
        state.records = [];
        unqueue(state);
        return recs;
    });

    define(global, 'MutationObserver', MutationObserver);
    define(global, 'MutationRecord', MutationRecord);

    // フックの登録は公開の後。登録後に変更が起きれば observers の状態へ流れる。
    fnCall(handoffSetSink, handoff, onMutation);
})(globalThis);
