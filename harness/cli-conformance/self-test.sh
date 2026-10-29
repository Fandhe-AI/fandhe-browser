#!/usr/bin/env bash
#
# run.sh のオフライン自己テスト（TASK-49・CLI-1）。偽バイナリ（fake_cli.mjs）と偽 curl（FIXTURE_URL の事前確認用）で、
# 判定ロジックが pass / fail / 前提未充足（unmet）を正しく出し分けること、前提未充足を合格にしないこと、
# 結果 JSONL・Markdown 表の形、引数エラーの終了コードを確かめる。実バイナリ・外部ネットワークには出ない
# （launch の待ち受けは自己テスト用に選んだ空きポートの loopback のみ）。要 bash・jq・curl・node。
# 呼び出し元: scripts/harness.sh の check-cli-conformance（make check-cli-conformance）と xos-measure.yml。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN="$HERE/run.sh"
export MSYS2_ARG_CONV_EXCL='*'

for t in jq curl node; do
  command -v "$t" >/dev/null 2>&1 || { echo "error: $t is required but was not found on PATH" >&2; exit 2; }
done

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok: $1"; }
ng() { FAIL=$((FAIL + 1)); echo "NG: $1" >&2; }
expect_eq() { if [ "$2" = "$3" ]; then ok "$1"; else ng "$1 (expected=[$2] actual=[$3])"; fi; }
expect_contains() { case "$3" in *"$2"*) ok "$1" ;; *) ng "$1 (lacks [$2] in [$3])" ;; esac; }

WORK="$(cd "$(mktemp -d)" && pwd -P)"
HOLDER_PID=""
cleanup() {
  [ -z "$HOLDER_PID" ] || kill -KILL "$HOLDER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

case "$(uname -s)" in
  Linux) LABEL=LINUX; EXE="" ;;
  Darwin) LABEL=MACOS; EXE="" ;;
  MINGW* | MSYS* | CYGWIN*) LABEL=WINDOWS; EXE=".exe" ;;
  *) echo "error: unsupported OS" >&2; exit 2 ;;
esac

# 自己テスト用の空きポート（実バイナリの 9333 とぶつからないようにする）
PORT="$(node -e 'const s=require("net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})' | tr -d '\r\n')"
export CT_LAUNCH_PORT="$PORT"

# 偽バイナリ・偽描画ホスト・偽 curl
mkdir -p "$WORK/bin"
FAKE_JS="$HERE/fake_cli.mjs"
if [ "$LABEL" = WINDOWS ]; then FAKE_JS="$(cygpath -m "$FAKE_JS")"; fi
printf '#!/bin/sh\nexec node "%s" "$@"\n' "$FAKE_JS" >"$WORK/bin/fandhe-browser$EXE"
printf '#!/bin/sh\nexit 0\n' >"$WORK/bin/fake-host$EXE"
# 偽 curl: `-o FILE ... -w FMT URL` を受け、FAKE_FIXTURE に応じた HTML を FILE へ書いて HTTP ステータスを出す
cat >"$WORK/bin/fake-curl" <<'CURL'
#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  case "$1" in -o) out="$2"; shift 2 ;; *) shift ;; esac
done
case "${FAKE_FIXTURE:-ok}" in
  down) echo "curl: (6) could not resolve host" >&2; exit 6 ;;
  drift) printf '<html lang="en"><head><title>Changed</title></head><body><h1>x</h1></body></html>' >"$out" ;;
  minified) printf '<!doctype html><html lang=en><head><meta charset=utf-8><title>Example Domain</title></head><body><p>This domain is for use in documentation examples without needing permission. This is not a service; avoid relying on it for testing and monitoring purposes.</p><script src=/s.js></script></body></html>' >"$out" ;;
  noscript) printf '<html lang="en"><head><title>Example Domain</title></head><body><p>This domain is for use in documentation examples without needing permission.</p></body></html>' >"$out" ;;
  *) printf '<html lang="en"><head><title>Example Domain</title><script src="/s.js"></script></head><body><p>This domain is for use in documentation examples without needing permission.</p></body></html>' >"$out" ;;
esac
printf '200'
CURL
chmod +x "$WORK/bin/fandhe-browser$EXE" "$WORK/bin/fake-host$EXE" "$WORK/bin/fake-curl"
export CT_FIXTURE_CURL="$WORK/bin/fake-curl"

N=0
RC=0
LAST_OUT=""
# run_suite <名前> [run.sh の追加引数...]: 環境変数 FAKE_MODE・FAKE_FIXTURE は呼び出し側で渡す。結果は $WORK/<名前>.jsonl
run_suite() {
  local name="$1"
  shift
  N=$((N + 1))
  set +e
  LAST_OUT="$(bash "$RUN" --bin "$WORK/bin/fandhe-browser$EXE" --out-jsonl "$WORK/$name.jsonl" --out-md "$WORK/$name.md" \
    --commit 0123456789abcdef --timeout 20 "$@" 2>&1)"
  RC=$?
  set -e
}
# verdict <名前> <ID>: そのケースの判定。note <名前> <ID>: 差分メモ
verdict() { jq -r --arg id "$2" 'select(.type=="result" and .id==$id) | .verdict' "$WORK/$1.jsonl"; }
note_of() { jq -r --arg id "$2" 'select(.type=="result" and .id==$id) | .note' "$WORK/$1.jsonl"; }

REQ="CT-LAUNCH-$LABEL CT-NAV-$LABEL CT-QUERY-$LABEL CT-SHOT-$LABEL CT-PROFILE-$LABEL"
AUX="CT-CMD-E1 CT-CMD-E2 CT-LAUNCH-E1 CT-LAUNCH-E2 CT-NAV-E1 CT-NAV-E2 CT-NAV-E3 CT-QUERY-E1 CT-QUERY-E2 CT-QUERY-E3 CT-SHOT-E1 CT-SHOT-E2 CT-SHOT-E3 CT-SHOT-E4 CT-PROFILE-E1 CT-PROFILE-E2 CT-PROFILE-E3 CT-PROFILE-E4 CT-PROFILE-E5 CT-PROFILE-E6"
# unix では E4 は成功期待、Windows は拒否期待。E5・E6 は Windows では前提未充足（unix 専用）
if [ "$LABEL" = WINDOWS ]; then UNIX_ONLY="CT-PROFILE-E5 CT-PROFILE-E6"; else UNIX_ONLY=""; fi

# --- 1. すべて期待どおり（描画ホストあり）---
FAKE_MODE="" FAKE_FIXTURE=ok run_suite all --render-host "$WORK/bin/fake-host$EXE" --strict
expect_eq "all-ok: exit code (strict)" "0" "$RC"
for id in $REQ; do expect_eq "all-ok: $id" "pass" "$(verdict all "$id")"; done
for id in $AUX; do
  want=pass
  case " $UNIX_ONLY " in *" $id "*) want=unmet ;; esac
  expect_eq "all-ok: $id" "$want" "$(verdict all "$id")"
done
expect_eq "all-ok: result line count" "$(echo "$REQ $AUX" | wc -w | tr -d ' ')" "$(jq -s '[.[]|select(.type=="result")]|length' "$WORK/all.jsonl")"
expect_eq "all-ok: meta commit" "0123456789abcdef" "$(jq -r 'select(.type=="meta")|.commit' "$WORK/all.jsonl")"
expect_eq "all-ok: meta suite" "cli-conformance" "$(jq -r 'select(.type=="meta")|.suite' "$WORK/all.jsonl")"
expect_eq "all-ok: meta bin recorded" "true" "$(jq -r 'select(.type=="meta")|(.bin|endswith("fandhe-browser'"$EXE"'"))' "$WORK/all.jsonl")"
expect_eq "all-ok: meta render_host" "fake-host$EXE" "$(jq -r 'select(.type=="meta")|.render_host' "$WORK/all.jsonl")"
expect_eq "all-ok: NAV stdout" "status=200 url=https://example.com/" "$(jq -r --arg id "CT-NAV-$LABEL" 'select(.id==$id)|.stdout' "$WORK/all.jsonl")"
expect_eq "all-ok: NAV exit_code is a number" "0" "$(jq -r --arg id "CT-NAV-$LABEL" 'select(.id==$id)|.exit_code' "$WORK/all.jsonl")"
expect_eq "all-ok: LAUNCH exit_code is null" "null" "$(jq -r --arg id "CT-LAUNCH-$LABEL" 'select(.id==$id)|.exit_code' "$WORK/all.jsonl")"
expect_contains "all-ok: summary line" "required: pass=5 fail=0 unmet=0" "$LAST_OUT"
# Markdown 表: 仕様書 5 章の 9 列と、ケース行
expect_contains "all-ok: markdown header columns" "| ケース ID | OS・版・アーキテクチャ | commit SHA | 実行日 | 終了コード | stdout（正規化後）・stderr | 副作用の確認 | 判定 | 差分メモ |" "$(cat "$WORK/all.md")"
expect_contains "all-ok: markdown has NAV row" "| \`CT-NAV-$LABEL\` |" "$(cat "$WORK/all.md")"
expect_eq "all-ok: markdown row count" "$(echo "$REQ $AUX" | wc -w | tr -d ' ')" "$(grep -c '^| `CT-' "$WORK/all.md")"

# --- 2. 描画ホスト未指定: SHOT は前提未充足（合格にしない）、他は合格 ---
FAKE_MODE="" FAKE_FIXTURE=ok run_suite nohost --strict
expect_eq "no-host-arg: SHOT unmet" "unmet" "$(verdict nohost "CT-SHOT-$LABEL")"
expect_contains "no-host-arg: SHOT note" "render host not resolved" "$(note_of nohost "CT-SHOT-$LABEL")"
expect_eq "no-host-arg: NAV pass" "pass" "$(verdict nohost "CT-NAV-$LABEL")"
expect_eq "no-host-arg: strict exits 1 (unmet is not a pass)" "1" "$RC"
expect_contains "no-host-arg: summary" "required: pass=4 fail=0 unmet=1" "$LAST_OUT"
expect_eq "no-host-arg: SHOT-E1 still pass (fails before host resolution)" "pass" "$(verdict nohost CT-SHOT-E1)"

# --- 3. 描画ホストのシグネチャ（指定済みでも未解決を返す）---
FAKE_MODE="no-host" FAKE_FIXTURE=ok run_suite sig --render-host "$WORK/bin/fake-host$EXE"
expect_eq "host-signature: SHOT unmet" "unmet" "$(verdict sig "CT-SHOT-$LABEL")"

# --- 4. 各コマンドの不一致は fail ---
FAKE_MODE="bad-title" FAKE_FIXTURE=ok run_suite badtitle --render-host "$WORK/bin/fake-host$EXE" --strict
expect_eq "bad-title: QUERY fail" "fail" "$(verdict badtitle "CT-QUERY-$LABEL")"
expect_contains "bad-title: QUERY note" "stdout mismatch" "$(note_of badtitle "CT-QUERY-$LABEL")"
expect_eq "bad-title: NAV still pass" "pass" "$(verdict badtitle "CT-NAV-$LABEL")"
expect_eq "bad-title: strict exits 1" "1" "$RC"
FAKE_MODE="bad-nav-exit" FAKE_FIXTURE=ok run_suite badnav --render-host "$WORK/bin/fake-host$EXE"
expect_eq "bad-nav-exit: NAV fail" "fail" "$(verdict badnav "CT-NAV-$LABEL")"
expect_contains "bad-nav-exit: note" "exit code 3 (expected 0)" "$(note_of badnav "CT-NAV-$LABEL")"
expect_eq "bad-nav-exit: exits 0 without --strict (recording done)" "0" "$RC"
FAKE_MODE="bad-devtools" FAKE_FIXTURE=ok run_suite baddev --render-host "$WORK/bin/fake-host$EXE"
expect_eq "bad-devtools: LAUNCH fail" "fail" "$(verdict baddev "CT-LAUNCH-$LABEL")"
FAKE_MODE="bad-png" FAKE_FIXTURE=ok run_suite badpng --render-host "$WORK/bin/fake-host$EXE"
expect_eq "bad-png: SHOT fail" "fail" "$(verdict badpng "CT-SHOT-$LABEL")"
expect_contains "bad-png: SHOT note" "IHDR dimensions mismatch" "$(note_of badpng "CT-SHOT-$LABEL")"

# --- 5. CRLF は正規化して合格にし、差分メモに残す（仕様書 2.4）---
FAKE_MODE="crlf" FAKE_FIXTURE=ok run_suite crlf --render-host "$WORK/bin/fake-host$EXE"
expect_eq "crlf: NAV pass after normalization" "pass" "$(verdict crlf "CT-NAV-$LABEL")"
expect_contains "crlf: NAV note records CRLF" "CRLF" "$(note_of crlf "CT-NAV-$LABEL")"
expect_eq "crlf: NAV stdout has no CR" "status=200 url=https://example.com/" "$(jq -r --arg id "CT-NAV-$LABEL" 'select(.id==$id)|.stdout' "$WORK/crlf.jsonl")"

# --- 6. FIXTURE_URL の事前確認（仕様書 2.3）---
FAKE_MODE="" FAKE_FIXTURE=drift run_suite drift --render-host "$WORK/bin/fake-host$EXE" --strict
for id in "CT-NAV-$LABEL" "CT-QUERY-$LABEL" "CT-SHOT-$LABEL" CT-QUERY-E3; do expect_eq "fixture-drift: $id unmet" "unmet" "$(verdict drift "$id")"; done
expect_eq "fixture-drift: LAUNCH unaffected" "pass" "$(verdict drift "CT-LAUNCH-$LABEL")"
expect_eq "fixture-drift: PROFILE unaffected" "pass" "$(verdict drift "CT-PROFILE-$LABEL")"
expect_eq "fixture-drift: strict exits 1" "1" "$RC"
expect_eq "fixture-drift: meta fixture_ok false" "false" "$(jq -r 'select(.type=="meta")|.fixture_ok' "$WORK/drift.jsonl")"
FAKE_MODE="" FAKE_FIXTURE=down run_suite down --render-host "$WORK/bin/fake-host$EXE"
expect_eq "fixture-down: NAV unmet" "unmet" "$(verdict down "CT-NAV-$LABEL")"
FAKE_MODE="" FAKE_FIXTURE=noscript run_suite noscript --render-host "$WORK/bin/fake-host$EXE"
expect_eq "fixture-noscript: QUERY unmet (src check only)" "unmet" "$(verdict noscript "CT-QUERY-$LABEL")"
expect_eq "fixture-noscript: NAV pass" "pass" "$(verdict noscript "CT-NAV-$LABEL")"

# example.com が実際に返す最小化 HTML（属性値の引用符なし: `<html lang=en>`・`<script src=/s.js>`）でも事前確認を通る
FAKE_MODE="" FAKE_FIXTURE=minified run_suite minified --render-host "$WORK/bin/fake-host$EXE"
expect_eq "fixture-minified: meta fixture_ok true" "true" "$(jq -r 'select(.type=="meta")|.fixture_ok' "$WORK/minified.jsonl")"
expect_eq "fixture-minified: meta fixture_detail" "ok" "$(jq -r 'select(.type=="meta")|.fixture_detail' "$WORK/minified.jsonl")"
expect_eq "fixture-minified: QUERY pass" "pass" "$(verdict minified "CT-QUERY-$LABEL")"

# --- 7. ポート使用中: LAUNCH は前提未充足 ---
node -e 'const s=require("net").createServer();s.listen(Number(process.argv[1]),"127.0.0.1");setInterval(()=>{try{process.kill(process.ppid,0)}catch{process.exit(0)}},200)' "$PORT" &
HOLDER_PID=$!
for _ in $(seq 1 25); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.2; done
FAKE_MODE="" FAKE_FIXTURE=ok run_suite busy --render-host "$WORK/bin/fake-host$EXE"
expect_eq "port-busy: LAUNCH unmet" "unmet" "$(verdict busy "CT-LAUNCH-$LABEL")"
expect_eq "port-busy: LAUNCH-E2 unmet" "unmet" "$(verdict busy CT-LAUNCH-E2)"
expect_eq "port-busy: PROFILE-E3 unmet" "unmet" "$(verdict busy CT-PROFILE-E3)"
expect_eq "port-busy: NAV unaffected" "pass" "$(verdict busy "CT-NAV-$LABEL")"
kill -KILL "$HOLDER_PID" 2>/dev/null || true
wait "$HOLDER_PID" 2>/dev/null || true
HOLDER_PID=""

# --- 8. --only と引数エラー ---
FAKE_MODE="" FAKE_FIXTURE=ok run_suite only --only "CT-NAV-$LABEL,CT-CMD-E1"
expect_eq "only: two results" "2" "$(jq -s '[.[]|select(.type=="result")]|length' "$WORK/only.jsonl")"
expect_eq "only: NAV pass" "pass" "$(verdict only "CT-NAV-$LABEL")"
set +e
bash "$RUN" --bin "$WORK/nonexistent" >/dev/null 2>&1; rc_a=$?
bash "$RUN" --bogus >/dev/null 2>&1; rc_b=$?
bash "$RUN" --bin "$WORK/bin/fandhe-browser$EXE" --timeout 0 >/dev/null 2>&1; rc_c=$?
bash "$RUN" --bin "$WORK/bin/fandhe-browser$EXE" --render-host "$WORK/nonexistent" >/dev/null 2>&1; rc_d=$?
bash "$RUN" --bin "$WORK/bin/fandhe-browser$EXE" --only 'bad id;' >/dev/null 2>&1; rc_e=$?
set -e
expect_eq "args: missing binary exits 2" "2" "$rc_a"
expect_eq "args: unknown option exits 2" "2" "$rc_b"
expect_eq "args: bad --timeout exits 2" "2" "$rc_c"
expect_eq "args: missing render host exits 2" "2" "$rc_d"
expect_eq "args: bad --only exits 2" "2" "$rc_e"

echo "cases=$((PASS + FAIL)) failures=$FAIL"
[ "$FAIL" -eq 0 ]
