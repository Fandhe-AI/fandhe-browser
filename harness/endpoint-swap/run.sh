#!/usr/bin/env bash
#
# TASK-46（#252・CDP-4）の実行入口。28 タスクを Playwright / Puppeteer のスクリプトで、
# fandhe-browser と Chromium の両方へ「エンドポイントだけ差し替えて」実行し、ライブラリ別・接続先別に集計する。
# 手動専用（実サイトへ出る・npm レジストリへ出る。CI には組み込まない）。
#
# 1. playwright-core / puppeteer-core を `=x.y.z` 固定で mktemp -d へ npm install --no-save --ignore-scripts する
#    （リポに package.json・lockfile・node_modules は残さない。ブラウザ本体は取得しない）。版と integrity は
#    scripts/versions.sh の正本（PLAYWRIGHT_*・PUPPETEER_*）。最上位の版・integrity は verify_install.mjs が検証する。
# 2. 接続先（fandhe-browser は --bin、Chromium は --chromium）を 1 つずつ起動し、run_swap.mjs を実行して停止する。
# 3. summarize.mjs が、今回生成した結果ファイルだけを集計する（60% の達成判定はしない。判定は人間）。
#    出力ディレクトリに前回の *.jsonl が残っていても集計に混ざらない（結果は run_swap.mjs が上書き拒否で新規作成する）。
#
# 使い方:
#   run.sh --out-dir ABS [--libraries playwright,puppeteer] [--targets fandhe-browser,chromium]
#          [--bin P] [--page-js v8|boa [--page-modules]] [--chromium P] [--only id,id]
#          [--stage-timeout SEC] [--total-timeout SEC] [--settle-ms N]
#          [--puppeteer-connect ws|url] [--puppeteer-viewport null|default] [--validate-only]
# 終了コード: 0 = 記録・集計完了（失敗タスクがあっても 0）、1 = 実行失敗、2 = 入力・使用エラー。
#
# SEC-2: UA・ヘッダ・フィンガープリントは変更しない。Chromium は既定の UA のまま headless で起動する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=../../scripts/versions.sh
. "$REPO_ROOT/scripts/versions.sh"
# shellcheck source=../compat-practical/lib.sh
. "$REPO_ROOT/harness/compat-practical/lib.sh"

die() {
  echo "error: $*" >&2
  exit 2
}

OUT_DIR=""
LIBRARIES="playwright,puppeteer"
TARGETS="fandhe-browser,chromium"
BIN_ARG=""
CHROMIUM=""
PAGE_JS=""
PAGE_MODULES=0
ONLY=""
STAGE_TIMEOUT=30
TOTAL_TIMEOUT=1800
SETTLE_MS=2500
PP_CONNECT="ws"
PP_VIEWPORT="null"
VALIDATE_ONLY=0
FANDHE_ENDPOINT="http://127.0.0.1:9333"

while [ $# -gt 0 ]; do
  case "$1" in
    --out-dir) [ $# -ge 2 ] || die "--out-dir requires a value"; OUT_DIR="$2"; shift 2 ;;
    --libraries) [ $# -ge 2 ] || die "--libraries requires a value"; LIBRARIES="$2"; shift 2 ;;
    --targets) [ $# -ge 2 ] || die "--targets requires a value"; TARGETS="$2"; shift 2 ;;
    --bin) [ $# -ge 2 ] || die "--bin requires a value"; BIN_ARG="$2"; shift 2 ;;
    --chromium) [ $# -ge 2 ] || die "--chromium requires a value"; CHROMIUM="$2"; shift 2 ;;
    --page-js) [ $# -ge 2 ] || die "--page-js requires a value"; PAGE_JS="$2"; shift 2 ;;
    --page-modules) PAGE_MODULES=1; shift ;;
    --only) [ $# -ge 2 ] || die "--only requires a value"; ONLY="$2"; shift 2 ;;
    --stage-timeout) [ $# -ge 2 ] || die "--stage-timeout requires a value"; STAGE_TIMEOUT="$2"; shift 2 ;;
    --total-timeout) [ $# -ge 2 ] || die "--total-timeout requires a value"; TOTAL_TIMEOUT="$2"; shift 2 ;;
    --settle-ms) [ $# -ge 2 ] || die "--settle-ms requires a value"; SETTLE_MS="$2"; shift 2 ;;
    --puppeteer-connect) [ $# -ge 2 ] || die "--puppeteer-connect requires a value"; PP_CONNECT="$2"; shift 2 ;;
    --puppeteer-viewport) [ $# -ge 2 ] || die "--puppeteer-viewport requires a value"; PP_VIEWPORT="$2"; shift 2 ;;
    --validate-only) VALIDATE_ONLY=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$OUT_DIR" ] || die "--out-dir is required"
case "$OUT_DIR" in /*) ;; *) die "--out-dir must be an absolute path" ;; esac
[[ "$LIBRARIES" =~ ^(playwright|puppeteer)(,(playwright|puppeteer))?$ ]] || die "--libraries must be playwright, puppeteer or both (comma separated)"
[[ "$TARGETS" =~ ^(fandhe-browser|chromium)(,(fandhe-browser|chromium))?$ ]] || die "--targets must be fandhe-browser, chromium or both (comma separated)"
case "$PAGE_JS" in "" | v8 | boa) ;; *) die "--page-js must be v8 or boa" ;; esac
[ "$PAGE_MODULES" -eq 0 ] || [ -n "$PAGE_JS" ] || die "--page-modules requires --page-js"
[[ "$STAGE_TIMEOUT" =~ ^[1-9][0-9]{0,2}$ ]] && [ "$STAGE_TIMEOUT" -le 120 ] || die "--stage-timeout must be an integer in 1..120"
[[ "$TOTAL_TIMEOUT" =~ ^[1-9][0-9]{0,3}$ ]] && [ "$TOTAL_TIMEOUT" -le 7200 ] || die "--total-timeout must be an integer in 1..7200"
[[ "$SETTLE_MS" =~ ^(0|[1-9][0-9]{0,4})$ ]] && [ "$SETTLE_MS" -le 30000 ] || die "--settle-ms must be an integer in 0..30000"
case "$PP_CONNECT" in ws | url) ;; *) die "--puppeteer-connect must be ws or url" ;; esac
case "$PP_VIEWPORT" in null | default) ;; *) die "--puppeteer-viewport must be null or default" ;; esac
[ -z "$ONLY" ] || [[ "$ONLY" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}(,[A-Za-z0-9][A-Za-z0-9._-]{0,63})*$ ]] || die "--only has an invalid id list"

want_lib() { [[ ",$LIBRARIES," == *",$1,"* ]]; }
want_target() { [[ ",$TARGETS," == *",$1,"* ]]; }

type -P node >/dev/null 2>&1 || die "node is required but was not found on PATH"
node -e 'process.exit(typeof WebSocket === "function" ? 0 : 1)' || die "node has no built-in WebSocket (Node 22+ is required)"

if want_target fandhe-browser; then
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      # run_core.sh と同じ理由（プロファイル隔離が XDG_DATA_HOME・HOME に効かず実プロファイルに触れる恐れ）で起動しない
      die "spawning fandhe-browser is not supported on Windows yet (profile isolation does not apply)" ;;
  esac
  BIN_RESOLVED="$(resolve_bin "$BIN_ARG")" || exit 2
fi
if want_target chromium; then
  [ -n "$CHROMIUM" ] || die "--chromium <executable> is required when the chromium target is selected"
  [ -f "$CHROMIUM" ] && [ -x "$CHROMIUM" ] || die "chromium executable not found or not executable: $CHROMIUM"
fi

if [ "$VALIDATE_ONLY" -eq 1 ]; then
  node "$SCRIPT_DIR/run_swap.mjs" --validate-only --library playwright --target validate --endpoint "http://127.0.0.1:1" >/dev/null || exit 2
  echo "ok: arguments validated"
  exit 0
fi

type -P npm >/dev/null 2>&1 || die "npm is required but was not found on PATH"

WORK="$(mktemp -d)"
PID=""
PROFILE_DIR=""
CONF_DIR=""
stop_target() {
  if [ -n "$PID" ]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    PID=""
  fi
  [ -z "$PROFILE_DIR" ] || rm -rf "$PROFILE_DIR"
  [ -z "$CONF_DIR" ] || rm -rf "$CONF_DIR"
  PROFILE_DIR=""
  CONF_DIR=""
}
cleanup() {
  stop_target
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$OUT_DIR" || { echo "error: cannot create $OUT_DIR" >&2; exit 1; }

# $1 の /json/version が 2xx で応答すれば 0（run_core.sh の probe と同じ考え方。1 回の上限は 3 秒）
probe() {
  node -e '
const http = require("node:http");
const done = (c) => process.exit(c);
const req = http.get(process.argv[1] + "/json/version", (res) => { res.resume(); done(res.statusCode >= 200 && res.statusCode < 300 ? 0 : 5); });
setTimeout(() => done(4), 3000);
req.on("error", () => done(3));
' "$1" >/dev/null 2>&1
}

wait_ready() { # $1=endpoint $2=待ち秒
  local start=$SECONDS
  until probe "$1"; do
    if [ -n "$PID" ] && ! kill -0 "$PID" 2>/dev/null; then
      PID=""
      echo "error: target exited before the endpoint became ready" >&2
      return 1
    fi
    if [ $((SECONDS - start)) -ge "$2" ]; then
      echo "error: endpoint did not become ready within ${2}s" >&2
      return 1
    fi
    sleep 0.3
  done
}

free_port() {
  node -e 'const s = require("node:net").createServer().listen(0, "127.0.0.1", () => { console.log(s.address().port); s.close(); });'
}

# npm の一時導入。$1=library $2=package $3=version $4=integrity $5=追加の verify フラグ
install_lib() {
  local lib="$1" pkg="$2" ver="$3" integ="$4" extra="$5" dir="$WORK/$1"
  [[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$pkg version must be exact x.y.z: $ver"
  [[ "$integ" =~ ^sha512-[A-Za-z0-9+/]+=*$ ]] || die "$pkg integrity must be an sha512 SRI string"
  mkdir -p "$dir"
  echo "info: installing $pkg@$ver into a temporary directory (npm install --no-save --ignore-scripts)" >&2
  (cd "$dir" && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 PUPPETEER_SKIP_DOWNLOAD=1 npm install --no-save --no-audit --no-fund --ignore-scripts "$pkg@$ver" >/dev/null) \
    || { echo "error: npm install failed for $pkg@$ver" >&2; exit 1; }
  # shellcheck disable=SC2086
  node "$SCRIPT_DIR/verify_install.mjs" "$dir" "$pkg" "$ver" "$integ" $extra >"$OUT_DIR/$lib-npm-packages.json" \
    || { echo "error: install verification failed for $pkg" >&2; exit 1; }
}

start_fandhe() {
  # 固定ポート（9333）のため、応答する他プロセスがあれば誤計測を避けて中止する
  if probe "$FANDHE_ENDPOINT"; then die "endpoint is already responding: $FANDHE_ENDPOINT (stop the other process first)"; fi
  # 実プロファイルに触れないよう一時ディレクトリへ隔離する（run_core.sh と同じ）
  PROFILE_DIR="$(mktemp -d)"
  if [ -n "$PAGE_JS" ]; then
    CONF_DIR="$(mktemp -d)"
    printf '[js]\nengine = "%s"\nexecute_page_scripts = true\n' "$PAGE_JS" >"$CONF_DIR/config.toml"
    if [ "$PAGE_MODULES" -eq 1 ]; then printf 'execute_modules = true\n' >>"$CONF_DIR/config.toml"; fi
    env XDG_DATA_HOME="$PROFILE_DIR" HOME="$PROFILE_DIR" FANDHE_BROWSER_CONFIG="$CONF_DIR/config.toml" "$BIN_RESOLVED" >"$WORK/fandhe.log" 2>&1 &
  else
    env -u FANDHE_BROWSER_CONFIG XDG_DATA_HOME="$PROFILE_DIR" HOME="$PROFILE_DIR" "$BIN_RESOLVED" >"$WORK/fandhe.log" 2>&1 &
  fi
  PID=$!
  wait_ready "$FANDHE_ENDPOINT" 20 || { tail -n 5 "$WORK/fandhe.log" | tr -d '\000-\010\013-\037\177' | cut -c1-200 >&2 || true; return 1; }
}

start_chromium() { # $1=port
  PROFILE_DIR="$(mktemp -d)"
  "$CHROMIUM" --headless=new --remote-debugging-port="$1" --user-data-dir="$PROFILE_DIR" \
    --no-first-run --no-default-browser-check about:blank >"$WORK/chromium.log" 2>&1 &
  PID=$!
  wait_ready "http://127.0.0.1:$1" 30
}

if want_lib playwright; then install_lib playwright playwright-core "$PLAYWRIGHT_VERSION" "$PLAYWRIGHT_INTEGRITY" ""; fi
if want_lib puppeteer; then install_lib puppeteer puppeteer-core "$PUPPETEER_VERSION" "$PUPPETEER_INTEGRITY" "--allow-transitive"; fi

BIN_SHA=""
[ -z "${BIN_RESOLVED:-}" ] || BIN_SHA="$(sha256_of "$BIN_RESOLVED")"
MODULES=0
[ "$PAGE_MODULES" -eq 1 ] && MODULES=1
# 今回のループで生成した結果ファイルだけを集計へ渡す（出力ディレクトリに前回の測定が残っていても混ぜない）
RESULT_FILES=()

for lib in playwright puppeteer; do
  want_lib "$lib" || continue
  for target in fandhe-browser chromium; do
    want_target "$target" || continue
    endpoint=""
    ctx="{\"harness\":\"endpoint-swap\"}"
    if [ "$target" = "fandhe-browser" ]; then
      endpoint="$FANDHE_ENDPOINT"
      start_fandhe || { stop_target; echo "error: failed to start fandhe-browser" >&2; exit 2; }
      ctx="{\"harness\":\"endpoint-swap\",\"bin_sha256\":\"$BIN_SHA\",\"page_js\":\"${PAGE_JS:-off}\",\"page_modules\":$([ "$MODULES" -eq 1 ] && echo true || echo false)}"
    else
      port="$(free_port)"
      endpoint="http://127.0.0.1:$port"
      start_chromium "$port" || { stop_target; echo "error: failed to start chromium" >&2; exit 2; }
    fi
    out_file="$OUT_DIR/$lib-$target.jsonl"
    args=(--library "$lib" --target "$target" --endpoint "$endpoint" --module-dir "$WORK/$lib" --out "$out_file"
      --stage-timeout "$STAGE_TIMEOUT" --total-timeout "$TOTAL_TIMEOUT" --settle-ms "$SETTLE_MS"
      --puppeteer-connect "$PP_CONNECT" --puppeteer-viewport "$PP_VIEWPORT" --context-json "$ctx")
    [ -z "$ONLY" ] || args+=(--only "$ONLY")
    status=0
    node "$SCRIPT_DIR/run_swap.mjs" "${args[@]}" || status=$?
    stop_target
    [ "$status" -eq 0 ] || { echo "error: run_swap.mjs failed for $lib/$target (exit $status)" >&2; exit 1; }
    RESULT_FILES+=("$out_file")
  done
done

if [ -n "$ONLY" ]; then
  echo "info: --only was given, so the summary (which needs the full task set) is skipped" >&2
  exit 0
fi
node "$SCRIPT_DIR/summarize.mjs" --json-out "$OUT_DIR/summary.json" "${RESULT_FILES[@]}" | tee "$OUT_DIR/summary.txt"
