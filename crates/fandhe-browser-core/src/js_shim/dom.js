// js_shim/dom.js: ページ内 JS に見せる document / Node / Element / Text の shim
// （TASK-108・Issue #778・ビヘイビア JS-5。SSOT: js-engine.md「ページ内 JS 実行の設計制約」決定 3）。
//
// 役割: ノード ID（数値）だけを持つ薄いラッパーを作り、操作はすべてネイティブ関数
// `__dom.op(opName, ...args)` へ転送する。DOM の実体・HTML パース・上限検証・ID 検証は
// core 側（dom_bridge.rs）が担い、この shim は ID の隠蔽と JS らしい API の形を整えるだけ。
// 呼び出し元: core の `js_shim::install`（`JsRuntime::install_dom_shim` 経由）が評価前に注入する。
//
// 設計上の要点:
// - ノード ID はクロージャ内の WeakMap/Map にだけ置き、ページ JS から直接見えない。
//   shim を迂回して `__dom.op` を直接呼ばれても、bridge 側が世代・範囲・型・arity を検証する。
// - `__dom.op` は初期化時に捕捉し、`__dom` 自体を固定する。ページが上書き・削除しても
//   再注入で真正な op が使われる。
// - readyState / currentScript は bridge（Rust 側）が持つ状態を毎回 op で読む（キャッシュしない）。
// - 全体を IIFE で包むため、同じコンテキストで再評価（ページごとの再 install）しても
//   SyntaxError にならない。
// - ES2015 の範囲に留める（V8 と boa で同一のソースを使うため。Proxy は使わない）。
//
// 未実装（REPAIR-3。後続 issue / #779 で追加）:
// - bridge のエラーはエンジンの例外（Error）のまま伝わる。DOMException への写像は未対応。
// - NodeList / HTMLCollection 互換（querySelectorAll は素の Array を返す）。
// - nodeType / tagName / getAttribute / parentNode 等（#771 のスパイク結果に応じて op ごと追加）。
// - window / location / navigator / console は js_shim の別ソース（#779）。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var nativeOp = bridge.op;
    // 再注入（ページごと）でもホストが bind した真正な op を使えるよう、`__dom` を
    // 書き換え・削除不能（non-writable / non-configurable）に固定し、bridge も凍結する。
    // 初回注入はページ JS の実行前に行われる前提（JS-5）。
    var desc = Object.getOwnPropertyDescriptor(global, '__dom');
    if (desc && desc.configurable) {
        Object.freeze(bridge);
        Object.defineProperty(global, '__dom', {
            value: bridge,
            writable: false,
            configurable: false,
            enumerable: false
        });
    }

    function op() {
        return nativeOp.apply(bridge, arguments);
    }

    // ページ JS から `new Node()` 等で作らせないための合言葉（クロージャ内のみ）。
    var TOKEN = {};
    var ids = new WeakMap();
    var wrappers = new Map();

    function Node(token) {
        if (token !== TOKEN) {
            throw new TypeError('Illegal constructor');
        }
    }

    function Element(token) {
        Node.call(this, token);
    }
    Element.prototype = Object.create(Node.prototype, {
        constructor: { value: Element, writable: true, configurable: true }
    });

    function Text(token) {
        Node.call(this, token);
    }
    Text.prototype = Object.create(Node.prototype, {
        constructor: { value: Text, writable: true, configurable: true }
    });

    function Document(token) {
        Node.call(this, token);
    }
    Document.prototype = Object.create(Node.prototype, {
        constructor: { value: Document, writable: true, configurable: true }
    });

    // id に対応する唯一のラッパーを返す（同じ ID は常に同じオブジェクト）。
    function wrap(id, Ctor) {
        if (id === null || id === undefined) {
            return null;
        }
        var w = wrappers.get(id);
        if (w === undefined) {
            w = Object.create(Ctor.prototype);
            ids.set(w, id);
            wrappers.set(id, w);
        }
        return w;
    }

    function idOf(node, method) {
        var id = ids.get(node);
        if (id === undefined) {
            throw new TypeError("Failed to execute '" + method + "': parameter is not of type 'Node'.");
        }
        return id;
    }

    function define(proto, name, descriptor) {
        descriptor.configurable = true;
        Object.defineProperty(proto, name, descriptor);
    }

    // --- Node ---
    define(Node.prototype, 'appendChild', {
        writable: true,
        value: function appendChild(child) {
            op('appendChild', idOf(this, 'appendChild'), idOf(child, 'appendChild'));
            return child;
        }
    });
    define(Node.prototype, 'insertBefore', {
        writable: true,
        value: function insertBefore(node, ref) {
            var refId = ref === null || ref === undefined ? null : idOf(ref, 'insertBefore');
            op('insertBefore', idOf(this, 'insertBefore'), idOf(node, 'insertBefore'), refId);
            return node;
        }
    });
    define(Node.prototype, 'removeChild', {
        writable: true,
        value: function removeChild(child) {
            op('removeChild', idOf(this, 'removeChild'), idOf(child, 'removeChild'));
            return child;
        }
    });
    define(Node.prototype, 'textContent', {
        get: function () {
            return op('getTextContent', idOf(this, 'textContent'));
        },
        set: function (value) {
            var text = value === null || value === undefined ? '' : String(value);
            op('setTextContent', idOf(this, 'textContent'), text);
        }
    });

    // --- Element ---
    define(Element.prototype, 'setAttribute', {
        writable: true,
        value: function setAttribute(name, value) {
            op('setAttribute', idOf(this, 'setAttribute'), String(name), String(value));
        }
    });
    define(Element.prototype, 'removeAttribute', {
        writable: true,
        value: function removeAttribute(name) {
            op('removeAttribute', idOf(this, 'removeAttribute'), String(name));
        }
    });
    define(Element.prototype, 'innerHTML', {
        get: function () {
            return op('getInnerHTML', idOf(this, 'innerHTML'));
        },
        set: function (value) {
            // HTML の解析は core のパーサーが行う（JS 側にパーサーは持たない）。
            var html = value === null ? '' : String(value);
            op('setInnerHTML', idOf(this, 'innerHTML'), html);
        }
    });

    function querySelector(scopeId, selector) {
        return wrap(op('querySelector', scopeId, String(selector)), Element);
    }

    function querySelectorAll(scopeId, selector) {
        var joined = op('querySelectorAll', scopeId, String(selector));
        var result = [];
        if (joined === '') {
            return result;
        }
        var parts = joined.split(',');
        for (var i = 0; i < parts.length; i++) {
            result.push(wrap(Number(parts[i]), Element));
        }
        return result;
    }

    define(Element.prototype, 'querySelector', {
        writable: true,
        value: function (selector) {
            return querySelector(idOf(this, 'querySelector'), selector);
        }
    });
    define(Element.prototype, 'querySelectorAll', {
        writable: true,
        value: function (selector) {
            return querySelectorAll(idOf(this, 'querySelectorAll'), selector);
        }
    });

    // --- Document ---
    define(Document.prototype, 'createElement', {
        writable: true,
        value: function createElement(name) {
            return wrap(op('createElement', String(name)), Element);
        }
    });
    define(Document.prototype, 'createTextNode', {
        writable: true,
        value: function createTextNode(data) {
            return wrap(op('createTextNode', String(data)), Text);
        }
    });
    define(Document.prototype, 'getElementById', {
        writable: true,
        value: function getElementById(id) {
            return wrap(op('getElementById', String(id)), Element);
        }
    });
    define(Document.prototype, 'querySelector', {
        writable: true,
        value: function (selector) {
            return querySelector(idOf(this, 'querySelector'), selector);
        }
    });
    define(Document.prototype, 'querySelectorAll', {
        writable: true,
        value: function (selector) {
            return querySelectorAll(idOf(this, 'querySelectorAll'), selector);
        }
    });
    define(Document.prototype, 'body', {
        get: function () {
            return wrap(op('body'), Element);
        }
    });
    define(Document.prototype, 'head', {
        get: function () {
            return wrap(op('head'), Element);
        }
    });
    define(Document.prototype, 'readyState', {
        get: function () {
            return op('readyState');
        }
    });
    define(Document.prototype, 'currentScript', {
        get: function () {
            return wrap(op('currentScript'), Element);
        }
    });

    function expose(name, value) {
        Object.defineProperty(global, name, {
            value: value,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    expose('Node', Node);
    expose('Element', Element);
    expose('Text', Text);
    expose('Document', Document);
    expose('document', wrap(op('documentRoot'), Document));
})(globalThis);
