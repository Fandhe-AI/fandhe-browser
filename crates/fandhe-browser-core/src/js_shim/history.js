// js_shim/history.js: `History` / `history` の最小 shim
// （TASK-108・Issue #1171・ビヘイビア JS-5・JS-6・SEC-2・REPAIR-3。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle（Backtrace SDK 等）が起動時に `history.pushState` を包む処理で、`history` が
// 未定義のため ReferenceError にならないようにする（d2 の module 評価で観測。REPORT.md の #1171 節）。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の末尾。abort.js の後に評価）。
// URL の実体は core 側（dom_bridge.rs）にあり、この shim は `pushState` / `replaceState` のたびに
// bridge op `historySetUrl` を呼ぶ。同一オリジンの検査は op の中（Rust 側）で行うので、
// ページが `__dom.op` を直接呼んでも現在のオリジンの外へは書き換えられない。
//
// 設計上の要点:
// - 内部状態（length・state・scrollRestoration）はクロージャに置き、ページから見えるプロパティにしない。
//   getter・メソッドは自分のインスタンスかを確認し、違えば TypeError。組み込み関数は install 時に捕捉する。
// - pushState / replaceState: 失敗時に状態を変えない順序（直列化と上限検査 → URL op → 反映）。
//   別オリジン等は SecurityError（DOMException）。URL の省略 / null は state（と length）だけ更新する。
//   メソッドは writable・configurable で、`history.pushState = wrapper` の包み直しができる。
// - state は構造化複製の代わりに JSON の往復で複製し、直列化後 2 MiB（UTF-16 code unit）を上限にする
//   （JS-6。無制限確保の防止）。超過・直列化失敗は DataCloneError。保持するのは現在の 1 件だけで、
//   `history.state` は毎回同じオブジェクトを返す。
// - length は 1 から始まり pushState のたびに増える。上限 50（一般的な値で本実装に固有にしない）で止め、
//   上限後の pushState も成功する。replaceState では増やさない。
// - 識別面を増やさない（SEC-2）: navigation API・PopStateEvent・onpopstate・残量や上限を示すプロパティは定義しない。
// - エラー文は固定文で入力（URL・state）を反響しない。ES2015 の範囲に留める（class / Proxy は使わない）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - back / forward / go によるページ内履歴の移動と popstate の配送。URL と state は動かさず、
//   `ignoreLocationChange` op で診断に記録するだけ（SPA のルーターの状態と URL を食い違わせないため）。
//   将来仕様: 汎用の EventTarget / dispatchEvent を足したうえで履歴の範囲で移動する（JS-5）。
// - 構造化複製の完全な実装。JSON の往復のため Date・Map・Blob 等は保持されず、undefined / 関数の
//   プロパティは落ち、toJSON が呼ばれる。
// - about:blank（opaque origin）での URL 付き pushState / replaceState は常に SecurityError。
// - hashchange の配送、履歴のプロファイルへの永続化（実行ごとに空から始まる）。
(function (global) {
    'use strict';

    var NativeDOMException = global.DOMException;
    if (typeof NativeDOMException !== 'function') {
        throw new TypeError('abort.js must be evaluated before history.js');
    }
    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var callOp = Function.prototype.call.bind(bridge.op, bridge);
    var NativeTypeError = TypeError;
    var defineProperty = Object.defineProperty;
    var strCtor = String;
    var jsonStringify = JSON.stringify;
    var jsonParse = JSON.parse;
    var WeakMapCtor = WeakMap;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);

    var MAX_STATE_UNITS = 2 * 1024 * 1024;
    var MAX_LENGTH = 50;

    var states = new WeakMapCtor();

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

    // state を JSON の往復で複製する。undefined / null はそのまま保持する。失敗は DataCloneError。
    function cloneState(data, name) {
        if (data === undefined || data === null) {
            return data;
        }
        var text;
        try {
            text = jsonStringify(data);
        } catch (e) {
            text = undefined;
        }
        if (typeof text !== 'string' || text.length > MAX_STATE_UNITS) {
            throw new NativeDOMException(
                "Failed to execute '" + name + "' on 'History': the state could not be cloned.",
                'DataCloneError'
            );
        }
        return jsonParse(text);
    }

    function update(self, args, name, isPush) {
        var st = stateOf(self);
        need(args, 2, name);
        var copy = cloneState(args[0], name);
        var url = args.length > 2 ? args[2] : undefined;
        if (url !== undefined && url !== null) {
            if (typeof url === 'symbol') {
                throw new NativeTypeError('Cannot convert a Symbol value to a string');
            }
            if (callOp('historySetUrl', strCtor(url)) !== true) {
                throw new NativeDOMException(
                    "Failed to execute '" + name + "' on 'History': the URL cannot be used.",
                    'SecurityError'
                );
            }
        }
        st.state = copy;
        if (isPush && st.length < MAX_LENGTH) {
            st.length++;
        }
    }

    defineMethod(History.prototype, 'pushState', function pushState(data, unused, url) {
        update(this, arguments, 'pushState', true);
    });

    defineMethod(History.prototype, 'replaceState', function replaceState(data, unused, url) {
        update(this, arguments, 'replaceState', false);
    });

    // back / forward / go は未実装（REPAIR-3）。遷移せず診断に記録するだけ。
    defineMethod(History.prototype, 'back', function back() {
        stateOf(this);
        callOp('ignoreLocationChange', 'historyBack', '');
    });

    defineMethod(History.prototype, 'forward', function forward() {
        stateOf(this);
        callOp('ignoreLocationChange', 'historyForward', '');
    });

    defineMethod(History.prototype, 'go', function go(delta) {
        stateOf(this);
        // 単項 + は ToNumber 相当で BigInt・Symbol を TypeError にする。| 0 で WebIDL long へ丸める。
        var d = (+delta) | 0;
        callOp('ignoreLocationChange', 'historyGo', strCtor(d));
    });

    defineProperty(History.prototype, 'length', {
        get: function () {
            return stateOf(this).length;
        },
        configurable: true,
        enumerable: true
    });

    defineProperty(History.prototype, 'state', {
        get: function () {
            return stateOf(this).state;
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
    wmSet(states, inst, { length: 1, state: null, scrollRestoration: 'auto' });
    defineProperty(global, 'history', {
        get: function () {
            return inst;
        },
        configurable: true,
        enumerable: false
    });
})(globalThis);
