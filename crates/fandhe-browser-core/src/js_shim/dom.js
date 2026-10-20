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
// - 変更系ラッパー（appendChild 等）は、ネイティブ op が成功した後に限り、observers.js の
//   MutationObserver へ変更を通知するフック（mutationSink）を呼ぶ（#1138・JS-5・JS-7）。
//   フックの受け渡しは install 中だけ存在する一時グローバル `__fandheDomMutationHandoff`
//   （observers.js が読んですぐ削除する）で行い、ノード ID はラッパーの外へ出さない。
//   フックの例外は DOM 操作の結果に影響させない。
//
// 未実装（REPAIR-3。後続 issue / #779 で追加）:
// - bridge のエラーはエンジンの例外（Error）のまま伝わる。DOMException への写像は未対応。
// - NodeList / HTMLCollection 互換（querySelectorAll は素の Array を返す）。
// - Element の tagName / nodeName（要素名を返す read-only op が無い。core の
//   `Document::local_name` / `namespace_url` を使う op が候補）。未定義のまま（装わない）。
// - parentNode / parentElement / childNodes / firstChild / lastChild / nextSibling /
//   previousSibling / nodeValue / namespaceURI / isConnected / contains /
//   compareDocumentPosition / attributes（属性の列挙）。対応する op が無く未定義のまま。
// - HTMLIFrameElement は名前だけ（instanceof が throw しないため。常に false。`new` は
//   Illegal constructor）。HTMLElement と他のサブクラスは未定義のまま（#1168）。
// - IDL 属性の反映は `name` / `type` だけ（簡略版。全要素に定義し、既定値・正規化なし。#1176）。
//   `id` / `className` / `value` / `checked` 等は未反映のまま（REPAIR-3）。
// - document.activeElement は簡略版（常に body。無ければ null。#1173）。focus / blur /
//   hasFocus は未定義のままでフォーカス追跡は無い（REPAIR-3）。
// - Comment / DocumentFragment のラッパー（nodeType 定数のみ。DOCUMENT_POSITION_* も未定義）。
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

    // react-dom の `getActiveElementDeep` が `el instanceof win.HTMLIFrameElement` で右辺 undefined の
    // TypeError にならないための名前だけの宣言（Issue #1168・JS-5）。ラッパーは常に Element で作る
    // （要素名を返す op が無い）ため、どの要素も `instanceof HTMLIFrameElement` は false になる
    // （REPAIR-3。実装済みを装わない）。HTMLElement は定義しない: 定義すると d3 の module 評価が
    // 未対応の擬似クラス selector で失敗するようになった（実測。REPORT #1168 節）。
    function HTMLIFrameElement(token) {
        Element.call(this, token);
    }
    HTMLIFrameElement.prototype = Object.create(Element.prototype, {
        constructor: { value: HTMLIFrameElement, writable: true, configurable: true }
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

    // MutationObserver への変更通知フック（observers.js が setSink で登録。未登録は null）。
    var mutationSink = null;
    var textProtoIsPrototypeOf = Function.prototype.call.bind(Object.prototype.isPrototypeOf, Text.prototype);

    // op 成功後に呼ぶ。フックの例外は握りつぶす（DOM 操作は既に成功している）。
    function notifyMutation(kind, target, info) {
        if (mutationSink !== null) {
            try {
                mutationSink(kind, target, info);
            } catch (e) {
                // 通知の失敗で DOM 操作の結果を変えない。
            }
        }
    }

    function define(proto, name, descriptor) {
        descriptor.configurable = true;
        Object.defineProperty(proto, name, descriptor);
    }

    // --- Node の基本プロパティ（#1151・JS-5。React createRoot の isValidContainer 等が読む） ---
    // nodeType の定数。WebIDL どおり Node コンストラクタとプロトタイプの両方に置く（読み取り専用）。
    var NODE_CONSTANTS = [
        ['ELEMENT_NODE', 1], ['ATTRIBUTE_NODE', 2], ['TEXT_NODE', 3], ['CDATA_SECTION_NODE', 4],
        ['ENTITY_REFERENCE_NODE', 5], ['ENTITY_NODE', 6], ['PROCESSING_INSTRUCTION_NODE', 7],
        ['COMMENT_NODE', 8], ['DOCUMENT_NODE', 9], ['DOCUMENT_TYPE_NODE', 10],
        ['DOCUMENT_FRAGMENT_NODE', 11], ['NOTATION_NODE', 12]
    ];
    for (var ci = 0; ci < NODE_CONSTANTS.length; ci++) {
        var constDesc = { value: NODE_CONSTANTS[ci][1], writable: false, enumerable: true, configurable: false };
        Object.defineProperty(Node, NODE_CONSTANTS[ci][0], constDesc);
        Object.defineProperty(Node.prototype, NODE_CONSTANTS[ci][0], constDesc);
    }

    // ラッパー型ごとに固定値を返す getter。ラッパー以外の受け手は拒否する（ID は露出しない）。
    function constGetter(name, value) {
        return {
            get: function () {
                if (!ids.has(this)) {
                    throw new TypeError("Illegal invocation");
                }
                return value;
            }
        };
    }
    define(Element.prototype, 'nodeType', constGetter('nodeType', 1));
    define(Text.prototype, 'nodeType', constGetter('nodeType', 3));
    define(Document.prototype, 'nodeType', constGetter('nodeType', 9));
    define(Text.prototype, 'nodeName', constGetter('nodeName', '#text'));
    define(Document.prototype, 'nodeName', constGetter('nodeName', '#document'));

    // 文書は 1 つだけなので ownerDocument は install 時の document ラッパー（op 不要）。
    var documentWrapper = null;
    function ownerDocumentGetter(isDocument) {
        return {
            get: function () {
                if (!ids.has(this)) {
                    throw new TypeError("Illegal invocation");
                }
                return isDocument ? null : documentWrapper;
            }
        };
    }
    define(Element.prototype, 'ownerDocument', ownerDocumentGetter(false));
    define(Text.prototype, 'ownerDocument', ownerDocumentGetter(false));
    define(Document.prototype, 'ownerDocument', ownerDocumentGetter(true));

    // --- Node ---
    define(Node.prototype, 'appendChild', {
        writable: true,
        value: function appendChild(child) {
            op('appendChild', idOf(this, 'appendChild'), idOf(child, 'appendChild'));
            notifyMutation('childList', this, { added: [child], removed: [], nextSibling: null });
            return child;
        }
    });
    define(Node.prototype, 'insertBefore', {
        writable: true,
        value: function insertBefore(node, ref) {
            var refId = ref === null || ref === undefined ? null : idOf(ref, 'insertBefore');
            op('insertBefore', idOf(this, 'insertBefore'), idOf(node, 'insertBefore'), refId);
            notifyMutation('childList', this, {
                added: [node],
                removed: [],
                nextSibling: refId === null ? null : ref
            });
            return node;
        }
    });
    define(Node.prototype, 'removeChild', {
        writable: true,
        value: function removeChild(child) {
            op('removeChild', idOf(this, 'removeChild'), idOf(child, 'removeChild'));
            notifyMutation('childList', this, { added: [], removed: [child], nextSibling: null });
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
            // Text は characterData、それ以外（Element / Document）は childList。後者の
            // 追加・削除ノードは子の列挙 op が無いため空（スタブ。REPAIR-3）。
            if (textProtoIsPrototypeOf(this)) {
                notifyMutation('characterData', this, null);
            } else {
                notifyMutation('childList', this, { added: [], removed: [], nextSibling: null });
            }
        }
    });

    // --- Element ---
    define(Element.prototype, 'setAttribute', {
        writable: true,
        value: function setAttribute(name, value) {
            var attr = String(name);
            // bridge は実際に保存された（正規化後の）属性名を返す。通知にはその名前を使う。
            var stored = op('setAttribute', idOf(this, 'setAttribute'), attr, String(value));
            notifyMutation('attributes', this, { attributeName: typeof stored === 'string' ? stored : attr });
        }
    });
    define(Element.prototype, 'removeAttribute', {
        writable: true,
        value: function removeAttribute(name) {
            var attr = String(name);
            // 属性が無いときは DOM 仕様どおり変更なし（レコードを作らない）。bridge が削除有無を返す。
            var removedName = op('removeAttribute', idOf(this, 'removeAttribute'), attr);
            if (typeof removedName === 'string') {
                notifyMutation('attributes', this, { attributeName: removedName });
            }
        }
    });
    // 属性値の読み取り。bridge は文字列か null を返す（エンジン間の null / undefined の差はここで吸収）。
    function readAttribute(self, method, name) {
        var v = op('getAttribute', idOf(self, method), String(name));
        return typeof v === 'string' ? v : null;
    }
    define(Element.prototype, 'getAttribute', {
        writable: true,
        value: function getAttribute(name) {
            return readAttribute(this, 'getAttribute', name);
        }
    });
    define(Element.prototype, 'hasAttribute', {
        writable: true,
        value: function hasAttribute(name) {
            // ページが差し替え得る getAttribute を経由せず、同じ op で判定する。
            return readAttribute(this, 'hasAttribute', name) !== null;
        }
    });
    // 属性を反映する IDL プロパティ `name` / `type`（#1176）。react-dom 19 の initInput は
    // `node.name = ...` / `node.type = ...` とプロパティへ代入するため、反映が無いと属性が残らず、
    // `input[name]` が空になる（d2 で `form` の名前付きフィールドが 0 件になる原因と推定）。
    // 簡略版（REPAIR-3）: 要素の種別を知る op が無いため全要素に定義し、`type` の既定値（input の
    // "text"）や列挙値の正規化はせず、属性の文字列をそのまま返す（無ければ空文字）。
    function reflectedAttribute(attr) {
        return {
            get: function () {
                var v = readAttribute(this, attr, attr);
                return v === null ? '' : v;
            },
            set: function (value) {
                // ページが差し替え得る setAttribute を経由せず、同じ op で直接保存する。
                var stored = op('setAttribute', idOf(this, attr), attr, String(value));
                notifyMutation('attributes', this, { attributeName: typeof stored === 'string' ? stored : attr });
            }
        };
    }
    define(Element.prototype, 'name', reflectedAttribute('name'));
    define(Element.prototype, 'type', reflectedAttribute('type'));
    define(Element.prototype, 'innerHTML', {
        get: function () {
            return op('getInnerHTML', idOf(this, 'innerHTML'));
        },
        set: function (value) {
            // HTML の解析は core のパーサーが行う（JS 側にパーサーは持たない）。
            var html = value === null ? '' : String(value);
            op('setInnerHTML', idOf(this, 'innerHTML'), html);
            // 追加・削除ノードは子の列挙 op が無いため空（スタブ。REPAIR-3）。
            notifyMutation('childList', this, { added: [], removed: [], nextSibling: null });
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
    // activeElement: フォーカス管理が無いため常に body（無ければ null）を返す簡略版
    // （REPAIR-3。実装済みを装わない）。react-dom の getActiveElementDeep /
    // getSelectionInformation が読む（#1168・#1173・JS-5）。標準プロパティで新しい識別面は
    // 足さない（SEC-2）。setter なし。将来仕様: focus() / blur() と連動したフォーカス追跡。
    define(Document.prototype, 'activeElement', {
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

    // observers.js へ渡す一回限りのハンドオフ。observers.js が読んで直ちに削除する
    // （install 中だけ存在し、ページ JS の実行前に消える。JS-5）。
    Object.defineProperty(global, '__fandheDomMutationHandoff', {
        value: Object.freeze({
            setSink: function (fn) {
                mutationSink = typeof fn === 'function' ? fn : null;
            },
            // 祖先関係の判定（MutationObserver の subtree 用）。ID はここで解決し外へ出さない。
            contains: function (ancestor, node) {
                return op('isInclusiveAncestor', idOf(ancestor, 'contains'), idOf(node, 'contains')) === true;
            }
        }),
        writable: true,
        configurable: true,
        enumerable: false
    });

    expose('Node', Node);
    expose('Element', Element);
    expose('HTMLIFrameElement', HTMLIFrameElement);
    expose('Text', Text);
    expose('Document', Document);
    documentWrapper = wrap(op('documentRoot'), Document);
    expose('document', documentWrapper);
})(globalThis);
