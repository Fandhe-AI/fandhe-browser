// self-test.sh 用の偽 JS ワーカー（TASK-66.3・Issue #304）。
//
// 役割: 実コンテナ・docker なしで driver.mjs の判定ロジック（成功・各種失敗の分類・exit code）を
// 検証するため、ワーカープロトコル v5 の子側を固定スクリプトの対応表で模擬する。V8 は動かさない。
// 呼び出し元: driver.mjs（環境変数 CONTAINER_JS_SMOKE_FAKE_WORKER_FOR_TEST 指定時に起動）。
// 第 1 引数のモードで異常系を再現する:
//   ok / version（HELLO の版不一致）/ boa（エンジン不一致）/ badtag（未知 tag）/ huge（過大な長さ）/
//   eof（評価中に終了）/ hang（無応答）/ mismatch（RESULT の値不一致）/ badutf8（不正 UTF-8 の文字列）/
//   nohello（HELLO を送らず無応答）/ dirtyexit（SHUTDOWN 後に非 0 で終了）
import { Reader, TAG, SCRIPTS, NATIVE_ID, frame, encValue, MAX_FRAME_PAYLOAD_CHILD_TO_PARENT } from './driver.mjs';

const mode = process.argv[2] ?? 'ok';
const out = (tag, payload) => process.stdout.write(frame(tag, payload));
const result = (v) => out(TAG.RESULT, encValue(v));
const error = (kind, msg) => out(TAG.ERROR, Buffer.concat([Buffer.from([kind]), Buffer.from(msg, 'utf8')]));
const reader = new Reader(process.stdin, MAX_FRAME_PAYLOAD_CHILD_TO_PARENT);

if (mode === 'nohello') {
  setInterval(() => {}, 1000);
} else {
  const hello = Buffer.alloc(3);
  hello.writeUInt16LE(mode === 'version' ? 4 : 5, 0);
  hello[2] = mode === 'boa' ? 1 : 0;
  out(TAG.HELLO, hello);
  run().catch(() => process.exit(9));
}

// 親へ NATIVE_CALL を送り、NATIVE_RETURN の戻り値（Ok 値 | Err メッセージ）を返す。
async function call(id, args) {
  const head = Buffer.alloc(8);
  head.writeUInt32LE(id, 0);
  head.writeUInt32LE(args.length, 4);
  out(TAG.NATIVE_CALL, Buffer.concat([head, ...args.map(encValue)]));
  const f = await reader.next(10000);
  if (f === null || f.tag !== TAG.NATIVE_RETURN) throw new Error('expected NATIVE_RETURN');
  return f.payload[0] === 0 ? { ok: true } : { err: f.payload.subarray(1).toString('utf8') };
}

// 模擬する取得系の戻り値は固定（dom ストアの実体はドライバ側にあるため、値は対応表で返す）。
async function evaluate(script, first) {
  if (first && mode === 'badtag') return out(99, Buffer.alloc(0));
  if (first && mode === 'huge') return process.stdout.write(Buffer.from([0xf0, 0xff, 0xff, 0xff]));
  if (first && mode === 'eof') process.exit(3);
  if (first && mode === 'hang') return setInterval(() => {}, 1000);
  switch (script) {
    case SCRIPTS.arith: return result(mode === 'mismatch' ? 8 : 7);
    case SCRIPTS.string:
      if (mode === 'badutf8') return out(TAG.RESULT, Buffer.from([4, 2, 0, 0, 0, 0xc3, 0x28]));
      return result('fandhe-browser');
    case SCRIPTS.bool: return result(true);
    case SCRIPTS.nul: return result(null);
    case SCRIPTS.undef: return result(undefined);
    case SCRIPTS.syntax: return error(0, 'Uncaught SyntaxError: Unexpected end of input (line 1)');
    case SCRIPTS.thrown: return error(0, 'Uncaught Error: boom (line 1)');
    case SCRIPTS.print: await call(NATIVE_ID.print, ['hello']); return result(undefined);
    case SCRIPTS.add: await call(NATIVE_ID.add, [2, 3]); return result(5);
    case SCRIPTS.fails: await call(NATIVE_ID.alwaysFails, []); return error(0, 'Uncaught Error: always fails (line 1)');
    case SCRIPTS.domRoundtrip:
      await call(NATIVE_ID.setText, ['title', 'Fandhe']);
      await call(NATIVE_ID.getText, ['title']);
      return result('Fandhe');
    case SCRIPTS.domCount:
      await call(NATIVE_ID.setText, ['body', 'x']);
      await call(NATIVE_ID.count, []);
      return result(2);
    case SCRIPTS.domMissing: await call(NATIVE_ID.getText, ['missing']); return result('');
    default: return error(0, 'unknown script');
  }
}

async function run() {
  let first = true;
  for (;;) {
    const f = await reader.next(60000);
    if (f === null) process.exit(0);
    if (f.tag === TAG.SHUTDOWN) process.exit(mode === 'dirtyexit' ? 4 : 0);
    if (f.tag === TAG.REGISTER_GLOBAL_FUNCTION || f.tag === TAG.BIND_DOM_LIKE_OBJECT) {
      result(undefined);
    } else if (f.tag === TAG.EVALUATE) {
      await evaluate(f.payload.subarray(16).toString('utf8'), first);
      first = false;
    } else {
      process.exit(8);
    }
  }
}
