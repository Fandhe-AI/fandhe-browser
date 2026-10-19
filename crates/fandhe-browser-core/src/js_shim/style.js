// js_shim/style.js: 要素の `style`（最小の CSSStyleDeclaration 相当）の shim
// （TASK-108・Issue #1141・ビヘイビア JS-5・JS-6・SEC-2・REPAIR-3。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle（React DOM 等）が行う CSS 機能検出
// （`document.createElement('div').style` を取って `'animation' in style` のように調べる処理）が、
// `style` が undefined のために例外で止まらないようにする。`element.style` の camelCase 読み書き・
// `setProperty` / `getPropertyValue` / `getPropertyPriority` / `removeProperty` / `cssText` /
// `length` / `item` と、`style` 属性への同期を提供する。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の末尾。observers.js の後に評価）。
// 属性の書き込みは dom.js の `setAttribute`（MutationObserver へ通知するラッパー）を install 時に
// 捕捉して使う。新しい bridge op は足さない。
//
// 設計上の要点:
// - 宣言は要素ごとの順序付きリスト（添字演算と null プロトタイプのコンテナ。配列メソッドは使わない）で持つ。
//   宣言が変わるたびに `style` 属性へ `name: value;`（important は `name: value !important;`）を
//   空白 1 つで連結して書く。1 回の変更につき bridge op は 1 回（ページの op 上限に従う）。
// - 変更は候補モデルを作り、`style` 属性への書き込みが成功してから確定する（DOM が書き込みを
//   拒否して例外になっても、読み取りと実属性が食い違わない）。
// - cssText の宣言リストでは、先の `!important` を後続の通常宣言で上書きしない（`setProperty` は明示更新）。
// - cssText の解析は引用符・括弧の内側を追跡する線形走査で、正規表現のバックトラッキングも
//   eval も使わない。不正な断片は捨てる。単一プロパティへの代入では、トップレベルに `;` `{` `}` を
//   含む値を無視し、別の宣言を紛れ込ませない。
// - `setAttribute` / `removeAttribute` を薄くラップし、ページ JS が `style` 属性を直接書いたときも
//   宣言のリストへ反映する（元の戻り値・例外は変えない）。ページが後から差し替えても同期は
//   install 時に捕捉した真正な関数を通る。
// - 上限（JS-6）: 要素ごとの宣言数 256・値 4096 UTF-16 単位・直列化後の合計 32768 UTF-16 単位。
//   超過分は無視し（例外は投げない）、ページごとに 1 回だけ `consoleMessage` op（warn）で通知する。
// - プロパティ集合（`in` 判定用）はベンダー接頭辞の無い標準名の最小集合を固定リストで持つ。
//   `webkit*` / `Moz*` / `ms*` 等の接頭辞付きの名前は定義しない（`'WebkitAnimation' in style` は
//   false。特定ブラウザの対応可否を装わない。SEC-2）。`profiles/` の CSSOM プロファイル（PLUG-8）は
//   ブラウザ別の識別面に近づくため JS の面には反映しない。
// - ES2015 の範囲に留める（V8 と boa で同一のソース。class / Proxy は使わない）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - `getComputedStyle`・レイアウト（`getBoundingClientRect` 等）・`CSS.supports`・`CSSStyleSheet` /
//   `CSSRule` は定義しない（`typeof getComputedStyle === 'undefined'` のまま。成功を一律に返す
//   偽の実装を作らない）。将来仕様: core の cssom の computed style と連携した `getComputedStyle`
//   （JS-5）。
// - 値の構文検証はしない（書いた値はそのまま読み戻せる。機能検出では「何でも対応」に見える）。
// - 固定リストに無いプロパティ名の `el.style.foo = v` は own データプロパティになり属性へ同期しない
//   （Proxy を使わない制約）。`setProperty` / `cssText` では扱える。添字アクセス（`style[0]`）は未対応。
// - HTML パーサー・`innerHTML` で作られた要素の元の `style` 属性は読む op が無いため基準値が不明。
//   その要素に shim から最初に書くと `style` 属性全体を shim の宣言で置き換える（元の宣言は
//   失われ、読み取りにも出ない）。`getAttribute` 相当の op を足せば解消する。
// - shorthand の展開（`margin` を `margin-top` 等へ）・相対 URL の解決・値の正規化はしない。
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
    var isProtoOf = Function.prototype.call.bind(Object.prototype.isPrototypeOf);
    var toStr = String;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var sCharAt = Function.prototype.call.bind(String.prototype.charAt);
    var sSub = Function.prototype.call.bind(String.prototype.substring);
    var sLower = Function.prototype.call.bind(String.prototype.toLowerCase);
    var sTrim = Function.prototype.call.bind(String.prototype.trim);
    var sIndexOf = Function.prototype.call.bind(String.prototype.indexOf);
    var sLastIndexOf = Function.prototype.call.bind(String.prototype.lastIndexOf);
    var sSplit = Function.prototype.call.bind(String.prototype.split);
    var reTest = Function.prototype.call.bind(RegExp.prototype.test);
    var IDENT = /^-{0,2}[A-Za-z_][A-Za-z0-9_-]*$/;

    var MAX_DECLARATIONS = 256;
    var MAX_VALUE_UNITS = 4096;
    var MAX_TOTAL_UNITS = 32768;

    var Element = global.Element;
    var Document = global.Document;
    if (typeof Element !== 'function' || typeof Document !== 'function') {
        throw new TypeError('dom.js must be evaluated before style.js');
    }
    var origSetAttribute = Element.prototype.setAttribute;
    var origRemoveAttribute = Element.prototype.removeAttribute;
    var origCreateElement = Document.prototype.createElement;

    var TOKEN = {};
    // 宣言オブジェクト → 内部状態 { el, model }。
    var states = new WeakMap();
    // 要素 → 基準の `style` 属性値（shim が知っている範囲。createElement 直後は ''）。
    var baselines = new WeakMap();
    // 要素 → 宣言オブジェクト（同じ要素は常に同じオブジェクトを返す）。
    var declarations = new WeakMap();
    var warned = false;

    function warnLimit() {
        if (warned) {
            return;
        }
        warned = true;
        try {
            callOp('consoleMessage', 'warn', 'style shim limit exceeded: declaration ignored');
        } catch (e) {
            // 診断の失敗でページを止めない。
        }
    }

    // --- 宣言モデル（順序付きリスト。配列メソッドを使わない） ---
    function newModel() {
        return { n: 0, names: [], values: createObject(null), prios: createObject(null), size: 0 };
    }

    function modelIndex(model, name) {
        for (var i = 0; i < model.n; i++) {
            if (model.names[i] === name) {
                return i;
            }
        }
        return -1;
    }

    function declSize(name, value, important) {
        return name.length + value.length + (important ? 12 : 0) + 4;
    }

    // 削除して前の値を返す（無ければ null）。
    function modelRemove(model, name) {
        var at = modelIndex(model, name);
        if (at < 0) {
            return null;
        }
        var prev = model.values[name];
        model.size -= declSize(name, prev, model.prios[name] === true);
        for (var i = at; i < model.n - 1; i++) {
            model.names[i] = model.names[i + 1];
        }
        model.n -= 1;
        model.names[model.n] = undefined;
        delete model.values[name];
        delete model.prios[name];
        return prev;
    }

    // 追加（既存は削除して末尾へ）。上限超過は false（変更なし）。
    function modelPut(model, name, value, important) {
        if (value.length > MAX_VALUE_UNITS) {
            return false;
        }
        var at = modelIndex(model, name);
        var prevSize = at < 0 ? 0 : declSize(name, model.values[name], model.prios[name] === true);
        if (at < 0 && model.n >= MAX_DECLARATIONS) {
            return false;
        }
        if (model.size - prevSize + declSize(name, value, important) > MAX_TOTAL_UNITS) {
            return false;
        }
        if (at >= 0) {
            modelRemove(model, name);
        }
        model.names[model.n] = name;
        model.n += 1;
        model.values[name] = value;
        model.prios[name] = important;
        model.size += declSize(name, value, important);
        return true;
    }

    // 候補モデル用の複製（属性への書き込みが成功してから確定するため。上限により小さい）。
    function cloneModel(model) {
        var copy = newModel();
        for (var i = 0; i < model.n; i++) {
            var name = model.names[i];
            copy.names[i] = name;
            copy.values[name] = model.values[name];
            copy.prios[name] = model.prios[name];
        }
        copy.n = model.n;
        copy.size = model.size;
        return copy;
    }

    function serialize(model) {
        var out = '';
        for (var i = 0; i < model.n; i++) {
            var name = model.names[i];
            if (i > 0) {
                out += ' ';
            }
            out += name + ': ' + model.values[name] + (model.prios[name] === true ? ' !important' : '') + ';';
        }
        return out;
    }

    // CSS では引用符内の未エスケープの改行（LF・CR・FF）で文字列トークンが終わる。
    function isNewline(ch) {
        return ch === '\n' || ch === '\r' || ch === '\f';
    }

    // `\` の直後が CRLF なら 2 文字ぶん（エスケープされた改行 1 つ）進める。
    function escapeSkip(text, i) {
        return sCharAt(text, i + 1) === '\r' && sCharAt(text, i + 2) === '\n' ? 2 : 1;
    }

    // 引用符の外の CSS コメント（`/* ... */`。閉じが無ければ末尾まで）を空白 1 つに置き換える。
    // 引用符内とエスケープ（`\` + 1 文字）はそのまま残す。コメント内の `;` や括弧を
    // 宣言境界・括弧深さに数えないための前処理で、cssText と基準属性の解析前に必ず通す。
    function stripComments(text) {
        if (sIndexOf(text, '/*') < 0) {
            return text;
        }
        var out = '';
        var quote = '';
        var len = text.length;
        for (var i = 0; i < len; i++) {
            var ch = sCharAt(text, i);
            if (ch === '\\' && i + 1 < len) {
                var skip = escapeSkip(text, i);
                out += sSub(text, i, i + 1 + skip);
                i += skip;
            } else if (quote !== '') {
                out += ch;
                if (ch === quote || isNewline(ch)) {
                    quote = '';
                }
            } else if (ch === '/' && sCharAt(text, i + 1) === '*') {
                var close = sIndexOf(text, '*/', i + 2);
                out += ' ';
                if (close < 0) {
                    break;
                }
                i = close + 1;
            } else {
                if (ch === '"' || ch === "'") {
                    quote = ch;
                }
                out += ch;
            }
        }
        return out;
    }

    // トップレベル（引用符・括弧の外）に `;` `{` `}` を含むと無効。引用符の外のコメント開始 `/*` と
    // 末尾の孤立した `\`（直列化の `;` をエスケープしてしまう）も、実属性との解釈の食い違いを
    // 避けるため安全側に倒して無効にする。
    function valueIsSafe(value) {
        var quote = '';
        var depth = 0;
        var len = value.length;
        for (var i = 0; i < len; i++) {
            var ch = sCharAt(value, i);
            if (ch === '\\') {
                if (i + 1 >= len) {
                    return false;
                }
                i += escapeSkip(value, i);
            } else if (quote !== '') {
                if (isNewline(ch)) {
                    // 未エスケープの改行で文字列が終わり、後続が別宣言として解釈され得るため無効。
                    return false;
                }
                if (ch === quote) {
                    quote = '';
                }
            } else if (ch === '"' || ch === "'") {
                quote = ch;
            } else if (ch === '/' && sCharAt(value, i + 1) === '*') {
                return false;
            } else if (ch === '(') {
                depth += 1;
            } else if (ch === ')') {
                if (depth > 0) {
                    depth -= 1;
                }
            } else if (depth === 0 && (ch === ';' || ch === '{' || ch === '}')) {
                return false;
            } else if (depth === 0 && ch === '!' && sLower(sTrim(sSub(value, i + 1))) === 'important') {
                // 値内のトップレベル `!important` は優先度指定。実属性だけ重要宣言になりモデルの優先度と
                // 食い違うため無効にする（重要宣言は setProperty の priority 引数でのみ指定する）。
                return false;
            }
        }
        // 閉じていない括弧は、後続の宣言を未完の関数へ取り込み、モデルと実属性をずらすため無効にする。
        return quote === '' && depth === 0;
    }

    function normalizeName(raw) {
        var s = sTrim(toStr(raw));
        if (s.length >= 2 && sCharAt(s, 0) === '-' && sCharAt(s, 1) === '-') {
            return s;
        }
        return sLower(s);
    }

    // `value` の末尾 `!important` を分離する。{ value, important }。
    function splitPriority(value) {
        var bang = sLastIndexOf(value, '!');
        if (bang >= 0 && sLower(sTrim(sSub(value, bang + 1))) === 'important') {
            return { value: sTrim(sSub(value, 0, bang)), important: true };
        }
        return { value: value, important: false };
    }

    // 1 宣言 "name: value" をモデルへ入れる。無効・上限超過は無視（上限超過は警告）。
    function parseDeclaration(model, piece) {
        var colon = sIndexOf(piece, ':');
        if (colon <= 0) {
            return;
        }
        var name = normalizeName(sSub(piece, 0, colon));
        if (!reTest(IDENT, name)) {
            return;
        }
        var split = splitPriority(sTrim(sSub(piece, colon + 1)));
        if (split.value === '' || !valueIsSafe(split.value)) {
            return;
        }
        // 宣言リスト内では、既存の important を後続の通常宣言で上書きしない（CSS のカスケード）。
        if (!split.important && model.prios[name] === true) {
            return;
        }
        if (!modelPut(model, name, split.value, split.important)) {
            warnLimit();
        }
    }

    // cssText を線形走査で宣言に分け、モデルへ入れる。
    function parseInto(model, raw) {
        var text = stripComments(raw);
        var len = text.length;
        var start = 0;
        var quote = '';
        var depth = 0;
        for (var i = 0; i < len; i++) {
            var ch = sCharAt(text, i);
            if (ch === '\\') {
                i += escapeSkip(text, i);
            } else if (quote !== '') {
                if (ch === quote || isNewline(ch)) {
                    quote = '';
                }
            } else if (ch === '"' || ch === "'") {
                quote = ch;
            } else if (ch === '(') {
                depth += 1;
            } else if (ch === ')') {
                if (depth > 0) {
                    depth -= 1;
                }
            } else if (ch === ';' && depth === 0) {
                parseDeclaration(model, sSub(text, start, i));
                start = i + 1;
            }
        }
        if (start < len) {
            parseDeclaration(model, sSub(text, start, len));
        }
    }

    // 上限超過は null（正常な空宣言の空モデルと区別する。呼び出し側が既存を保持するか空に倒す）。
    function parseText(text) {
        var model = newModel();
        if (text.length > MAX_TOTAL_UNITS) {
            warnLimit();
            return null;
        }
        parseInto(model, text);
        return model;
    }

    // 候補モデルを `style` 属性へ書き（dom.js の setAttribute 経由。MutationObserver へも通知される）、
    // 成功してからだけ state.model に確定する。書き込みが例外で拒否されたら state は変えない。
    function commit(state, candidate) {
        fnCall(origSetAttribute, state.el, 'style', serialize(candidate));
        state.model = candidate;
    }

    function stateOf(self) {
        var state = wmGet(states, self);
        if (state === undefined) {
            throw new TypeError('Illegal invocation');
        }
        return state;
    }

    // --- CSSStyleDeclaration ---
    function CSSStyleDeclaration(token) {
        if (token !== TOKEN) {
            throw new TypeError('Illegal constructor');
        }
    }

    function methodDefine(name, fn) {
        defineProperty(CSSStyleDeclaration.prototype, name, {
            value: fn,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    function getterDefine(name, get, set) {
        var desc = { get: get, configurable: true, enumerable: false };
        if (set) {
            desc.set = set;
        }
        defineProperty(CSSStyleDeclaration.prototype, name, desc);
    }

    function readValue(state, name) {
        var v = state.model.values[name];
        return v === undefined ? '' : v;
    }

    function writeValue(state, name, value, important) {
        var next = cloneModel(state.model);
        if (value === '') {
            if (modelRemove(next, name) !== null) {
                commit(state, next);
            }
            return;
        }
        if (!valueIsSafe(value)) {
            return;
        }
        if (!modelPut(next, name, value, important)) {
            warnLimit();
            return;
        }
        commit(state, next);
    }

    function valueArg(v) {
        return v === null || v === undefined ? '' : sTrim(toStr(v));
    }

    methodDefine('getPropertyValue', function getPropertyValue(name) {
        return readValue(stateOf(this), normalizeName(name));
    });
    methodDefine('getPropertyPriority', function getPropertyPriority(name) {
        var state = stateOf(this);
        // 名前の文字列化で style が更新されうるため、先に文字列化してから最新のモデルを参照する。
        var n = normalizeName(name);
        return state.model.prios[n] === true ? 'important' : '';
    });
    methodDefine('setProperty', function setProperty(name, value, priority) {
        var state = stateOf(this);
        var n = normalizeName(name);
        if (!reTest(IDENT, n)) {
            return;
        }
        var prio = priority === null || priority === undefined ? '' : sLower(toStr(priority));
        if (prio !== '' && prio !== 'important') {
            return;
        }
        writeValue(state, n, valueArg(value), prio === 'important');
    });
    methodDefine('removeProperty', function removeProperty(name) {
        var state = stateOf(this);
        // 引数の文字列化で style が更新されうるため、先に文字列化してから最新のモデルを複製する。
        var n = normalizeName(name);
        var next = cloneModel(state.model);
        var prev = modelRemove(next, n);
        if (prev === null) {
            return '';
        }
        commit(state, next);
        return prev;
    });
    methodDefine('item', function item(index) {
        var state = stateOf(this);
        var i = Number(index);
        if (i >= 0 && i < state.model.n && i === Math.floor(i)) {
            return state.model.names[i];
        }
        return '';
    });
    getterDefine('length', function () {
        return stateOf(this).model.n;
    });
    getterDefine('parentRule', function () {
        stateOf(this);
        return null;
    });
    function setCssText(state, value) {
        var parsed = parseText(valueArg(value));
        if (parsed !== null) {
            commit(state, parsed);
        }
    }
    getterDefine(
        'cssText',
        function () {
            return serialize(stateOf(this).model);
        },
        function (value) {
            setCssText(stateOf(this), value);
        }
    );

    // ベンダー接頭辞の無い標準プロパティの最小集合（ダッシュ形）。接頭辞付きは意図して含めない（SEC-2）。
    var STANDARD_PROPERTIES = sSplit(
        'align-content align-items align-self all animation animation-delay animation-direction ' +
            'animation-duration animation-fill-mode animation-iteration-count animation-name ' +
            'animation-play-state animation-timing-function appearance aspect-ratio backface-visibility ' +
            'background background-attachment background-clip background-color background-image ' +
            'background-origin background-position background-repeat background-size border ' +
            'border-bottom border-bottom-color border-bottom-left-radius border-bottom-right-radius ' +
            'border-bottom-style border-bottom-width border-collapse border-color border-left ' +
            'border-left-color border-left-style border-left-width border-radius border-right ' +
            'border-right-color border-right-style border-right-width border-spacing border-style ' +
            'border-top border-top-color border-top-left-radius border-top-right-radius ' +
            'border-top-style border-top-width border-width bottom box-shadow box-sizing caret-color ' +
            'clear clip clip-path color column-count column-gap content cursor direction display ' +
            'fill filter flex flex-basis flex-direction flex-flow flex-grow flex-shrink flex-wrap ' +
            'float font font-family font-size font-style font-variant font-weight gap grid ' +
            'grid-area grid-auto-columns grid-auto-flow grid-auto-rows grid-column grid-gap grid-row ' +
            'grid-template grid-template-areas grid-template-columns grid-template-rows height ' +
            'inset justify-content justify-items justify-self left letter-spacing line-height ' +
            'list-style list-style-type margin margin-bottom margin-left margin-right margin-top ' +
            'max-height max-width min-height min-width object-fit object-position opacity order ' +
            'outline outline-color outline-offset outline-style outline-width overflow overflow-wrap ' +
            'overflow-x overflow-y padding padding-bottom padding-left padding-right padding-top ' +
            'perspective pointer-events position resize right row-gap scroll-behavior stroke ' +
            'table-layout text-align text-decoration text-indent text-overflow text-shadow ' +
            'text-transform top touch-action transform transform-origin transform-style transition ' +
            'transition-delay transition-duration transition-property transition-timing-function ' +
            'unicode-bidi user-select vertical-align visibility white-space width will-change ' +
            'word-break word-spacing word-wrap z-index',
        ' '
    );

    function camelOf(dashed) {
        var out = '';
        var up = false;
        for (var i = 0; i < dashed.length; i++) {
            var ch = sCharAt(dashed, i);
            if (ch === '-') {
                up = true;
            } else if (up) {
                out += ch.toUpperCase();
                up = false;
            } else {
                out += ch;
            }
        }
        return out;
    }

    function defineStandard(jsName, cssName) {
        getterDefine(
            jsName,
            function () {
                return readValue(stateOf(this), cssName);
            },
            function (value) {
                writeValue(stateOf(this), cssName, valueArg(value), false);
            }
        );
    }

    for (var pi = 0; pi < STANDARD_PROPERTIES.length; pi++) {
        var cssName = STANDARD_PROPERTIES[pi];
        var jsName = camelOf(cssName);
        defineStandard(jsName, cssName);
        if (jsName !== cssName) {
            defineStandard(cssName, cssName);
        }
        if (cssName === 'float') {
            defineStandard('cssFloat', cssName);
        }
    }

    // --- Element.prototype.style ---
    var elementProtoIsPrototypeOf = function (x) {
        return isProtoOf(Element.prototype, x);
    };

    function declarationFor(el) {
        if (!elementProtoIsPrototypeOf(el)) {
            throw new TypeError('Illegal invocation');
        }
        var decl = wmGet(declarations, el);
        if (decl === undefined) {
            decl = createObject(CSSStyleDeclaration.prototype);
            var base = wmGet(baselines, el);
            wmSet(states, decl, { el: el, model: base === undefined ? newModel() : parseText(base) || newModel() });
            wmSet(declarations, el, decl);
        }
        return decl;
    }

    defineProperty(Element.prototype, 'style', {
        get: function () {
            return declarationFor(this);
        },
        set: function (value) {
            setCssText(stateOf(declarationFor(this)), value);
        },
        configurable: true,
        enumerable: false
    });

    // ページ JS が `style` 属性を直接書いたとき、宣言のリストへ反映する（戻り値・例外は変えない）。
    function noteStyleAttribute(el, text) {
        if (!elementProtoIsPrototypeOf(el)) {
            return;
        }
        wmSet(baselines, el, text);
        var decl = wmGet(declarations, el);
        if (decl !== undefined) {
            stateOf(decl).model = parseText(text) || newModel();
        }
    }

    defineProperty(Element.prototype, 'setAttribute', {
        value: function setAttribute(name, value) {
            // 引数は一度だけ文字列化し、元メソッドと同期処理へ同じ文字列を渡す。
            var attrName = toStr(name);
            var attrValue = toStr(value);
            fnCall(origSetAttribute, this, attrName, attrValue);
            if (sLower(sTrim(attrName)) === 'style') {
                noteStyleAttribute(this, attrValue);
            }
        },
        writable: true,
        configurable: true,
        enumerable: false
    });
    defineProperty(Element.prototype, 'removeAttribute', {
        value: function removeAttribute(name) {
            var attrName = toStr(name);
            fnCall(origRemoveAttribute, this, attrName);
            if (sLower(sTrim(attrName)) === 'style') {
                noteStyleAttribute(this, '');
            }
        },
        writable: true,
        configurable: true,
        enumerable: false
    });
    // 新規要素の `style` 属性は空と分かっている。
    defineProperty(Document.prototype, 'createElement', {
        value: function createElement(name) {
            var el = fnCall(origCreateElement, this, name);
            if (el !== null && el !== undefined) {
                wmSet(baselines, el, '');
            }
            return el;
        },
        writable: true,
        configurable: true,
        enumerable: false
    });

    defineProperty(global, 'CSSStyleDeclaration', {
        value: CSSStyleDeclaration,
        writable: true,
        configurable: true,
        enumerable: false
    });
})(globalThis);
