// js_shim/net.js: fetch / XMLHttpRequest の最小 shim
// （TASK-111・MS-6・Issue #783・ビヘイビア JS-8・JS-6。SSOT: js-engine.md「ページ内 JS 実行の設計制約」）。
//
// 役割: ページ JS の GET 取得を「要求の受理（`netRequest` op）→ ランナーが core の Fetcher で取得 →
// `__fandheNetDeliver(id)` で結果を配送」という経路に限定する。この shim 自身はネットワークへ出ず、
// 評価中に待機もしない（要求を Rust 側のキューへ積み、Promise / コールバックを保持するだけ）。
// URL の解決（navigate 先 URL 基準）、scheme / userinfo / メソッド / ボディの検証、件数・累計バイトの
// 上限、内部アドレス・リダイレクトの検査（SSRF 対策）は core 側（page_net / Fetcher）が行う。
// 呼び出し元: core の `js_shim::install`（timers.js の後に評価）と、ランナーのイベントループ段。
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
// - GET 以外・要求ボディ・同期 XHR（reject / 例外）。Request / AbortController / Response.headers /
//   statusText / blob / arrayBuffer / XHR の responseType / setRequestHeader / addEventListener。
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
    var MAX_TEXT_UNITS = 4097;
    // 1 回の配送で読む本文チャンク数の上限（64 KiB × 1024 は累計上限 4 MiB より十分大きい）。
    var MAX_CHUNKS = 1024;

    // 要求 ID → { kind: 'fetch', resolve, reject } | { kind: 'xhr', xhr }。
    var pending = createObject(null);

    function reportError(kind, error) {
        try {
            var text;
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
            callOp('netCallbackError', kind, text);
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

    function makeResponse(status, url, body) {
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
        try {
            url =
                typeof input === 'object' && input !== null && typeof input.href === 'string'
                    ? input.href
                    : toStr(input);
            if (init !== undefined && init !== null && typeof init === 'object') {
                for (var key in init) {
                    if (!hasOwn(init, key)) {
                        continue;
                    }
                    if (key === 'method') {
                        if (init.method !== undefined) {
                            method = toStr(init.method);
                        }
                    } else if (key === 'body') {
                        if (init.body !== undefined && init.body !== null) {
                            hasBody = true;
                        }
                    } else {
                        ignored += 1;
                    }
                }
            }
        } catch (e) {
            return NativePromise.reject(failedToFetch());
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
            pending[id] = { kind: 'fetch', resolve: resolve, reject: reject };
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
    }

    function xhrReset(xhr) {
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
        var body = '';
        var url = '';
        if (status > 0) {
            try {
                // 空チャンク（終端）を確認できないまま上限に達したら、切り詰めた本文を成功として
                // 公開せず失敗にする。
                var ended = false;
                for (var i = 0; i < MAX_CHUNKS; i++) {
                    var chunk = callOp('netReadBody', id);
                    if (typeof chunk !== 'string' || chunk.length === 0) {
                        ended = true;
                        break;
                    }
                    body += chunk;
                }
                if (!ended) {
                    throw new NativeError('response body too long to deliver');
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
                    entry.resolve(makeResponse(status, url, body));
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
