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
// - bridge のエラーはエンジンの例外（Error）のまま伝わる。DOMException への写像は未対応
//   （classList のトークン検証の例外も DOMException ではなく TypeError。#1236）。
// - classList（#1236）は DOMTokenList の最小実装（add・remove・contains・toggle・length・item）。
//   value・toString・replace・supports・forEach / entries / keys / values / Symbol.iterator・添字アクセス・
//   グローバル `DOMTokenList` は未実装（成功を装わない）。
// - getElementsByTagName は簡略版（#1200）: live でない静的な Array で HTMLCollection（item / namedItem）
//   ではない。照合は要素の名前空間ごと（HTML は小文字化・それ以外は大文字小文字を区別）。
//   getElementsByTagNameNS・getElementsByClassName は未実装。
// - NodeList / HTMLCollection 互換（querySelectorAll は素の Array を返す）。
// - Element の tagName / localName / nodeName は実装済み（#1236）。op `elementLocalName` と
//   `htmlLocalName` から求め、HTML 名前空間の要素の tagName・nodeName は大文字。接頭辞付きの要素は
//   作られないため qualified name は local name と一致する。
// - parentNode / parentElement は簡略版（#1203）: op `parentNode` が返す親（Element か文書）だけを
//   読み取り専用で返し、親が DocumentFragment なら null（ラッパー型が無い。REPAIR-3）。
// - isEqualNode（#1227）は標準どおりの比較。Comment・Doctype のラッパーは無いが、子としては比較に含まれる。
// - childNodes / firstChild / lastChild / nextSibling /
//   previousSibling / nodeValue / namespaceURI / isConnected / contains /
//   compareDocumentPosition / attributes（属性の列挙）。対応する op が無く未定義のまま。
// - HTMLElement・HTMLIFrameElement は名前だけ（instanceof / typeof 判定が通るため。常に false。
//   `new` は Illegal constructor。継承は Element ← HTMLElement ← HTMLIFrameElement）。他の
//   サブクラスは未定義のまま（#1168・#1184）。要素名を返す op（`elementLocalName`）は追加済みだがラッパーは常に Element で作る。将来は HTML
//   名前空間の要素を HTMLElement のインスタンスにする（REPAIR-3）。
// - createElementNS は HTML・SVG の名前空間だけを扱う簡略版（#1192）。HTML 名前空間では名前を
//   小文字化する（仕様は大文字小文字を保つ。差異）。MathML・null・その他の名前空間と接頭辞付きの名前
//   （`svg:rect`）は例外になる（成功を装わない）。`namespaceURI` / `prefix`
//   の IDL、`SVGElement` などの SVG 側のクラス、`setAttributeNS` / `getAttributeNS` は未実装。
// - IDL 属性の反映（#1176・#1186）は簡略版。反映するのは `name`・`type`（全要素。`type` の getter は
//   input を列挙値へ正規化し既定 "text"、button は既定 "submit"）、`id`・`className`（全要素。SVG の
//   className も文字列で返す点が標準と異なる）、`placeholder`・`value`・`disabled`・`checked`・
//   `readOnly`・`required`・`multiple`（HTML 名前空間の該当要素のみ。適用外の要素では属性へ反映せず
//   通常の own データプロパティとして代入値を読み戻すだけで、`'value' in div` が代入前から true になる点が標準と異なる）。
//   未実装（REPAIR-3）: `value` の dirty value flag・内部値（代入は属性を書き換える。AI スナップショット・
//   セレクタ・22 タスク判定と一致させるため）、checkedness・ラジオ排他、`defaultValue` /
//   `defaultChecked` / `selected`、value mode・サニタイズ、select / textarea の value（読み戻しのみ）、
//   select.type 等の計算済み type、form.method 等の列挙既定値・`htmlFor`・URL 反映属性、
//   HTMLInputElement 等の具象サブクラス。
// - フォーカス追跡は簡略版（#1173・#1185）。`Element.prototype.focus()` / `blur()` が `document.activeElement` を
//   更新する（何もフォーカスされていなければ body。無ければ null）。フォーカスできるかは op `isFocusable`
//   （Rust 側の簡略判定。dom_bridge/focus.rs）で決め、状態はこのクロージャの 1 枠だけが持つ。DOM・属性を変える操作の直後（と `activeElement` の読み取り時）に有効性を
//   確かめ直し、切り離し・disabled 付与・tabindex 除去でフォーカスを失った要素は失効を保持して
//   blur を配らず body に戻す（HTML の focus fixup の近似）。focus / blur は install 中だけの一時グローバル
//   `__fandheDomFocusHandoff` 経由で events.js が対象要素にだけ配る。
//   未実装（REPAIR-3）: `document.hasFocus()`・FocusEvent / relatedTarget・focusin / focusout・祖先への
//   伝播・`tabIndex` IDL 属性・autofocus・レンダリングに基づく判定。focus / blur は仕様では
//   HTMLOrSVGElement の mixin だがラッパーがすべて Element のため Element.prototype に置く。
// - `Element.prototype.sheet` / `document.styleSheets` は stylesheet.js が担う（#1202）。
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

    // `typeof HTMLElement !== 'undefined'` を判定する emotion（`@emotion/react` の EmotionCacheContext）と、
    // react-dom の `getActiveElementDeep` の `el instanceof win.HTMLIFrameElement` が右辺 undefined の
    // TypeError にならないための名前だけの宣言（Issue #1168・#1184・JS-5）。ラッパーは常に Element で
    // 作る（要素名・名前空間を返す op が無い）ため、どの要素も `instanceof HTMLElement` /
    // `HTMLIFrameElement` は false になる（REPAIR-3。実装済みを装わない。全ラッパーを
    // HTMLElement にすると SVG 要素まで名乗ってしまう）。
    // #1192 で d3 の次の失敗が instanceof 由来かを確かめた結果は REPORT #1192 節にある
    // （由来でなければ据え置き）。
    // HTMLElement は #1184 で selector の `:not()` に対応した後に定義した（定義だけ先行すると d3 が失敗した。REPORT #1168 節）。
    function HTMLElement(token) {
        Element.call(this, token);
    }
    HTMLElement.prototype = Object.create(Element.prototype, {
        constructor: { value: HTMLElement, writable: true, configurable: true }
    });
    function HTMLIFrameElement(token) {
        HTMLElement.call(this, token);
    }
    HTMLIFrameElement.prototype = Object.create(HTMLElement.prototype, {
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
            // 移動は切り離し＋再接続。移動対象の部分木内のフォーカスは失われ、再接続では復活しない。
            // 失敗する操作（循環・不正な親）では DOM が変わらないので、解除は op 成功後に確定する。
            var dropFocus = focusInSubtree(child);
            op('appendChild', idOf(this, 'appendChild'), idOf(child, 'appendChild'));
            if (dropFocus) {
                focused = null;
            }
            revalidateFocus();
            notifyMutation('childList', this, { added: [child], removed: [], nextSibling: null });
            return child;
        }
    });
    define(Node.prototype, 'insertBefore', {
        writable: true,
        value: function insertBefore(node, ref) {
            var refId = ref === null || ref === undefined ? null : idOf(ref, 'insertBefore');
            // node === ref は Rust 側が DOM を変えず成功する（位置不変）ため、フォーカスも保持する。
            var dropFocus = ref !== node && focusInSubtree(node);
            op('insertBefore', idOf(this, 'insertBefore'), idOf(node, 'insertBefore'), refId);
            if (dropFocus) {
                focused = null;
            }
            revalidateFocus();
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
            revalidateFocus();
            notifyMutation('childList', this, { added: [], removed: [child], nextSibling: null });
            return child;
        }
    });
    // 親ノード（タグ管理系の `getElementsByTagName('script')[0].parentNode.insertBefore` が使う。
    // #1203）。親は Element か document のラッパーで、親なし・fragment 親は null。
    function parentNodeOf(self) {
        if (!ids.has(self)) {
            throw new TypeError("Illegal invocation");
        }
        var pid = op('parentNode', ids.get(self));
        if (pid === null || pid === undefined) {
            return null;
        }
        if (pid === ids.get(documentWrapper)) {
            return documentWrapper;
        }
        return wrap(pid, Element);
    }
    // isEqualNode: DOM 標準の「node equals」。比較は Rust 側（dom_bridge/node_equal.rs。明示スタック・
    // 反復上限つき）。引数省略・null / undefined は false（IDL は optional Node? otherNode = null）、Node でない値は TypeError（#1227・JS-5。React / Next.js
    // のクライアントが差分検出に使う）。
    define(Node.prototype, 'isEqualNode', {
        writable: true,
        value: function isEqualNode(other) {
            var self = idOf(this, 'isEqualNode');
            if (other === null || other === undefined) {
                return false;
            }
            return op('isEqualNode', self, idOf(other, 'isEqualNode')) === true;
        }
    });

    define(Node.prototype, 'parentNode', {
        get: function () {
            return parentNodeOf(this);
        }
    });
    define(Node.prototype, 'parentElement', {
        get: function () {
            var parent = parentNodeOf(this);
            return parent === documentWrapper ? null : parent;
        }
    });
    define(Node.prototype, 'textContent', {
        get: function () {
            return op('getTextContent', idOf(this, 'textContent'));
        },
        set: function (value) {
            var text = value === null || value === undefined ? '' : String(value);
            op('setTextContent', idOf(this, 'textContent'), text);
            revalidateFocus();
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
            revalidateFocus();
            notifyMutation('attributes', this, { attributeName: typeof stored === 'string' ? stored : attr });
        }
    });
    define(Element.prototype, 'removeAttribute', {
        writable: true,
        value: function removeAttribute(name) {
            var attr = String(name);
            // 属性が無いときは DOM 仕様どおり変更なし（レコードを作らない）。bridge が削除有無を返す。
            var removedName = op('removeAttribute', idOf(this, 'removeAttribute'), attr);
            revalidateFocus();
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
    // `name` は全要素へ属性をそのまま反映する簡略版（REPAIR-3）。`type` は下の種別規則つき。
    function reflectedAttribute(attr) {
        return {
            get: function () {
                var v = readAttribute(this, attr, attr);
                return v === null ? '' : v;
            },
            set: function (value) {
                storeAttribute(this, attr, String(value));
            }
        };
    }
    // ページが差し替え得る setAttribute を経由せず、同じ op で直接保存する。
    function storeAttribute(self, attr, text) {
        var stored = op('setAttribute', idOf(self, attr), attr, text);
        revalidateFocus();
        notifyMutation('attributes', self, { attributeName: typeof stored === 'string' ? stored : attr });
    }
    // 属性を除去する。属性が無かった場合（bridge が名前を返さない）は通知しない。
    function dropAttribute(self, attr) {
        var removed = op('removeAttribute', idOf(self, attr), attr);
        revalidateFocus();
        if (typeof removed === 'string') {
            notifyMutation('attributes', self, { attributeName: removed });
        }
    }

    // --- 要素種別ごとの IDL 属性の反映（#1186・JS-5・REPAIR-3・SEC-2）---
    // 種別（HTML 名前空間の local name）は op `htmlLocalName` で 1 度だけ取得してラッパーごとに
    // キャッシュする（ノード ID に対し不変。max_ops_per_page を消費し続けないため）。
    var localNames = new WeakMap();
    function localNameOf(self) {
        if (localNames.has(self)) {
            return localNames.get(self);
        }
        var v = op('htmlLocalName', idOf(self, 'localName'));
        var name = typeof v === 'string' ? v : null;
        localNames.set(self, name);
        return name;
    }
    // ページが差し替え得る String.prototype のメソッドに読み取り時に頼らないよう、install 時に束縛する。
    var sCharCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
    var sCharAt = Function.prototype.call.bind(String.prototype.charAt);
    var fromCharCode = String.fromCharCode;
    // ASCII 小文字化。
    function asciiLower(str) {
        var out = '';
        for (var i = 0; i < str.length; i++) {
            var c = sCharCodeAt(str, i);
            out += c >= 65 && c <= 90 ? fromCharCode(c + 32) : sCharAt(str, i);
        }
        return out;
    }
    function isAsciiSpace(c) {
        return c === 0x20 || c === 0x09 || c === 0x0A || c === 0x0C || c === 0x0D;
    }

    // --- tagName / localName / nodeName（#1236・JS-5）---
    // Next.js の head 更新処理が `e.tagName.toLowerCase()` を読む。値は op `elementLocalName`（名前空間を
    // 問わず大文字小文字を保つ local name）と `htmlLocalName`（HTML 名前空間か）から求め、ラッパーごとに
    // キャッシュする（ノード ID に対し不変。max_ops_per_page を消費し続けないため）。HTML 名前空間の
    // 要素の tagName は ASCII 大文字、それ以外は local name のまま。本実装では接頭辞付きの要素は作られない
    // （パーサーは接頭辞を付けず、createElementNS は接頭辞付きの名前を拒否する）ため、qualified name は
    // local name と一致する。
    function asciiUpper(str) {
        var out = '';
        for (var i = 0; i < str.length; i++) {
            var c = sCharCodeAt(str, i);
            out += c >= 97 && c <= 122 ? fromCharCode(c - 32) : sCharAt(str, i);
        }
        return out;
    }
    var elementNames = new WeakMap();
    function elementNameOf(self, member) {
        var cached = elementNames.get(self);
        if (cached === undefined) {
            var id = idOf(self, member);
            var local = op('elementLocalName', id);
            local = typeof local === 'string' ? local : '';
            var html = op('htmlLocalName', id);
            cached = { local: local, tag: typeof html === 'string' ? asciiUpper(local) : local };
            elementNames.set(self, cached);
        }
        return cached;
    }
    define(Element.prototype, 'tagName', {
        get: function () { return elementNameOf(this, 'tagName').tag; }
    });
    define(Element.prototype, 'nodeName', {
        get: function () { return elementNameOf(this, 'nodeName').tag; }
    });
    define(Element.prototype, 'localName', {
        get: function () { return elementNameOf(this, 'localName').local; }
    });

    // --- classList（#1236・JS-5・REPAIR-3）---
    // DOMTokenList 風の最小実装。`class` 属性を毎回読み（live）、ASCII 空白で区切って重複を除いた集合
    // として扱い、更新は `storeAttribute` で直接保存する（ページが差し替え得る setAttribute / className /
    // String.prototype.split には頼らない）。属性が無く結果が空なら何もしない（空の class を作らない）。
    // 未実装（実装済みを装わない）: value・toString・replace・supports・forEach / entries / keys / values /
    // Symbol.iterator・添字アクセス（classList[0]）・グローバル `DOMTokenList`・DOMException
    // （トークン検証の例外は TypeError）。
    function parseTokens(str) {
        var out = [];
        var cur = '';
        for (var i = 0; i <= str.length; i++) {
            var c = i < str.length ? sCharCodeAt(str, i) : 0x20;
            if (isAsciiSpace(c)) {
                if (cur !== '') {
                    var dup = false;
                    for (var j = 0; j < out.length; j++) {
                        if (out[j] === cur) { dup = true; break; }
                    }
                    if (!dup) { out[out.length] = cur; }
                    cur = '';
                }
            } else {
                cur += sCharAt(str, i);
            }
        }
        return out;
    }
    function serializeTokens(tokens) {
        var out = '';
        for (var i = 0; i < tokens.length; i++) {
            out += (i === 0 ? '' : ' ') + tokens[i];
        }
        return out;
    }
    function checkToken(token) {
        if (token === '') {
            throw new TypeError('The token provided must not be empty.');
        }
        for (var i = 0; i < token.length; i++) {
            if (isAsciiSpace(sCharCodeAt(token, i))) {
                throw new TypeError('The token provided contains HTML space characters, which are not valid in tokens.');
            }
        }
    }
    var classListOwner = new WeakMap();
    var classListOf = new WeakMap();
    var DOMTokenListProto = {};
    function tokenListOwner(list) {
        var el = classListOwner.get(list);
        if (el === undefined) {
            throw new TypeError('Illegal invocation');
        }
        return el;
    }
    function readTokens(el) {
        var v = readAttribute(el, 'classList', 'class');
        return { had: v !== null, tokens: parseTokens(v === null ? '' : v) };
    }
    function writeTokens(el, state, tokens) {
        if (!state.had && tokens.length === 0) {
            return;
        }
        storeAttribute(el, 'class', serializeTokens(tokens));
    }
    function indexOfToken(tokens, t) {
        for (var i = 0; i < tokens.length; i++) {
            if (tokens[i] === t) { return i; }
        }
        return -1;
    }
    define(DOMTokenListProto, 'length', {
        get: function () { return readTokens(tokenListOwner(this)).tokens.length; }
    });
    define(DOMTokenListProto, 'item', {
        writable: true,
        value: function item(index) {
            var tokens = readTokens(tokenListOwner(this)).tokens;
            var i = Number(index) >>> 0;
            return i < tokens.length ? tokens[i] : null;
        }
    });
    define(DOMTokenListProto, 'contains', {
        writable: true,
        value: function contains(token) {
            return indexOfToken(readTokens(tokenListOwner(this)).tokens, String(token)) !== -1;
        }
    });
    define(DOMTokenListProto, 'add', {
        writable: true,
        value: function add() {
            var el = tokenListOwner(this);
            var given = [];
            for (var i = 0; i < arguments.length; i++) {
                var t = String(arguments[i]);
                checkToken(t);
                given[given.length] = t;
            }
            var state = readTokens(el);
            for (var k = 0; k < given.length; k++) {
                if (indexOfToken(state.tokens, given[k]) === -1) {
                    state.tokens[state.tokens.length] = given[k];
                }
            }
            writeTokens(el, state, state.tokens);
        }
    });
    define(DOMTokenListProto, 'remove', {
        writable: true,
        value: function remove() {
            var el = tokenListOwner(this);
            var given = [];
            for (var i = 0; i < arguments.length; i++) {
                var t = String(arguments[i]);
                checkToken(t);
                given[given.length] = t;
            }
            var state = readTokens(el);
            var kept = [];
            for (var k = 0; k < state.tokens.length; k++) {
                if (indexOfToken(given, state.tokens[k]) === -1) {
                    kept[kept.length] = state.tokens[k];
                }
            }
            writeTokens(el, state, kept);
        }
    });
    define(DOMTokenListProto, 'toggle', {
        writable: true,
        value: function toggle(token, force) {
            var el = tokenListOwner(this);
            var t = String(token);
            checkToken(t);
            var state = readTokens(el);
            var at = indexOfToken(state.tokens, t);
            var hasForce = arguments.length > 1 && force !== undefined;
            if (at !== -1) {
                if (hasForce && force) {
                    return true;
                }
                var kept = [];
                for (var k = 0; k < state.tokens.length; k++) {
                    if (k !== at) { kept[kept.length] = state.tokens[k]; }
                }
                writeTokens(el, state, kept);
                return false;
            }
            if (hasForce && !force) {
                return false;
            }
            state.tokens[state.tokens.length] = t;
            writeTokens(el, state, state.tokens);
            return true;
        }
    });
    define(Element.prototype, 'classList', {
        get: function () {
            idOf(this, 'classList');
            var list = classListOf.get(this);
            if (list === undefined) {
                list = Object.create(DOMTokenListProto);
                classListOwner.set(list, this);
                classListOf.set(this, list);
            }
            return list;
        }
    });
    // 仕様の option の text 値: ASCII 空白の連続を 1 つの U+0020 に畳み、前後を除去する。
    function stripCollapseWhitespace(str) {
        var out = '';
        var pendingSpace = false;
        for (var i = 0; i < str.length; i++) {
            var c = sCharCodeAt(str, i);
            if (isAsciiSpace(c)) {
                pendingSpace = out !== '';
            } else {
                if (pendingSpace) {
                    out += ' ';
                    pendingSpace = false;
                }
                out += sCharAt(str, i);
            }
        }
        return out;
    }
    function nameSet(list) {
        var set = Object.create(null);
        for (var i = 0; i < list.length; i++) {
            set[list[i]] = true;
        }
        return set;
    }
    // 適用外の要素（div.value 等）は属性へ反映せず、代入時にラッパー自身へ通常の own データプロパティ
    // （configurable・enumerable・writable）を定義する。以後の読み書き・delete・hasOwnProperty は
    // 従来の expando と同じに動く。delete 後はプロトタイプの getter に戻り undefined を返す。
    // React の値追跡が先に張る own accessor がある場合はこの setter に到達しない。
    function expandoSet(self, key, value) {
        Object.defineProperty(self, key, {
            value: value,
            writable: true,
            enumerable: true,
            configurable: true
        });
    }
    function applies(self, set) {
        if (set === null) {
            return true;
        }
        var n = localNameOf(self);
        return n !== null && set[n] === true;
    }
    // 文字列属性（`id` / `className` / `placeholder` / `value`）。`appliesTo` が null なら全要素。
    function reflectString(prop, attr, appliesTo, opts) {
        var set = appliesTo === null ? null : nameSet(appliesTo);
        return {
            get: function () {
                if (!applies(this, set)) {
                    return undefined;
                }
                var v = readAttribute(this, prop, attr);
                if (v === null && opts && opts.optionText && localNameOf(this) === 'option') {
                    return stripCollapseWhitespace(String(op('getOptionText', idOf(this, prop))));
                }
                return v === null ? '' : v;
            },
            set: function (value) {
                if (!applies(this, set)) {
                    expandoSet(this, prop, value);
                    return;
                }
                var text = opts && opts.nullAsEmpty && value === null && localNameOf(this) === 'input' ? '' : String(value);
                storeAttribute(this, attr, text);
            }
        };
    }
    // 真偽属性（属性の有無）。
    function reflectBoolean(prop, attr, appliesTo) {
        var set = nameSet(appliesTo);
        return {
            get: function () {
                if (!applies(this, set)) {
                    return undefined;
                }
                return readAttribute(this, prop, attr) !== null;
            },
            set: function (value) {
                if (!applies(this, set)) {
                    expandoSet(this, prop, value);
                    return;
                }
                if (value) {
                    storeAttribute(this, attr, '');
                } else {
                    dropAttribute(this, attr);
                }
            }
        };
    }
    var INPUT_TYPES = nameSet([
        'hidden', 'text', 'search', 'tel', 'url', 'email', 'password', 'date', 'month', 'week',
        'time', 'datetime-local', 'number', 'range', 'color', 'checkbox', 'radio', 'file', 'submit',
        'image', 'reset', 'button'
    ]);
    var BUTTON_TYPES = nameSet(['submit', 'reset', 'button']);
    // `type`: input は列挙値へ正規化（既定 text）、button は既定 submit。他は属性値か空文字。
    // setter は仕様どおり与えた文字列をそのまま属性へ保存する（正規化は getter だけ）。
    define(Element.prototype, 'type', {
        get: function () {
            var raw = readAttribute(this, 'type', 'type');
            var kind = localNameOf(this);
            if (kind === 'input') {
                var lower = raw === null ? '' : asciiLower(raw);
                return INPUT_TYPES[lower] === true ? lower : 'text';
            }
            if (kind === 'button') {
                var b = raw === null ? '' : asciiLower(raw);
                return BUTTON_TYPES[b] === true ? b : 'submit';
            }
            return raw === null ? '' : raw;
        },
        set: reflectedAttribute('type').set
    });
    define(Element.prototype, 'name', reflectedAttribute('name'));
    define(Element.prototype, 'id', reflectString('id', 'id', null));
    define(Element.prototype, 'className', reflectString('className', 'class', null));
    define(Element.prototype, 'placeholder', reflectString('placeholder', 'placeholder', ['input', 'textarea']));
    // `value` は属性への反映のみ（dirty value flag・内部値は未実装。REPAIR-3）。textarea / select は
    // 適用外のため従来どおり代入値の読み戻しだけ。
    define(
        Element.prototype,
        'value',
        reflectString('value', 'value', ['input', 'button', 'data', 'param', 'option'], {
            nullAsEmpty: true,
            optionText: true
        })
    );
    define(
        Element.prototype,
        'disabled',
        reflectBoolean('disabled', 'disabled', ['button', 'fieldset', 'input', 'optgroup', 'option', 'select', 'textarea'])
    );
    define(Element.prototype, 'checked', reflectBoolean('checked', 'checked', ['input']));
    define(Element.prototype, 'readOnly', reflectBoolean('readOnly', 'readonly', ['input', 'textarea']));
    define(Element.prototype, 'required', reflectBoolean('required', 'required', ['input', 'select', 'textarea']));
    define(Element.prototype, 'multiple', reflectBoolean('multiple', 'multiple', ['input', 'select']));
    define(Element.prototype, 'innerHTML', {
        get: function () {
            return op('getInnerHTML', idOf(this, 'innerHTML'));
        },
        set: function (value) {
            // HTML の解析は core のパーサーが行う（JS 側にパーサーは持たない）。
            var html = value === null ? '' : String(value);
            op('setInnerHTML', idOf(this, 'innerHTML'), html);
            revalidateFocus();
            // 追加・削除ノードは子の列挙 op が無いため空（スタブ。REPAIR-3）。
            notifyMutation('childList', this, { added: [], removed: [], nextSibling: null });
        }
    });

    function querySelector(scopeId, selector) {
        return wrap(op('querySelector', scopeId, String(selector)), Element);
    }

    function querySelectorAll(scopeId, selector) {
        return wrapIdList(op('querySelectorAll', scopeId, String(selector)));
    }

    // カンマ結合の ID 列を要素ラッパーの素の Array にする（querySelectorAll・getElementsByTagName 共通）。
    function wrapIdList(joined) {
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

    // 簡略版（#1200。REPAIR-3）: live でない静的な Array を返す。
    function getElementsByTagName(scopeId, name) {
        return wrapIdList(op('getElementsByTagName', scopeId, String(name)));
    }

    define(Element.prototype, 'getElementsByTagName', {
        writable: true,
        value: function getElementsByTagName_(qualifiedName) {
            return getElementsByTagName(idOf(this, 'getElementsByTagName'), qualifiedName);
        }
    });
    define(Document.prototype, 'getElementsByTagName', {
        writable: true,
        value: function getElementsByTagName_(qualifiedName) {
            return getElementsByTagName(idOf(this, 'getElementsByTagName'), qualifiedName);
        }
    });

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
    // 名前空間は null / undefined を '' にして bridge へ渡す（bridge が HTML・SVG 以外を Unsupported で拒否する）。
    define(Document.prototype, 'createElementNS', {
        writable: true,
        value: function createElementNS(namespace, qualifiedName) {
            var ns = namespace === null || namespace === undefined ? '' : String(namespace);
            return wrap(op('createElementNS', ns, String(qualifiedName)), Element);
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
    // --- フォーカス追跡（#1173・#1185・JS-5・JS-6・REPAIR-3。方針は冒頭コメント） ---
    // 現在フォーカス中のラッパー（無ければ null）。ページごとに IIFE が作り直されるので状態も消える。
    var focused = null;
    // focus / blur イベントのネスト深さ。リスナーが互いに focus() し合っても無限に再帰しない（JS-6）。
    var MAX_FOCUS_EVENT_DEPTH = 16;
    var focusDepth = 0;
    // events.js が setSink で登録する配送関数（未登録は null。その場合イベントは配らず状態だけ追跡する）。
    var focusSink = null;

    function focusStillValid(el) {
        return op('isFocusable', ids.get(el)) === true;
    }

    // フォーカス中の要素が無効（切り離し・disabled・tabindex 除去等）なら blur を配らずに解除する。
    // DOM・属性を変える各操作の直後に呼び、失効を保持する（後から再接続・属性復元されても復活しない）。
    function revalidateFocus() {
        if (focused !== null && !focusStillValid(focused)) {
            focused = null;
        }
    }

    // node が現在のフォーカス要素自身またはその祖先か（移動でフォーカスが外れるか）。
    // 不正な引数は false を返し、検証エラーは後続の op に任せる。
    function focusInSubtree(node) {
        if (focused === null || !ids.has(node)) {
            return false;
        }
        return focused === node
            || op('isInclusiveAncestor', ids.get(node), ids.get(focused)) === true;
    }

    // 有効なフォーカス中の要素か null（読み取り時の保険として再検証する）。
    function currentFocus() {
        revalidateFocus();
        return focused;
    }

    function fireFocusEvent(type, el) {
        if (focusSink === null || focusDepth >= MAX_FOCUS_EVENT_DEPTH) {
            return;
        }
        focusDepth++;
        try {
            focusSink(type, el);
        } catch (e) {
            // 配送の失敗でフォーカス状態の更新を壊さない。
        } finally {
            focusDepth--;
        }
    }

    // options（preventScroll 等）は受け取るだけ。スクロールが無いので使わない。
    define(Element.prototype, 'focus', {
        writable: true,
        value: function focus(options) {
            if (!ids.has(this)) {
                throw new TypeError('Illegal invocation');
            }
            if (!focusStillValid(this)) {
                return;
            }
            var old = currentFocus();
            if (old === this) {
                return;
            }
            // blur リスナーの中では activeElement は body（Chrome と同じ）。
            focused = null;
            if (old !== null) {
                fireFocusEvent('blur', old);
                // blur リスナーが別の要素をフォーカスしたら、そちらを優先してここで中断する。
                if (focused !== null) {
                    return;
                }
                // blur リスナーが対象を切り離し・disabled にした場合は focus を配らず中断する。
                if (!focusStillValid(this)) {
                    return;
                }
            }
            focused = this;
            fireFocusEvent('focus', this);
        }
    });
    define(Element.prototype, 'blur', {
        writable: true,
        value: function blur() {
            if (!ids.has(this)) {
                throw new TypeError('Illegal invocation');
            }
            if (currentFocus() === this) {
                focused = null;
                fireFocusEvent('blur', this);
            }
        }
    });

    // activeElement: フォーカス中の要素。無ければ body（無ければ null）。react-dom の
    // getActiveElementDeep / getSelectionInformation が読む（#1168・#1173・#1185・JS-5）。
    // 標準プロパティで新しい識別面は足さない（SEC-2）。setter なし。
    define(Document.prototype, 'activeElement', {
        get: function () {
            var c = currentFocus();
            return c !== null ? c : wrap(op('body'), Element);
        }
    });
    define(Document.prototype, 'head', {
        get: function () {
            return wrap(op('head'), Element);
        }
    });
    // documentElement: 文書ルート直下の最初の要素（body / head と同じ層。#1236）。setter なし。
    define(Document.prototype, 'documentElement', {
        get: function () {
            return wrap(op('documentElement'), Element);
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

    // events.js へ渡す一回限りのハンドオフ（focus / blur の配送先の登録のみ。events.js が読んで直ちに削除する）。
    Object.defineProperty(global, '__fandheDomFocusHandoff', {
        value: Object.freeze({
            setSink: function (fn) {
                focusSink = typeof fn === 'function' ? fn : null;
            }
        }),
        writable: true,
        configurable: true,
        enumerable: false
    });

    // stylesheet.js へ渡す一回限りのハンドオフ（`sheet` / `styleSheets` 用。stylesheet.js が読んで直ちに削除する）。
    // ID は外へ出さず、ここで解決した真偽値だけを返す（#1202・JS-5）。
    var protoOfElement = Function.prototype.call.bind(Object.prototype.isPrototypeOf, Element.prototype);
    Object.defineProperty(global, '__fandheDomStyleSheetHandoff', {
        value: Object.freeze({
            // HTML 名前空間の `<style>` 要素か（Element 以外・ID の無いものは false）。
            isHtmlStyleElement: function (node) {
                return ids.has(node) && protoOfElement(node) && localNameOf(node) === 'style';
            },
            // 文書に接続されているか。
            isConnected: function (node) {
                return ids.has(node)
                    && op('isInclusiveAncestor', ids.get(documentWrapper), ids.get(node)) === true;
            }
        }),
        writable: true,
        configurable: true,
        enumerable: false
    });

    expose('Node', Node);
    expose('Element', Element);
    expose('HTMLElement', HTMLElement);
    expose('HTMLIFrameElement', HTMLIFrameElement);
    expose('Text', Text);
    expose('Document', Document);
    documentWrapper = wrap(op('documentRoot'), Document);
    expose('document', documentWrapper);
})(globalThis);
