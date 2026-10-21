// js_shim/stylesheet.js: `<style>` 要素の `sheet` と `document.styleSheets` の最小の shim
// （Issue #1202・ビヘイビア JS-5・JS-6・REPAIR-3・SEC-2。TASK-108・TASK-71）。
//
// 役割: d3（DemoQA）のバンドル内の emotion（`@emotion/sheet`）が、`<style>` 要素の `sheet` と
// `document.styleSheets.length` を読み、`sheet.insertRule(rule, sheet.cssRules.length)` で
// ルールを足す経路（speedy モード）を、例外なしで通すための最小の面を提供する。
// 呼び出し元: core の `js_shim::install` が style.js の直後に評価する。
//
// 設計上の要点:
// - ノード ID は扱わない。dom.js が install 中だけ公開する一時グローバル
//   `__fandheDomStyleSheetHandoff`（読んですぐ削除）の窓口 `isHtmlStyleElement` /
//   `isConnected` を使い、ID は dom.js の内側で解決する。新しい bridge op は足さない。
// - `Element.prototype.sheet`（getter）: 接続済みの HTML `<style>` は要素ごとに同じ
//   シートオブジェクトを返し、未接続の HTML `<style>` は null、それ以外の要素は undefined。
//   HTMLStyleElement の具象サブクラスが無いため Element.prototype に置く（`'sheet' in div` が
//   true になる点は標準と異なる）。アクセスごとに op を最大 2 回消費する（初回の `htmlLocalName`
//   と毎回の `isInclusiveAncestor`）。ルールを数万件入れる用途ではページの op 予算を圧迫し得る。
// - `cssRules` はシートごとに 1 つの持続オブジェクトで、insert / delete で中身を更新する
//   （emotion は insert のたびに `cssRules.length` を読むため、作り直すと O(N^2) になる）。
// - `insertRule` は 1 つのルールであることだけを構造的に検査する（引用符・エスケープ・
//   コメント・括弧を追跡する線形走査。正規表現も eval も使わない）。文字列は保持して読み戻す
//   だけで、評価・取得（`@import` を含む）はしない。
// - 上限（JS-6）: シートごとのルール数 65536・1 ルール 65536 UTF-16 単位・シートの合計
//   4 MiB（UTF-16 単位）。超過は成功を装わず `QuotaExceededError`（DOMException）を投げ、
//   ページごとに 1 回だけ `consoleMessage` op（warn）で通知する。長さは走査より前に判定する。
// - グローバルの `CSSStyleSheet` / `StyleSheetList` / `CSSRuleList` / `CSSRule` は公開しない
//   （constructable stylesheet の機能検出を変えない。特定ブラウザに見せかけない。SEC-2）。
// - ES2015 の範囲に留める（V8 と boa で同一のソース。class / Proxy は使わない）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - `cssRules` は空から始まり、`<style>` のテキスト内容・後から追加されたテキストノード・
//   HTML パーサーで作られた style 要素の内容を反映しない。将来は core の
//   `cssom::split_rule_blocks` と連携した初期化を検討する。
// - `insertRule` で入れたルールは core の Rust CSSOM（computed style）にも DOM の直列化にも
//   反映されない。構文・セレクタ・宣言の妥当性は検証しない（ブラウザなら拒否する入力も通る）。
// - `<link rel="stylesheet">`・`@import` のシート、SVG の `<style>`、Shadow DOM のシートは
//   `styleSheets` に含めない。再接続時にシートを作り直さず同じオブジェクトを使い続ける。
// - `media`・`title`・`disabled` の効果、`rules` / `addRule` / `removeRule`、`replace` /
//   `replaceSync`、CSSRule の種別ごとの面（`type`・`selectorText`・`style`）、
//   `adoptedStyleSheets`、live な StyleSheetList（`styleSheets` はアクセスごとの静的な一覧）。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var callOp = Function.prototype.call.bind(bridge.op, bridge);
    var handoff = global.__fandheDomStyleSheetHandoff;
    if (!handoff || typeof handoff.isHtmlStyleElement !== 'function') {
        throw new TypeError('dom.js must be evaluated before stylesheet.js');
    }
    try {
        delete global.__fandheDomStyleSheetHandoff;
    } catch (e) {
        // 削除できなくても窓口は凍結済みで ID は出ない。
    }
    var isHtmlStyleElement = handoff.isHtmlStyleElement;
    var isConnected = handoff.isConnected;

    var Element = global.Element;
    var Document = global.Document;
    var NativeDOMException = global.DOMException;
    if (typeof Element !== 'function' || typeof Document !== 'function') {
        throw new TypeError('dom.js must be evaluated before stylesheet.js');
    }
    if (typeof NativeDOMException !== 'function') {
        throw new TypeError('abort.js must be evaluated before stylesheet.js');
    }
    var callGebt = Function.prototype.call.bind(Document.prototype.getElementsByTagName);
    var defineProperty = Object.defineProperty;
    var createObject = Object.create;
    var isProtoOf = Function.prototype.call.bind(Object.prototype.isPrototypeOf);
    var toStr = String;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var sCharCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
    var aPop = Function.prototype.call.bind(Array.prototype.pop);
    var sTrim = Function.prototype.call.bind(String.prototype.trim);
    var sIndexOf = Function.prototype.call.bind(String.prototype.indexOf);
    var sSub = Function.prototype.call.bind(String.prototype.substring);
    var ElementProto = Element.prototype;
    var DocumentProto = Document.prototype;

    var MAX_RULES = 65536;
    var MAX_RULE_UNITS = 65536;
    var MAX_TOTAL_UNITS = 4 * 1024 * 1024;

    // シートオブジェクト → { el, rules, total, list, disabled }。
    var sheetStates = new WeakMap();
    // 要素 → シート（同じ要素は常に同じオブジェクト）。
    var sheetsByElement = new WeakMap();
    // ルールオブジェクト → { text, sheet, charge }。
    var ruleStates = new WeakMap();
    var warned = false;

    function warnLimit() {
        if (warned) {
            return;
        }
        warned = true;
        try {
            callOp('consoleMessage', 'warn', 'stylesheet shim limit exceeded: rule rejected', '');
        } catch (e) {
            // 診断の失敗でページを止めない。
        }
    }

    function domError(message, name) {
        return new NativeDOMException(message, name);
    }

    function illegal() {
        return new TypeError('Illegal invocation');
    }

    function isSpace(c) {
        return c === 32 || c === 9 || c === 10 || c === 13 || c === 12;
    }

    // 1 つのルールであることだけを見る構造検査（構文・セレクタ・宣言は検証しない）。
    function isSingleRule(text) {
        var n = text.length;
        var depth = 0;
        var paren = 0;
        var blocks = 0;
        var atDone = false;
        var prelude = false;
        var isAt = false;
        var done = false;
        var i = 0;
        while (i < n) {
            var c = sCharCodeAt(text, i);
            // コメント。
            if (c === 47 && i + 1 < n && sCharCodeAt(text, i + 1) === 42) {
                i += 2;
                while (i + 1 < n && !(sCharCodeAt(text, i) === 42 && sCharCodeAt(text, i + 1) === 47)) {
                    i++;
                }
                if (i + 1 >= n) {
                    return false;
                }
                i += 2;
                continue;
            }
            if (isSpace(c)) {
                i++;
                continue;
            }
            if (done) {
                return false;
            }
            if (c === 34 || c === 39) {
                // 文字列（バックスラッシュは次の 1 文字を飛ばす）。
                if (depth === 0) {
                    prelude = true;
                }
                i++;
                var closed = false;
                while (i < n) {
                    var d = sCharCodeAt(text, i);
                    if (d === 92) {
                        i += 2;
                        continue;
                    }
                    i++;
                    if (d === c) {
                        closed = true;
                        break;
                    }
                }
                if (!closed) {
                    return false;
                }
                continue;
            }
            if (c === 92) {
                if (depth === 0) {
                    prelude = true;
                }
                i += 2;
                continue;
            }
            if (c === 40 || c === 91) {
                paren++;
            } else if (c === 41 || c === 93) {
                if (paren > 0) {
                    paren--;
                }
            } else if (c === 123) {
                if (depth === 0 && !prelude) {
                    return false;
                }
                depth++;
                i++;
                continue;
            } else if (c === 125) {
                if (depth === 0) {
                    return false;
                }
                depth--;
                if (depth === 0) {
                    blocks++;
                    done = true;
                }
                i++;
                continue;
            } else if (c === 59 && depth === 0 && paren === 0) {
                if (isAt && prelude && blocks === 0) {
                    atDone = true;
                    done = true;
                    i++;
                    continue;
                }
                return false;
            }
            if (depth === 0) {
                if (!prelude && c === 64) {
                    isAt = true;
                }
                prelude = true;
            }
            i++;
        }
        if (depth !== 0) {
            return false;
        }
        if (blocks === 1) {
            return true;
        }
        // 末尾の `;` なしのブロックなし at-rule（`@import url(x.css)`）。
        return blocks === 0 && (atDone || (isAt && prelude));
    }

    // JS の trim が除くが CSS 空白ではない文字（NBSP・VT・各種 Unicode 空白・BOM）。
    var EXTRA_JS_SPACES = [
        '\u00a0', '\u000b', '\u1680', '\u2000', '\u2001', '\u2002', '\u2003', '\u2004', '\u2005',
        '\u2006', '\u2007', '\u2008', '\u2009', '\u200a', '\u2028', '\u2029', '\u202f', '\u205f',
        '\u3000', '\ufeff'
    ];

    // JS の trim が除いた部分（JS 空白だけで構成される）が CSS 空白だけかを、ネイティブの
    // indexOf だけで判定する（CSS 空白でない JS 空白を含まなければ全て CSS 空白）。
    function isAllCssSpace(str) {
        for (var k = 0; k < EXTRA_JS_SPACES.length; k++) {
            if (sIndexOf(str, EXTRA_JS_SPACES[k]) !== -1) {
                return false;
            }
        }
        return true;
    }

    // 前後の CSS 空白（SP・TAB・LF・CR・FF）を除く。長い空白でも走査が軽くなるよう、まず
    // ネイティブの trim で候補を作り、除かれた部分に CSS 空白でない JS 空白が無いと確認できたときに採用する
    // （JS の trim は NBSP 等も除くが、CSS では識別子の一部になり得るため）。確認できなければ
    // 1 文字ずつの走査に戻る。
    function trimCss(text) {
        var n = text.length;
        if (n === 0 || (sCharCodeAt(text, 0) > 32 && sCharCodeAt(text, n - 1) > 32)) {
            return text;
        }
        var t = sTrim(text);
        if (t.length === text.length) {
            return text;
        }
        var lead = t.length === 0 ? text.length : sIndexOf(text, t);
        if (isAllCssSpace(sSub(text, 0, lead)) && isAllCssSpace(sSub(text, lead + t.length, text.length))) {
            return t;
        }
        var s = 0;
        var e = text.length;
        while (s < e && isSpace(sCharCodeAt(text, s))) {
            s++;
        }
        while (e > s && isSpace(sCharCodeAt(text, e - 1))) {
            e--;
        }
        return sSub(text, s, e);
    }

    // --- CSSRule 相当 ---
    function CssRuleLike() {}
    var RuleProto = CssRuleLike.prototype;
    defineProperty(RuleProto, 'cssText', {
        get: function () {
            var st = wmGet(ruleStates, this);
            if (st === undefined) {
                throw illegal();
            }
            return st.text;
        },
        enumerable: true,
        configurable: true
    });
    defineProperty(RuleProto, 'parentStyleSheet', {
        get: function () {
            var st = wmGet(ruleStates, this);
            if (st === undefined) {
                throw illegal();
            }
            return st.sheet;
        },
        enumerable: true,
        configurable: true
    });

    function itemOf(list, index) {
        var i = index >>> 0;
        var v = i < list.length ? list[i] : undefined;
        return v === undefined ? null : v;
    }

    // --- CSSRuleList 相当（シートごとに持続。length と添字は own データプロパティ） ---
    function CssRuleListLike() {}
    defineProperty(CssRuleListLike.prototype, 'item', {
        value: function item(index) {
            return itemOf(this, index);
        },
        writable: true,
        configurable: true
    });

    function setLength(list, n) {
        defineProperty(list, 'length', { value: n, writable: false, enumerable: false, configurable: true });
    }

    // length は rules の件数を読む own アクセサ（挿入のたびに定義し直さない。set は無く読み取り専用）。
    function defineLength(list, rules) {
        defineProperty(list, 'length', {
            get: function () {
                return rules.length;
            },
            enumerable: false,
            configurable: true
        });
    }

    function syncList(st, from, oldLen) {
        var list = st.list;
        var rules = st.rules;
        for (var i = from; i < rules.length; i++) {
            defineProperty(list, i, { value: rules[i], writable: false, enumerable: true, configurable: true });
        }
        for (var j = rules.length; j < oldLen; j++) {
            delete list[j];
        }
    }

    // --- CSSStyleSheet 相当 ---
    function CssStyleSheetLike() {}
    var SheetProto = CssStyleSheetLike.prototype;

    function stateOf(self) {
        var st = wmGet(sheetStates, self);
        if (st === undefined) {
            throw illegal();
        }
        return st;
    }

    defineProperty(SheetProto, 'cssRules', {
        get: function () {
            return stateOf(this).list;
        },
        enumerable: true,
        configurable: true
    });
    defineProperty(SheetProto, 'ownerNode', {
        get: function () {
            return stateOf(this).el;
        },
        enumerable: true,
        configurable: true
    });
    function constGetter(name, value) {
        defineProperty(SheetProto, name, {
            get: function () {
                stateOf(this);
                return value;
            },
            enumerable: true,
            configurable: true
        });
    }
    constGetter('type', 'text/css');
    constGetter('href', null);
    constGetter('parentStyleSheet', null);
    constGetter('ownerRule', null);
    // `disabled` は代入値を保持するだけで効果は無い（REPAIR-3）。
    defineProperty(SheetProto, 'disabled', {
        get: function () {
            return stateOf(this).disabled;
        },
        set: function (v) {
            stateOf(this).disabled = !!v;
        },
        enumerable: true,
        configurable: true
    });

    defineProperty(SheetProto, 'insertRule', {
        value: function insertRule(rule) {
            var st = stateOf(this);
            if (arguments.length < 1) {
                throw new TypeError(
                    "Failed to execute 'insertRule' on 'CSSStyleSheet': 1 argument required, but only 0 present."
                );
            }
            var index = arguments.length > 1 ? arguments[1] >>> 0 : 0;
            var text = toStr(rule);
            var oldLen = st.rules.length;
            if (index > oldLen) {
                throw domError(
                    "Failed to execute 'insertRule' on 'CSSStyleSheet': the index " + index + ' is greater than the length.',
                    'IndexSizeError'
                );
            }
            // 長さ・件数は走査より前に判定する（JS-6）。
            if (text.length > MAX_RULE_UNITS || oldLen >= MAX_RULES || st.total + text.length > MAX_TOTAL_UNITS) {
                warnLimit();
                throw domError(
                    "Failed to execute 'insertRule' on 'CSSStyleSheet': the quota has been exceeded.",
                    'QuotaExceededError'
                );
            }
            // 前後の空白は構造検査に影響しないため、trim 後を走査する（長い空白の走査を軽くする）。
            var trimmed = trimCss(text);
            if (!isSingleRule(trimmed)) {
                throw domError(
                    "Failed to execute 'insertRule' on 'CSSStyleSheet': Failed to parse the rule.",
                    'SyntaxError'
                );
            }
            var ruleObj = createObject(RuleProto);
            wmSet(ruleStates, ruleObj, { text: trimmed, sheet: this, charge: text.length });
            // 末尾追加は O(1)、途中挿入だけ後ろをずらす。
            var rules = st.rules;
            rules[oldLen] = ruleObj;
            for (var i = oldLen; i > index; i--) {
                rules[i] = rules[i - 1];
            }
            rules[index] = ruleObj;
            st.total += text.length;
            if (index === oldLen) {
                defineProperty(st.list, oldLen, { value: ruleObj, writable: false, enumerable: true, configurable: true });
            } else {
                syncList(st, index, oldLen);
            }
            return index;
        },
        writable: true,
        configurable: true
    });

    defineProperty(SheetProto, 'deleteRule', {
        value: function deleteRule(index) {
            var st = stateOf(this);
            var at = index >>> 0;
            var oldLen = st.rules.length;
            if (at >= oldLen) {
                throw domError(
                    "Failed to execute 'deleteRule' on 'CSSStyleSheet': the index " + at + ' is out of range.',
                    'IndexSizeError'
                );
            }
            var rst = wmGet(ruleStates, st.rules[at]);
            if (rst !== undefined) {
                // 挿入時に計上した文字数（trim 前）をそのまま返す。
                st.total -= rst.charge;
                // 保持済みの削除済みルール参照から親シートを外す。
                rst.sheet = null;
            }
            for (var i = at; i < oldLen - 1; i++) {
                st.rules[i] = st.rules[i + 1];
            }
            aPop(st.rules);
            syncList(st, at, oldLen);
        },
        writable: true,
        configurable: true
    });

    function sheetFor(el) {
        var s = wmGet(sheetsByElement, el);
        if (s !== undefined) {
            return s;
        }
        s = createObject(SheetProto);
        var list = createObject(CssRuleListLike.prototype);
        var rules = [];
        defineLength(list, rules);
        wmSet(sheetStates, s, { el: el, rules: rules, total: 0, list: list, disabled: false });
        wmSet(sheetsByElement, el, s);
        return s;
    }

    defineProperty(ElementProto, 'sheet', {
        get: function () {
            if (!isProtoOf(ElementProto, this)) {
                throw illegal();
            }
            if (!isHtmlStyleElement(this)) {
                return undefined;
            }
            return isConnected(this) ? sheetFor(this) : null;
        },
        enumerable: true,
        configurable: true
    });

    // --- StyleSheetList 相当（アクセスごとの静的な一覧） ---
    function StyleSheetListLike() {}
    defineProperty(StyleSheetListLike.prototype, 'item', {
        value: function item(index) {
            return itemOf(this, index);
        },
        writable: true,
        configurable: true
    });

    defineProperty(DocumentProto, 'styleSheets', {
        get: function () {
            if (!isProtoOf(DocumentProto, this)) {
                throw illegal();
            }
            var els = callGebt(this, 'style');
            var list = createObject(StyleSheetListLike.prototype);
            var n = 0;
            for (var i = 0; i < els.length; i++) {
                var el = els[i];
                if (isHtmlStyleElement(el) && isConnected(el)) {
                    defineProperty(list, n, { value: sheetFor(el), writable: false, enumerable: true, configurable: true });
                    n++;
                }
            }
            setLength(list, n);
            return list;
        },
        enumerable: true,
        configurable: true
    });
})(globalThis);
