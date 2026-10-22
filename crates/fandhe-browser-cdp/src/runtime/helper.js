// Runtime.evaluate / Runtime.releaseObject / Runtime.callFunctionOn 用の評価ヘルパー（fandhe-browser-cdp。
// TASK-44.7・#1218・TASK-44.8・#1219・`CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`）。
// `runtime/evaluate.rs` が `include_str!` で取り込み、
// 評価コンテキストごとに 1 回だけ「(本ファイルの関数式)(設定の JSON 文字列リテラル)」として評価する。
// ES2015 の範囲（V8 と boa で同じソースを使う。Proxy は使わない）。
//
// 役割: ユーザー式をグローバルスコープで評価し（間接 eval）、結果を CDP RemoteObject の素材へ整え、
// objectId の元になるハンドル表を JS の中で保持する。戻り値は常に JSON 文字列（封筒）で、Rust 側が
// 許可リストで検証して組み立て直す。式や設定値を JS ソースへ連結しない（呼び出しは JSON.parse 経由）。
// awaitPromise: Promise の結果は「待機スロット」に記録し、Rust 側が別の評価で checkpoint（microtask の
// drain）→ settle（スロットの読み出しと削除）を呼んで決着させる。反応ジョブは JS の呼び出しフレームが
// 残っている間は走らないため、1 回の評価の中では待たない。
(function (cfgText) {
  "use strict";
  var G = globalThis;
  // 注入時点の組込みを退避する。同じ world のユーザーコードがプロトタイプを書き換えても影響を受けない。
  var jsonParse = JSON.parse;
  var jsonStringify = JSON.stringify;
  var indirectEval = G.eval;
  var apply = Reflect.apply;
  var MapCtor = Map;
  var mapSet = MapCtor.prototype.set;
  var mapDelete = MapCtor.prototype.delete;
  var mapHas = MapCtor.prototype.has;
  var mapGet = MapCtor.prototype.get;
  var promiseThen = Promise.prototype.then;
  var BigIntCtor = typeof BigInt === "function" ? BigInt : null;
  // shim の microtask キューを流す関数（checkpoint が呼ぶ。無ければ checkpoint は何もしない）。
  var runMicrotasks = typeof G.__fandheRunMicrotasks === "function" ? G.__fandheRunMicrotasks : null;
  var objectIs = Object.is;
  var objToString = Object.prototype.toString;
  var isArray = Array.isArray;
  var ErrorCtor = Error;
  var NodeCtor = typeof Node === "function" ? Node : null;
  var StringCtor = String;
  var strSlice = String.prototype.slice;
  var strToLower = String.prototype.toLowerCase;
  var fnToString = Function.prototype.toString;
  var defineProperty = Object.defineProperty;
  var freeze = Object.freeze;
  var objCreate = Object.create;
  var objKeys = Object.keys;
  var charCodeAt = String.prototype.charCodeAt;

  var cfg = jsonParse(cfgText);
  var epoch = cfg.epoch;
  var maxHandles = cfg.maxHandles;
  var maxResultLength = cfg.maxResultLength;
  var maxDescriptionLength = cfg.maxDescriptionLength;
  var maxUnserializableLength = cfg.maxUnserializableLength;
  var maxAwaiting = cfg.maxAwaiting;
  var MAX_HANDLE = 9007199254740991;

  var handles = new MapCtor();
  var handleCount = 0;
  var nextHandle = 1;
  var nextExceptionId = 1;
  // 待機スロット（awaitPromise 用）。決着・未決着のどちらでも settle / dropSlot で必ず削除する。
  var slots = new MapCtor();
  var slotCount = 0;
  var nextSlot = 1;

  // 内部の制御用に投げる固有の値（ユーザーコードからは見えない）。
  var HANDLE_LIMIT = {};
  var NOT_SERIALIZABLE = {};
  var TOO_LARGE = {};

  var RESET = '{"kind":"reset"}';
  var INTERNAL_ERROR = '{"kind":"internalError"}';

  // 封筒・RemoteObject の素材は null prototype で作る。ユーザーが Object.prototype.toJSON 等を
  // 足しても JSON 化に影響しないようにする（封筒の偽装を防ぐ）。
  function mk(o) {
    var r = objCreate(null);
    var ks = objKeys(o);
    for (var i = 0; i < ks.length; i++) {
      r[ks[i]] = o[ks[i]];
    }
    return r;
  }

  // UTF-16 の上限位置で切る。直前が上位サロゲートなら 1 つ手前で切り、ペアを分断しない。
  function cut(s, n) {
    var t = StringCtor(s);
    if (t.length <= n) return t;
    var end = n;
    if (end > 0) {
      var c = apply(charCodeAt, t, [end - 1]);
      if (c >= 0xd800 && c <= 0xdbff) end = end - 1;
    }
    return apply(strSlice, t, [0, end]);
  }

  // 単独サロゲートを U+FFFD に置換する（封筒の JSON 文字列に対して作用する。キー・値・入れ子を問わない）。
  // JSON.stringify（well-formed）は単独サロゲートを `\\udXXX` で出力するが Rust 側（serde_json）が
  // 受理しないため、ここで `\\ufffd` へ正規化する。エンジンが生の単独サロゲートを出す場合も同様。
  // 置換後の長さは元以下（上限検査を壊さない）。エスケープ済みバックスラッシュ（`\\\\`）は 2 文字で飛ばす。
  function hexVal(t, i) {
    var c = apply(charCodeAt, t, [i]);
    if (c >= 48 && c <= 57) return c - 48;
    if (c >= 97 && c <= 102) return c - 87;
    if (c >= 65 && c <= 70) return c - 55;
    return -1;
  }
  // t[i..i+6) が `\uXXXX` ならコードユニット、そうでなければ -1。
  function escUnit(t, i) {
    if (i + 6 > t.length) return -1;
    if (apply(charCodeAt, t, [i]) !== 92 || apply(charCodeAt, t, [i + 1]) !== 117) return -1;
    var n = 0;
    for (var k = 2; k < 6; k++) {
      var h = hexVal(t, i + k);
      if (h < 0) return -1;
      n = n * 16 + h;
    }
    return n;
  }
  function wellFormed(t) {
    var out = "";
    var from = 0;
    var i = 0;
    var len = t.length;
    while (i < len) {
      var c = apply(charCodeAt, t, [i]);
      if (c === 92) {
        var u = escUnit(t, i);
        if (u >= 0xd800 && u <= 0xdbff) {
          var u2 = escUnit(t, i + 6);
          if (u2 >= 0xdc00 && u2 <= 0xdfff) {
            i = i + 12;
            continue;
          }
          out = out + apply(strSlice, t, [from, i]) + "\\ufffd";
          i = i + 6;
          from = i;
          continue;
        }
        if (u >= 0xdc00 && u <= 0xdfff) {
          out = out + apply(strSlice, t, [from, i]) + "\\ufffd";
          i = i + 6;
          from = i;
          continue;
        }
        i = i + (u >= 0 ? 6 : 2);
        continue;
      }
      if (c >= 0xd800 && c <= 0xdbff) {
        var d = i + 1 < len ? apply(charCodeAt, t, [i + 1]) : 0;
        if (d >= 0xdc00 && d <= 0xdfff) {
          i = i + 2;
          continue;
        }
        out = out + apply(strSlice, t, [from, i]) + "\ufffd";
        i = i + 1;
        from = i;
        continue;
      }
      if (c >= 0xdc00 && c <= 0xdfff) {
        out = out + apply(strSlice, t, [from, i]) + "\ufffd";
        i = i + 1;
        from = i;
        continue;
      }
      i = i + 1;
    }
    if (from === 0) return t;
    return out + apply(strSlice, t, [from, len]);
  }

  // 払い出したハンドルのうち、クライアントへ通知できなかったものを回収する。
  // 1 回の評価で払い出す id は連番で連続するため、開始位置（evalStart）から nextHandle 未満までを
  // 数値だけで走査して回収する。通常の配列を使わないので、ユーザーが Array.prototype に
  // setter を仕込んでも記録の欠落・件数のずれは起きない（`SEC-2`・リソース上限）。
  var evalStart = 1;
  // 直近の評価が払い出した範囲 [lastFrom, lastTo)。Rust 側が封筒を使えなかったときの discard 用。
  var lastFrom = 1;
  var lastTo = 1;
  function rollback() {
    for (var i = evalStart; i < nextHandle; i++) {
      if (apply(mapDelete, handles, [i]) === true) {
        handleCount = handleCount - 1;
      }
    }
    evalStart = nextHandle;
  }

  function allocHandle(v) {
    if (handleCount >= maxHandles || nextHandle > MAX_HANDLE) {
      throw HANDLE_LIMIT;
    }
    var n = nextHandle;
    // 先に件数と番号を確保し、保存に失敗したら必ず巻き戻す。
    nextHandle = n + 1;
    handleCount = handleCount + 1;
    try {
      apply(mapSet, handles, [n, v]);
    } catch (e) {
      handleCount = handleCount - 1;
      throw e;
    }
    return n;
  }

  function tagOf(v) {
    try {
      return apply(strSlice, apply(objToString, v, []), [8, -1]);
    } catch (e) {
      return "Object";
    }
  }

  function isNode(v) {
    try {
      if (NodeCtor !== null) {
        return v instanceof NodeCtor;
      }
      return typeof v.nodeType === "number" && typeof v.nodeName === "string";
    } catch (e) {
      return false;
    }
  }

  function subtypeOf(v, tag) {
    try {
      if (isArray(v)) return "array";
      if (isNode(v)) return "node";
      if (v instanceof ErrorCtor || tag === "Error") return "error";
    } catch (e) {
      // 続行
    }
    if (tag === "RegExp") return "regexp";
    if (tag === "Date") return "date";
    if (tag === "Map") return "map";
    if (tag === "Set") return "set";
    if (tag === "Promise") return "promise";
    return undefined;
  }

  function classNameOf(v, tag) {
    try {
      var c = v.constructor;
      if (typeof c === "function" && typeof c.name === "string" && c.name !== "") {
        return cut(c.name, 256);
      }
    } catch (e) {
      // 続行
    }
    return cut(tag, 256);
  }

  function descriptionOf(v, type, subtype, className) {
    try {
      if (type === "function") {
        return cut(apply(fnToString, v, []), maxDescriptionLength);
      }
      if (type === "symbol") {
        return cut(v.toString(), maxDescriptionLength);
      }
      if (subtype === "array") {
        return "Array(" + StringCtor(v.length) + ")";
      }
      if (subtype === "error") {
        // エンジンにより stack の書式が違う（boa はメッセージを含まない）ため、決定的な
        // 「名前: メッセージ」にする（スタックは載せない。`REPAIR-3`）。
        return cut(StringCtor(v.name) + ": " + StringCtor(v.message), maxDescriptionLength);
      }
      if (subtype === "node") {
        // shim の Element は nodeName を持たない場合があるため localName・tagName へ順に代える。
        var name = v.nodeName;
        if (typeof name !== "string") name = v.localName;
        if (typeof name !== "string") name = v.tagName;
        if (typeof name === "string") {
          return cut(apply(strToLower, name, []), maxDescriptionLength);
        }
      }
    } catch (e) {
      // ユーザー定義の getter・toString が投げても、説明は固定の代替にする。
    }
    return className;
  }

  // 値を RemoteObject の素材へ。オブジェクト・関数・symbol はハンドルを払い出す。
  // ユーザーコードを呼び得るメタデータ（constructor・toStringTag・説明）はハンドル確保の前に済ませ、
  // 応答できない評価にハンドルが残らないようにする。
  function remote(v) {
    var t = typeof v;
    if (v === undefined) return mk({ type: "undefined" });
    if (v === null) return mk({ type: "object", subtype: "null", value: null });
    if (t === "boolean" || t === "string") return mk({ type: t, value: v });
    if (t === "number") {
      if (v !== v) return mk({ type: "number", unserializableValue: "NaN" });
      if (v === Infinity) return mk({ type: "number", unserializableValue: "Infinity" });
      if (v === -Infinity) return mk({ type: "number", unserializableValue: "-Infinity" });
      if (objectIs(v, -0)) return mk({ type: "number", unserializableValue: "-0" });
      return mk({ type: "number", value: v });
    }
    if (t === "bigint") {
      var bs = StringCtor(v) + "n";
      // Rust 側の検証（maxUnserializableLength）と同じ上限。超えたら明示的なサイズ上限エラーにする。
      if (bs.length > maxUnserializableLength) throw TOO_LARGE;
      return mk({ type: "bigint", unserializableValue: bs });
    }
    if (t === "symbol") {
      var sd = descriptionOf(v, t, undefined, "Symbol");
      return mk({ type: "symbol", description: sd, handle: allocHandle(v) });
    }
    var tag = tagOf(v);
    var subtype = t === "function" ? undefined : subtypeOf(v, tag);
    var className = classNameOf(v, tag);
    var type = t === "function" ? "function" : "object";
    var description = descriptionOf(v, type, subtype, className);
    var out = mk({ type: type });
    if (subtype !== undefined) out.subtype = subtype;
    out.className = className;
    out.description = description;
    out.handle = allocHandle(v);
    return out;
  }

  // returnByValue 用。JSON 化できる値だけを値として返す（ハンドルは払い出さない）。
  function byValue(v) {
    var t = typeof v;
    if (t !== "object" || v === null) {
      if (t === "function" || t === "symbol") throw NOT_SERIALIZABLE;
      return finish(mk({ kind: "value", result: remote(v) }));
    }
    var s;
    try {
      s = jsonStringify(v);
    } catch (e) {
      throw NOT_SERIALIZABLE;
    }
    if (typeof s !== "string") throw NOT_SERIALIZABLE;
    if (s.length > maxResultLength) throw TOO_LARGE;
    var tag = tagOf(v);
    var subtype = subtypeOf(v, tag);
    var out = mk({ type: "object" });
    if (subtype !== undefined) out.subtype = subtype;
    out.className = classNameOf(v, tag);
    // value は JSON.parse し直さず文字列のまま埋め込む（通常の Object を介さない）。
    var rs = jsonStringify(out);
    return '{"kind":"value","result":' + apply(strSlice, rs, [0, -1]) + ',"value":' + s + "}}";
  }

  function finish(env) {
    var text = jsonStringify(env);
    if (text.length > maxResultLength) throw TOO_LARGE;
    return text;
  }

  // 値（または例外）を封筒の文字列へ整える。払い出したハンドルの範囲は呼び出し側が commit で確定する。
  function render(v, threw, returnByValue) {
    var out;
    if (threw) {
      var id = nextExceptionId;
      nextExceptionId = id + 1;
      out = finish(mk({ kind: "exception", exceptionId: id, exception: remote(v) }));
    } else if (returnByValue === true) {
      out = byValue(v);
      if (out.length > maxResultLength) throw TOO_LARGE;
    } else {
      out = finish(mk({ kind: "value", result: remote(v) }));
    }
    return wellFormed(out);
  }

  function commit(out) {
    lastFrom = evalStart;
    lastTo = nextHandle;
    evalStart = nextHandle;
    return out;
  }

  // 失敗時: この評価で払い出したハンドルを巻き戻し、制御用の値を封筒にする。
  function failure(e) {
    rollback();
    lastFrom = nextHandle;
    lastTo = nextHandle;
    if (e === HANDLE_LIMIT) return '{"kind":"handleLimit"}';
    if (e === NOT_SERIALIZABLE) return '{"kind":"notSerializable"}';
    if (e === TOO_LARGE) return '{"kind":"resultTooLarge"}';
    return INTERNAL_ERROR;
  }

  // v が Promise か（組込みの brand を使う判定。then は呼ばない）。
  // 識別と反応登録の失敗を分けるための判定で、登録の例外を「非 Promise」の根拠にしない（`REPAIR-3`）。
  function isPromiseValue(v) {
    try {
      return tagOf(v) === "Promise" && v instanceof Promise;
    } catch (e) {
      return false;
    }
  }

  // 反応登録が投げた例外（Promise と判定できたのに then が失敗したとき。constructor・Symbol.species の
  // 解決が投げる場合など）。呼び出し側が exceptionDetails 付きの例外封筒にする。
  var thenError;
  var THEN_FAILED = -1;

  // v が native Promise なら待機スロットを払い出してその番号を返す。Promise でなければ 0。
  // Promise と判定できたのに反応登録が失敗したら thenError に記録して THEN_FAILED を返す。
  // 反応は記録だけを行い、遅れて決着してもスロットが無ければ何も復活させない（レコードは閉じ込める）。
  function track(v) {
    var rec = objCreate(null);
    rec.state = 0;
    rec.value = undefined;
    try {
      apply(promiseThen, v, [
        function (x) {
          if (rec.state === 0) {
            rec.state = 1;
            rec.value = x;
          }
        },
        function (e) {
          if (rec.state === 0) {
            rec.state = 2;
            rec.value = e;
          }
        }
      ]);
    } catch (e) {
      if (!isPromiseValue(v)) return 0;
      thenError = e;
      return THEN_FAILED;
    }
    if (slotCount >= maxAwaiting || nextSlot > MAX_HANDLE) throw HANDLE_LIMIT;
    var n = nextSlot;
    nextSlot = n + 1;
    slotCount = slotCount + 1;
    apply(mapSet, slots, [n, rec]);
    return n;
  }

  function dropSlotNow(n) {
    if (apply(mapDelete, slots, [n]) === true) {
      slotCount = slotCount - 1;
    }
  }

  // 結果が awaitPromise 対象の Promise なら待機中の封筒、そうでなければ通常の封筒。
  function finishCall(v, p) {
    if (p.awaitPromise === true) {
      var n = track(v);
      if (n === THEN_FAILED) {
        var te = thenError;
        thenError = undefined;
        return commit(render(te, true, false));
      }
      if (n > 0) {
        return commit('{"kind":"awaiting","slot":' + StringCtor(n) + "}");
      }
    }
    return commit(render(v, false, p.returnByValue));
  }

  function evaluate(text) {
    evalStart = nextHandle;
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      var v;
      var threw = false;
      try {
        v = indirectEval(p.expression);
      } catch (e) {
        v = e;
        threw = true;
      }
      if (threw) {
        return commit(render(v, true, false));
      }
      return finishCall(v, p);
    } catch (e) {
      return failure(e);
    }
  }

  var NOT_FOUND = '{"kind":"objectNotFound"}';
  var NOT_FUNCTION = '{"kind":"notFunction"}';

  // 引数のハンドルが無いときに投げる固有の印（ユーザーコードからは見えない）。
  var MISSING = {};
  // 引数 1 件を JS の値にする（k: "v" 値・"u" unserializableValue・"h" ハンドル・それ以外は undefined）。
  function argValue(a) {
    var k = a.k;
    if (k === "v") return a.value;
    if (k === "u") {
      var t = a.text;
      if (t === "NaN") return NaN;
      if (t === "Infinity") return Infinity;
      if (t === "-Infinity") return -Infinity;
      if (t === "-0") return -0;
      if (BigIntCtor === null) throw MISSING;
      var neg = apply(strSlice, t, [0, 1]) === "-";
      var digits = apply(strSlice, t, [neg ? 1 : 0, t.length - 1]);
      var b = apply(BigIntCtor, undefined, [digits]);
      return neg ? -b : b;
    }
    if (k === "h") {
      if (apply(mapHas, handles, [a.handle]) !== true) throw MISSING;
      return apply(mapGet, handles, [a.handle]);
    }
    return undefined;
  }

  // Runtime.callFunctionOn。this と引数のハンドルはユーザーコードを動かす前にすべて解決する。
  function callFunctionOn(text) {
    evalStart = nextHandle;
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      if (apply(mapHas, handles, [p.thisHandle]) !== true) return NOT_FOUND;
      var thisValue = apply(mapGet, handles, [p.thisHandle]);
      // 引数リストは null prototype の array-like にする（Array.prototype の setter の影響を避ける）。
      var list = objCreate(null);
      var n = p.args.length;
      try {
        for (var i = 0; i < n; i++) {
          list[i] = argValue(p.args[i]);
        }
      } catch (e) {
        if (e === MISSING) return NOT_FOUND;
        throw e;
      }
      list.length = n;
      var fn;
      try {
        fn = indirectEval("(" + p.functionDeclaration + "\n)");
      } catch (e) {
        return commit(render(e, true, false));
      }
      if (typeof fn !== "function") return NOT_FUNCTION;
      var v;
      try {
        v = apply(fn, thisValue, list);
      } catch (e) {
        return commit(render(e, true, false));
      }
      return finishCall(v, p);
    } catch (e) {
      return failure(e);
    }
  }

  // microtask を流す（awaitPromise の 1 段目。Rust 側は評価を分けて呼ぶ）。
  function checkpoint(text) {
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      if (runMicrotasks !== null) {
        try {
          apply(runMicrotasks, undefined, []);
        } catch (e) {
          // shim のコールバックの例外は決着の可否に影響しない。
        }
      }
      return '{"kind":"checkpointed"}';
    } catch (e) {
      return INTERNAL_ERROR;
    }
  }

  // スロットを読んで必ず削除し、決着していれば封筒にする（未決着は notSettled）。
  function settle(text) {
    evalStart = nextHandle;
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      if (apply(mapHas, slots, [p.slot]) !== true) return INTERNAL_ERROR;
      var rec = apply(mapGet, slots, [p.slot]);
      dropSlotNow(p.slot);
      if (rec.state === 0) return '{"kind":"notSettled"}';
      return commit(render(rec.value, rec.state === 2, p.returnByValue));
    } catch (e) {
      return failure(e);
    }
  }

  // 決着を待てなかった場合にスロットだけを捨てる（冪等）。
  function dropSlot(text) {
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      dropSlotNow(p.slot);
      return '{"kind":"checkpointed"}';
    } catch (e) {
      return INTERNAL_ERROR;
    }
  }

  // 直近の評価が払い出したハンドルを回収する（Rust 側が結果を返せなかったとき。冪等）。
  function discard(text) {
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      var n = 0;
      for (var i = lastFrom; i < lastTo; i++) {
        if (apply(mapDelete, handles, [i]) === true) {
          handleCount = handleCount - 1;
          n = n + 1;
        }
      }
      lastFrom = lastTo;
      return finish(mk({ kind: "released", found: n > 0 }));
    } catch (e) {
      return INTERNAL_ERROR;
    }
  }

  function release(text) {
    try {
      var p = jsonParse(text);
      if (p.epoch !== epoch) return RESET;
      var found = apply(mapHas, handles, [p.handle]) === true;
      if (found) {
        apply(mapDelete, handles, [p.handle]);
        handleCount = handleCount - 1;
      }
      return finish(mk({ kind: "released", found: found }));
    } catch (e) {
      return INTERNAL_ERROR;
    }
  }

  // 既に注入済みなら何もしない（並行した初回評価への冪等性）。
  if (typeof __fandheCdpHelper === "object" && __fandheCdpHelper !== null) {
    return '{"kind":"installed","epoch":' + StringCtor(__fandheCdpHelper.epoch) + "}";
  }
  var api = freeze({
    epoch: epoch,
    evaluate: evaluate,
    release: release,
    discard: discard,
    callFunctionOn: callFunctionOn,
    checkpoint: checkpoint,
    settle: settle,
    dropSlot: dropSlot
  });
  defineProperty(G, "__fandheCdpHelper", {
    value: api,
    writable: false,
    configurable: false,
    enumerable: false
  });
  return '{"kind":"installed","epoch":' + StringCtor(epoch) + "}";
})
