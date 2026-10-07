#!/usr/bin/env bash
#
# 22 タスクを実装リポの `fandhe-browser` バイナリ（CDP サーバー）で実行し、成否を JSONL に記録する
# （TASK-71.2・MEAS-4。関連 JS-2・COMPAT-4・REPAIR-3・SEC-2）。
# 結果は matrix.json 生成（TASK-71.3・#312）が id で Chromium 実測と突合し、測定レポート（#309）の入力になる。
#
# 実行経路は CDP クライアント方式。CLI サブコマンド（TASK-47）は未実装で、バイナリは引数なしで
# 127.0.0.1:9333 に CDP サーバーを立てるだけのため、run_core.mjs が Page.navigate・DOM.* を送る。
# 重要（REPAIR-3）: 現状 Page.navigate は fetch して HTML を保存するだけでページ内 JS は実行されない
# （JS エンジンはナビゲーション経路に未配線）。b5・d2・d3 の「V8 統合による解消」は示せず、
# 結果は meta の page_js_executed=false とともに実測のまま記録する。
#
# 使い方: run_core.sh [--tasks P] [--out P] [--bin P] [--endpoint URL] [--no-spawn]
#                     [--task-timeout SEC] [--total-timeout SEC] [--startup-timeout SEC] [--validate-only]
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
    --no-spawn) NO_SPAWN=1; shift ;;
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

# endpoint の /json/version が応答するか（応答すれば 0）。判定は node の fetch で行い curl に依存しない
probe() {
  node -e 'fetch(process.argv[1] + "/json/version", { signal: AbortSignal.timeout(1500) }).then((r) => process.exit(r.ok ? 0 : 1), () => process.exit(1))' "$ENDPOINT" >/dev/null 2>&1
}

PID=""
PROFILE_DIR=""
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
  rm -f "$TMP" "$RAW" "$LOG"
}
trap cleanup EXIT

BIN_RESOLVED=""
if [ "$NO_SPAWN" -eq 0 ]; then
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      # Windows は Profile::open が Unsupported（XOS-7〜XOS-10 待ち）で起動できない。成功を装わない
      die "spawning the binary is not supported on Windows yet (profile ACL pending); use --no-spawn" ;;
  esac
  # バイナリは待ち受けポートを引数・環境変数で変更できず 9333 固定のため、別ポートでは readiness を観測できない。
  # 起動モードでは 9333 以外を拒否する（別ポートの既存サーバーへ接続するなら --no-spawn を使う）
  # FC_TEST_ALLOW_SPAWN_PORT=1 は self-test の偽バイナリ専用（実バイナリでは設定しない）
  [ "$ENDPOINT" = "http://127.0.0.1:9333" ] || [ "${FC_TEST_ALLOW_SPAWN_PORT:-}" = "1" ] || die "spawn mode supports only http://127.0.0.1:9333 (the binary listens on a fixed port); use --no-spawn for other ports"
  BIN_RESOLVED="$(resolve_bin "$BIN_ARG")" || exit 2
  # 固定ポートのため、起動前に応答があれば他プロセスを誤計測しないよう中止する（fail-closed）
  if probe; then die "endpoint is already responding: $ENDPOINT (stop the other process first)"; fi
  # 利用者の実プロファイルに触れず Locked 衝突も避けるため、計測用プロファイルを一時ディレクトリへ隔離する
  PROFILE_DIR="$(mktemp -d)" || die "cannot create profile dir"
  env XDG_DATA_HOME="$PROFILE_DIR" HOME="$PROFILE_DIR" "$BIN_RESOLVED" >"$LOG" 2>&1 &
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
  probe || die "endpoint is not responding: $ENDPOINT"
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
  --argjson tt "$TASK_TIMEOUT" --argjson tot "$TOTAL_TIMEOUT" \
  '{type:"meta",schema_version:1,measured_at:$at,tasks_sha256:$sha,bin:(if $bin=="" then null else $bin end),endpoint:$ep,cdp_browser:$cb,driver:"cdp",page_js_executed:false,task_timeout_sec:$tt,total_timeout_sec:$tot,os:$os}' >"$TMP"
jq -c 'select(.type=="result")' "$RAW" >>"$TMP"

mv -f "$TMP" "$OUT" || { echo "error: cannot write $OUT" >&2; exit 1; }
SUCCESS=$(jq -s '[.[] | select(.type=="result" and .success==true)] | length' "$OUT")
echo "success=$SUCCESS/$COUNT"
