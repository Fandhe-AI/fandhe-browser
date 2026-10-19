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
//   更新する（直列化結果が空なら `?` ごと消す）。再解析に失敗したら一覧も URL も変えず TypeError で拒否する。逆に `href` / `search` の setter は中身を作り直す。
// - form-urlencoded は `decodeURIComponent` / `encodeURIComponent` / `TextDecoder` を使わず、UTF-8 を
//   手書きで符号化・復号する（不正な `%` はそのまま残し、不正な UTF-8 と孤立サロゲートは U+FFFD にする）。
//   解析は線形走査で正規表現のバックトラッキングも eval も使わない。
// - 上限（JS-6）: URLSearchParams は純 JS だが、構築・append・set は確保の前に件数（65536）と
//   文字数（1 MiB）の共通予算で検査して TypeError で拒否する。pair は 3 個目で拒否する。
//   `URL` 経由の入力は op の文字列上限（既定 64 KiB）と応答上限で止まる。
//   search・hash setter は入力長と符号化後の長さを 64 KiB で打ち切り（符号化の前後）、超えたら状態を変えない。
//   record 初期化は正規化後に同名のキーを後の値で上書きする。
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
// - `URLSearchParams` の `sort`・`size`。
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
    var getOwnPropertySymbols = Object.getOwnPropertySymbols;
    var propIsEnumerable = Function.prototype.call.bind(Object.prototype.propertyIsEnumerable);
    var strCtor = String;
    var fromCharCode = String.fromCharCode;
    var reTest = Function.prototype.call.bind(RegExp.prototype.test);
    var SURROGATE_RE = /[\uD800-\uDFFF]/;
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
    var isArray = Array.isArray;
    // 未改変の配列イテレータだけを添字ループで走査する高速経路の判定用（反復 1 件ごとの呼び出し費用を避ける）。
    var arrValues = Array.prototype[Symbol.iterator];
    var arrIterNext = Object.getPrototypeOf([][Symbol.iterator]()).next;
    var arrIterProto = Object.getPrototypeOf([][Symbol.iterator]());
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
    // 区切りを逐次走査し、各項目を確保する前に件数・文字数の予算を検査する（JS-6）。
    function parseForm(s) {
        var list = [];
        var len = s.length;
        chargeChars({ n: 0, c: 0 }, len);
        // 項目を確保・復号する前に、charCodeAt の線形走査で空項目を除いた件数を数えて上限超過を拒否する。
        // indexOf(…, 開始位置)・split・正規表現の全体走査は boa で入力長に対して二次時間になるため使わない。
        var count = 0;
        var inPiece = false;
        for (var i = 0; i < len; i++) {
            if (sCharCodeAt(s, i) === 38) {
                inPiece = false;
            } else if (!inPiece) {
                inPiece = true;
                count++;
                if (count > MAX_ENTRIES) {
                    throw new TypeError('URLSearchParams input exceeds the entry limit');
                }
            }
        }
        var start = 0;
        for (var j = 0; j <= len; j++) {
            if (j === len || sCharCodeAt(s, j) === 38) {
                if (j > start) {
                    var piece = sSub(s, start, j);
                    var eq = sIndexOf(piece, '=');
                    var name = eq < 0 ? piece : sSub(piece, 0, eq);
                    var value = eq < 0 ? '' : sSub(piece, eq + 1);
                    arrPush(list, { n: formDecode(name), v: formDecode(value) });
                }
                start = j + 1;
            }
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

    // USVString への変換（孤立サロゲートを U+FFFD に置き換える）。名前・値の保存と検索の前に必ず通す。
    // 検索系（get・has・delete 等）の引数は、予算を超える長さなら変換の前に TypeError で拒否する（JS-6）。
    function usv(value) {
        var s = toStr(value);
        if (s.length > MAX_CHARS) {
            throw new TypeError('URLSearchParams input exceeds the size limit');
        }
        return usvStr(s);
    }

    // 確保前の件数・文字数の共通予算（JS-6）。URLSearchParams の構築・append・set は、確保の前に
    // この予算で検査し、超えたら TypeError で拒否する（終端のない iterable でも確保を続けない）。
    var MAX_ENTRIES = 65536;
    var MAX_CHARS = 1048576;
    // search / hash setter の入力・符号化後の上限。core の `urlParse` op の文字列上限（既定 64 KiB）に合わせ、
    // 超える値は op に渡しても失敗するため、符号化・split の前に打ち切る。
    var MAX_SETTER_CHARS = 65536;

    function newBudget(list) {
        var b = { n: 0, c: 0 };
        if (list) {
            for (var i = 0; i < list.length; i++) {
                b.n++;
                b.c += list[i].n.length + list[i].v.length;
            }
        }
        return b;
    }

    function chargeEntry(b) {
        b.n++;
        if (b.n > MAX_ENTRIES) {
            throw new TypeError('URLSearchParams input exceeds the entry limit');
        }
    }

    function chargeChars(b, len) {
        b.c += len;
        if (b.c > MAX_CHARS) {
            throw new TypeError('URLSearchParams input exceeds the size limit');
        }
    }

    // 文字列化してから長さを予算で検査し、通れば USVString に変換する（変換前に確保を止める）。
    function usvCharged(value, b) {
        var s = toStr(value);
        chargeChars(b, s.length);
        return usvStr(s);
    }

    function usvStr(s) {
        // サロゲートを含まない文字列は変換不要（長い入力で 1 文字ずつ走査しない）。
        if (!reTest(SURROGATE_RE, s)) {
            return s;
        }
        var out = '';
        var n = s.length;
        var i = 0;
        while (i < n) {
            var cp = codePointAt(s, i);
            if (cp > 0xFFFF) {
                out += sSub(s, i, i + 2);
                i += 2;
            } else {
                out += cp === 0xFFFD ? fromCharCode(0xFFFD) : sCharAt(s, i);
                i++;
            }
        }
        return out;
    }

    // search / hash の値を再解析用に符号化する。URL 全体の解析は先頭・末尾の空白と C0 制御文字を
    // 削るため、値の末尾が失われないよう空白と C0 制御文字を先に `%XX` にする（tab・LF・CR は
    // WHATWG の解析と同じく取り除く）。
    // 入力は呼び出し側が MAX_SETTER_CHARS 以下に確認済みで、符号化の途中でも出力が予算を超えたら
    // null を返して打ち切る（JS-6。1 文字が最大 3 倍に展開されるため出力側も検査する）。
    function encodeTail(s) {
        var out = '';
        for (var i = 0; i < s.length; i++) {
            if (out.length > MAX_SETTER_CHARS) {
                return null;
            }
            var c = sCharCodeAt(s, i);
            if (c === 9 || c === 10 || c === 13) {
                continue;
            }
            if (c <= 0x20) {
                out += '%' + sCharAt(HEX, c >> 4) + sCharAt(HEX, c & 15);
            } else {
                out += sCharAt(s, i);
            }
        }
        return out;
    }

    // 文字列変換。Symbol は Web IDL どおり TypeError にする（String() は Symbol を文字列化してしまう）。
    function toStr(value) {
        if (typeof value === 'symbol') {
            throw new TypeError('Cannot convert a Symbol value to a string');
        }
        return strCtor(value);
    }

    function requireArgs(given, needed) {
        if (given < needed) {
            throw new TypeError(needed + ' argument' + (needed === 1 ? '' : 's') +
                ' required, but only ' + given + ' present.');
        }
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
        if (new.target === undefined) {
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
            var raw = toStr(value);
            if (raw.length > MAX_SETTER_CHARS) {
                return;
            }
            var p = splitHref(st.c[IDX_HREF]);
            var query = '';
            // 空文字の代入だけが query の削除。'?' は空の query として残す。
            if (raw !== '') {
                var enc = encodeTail(stripFirst(raw, '?'));
                if (enc === null) {
                    return;
                }
                query = '?' + arrJoin(sSplit(enc, '#'), '%23');
            }
            // 失敗したら変更しない（setter は投げない）。
            reparse(st, p.head + query + p.frag);
        },
        hash: function (value) {
            var st = requireUrl(this);
            var raw = toStr(value);
            if (raw.length > MAX_SETTER_CHARS) {
                return;
            }
            var v = encodeTail(stripFirst(raw, '#'));
            if (v === null) {
                return;
            }
            var href = st.c[IDX_HREF];
            var h = sIndexOf(href, '#');
            var head = h < 0 ? href : sSub(href, 0, h);
            reparse(st, raw === '' ? head : head + '#' + v);
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
        requireArgs(arguments.length, 1);
        return parseComponents(toStr(input), base === undefined ? undefined : toStr(base)) !== null;
    });

    // --- URLSearchParams ---
    function iterate(obj, fn) {
        var method = obj[symIter];
        if (isArray(obj) && method === arrValues && arrIterProto.next === arrIterNext) {
            for (var k = 0; k < obj.length; k++) {
                fn(obj[k]);
            }
            return;
        }
        var it = fnCall(method, obj);
        var next = it.next;
        for (;;) {
            var r = fnCall(next, it);
            if (!isObjectLike(r)) {
                throw new TypeError('Iterator result is not an object');
            }
            if (r.done) {
                return;
            }
            try {
                fn(r.value);
            } catch (e) {
                // コールバックの例外では IteratorClose 相当（return を呼ぶ）を行い、元の例外を優先して再送出する。
                try {
                    var ret = it['return'];
                    if (ret !== undefined && ret !== null) {
                        fnCall(ret, it);
                    }
                } catch (ignored) {
                    // 元の例外を優先する。
                }
                throw e;
            }
        }
    }

    function isObjectLike(v) {
        return (typeof v === 'object' && v !== null) || typeof v === 'function';
    }

    function URLSearchParams(init) {
        if (new.target === undefined) {
            throw new TypeError("Failed to construct 'URLSearchParams': Please use the 'new' operator");
        }
        var list = [];
        var budget = newBudget(null);
        if (init === undefined || init === null) {
            // 空。
        } else if (isObjectLike(init)) {
            var initIter = init[symIter];
            if (initIter !== undefined && initIter !== null && typeof initIter !== 'function') {
                throw new TypeError('URLSearchParams input Symbol.iterator is not a function');
            }
            if (initIter !== undefined && initIter !== null) {
                // 配列は件数が上限を超えるなら反復の前に拒否する（要素ごとの処理に入らない）。
                if (isArray(init) && init.length > MAX_ENTRIES) {
                    throw new TypeError('URLSearchParams input exceeds the entry limit');
                }
                iterate(init, function (item) {
                    if (!isObjectLike(item) || typeof item[symIter] !== 'function') {
                        throw new TypeError('Each URLSearchParams pair must be iterable');
                    }
                    chargeEntry(budget);
                    var pair = [];
                    iterate(item, function (x) {
                        // 3 個目を検出した時点で拒否する（終端のない iterable で確保を続けない）。
                        if (pair.length >= 2) {
                            throw new TypeError('Each URLSearchParams pair must have exactly two elements');
                        }
                        arrPush(pair, usvCharged(x, budget));
                    });
                    if (pair.length !== 2) {
                        throw new TypeError('Each URLSearchParams pair must have exactly two elements');
                    }
                    arrPush(list, { n: pair[0], v: pair[1] });
                });
            } else {
                // 列挙可能な Symbol キーは Web IDL の record 変換で文字列化に失敗する（TypeError）。
                var syms = getOwnPropertySymbols(init);
                for (var si = 0; si < syms.length; si++) {
                    if (propIsEnumerable(init, syms[si])) {
                        throw new TypeError('Cannot convert a Symbol value to a string');
                    }
                }
                var keys = objectKeys(init);
                var seen = createObject(null);
                if (keys.length > MAX_ENTRIES) {
                    throw new TypeError('URLSearchParams input exceeds the entry limit');
                }
                for (var i = 0; i < keys.length; i++) {
                    chargeEntry(budget);
                    var rn = usvCharged(keys[i], budget);
                    var rv = usvCharged(init[keys[i]], budget);
                    // 正規化後に同名になるキーは、最初の位置を保ち後の値で上書きする（WHATWG の record 変換）。
                    var at = seen[rn];
                    if (at !== undefined) {
                        list[at].v = rv;
                    } else {
                        seen[rn] = list.length;
                        arrPush(list, { n: rn, v: rv });
                    }
                }
            }
        } else {
            var text = toStr(init);
            chargeChars(budget, text.length);
            list = parseForm(stripFirst(usvStr(text), '?'));
        }
        wmSet(spStates, this, { list: list, url: null });
    }

    function copyList(list) {
        var out = [];
        for (var i = 0; i < list.length; i++) {
            arrPush(out, { n: list[i].n, v: list[i].v });
        }
        return out;
    }

    // 変更後の候補一覧を確定する。紐づく URL があれば先に href への反映を検証し、成功したときだけ
    // 一覧と URL の両方を更新する。失敗（上限超過など）は一覧も URL も変えずに TypeError で拒否する。
    function commit(sp, next) {
        var st = sp.url;
        if (st) {
            var ser = serializeForm(next);
            var p = splitHref(st.c[IDX_HREF]);
            var comps = parseComponents(p.head + (ser === '' ? '' : '?' + ser) + p.frag, undefined);
            if (comps === null) {
                throw new TypeError('Invalid URL');
            }
            st.c = comps;
        }
        sp.list = next;
    }

    var SP = URLSearchParams.prototype;
    defMethod(SP, 'append', function (name, value) {
        requireArgs(arguments.length, 2);
        var sp = requireSp(this);
        // 引数の変換（利用者コードが走りうる）を一覧の複製より先に済ませ、最新の一覧を複製する。
        var n = toStr(name);
        var v = toStr(value);
        var b = newBudget(sp.list);
        chargeEntry(b);
        chargeChars(b, n.length + v.length);
        var next = copyList(sp.list);
        arrPush(next, { n: usvStr(n), v: usvStr(v) });
        commit(sp, next);
    });
    defMethod(SP, 'delete', function (name, value) {
        requireArgs(arguments.length, 1);
        var sp = requireSp(this);
        var key = usv(name);
        // 第 2 引数が undefined でなければ、名前と値の両方が一致する項目だけを消す。
        var hasVal = value !== undefined;
        var val = hasVal ? usv(value) : '';
        var next = [];
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n !== key || (hasVal && sp.list[i].v !== val)) {
                arrPush(next, { n: sp.list[i].n, v: sp.list[i].v });
            }
        }
        commit(sp, next);
    });
    defMethod(SP, 'get', function (name) {
        requireArgs(arguments.length, 1);
        var sp = requireSp(this);
        var key = usv(name);
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key) {
                return sp.list[i].v;
            }
        }
        return null;
    });
    defMethod(SP, 'getAll', function (name) {
        requireArgs(arguments.length, 1);
        var sp = requireSp(this);
        var key = usv(name);
        var out = [];
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key) {
                arrPush(out, sp.list[i].v);
            }
        }
        return out;
    });
    defMethod(SP, 'has', function (name, value) {
        requireArgs(arguments.length, 1);
        var sp = requireSp(this);
        var key = usv(name);
        var hasVal = value !== undefined;
        var val = hasVal ? usv(value) : '';
        for (var i = 0; i < sp.list.length; i++) {
            if (sp.list[i].n === key && (!hasVal || sp.list[i].v === val)) {
                return true;
            }
        }
        return false;
    });
    defMethod(SP, 'set', function (name, value) {
        requireArgs(arguments.length, 2);
        var sp = requireSp(this);
        var key = toStr(name);
        var val = toStr(value);
        chargeChars({ n: 0, c: 0 }, key.length + val.length);
        key = usvStr(key);
        val = usvStr(val);
        // 置換で消える同名の項目を予算から差し引き、更新後の最終状態で検査する。
        var b = { n: 0, c: 0 };
        for (var j = 0; j < sp.list.length; j++) {
            var cur = sp.list[j];
            if (cur.n !== key) {
                b.n++;
                b.c += cur.n.length + cur.v.length;
            }
        }
        chargeEntry(b);
        chargeChars(b, key.length + val.length);
        var next = [];
        var done = false;
        for (var i = 0; i < sp.list.length; i++) {
            var e = sp.list[i];
            if (e.n !== key) {
                arrPush(next, { n: e.n, v: e.v });
            } else if (!done) {
                arrPush(next, { n: key, v: val });
                done = true;
            }
        }
        if (!done) {
            arrPush(next, { n: key, v: val });
        }
        commit(sp, next);
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
