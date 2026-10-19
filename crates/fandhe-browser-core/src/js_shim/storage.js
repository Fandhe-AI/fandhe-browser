// js_shim/storage.js: `Storage` / `localStorage` / `sessionStorage` の in-memory 最小 shim
// （TASK-108・Issue #1167・ビヘイビア JS-5・JS-6・SEC-2・REPAIR-3。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle が起動時に読む `localStorage.getItem(...)` / `sessionStorage.getItem(...)` が
// `undefined` への参照で TypeError にならないようにする（d2 の module 評価で観測。REPORT.md の #1167 節）。
// 保存はこの shim を install した 1 回のページ実行の中だけ（in-memory）。プロファイルへの永続化も、
// ページ・実行・オリジン間の共有もしない（PROF 系の境界。永続化の設計は別 issue）。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の末尾。abort.js の後に評価）。
// install 時に op は呼ばない（bridge には依存しない純 JS）。
//
// 設計上の要点:
// - 内部状態（キー配列・値の Map・合計長）はクロージャ内の WeakMap に置き、ページから見えるプロパティにしない。
//   値は捕捉した Map に置くので、`'__proto__'` も普通のキーとして扱える。組み込み関数は install 時に捕捉し、
//   ページが後から差し替えても影響を受けない。
// - `localStorage` / `sessionStorage` は install のたびに新しく作る独立した 2 つのインスタンスを返す getter
//   （setter なし）。install し直すとストアは空に戻る。
// - キーと値は WebIDL の DOMString に合わせて文字列化する（Symbol は TypeError）。引数不足は TypeError。
//   エラー文は固定文で、キーと値を反響しない。
// - 上限（JS-6。1 ストアごと。主要ブラウザで一般的な値で、本実装に固有の値にしない）: 全キー長と全値長の合計
//   5 MiB（UTF-16 code unit）、件数 65536。検査は置き換え後の値で確保の前に行い、超えたら
//   `QuotaExceededError`（DOMException・code 22）を投げて状態を変えない。
// - `key(n)` は O(1)（プロトタイプ null のキー配列を持つ。件数は Map.size を真実源にする）。削除時の詰め直しは O(n)。
// - 識別面を増やさない（SEC-2）: 残量・クォータを示すプロパティ、`navigator.storage`、`StorageEvent` は定義しない。
//   クォータは書き込みを試せば推測できる（緩和策は「明示的に公開しない」ことまで）。
// - ES2015 の範囲に留める（V8 と boa で同一のソース。class / Proxy は使わない）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - `storage` イベントの配送と `StorageEvent`・`onstorage`。将来仕様: 同一オリジンの他文書への配送（JS-5）。
// - プロパティ形式のアクセス（`localStorage.foo = 'x'`・`localStorage['foo']`・`Object.keys(localStorage)`）。
//   Proxy を使わない方針のため。将来仕様: named property の対応（JS-5）。
// - プロファイルへの永続化とオリジンごとの分離（実行ごとに空から始まる）。将来仕様: PROF 系で別 issue で設計する。
// - opaque origin（`about:blank`・`data:`）での `SecurityError`。
// - キーの列挙順は挿入順の簡略化（ブラウザでは実装依存）。
(function (global) {
    'use strict';

    var NativeDOMException = global.DOMException;
    if (typeof NativeDOMException !== 'function') {
        throw new TypeError('abort.js must be evaluated before storage.js');
    }
    var NativeTypeError = TypeError;
    var defineProperty = Object.defineProperty;
    var createObject = Object.create;
    var strCtor = String;
    var WeakMapCtor = WeakMap;
    var MapCtor = Map;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var mapGet = Function.prototype.call.bind(Map.prototype.get);
    var mapSet = Function.prototype.call.bind(Map.prototype.set);
    var mapHas = Function.prototype.call.bind(Map.prototype.has);
    var mapDelete = Function.prototype.call.bind(Map.prototype.delete);
    var mapClear = Function.prototype.call.bind(Map.prototype.clear);
    var mapSize = Function.prototype.call.bind(Object.getOwnPropertyDescriptor(Map.prototype, 'size').get);

    var MAX_TOTAL = 5 * 1024 * 1024;
    var MAX_ENTRIES = 65536;

    var states = new WeakMapCtor();

    function defineMethod(target, name, fn) {
        defineProperty(target, name, { value: fn, writable: true, configurable: true, enumerable: false });
    }

    function Storage() {
        throw new NativeTypeError('Illegal constructor');
    }
    defineProperty(Storage.prototype, Symbol.toStringTag, {
        value: 'Storage',
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

    function toDomString(v) {
        if (typeof v === 'symbol') {
            throw new NativeTypeError('Cannot convert a Symbol value to a string');
        }
        return strCtor(v);
    }

    function need(args, n, name) {
        if (args.length < n) {
            throw new NativeTypeError("Failed to execute '" + name + "' on 'Storage': " + n + ' argument(s) required');
        }
    }

    function create() {
        var inst = createObject(Storage.prototype);
        // キー列は配列でなくプロトタイプ null のオブジェクト（添字 → キー）で持ち、件数は Map.size を真実源にする。
        // Array.prototype の数値プロパティ setter や Symbol.species をページが差し込んでも、内部状態の取得・
        // length 改ざんができない（件数上限の迂回を防ぐ。JS-6）。
        wmSet(states, inst, { keys: createObject(null), map: new MapCtor(), total: 0 });
        return inst;
    }

    defineMethod(Storage.prototype, 'getItem', function getItem(key) {
        var st = stateOf(this);
        need(arguments, 1, 'getItem');
        var k = toDomString(key);
        return mapHas(st.map, k) ? mapGet(st.map, k) : null;
    });

    defineMethod(Storage.prototype, 'setItem', function setItem(key, value) {
        var st = stateOf(this);
        need(arguments, 2, 'setItem');
        var k = toDomString(key);
        var v = toDomString(value);
        var exists = mapHas(st.map, k);
        var next = st.total + v.length;
        if (exists) {
            next -= mapGet(st.map, k).length;
        } else {
            next += k.length;
        }
        if (next > MAX_TOTAL || (!exists && mapSize(st.map) >= MAX_ENTRIES)) {
            throw new NativeDOMException(
                "Failed to execute 'setItem' on 'Storage': the quota has been exceeded.",
                'QuotaExceededError'
            );
        }
        if (!exists) {
            st.keys[mapSize(st.map)] = k;
        }
        mapSet(st.map, k, v);
        st.total = next;
    });

    defineMethod(Storage.prototype, 'removeItem', function removeItem(key) {
        var st = stateOf(this);
        need(arguments, 1, 'removeItem');
        var k = toDomString(key);
        if (mapHas(st.map, k)) {
            st.total -= k.length + mapGet(st.map, k).length;
            mapDelete(st.map, k);
            // 自前の詰め直し（splice はプロトタイプ・species に依存するため使わない）。
            // Map からの削除前の件数が n（削除後は n - 1）。
            var keys = st.keys;
            var n = mapSize(st.map) + 1;
            var at = 0;
            while (at < n && keys[at] !== k) {
                at++;
            }
            for (; at + 1 < n; at++) {
                keys[at] = keys[at + 1];
            }
            delete keys[n - 1];
        }
    });

    defineMethod(Storage.prototype, 'clear', function clear() {
        var st = stateOf(this);
        mapClear(st.map);
        st.keys = createObject(null);
        st.total = 0;
    });

    defineMethod(Storage.prototype, 'key', function key(index) {
        var st = stateOf(this);
        need(arguments, 1, 'key');
        // 単項 + は ToNumber 相当で BigInt を TypeError にする（WebIDL unsigned long 変換）。グローバル Number の差し替えに依存しない。
        var i = +index >>> 0;
        return i < mapSize(st.map) ? st.keys[i] : null;
    });

    defineProperty(Storage.prototype, 'length', {
        get: function () {
            return mapSize(stateOf(this).map);
        },
        configurable: true,
        enumerable: false
    });

    defineProperty(global, 'Storage', { value: Storage, writable: true, configurable: true, enumerable: false });

    function defineStore(name) {
        var inst = create();
        defineProperty(global, name, {
            get: function () {
                return inst;
            },
            configurable: true,
            enumerable: false
        });
    }
    defineStore('localStorage');
    defineStore('sessionStorage');
})(globalThis);
