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

  var cfg = jsonParse(cfgText);
  var epoch = cfg.epoch;
  var maxHandles = cfg.maxHandles;
  var maxResultLength = cfg.maxResultLength;
  var maxDescriptionLength = cfg.maxDescriptionLength;
  var MAX_HANDLE = 9007199254740991;

  var handles = new MapCtor();
  var handleCount = 0;
  var nextHandle = 1;
  var nextExceptionId = 1;

  // 内部の制御用に投げる固有の値（ユーザーコードからは見えない）。
  var HANDLE_LIMIT = {};
  var NOT_SERIALIZABLE = {};

  var RESET = '{"kind":"reset"}';
  var INTERNAL_ERROR = '{"kind":"internalError"}';

  function cut(s, n) {
    var t = StringCtor(s);
    return t.length > n ? apply(strSlice, t, [0, n]) : t;
  }

  function allocHandle(v) {
    if (handleCount >= maxHandles || nextHandle > MAX_HANDLE) {
      throw HANDLE_LIMIT;
    }
    var n = nextHandle;
    nextHandle = n + 1;
    apply(mapSet, handles, [n, v]);
    handleCount = handleCount + 1;
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
  function remote(v) {
    var t = typeof v;
    if (v === undefined) return { type: "undefined" };
    if (v === null) return { type: "object", subtype: "null", value: null };
    if (t === "boolean" || t === "string") return { type: t, value: v };
    if (t === "number") {
      if (v !== v) return { type: "number", unserializableValue: "NaN" };
      if (v === Infinity) return { type: "number", unserializableValue: "Infinity" };
      if (v === -Infinity) return { type: "number", unserializableValue: "-Infinity" };
      if (objectIs(v, -0)) return { type: "number", unserializableValue: "-0" };
      return { type: "number", value: v };
    }
    if (t === "bigint") {
      return { type: "bigint", unserializableValue: StringCtor(v) + "n" };
    }
    var h = allocHandle(v);
    if (t === "symbol") {
      return { type: "symbol", description: descriptionOf(v, t, undefined, "Symbol"), handle: h };
    }
    var tag = tagOf(v);
    var subtype = t === "function" ? undefined : subtypeOf(v, tag);
    var className = classNameOf(v, tag);
    var out = { type: t === "function" ? "function" : "object" };
    if (subtype !== undefined) out.subtype = subtype;
    out.className = className;
    out.description = descriptionOf(v, out.type, subtype, className);
    out.handle = h;
    return out;
  }

  // returnByValue 用。JSON 化できる値だけを値として返す（ハンドルは払い出さない）。
  function byValue(v) {
    var t = typeof v;
    if (t !== "object" || v === null) {
      if (t === "function" || t === "symbol") throw NOT_SERIALIZABLE;
      return remote(v);
    }
    var s;
    try {
      s = jsonStringify(v);
    } catch (e) {
      throw NOT_SERIALIZABLE;
    }
    if (typeof s !== "string") throw NOT_SERIALIZABLE;
    if (s.length > maxResultLength) return { kind: "resultTooLarge" };
    var tag = tagOf(v);
    var subtype = subtypeOf(v, tag);
    var out = { type: "object" };
    if (subtype !== undefined) out.subtype = subtype;
    out.className = classNameOf(v, tag);
    out.value = jsonParse(s);
    return out;
  }

  function finish(env) {
    var text = jsonStringify(env);
    if (text.length > maxResultLength) return '{"kind":"resultTooLarge"}';
    return text;
  }

  function evaluate(text) {
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
        var id = nextExceptionId;
        nextExceptionId = id + 1;
        return finish({ kind: "exception", exceptionId: id, exception: remote(v) });
      }
      if (p.returnByValue === true) {
        var r = byValue(v);
        if (r.kind === "resultTooLarge") return finish(r);
        return finish({ kind: "value", result: r });
      }
      return finish({ kind: "value", result: remote(v) });
    } catch (e) {
      if (e === HANDLE_LIMIT) return '{"kind":"handleLimit"}';
      if (e === NOT_SERIALIZABLE) return '{"kind":"notSerializable"}';
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
      return finish({ kind: "released", found: found });
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
