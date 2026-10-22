// Runtime.evaluate / Runtime.releaseObject 用の評価ヘルパー（fandhe-browser-cdp。TASK-44.7・#1218・
// `CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`）。`runtime/evaluate.rs` が `include_str!` で取り込み、
// 評価コンテキストごとに 1 回だけ「(本ファイルの関数式)(設定の JSON 文字列リテラル)」として評価する。
// ES2015 の範囲（V8 と boa で同じソースを使う。Proxy は使わない）。
//
// 役割: ユーザー式をグローバルスコープで評価し（間接 eval）、結果を CDP RemoteObject の素材へ整え、
// objectId の元になるハンドル表を JS の中で保持する。戻り値は常に JSON 文字列（封筒）で、Rust 側が
// 許可リストで検証して組み立て直す。式や設定値を JS ソースへ連結しない（呼び出しは JSON.parse 経由）。
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
  var MAX_HANDLE = 9007199254740991;

  var handles = new MapCtor();
  var handleCount = 0;
  var nextHandle = 1;
  var nextExceptionId = 1;

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

  // 払い出したハンドルのうち、クライアントへ通知できなかったものを回収する。
  // 1 回の評価で払い出す id は連番で連続するため、開始位置（evalStart）から nextHandle 未満までを
  // 数値だけで走査して回収する。通常の配列を使わないので、ユーザーが Array.prototype に
  // setter を仕込んでも記録の欠落・件数のずれは起きない（`SEC-2`・リソース上限）。
  var evalStart = 1;
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
      var out;
      if (threw) {
        var id = nextExceptionId;
        nextExceptionId = id + 1;
        out = finish(mk({ kind: "exception", exceptionId: id, exception: remote(v) }));
      } else if (p.returnByValue === true) {
        out = byValue(v);
        if (out.length > maxResultLength) throw TOO_LARGE;
      } else {
        out = finish(mk({ kind: "value", result: remote(v) }));
      }
      evalStart = nextHandle;
      return out;
    } catch (e) {
      rollback();
      if (e === HANDLE_LIMIT) return '{"kind":"handleLimit"}';
      if (e === NOT_SERIALIZABLE) return '{"kind":"notSerializable"}';
      if (e === TOO_LARGE) return '{"kind":"resultTooLarge"}';
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
  var api = freeze({ epoch: epoch, evaluate: evaluate, release: release });
  defineProperty(G, "__fandheCdpHelper", {
    value: api,
    writable: false,
    configurable: false,
    enumerable: false
  });
  return '{"kind":"installed","epoch":' + StringCtor(epoch) + "}";
})
