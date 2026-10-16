// 既定コンテナ（V8 同梱・musl 静的・scratch）内の JS 実行スモークのドライバ（TASK-66.3・Issue #304・CTR-2・JS-1・MS-5）。
//
// 役割: 配布バイナリ `/fandhe-browser` を JS ワーカーモード（環境変数 FANDHE_BROWSER_JS_WORKER=5・
// FANDHE_BROWSER_JS_WORKER_ENGINE=v8）で `docker run -i` 起動し、親プロセスの代わりに stdio 越しに
// ワーカープロトコルを話す。crates/fandhe-browser-js/tests/conformance.rs の 3 分類
// （スクリプト評価・グローバル関数注入・DOM 風バインディング）に相当する処理を、コンテナ内の V8（musl）で
// 実際に実行して成否を判定する。
//
// 呼び出し元: harness/container-js-smoke/run.sh（docker build/inspect と環境行の出力は run.sh 側）。
// 自己テスト（self-test.sh）は fake_worker.mjs を同じ経路で起動するため、本ファイルの codec と Reader を
// export している（直接起動時のみ main を実行する）。
//
// プロトコル: crates/fandhe-browser-js/src/worker_protocol.rs の v5 にハードコードした写しである。
// 対応関数は各 codec のコメントに記す。PROTOCOL_VERSION が上がると HELLO で result=blocked になり検出される。
//
// 判定は fail-closed: 未知の tag・不正な値タグ・不正 UTF-8・想定外のフレーム順・タイムアウト・EOF は
// すべて失敗にし、実行ケース 0 件も失敗にする（成功を装わない）。
// 評価するスクリプトはこのファイル内の固定定数のみ。argv・環境変数からは受け取らない（インジェクション経路を作らない）。
//
// 依存は Node の組み込みモジュールのみ（dependency-policy）。
import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { pathToFileURL } from 'node:url';

export const PROTOCOL_VERSION = 5;
// worker_protocol.rs の tag モジュール。
export const TAG = {
  HELLO: 1,
  EVALUATE: 2,
  RESULT: 3,
  ERROR: 4,
  NATIVE_CALL: 5,
  NATIVE_RETURN: 6,
  SHUTDOWN: 7,
  REGISTER_GLOBAL_FUNCTION: 8,
  BIND_DOM_LIKE_OBJECT: 9,
};
// MAX_FRAME_PAYLOAD_CHILD_TO_PARENT（子 → 親の受信上限。確保前に検査する）。
export const MAX_FRAME_PAYLOAD_CHILD_TO_PARENT = 3 * 1_048_576 + 65_536;
const MAX_NATIVE_CALL_ARGS = 64;

export class ProtoError extends Error {}
export class CrashError extends Error {}
export class TimeoutError extends Error {}

const utf8 = new TextDecoder('utf-8', { fatal: true });

// encode_js_value の写し。undefined=Undefined・null=Null。
export function encValue(v) {
  if (v === undefined) return Buffer.from([0]);
  if (v === null) return Buffer.from([1]);
  if (typeof v === 'boolean') return Buffer.from([2, v ? 1 : 0]);
  if (typeof v === 'number') {
    const b = Buffer.alloc(9);
    b[0] = 3;
    b.writeDoubleLE(v, 1);
    return b;
  }
  if (typeof v === 'string') {
    const s = Buffer.from(v, 'utf8');
    const b = Buffer.alloc(5);
    b[0] = 4;
    b.writeUInt32LE(s.length, 1);
    return Buffer.concat([b, s]);
  }
  throw new ProtoError('unsupported value type');
}

// decode_js_value の写し。[値, 消費後のオフセット] を返す。
export function decValue(buf, off = 0) {
  const need = (n) => {
    if (off + n > buf.length) throw new ProtoError('truncated value');
  };
  need(1);
  const t = buf[off];
  switch (t) {
    case 0:
      return [undefined, off + 1];
    case 1:
      return [null, off + 1];
    case 2:
      need(2);
      if (buf[off + 1] > 1) throw new ProtoError('invalid bool byte');
      return [buf[off + 1] === 1, off + 2];
    case 3:
      need(9);
      return [buf.readDoubleLE(off + 1), off + 9];
    case 4: {
      need(5);
      const len = buf.readUInt32LE(off + 1);
      need(5 + len);
      let s;
      try {
        s = utf8.decode(buf.subarray(off + 5, off + 5 + len));
      } catch {
        throw new ProtoError('invalid utf-8');
      }
      return [s, off + 5 + len];
    }
    case 5:
      throw new ProtoError('unexpected object handle value');
    default:
      throw new ProtoError(`invalid value tag ${t}`);
  }
}

export function frame(tag, payload = Buffer.alloc(0)) {
  const h = Buffer.alloc(5);
  h.writeUInt32LE(payload.length + 1, 0);
  h[4] = tag;
  return Buffer.concat([h, payload]);
}

// encode_evaluate の写し（u64 timeout_ms + u64 max_result_bytes + script）。
export function encEvaluate(script, timeoutMs = 5000, maxBytes = 1_048_576) {
  const h = Buffer.alloc(16);
  h.writeBigUInt64LE(BigInt(timeoutMs), 0);
  h.writeBigUInt64LE(BigInt(maxBytes), 8);
  return Buffer.concat([h, Buffer.from(script, 'utf8')]);
}

// encode_register_global_function の写し（u32 id + 名前 UTF-8）。
export function encRegister(id, name) {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(id, 0);
  return Buffer.concat([b, Buffer.from(name, 'utf8')]);
}

// encode_bind_dom_like_object の写し
// （u32 handle + u16 名前長 + 名前 + u32 件数 + 各 [u8 kind(0=Method) + u32 call id + u16 名前長 + 名前]）。
export function encBindDom(handle, name, members) {
  const nm = Buffer.from(name, 'utf8');
  const head = Buffer.alloc(6);
  head.writeUInt32LE(handle, 0);
  head.writeUInt16LE(nm.length, 4);
  const cnt = Buffer.alloc(4);
  cnt.writeUInt32LE(members.length, 0);
  const parts = [head, nm, cnt];
  for (const [mname, callId] of members) {
    const mn = Buffer.from(mname, 'utf8');
    const m = Buffer.alloc(7);
    m[0] = 0;
    m.writeUInt32LE(callId, 1);
    m.writeUInt16LE(mn.length, 5);
    parts.push(m, mn);
  }
  return Buffer.concat(parts);
}

// decode_native_call の写し（u32 id + u32 argc + 値列。末尾余剰は拒否）。
export function decNativeCall(p) {
  if (p.length < 8) throw new ProtoError('truncated native call');
  const id = p.readUInt32LE(0);
  const argc = p.readUInt32LE(4);
  if (argc > MAX_NATIVE_CALL_ARGS) throw new ProtoError('too many native call args');
  const args = [];
  let off = 8;
  for (let i = 0; i < argc; i += 1) {
    const [v, n] = decValue(p, off);
    args.push(v);
    off = n;
  }
  if (off !== p.length) throw new ProtoError('trailing bytes in native call');
  return [id, args];
}

// encode_native_return の写し（0 + 値 | 1 + メッセージ）。
export function encNativeReturn(r) {
  if (r.err !== undefined) return Buffer.concat([Buffer.from([1]), Buffer.from(r.err, 'utf8')]);
  return Buffer.concat([Buffer.from([0]), encValue(r.ok)]);
}

// decode_error の写し（u8 kind + メッセージ。kind は 0..3）。
export function decError(p) {
  if (p.length < 1) throw new ProtoError('truncated error');
  if (p[0] > 3) throw new ProtoError(`invalid error kind ${p[0]}`);
  let msg;
  try {
    msg = utf8.decode(p.subarray(1));
  } catch {
    throw new ProtoError('invalid utf-8');
  }
  return [p[0], msg];
}

// 受信フレームのリーダー。長さを読んだ時点で上限と比べ、確保前に拒否する（OWASP A04）。
export class Reader {
  constructor(stream, max = MAX_FRAME_PAYLOAD_CHILD_TO_PARENT) {
    this.buf = Buffer.alloc(0);
    this.eof = false;
    this.max = max;
    this.wake = null;
    this.stream = stream;
    this.overflow = false;
    this.stopped = false;
    // 未処理バッファの総量上限（完成フレーム 1 個 + 次フレーム 1 個分）。追加前に検査する。
    this.cap = 2 * (max + 5);
    stream.on('data', (d) => {
      if (this.stopped) return;
      if (this.buf.length + d.length > this.cap) {
        this.overflow = true;
        this.stop();
        this.#notify();
        return;
      }
      this.buf = Buffer.concat([this.buf, d]);
      this.#notify();
    });
    const end = () => {
      this.eof = true;
      this.#notify();
    };
    stream.on('end', end);
    stream.on('close', end);
    stream.on('error', end);
  }

  // 受信を直ちに止め、バッファを破棄する（異常検出後・中断後に受信を続けない）。
  stop() {
    this.stopped = true;
    this.buf = Buffer.alloc(0);
    try {
      this.stream.pause();
      if (typeof this.stream.destroy === 'function') this.stream.destroy();
    } catch {
      // 既に閉じている場合は無視する。
    }
  }

  #notify() {
    const w = this.wake;
    this.wake = null;
    if (w) w();
  }

  // 次のフレーム { tag, payload } を返す。クリーンな EOF は null。
  async next(timeoutMs) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (this.overflow) throw new ProtoError('receive buffer limit exceeded');
      if (this.buf.length >= 4) {
        const len = this.buf.readUInt32LE(0);
        if (len === 0) throw new ProtoError('empty frame');
        if (len - 1 > this.max) throw new ProtoError(`frame too large (${len - 1} > ${this.max})`);
        if (this.buf.length >= 4 + len) {
          const tag = this.buf[4];
          const payload = Buffer.from(this.buf.subarray(5, 4 + len));
          this.buf = this.buf.subarray(4 + len);
          return { tag, payload };
        }
      }
      if (this.eof) {
        if (this.buf.length === 0) return null;
        throw new CrashError('stream ended mid-frame');
      }
      const left = deadline - Date.now();
      if (left <= 0) throw new TimeoutError('timed out waiting for a frame');
      await new Promise((resolve) => {
        const t = setTimeout(resolve, left);
        this.wake = () => {
          clearTimeout(t);
          resolve();
        };
      });
    }
  }
}

// 評価するスクリプトは固定定数（argv・環境変数から受け取らない）。fake_worker.mjs も共有する。
export const SCRIPTS = {
  arith: '1 + 2 * 3',
  string: "'fandhe' + '-' + 'browser'",
  bool: 'true',
  nul: 'null',
  undef: 'undefined',
  syntax: '1 +',
  thrown: "throw new Error('boom')",
  print: "print('hello')",
  add: 'add(2, 3)',
  fails: 'alwaysFails()',
  domRoundtrip: "dom.setText('title', 'Fandhe'); dom.getText('title')",
  domCount: "dom.setText('body', 'x'); dom.count()",
  domMissing: "dom.getText('missing')",
};
export const NATIVE_ID = { print: 1, add: 2, alwaysFails: 3, setText: 10, getText: 11, count: 12 };
// ErrorKind のワイヤ値（0=Evaluation, 1=Binding）。
const KIND_EVALUATION = 0;

function buildCases(state) {
  const evalCase = (id, category, script, expectValue) => ({
    id,
    category,
    tag: TAG.EVALUATE,
    payload: encEvaluate(script),
    check: (o) => (o.type === 'result' && Object.is(o.value, expectValue) ? true : `expected result ${JSON.stringify(expectValue) ?? 'undefined'}`),
  });
  const errCase = (id, category, script, kinds, mustContain) => ({
    id,
    category,
    tag: TAG.EVALUATE,
    payload: encEvaluate(script),
    expectsError: true,
    check: (o) => {
      if (o.type !== 'error') return 'expected an error frame';
      if (!kinds.includes(o.kind)) return `unexpected error kind ${o.kind}`;
      if (mustContain && !o.msg.includes(mustContain)) return 'error message lacks expected text';
      return true;
    },
  });
  const regCase = (id, category, tag, payload) => ({
    id,
    category,
    tag,
    payload,
    check: (o) => (o.type === 'result' && o.value === undefined ? true : 'expected result undefined'),
  });
  return [
    evalCase('eval-arith', 'eval', SCRIPTS.arith, 7),
    evalCase('eval-string', 'eval', SCRIPTS.string, 'fandhe-browser'),
    evalCase('eval-bool', 'eval', SCRIPTS.bool, true),
    evalCase('eval-null', 'eval', SCRIPTS.nul, null),
    evalCase('eval-undefined', 'eval', SCRIPTS.undef, undefined),
    errCase('eval-syntax-error', 'eval', SCRIPTS.syntax, [KIND_EVALUATION]),
    errCase('eval-throw', 'eval', SCRIPTS.thrown, [KIND_EVALUATION], 'boom'),
    regCase('inject-register-print', 'inject', TAG.REGISTER_GLOBAL_FUNCTION, encRegister(NATIVE_ID.print, 'print')),
    regCase('inject-register-add', 'inject', TAG.REGISTER_GLOBAL_FUNCTION, encRegister(NATIVE_ID.add, 'add')),
    regCase('inject-register-fails', 'inject', TAG.REGISTER_GLOBAL_FUNCTION, encRegister(NATIVE_ID.alwaysFails, 'alwaysFails')),
    {
      id: 'inject-print',
      category: 'inject',
      tag: TAG.EVALUATE,
      payload: encEvaluate(SCRIPTS.print),
      check: (o) => {
        if (o.type !== 'result' || o.value !== undefined) return 'expected result undefined';
        return JSON.stringify(state.printed) === '["hello"]' ? true : 'print arguments were not recorded as ["hello"]';
      },
    },
    evalCase('inject-add', 'inject', SCRIPTS.add, 5),
    errCase('inject-fails', 'inject', SCRIPTS.fails, [KIND_EVALUATION], 'always fails'),
    regCase(
      'dom-bind',
      'dom',
      TAG.BIND_DOM_LIKE_OBJECT,
      encBindDom(1, 'dom', [
        ['setText', NATIVE_ID.setText],
        ['getText', NATIVE_ID.getText],
        ['count', NATIVE_ID.count],
      ]),
    ),
    evalCase('dom-roundtrip', 'dom', SCRIPTS.domRoundtrip, 'Fandhe'),
    {
      id: 'dom-count',
      category: 'dom',
      tag: TAG.EVALUATE,
      payload: encEvaluate(SCRIPTS.domCount),
      check: (o) => {
        if (o.type !== 'result' || o.value !== 2) return 'expected result 2';
        return state.store.get('title') === 'Fandhe' && state.store.get('body') === 'x' && state.store.size === 2
          ? true
          : 'store contents differ from title=Fandhe, body=x';
      },
    },
    evalCase('dom-missing', 'dom', SCRIPTS.domMissing, ''),
  ];
}

// 逆方向 RPC（NATIVE_CALL）の応答役。ドライバ側が conformance.rs の NativeFn 相当を担う。
function nativeHandler(state, id, args) {
  switch (id) {
    case NATIVE_ID.print:
      state.printed.push(...args);
      return { ok: undefined };
    case NATIVE_ID.add:
      return typeof args[0] === 'number' && typeof args[1] === 'number' ? { ok: args[0] + args[1] } : { err: 'add expects two numbers' };
    case NATIVE_ID.alwaysFails:
      return { err: 'always fails' };
    case NATIVE_ID.setText:
      if (typeof args[0] === 'string' && typeof args[1] === 'string') {
        state.store.set(args[0], args[1]);
        return { ok: undefined };
      }
      return { err: 'setText expects (id, text) strings' };
    case NATIVE_ID.getText:
      return typeof args[0] === 'string' ? { ok: state.store.get(args[0]) ?? '' } : { err: 'getText expects an id string' };
    case NATIVE_ID.count:
      return { ok: state.store.size };
    default:
      return { err: 'unknown native call id' };
  }
}

export function sanitize(s) {
  // eslint-disable-next-line no-control-regex
  return String(s).replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, 300);
}

const IMAGE_REF_RE = /^[a-z0-9][a-z0-9._/-]{0,127}(:[A-Za-z0-9._-]{1,128})?$/;
const PLATFORM_RE = /^linux\/(amd64|arm64)$/;

function summary(total, executed, success, result, reason) {
  console.log(`container-js-smoke-summary: role=default total=${total} executed=${executed} success=${success} result=${result} reason=${reason}`);
}

async function main(argv) {
  let image = 'fandhe-browser:default';
  let platform = '';
  let caseSec = 20;
  for (let i = 0; i < argv.length; i += 2) {
    const v = argv[i + 1];
    if (v === undefined) {
      console.error('error: missing value for an option');
      return 2;
    }
    if (argv[i] === '--image') image = v;
    else if (argv[i] === '--platform') platform = v;
    else if (argv[i] === '--case-timeout') caseSec = Number(v);
    else {
      console.error('error: unknown argument');
      return 2;
    }
  }
  if (!IMAGE_REF_RE.test(image)) {
    console.error('error: --image is invalid');
    return 2;
  }
  if (platform !== '' && !PLATFORM_RE.test(platform)) {
    console.error('error: --platform is invalid');
    return 2;
  }
  if (!Number.isInteger(caseSec) || caseSec < 1 || caseSec > 120) {
    console.error('error: --case-timeout is invalid');
    return 2;
  }
  const timeoutMs = caseSec * 1000;

  // 自己テスト専用フック（fake_worker.mjs を同じ経路で起動する）。実行では設定しない。
  const fake = process.env.CONTAINER_JS_SMOKE_FAKE_WORKER_FOR_TEST;
  const name = `fandhe-js-smoke-${process.pid}-${randomBytes(4).toString('hex')}`;
  let cmd;
  let args;
  if (fake) {
    cmd = process.execPath;
    args = [fake, process.env.CONTAINER_JS_SMOKE_FAKE_MODE_FOR_TEST || 'ok'];
  } else {
    cmd = 'docker';
    args = [
      'run', '-i', '--rm', '--name', name, '--network', 'none', '--cap-drop', 'ALL',
      '--security-opt', 'no-new-privileges', '--read-only',
      '-e', `FANDHE_BROWSER_JS_WORKER=${PROTOCOL_VERSION}`, '-e', 'FANDHE_BROWSER_JS_WORKER_ENGINE=v8',
    ];
    if (platform) args.push('--platform', platform);
    args.push(image);
  }
  // docker CLI を kill してもコンテナは残るため、終了時に必ず rm -f する。
  const removeContainer = () => {
    if (!fake) spawnSync('docker', ['rm', '-f', name], { stdio: 'ignore' });
  };
  process.on('exit', removeContainer);
  for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => process.exit(130));

  // ホストの環境変数は継承させない（docker CLI の動作に必要な最小限のみ）。
  const env = fake
    ? process.env
    : Object.fromEntries(
      ['PATH', 'HOME', 'DOCKER_HOST', 'DOCKER_CONFIG', 'DOCKER_CONTEXT']
        .filter((k) => process.env[k] !== undefined)
        .map((k) => [k, process.env[k]]),
    );
  const proc = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'pipe'], env });
  let errTail = '';
  let errHead = '';
  proc.stderr.on('data', (d) => {
    const s = d.toString('utf8');
    if (errHead.length < 300) errHead += s;
    errTail = (errTail + s).slice(-600);
  });
  proc.stdin.on('error', () => {});
  let exitCode = null;
  const closed = new Promise((r) => {
    proc.on('close', (c) => {
      exitCode = c;
      r();
    });
  });
  proc.on('error', () => {
    exitCode = -1;
  });
  const reader = new Reader(proc.stdout);
  const send = (tag, payload) => proc.stdin.write(frame(tag, payload));

  const state = { printed: [], store: new Map() };
  const cases = buildCases(state);
  const total = cases.length;
  let executed = 0;
  let success = 0;
  let abortReason = null;

  // ワーカーを確実に終了させ、終了を待つ（孤児プロセス・無制限受信を残さない）。
  const killWorker = async () => {
    reader.stop();
    try {
      proc.kill('SIGKILL');
    } catch {
      // 既に終了している場合は無視する。
    }
    await Promise.race([closed, new Promise((r) => setTimeout(r, 3000))]);
  };

  const diag = async (why) => {
    await Promise.race([closed, new Promise((r) => setTimeout(r, 2000))]);
    console.error(`info: ${why}; worker exit=${exitCode ?? 'killed'}; stderr tail: ${sanitize(errTail)}`);
  };

  // ハンドシェイク。
  try {
    const hello = await reader.next(timeoutMs);
    if (hello === null) throw new CrashError('eof before HELLO');
    if (hello.tag !== TAG.HELLO) throw new ProtoError(`first frame is not HELLO (tag ${hello.tag})`);
    if (hello.payload.length !== 3) throw new ProtoError('HELLO payload length is not 3');
    const ver = hello.payload.readUInt16LE(0);
    const eng = hello.payload[2];
    if (ver !== PROTOCOL_VERSION) {
      console.error(`error: worker protocol version ${ver} != ${PROTOCOL_VERSION} (this driver is hard-coded to v${PROTOCOL_VERSION})`);
      await killWorker();
      summary(total, 0, 0, 'blocked', 'hello-version-mismatch');
      return 2;
    }
    if (eng !== 0) {
      console.error(`error: worker engine byte ${eng} is not V8 (0)`);
      await killWorker();
      summary(total, 0, 0, 'blocked', 'hello-engine-mismatch');
      return 2;
    }
  } catch (e) {
    await killWorker();
    await diag(`handshake failed: ${e.message}`);
    const label = e instanceof TimeoutError ? 'timeout' : e instanceof CrashError ? 'crash' : 'protocol-error';
    summary(total, 0, 0, 'fail', `handshake-${label}`);
    return 1;
  }
  console.error(`info: worker stderr head: ${sanitize(errHead)}`);

  for (const c of cases) {
    if (abortReason) {
      console.log(`container-js-smoke: case=${c.id} category=${c.category} status=not-run`);
      continue;
    }
    executed += 1;
    let status = 'ok';
    try {
      send(c.tag, c.payload);
      const deadline = Date.now() + timeoutMs;
      let outcome = null;
      while (outcome === null) {
        // 逆方向 RPC を連続送信されてもケース期限を超えて続行しない（反復ごとに絶対期限を確認する）。
        if (Date.now() >= deadline) throw new TimeoutError('case deadline exceeded');
        const f = await reader.next(deadline - Date.now());
        if (f === null) throw new CrashError('eof during case');
        if (f.tag === TAG.NATIVE_CALL) {
          const [id, a] = decNativeCall(f.payload);
          send(TAG.NATIVE_RETURN, encNativeReturn(nativeHandler(state, id, a)));
        } else if (f.tag === TAG.RESULT) {
          const [value, off] = decValue(f.payload);
          if (off !== f.payload.length) throw new ProtoError('trailing bytes in RESULT');
          outcome = { type: 'result', value };
        } else if (f.tag === TAG.ERROR) {
          const [kind, msg] = decError(f.payload);
          outcome = { type: 'error', kind, msg };
        } else {
          throw new ProtoError(`unexpected frame tag ${f.tag}`);
        }
      }
      const verdict = c.check(outcome);
      if (verdict === true) {
        success += 1;
        if (outcome.type === 'error') console.error(`info: ${c.id}: error frame kind=${outcome.kind} message=${sanitize(outcome.msg)}`);
      } else {
        status = outcome.type === 'error' && !c.expectsError ? 'error-frame' : 'mismatch';
        console.error(`info: ${c.id}: ${verdict}; got ${sanitize(JSON.stringify(outcome))}`);
      }
    } catch (e) {
      status = e instanceof TimeoutError ? 'timeout' : e instanceof CrashError ? 'crash' : 'protocol-error';
      abortReason = status;
      await killWorker();
      await diag(`${c.id}: ${e.message}`);
    }
    console.log(`container-js-smoke: case=${c.id} category=${c.category} status=${status}`);
  }

  if (!abortReason) {
    send(TAG.SHUTDOWN);
    proc.stdin.end();
    await Promise.race([closed, new Promise((r) => setTimeout(r, 10000))]);
    if (exitCode !== 0) {
      await diag('worker did not exit cleanly after SHUTDOWN');
      summary(total, executed, success, 'fail', 'shutdown-unclean');
      return 1;
    }
  } else {
    await killWorker();
  }
  if (executed === 0) {
    summary(total, 0, 0, 'fail', 'no-cases-executed');
    return 1;
  }
  if (success === total) {
    summary(total, executed, success, 'pass', 'none');
    return 0;
  }
  summary(total, executed, success, 'fail', abortReason ?? 'case-failures');
  return 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).then(
    (c) => process.exit(c),
    (e) => {
      console.error(`error: ${sanitize(e && e.message)}`);
      process.exit(1);
    },
  );
}
