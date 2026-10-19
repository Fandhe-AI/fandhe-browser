// js_shim/url.js: `URL` / `URLSearchParams` の shim
// （TASK-108・Issue #1144・ビヘイビア JS-5・JS-6・SEC-2・REPAIR-3。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページの bundle が使うグローバルの `URL` / `URLSearchParams`（`new URL(href, base)`・
// `url.searchParams.get(...)` 等）が `ReferenceError` で止まらないようにする。WHATWG URL の解析は
// core の bridge op `urlParse`（`reqwest::Url` に委譲。ネットワークには出ない）に任せ、
// `application/x-www-form-urlencoded` の解析と直列化は JS で実装する。
// 呼び出し元: core の `js_shim::install`（`JS_SHIM_SOURCES` の末尾。style.js の後に評価）。install 時には op を呼ばない。
//
// 設計上の要点:
// - 内部状態（11 個の構成要素・キャッシュした URLSearchParams）はクロージャ内の WeakMap で持ち、
//   ページから見えるプロパティにしない。組み込み関数は install 時に捕捉し、ページが後から差し替えても
//   shim は影響を受けない。
// - `urlParse` の戻りは構成要素 11 個を `\n` で連結した 1 文字列、失敗は null（core 側の doc 参照）。
//   op の例外（長さ超過・op 予算超過）も含め、失敗は `TypeError('Invalid URL')` に変換する
//   （`URL.canParse` は false）。メッセージは固定文で、入力 URL を反響しない。
// - setter は「変更後の文字列を op で再解析する」形で済むものだけ実装する: `href`・`search`・`hash`。
//   href を最初の `#` で fragment 部に、残りを最初の `?` で query 部に分けられるのは、WHATWG の
//   直列化が path・userinfo・query で `#` を、path・userinfo で `?` をエンコードするため。
//   `search` の値に含まれる `#` は `%23` に置き換えてから再解析する（WHATWG の state override と同じ結果）。
// - `searchParams` は URL ごとに同じオブジェクトを返し、変更系メソッドの後に URL の query を再解析で
//   更新する（直列化結果が空なら `?` ごと消す）。逆に `href` / `search` の setter は中身を作り直す。
// - form-urlencoded は `decodeURIComponent` / `encodeURIComponent` / `TextDecoder` を使わず、UTF-8 を
//   手書きで符号化・復号する（不正な `%` はそのまま残し、不正な UTF-8 と孤立サロゲートは U+FFFD にする）。
//   解析は線形走査で正規表現のバックトラッキングも eval も使わない。
// - 上限（JS-6）: URLSearchParams は純 JS で、入力文字列長に比例するだけで増幅しない（op を通らないため
//   新しい上限は設けない）。`URL` 経由の入力は op の文字列上限（既定 64 KiB）と応答上限で止まる。
// - ES2015 の範囲に留める（V8 と boa で同一のソース。class / Proxy は使わない）。
//
// 未実装（REPAIR-3。黙って成功扱いにはしない）:
// - setter: `protocol`・`username`・`password`・`host`・`hostname`・`port`・`pathname` は getter だけの
//   アクセサにする（スキームの special / 非 special 切替の制約・ポートの検証・path の state override は
//   再解析では表せない）。strict モードのコードが代入すると TypeError になる。将来仕様: setter 全般の
//   WHATWG 準拠（JS-5）。
// - `URL.createObjectURL` / `URL.revokeObjectURL` / `URLPattern` は定義しない（Blob が無いため。
//   `typeof` は 'undefined' のまま。機能検出で対応を装わない。SEC-2）。将来仕様: Blob 導入後の
//   `createObjectURL`（JS-5）。
// - `URLSearchParams` の `sort`・`size`・`has(name, value)` / `delete(name, value)` の第 2 引数。
// - op の文字列上限（既定 64 KiB）を超える URL は失敗として扱う。
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
    var objectKeys = Object.keys;
    var isProtoOf = Function.prototype.call.bind(Object.prototype.isPrototypeOf);
    var toStr = String;
    var fromCharCode = String.fromCharCode;
    var symIter = Symbol.iterator;
    var wmGet = Function.prototype.call.bind(WeakMap.prototype.get);
    var wmSet = Function.prototype.call.bind(WeakMap.prototype.set);
    var sCharAt = Function.prototype.call.bind(String.prototype.charAt);
    var sCharCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
    var sSub = Function.prototype.call.bind(String.prototype.substring);
    var sIndexOf = Function.prototype.call.bind(String.prototype.indexOf);
    var sSplit = Function.prototype.call.bind(String.prototype.split);
    var arrPush = Function.prototype.call.bind(Array.prototype.push);
    var arrSplice = Function.prototype.call.bind(Array.prototype.splice);
    var arrJoin = Function.prototype.call.bind(Array.prototype.join);

    var HEX = '0123456789ABCDEF';
    // 構成要素の並び（core の `urlParse` op の戻りと同じ順）。
    var NAMES = ['href', 'origin', 'protocol', 'username', 'password', 'host', 'hostname', 'port',
        'pathname', 'search', 'hash'];
    var IDX_HREF = 0;
    var IDX_SEARCH = 9;

    var urlStates = new WeakMap();
    var spStates = new WeakMap();

    // --- UTF-8 / パーセント符号化 ---
    function hexValue(code) {
        if (code >= 48 && code <= 57) {
            return code - 48;
        }
        if (code >= 65 && code <= 70) {
            return code - 55;
        }
        if (code >= 97 && code <= 102) {
            return code - 87;
        }
        return -1;
    }

    // UTF-16 の文字列を走査して、次のコードポイントを返す（孤立サロゲートは U+FFFD）。
    function codePointAt(s, i) {
        var c = sCharCodeAt(s, i);
        if (c >= 0xD800 && c <= 0xDBFF && i + 1 < s.length) {
            var d = sCharCodeAt(s, i + 1);
            if (d >= 0xDC00 && d <= 0xDFFF) {
                return ((c - 0xD800) << 10) + (d - 0xDC00) + 0x10000;
            }
        }
        if (c >= 0xD800 && c <= 0xDFFF) {
            return 0xFFFD;
        }
        return c;
    }

    function pushUtf8(bytes, cp) {
        if (cp < 0x80) {
            arrPush(bytes, cp);
        } else if (cp < 0x800) {
            arrPush(bytes, 0xC0 | (cp >> 6), 0x80 | (cp & 0x3F));
        } else if (cp < 0x10000) {
            arrPush(bytes, 0xE0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3F), 0x80 | (cp & 0x3F));
        } else {
            arrPush(bytes, 0xF0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3F),
                0x80 | ((cp >> 6) & 0x3F), 0x80 | (cp & 0x3F));
        }
    }

    function utf8Decode(bytes) {
        var out = '';
        var i = 0;
        var n = bytes.length;
        while (i < n) {
            var b = bytes[i];
            i++;
            if (b < 0x80) {
                out += fromCharCode(b);
                continue;
            }
            var need;
            var cp;
            var lower = 0x80;
            var upper = 0xBF;
            if (b >= 0xC2 && b <= 0xDF) {
                need = 1;
                cp = b & 0x1F;
            } else if (b >= 0xE0 && b <= 0xEF) {
                if (b === 0xE0) {
                    lower = 0xA0;
                }
                if (b === 0xED) {
                    upper = 0x9F;
                }
                need = 2;
                cp = b & 0x0F;
            } else if (b >= 0xF0 && b <= 0xF4) {
                if (b === 0xF0) {
                    lower = 0x90;
                }
                if (b === 0xF4) {
                    upper = 0x8F;
                }
                need = 3;
                cp = b & 0x07;
            } else {
                out += fromCharCode(0xFFFD);
                continue;
            }
            var ok = true;
            for (var got = 0; got < need; got++) {
                if (i >= n) {
                    ok = false;
                    break;
                }
                var c = bytes[i];
                if (c < lower || c > upper) {
                    ok = false;
                    break;
                }
                lower = 0x80;
                upper = 0xBF;
                cp = (cp << 6) | (c & 0x3F);
                i++;
            }
            if (!ok) {
                out += fromCharCode(0xFFFD);
            } else if (cp < 0x10000) {
                out += fromCharCode(cp);
            } else {
                cp -= 0x10000;
                out += fromCharCode(0xD800 + (cp >> 10), 0xDC00 + (cp & 0x3FF));
            }
        }
        return out;
    }

    // `+` を空白にしてからパーセントデコードする（不正な `%` はそのまま残す）。
    function formDecode(s) {
        var bytes = [];
        var n = s.length;
        var i = 0;
        while (i < n) {
            var c = sCharCodeAt(s, i);
            if (c === 0x2B) {
                arrPush(bytes, 0x20);
                i++;
            } else if (c === 0x25 && i + 2 < n && hexValue(sCharCodeAt(s, i + 1)) >= 0 &&
                    hexValue(sCharCodeAt(s, i + 2)) >= 0) {
                arrPush(bytes, hexValue(sCharCodeAt(s, i + 1)) * 16 + hexValue(sCharCodeAt(s, i + 2)));
                i += 3;
            } else {
                var cp = codePointAt(s, i);
                pushUtf8(bytes, cp);
                i += cp > 0xFFFF ? 2 : 1;
            }
        }
        return utf8Decode(bytes);
    }

    function formEncode(s) {
        var out = '';
        var n = s.length;
        var i = 0;
        while (i < n) {
            var cp = codePointAt(s, i);
            i += cp > 0xFFFF ? 2 : 1;
            if ((cp >= 48 && cp <= 57) || (cp >= 65 && cp <= 90) || (cp >= 97 && cp <= 122) ||
                    cp === 0x2A || cp === 0x2D || cp === 0x2E || cp === 0x5F) {
                out += fromCharCode(cp);
            } else if (cp === 0x20) {
                out += '+';
            } else {
                var bytes = [];
                pushUtf8(bytes, cp);
                for (var k = 0; k < bytes.length; k++) {
                    out += '%' + sCharAt(HEX, bytes[k] >> 4) + sCharAt(HEX, bytes[k] & 15);
                }
            }
        }
        return out;
    }

    // --- application/x-www-form-urlencoded の一覧 ---
    function parseForm(s) {
        var list = [];
        var pieces = sSplit(s, '&');
        for (var i = 0; i < pieces.length; i++) {
            var piece = pieces[i];
            if (piece === '') {
                continue;
            }
            var eq = sIndexOf(piece, '=');
            var name = eq < 0 ? piece : sSub(piece, 0, eq);
            var value = eq < 0 ? '' : sSub(piece, eq + 1);
            arrPush(list, { n: formDecode(name), v: formDecode(value) });
        }
        return list;
    }

    function serializeForm(list) {
        var parts = [];
        for (var i = 0; i < list.length; i++) {
            arrPush(parts, formEncode(list[i].n) + '=' + formEncode(list[i].v));
        }
        return arrJoin(parts, '&');
    }

    function stripFirst(s, ch) {
        return sCharAt(s, 0) === ch ? sSub(s, 1) : s;
    }

    // --- urlParse op の呼び出し ---
    // 成功は 11 要素の配列、失敗（構文エラー・op の例外）は null。
    function parseComponents(input, base) {
        var r;
        try {
            r = callOp('urlParse', input, base === undefined ? null : base);
        } catch (e) {
            return null;
        }
        if (typeof r !== 'string') {
            return null;
        }
        var parts = sSplit(r, '\n');
        return parts.length === NAMES.length ? parts : null;
    }

    // href を [最初の `?` より前, query（無ければ null）, fragment 部（`#` 込み。無ければ ''）] に分ける。
    function splitHref(href) {
        var h = sIndexOf(href, '#');
        var frag = h < 0 ? '' : sSub(href, h);
        var rest = h < 0 ? href : sSub(href, 0, h);
        var q = sIndexOf(rest, '?');
        return {
            head: q < 0 ? rest : sSub(rest, 0, q),
            query: q < 0 ? null : sSub(rest, q + 1),
            frag: frag
        };
    }

    function requireUrl(self) {
        var st = wmGet(urlStates, self);
        if (!st) {
            throw new TypeError('Illegal invocation');
        }
        return st;
    }

    function requireSp(self) {
        var st = wmGet(spStates, self);
        if (!st) {
            throw new TypeError('Illegal invocation');
        }
        return st;
    }

    // 構成要素を差し替え、キャッシュ済みの searchParams の中身を作り直す。
    function applyComponents(st, comps) {
        st.c = comps;
        if (st.sp) {
            st.sp.list = parseForm(stripFirst(comps[IDX_SEARCH], '?'));
        }
    }

    // 変更後の href 文字列を再解析し、成功したときだけ適用する。
    function reparse(st, text) {
        var comps = parseComponents(text, undefined);
        if (comps === null) {
            return false;
        }
        applyComponents(st, comps);
        return true;
    }

    // --- URL ---
    function URL(input, base) {
        if (this === undefined || this === null || !isProtoOf(URL.prototype, this)) {
            throw new TypeError("Failed to construct 'URL': Please use the 'new' operator");
        }
        var comps = parseComponents(toStr(input), base === undefined ? undefined : toStr(base));
        if (comps === null) {
            throw new TypeError('Invalid URL');
        }
        wmSet(urlStates, this, { c: comps, sp: null });
    }

    function defMethod(target, name, fn) {
        defineProperty(target, name, { value: fn, writable: true, configurable: true, enumerable: false });
    }

    function defAccessor(name, index, setter) {
        defineProperty(URL.prototype, name, {
            get: function () {
                return requireUrl(this).c[index];
            },
            set: setter,
            configurable: true,
            enumerable: true
        });
    }

    var SETTERS = {
        href: function (value) {
            var st = requireUrl(this);
            var comps = parseComponents(toStr(value), undefined);
            if (comps === null) {
                throw new TypeError('Invalid URL');
            }
            applyComponents(st, comps);
        },
        search: function (value) {
            var st = requireUrl(this);
            var v = stripFirst(toStr(value), '?');
            var p = splitHref(st.c[IDX_HREF]);
            var query = v === '' ? '' : '?' + arrJoin(sSplit(v, '#'), '%23');
            // 失敗したら変更しない（setter は投げない）。
            reparse(st, p.head + query + p.frag);
        },
        hash: function (value) {
            var st = requireUrl(this);
            var v = stripFirst(toStr(value), '#');
            var href = st.c[IDX_HREF];
            var h = sIndexOf(href, '#');
            var head = h < 0 ? href : sSub(href, 0, h);
            reparse(st, v === '' ? head : head + '#' + v);
        }
    };
    for (var ni = 0; ni < NAMES.length; ni++) {
        defAccessor(NAMES[ni], ni, SETTERS[NAMES[ni]]);
    }
    defineProperty(URL.prototype, 'searchParams', {
        get: function () {
            var st = requireUrl(this);
            if (!st.sp) {
                var sp = createObject(URLSearchParams.prototype);
                var spState = { list: parseForm(stripFirst(st.c[IDX_SEARCH], '?')), url: st };
                wmSet(spStates, sp, spState);
                st.sp = spState;
                st.spObj = sp;
            }
            return st.spObj;
        },
        configurable: true,
        enumerable: true
    });
    defMethod(URL.prototype, 'toString', function () {
        return requireUrl(this).c[IDX_HREF];
    });
    defMethod(URL.prototype, 'toJSON', function () {
        return requireUrl(this).c[IDX_HREF];
    });
    defMethod(URL, 'canParse', function (input, base) {
        return parseComponents(toStr(input), base === undefined ? undefined : toStr(base)) !== null;
    });

    // --- URLSearchParams ---
    function iterate(obj, fn) {
        var it = fnCall(obj[symIter], obj);
        var next = it.next;
        for (;;) {
            var r = fnCall(next, it);
            if (r.done) {
                return;
            }
            fn(r.value);
        }
    }

    function isObjectLike(v) {
        return (typeof v === 'object' && v !== null) || typeof v === 'function';
    }

    function URLSearchParams(init) {
        if (this === undefined || this === null || !isProtoOf(URLSearchParams.prototype, this)) {
            throw new TypeError("Failed to construct 'URLSearchParams': Please use the 'new' operator");
        }
        var list = [];
        if (init === undefined) {
            // 空。
        } else if (isObjectLike(init)) {
            if (typeof init[symIter] === 'function') {
                iterate(init, function (item) {
                    if (!isObjectLike(item) || typeof item[symIter] !== 'function') {
                        throw new TypeError('Each URLSearchParams pair must be iterable');
                    }
                    var pair = [];
                    iterate(item, function (x) {
                        arrPush(pair, toStr(x));
                    });
                    if (pair.length !== 2) {
                        throw new TypeError('Each URLSearchParams pair must have exactly two elements');
                    }
                    arrPush(list, { n: pair[0], v: pair[1] });
                });
            } else {
                var keys = objectKeys(init);
                for (var i = 0; i < keys.length; i++) {
                    arrPush(list, { n: keys[i], v: toStr(init[keys[i]]) });
                }
            }
        } else {
            list = parseForm(stripFirst(toStr(init), '?'));
        }
        wmSet(spStates, this, { list: list, url: null });
    }

    // 変更系メソッドの後で、紐づく URL の query を再解析で更新する（失敗したら URL を変えない）。
    function syncUrl(sp) {
        var st = sp.url;
        if (!st) {
            return;
        }
        var ser = serializeForm(sp.list);
        var p = splitHref(st.c[IDX_HREF]);
        var comps = parseComponents(p.head + (ser === '' ? '' : '?' + ser) + p.frag, undefined);
        if (comps !== null) {
            st.c = comps;
        }
    }

    var SP = URLSearchParams.prototype;
    defMethod(SP, 'append', function (name, value) {
        var sp = requireSp(this);
        arrPush(sp.list, { n: toStr(name), v: toStr(value) });
        syncUrl(sp);
    });
    defMethod(SP, 'delete', function (name) {
        var sp = requireSp(this);
        var key = toStr(name);
        for (var i = sp.list.length - 1; i >= 0; i--) {
            if (sp.list[i].n === key) {
                arrSplice(sp.list, i, 1);
            }
        }
        syncUrl(sp);
    });
    defMethod(SP, 'get', function (name) {
        var sp = requireSp(this);
        var key = toStr(name);
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key) {
                return sp.list[i].v;
            }
        }
        return null;
    });
    defMethod(SP, 'getAll', function (name) {
        var sp = requireSp(this);
        var key = toStr(name);
        var out = [];
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key) {
                arrPush(out, sp.list[i].v);
            }
        }
        return out;
    });
    defMethod(SP, 'has', function (name) {
        var sp = requireSp(this);
        var key = toStr(name);
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key) {
                return true;
            }
        }
        return false;
    });
    defMethod(SP, 'set', function (name, value) {
        var sp = requireSp(this);
        var key = toStr(name);
        var val = toStr(value);
        var first = -1;
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key) {
                first = i;
                break;
            }
        }
        if (first < 0) {
            arrPush(sp.list, { n: key, v: val });
        } else {
            sp.list[first].v = val;
            var j = first + 1;
            while (j < sp.list.length) {
                if (sp.list[j].n === key) {
                    arrSplice(sp.list, j, 1);
                } else {
                    j++;
                }
            }
        }
        syncUrl(sp);
    });
    defMethod(SP, 'toString', function () {
        return serializeForm(requireSp(this).list);
    });
    defMethod(SP, 'forEach', function (callback, thisArg) {
        var sp = requireSp(this);
        if (typeof callback !== 'function') {
            throw new TypeError('The callback provided as parameter 1 is not a function');
        }
        // 生きた一覧を添字で読む（コールバック内の変更が後続に見える。WHATWG と同じ）。
        for (var i = 0; i < sp.list.length; i++) {
            fnCall(callback, thisArg, sp.list[i].v, sp.list[i].n, this);
        }
    });

    // kind: 0 = keys, 1 = values, 2 = entries。
    function makeIterator(sp, kind) {
        var idx = 0;
        var it = {
            next: function () {
                if (idx >= sp.list.length) {
                    idx = Infinity;
                    return { value: undefined, done: true };
                }
                var p = sp.list[idx];
                idx++;
                return { value: kind === 0 ? p.n : (kind === 1 ? p.v : [p.n, p.v]), done: false };
            }
        };
        defineProperty(it, symIter, {
            value: function () {
                return this;
            },
            writable: true,
            configurable: true,
            enumerable: false
        });
        return it;
    }
    defMethod(SP, 'keys', function () {
        return makeIterator(requireSp(this), 0);
    });
    defMethod(SP, 'values', function () {
        return makeIterator(requireSp(this), 1);
    });
    defMethod(SP, 'entries', function () {
        return makeIterator(requireSp(this), 2);
    });
    defMethod(SP, symIter, SP.entries);

    defineProperty(global, 'URL', { value: URL, writable: true, configurable: true, enumerable: false });
    defineProperty(global, 'URLSearchParams',
        { value: URLSearchParams, writable: true, configurable: true, enumerable: false });
})(globalThis);
