// js_shim/net.js: fetch / XMLHttpRequest の最小 shim
// （TASK-111・MS-6・Issue #783・ビヘイビア JS-8・JS-6。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページ JS の GET 取得を「要求の受理（`netRequest` op）→ ランナーが core の Fetcher で取得 →
// `__fandheNetDeliver(id)` で結果を配送」という経路に限定する。この shim 自身はネットワークへ出ず、
// 評価中に待機もしない（要求を Rust 側のキューへ積み、Promise / コールバックを保持するだけ）。
// URL の解決（navigate 先 URL 基準）、scheme / userinfo / メソッド / ボディの検証、件数・累計バイトの
// 上限、内部アドレス・リダイレクトの検査（SSRF 対策）は core 側（page_net / Fetcher）が行う。
// 呼び出し元: core の `js_shim::install`（abort.js の後に評価）と、ランナーのイベントループ段。
//
// 設計上の要点:
// - __fandheNetDeliver は `netConsume` op（ランナーが指定した配送中 ID への一回限りの許可）が
//   0 以上を返したときだけ処理する。ページが直接呼んでも他の要求の本文は読めない。
// - ディスパッチャーは決して throw しない。コールバックの例外は try/catch で捕捉し、
//   `netCallbackError` op でメッセージだけ通知する。non-writable / non-configurable で固定し、
//   再 install 時の差し替えは events.js / timers.js と同じく `lifecycleInstallOpen` が true のときだけ。
// - fetch の init の headers / credentials / mode 等は送らない（Cookie・認証ヘッダを載せない。
//   無視した件数だけ `netOptionsIgnored` で通知する）。User-Agent は Fetcher の既定のまま。
// - 配列メソッドは使わず（ページによる Array.prototype の改変の影響を避ける）、null プロトタイプの
//   コンテナを使う。ES2015 の範囲に留める（V8 と boa で同一ソース）。
//
// 未実装（REPAIR-3）:
// - GET 以外・要求ボディ・同期 XHR（reject / 例外）。Request / Response.headers /
//   statusText / blob / arrayBuffer / XHR の responseType / setRequestHeader / addEventListener。
// - fetch の `signal`（#1150）: 中止済みなら要求を出さずに reason で reject し、要求後の中止は未着手の要求を
//   `netCancel` で取り除いて reason で reject する。着手済みの要求は取り消せず、応答は届くが pending に無いため
//   捨てられる（`completed()` は増え得る）。受理件数は中止しても戻らない（連打で上限を回避できない）。
//   応答取得後の中止（#1162・JS-5・JS-8）: 未読の `text()` / `json()` は読み取り時に signal を問い合わせ、
//   中止済みなら reason で reject する（bodyUsed は true になり、2 回目は TypeError）。読み取り済みの値は変わらない。
//   配送後の追従登録は持たない（リスナー上限 JS-6 に影響しない）。
//   XHR の signal・Request の signal は扱わない。
// - boa ではネイティブ Promise のジョブが drain されない（js crate が run_jobs を呼ばない。TASK-30）。
//   そのため boa では fetch の `.then` / await の続きが走らない。XHR（コールバック式）は動く。
(function (global) {
    'use strict';

    var bridge = global.__dom;
    if (!bridge || typeof bridge.op !== 'function') {
        throw new TypeError('__dom.op is not available');
    }
    var nativeOp = bridge.op;
    var callOp = Function.prototype.call.bind(nativeOp, bridge);
    var defineProperty = Object.defineProperty;
    var createObject = Object.create;
    var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    var fnCall = Function.prototype.call.bind(Function.prototype.call);
    var NativePromise = global.Promise;
    var NativeTypeError = TypeError;
    var NativeError = Error;
    var jsonParse = JSON.parse;
    var toStr = String;
    var queueMicro = global.queueMicrotask;
    // abort.js の窓口と reason の getter は install 時に捕捉する（ページによる差し替えの影響を避ける。JS-4）。
    var abortFollow = typeof global.__fandheAbortFollow === 'function' ? global.__fandheAbortFollow : null;
    var abortReasonDesc =
        typeof global.AbortSignal === 'function'
            ? Object.getOwnPropertyDescriptor(global.AbortSignal.prototype, 'reason')
            : undefined;
    var abortReasonGet = abortReasonDesc !== undefined ? abortReasonDesc.get : null;
    var MAX_TEXT_UNITS = 4097;

    // 要求 ID → { kind: 'fetch', resolve, reject } | { kind: 'xhr', xhr }。
    var pending = createObject(null);

    function reportError(kind, error) {
        try {
            var text;
            var name = '';
            try {
                // 例外の種類の名前（TypeError 等）。ページが書き換え得るため文字列のときだけ渡す。
                name = error && typeof error.name === 'string' ? error.name.slice(0, 64) : '';
            } catch (e) {
                name = '';
            }
            try {
                text = toStr(error && error.message !== undefined ? error.message : error);
            } catch (e) {
                text = '[unprintable]';
            }
            if (text.length > MAX_TEXT_UNITS) {
                var end = MAX_TEXT_UNITS;
                var last = text.charCodeAt(end - 1);
                // サロゲートペアの途中で切らない。
                if (last >= 0xd800 && last <= 0xdbff) {
                    end -= 1;
                }
                text = text.slice(0, end);
            }
            var stack = '';
            try {
                // error.stack はページが偽造できる。文字列のときだけ、サロゲートを割らずに切って渡す。
                if (error && typeof error.stack === 'string') {
                    stack = error.stack.slice(0, 1024);
                    var sl = stack.charCodeAt(stack.length - 1);
                    if (sl >= 0xd800 && sl <= 0xdbff) {
                        stack = stack.slice(0, -1);
                    }
                }
            } catch (e) {
                stack = '';
            }
            callOp('netCallbackError', kind, name, text, stack);
        } catch (e) {
            // 報告に失敗しても続行する。
        }
    }

    function requestId(kind, method, hasBody, url) {
        try {
            var id = callOp('netRequest', kind, method, hasBody, url);
            return typeof id === 'number' && id > 0 ? id : 0;
        } catch (e) {
            return 0;
        }
    }

    function failedToFetch() {
        return new NativeTypeError('Failed to fetch');
    }

    // ---- Response（最小。構築できるのは fetch の配送だけ） ----

    function Response() {
        throw new NativeTypeError('Illegal constructor');
    }

    // 応答取得後に signal が中止されたかを読み取り時に問い合わせる（#1162・JS-5・JS-8）。
    // 配送時に追従を解除済みのためリスナーは持たず、install 時捕捉の窓口だけを使う。
    function isAborted(signal) {
        return signal !== null && abortFollow !== null && abortFollow(signal) === 2;
    }

    function abortedReject(signal) {
        return NativePromise.reject(abortReasonGet !== null ? fnCall(abortReasonGet, signal) : undefined);
    }

    function makeResponse(status, url, body, signal) {
        var used = false;
        var r = createObject(Response.prototype);
        defineProperty(r, 'status', { value: status, enumerable: true });
        defineProperty(r, 'ok', { value: status >= 200 && status <= 299, enumerable: true });
        defineProperty(r, 'url', { value: url, enumerable: true });
        defineProperty(r, 'bodyUsed', {
            get: function () {
                return used;
            },
            enumerable: true
        });
        defineProperty(r, 'text', {
            value: function text() {
                if (used) {
                    return NativePromise.reject(
                        new NativeTypeError('Body has already been consumed')
                    );
                }
                used = true;
                if (isAborted(signal)) {
                    return abortedReject(signal);
                }
                return NativePromise.resolve(body);
            }
        });
        defineProperty(r, 'json', {
            value: function json() {
                if (used) {
                    return NativePromise.reject(
                        new NativeTypeError('Body has already been consumed')
                    );
                }
                used = true;
                if (isAborted(signal)) {
                    return abortedReject(signal);
                }
                try {
                    return NativePromise.resolve(jsonParse(body));
                } catch (e) {
                    return NativePromise.reject(e);
                }
            }
        });
        return r;
    }

    function fetch(input, init) {
        var url;
        var method = 'GET';
        var hasBody = false;
        var ignored = 0;
        var signal;
        try {
            url =
                typeof input === 'object' && input !== null && typeof input.href === 'string'
                    ? input.href
                    : toStr(input);
            if (init !== undefined && init !== null && typeof init === 'object') {
                // method / body は列挙可能性・own かどうかに依存せず通常のプロパティ取得で読む
                // （プロトタイプ上や非列挙の指定を無視して GET を送らないため）。
                var m = init.method;
                if (m !== undefined) {
                    method = toStr(m);
                }
                var b = init.body;
                if (b !== undefined && b !== null) {
                    hasBody = true;
                }
                signal = init.signal;
                // 無視したオプションの集計は method / body / signal と分離する。
                for (var key in init) {
                    if (hasOwn(init, key) && key !== 'method' && key !== 'body' && key !== 'signal') {
                        ignored += 1;
                    }
                }
            }
        } catch (e) {
            return NativePromise.reject(failedToFetch());
        }
        var hasSignal = signal !== undefined && signal !== null;
        if (hasSignal) {
            var state = abortFollow !== null ? abortFollow(signal) : 0;
            if (state === 0) {
                return NativePromise.reject(
                    new NativeTypeError("Failed to execute 'fetch': 'signal' is not an AbortSignal")
                );
            }
            if (state === 2) {
                // 中止済みなら要求を出さない。
                return NativePromise.reject(abortReasonGet !== null ? fnCall(abortReasonGet, signal) : undefined);
            }
        }
        if (ignored > 0) {
            try {
                callOp('netOptionsIgnored', ignored);
            } catch (e) {
                // 通知に失敗しても要求自体は続行する。
            }
        }
        var id = requestId('fetch', method, hasBody, url);
        if (id === 0) {
            return NativePromise.reject(failedToFetch());
        }
        return new NativePromise(function (resolve, reject) {
            var entry = { kind: 'fetch', resolve: resolve, reject: reject, signal: null, onAbort: null };
            pending[id] = entry;
            if (hasSignal) {
                var onAbort = function (reason) {
                    if (!hasOwn(pending, id) || pending[id] !== entry) {
                        return;
                    }
                    delete pending[id];
                    try {
                        callOp('netCancel', id);
                    } catch (e) {
                        // 取り消せなくても、応答は pending に無いため捨てられる。
                    }
                    reject(reason);
                };
                if (abortFollow(signal, onAbort) === true) {
                    entry.signal = signal;
                    entry.onAbort = onAbort;
                } else {
                    delete pending[id];
                    try {
                        callOp('netCancel', id);
                    } catch (e) {
                        // 無視する。
                    }
                    reject(failedToFetch());
                }
            }
        });
    }

    // ---- XMLHttpRequest（最小。非同期 GET のみ） ----

    function callHandler(xhr, name) {
        // ハンドラの取得（getter の例外を含む）も診断へ集約し、後続の処理を続ける。
        try {
            var handler = xhr[name];
            if (typeof handler === 'function') {
                fnCall(handler, xhr);
            }
        } catch (e) {
            reportError('xhr', e);
        }
    }

    // readystatechange の後に abort() / open() で要求世代が変わったら、後続イベントは発火しない。
    function xhrFail(xhr) {
        var gen = xhr._g;
        xhr.readyState = 4;
        xhr.status = 0;
        callHandler(xhr, 'onreadystatechange');
        if (xhr._g === gen) {
            callHandler(xhr, 'onerror');
        }
    }

    function xhrDone(xhr, status, url, body) {
        var gen = xhr._g;
        xhr.readyState = 4;
        xhr.status = status;
        xhr.responseURL = url;
        xhr.responseText = body;
        callHandler(xhr, 'onreadystatechange');
        if (xhr._g === gen) {
            callHandler(xhr, 'onload');
        }
    }

    function later(fn) {
        if (typeof queueMicro === 'function') {
            queueMicro(fn);
        } else {
            fn();
        }
    }

    function XMLHttpRequest() {
        this.readyState = 0;
        this.status = 0;
        this.statusText = '';
        this.responseText = '';
        this.responseURL = '';
        this.onload = null;
        this.onerror = null;
        this.onreadystatechange = null;
        defineProperty(this, '_m', { value: 'GET', writable: true });
        defineProperty(this, '_u', { value: '', writable: true });
        defineProperty(this, '_sent', { value: false, writable: true });
        // 要求世代。open() / abort() で進め、旧世代の配送・失敗通知を破棄する。
        defineProperty(this, '_g', { value: 0, writable: true });
        // 送信中の要求 ID（0 は無し）。abort() / open() で Rust 側の未送信要求を取り消す。
        defineProperty(this, '_id', { value: 0, writable: true });
    }

    function xhrCancel(xhr) {
        var id = xhr._id;
        xhr._id = 0;
        if (id === 0) {
            return;
        }
        delete pending[id];
        try {
            callOp('netCancel', id);
        } catch (e) {
            // 取り消せなくても、旧世代の応答は世代検査で破棄される。
        }
    }

    function xhrReset(xhr) {
        xhrCancel(xhr);
        xhr._g += 1;
        xhr.status = 0;
        xhr.statusText = '';
        xhr.responseText = '';
        xhr.responseURL = '';
    }

    XMLHttpRequest.prototype.open = function open(method, url, isAsync) {
        if (isAsync === false) {
            // 同期 XHR は評価中に待てない。診断に残して例外にする。
            requestId('xhr_sync', 'GET', false, toStr(url));
            throw new NativeError('Synchronous XMLHttpRequest is not supported');
        }
        this._m = toStr(method);
        this._u = toStr(url);
        this._sent = false;
        xhrReset(this);
        this.readyState = 1;
        callHandler(this, 'onreadystatechange');
    };

    XMLHttpRequest.prototype.send = function send(body) {
        if (this.readyState !== 1 || this._sent) {
            throw new NativeError('InvalidStateError');
        }
        this._sent = true;
        var hasBody = body !== undefined && body !== null;
        var id = requestId('xhr', this._m, hasBody, this._u);
        if (id === 0) {
            var self = this;
            var failGen = this._g;
            later(function () {
                if (self._g === failGen) {
                    xhrFail(self);
                }
            });
            return;
        }
        this._id = id;
        pending[id] = { kind: 'xhr', xhr: this, gen: this._g };
    };

    // 未実装のメソッドは何もしない（ヘッダは送らない。REPAIR-3）。
    XMLHttpRequest.prototype.setRequestHeader = function setRequestHeader() {
        try {
            callOp('netOptionsIgnored', 1);
        } catch (e) {
            // 通知に失敗しても無視する。
        }
    };
    XMLHttpRequest.prototype.abort = function abort() {
        // 送信済みの要求の配送を無効化する（世代を進める）。
        xhrReset(this);
        this._sent = false;
        this.readyState = 0;
    };

    // ---- 配送（ランナーが `__fandheNetDeliver(id)` を評価する） ----

    function deliver(id) {
        var status = -1;
        try {
            status = callOp('netConsume', id);
        } catch (e) {
            return;
        }
        if (typeof status !== 'number' || !(status >= 0)) {
            return;
        }
        if (!hasOwn(pending, id)) {
            return;
        }
        var entry = pending[id];
        delete pending[id];
        if (entry.kind === 'xhr' && entry.xhr._id === id) {
            entry.xhr._id = 0;
        }
        if (entry.kind === 'fetch' && entry.signal !== null) {
            abortFollow(entry.signal, entry.onAbort, true);
        }
        var body = '';
        var url = '';
        if (status > 0) {
            try {
                // 空チャンク（終端）まで読む。チャンクの大きさはブリッジの設定上限で変わるため
                // 固定の回数では打ち切らない。各回は 1 バイト以上進み、本文の総量は Rust 側で
                // 累計上限に収まっており、読み出しの回数は操作予算と実時間上限（`JS-6`）が
                // 制限する（超過時は op がエラーになり、下の catch で失敗にする）。
                for (;;) {
                    var chunk = callOp('netReadBody', id);
                    if (typeof chunk !== 'string' || chunk.length === 0) {
                        break;
                    }
                    body += chunk;
                }
                url = toStr(callOp('netFinalUrl', id));
            } catch (e) {
                status = 0;
            }
        }
        if (entry.kind === 'fetch') {
            try {
                if (status === 0) {
                    entry.reject(failedToFetch());
                } else {
                    entry.resolve(makeResponse(status, url, body, entry.signal));
                }
            } catch (e) {
                reportError('fetch', e);
            }
        } else if (entry.xhr._g !== entry.gen) {
            // open() / abort() 後に届いた旧世代の応答は破棄する。
            return;
        } else if (status === 0) {
            xhrFail(entry.xhr);
        } else {
            xhrDone(entry.xhr, status, url, body);
        }
    }

    function defineMethod(name, fn) {
        defineProperty(global, name, {
            value: fn,
            writable: true,
            configurable: true,
            enumerable: false
        });
    }

    if (typeof NativePromise === 'function') {
        defineMethod('fetch', fetch);
        defineMethod('Response', Response);
    }
    defineMethod('XMLHttpRequest', XMLHttpRequest);

    // 固定の窓口関数は初回だけ定義し、実体は再 install のたびに差し替える（timers.js と同じ作法）。
    function defineDispatcher(name, impl) {
        var existing = hasOwn(global, name) ? global[name] : null;
        if (existing !== null && typeof existing === 'function' && existing.__fandheImpl === true) {
            existing.setImpl(impl);
            return;
        }
        if (existing !== null) {
            return;
        }
        var currentImpl = impl;
        var entry = function (arg) {
            try {
                currentImpl(arg);
            } catch (e) {
                // ディスパッチャーは throw しない。
            }
        };
        defineProperty(entry, '__fandheImpl', { value: true });
        defineProperty(entry, 'setImpl', {
            // 差し替えは Rust 側が shim 注入中だけ立てるフラグが true のときに限る。
            value: function (next) {
                var open = false;
                try {
                    open = callOp('lifecycleInstallOpen') === true;
                } catch (e) {
                    open = false;
                }
                if (open) {
                    currentImpl = next;
                }
            }
        });
        defineProperty(global, name, {
            value: entry,
            writable: false,
            configurable: false,
            enumerable: false
        });
    }

    defineDispatcher('__fandheNetDeliver', deliver);
})(globalThis);
