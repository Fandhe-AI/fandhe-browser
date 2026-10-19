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
// - state は構造化複製の代わりに予算付きの再帰複製（配列は添字で defineProperty、オブジェクトは for...in を
//   逐次処理。ページが書き換え可能な Array.prototype.push・Object.keys は使わない）で複製し、累計 2 MiB（UTF-16 code unit）・
//   10 万ノード・深さ 100 を確保前に検査して上限にする（JS-6。共有参照による指数展開も打ち切る）。
//   超過・複製不能は DataCloneError。保持するのは現在の 1 件だけで、
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
// - 構造化複製の完全な実装。Date・Map・Blob 等は保持されず（own enumerable プロパティだけの素の
//   オブジェクトになる）、関数・symbol・bigint は DataCloneError。循環参照は深さ上限で DataCloneError。
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
    var isArray = Array.isArray;
    var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    var WeakMapCtor = WeakMap;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);

    var MAX_STATE_UNITS = 2 * 1024 * 1024;
    var MAX_STATE_NODES = 100000;
    var MAX_STATE_DEPTH = 100;
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

    // state を予算付きの再帰複製で複製する（独自の簡易な構造化複製）。
    // 確保前に走査件数・深さ・累計サイズ（UTF-16 code unit。数値・真偽値は固定 8）を検査し、超えたら
    // 即座に打ち切る。共有参照や循環による指数的な展開・無限再帰も、件数・深さの上限で止まる。
    // 関数・symbol・bigint は複製できず、失敗はすべて DataCloneError。
    function cloneState(data, name) {
        if (data === undefined || data === null) {
            return data;
        }
        var budget = { nodes: 0, units: 0 };
        try {
            return cloneValue(data, 0, budget);
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

    function update(self, args, name, isPush) {
        var st = stateOf(self);
        need(args, 2, name);
        var copy = cloneState(args[0], name);
        var url = args.length > 2 ? args[2] : undefined;
        if (url !== undefined && url !== null) {
            if (typeof url === 'symbol') {
                throw new NativeTypeError('Cannot convert a Symbol value to a string');
            }
            var urlText = strCtor(url);
            // 空文字列は現在の URL を保持する（解決するとフラグメントが落ちるため op を呼ばない）。
            if (urlText !== '' && callOp('historySetUrl', urlText) !== true) {
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
