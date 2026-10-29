#!/usr/bin/env bash
#
# CLI 基本操作コマンドのコンフォーマンステストの実行スクリプト（TASK-49・`CLI-1`。関連 `CLI-3`・`MEAS-5`・
# `XOS-7`・`XOS-10`・`PROF-1`）。仕様は docs/design/cli-conformance-tests.md（以下「仕様書」）。
# 1 回の実行は 1 つの OS（実行したランナー / 端末）だけを測る。必須 5 ケース（`CT-<コマンド>-<OS>`）と補助ケース
# （`CT-<コマンド>-E<n>`）を実行し、仕様書 5 章の記録表の列に合わせて JSONL と Markdown 表を出す。
# 3 OS の一致判定（同じケースの正規化後の結果比較）は、OS ごとに出した JSONL を突き合わせる側で行う。
#
# 判定は pass（合格）/ fail（不合格）/ unmet（前提未充足）の 3 値。前提未充足は合格に数えない:
#   - CT-SHOT: 描画ホストが解決できない固定シグネチャ（仕様書 3.4）のときだけ unmet。それ以外の失敗は fail
#   - CT-LAUNCH: 127.0.0.1:9333 が使用中のとき unmet
#   - CT-NAV・CT-QUERY: FIXTURE_URL の事前確認（仕様書 2.3）が期待と食い違うとき unmet
#
# 使い方: run.sh [--bin P] [--render-host P] [--out-jsonl P] [--out-md P] [--commit SHA]
#                [--outside-dir DIR] [--timeout SEC] [--only ID[,ID...]] [--strict]
#   --bin           対象バイナリ（既定: <repo>/target/release/fandhe-browser。Windows は .exe）
#   --render-host   描画ホスト fandhe-browser-servo-host（FANDHE_BROWSER_RENDER_HOST で渡す。未指定なら環境変数を外す）
#   --out-jsonl     結果 JSONL（既定: <repo>/target/cli-conformance/results.jsonl）
#   --out-md        結果の Markdown 表（既定: 同ディレクトリの results.md）
#   --commit        記録する commit SHA（既定: GITHUB_SHA、無ければ git rev-parse HEAD）
#   --outside-dir   Windows の CT-PROFILE-E4 用に %LOCALAPPDATA% の外にあるディレクトリ（既定: RUNNER_TEMP）
#   --timeout       コマンド 1 回の上限秒（既定 60。screenshot は 2 倍）
#   --only          指定したケース ID だけ実行する（再実行・デバッグ用）
#   --strict        必須ケースに fail または unmet があれば終了コード 1（既定は記録完了で 0）
# 環境変数（自己テスト用。実測では設定しない）: CT_LAUNCH_PORT（既定 9333）・CT_FIXTURE_CURL（既定 curl）
# 終了コード: 0=記録完了（fail があっても 0。成否は計測結果）、1=--strict で必須ケースが非合格、2=入力・環境エラー
#
# 隔離（仕様書 2.5）: Linux・macOS は HOME・XDG_DATA_HOME をケースごとの一時ディレクトリへ向ける。Windows は
# LOCALAPPDATA を差し替えられない（`...\AppData\Local` で終わる必要がある）ため実領域を使い、実行前に
# `%LOCALAPPDATA%\fandhe-browser` が無いことを確認して（あれば exit 2）、本スクリプトが作った分だけを消す。
# 外部アクセス: example.com（FIXTURE_URL）と 127.0.0.1 のみ。SSRF の既定拒否を緩める手順は含まない（仕様書 6 章）。
set -euo pipefail

# MSYS（Git Bash）の引数パス変換は、ネイティブの curl・jq に /tmp/... の絶対パスを渡すために必要なので全体では止めない。
# 変換が害になる箇所（`/c`・`/F` を取る cmd・taskkill、値だけを渡す jq -cn、バイナリへ渡す env）にだけ
# MSYS2_ARG_CONV_EXCL='*' をインラインで付ける

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ---------------------------------------------------------------------------
# 仕様書の固定値（仕様書 2.3・3・4 章から引き写し。変更時は仕様書と同時に直す）
# ---------------------------------------------------------------------------
FIXTURE_URL="https://example.com/"
USAGE_SUFFIX="; run 'fandhe-browser --help' for usage"
QUERY_P_TEXT='"This domain is for use in documentation examples without needing permission. This is not a service; avoid relying on it for testing and monitoring purposes."'
SHOT_UNAVAILABLE_ERR="error: rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)"
WIN_OUTSIDE_ERR="error: on Windows, the profile root must be located under %LOCALAPPDATA% (XOS-7, XOS-10)"
PORT="${CT_LAUNCH_PORT:-9333}"
FIXTURE_CURL="${CT_FIXTURE_CURL:-curl}"

BIN_ARG=""
RENDER_HOST_ARG=""
OUT_JSONL=""
OUT_MD=""
COMMIT_ARG=""
OUTSIDE_DIR_ARG=""
CMD_TIMEOUT=60
ONLY=""
STRICT=0

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --bin) [ $# -ge 2 ] || die "--bin requires a value"; BIN_ARG="$2"; shift 2 ;;
    --render-host) [ $# -ge 2 ] || die "--render-host requires a value"; RENDER_HOST_ARG="$2"; shift 2 ;;
    --out-jsonl) [ $# -ge 2 ] || die "--out-jsonl requires a value"; OUT_JSONL="$2"; shift 2 ;;
    --out-md) [ $# -ge 2 ] || die "--out-md requires a value"; OUT_MD="$2"; shift 2 ;;
    --commit) [ $# -ge 2 ] || die "--commit requires a value"; COMMIT_ARG="$2"; shift 2 ;;
    --outside-dir) [ $# -ge 2 ] || die "--outside-dir requires a value"; OUTSIDE_DIR_ARG="$2"; shift 2 ;;
    --timeout) [ $# -ge 2 ] || die "--timeout requires a value"; CMD_TIMEOUT="$2"; shift 2 ;;
    --only) [ $# -ge 2 ] || die "--only requires a value"; ONLY="$2"; shift 2 ;;
    --strict) STRICT=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

command -v jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"
command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"
[[ "$CMD_TIMEOUT" =~ ^[1-9][0-9]{0,2}$ ]] && [ "$CMD_TIMEOUT" -le 600 ] || die "--timeout must be an integer in 1..600"
[[ "$PORT" =~ ^[1-9][0-9]{0,4}$ ]] && [ "$PORT" -le 65535 ] || die "CT_LAUNCH_PORT must be a port number"
if [ -n "$ONLY" ] && ! [[ "$ONLY" =~ ^[A-Za-z0-9_,-]+$ ]]; then die "--only must be a comma-separated list of case IDs"; fi

# jq の出力から CR を除く（Windows のネイティブ jq は CRLF で出力し、等値判定が壊れるため）
jq() { command jq "$@" | tr -d '\r'; }

# ---------------------------------------------------------------------------
# OS・環境の判定
# ---------------------------------------------------------------------------
case "$(uname -s)" in
  Linux) OS_LABEL=LINUX; OS_NAME=Linux ;;
  Darwin) OS_LABEL=MACOS; OS_NAME=macOS ;;
  MINGW* | MSYS* | CYGWIN*) OS_LABEL=WINDOWS; OS_NAME=Windows ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac
IS_WINDOWS=0
[ "$OS_LABEL" = WINDOWS ] && IS_WINDOWS=1

# bash 用のパス（unix 形式）→ ネイティブバイナリへ渡すパス（Windows は C:\... 形式）
native_path() {
  if [ "$IS_WINDOWS" -eq 1 ]; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

os_version() {
  case "$OS_LABEL" in
    LINUX)
      # shellcheck source=/dev/null
      if [ -r /etc/os-release ]; then (. /etc/os-release && printf '%s (kernel %s)' "${PRETTY_NAME:-Linux}" "$(uname -r)"); else uname -sr; fi ;;
    MACOS) printf 'macOS %s (Darwin %s)' "$(sw_vers -productVersion 2>/dev/null || echo unknown)" "$(uname -r)" ;;
    WINDOWS) (MSYS2_ARG_CONV_EXCL='*' cmd /c ver 2>/dev/null | tr -d '\r' | sed '/^$/d' | head -n 1) || uname -sr ;;
  esac
}
OS_VERSION="$(os_version)"
ARCH="$(uname -m)"
RUN_DATE="$(date -u +%Y-%m-%d)"

if [ -n "$COMMIT_ARG" ]; then COMMIT="$COMMIT_ARG"
elif [ -n "${GITHUB_SHA:-}" ]; then COMMIT="$GITHUB_SHA"
else COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
fi
[[ "$COMMIT" =~ ^[0-9A-Za-z._-]{1,64}$ ]] || die "invalid commit value"

# ---------------------------------------------------------------------------
# 対象バイナリ・描画ホスト・出力先
# ---------------------------------------------------------------------------
if [ -z "$BIN_ARG" ]; then
  BIN_ARG="$REPO_ROOT/target/release/fandhe-browser"
  [ "$IS_WINDOWS" -eq 0 ] || BIN_ARG="$BIN_ARG.exe"
fi
[ -f "$BIN_ARG" ] && [ -x "$BIN_ARG" ] || die "binary not found or not executable: $BIN_ARG"
BIN="$(cd "$(dirname "$BIN_ARG")" && pwd)/$(basename "$BIN_ARG")"
BIN_NATIVE="$(native_path "$BIN")"
case "$BIN" in "$REPO_ROOT"/*) BIN_REC="${BIN#"$REPO_ROOT"/}" ;; *) BIN_REC="$BIN_NATIVE" ;; esac

RENDER_HOST=""
if [ -n "$RENDER_HOST_ARG" ]; then
  [ -f "$RENDER_HOST_ARG" ] && [ -x "$RENDER_HOST_ARG" ] || die "render host not found or not executable: $RENDER_HOST_ARG"
  RENDER_HOST="$(cd "$(dirname "$RENDER_HOST_ARG")" && pwd)/$(basename "$RENDER_HOST_ARG")"
fi

[ -n "$OUT_JSONL" ] || OUT_JSONL="$REPO_ROOT/target/cli-conformance/results.jsonl"
[ -n "$OUT_MD" ] || OUT_MD="$(dirname "$OUT_JSONL")/results.md"
mkdir -p "$(dirname "$OUT_JSONL")" "$(dirname "$OUT_MD")" || die "cannot create output directory"

# 作業領域。macOS の mktemp は /var/folders/...（/var は /private/var への symlink）を返し、profile は祖先の
# symlink を拒否する（仕様書 2.5 の注）ため、実体パスへ解決してから使う
BASE="$(cd "$(mktemp -d)" && pwd -P)" || die "cannot create work directory"
SEQ=0
LA_CLEAN=()
SERVER_PIDS=()
NODE_HOLDER_PID=""

# Windows の既定プロファイルルート（本スクリプトが作らない限り存在してはならない）
DEFAULT_ROOT_WIN=""
if [ "$IS_WINDOWS" -eq 1 ]; then
  [ -n "${LOCALAPPDATA:-}" ] || die "LOCALAPPDATA is not set"
  LA_UNIX="$(cygpath -u "$LOCALAPPDATA")"
  DEFAULT_ROOT_WIN="$LA_UNIX/fandhe-browser"
  if [ -e "$DEFAULT_ROOT_WIN" ]; then
    rm -rf "$BASE"
    die "refusing to run: the default profile root already exists. Run on a dedicated Windows user or a fresh environment (spec 2.5)"
  fi
fi

# shellcheck disable=SC2329 # trap EXIT から間接的に呼ばれる
cleanup() {
  local p
  for p in ${SERVER_PIDS[@]+"${SERVER_PIDS[@]}"} ${NODE_HOLDER_PID:+"$NODE_HOLDER_PID"}; do
    kill -KILL "$p" 2>/dev/null || true
  done
  for p in ${LA_CLEAN[@]+"${LA_CLEAN[@]}"}; do rm -rf "$p" 2>/dev/null || true; done
  [ -z "$DEFAULT_ROOT_WIN" ] || rm -rf "$DEFAULT_ROOT_WIN" 2>/dev/null || true
  rm -rf "$BASE" 2>/dev/null || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 結果の蓄積
# ---------------------------------------------------------------------------
RESULT_COUNT=0
REQ_PASS=0; REQ_FAIL=0; REQ_UNMET=0
AUX_PASS=0; AUX_FAIL=0; AUX_UNMET=0

# 先頭行（メタ）は FIXTURE の事前確認後に書く。ここでは Markdown の見出しだけ先に作る
{
  echo "# CLI コンフォーマンステスト結果（${OS_NAME}）"
  echo
  echo "- commit: \`$COMMIT\`"
  echo "- OS・版・アーキテクチャ: $OS_NAME / $OS_VERSION / $ARCH"
  echo "- バイナリ: \`$BIN_REC\`"
  echo "- 実行日: $RUN_DATE"
  echo
  echo "| ケース ID | OS・版・アーキテクチャ | commit SHA | 実行日 | 終了コード | stdout（正規化後）・stderr | 副作用の確認 | 判定 | 差分メモ |"
  echo "| --------- | ---------------------- | ---------- | ------ | ---------- | -------------------------- | ------------ | ---- | -------- |"
} >"$OUT_MD"

selected() {
  [ -z "$ONLY" ] && return 0
  case ",$ONLY," in *",$1,"*) return 0 ;; esac
  return 1
}

# 1 ケースの状態（begin_case で初期化し、end_case で書き出す）
CUR_ID=""; CUR_REQ=0; CUR_FAILED=0; CUR_UNMET=0
CUR_NOTES=""; CUR_SIDE=""; CUR_ARGV=""; CUR_RC=""; CUR_STDOUT=""; CUR_STDERR=""
CASE_DIR=""; CASE_HOME=""

begin_case() {
  CUR_ID="$1"; CUR_REQ="$2"
  CUR_FAILED=0; CUR_UNMET=0
  CUR_NOTES=""; CUR_SIDE=""; CUR_ARGV=""; CUR_RC=""; CUR_STDOUT=""; CUR_STDERR=""
  CASE_DIR="$(mktemp -d "$BASE/case.XXXXXX")"
  CASE_HOME="$CASE_DIR/home"
  mkdir -p "$CASE_HOME"
}

# note <text>: 差分メモに追記（"; " 区切り）
note() { if [ -z "$CUR_NOTES" ]; then CUR_NOTES="$1"; else CUR_NOTES="$CUR_NOTES; $1"; fi; }
# side <text>: 副作用の確認に追記
side() { if [ -z "$CUR_SIDE" ]; then CUR_SIDE="$1"; else CUR_SIDE="$CUR_SIDE; $1"; fi; }
# fail_check <理由>: 不合格にして理由を差分メモへ残す
fail_check() { CUR_FAILED=1; note "FAIL: $1"; }
# unmet_check <理由>: 前提未充足にして理由を差分メモへ残す
unmet_check() { CUR_UNMET=1; note "UNMET: $1"; }

# 表示・記録用に制御文字（改行・CR を除く）を落として長さを切り詰める
clip() { printf '%s' "$1" | tr -d '\000-\010\013\014\016-\037\177' | cut -c1-"$2"; }
md_cell() { printf '%s' "$1" | sed -e 's/|/\\|/g' | tr '\n' ' ' | sed -e 's/  */ /g'; }

end_case() {
  local verdict verdict_ja
  if [ "$CUR_FAILED" -eq 1 ]; then verdict=fail; verdict_ja="不合格"
  elif [ "$CUR_UNMET" -eq 1 ]; then verdict=unmet; verdict_ja="前提未充足"
  else verdict=pass; verdict_ja="合格"
  fi
  MSYS2_ARG_CONV_EXCL='*' jq -cn --arg id "$CUR_ID" --argjson req "$CUR_REQ" --arg os "$OS_NAME" --arg osv "$OS_VERSION" --arg arch "$ARCH" \
    --arg commit "$COMMIT" --arg date "$RUN_DATE" --arg rc "$CUR_RC" --arg argv "$CUR_ARGV" \
    --arg out "$(clip "$CUR_STDOUT" 2000)" --arg err "$(clip "$CUR_STDERR" 2000)" \
    --arg side "$CUR_SIDE" --arg verdict "$verdict" --arg note "$CUR_NOTES" \
    '{type:"result",id:$id,required:($req==1),os:$os,os_version:$osv,arch:$arch,commit:$commit,date:$date,
      exit_code:(if $rc=="" then null else ($rc|tonumber) end),argv:$argv,stdout:$out,stderr:$err,
      side_effects:$side,verdict:$verdict,note:$note}' >>"$OUT_JSONL"
  {
    # shellcheck disable=SC2016 # 表のコード書式のバッククォートを出力する
    printf '| `%s` | %s | `%s` | %s | %s | %s | %s | %s | %s |\n' \
      "$CUR_ID" "$(md_cell "$OS_NAME $OS_VERSION $ARCH")" "$(md_cell "$COMMIT")" "$RUN_DATE" "${CUR_RC:--}" \
      "$(md_cell "stdout: $(clip "$CUR_STDOUT" 300) / stderr: $(clip "$CUR_STDERR" 300)")" \
      "$(md_cell "${CUR_SIDE:--}")" "$verdict_ja" "$(md_cell "${CUR_NOTES:--}")"
  } >>"$OUT_MD"
  RESULT_COUNT=$((RESULT_COUNT + 1))
  if [ "$CUR_REQ" -eq 1 ]; then
    case "$verdict" in pass) REQ_PASS=$((REQ_PASS + 1)) ;; fail) REQ_FAIL=$((REQ_FAIL + 1)) ;; unmet) REQ_UNMET=$((REQ_UNMET + 1)) ;; esac
  else
    case "$verdict" in pass) AUX_PASS=$((AUX_PASS + 1)) ;; fail) AUX_FAIL=$((AUX_FAIL + 1)) ;; unmet) AUX_UNMET=$((AUX_UNMET + 1)) ;; esac
  fi
  echo "[$verdict] $CUR_ID${CUR_NOTES:+ ($(clip "$CUR_NOTES" 200))}"
}

# ---------------------------------------------------------------------------
# コマンド実行
# ---------------------------------------------------------------------------
# cli_env_args: 隔離した環境でバイナリを起動するための `env` 引数を ENV_ARGS に組む（仕様書 2.5）
ENV_ARGS=()
cli_env_args() {
  # `env` は NAME=VALUE の後ろの -u をコマンドと解釈するため、-u を先に全部並べる
  ENV_ARGS=(-u FANDHE_BROWSER_CONFIG)
  [ -n "$RENDER_HOST" ] || ENV_ARGS+=(-u FANDHE_BROWSER_RENDER_HOST)
  # バイナリへ渡す argv（URL・file:///・--オプション）を MSYS に書き換えさせない
  [ "$IS_WINDOWS" -eq 0 ] || ENV_ARGS+=("MSYS2_ARG_CONV_EXCL=*")
  if [ "$IS_WINDOWS" -eq 0 ]; then
    ENV_ARGS+=("HOME=$CASE_HOME" "XDG_DATA_HOME=$CASE_HOME/xdg")
  fi
  if [ -n "$RENDER_HOST" ]; then
    ENV_ARGS+=("FANDHE_BROWSER_RENDER_HOST=$(native_path "$RENDER_HOST")")
  fi
}

# 既定プロファイルルート（bash 用の unix 形式。仕様書 2.5）
default_root() {
  case "$OS_LABEL" in
    LINUX) printf '%s' "$CASE_HOME/xdg/fandhe-browser" ;;
    MACOS) printf '%s' "$CASE_HOME/Library/Application Support/fandhe-browser" ;;
    WINDOWS) printf '%s' "$DEFAULT_ROOT_WIN" ;;
  esac
}

# プロセスの停止。SIGINT（仕様書 2.5。非対話 bash のバックグラウンドでは無視されうる）→ SIGTERM → SIGKILL の
# 順に試し、使えた手段を STOP_METHOD に残す。Windows は最後に taskkill へフォールバックする
STOP_METHOD=""
stop_proc() {
  local pid="$1" i
  STOP_METHOD="already-exited"
  kill -0 "$pid" 2>/dev/null || return 0
  STOP_METHOD="SIGINT"
  kill -INT "$pid" 2>/dev/null || true
  for i in 1 2 3 4 5; do kill -0 "$pid" 2>/dev/null || return 0; sleep 0.2; done
  STOP_METHOD="SIGTERM"
  kill -TERM "$pid" 2>/dev/null || true
  for i in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$pid" 2>/dev/null || return 0; sleep 0.2; done
  STOP_METHOD="SIGKILL"
  kill -KILL "$pid" 2>/dev/null || true
  if [ "$IS_WINDOWS" -eq 1 ] && kill -0 "$pid" 2>/dev/null; then
    STOP_METHOD="taskkill"
    local winpid=""
    [ -r "/proc/$pid/winpid" ] && winpid="$(tr -d '\r\n' <"/proc/$pid/winpid")"
    [ -z "$winpid" ] || MSYS2_ARG_CONV_EXCL='*' taskkill /F /PID "$winpid" >/dev/null 2>&1 || true
  fi
  for i in 1 2 3 4 5; do kill -0 "$pid" 2>/dev/null || return 0; sleep 0.2; done
  return 1
}

# reap <pid>: 終了済みのバックグラウンドプロセスを回収する。まだ生きている場合は wait せず（ハング防止）戻る
reap() {
  if kill -0 "$1" 2>/dev/null; then return 0; fi
  wait "$1" 2>/dev/null || true
}

# run_cli <timeout> <args...>: バイナリを 1 回実行する。RC・OUT_FILE・ERR_FILE を設定し、CUR_ARGV に argv を追記する
RC=0; OUT_FILE=""; ERR_FILE=""
run_cli() {
  local to="$1" pid i ticks
  shift
  SEQ=$((SEQ + 1))
  OUT_FILE="$CASE_DIR/out.$SEQ"; ERR_FILE="$CASE_DIR/err.$SEQ"
  cli_env_args
  local desc
  desc="[$(printf '"%s", ' "$@" | sed 's/, $//')]"
  if [ -z "$CUR_ARGV" ]; then CUR_ARGV="$desc"; else CUR_ARGV="$CUR_ARGV; $desc"; fi
  env "${ENV_ARGS[@]}" "$BIN_NATIVE" "$@" >"$OUT_FILE" 2>"$ERR_FILE" &
  pid=$!
  ticks=$((to * 5))
  for ((i = 0; i < ticks; i++)); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.2
  done
  if kill -0 "$pid" 2>/dev/null; then
    stop_proc "$pid" || true
    reap "$pid"
    RC=124
    note "command timed out after ${to}s (stopped with $STOP_METHOD)"
  else
    set +e
    wait "$pid" 2>/dev/null
    RC=$?
    set -e
  fi
}

# 取り込み: stdout・stderr を正規化して CUR_STDOUT / CUR_STDERR へ入れる。CR が混じったら差分として記録する
absorb_output() {
  if grep -q $'\r' "$OUT_FILE" "$ERR_FILE" 2>/dev/null; then note "CRLF found in output (normalized to LF)"; fi
  CUR_RC="$RC"
  CUR_STDOUT="$(tr -d '\r' <"$OUT_FILE")"
  CUR_STDERR="$(tr -d '\r' <"$ERR_FILE")"
}

# 判定ヘルパー。期待の書式: =完全一致 / ~正規表現（bash ERE） / -空 / +部分文字列 / *検査しない
check_text() {
  local what="$1" actual="$2" expect="$3"
  case "$expect" in
    "="*) [ "$actual" = "${expect#=}" ] || fail_check "$what mismatch (expected: $(clip "${expect#=}" 160))" ;;
    "~"*) [[ "$actual" =~ ${expect#\~} ]] || fail_check "$what does not match /${expect#\~}/" ;;
    "-") [ -z "$actual" ] || fail_check "$what is not empty" ;;
    "+"*) case "$actual" in *"${expect#+}"*) ;; *) fail_check "$what lacks '$(clip "${expect#+}" 120)'" ;; esac ;;
    "*") ;;
    *) die "internal: bad expectation: $expect" ;;
  esac
}

# check_run <終了コード> <stdout 期待> <stderr 期待>: 直近の run_cli の結果を取り込んで判定する
check_run() {
  absorb_output
  if [ "$RC" != "$1" ]; then fail_check "exit code $RC (expected $1)"; fi
  check_text stdout "$CUR_STDOUT" "$2"
  check_text stderr "$CUR_STDERR" "$3"
}

# ---------------------------------------------------------------------------
# サーバー（launch）の起動・停止・ポート操作
# ---------------------------------------------------------------------------
port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; }

wait_port_free() {
  local i
  for i in $(seq 1 25); do port_in_use || return 0; sleep 0.2; done
  return 1
}

# start_server <args...>: launch をバックグラウンドで起動する（SRV_PID・SRV_OUT・SRV_ERR）。DevTools 行を最大 20 秒待つ
SRV_PID=""; SRV_OUT=""; SRV_ERR=""
start_server() {
  local i
  SEQ=$((SEQ + 1))
  SRV_OUT="$CASE_DIR/srv-out.$SEQ"; SRV_ERR="$CASE_DIR/srv-err.$SEQ"
  cli_env_args
  local desc
  desc="[$(printf '"%s", ' "$@" | sed 's/, $//')]"
  if [ -z "$CUR_ARGV" ]; then CUR_ARGV="$desc"; else CUR_ARGV="$CUR_ARGV; $desc"; fi
  env "${ENV_ARGS[@]}" "$BIN_NATIVE" "$@" >"$SRV_OUT" 2>"$SRV_ERR" &
  SRV_PID=$!
  SERVER_PIDS+=("$SRV_PID")
  for ((i = 0; i < 100; i++)); do
    if tr -d '\r' <"$SRV_ERR" | grep -q '^DevTools listening on '; then return 0; fi
    kill -0 "$SRV_PID" 2>/dev/null || return 1
    sleep 0.2
  done
  return 1
}

DEVTOOLS_RE="^DevTools listening on ws://127\\.0\\.0\\.1:${PORT}/devtools/browser/([0-9A-F]{32})$"

# ---------------------------------------------------------------------------
# FIXTURE_URL の事前確認（仕様書 2.3）。食い違ったら example.com 依存のケースを前提未充足にする
# ---------------------------------------------------------------------------
FIXTURE_OK=1; FIXTURE_SCRIPT_OK=1; FIXTURE_DETAIL="ok"
check_fixture() {
  local f="$BASE/fixture.html" code
  set +e
  code="$("$FIXTURE_CURL" -sS --noproxy '*' --max-time 30 -o "$f" -w '%{http_code}' "$FIXTURE_URL" 2>"$BASE/fixture.err")"
  local rc=$?
  set -e
  code="$(printf '%s' "$code" | tr -d '\r')"
  if [ "$rc" -ne 0 ] || [ "$code" != 200 ]; then
    FIXTURE_OK=0; FIXTURE_SCRIPT_OK=0
    FIXTURE_DETAIL="fetch failed (curl exit $rc, http status ${code:-none})"
    return
  fi
  local missing=""
  # example.com は HTML を最小化しており属性値の引用符を省く（`<html lang=en>`・`<script src=/s.js>`）。引用符の有無は問わない
  grep -qiE -- '<title>Example Domain</title>' "$f" || missing="$missing <title>"
  grep -qiE -- "<html[^>]*[[:space:]]lang=[\"']?en[\"']?[[:space:]>]" "$f" || missing="$missing <html lang>"
  grep -qiF -- '<p>This domain is for use in documentation examples' "$f" || missing="$missing <p>"
  if [ -n "$missing" ]; then
    FIXTURE_OK=0; FIXTURE_SCRIPT_OK=0
    FIXTURE_DETAIL="content differs from the spec:$missing"
    return
  fi
  if ! grep -qiE -- "<script[^>]*[[:space:]]src=[\"']?/s\\.js[\"']?[[:space:]>]" "$f"; then
    FIXTURE_SCRIPT_OK=0
    FIXTURE_DETAIL="script src=\"/s.js\" not found (affects the --attr src check only)"
  fi
}

fixture_unmet() { unmet_check "FIXTURE_URL precondition not met: $FIXTURE_DETAIL"; }

# ---------------------------------------------------------------------------
# プロファイル <P> の組み立て（仕様書 2.5）。P_UNIX は bash 用、P_NATIVE は argv 用
# ---------------------------------------------------------------------------
P_UNIX=""; P_NATIVE=""
make_profile_path() {
  if [ "$IS_WINDOWS" -eq 1 ]; then
    local parent="$LA_UNIX/fandhe-ct-$$-$RANDOM"
    mkdir "$parent"
    LA_CLEAN+=("$parent")
    P_UNIX="$parent/p"
  else
    P_UNIX="$CASE_DIR/p"
  fi
  P_NATIVE="$(native_path "$P_UNIX")"
}

# Windows だけ、本ケースで作った既定プロファイルルートを消す（実施前から存在しないことは起動時に確認済み）
cleanup_default_root() {
  [ "$IS_WINDOWS" -eq 0 ] || rm -rf "$DEFAULT_ROOT_WIN" 2>/dev/null || true
}

# プロファイル構造（profile.lock と cookies・storage・cache・history）の確認。$1=ルート（unix 形式）
check_profile_layout() {
  local root="$1" missing="" n
  for n in profile.lock cookies storage cache history; do
    [ -e "$root/$n" ] || missing="$missing $n"
  done
  if [ -n "$missing" ]; then fail_check "profile layout lacks:$missing"; else side "profile layout ok (profile.lock, cookies, storage, cache, history)"; fi
}

# ファイルモード（8 進）。GNU stat と BSD stat の両方に対応
file_mode() { stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1" 2>/dev/null; }

# ---------------------------------------------------------------------------
# 必須ケース
# ---------------------------------------------------------------------------
case_launch() {
  selected "CT-LAUNCH-$OS_LABEL" || return 0
  begin_case "CT-LAUNCH-$OS_LABEL" 1
  if port_in_use; then
    unmet_check "port $PORT is already in use"
    end_case; return 0
  fi
  local variant url wsurl code root idx=0
  root="$(default_root)"
  # 仕様書 3.1: argv ["launch"] と引数なし []（run_default が同じ起動処理を通る）の両方で同じ出力になること
  for variant in launch default; do
    idx=$((idx + 1))
    if [ "$variant" = launch ]; then start_server launch; else start_server; fi || true
    CUR_RC=""
    CUR_STDERR="$(tr -d '\r' <"$SRV_ERR")"
    CUR_STDOUT="$(tr -d '\r' <"$SRV_OUT")"
    if [[ "$CUR_STDERR" =~ $DEVTOOLS_RE ]]; then
      url="ws://127.0.0.1:${PORT}/devtools/browser/${BASH_REMATCH[1]}"
      side "[$variant] stderr has DevTools line"
      if [ "$idx" -eq 1 ]; then
        # 補助確認: /json/version の webSocketDebuggerUrl が stderr の URL と一致する
        set +e
        code="$(curl -sS --noproxy '*' --max-time 10 -o "$CASE_DIR/version.json" -w '%{http_code}' "http://127.0.0.1:$PORT/json/version" 2>/dev/null)"
        set -e
        if [ "$(printf '%s' "$code" | tr -d '\r')" != 200 ]; then
          fail_check "/json/version returned status ${code:-none}"
        else
          wsurl="$(jq -r '.webSocketDebuggerUrl // ""' "$CASE_DIR/version.json" 2>/dev/null || true)"
          if [ "$wsurl" = "$url" ]; then side "/json/version webSocketDebuggerUrl matches stderr"; else fail_check "/json/version webSocketDebuggerUrl differs from stderr"; fi
        fi
        check_profile_layout "$root"
      fi
      [ -z "$CUR_STDOUT" ] || fail_check "[$variant] stdout is not empty while running"
    else
      fail_check "[$variant] no valid 'DevTools listening' line within 20s (stderr: $(clip "$CUR_STDERR" 160))"
    fi
    stop_proc "$SRV_PID" || fail_check "[$variant] process could not be stopped"
    reap "$SRV_PID"
    side "[$variant] stopped with $STOP_METHOD"
    if wait_port_free; then side "[$variant] port $PORT released"; else fail_check "[$variant] port $PORT not released after stop"; fi
  done
  # 常駐プロセスのため終了コードは判定対象にしない（仕様書 3.1）。記録欄は空にする。stdout・stderr は 2 回目の起動の内容
  CUR_RC=""
  cleanup_default_root
  end_case
}

case_nav() {
  selected "CT-NAV-$OS_LABEL" || return 0
  begin_case "CT-NAV-$OS_LABEL" 1
  if [ "$FIXTURE_OK" -eq 0 ]; then fixture_unmet; end_case; return 0; fi
  run_cli "$CMD_TIMEOUT" navigate "$FIXTURE_URL"
  check_run 0 "=status=200 url=$FIXTURE_URL" "-"
  # --profile-dir なしでは既定プロファイルに触れない（ファイルを作らない）
  local root
  root="$(default_root)"
  if [ "$IS_WINDOWS" -eq 1 ]; then
    if [ -e "$root" ]; then fail_check "default profile root was created"; else side "default profile root not created"; fi
  else
    if [ -z "$(find "$CASE_HOME" -mindepth 1 2>/dev/null | head -n 1)" ]; then side "no files created under isolated HOME"; else fail_check "files were created under isolated HOME"; fi
  fi
  end_case
}

case_query() {
  selected "CT-QUERY-$OS_LABEL" || return 0
  begin_case "CT-QUERY-$OS_LABEL" 1
  if [ "$FIXTURE_OK" -eq 0 ]; then fixture_unmet; end_case; return 0; fi
  run_cli "$CMD_TIMEOUT" query "$FIXTURE_URL" title
  check_run 0 '="Example Domain"' "-"
  # 同じケースの中で次も確認する（完全一致）。代表の記録は 1 件目、他は副作用欄に残す
  local first_rc="$CUR_RC" first_out="$CUR_STDOUT" first_err="$CUR_STDERR" first_argv="$CUR_ARGV"
  local sub
  for sub in lang src p; do
    case "$sub" in
      lang) run_cli "$CMD_TIMEOUT" query --attr lang "$FIXTURE_URL" html ;;
      src) run_cli "$CMD_TIMEOUT" query --attr src "$FIXTURE_URL" script ;;
      p) run_cli "$CMD_TIMEOUT" query --all "$FIXTURE_URL" p ;;
    esac
    absorb_output
    local want
    case "$sub" in lang) want='"en"' ;; src) want='"/s.js"' ;; p) want="$QUERY_P_TEXT" ;; esac
    if [ "$sub" = src ] && [ "$FIXTURE_SCRIPT_OK" -eq 0 ]; then
      unmet_check "$FIXTURE_DETAIL"
      continue
    fi
    if [ "$RC" -ne 0 ]; then fail_check "[$sub] exit code $RC (expected 0)"; fi
    if [ "$CUR_STDOUT" != "$want" ]; then fail_check "[$sub] stdout mismatch (got: $(clip "$CUR_STDOUT" 120))"; else side "[$sub] stdout matches"; fi
    [ -z "$CUR_STDERR" ] || fail_check "[$sub] stderr is not empty"
  done
  CUR_RC="$first_rc"; CUR_STDOUT="$first_out"; CUR_STDERR="$first_err"; CUR_ARGV="$first_argv; (+ lang, src, p sub-checks)"
  end_case
}

case_shot() {
  selected "CT-SHOT-$OS_LABEL" || return 0
  begin_case "CT-SHOT-$OS_LABEL" 1
  if [ "$FIXTURE_OK" -eq 0 ]; then fixture_unmet; end_case; return 0; fi
  local shot="$CASE_DIR/shot.png"
  run_cli $((CMD_TIMEOUT * 2)) screenshot "$FIXTURE_URL" "$(native_path "$shot")"
  absorb_output
  if [ "$RC" -eq 1 ] && [ "$CUR_STDERR" = "$SHOT_UNAVAILABLE_ERR" ]; then
    # 描画ホストが無い固定シグネチャ。前提未充足として記録し、合格にはしない（仕様書 2.1・3.4）
    unmet_check "render host not resolved (signature matched)"
    end_case; return 0
  fi
  [ "$RC" -eq 0 ] || fail_check "exit code $RC (expected 0)"
  [ -z "$CUR_STDERR" ] || fail_check "stderr is not empty"
  local size=""
  if [ -f "$shot" ]; then size="$(wc -c <"$shot" | tr -d ' ')"; fi
  local re="^width=1280 height=800 bytes=([0-9]+) url=https://example\\.com/\$"
  if [[ "$CUR_STDOUT" =~ $re ]]; then
    side "stdout format ok"
    if [ -n "$size" ] && [ "${BASH_REMATCH[1]}" = "$size" ]; then side "bytes=$size matches file size"; else fail_check "bytes in stdout (${BASH_REMATCH[1]}) differs from file size (${size:-none})"; fi
  else
    fail_check "stdout mismatch (got: $(clip "$CUR_STDOUT" 160))"
  fi
  if [ -f "$shot" ]; then
    local head8 dims
    head8="$(od -An -tx1 -N8 "$shot" | tr -d ' \n')"
    dims="$(od -An -tu1 -j16 -N8 "$shot" | tr -s ' ' | sed 's/^ //; s/ $//')"
    if [ "$head8" = 89504e470d0a1a0a ]; then side "PNG signature ok"; else fail_check "PNG signature mismatch ($head8)"; fi
    # IHDR の幅・高さ（ビッグエンディアン 4 バイト × 2）を 10 進で比較する。1280 = 0 0 5 0、800 = 0x0320 = 0 0 3 32
    if [ "$dims" = "0 0 5 0 0 0 3 32" ]; then side "IHDR 1280x800 ok"; else fail_check "IHDR dimensions mismatch (bytes: $dims)"; fi
  else
    fail_check "output file was not created"
  fi
  end_case
}

case_profile() {
  selected "CT-PROFILE-$OS_LABEL" || return 0
  begin_case "CT-PROFILE-$OS_LABEL" 1
  make_profile_path
  run_cli "$CMD_TIMEOUT" navigate --profile-dir "$P_NATIVE" about:blank
  check_run 0 "=url=about:blank" "-"
  check_profile_layout "$P_UNIX"
  if [ "$IS_WINDOWS" -eq 0 ]; then
    local mode
    mode="$(file_mode "$P_UNIX" || true)"
    if [ "$mode" = 700 ]; then side "profile root mode 0700"; else fail_check "profile root mode is '${mode:-unknown}' (expected 700)"; fi
  else
    side "profile root under %LOCALAPPDATA% (spec-defined OS difference, not counted as a diff)"
  fi
  end_case
}

# ---------------------------------------------------------------------------
# 補助ケース
# ---------------------------------------------------------------------------
# aux_simple <ID> <終了コード> <stdout 期待> <stderr 期待> <args...>
aux_simple() {
  local id="$1" rc="$2" eo="$3" ee="$4"
  shift 4
  selected "$id" || return 0
  begin_case "$id" 0
  run_cli "$CMD_TIMEOUT" "$@"
  check_run "$rc" "$eo" "$ee"
  end_case
}

aux_help() {
  selected CT-CMD-E2 || return 0
  begin_case CT-CMD-E2 0
  run_cli "$CMD_TIMEOUT" --help
  check_run 0 "*" "-"
  case "$CUR_STDOUT" in "Usage: fandhe-browser [COMMAND] [ARGS]..."*) ;; *) fail_check "help does not start with the usage line" ;; esac
  local w
  for w in launch navigate query screenshot help "--profile-dir PATH"; do
    case "$CUR_STDOUT" in *"$w"*) ;; *) fail_check "help lacks '$w'" ;; esac
  done
  end_case
}

aux_launch_bind() {
  selected CT-LAUNCH-E2 || return 0
  begin_case CT-LAUNCH-E2 0
  if ! command -v node >/dev/null 2>&1; then unmet_check "node is required to occupy the port"; end_case; return 0; fi
  if port_in_use; then unmet_check "port $PORT is already in use before the test"; end_case; return 0; fi
  node -e 'const s=require("net").createServer();s.listen(Number(process.argv[1]),"127.0.0.1");setInterval(()=>{try{process.kill(process.ppid,0)}catch{process.exit(0)}},200)' "$PORT" >/dev/null 2>&1 &
  NODE_HOLDER_PID=$!
  local i
  for i in $(seq 1 50); do port_in_use && break; sleep 0.2; done
  if ! port_in_use; then
    unmet_check "could not occupy port $PORT"
  else
    run_cli "$CMD_TIMEOUT" launch
    check_run 1 "*" "=error: failed to bind listener"
  fi
  kill -KILL "$NODE_HOLDER_PID" 2>/dev/null || true
  reap "$NODE_HOLDER_PID"
  NODE_HOLDER_PID=""
  # node の shim 経由だと本体の終了が少し遅れるため、次のケース（CT-PROFILE-E3 等）に備えてポートの解放を待つ
  wait_port_free || note "port $PORT still busy after releasing the holder"
  cleanup_default_root
  end_case
}

aux_shot_exists() {
  selected CT-SHOT-E1 || return 0
  begin_case CT-SHOT-E1 0
  local f="$CASE_DIR/exists.png"
  printf 'keep-me' >"$f"
  run_cli "$CMD_TIMEOUT" screenshot "$FIXTURE_URL" "$(native_path "$f")"
  check_run 1 "*" "=error: output file already exists"
  if [ "$(cat "$f")" = keep-me ]; then side "existing file unchanged"; else fail_check "existing file was modified"; fi
  end_case
}

aux_shot_timeout() {
  selected CT-SHOT-E4 || return 0
  begin_case CT-SHOT-E4 0
  local v
  for v in 61 0 abc 5.0; do
    run_cli "$CMD_TIMEOUT" screenshot --timeout "$v" "$FIXTURE_URL" "$(native_path "$CASE_DIR/n-$v.png")"
    absorb_output
    if [ "$RC" -ne 2 ]; then fail_check "[$v] exit code $RC (expected 2)"; fi
    if [ "$CUR_STDERR" != "error: invalid value for option '--timeout'$USAGE_SUFFIX" ]; then fail_check "[$v] stderr mismatch (got: $(clip "$CUR_STDERR" 120))"; fi
    if [ -e "$CASE_DIR/n-$v.png" ]; then fail_check "[$v] output file was created"; fi
  done
  side "four invalid values rejected without creating files"
  end_case
}

aux_profile_inuse() {
  selected CT-PROFILE-E3 || return 0
  begin_case CT-PROFILE-E3 0
  if port_in_use; then unmet_check "port $PORT is already in use"; end_case; return 0; fi
  make_profile_path
  if start_server launch --profile-dir "$P_NATIVE"; then
    run_cli "$CMD_TIMEOUT" navigate --profile-dir "$P_NATIVE" about:blank
    check_run 1 "*" "=error: profile is already in use"
  else
    fail_check "launch --profile-dir did not become ready (stderr: $(clip "$(tr -d '\r' <"$SRV_ERR")" 160))"
  fi
  stop_proc "$SRV_PID" || true
  reap "$SRV_PID"
  wait_port_free || true
  end_case
}

aux_profile_outside() {
  selected CT-PROFILE-E4 || return 0
  begin_case CT-PROFILE-E4 0
  if [ "$IS_WINDOWS" -eq 1 ]; then
    # %LOCALAPPDATA% の外（既定 RUNNER_TEMP）。%TEMP% は LOCALAPPDATA 配下なので使えない
    local outside_root="${OUTSIDE_DIR_ARG:-${RUNNER_TEMP:-}}" outside_unix lower_la lower_out
    if [ -z "$outside_root" ]; then unmet_check "no directory outside %LOCALAPPDATA% (pass --outside-dir)"; end_case; return 0; fi
    outside_unix="$(cygpath -u "$outside_root")/fandhe-ct-out-$$-$RANDOM"
    lower_la="$(printf '%s' "$LA_UNIX" | tr '[:upper:]' '[:lower:]')"
    lower_out="$(printf '%s' "$outside_unix" | tr '[:upper:]' '[:lower:]')"
    case "$lower_out" in "$lower_la"/*) unmet_check "the outside directory is under %LOCALAPPDATA%"; end_case; return 0 ;; esac
    mkdir -p "$outside_unix"
    LA_CLEAN+=("$outside_unix")
    run_cli "$CMD_TIMEOUT" navigate --profile-dir "$(cygpath -w "$outside_unix/p")" about:blank
    check_run 1 "*" "$WIN_OUTSIDE_ERR"
    if [ -e "$outside_unix/p" ]; then fail_check "directory was created outside %LOCALAPPDATA%"; else side "no directory created outside"; fi
  else
    # Linux・macOS では同じ argv（LOCALAPPDATA の外）が成功するのが期待値
    run_cli "$CMD_TIMEOUT" navigate --profile-dir "$CASE_DIR/outside" about:blank
    check_run 0 "=url=about:blank" "-"
    side "profile outside any LOCALAPPDATA-like root is accepted (unix expectation)"
  fi
  end_case
}

aux_profile_dotdot() {
  selected CT-PROFILE-E5 || return 0
  begin_case CT-PROFILE-E5 0
  if [ "$IS_WINDOWS" -eq 1 ]; then unmet_check "unix only (symlink creation)"; end_case; return 0; fi
  # link -> <case>/a/b。symlink を辿った後の `..`（<case>/a/x）と字面どおりの `..`（<case>/x）のどちらも作られないこと
  mkdir -p "$CASE_DIR/a/b"
  ln -s "$CASE_DIR/a/b" "$CASE_DIR/link"
  run_cli "$CMD_TIMEOUT" navigate --profile-dir "$CASE_DIR/link/../x" about:blank
  check_run 2 "*" "~^error: invalid profile directory: .+$"
  if [ -e "$CASE_DIR/a/x" ] || [ -e "$CASE_DIR/x" ]; then fail_check "a directory was created"; else side "no directory created"; fi
  end_case
}

aux_profile_symlink() {
  selected CT-PROFILE-E6 || return 0
  begin_case CT-PROFILE-E6 0
  if [ "$IS_WINDOWS" -eq 1 ]; then unmet_check "unix only (symlink creation)"; end_case; return 0; fi
  mkdir -p "$CASE_DIR/real"
  ln -s "$CASE_DIR/real" "$CASE_DIR/link"
  run_cli "$CMD_TIMEOUT" navigate --profile-dir "$CASE_DIR/link/p" about:blank
  check_run 1 "*" "~^error: invalid profile directory: a path component is a symlink; .+$"
  if [ -e "$CASE_DIR/real/p" ]; then fail_check "a directory was created at the symlink target"; else side "no directory created at the target"; fi
  end_case
}

# ---------------------------------------------------------------------------
# 実行
# ---------------------------------------------------------------------------
check_fixture
MSYS2_ARG_CONV_EXCL='*' jq -cn --arg commit "$COMMIT" --arg date "$RUN_DATE" --arg os "$OS_NAME" --arg osv "$OS_VERSION" --arg arch "$ARCH" \
  --arg bin "$BIN_REC" --arg host "$([ -n "$RENDER_HOST" ] && basename "$RENDER_HOST" || true)" --arg fixture "$FIXTURE_URL" \
  --argjson fok "$FIXTURE_OK" --arg fdetail "$FIXTURE_DETAIL" --argjson port "$PORT" \
  '{type:"meta",schema_version:1,suite:"cli-conformance",spec:"docs/design/cli-conformance-tests.md",commit:$commit,date:$date,
    os:$os,os_version:$osv,arch:$arch,bin:$bin,render_host:(if $host=="" then null else $host end),
    fixture_url:$fixture,fixture_ok:($fok==1),fixture_detail:$fdetail,launch_port:$port}' >"$OUT_JSONL"
echo "fixture: $FIXTURE_DETAIL"

case_launch
case_nav
case_query
case_shot
case_profile

aux_simple CT-CMD-E1 2 "*" "=error: unknown command 'nope'$USAGE_SUFFIX" nope
aux_help
aux_simple CT-LAUNCH-E1 2 "*" "=error: unknown option '--bogus'$USAGE_SUFFIX" launch --bogus
aux_launch_bind
aux_simple CT-NAV-E1 2 "*" "=error: missing required argument 'URL'$USAGE_SUFFIX" navigate
aux_simple CT-NAV-E2 1 "*" "=error: navigation to private or loopback address is not allowed" navigate "http://127.0.0.1/"
if [ "$IS_WINDOWS" -eq 1 ]; then
  aux_simple CT-NAV-E3 1 "*" "=error: unsupported URL scheme (only http, https and about:blank are allowed)" navigate "file:///C:/Windows/win.ini"
else
  aux_simple CT-NAV-E3 1 "*" "=error: unsupported URL scheme (only http, https and about:blank are allowed)" navigate "file:///etc/passwd"
fi
aux_simple CT-QUERY-E1 2 "*" "=error: options '--all' and '--attr' cannot be used together$USAGE_SUFFIX" query --all --attr href "$FIXTURE_URL" a
aux_simple CT-QUERY-E2 1 "*" "=error: query requires a fetched document (about:blank has no content)" query about:blank h1
if selected CT-QUERY-E3; then
  if [ "$FIXTURE_OK" -eq 1 ]; then
    aux_simple CT-QUERY-E3 1 "*" "=error: no element matches the selector" query "$FIXTURE_URL" h1
  else
    begin_case CT-QUERY-E3 0; fixture_unmet; end_case
  fi
fi
aux_shot_exists
aux_simple CT-SHOT-E2 2 "*" "=error: option '--full-page' is not supported yet$USAGE_SUFFIX" screenshot --full-page "$FIXTURE_URL" "$(native_path "$BASE/n-full.png")"
aux_simple CT-SHOT-E3 1 "*" "=error: screenshot requires a fetched document (about:blank has no content)" screenshot about:blank "$(native_path "$BASE/n-blank.png")"
aux_shot_timeout
aux_simple CT-PROFILE-E1 2 "*" "=error: missing required argument 'PATH'$USAGE_SUFFIX" navigate --profile-dir
aux_simple CT-PROFILE-E2 2 "*" "=error: unknown option '--profile'$USAGE_SUFFIX" navigate --profile p about:blank
aux_profile_inuse
aux_profile_outside
aux_profile_dotdot
aux_profile_symlink

cleanup_default_root

# ---------------------------------------------------------------------------
# 集計
# ---------------------------------------------------------------------------
{
  echo
  echo "- 必須ケース（この OS の分）: 合格 $REQ_PASS / 不合格 $REQ_FAIL / 前提未充足 $REQ_UNMET"
  echo "- 補助ケース: 合格 $AUX_PASS / 不合格 $AUX_FAIL / 前提未充足 $AUX_UNMET"
  echo "- FIXTURE_URL 事前確認: $FIXTURE_DETAIL"
  echo "- 合格条件（仕様書 5 章）は 3 OS の必須 15 ケースが期待どおりで、正規化後の結果が OS 間で一致すること。前提未充足は合格に数えない"
} >>"$OUT_MD"
echo "required: pass=$REQ_PASS fail=$REQ_FAIL unmet=$REQ_UNMET / auxiliary: pass=$AUX_PASS fail=$AUX_FAIL unmet=$AUX_UNMET (cases=$RESULT_COUNT)"
echo "jsonl: $OUT_JSONL"
echo "markdown: $OUT_MD"

if [ "$STRICT" -eq 1 ] && [ $((REQ_FAIL + REQ_UNMET)) -gt 0 ]; then exit 1; fi
exit 0
