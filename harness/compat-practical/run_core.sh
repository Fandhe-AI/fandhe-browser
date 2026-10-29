#!/usr/bin/env bash
#
# 28 タスクを実装リポの `fandhe-browser` バイナリ（CDP サーバー）で実行し、成否を JSONL に記録する
# （TASK-71.2・MEAS-4。関連 JS-2・COMPAT-4・REPAIR-3・SEC-2）。
# 結果は matrix.json 生成（TASK-71.3・#312）が id で Chromium 実測と突合し、測定レポート（#309）の入力になる。
#
# 実行経路は CDP クライアント方式。CLI サブコマンド（TASK-47）は未実装で、バイナリは引数なしで
# 127.0.0.1:9333 に CDP サーバーを立てるだけのため、run_core.mjs が Page.navigate・DOM.* を送る。
# ページ内 JS（JS-4〜JS-6）は既定オフ。--page-js <v8|boa> を付けると、固定文面の設定ファイル
# （[js] engine=<..>, execute_page_scripts=true）を FANDHE_BROWSER_CONFIG で渡して起動する。
# --page-modules を足すと同じ設定へ execute_modules=true も加える（ES module 実行。JS-2・#1119。--page-js が必須）。
# 付けない起動では呼び出し元の FANDHE_BROWSER_CONFIG を外し、meta の page_js_executed=false を事実と一致させる（REPAIR-3）。
# page_js_executed は「実行を有効にして起動した」意味で、ページごとの実行成否ではない。--no-spawn では確認できないため併用不可。
#
# 使い方: run_core.sh [--tasks P] [--out P] [--bin P] [--endpoint URL] [--no-spawn]
#                     [--page-js v8|boa [--page-modules]] [--log ABS_PATH] [--task-timeout SEC] [--total-timeout SEC] [--startup-timeout SEC] [--validate-only]
#                     [--windows-localappdata-profile]
# --windows-localappdata-profile は Windows の起動モード専用の明示オプトイン（xos-measure.yml の手動計測用）。Windows のバイナリは
# XDG_DATA_HOME・HOME を見ず `%LOCALAPPDATA%` 配下をプロファイルに使うため既定では起動不可（exit 2）のままにし、このオプションで
# `launch --profile-dir %LOCALAPPDATA%\fandhe-ct-<pid>-<rand>\p`（XOS-7・XOS-10）へ隔離して起動する。終了時にその一時領域だけを消す。
# --startup-timeout は起動モードでは起動待ち、--no-spawn では接続確認の予算（拒否・2xx 以外は即失敗し理由を出力）。
# 終了コード: 0=記録完了（失敗タスクがあっても 0。成否は計測結果でありゲートではない）
#            1=記録の書き込み失敗・結果件数/id 集合の不一致
#            2=入力・使用エラー（スキーマ違反・引数不正・jq/node 無し・バイナリ検証/起動失敗）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

TASKS="$SCRIPT_DIR/tasks.json"
OUT="$SCRIPT_DIR/results/core_results.jsonl"
ENDPOINT="http://127.0.0.1:9333"
BIN_ARG=""
NO_SPAWN=0
PAGE_JS=""
PAGE_MODULES=0
LOG_OUT=""
WIN_PROFILE=0
VALIDATE_ONLY=0
TASK_TIMEOUT=45
TOTAL_TIMEOUT=1200
STARTUP_TIMEOUT=15

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --tasks) [ $# -ge 2 ] || die "--tasks requires a value"; TASKS="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out requires a value"; OUT="$2"; shift 2 ;;
    --bin) [ $# -ge 2 ] || die "--bin requires a value"; BIN_ARG="$2"; shift 2 ;;
    --endpoint) [ $# -ge 2 ] || die "--endpoint requires a value"; ENDPOINT="$2"; shift 2 ;;
    --task-timeout) [ $# -ge 2 ] || die "--task-timeout requires a value"; TASK_TIMEOUT="$2"; shift 2 ;;
    --total-timeout) [ $# -ge 2 ] || die "--total-timeout requires a value"; TOTAL_TIMEOUT="$2"; shift 2 ;;
    --startup-timeout) [ $# -ge 2 ] || die "--startup-timeout requires a value"; STARTUP_TIMEOUT="$2"; shift 2 ;;
    --page-js) [ $# -ge 2 ] || die "--page-js requires a value"; PAGE_JS="$2"; shift 2 ;;
    --page-modules) PAGE_MODULES=1; shift ;;
    --log) [ $# -ge 2 ] || die "--log requires a value"; LOG_OUT="$2"; shift 2 ;;
    --no-spawn) NO_SPAWN=1; shift ;;
    --windows-localappdata-profile) WIN_PROFILE=1; shift ;;
    --validate-only) VALIDATE_ONLY=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

type -P jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"
in_range() { [[ "$1" =~ ^[1-9][0-9]{0,3}$ ]] && [ "$1" -ge 1 ] && [ "$1" -le "$2" ]; }
in_range "$TASK_TIMEOUT" 120 || die "--task-timeout must be an integer in 1..120"
in_range "$TOTAL_TIMEOUT" 3600 || die "--total-timeout must be an integer in 1..3600"
in_range "$STARTUP_TIMEOUT" 60 || die "--startup-timeout must be an integer in 1..60"
# 取得先は loopback の CDP サーバーに限る（SEC-4。接続先をスクリプト引数で外へ向けさせない）
if ! [[ "$ENDPOINT" =~ ^http://127\.0\.0\.1:([1-9][0-9]{0,4})$ ]] || [ "${BASH_REMATCH[1]}" -gt 65535 ]; then
  die "--endpoint must be http://127.0.0.1:<port>"
fi

# --page-js は列挙値のみ（設定ファイルは固定文面で、引数を連結しない）。起動モード専用
case "$PAGE_JS" in
  "" | v8 | boa) ;;
  *) die "--page-js must be v8 or boa" ;;
esac
[ -z "$PAGE_JS" ] || [ "$NO_SPAWN" -eq 0 ] || die "--page-js cannot be combined with --no-spawn (server configuration is not verifiable)"
# --page-modules は真偽フラグで --page-js が前提（module は ES module 実行を有効にしたページ JS 実行の一部）
[ "$PAGE_MODULES" -eq 0 ] || [ -n "$PAGE_JS" ] || die "--page-modules requires --page-js"
if [ -n "$LOG_OUT" ]; then
  [ "$NO_SPAWN" -eq 0 ] || die "--log cannot be combined with --no-spawn"
  case "$LOG_OUT" in /*) ;; *) die "--log must be an absolute path" ;; esac
fi

# Windows（Git Bash 等）かどうか。ネイティブバイナリの起動可否とパス表記の分岐に使う
IS_WINDOWS=0
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) IS_WINDOWS=1 ;; esac
if [ "$WIN_PROFILE" -eq 1 ]; then
  [ "$IS_WINDOWS" -eq 1 ] || die "--windows-localappdata-profile is only supported on Windows"
  [ "$NO_SPAWN" -eq 0 ] || die "--windows-localappdata-profile cannot be combined with --no-spawn"
fi

# tasks.json の検証は access_check.sh と同一規則（重複実装しない）。違反は exit 2 で伝わる
[ -f "$TASKS" ] || die "tasks file not found: $TASKS"
bash "$SCRIPT_DIR/access_check.sh" --tasks "$TASKS" --validate-only >/dev/null || die "tasks file validation failed: $TASKS"
COUNT=$(jq length "$TASKS")
if [ "$VALIDATE_ONLY" -eq 1 ]; then
  echo "ok: $COUNT tasks validated"
  exit 0
fi

# node の存在と組み込み WebSocket（Node 22 以降）を確認する。無ければ成功を装わず中止する
type -P node >/dev/null 2>&1 || die "node is required but was not found on PATH"
node -e 'process.exit(typeof WebSocket === "function" ? 0 : 1)' || die "node has no built-in WebSocket (Node 22+ is required)"

# endpoint の /json/version が 2xx で応答するか（応答すれば 0）。curl に依存せず node の組み込み http で判定する。
# 以前は fetch + AbortSignal.timeout(1500) の単発だったが、Windows ランナーで fetch の初回ロード（undici の遅延
# 読み込み）と接続確立が 1.5 秒を超えて abort し、健全な偽サーバーを「無応答」と誤判定した疑いがある（#956。
# 同じ偽サーバーが約 4 秒後に 22 件を処理できていた）。期限は要求を作った後に数え、1 回の上限を 3 秒とする。
# 引数 $1（任意）は全体の期限（epoch ミリ秒）。指定時は試行前に残り時間を確認し、1 回の上限を min(3000ms, 残り) にし、
# 成功時も期限内であることを確認する（期限後の応答を成功扱いにしない）。
# 終了コード: 0=2xx / 3=接続拒否 / 4=期限切れ / 5=2xx 以外の HTTP / 6=その他 / 7=全体の期限超過（試行せず）。
# 失敗理由（英語の固定書式。接続先の本文は出さない）は PROBE_REASON に入る。
PROBE_REASON=""
probe() {
  local rc raw deadline="${1:-0}"
  set +e
  raw=$(node -e '
const http = require("node:http");
const deadline = Number(process.argv[2]) || 0;
const remain = deadline > 0 ? deadline - Date.now() : Infinity;
const ms = Math.min(3000, remain);
const done = (code, msg) => { if (msg) console.error(msg); process.exit(code); };
if (!(ms > 0)) done(7, "deadline exceeded");
const req = http.get(process.argv[1] + "/json/version", (res) => {
  res.resume();
  const s = res.statusCode;
  if (s >= 200 && s < 300) {
    if (deadline > 0 && Date.now() > deadline) done(7, "deadline exceeded");
    done(0);
  }
  else done(5, "http status " + s);
});
setTimeout(() => done(4, "timeout after " + Math.round(ms) + "ms"), ms);
req.on("error", (e) => {
  const c = String((e && e.code) || "unknown").replace(/[^A-Za-z0-9_]/g, "").slice(0, 64);
  if (c === "ECONNREFUSED") done(3, "refused");
  else done(6, "error " + c);
});
' "$ENDPOINT" "$deadline" 2>&1 >/dev/null; echo "@rc=$?")
  # サブシェル内の終了コードを末尾マーカーで受け取る（PIPESTATUS はコマンド置換の外へ出ない）
  rc=$(printf '%s' "${raw##*@rc=}" | tr -d '\r\n')
  PROBE_REASON=$(printf '%s' "${raw%@rc=*}" | tr -d '\000-\037\177' | cut -c1-120)
  case "$rc" in '' | *[!0-9]*) rc=6 ;; esac
  set -e
  return "$rc"
}

# --no-spawn 用の接続確認。--startup-timeout を予算に、期限切れ・その他の失敗だけを有界リトライする（#956）。
# 接続拒否（待ち受けが無い）と 2xx 以外は待っても好転しないため即失敗し、成功を装わない（REPAIR-5）。
wait_ready_no_spawn() {
  local start=$SECONDS attempts=0 rc=0 deadline last=""
  # 期限はミリ秒精度の epoch で保持する（SECONDS は秒単位で --startup-timeout 1 では粗すぎる）
  deadline=$(node -e 'process.stdout.write(String(Date.now() + Number(process.argv[1]) * 1000))' "$STARTUP_TIMEOUT") || die "cannot compute deadline"
  while :; do
    attempts=$((attempts + 1))
    probe "$deadline" && return 0
    rc=$?
    # 期限超過（7）は直前の実際の失敗理由を隠さない（試行せず打ち切った場合は last を保持する）
    if [ "$rc" -ne 7 ] || [ -z "$last" ]; then last="$PROBE_REASON"; fi
    if [ "$rc" -ne 4 ] && [ "$rc" -ne 6 ]; then break; fi
    # 予算を使い切った後は待たずに打ち切る（last を確実に実際の失敗理由のまま報告する）
    node -e 'process.exit(Date.now() >= Number(process.argv[1]) ? 0 : 1)' "$deadline" && break
    sleep 0.3
  done
  die "endpoint is not responding: $ENDPOINT (attempts=$attempts, elapsed=$((SECONDS - start))s, last=${last:-unknown})"
}

PID=""
PROFILE_DIR=""
CONF_DIR=""
OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR" || { echo "error: cannot create $OUT_DIR" >&2; exit 1; }
TMP="$(mktemp "$OUT_DIR/.core_results.XXXXXX")" || { echo "error: cannot create temp file" >&2; exit 1; }
RAW="$(mktemp)" || { rm -f "$TMP"; echo "error: cannot create temp file" >&2; exit 1; }
LOG="$(mktemp)" || { rm -f "$TMP" "$RAW"; echo "error: cannot create temp file" >&2; exit 1; }

cleanup() {
  if [ -n "$PID" ]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  [ -z "$PROFILE_DIR" ] || rm -rf "$PROFILE_DIR"
  [ -z "$CONF_DIR" ] || rm -rf "$CONF_DIR"
  rm -f "$TMP" "$RAW" "$LOG"
}
trap cleanup EXIT

BIN_RESOLVED=""
if [ "$NO_SPAWN" -eq 0 ]; then
  if [ "$IS_WINDOWS" -eq 1 ] && [ "$WIN_PROFILE" -eq 0 ]; then
    # 計測用プロファイルは XDG_DATA_HOME・HOME の一時ディレクトリで隔離しているが、Windows の
    # バイナリはこれらを見ず `%LOCALAPPDATA%` 配下を使うため、起動すると利用者の実プロファイルに
    # 触れて隔離の約束が崩れる。成功を装わず fail-closed にする（隔離して起動するなら
    # --windows-localappdata-profile でオプトインする）
    die "spawning the binary is not supported on Windows yet (profile isolation via XDG_DATA_HOME/HOME does not apply on Windows); use --no-spawn or --windows-localappdata-profile"
  fi
  # バイナリは待ち受けポートを引数・環境変数で変更できず 9333 固定のため、別ポートでは readiness を観測できない。
  # 起動モードでは 9333 以外を拒否する（別ポートの既存サーバーへ接続するなら --no-spawn を使う）
  # FC_TEST_ALLOW_SPAWN_PORT=1 は self-test の偽バイナリ専用（実バイナリでは設定しない）
  [ "$ENDPOINT" = "http://127.0.0.1:9333" ] || [ "${FC_TEST_ALLOW_SPAWN_PORT:-}" = "1" ] || die "spawn mode supports only http://127.0.0.1:9333 (the binary listens on a fixed port); use --no-spawn for other ports"
  BIN_RESOLVED="$(resolve_bin "$BIN_ARG")" || exit 2
  # 固定ポートのため、起動前に応答があれば他プロセスを誤計測しないよう中止する（fail-closed）
  if probe; then die "endpoint is already responding: $ENDPOINT (stop the other process first)"; fi
  # 利用者の実プロファイルに触れず Locked 衝突も避けるため、計測用プロファイルを一時ディレクトリへ隔離する
  SPAWN_ARGS=()
  if [ "$IS_WINDOWS" -eq 1 ]; then
    # Windows のバイナリは `%LOCALAPPDATA%` 配下のプロファイルだけを受け付ける（XOS-7・XOS-10）。一意名の
    # 一時領域を作り、`launch --profile-dir` で明示する。利用者の既定プロファイルには触れない
    [ -n "${LOCALAPPDATA:-}" ] || die "LOCALAPPDATA is not set"
    PROFILE_DIR="$(cygpath -u "$LOCALAPPDATA")/fandhe-ct-$$-$RANDOM" || die "cannot resolve LOCALAPPDATA"
    mkdir "$PROFILE_DIR" || die "cannot create profile dir"
    SPAWN_ARGS=(launch --profile-dir "$(cygpath -w "$PROFILE_DIR/p")")
  else
    PROFILE_DIR="$(mktemp -d)" || die "cannot create profile dir"
  fi
  if [ -n "$PAGE_JS" ]; then
    CONF_DIR="$(mktemp -d)" || die "cannot create config dir"
    CONF_PATH="$CONF_DIR/config.toml"
    # [profile] は書かない（cli は profile root を含む設定を拒否する）。エンジン未同梱のバイナリは起動前に非 0 終了する
    printf '[js]\nengine = "%s"\nexecute_page_scripts = true\n' "$PAGE_JS" >"$CONF_PATH"
    # execute_modules は真偽の固定行のみ追記する（引数文字列は連結しない）
    if [ "$PAGE_MODULES" -eq 1 ]; then printf 'execute_modules = true\n' >>"$CONF_PATH"; fi
    # ネイティブの Windows バイナリは MSYS 形式のパスを解釈できないため、混在形式（C:/...）へ変換して渡す
    if [ "$IS_WINDOWS" -eq 1 ]; then CONF_PATH="$(cygpath -m "$CONF_PATH")"; fi
    env XDG_DATA_HOME="$PROFILE_DIR" HOME="$PROFILE_DIR" FANDHE_BROWSER_CONFIG="$CONF_PATH" "$BIN_RESOLVED" ${SPAWN_ARGS[@]+"${SPAWN_ARGS[@]}"} >"$LOG" 2>&1 &
  else
    env -u FANDHE_BROWSER_CONFIG XDG_DATA_HOME="$PROFILE_DIR" HOME="$PROFILE_DIR" "$BIN_RESOLVED" ${SPAWN_ARGS[@]+"${SPAWN_ARGS[@]}"} >"$LOG" 2>&1 &
  fi
  PID=$!
  start=$SECONDS
  until probe; do
    if ! kill -0 "$PID" 2>/dev/null; then
      PID=""
      echo "error: binary exited before the endpoint became ready" >&2
      tail -n 5 "$LOG" | tr -d '\000-\010\013-\037\177' | cut -c1-200 >&2 || true
      exit 2
    fi
    if [ $((SECONDS - start)) -ge "$STARTUP_TIMEOUT" ]; then
      echo "error: endpoint did not become ready within ${STARTUP_TIMEOUT}s" >&2
      tail -n 5 "$LOG" | tr -d '\000-\010\013-\037\177' | cut -c1-200 >&2 || true
      exit 2
    fi
    sleep 0.3
  done
else
  wait_ready_no_spawn
fi

# Node クライアントを 1 回起動して全タスクを直列に実行する。URL は環境変数で渡す
set +e
FC_ENDPOINT="$ENDPOINT" FC_TASKS="$TASKS" FC_TASK_TIMEOUT_MS=$((TASK_TIMEOUT * 1000)) \
  FC_TOTAL_TIMEOUT_MS=$((TOTAL_TIMEOUT * 1000)) node "$SCRIPT_DIR/run_core.mjs" >"$RAW"
node_status=$?
set -e
[ "$node_status" -eq 0 ] || die "task runner failed (exit $node_status)"

# 結果行がタスク数・id 集合と一致しなければ完了扱いにしない
got_ids=$(jq -r 'select(.type=="result") | .id' "$RAW" | LC_ALL=C sort | tr '\n' ' ')
want_ids=$(jq -r '.[].id' "$TASKS" | LC_ALL=C sort | tr '\n' ' ')
if [ "$got_ids" != "$want_ids" ]; then
  echo "error: result ids do not match tasks (got: $got_ids)" >&2
  exit 1
fi

# コミットする実測にユーザー名を含むホームパスが入らないよう、リポジトリ配下ならリポ相対で記録する
BIN_REC="$BIN_RESOLVED"
case "$BIN_REC" in "$REPO_ROOT"/*) BIN_REC="${BIN_REC#"$REPO_ROOT"/}" ;; esac
CDP_BROWSER=$(jq -r 'select(.type=="browser") | .cdp_browser' "$RAW" | head -n 1)

MSYS2_ARG_CONV_EXCL='*' jq -cn --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg sha "$(sha256_of "$TASKS")" \
  --arg bin "$BIN_REC" --arg ep "$ENDPOINT" --arg cb "$CDP_BROWSER" --arg os "$(uname -s)" \
  --arg pjs "$PAGE_JS" --argjson pm "$PAGE_MODULES" --argjson tt "$TASK_TIMEOUT" --argjson tot "$TOTAL_TIMEOUT" \
  '{type:"meta",schema_version:1,measured_at:$at,tasks_sha256:$sha,bin:(if $bin=="" then null else $bin end),endpoint:$ep,cdp_browser:$cb,driver:"cdp",page_js_executed:($pjs!=""),page_js_engine:(if $pjs=="" then null else $pjs end),page_modules_executed:($pm==1),task_timeout_sec:$tt,total_timeout_sec:$tot,os:$os}' >"$TMP"
jq -c 'select(.type=="result")' "$RAW" >>"$TMP"

# 起動したバイナリの stdout / stderr を残す（固定診断行 `cdp: page scripts: ...` の回収用。非信頼データでコミットしない）
if [ -n "$LOG_OUT" ]; then
  cp -f "$LOG" "$LOG_OUT" || { echo "error: cannot write $LOG_OUT" >&2; exit 1; }
fi
mv -f "$TMP" "$OUT" || { echo "error: cannot write $OUT" >&2; exit 1; }
SUCCESS=$(jq -s '[.[] | select(.type=="result" and .success==true)] | length' "$OUT")
echo "success=$SUCCESS/$COUNT"
