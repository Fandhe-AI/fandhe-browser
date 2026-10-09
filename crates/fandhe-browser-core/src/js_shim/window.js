// js_shim/window.js: ページ内 JS に見せる window / self / location / navigator / console の shim
// （TASK-108・Issue #779・ビヘイビア JS-5・SEC-2。SSOT: js-engine.md「ページ内 JS 実行の設計制約」決定 3）。
//
// 役割: ページの bundle が参照する最小のグローバルを揃える。状態（URL・UA・診断）の実体は
// すべて core 側（dom_bridge.rs）にあり、この shim は値をキャッシュせず毎回
// `__dom.op(opName, ...args)` で読む。ページ JS が shim のオブジェクトや global を書き換えても
// 真の値は変わらない。
// 呼び出し元: core の `js_shim::install`（`JsRuntime::install_dom_shim` 経由）が dom.js の後に評価する。
//
// 設計上の要点:
// - location: 読み取りは getLocation。ページ JS が変えられるのは hash だけ（setLocationHash）。
//   href 等の代入・assign / replace / reload は実行せず ignoreLocationChange で診断に記録する
//   （遷移も取得も行わない）。
// - navigator: 自動化ブラウザ fandhe-browser であることを正直に示す（SEC-2）。userAgent は
//   Fetcher と同じ単一の定数（navigatorUserAgent op）、webdriver は true 固定。UA・webdriver・
//   window.chrome 等で実ブラウザを装わない・検出回避の目的で操作しない。
// - console: 出力は持ち越さない。記録対象は log/info/warn/error/debug のみで、ページ由来の文字列は
//   4097 UTF-16 単位（上限超過を Rust 側で検知させる 1 単位込み）で打ち切って consoleMessage op へ渡す（Rust 側でさらに 4 KiB・文字境界で
//   切り詰める）。op 予算を使い切らせないよう転送は 64 回まで。決して throw しない。
// - 再 install（ページごと）に耐えるよう、global への定義は configurable にする。
// - ES2015 の範囲に留める（V8 と boa で同一のソースを使うため。Proxy は使わない）。
//
// 未実装（REPAIR-3）:
// - hashchange / popstate 等のイベント、window.addEventListener（イベント系は後続 issue）。
// - document.location / document.URL、location 代入による実際の遷移。
// - navigator の追加プロパティ（language / platform / plugins 等。事実と異なる値や
//   フィンガープリント面を作らないため入れていない。必要性は後続 issue で判断する）。
// - 実ブラウザの [LegacyUnforgeable]（location / window の再定義不可）との差異。真の状態は
//   Rust 側にあるため安全性は変わらない。DOMException への写像も未対応。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var nativeOp = bridge.op;

    function op() {
        return nativeOp.apply(bridge, arguments);
    }

    // ページ JS から `new Location()` 等で作らせないための合言葉（クロージャ内のみ）。
    var TOKEN = {};

    function Location(token) {
        if (token !== TOKEN) {
            throw new TypeError('Illegal constructor');
        }
    }

    function Navigator(token) {
        if (token !== TOKEN) {
            throw new TypeError('Illegal constructor');
        }
    }

    function defineAccessor(target, name, getter, setter) {
        Object.defineProperty(target, name, {
            get: getter,
            set: setter,
            configurable: true,
            enumerable: true
        });
    }

    function defineMethod(target, name, fn) {
        Object.defineProperty(target, name, {
            value: fn,
            writable: true,
            configurable: true,
            enumerable: true
        });
    }

    // 遷移系の代入は実行せず記録する（値の本文は Rust 側でも保存せず長さだけ残す）。
    function ignoreChange(kind, value) {
        op('ignoreLocationChange', kind, String(value));
    }

    ['href', 'protocol', 'host', 'hostname', 'port', 'pathname', 'search', 'origin'].forEach(
        function (name) {
            defineAccessor(
                Location.prototype,
                name,
                function () {
                    return op('getLocation', name);
                },
                function (value) {
                    ignoreChange(name, value);
                }
            );
        }
    );
    defineAccessor(
        Location.prototype,
        'hash',
        function () {
            return op('getLocation', 'hash');
        },
        function (value) {
            op('setLocationHash', String(value));
        }
    );
    defineMethod(Location.prototype, 'assign', function assign(url) {
        ignoreChange('assign', url);
    });
    defineMethod(Location.prototype, 'replace', function replace(url) {
        ignoreChange('replace', url);
    });
    defineMethod(Location.prototype, 'reload', function reload() {
        ignoreChange('reload', '');
    });
    defineMethod(Location.prototype, 'toString', function toString() {
        return op('getLocation', 'href');
    });

    // Navigator: 正直な最小実装。getter のみ（setter なし）で、代入しても値は変わらない。
    defineAccessor(
        Navigator.prototype,
        'userAgent',
        function () {
            return op('navigatorUserAgent');
        },
        undefined
    );
    defineAccessor(
        Navigator.prototype,
        'webdriver',
        function () {
            return true;
        },
        undefined
    );

    // console: 記録は log/info/warn/error/debug のみ。引数の文字列化や転送で throw しない。
    // Rust 側の上限（4096 バイト）を超えたことが分かるよう、1 単位余分に残す。
    var MAX_TEXT_UNITS = 4097;
    var MAX_FORWARDED = 64;
    var forwarded = 0;

    function send(level, args) {
        try {
            if (forwarded >= MAX_FORWARDED) {
                return;
            }
            var text = '';
            for (var i = 0; i < args.length && text.length < MAX_TEXT_UNITS; i++) {
                var piece;
                try {
                    piece = String(args[i]);
                } catch (e) {
                    piece = '[unprintable]';
                }
                text += (i > 0 ? ' ' : '') + piece;
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
            forwarded += 1;
            op('consoleMessage', level, text);
        } catch (e) {
            // console はページを壊さない。
        }
    }

    var consoleObject = {};
    function recorder(level) {
        return function () {
            send(level, arguments);
        };
    }
    function noop() {}

    defineMethod(consoleObject, 'log', recorder('log'));
    defineMethod(consoleObject, 'info', recorder('info'));
    defineMethod(consoleObject, 'warn', recorder('warn'));
    defineMethod(consoleObject, 'error', recorder('error'));
    defineMethod(consoleObject, 'debug', recorder('debug'));
    defineMethod(consoleObject, 'trace', recorder('debug'));
    defineMethod(consoleObject, 'dir', recorder('log'));
    defineMethod(consoleObject, 'table', recorder('log'));
    defineMethod(consoleObject, 'assert', function (condition) {
        if (!condition) {
            send('error', Array.prototype.slice.call(arguments, 1));
        }
    });
    [
        'group', 'groupCollapsed', 'groupEnd', 'time', 'timeEnd', 'timeLog',
        'count', 'countReset', 'clear'
    ].forEach(function (name) {
        defineMethod(consoleObject, name, noop);
    });

    var locationObject = new Location(TOKEN);
    var navigatorObject = new Navigator(TOKEN);

    function expose(name, value) {
        Object.defineProperty(global, name, {
            value: value,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    expose('Location', Location);
    expose('Navigator', Navigator);
    expose('window', global);
    expose('self', global);
    expose('navigator', navigatorObject);
    expose('console', consoleObject);
    Object.defineProperty(global, 'location', {
        get: function () {
            return locationObject;
        },
        set: function (value) {
            ignoreChange('assignLocation', value);
        },
        configurable: true,
        enumerable: false
    });
})(globalThis);
