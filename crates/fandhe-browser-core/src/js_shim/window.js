// js_shim/window.js: ページ内 JS に見せる window / self / location / document.location / document.URL / navigator / console の shim
// （TASK-108・Issue #779・#1158・ビヘイビア JS-5・SEC-2。SSOT: js-engine.md「ページ内 JS 実行の設計制約」決定 3）。
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
//   （遷移も取得も行わない）。document.location は window.location と同一のオブジェクトを返し、
//   document.URL は location.href と同じ値の読み取り専用 getter（Backtrace SDK 等が
//   document.location.port を読むため。Issue #1158）。document 側の代入も window 側と同じ
//   assignLocation 診断に記録する（両者は区別しない簡略化）。
// - navigator: 自動化ブラウザ fandhe-browser であることを正直に示す（SEC-2）。userAgent は
//   Fetcher と同じ単一の定数（navigatorUserAgent op）、webdriver は true 固定。UA・webdriver・
//   window.chrome 等で実ブラウザを装わない・検出回避の目的で操作しない。
// - console: 出力は持ち越さない。記録対象は log/info/warn/error/debug のみで、ページ由来の文字列は
//   4097 UTF-16 単位（上限超過を Rust 側で検知させる 1 単位込み）で打ち切って consoleMessage op へ渡す（Rust 側でさらに 4 KiB・文字境界で
//   切り詰める）。op 予算を使い切らせないよう転送は 64 回まで。決して throw しない。
//   error / warn に渡された Error 風オブジェクトの stack は、診断専用の opt-in（timerOriginCapture と
//   同じフラグ）が立っているときだけ 1024 UTF-16 単位まで第 3 引数で渡す。既定では `.stack` に触れない
//   （Issue #1183・SEC-2）。
// - matchMedia: 画面サイズ・色テーマ・ポインタ等、環境の実値を一切読まない固定値の最小実装
//   （#1227・SEC-2。メディアクエリを評価しないので識別面を増やさない）。`matches` は常に false、
//   `media` は渡された文字列をそのまま返す。
// - IntersectionObserver: 何も観測しない不活性な最小実装（#1227・SEC-2。ビューポートや要素の寸法を
//   読まないので識別面を増やさない）。callback は一度も呼ばず、登録も保持しない（無制限に増えない）。
// - performance.now(): install 時点からの経過ミリ秒だけを返す最小実装（#1227。timeOrigin・mark・measure・
//   getEntries 等は持たない）。
// - 再 install（ページごと）に耐えるよう、global への定義は configurable にする。
// - ES2015 の範囲に留める（V8 と boa で同一のソースを使うため。Proxy は使わない）。
//
// 未実装（REPAIR-3）:
// - hashchange 等のイベント（popstate の配送と history の移動は history.js、#1177。DOMContentLoaded / load の addEventListener は events.js、#781）。
// - location 代入による実際の遷移。
// - navigator の追加プロパティ（language / platform / plugins 等。事実と異なる値や
//   フィンガープリント面を作らないため入れていない。必要性は後続 issue で判断する）。
// - matchMedia は固定値（REPAIR-3・#1227）: 常に `matches: false`（`all` や `(min-width: 1px)` のように
//   実ブラウザで常に真になるクエリでも false）、`media` を正規化しない、`change` を配らない、
//   MediaQueryList は EventTarget を継承せず dispatchEvent も無い。将来はレンダリング層の実ビューポートと
//   連動させる（ただし SEC-2 の範囲で）。
// - IntersectionObserver は常に交差なし（REPAIR-3・#1227）: callback を呼ばず、`observe` の引数も検証しない。
//   `root` は null・`rootMargin` は '0px 0px 0px 0px'・`thresholds` は [0] の固定値で、options は無視する。
//   将来はレンダリング層のレイアウトと連動させる（ただし SEC-2 の範囲で）。
// - performance は `now()` のみ（REPAIR-3）。実時間の差分で、仮想時間とは連動しない。
// - 実ブラウザの [LegacyUnforgeable]（location / window の再定義不可。document.location は
//   document インスタンスではなく Document.prototype に置いている）との差異。真の状態は
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

    // matchMedia が返す MediaQueryList。状態（media 文字列）は WeakMap だけが持つ（#1227）。
    var mediaOf = new WeakMap();
    function MediaQueryList(token) {
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

    // MediaQueryList: 環境を評価しない固定値（SEC-2）。リスナーは保持せず呼ばない。
    function mediaBrand(self) {
        var m = mediaOf.get(self);
        if (m === undefined) {
            throw new TypeError('Illegal invocation');
        }
        return m;
    }
    defineAccessor(
        MediaQueryList.prototype,
        'media',
        function () {
            return mediaBrand(this);
        },
        undefined
    );
    defineAccessor(
        MediaQueryList.prototype,
        'matches',
        function () {
            mediaBrand(this);
            return false;
        },
        undefined
    );
    var onchangeOf = new WeakMap();
    defineAccessor(
        MediaQueryList.prototype,
        'onchange',
        function () {
            mediaBrand(this);
            return onchangeOf.has(this) ? onchangeOf.get(this) : null;
        },
        function (value) {
            mediaBrand(this);
            onchangeOf.set(this, value);
        }
    );
    ['addListener', 'removeListener', 'addEventListener', 'removeEventListener'].forEach(function (name) {
        defineMethod(MediaQueryList.prototype, name, function () {
            mediaBrand(this);
        });
    });
    function matchMedia(query) {
        if (arguments.length < 1) {
            throw new TypeError("Failed to execute 'matchMedia' on 'Window': 1 argument required, but only 0 present.");
        }
        var list = new MediaQueryList(TOKEN);
        mediaOf.set(list, String(query));
        return list;
    }

    // IntersectionObserver: 何も観測しない不活性な最小実装（SEC-2。環境の実値を読まない）。
    function IntersectionObserver(callback) {
        if (!(this instanceof IntersectionObserver)) {
            throw new TypeError("Failed to construct 'IntersectionObserver': Please use the 'new' operator.");
        }
        if (typeof callback !== 'function') {
            throw new TypeError("Failed to construct 'IntersectionObserver': The callback provided as parameter 1 is not a function.");
        }
    }
    defineAccessor(IntersectionObserver.prototype, 'root', function () { return null; }, undefined);
    defineAccessor(IntersectionObserver.prototype, 'rootMargin', function () { return '0px 0px 0px 0px'; }, undefined);
    defineAccessor(IntersectionObserver.prototype, 'thresholds', function () { return [0]; }, undefined);
    defineMethod(IntersectionObserver.prototype, 'observe', function observe() {});
    defineMethod(IntersectionObserver.prototype, 'unobserve', function unobserve() {});
    defineMethod(IntersectionObserver.prototype, 'disconnect', function disconnect() {});
    defineMethod(IntersectionObserver.prototype, 'takeRecords', function takeRecords() {
        return [];
    });

    // performance: now() だけ（install 時点からの経過ms）。
    var perfStart = Date.now();
    var performanceObject = {};
    defineMethod(performanceObject, 'now', function now() {
        return Date.now() - perfStart;
    });

    // console: 記録は log/info/warn/error/debug のみ。引数の文字列化や転送で throw しない。
    // Rust 側の上限（4096 バイト）を超えたことが分かるよう、1 単位余分に残す。
    var MAX_TEXT_UNITS = 4097;
    var MAX_FORWARDED = 64;
    var forwarded = 0;
    // console.error / warn の Error の stack を取るか（診断専用の opt-in。install 時に 1 回だけ Rust 側へ
    // 問い合わせる。失敗は false。false の間は `.stack` に一切触れない。Issue #1183・SEC-2）。
    var captureStack = false;
    try {
        captureStack = op('timerOriginCapture') === true;
    } catch (e) {
        captureStack = false;
    }

    // 引数のうち最初の Error 風オブジェクトの stack を 1024 UTF-16 単位で返す（なければ ''）。
    // ページが getter で throw しても console を壊さない。
    function pickStack(args) {
        for (var i = 0; i < args.length; i++) {
            try {
                var a = args[i];
                if (a !== null && typeof a === 'object') {
                    var st = a.stack;
                    if (typeof st === 'string' && st.length > 0) {
                        var end = Math.min(st.length, 1024);
                        var c = st.charCodeAt(end - 1);
                        if (end < st.length && c >= 0xd800 && c <= 0xdbff) {
                            end -= 1;
                        }
                        return st.slice(0, end);
                    }
                }
            } catch (e) {
                // 次の引数へ。
            }
        }
        return '';
    }

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
                var sep = i > 0 ? ' ' : '';
                // 連結前に残り文字数へ切り詰め、巨大文字列の連結を避ける。
                var room = MAX_TEXT_UNITS - text.length - sep.length;
                if (room <= 0) {
                    text += sep;
                    break;
                }
                text += sep + (piece.length > room ? piece.slice(0, room) : piece);
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
            var stack = captureStack && (level === 'error' || level === 'warn') ? pickStack(args) : '';
            op('consoleMessage', level, text, stack);
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
    expose('MediaQueryList', MediaQueryList);
    expose('matchMedia', matchMedia);
    expose('IntersectionObserver', IntersectionObserver);
    expose('performance', performanceObject);
    expose('window', global);
    expose('self', global);
    expose('navigator', navigatorObject);
    expose('console', consoleObject);
    // dom.js の後に評価されるため Document が参照できる。無ければ install の失敗を隠さず throw する。
    if (typeof global.Document !== 'function') {
        throw new TypeError('Document is not available');
    }
    defineAccessor(
        global.Document.prototype,
        'location',
        function () {
            return locationObject;
        },
        function (value) {
            ignoreChange('assignLocation', value);
        }
    );
    defineAccessor(
        global.Document.prototype,
        'URL',
        function () {
            return op('getLocation', 'href');
        },
        undefined
    );
    // document.defaultView: window（global）を返す標準の read-only プロパティ。d3 の bundle が
    // `options.window = document.defaultView` から `.history` を読む（#1173・JS-5。新しい識別面は
    // 足さない: SEC-2）。
    defineAccessor(
        global.Document.prototype,
        'defaultView',
        function () {
            return global;
        },
        undefined
    );
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
