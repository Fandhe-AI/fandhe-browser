// js_shim/history.js: `History` / `history` の最小 shim
// （TASK-108・Issue #1171・ビヘイビア JS-5・JS-6・SEC-2・REPAIR-3。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle（Backtrace SDK 等）が起動時に `history.pushState` を包む処理で、`history` が
// 未定義のため ReferenceError にならないようにする（d2 の module 評価で観測。REPORT.md の #1171 節）。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の末尾。abort.js・events.js の後に評価）。
// URL の実体は core 側（dom_bridge.rs）にあり、この shim は `pushState` / `replaceState` のたびに
// bridge op `historySetUrl` を呼ぶ。同一オリジンの検査は op の中（Rust 側）で行うので、
// ページが `__dom.op` を直接呼んでも現在のオリジンの外へは書き換えられない。
//
// 設計上の要点:
// - 内部状態（履歴エントリ・現在位置・scrollRestoration）はクロージャに置き、ページから見えるプロパティにしない。
//   getter・メソッドは自分のインスタンスかを確認し、違えば TypeError。組み込み関数は install 時に捕捉する。
// - 履歴エントリ（#1177）: 同一ドキュメント内の履歴を {url, state} の列と現在位置で持つ（null プロトタイプの
//   コンテナ＋件数。配列メソッドは使わない。JS-4）。初期エントリの URL は未確定（null）で、最初の pushState /
//   移動のときに `getLocation('href')` で確定する（`location.hash` 代入で変わった URL も取り込む）。
//   pushState は前方のエントリを捨てて末尾に足し、`length` は件数。上限 50（一般的な値で本実装に固有にしない）を
//   超えたら最古を捨てる。replaceState は現在のエントリを置き換える。
// - back / forward / go(delta): 取得・遷移はせず、履歴の範囲で URL と state を動かして `popstate` を window へ配る
//   （JS-5）。実ブラウザは task で処理し呼び出し直後の location は変わらないため、install 時に捕捉した
//   `queueMicrotask` で非同期に実行する（近似）。移動の URL 書き換えは `historySetUrl` op（同一オリジンの検査は
//   Rust 側）。範囲外は何もしない。`go(0)`（再読み込み）は実行せず `ignoreLocationChange` に記録する。
//   キュー登録・URL 書き換えに失敗した移動も `ignoreLocationChange` に記録し、黙って成功扱いにしない。
// - pushState / replaceState: 失敗時に状態を変えない順序（直列化と上限検査 → URL op → 反映）。
//   別オリジン・長すぎる URL（8192 UTF-16 unit 超。bridge を呼ぶ前に判定。#1175）等は SecurityError（DOMException）。URL の省略 / null は state（と length）だけ更新する。
//   メソッドは writable・configurable で、`history.pushState = wrapper` の包み直しができる。
// - state は構造化複製の代わりに予算付きの再帰複製（配列は添字で defineProperty、オブジェクトは for...in を
//   逐次処理。ページが書き換え可能な Array.prototype.push・Object.keys は使わない）で複製し、1 件あたり累計 2 MiB
//   （UTF-16 code unit）・10 万ノード・深さ 100 を確保前に検査して上限にする（JS-6。共有参照による指数展開も打ち切る）。
//   超過・複製不能は DataCloneError。加えて保持する全エントリの合計を 8 MiB・40 万ノードまでとし、超えたら現在の
//   エントリ以外を古い順に捨てる（JS-6）。`history.state` は毎回同じオブジェクトを返し、popstate の `event.state` と同一。
// - `PopStateEvent` / `window.onpopstate`（events.js）は主要ブラウザすべてにある標準 API で、UA・navigator 等の
//   識別面を変えず、実装を伴わない機能検出の偽装にもならない（SEC-2 の確認結果。#1177）。`navigation` API や
//   残量・上限を示すプロパティは定義しない。ページが生成したイベントの `isTrusted` は false（信頼済みを装わない）。
// - エラー文は固定文で入力（URL・state）を反響しない。ES2015 の範囲に留める（class / Proxy は使わない）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - 移動を task 単位で処理するタイミングと信頼済みの popstate（`isTrusted` が true）。hashchange の配送。
//   `PopStateEvent.hasUAVisualTransition`。
// - 構造化複製の完全な実装。Date・Map・Blob 等は保持されず（own enumerable プロパティだけの素の
//   オブジェクトになる）、関数・symbol・bigint は DataCloneError。循環参照は深さ上限で DataCloneError。
// - about:blank（opaque origin）での URL 付き pushState / replaceState は常に SecurityError。
// - 履歴のプロファイルへの永続化（実行ごとに空から始まる）。scroll restoration の実際の挙動。
(function (global) {
    'use strict';

    var NativeDOMException = global.DOMException;
    if (typeof NativeDOMException !== 'function') {
        throw new TypeError('abort.js must be evaluated before history.js');
    }
    var NativeEvent = global.Event;
    var nativeDispatchEvent = global.dispatchEvent;
    var nativeQueueMicrotask = global.queueMicrotask;
    // timers.js の登録成否を返す内部窓口（install 時に捕捉。無ければ公開の queueMicrotask で成否不明のまま積む）。
    var nativeTryQueue = typeof nativeQueueMicrotask === 'function' ? nativeQueueMicrotask.__fandheTry : undefined;
    if (typeof NativeEvent !== 'function' || typeof nativeDispatchEvent !== 'function') {
        throw new TypeError('events.js must be evaluated before history.js');
    }
    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var callOp = Function.prototype.call.bind(bridge.op, bridge);
    var NativeTypeError = TypeError;
    var defineProperty = Object.defineProperty;
    var strCtor = String;
    var isArray = Array.isArray;
    var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    var WeakMapCtor = WeakMap;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var wmHas = Function.prototype.call.bind(WeakMap.prototype.has);

    var MAX_STATE_UNITS = 2 * 1024 * 1024;
    var MAX_STATE_NODES = 100000;
    var MAX_STATE_DEPTH = 100;
    var MAX_LENGTH = 50;
    // 保持する全エントリの state の合計の上限（JS-6）。
    var MAX_TOTAL_UNITS = 8 * 1024 * 1024;
    var MAX_TOTAL_NODES = 400000;
    var createObject = Object.create;
    var fnCall = Function.prototype.call.bind(Function.prototype.call);
    var setPrototypeOf = Object.setPrototypeOf;
    var symTag = Symbol.toStringTag;
    // URL の長さ上限（UTF-16 code unit）。Rust 側 page_env.rs の `MAX_LOCATION_URL_BYTES`（UTF-8 で 8192 バイト）と同値。
    // 1 unit は UTF-8 で 1 バイト以上なので、これを超える文字列は Rust 側でも必ず拒否される（受理される URL は落とさない）。
    // 8192 unit 以下なら UTF-8 でも最大 49152 バイトで bridge の文字列上限（64 KiB）に届かない。
    // bridge 到達前に判定し、入力サイズによらず例外を SecurityError に揃える（#1175）。
    var MAX_URL_UNITS = 8192;

    var states = new WeakMapCtor();
    var popStates = new WeakMapCtor();

    function defineMethod(target, name, fn) {
        defineProperty(target, name, { value: fn, writable: true, configurable: true, enumerable: false });
    }

    function History() {
        throw new NativeTypeError('Illegal constructor');
    }
    defineProperty(History.prototype, Symbol.toStringTag, {
        value: 'History',
        configurable: true,
        writable: false,
        enumerable: false
    });

    function stateOf(self) {
        var st = (self !== null && typeof self === 'object') ? wmGet(states, self) : undefined;
        if (st === undefined) {
            throw new NativeTypeError('Illegal invocation');
        }
        return st;
    }

    function need(args, n, name) {
        if (args.length < n) {
            throw new NativeTypeError("Failed to execute '" + name + "' on 'History': " + n + ' argument(s) required');
        }
    }

    // state を予算付きの再帰複製で複製する（独自の簡易な構造化複製）。
    // 確保前に走査件数・深さ・累計サイズ（UTF-16 code unit。数値・真偽値は固定 8）を検査し、超えたら
    // 即座に打ち切る。共有参照や循環による指数的な展開・無限再帰も、件数・深さの上限で止まる。
    // 関数・symbol・bigint は複製できず、失敗はすべて DataCloneError。
    function cloneState(data, name) {
        var budget = { nodes: 0, units: 0, value: data };
        if (data === undefined || data === null) {
            return budget;
        }
        try {
            budget.value = cloneValue(data, 0, budget);
            return budget;
        } catch (e) {
            // 予算超過・複製不能・getter の例外・スタック枯渇はすべて固定文の DataCloneError にする。
            throw new NativeDOMException(
                "Failed to execute '" + name + "' on 'History': the state could not be cloned.",
                'DataCloneError'
            );
        }
    }

    function cloneValue(v, depth, budget) {
        var t = typeof v;
        // 値の訪問ごと（null・undefined・空文字列・疎配列の穴を含む）に 1 ノードを累積予算へ計上する。
        budget.nodes++;
        if (budget.nodes > MAX_STATE_NODES) {
            throw 0;
        }
        if (v === null || t === 'undefined') {
            return v;
        }
        if (t === 'string') {
            budget.units += v.length;
            if (budget.units > MAX_STATE_UNITS) {
                throw 0;
            }
            return v;
        }
        if (t === 'number' || t === 'boolean') {
            budget.units += 8;
            if (budget.units > MAX_STATE_UNITS) {
                throw 0;
            }
            return v;
        }
        if (t !== 'object' || depth >= MAX_STATE_DEPTH) {
            throw 0;
        }
        var i;
        var out;
        if (isArray(v)) {
            var n = v.length;
            // 配列の長さは確保前に件数予算と照合する（疎な巨大配列で確保しない）。
            if (n > MAX_STATE_NODES - budget.nodes) {
                throw 0;
            }
            out = [];
            for (i = 0; i < n; i++) {
                // ページが書き換え可能な Array.prototype.push・添字 setter を経由しないよう defineProperty で置く。
                defineProperty(out, i, {
                    value: cloneValue(v[i], depth + 1, budget),
                    writable: true,
                    configurable: true,
                    enumerable: true
                });
            }
            return out;
        }
        out = {};
        // 全キーの配列を作らず for...in で逐次処理し、列挙したキーごと（継承分を含む）に件数予算へ計上して
        // 超えたら確保前に打ち切る（大量プロパティのオブジェクトで巨大な一覧を作らない）。
        for (var key in v) {
            budget.nodes++;
            if (budget.nodes > MAX_STATE_NODES) {
                throw 0;
            }
            if (!hasOwn(v, key)) {
                continue;
            }
            budget.units += key.length;
            if (budget.units > MAX_STATE_UNITS) {
                throw 0;
            }
            defineProperty(out, key, {
                value: cloneValue(v[key], depth + 1, budget),
                writable: true,
                configurable: true,
                enumerable: true
            });
        }
        return out;
    }

    function currentHref() {
        try {
            var h = callOp('getLocation', 'href');
            return typeof h === 'string' ? h : null;
        } catch (e) {
            return null;
        }
    }

    // 保存状態から独立した公開用コピーを作る（history.state・popstate 用。可変オブジェクトの共有を防ぐ）。
    // 保存状態は検証済みの複製なので予算内に収まるが、失敗時は DataCloneError を投げる。
    function publicState(stored, name) {
        return cloneState(stored, name).value;
    }

    function newEntry(url, copy, pub) {
        return { url: url, state: copy.value, pub: pub, units: copy.units, nodes: copy.nodes };
    }

    // 件数上限と累計予算を満たすまで、現在のエントリ以外を古い順（なければ末尾側）に捨てる（JS-6）。
    function enforceLimits(st) {
        for (;;) {
            var units = 0;
            var nodes = 0;
            for (var i = 0; i < st.count; i++) {
                units += st.entries[i].units;
                nodes += st.entries[i].nodes;
            }
            var over = st.count > MAX_LENGTH || units > MAX_TOTAL_UNITS || nodes > MAX_TOTAL_NODES;
            if (!over) {
                return;
            }
            if (st.index > 0) {
                for (var j = 0; j < st.count - 1; j++) {
                    st.entries[j] = st.entries[j + 1];
                }
                delete st.entries[st.count - 1];
                st.count--;
                st.index--;
            } else if (st.count - 1 > st.index) {
                delete st.entries[st.count - 1];
                st.count--;
            } else {
                return;
            }
        }
    }

    function update(self, args, name, isPush) {
        var st = stateOf(self);
        need(args, 2, name);
        var copy = cloneState(args[0], name);
        var pubCopy = publicState(copy.value, name);
        var url = args.length > 2 ? args[2] : undefined;
        // 書き換え前の URL（現在のエントリの URL を確定するため）。url の文字列変換は
        // ページのコード（toString）を実行し pushState を再入させ得るため、変換後に URL op の直前で取得する。
        var prevHref = null;
        var urlChanged = false;
        if (url !== undefined && url !== null) {
            if (typeof url === 'symbol') {
                throw new NativeTypeError('Cannot convert a Symbol value to a string');
            }
            var urlText = strCtor(url);
            prevHref = currentHref();
            // 空文字列は現在の URL を保持する（解決するとフラグメントが落ちるため op を呼ばない）。
            // 長すぎる URL は bridge を呼ばずに拒否する（bridge の StringTooLong を見せない。#1175）。
            if (urlText.length > MAX_URL_UNITS || (urlText !== '' && callOp('historySetUrl', urlText) !== true)) {
                throw new NativeDOMException(
                    "Failed to execute '" + name + "' on 'History': the URL cannot be used.",
                    'SecurityError'
                );
            }
            urlChanged = urlText !== '';
        }
        if (url === undefined || url === null) {
            prevHref = currentHref();
        }
        var newHref = urlChanged ? currentHref() : prevHref;
        var cur = st.entries[st.index];
        if (isPush) {
            if (prevHref !== null) {
                cur.url = prevHref;
            }
            // 前方のエントリを捨てて末尾に足す。
            for (var i = st.index + 1; i < st.count; i++) {
                delete st.entries[i];
            }
            st.count = st.index + 1;
            st.entries[st.count] = newEntry(newHref !== null ? newHref : cur.url, copy, pubCopy);
            st.count++;
            st.index = st.count - 1;
        } else {
            cur.state = copy.value;
            cur.pub = pubCopy;
            cur.units = copy.units;
            cur.nodes = copy.nodes;
            if (urlChanged && newHref !== null) {
                cur.url = newHref;
            }
        }
        enforceLimits(st);
    }

    defineMethod(History.prototype, 'pushState', function pushState(data, unused, url) {
        update(this, arguments, 'pushState', true);
    });

    defineMethod(History.prototype, 'replaceState', function replaceState(data, unused, url) {
        update(this, arguments, 'replaceState', false);
    });

    // 移動の実体（microtask から呼ぶ）。範囲外は何もしない。URL の書き換えに失敗したら移動せず診断に記録する。
    function traverse(st, delta, kind, value) {
        var target = st.index + delta;
        if (target < 0 || target >= st.count) {
            return;
        }
        var cur = st.entries[st.index];
        var dest = st.entries[target];
        var ok = true;
        var pub;
        try {
            // 状態の準備に成功してから URL と index を確定する。
            pub = publicState(dest.state, 'traverse');
            var here = currentHref();
            if (here !== null) {
                cur.url = here;
            }
            if (dest.url !== null && dest.url !== cur.url) {
                ok = callOp('historySetUrl', dest.url) === true;
            }
        } catch (e) {
            ok = false;
        }
        if (!ok) {
            callOp('ignoreLocationChange', kind, value);
            return;
        }
        st.index = target;
        // 移動先の公開 state は保存状態から作り直す（以前に公開した可変オブジェクトの変更を持ち込まない）。
        dest.pub = pub;
        try {
            var ev = new PopStateEvent('popstate', { state: dest.pub });
            // 公開コンストラクターは undefined を null に写すが、traversal では history.state と一致させる。
            wmSet(popStates, ev, dest.pub);
            fnCall(nativeDispatchEvent, global, ev);
        } catch (e) {
            // 配送の失敗は移動の結果に影響させない（リスナー例外は dispatchEvent が報告する）。
        }
    }

    // 移動を非同期に積む。積めなかったときは移動せず、診断に記録する（黙って成功扱いにしない）。
    function schedule(st, delta, kind, value) {
        var queued = false;
        var task = function () {
            traverse(st, delta, kind, value);
        };
        try {
            if (typeof nativeTryQueue === 'function') {
                queued = nativeTryQueue(task) === true;
            } else if (typeof nativeQueueMicrotask === 'function') {
                nativeQueueMicrotask(task);
                queued = true;
            }
        } catch (e) {
            queued = false;
        }
        if (!queued) {
            callOp('ignoreLocationChange', kind, value);
        }
    }

    defineMethod(History.prototype, 'back', function back() {
        schedule(stateOf(this), -1, 'historyBack', '');
    });

    defineMethod(History.prototype, 'forward', function forward() {
        schedule(stateOf(this), 1, 'historyForward', '');
    });

    defineMethod(History.prototype, 'go', function go(delta) {
        var st = stateOf(this);
        // 単項 + は ToNumber 相当で BigInt・Symbol を TypeError にする。| 0 で WebIDL long へ丸める。
        var d = (+delta) | 0;
        if (d === 0) {
            // 再読み込みに当たるので実行せず記録する（REPAIR-3）。
            callOp('ignoreLocationChange', 'historyGo', '0');
            return;
        }
        schedule(st, d, 'historyGo', strCtor(d));
    });

    // PopStateEvent（SEC-2 の確認結果は冒頭）。Event の派生で、state は init.state をそのまま保持する。
    function PopStateEvent(type, init) {
        if (!(this instanceof PopStateEvent)) {
            throw new NativeTypeError("Failed to construct 'PopStateEvent': Please use the 'new' operator");
        }
        if (arguments.length < 1) {
            throw new NativeTypeError("Failed to construct 'PopStateEvent': 1 argument required, but only 0 present.");
        }
        fnCall(NativeEvent, this, type, init);
        var s = null;
        if (init !== undefined && init !== null && (typeof init === 'object' || typeof init === 'function')) {
            s = init.state === undefined ? null : init.state;
        }
        wmSet(popStates, this, s);
    }
    PopStateEvent.prototype = createObject(NativeEvent.prototype);
    setPrototypeOf(PopStateEvent, NativeEvent);
    defineMethod(PopStateEvent.prototype, 'constructor', PopStateEvent);
    defineProperty(PopStateEvent.prototype, 'state', {
        get: function () {
            var has = this !== null && typeof this === 'object' && wmHas(popStates, this);
            if (!has) {
                throw new NativeTypeError('Illegal invocation');
            }
            return wmGet(popStates, this);
        },
        configurable: true,
        enumerable: true
    });
    var tagDesc = createObject(null);
    tagDesc.value = 'PopStateEvent';
    tagDesc.configurable = true;
    defineProperty(PopStateEvent.prototype, symTag, tagDesc);
    defineProperty(global, 'PopStateEvent', {
        value: PopStateEvent,
        writable: true,
        configurable: true,
        enumerable: false
    });

    defineProperty(History.prototype, 'length', {
        get: function () {
            return stateOf(this).count;
        },
        configurable: true,
        enumerable: true
    });

    defineProperty(History.prototype, 'state', {
        get: function () {
            var st = stateOf(this);
            return st.entries[st.index].pub;
        },
        configurable: true,
        enumerable: true
    });

    defineProperty(History.prototype, 'scrollRestoration', {
        get: function () {
            return stateOf(this).scrollRestoration;
        },
        set: function (v) {
            var st = stateOf(this);
            // WebIDL の enum と同じく、列挙外の値は黙って無視する。
            var s = typeof v === 'symbol' ? '' : strCtor(v);
            if (s === 'auto' || s === 'manual') {
                st.scrollRestoration = s;
            }
        },
        configurable: true,
        enumerable: true
    });

    defineProperty(global, 'History', { value: History, writable: true, configurable: true, enumerable: false });

    var inst = Object.create(History.prototype);
    var initialEntries = createObject(null);
    initialEntries[0] = { url: null, state: null, pub: null, units: 0, nodes: 0 };
    wmSet(states, inst, { entries: initialEntries, count: 1, index: 0, scrollRestoration: 'auto' });
    defineProperty(global, 'history', {
        get: function () {
            return inst;
        },
        configurable: true,
        enumerable: false
    });
})(globalThis);
