#!/usr/bin/env bash
#
# 配布コンテナ（軽量 = boa / 既定 = V8）にローカルフィクスチャ 12 種をボリュームマウントし、
# text / texts / attr 相当の取得を実行して成否を判定する（TASK-66.1・Issue #302・CTR-2・MS-5。親 Issue #301）。
# 呼び出し元は scripts/harness.sh の check-container-smoke（Makefile の同名ターゲット）。
# 後続は #303（TASK-66.2・既定コンテナで本スクリプトを流用）と #304（TASK-66.3・JS スモーク）。
# 出力契約・終了コード・暫定契約・要判断事項は README.md を参照。
#
# 呼び出しは `query [--all | --attr NAME] --file PATH -- SELECTOR`（#824・CLI-1）。一発実行を持たない旧バイナリ
# （argv を解釈せずサーバーとして動き続ける）に当たった場合は preflight で `result=blocked`（exit 2）とし、
# 成功を装わない。構文は build_task_argv に隔離してある。
#
# コンテナ実行条件は固定（呼び出し側から変更不可）: --network none・フィクスチャは :ro・--cap-drop ALL・
# no-new-privileges・非 root（イメージ既定の UID 65532）。SSRF 防御（file: 拒否・private 帯拒否）を
# 回避する経路は作らない（security.md）。
#
# 依存は bash + docker + jq + tar + coreutils のみ（dependency-policy）。
# macOS の bash 3.2 でも動くよう連想配列・mapfile は使わない。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/harness/compat_fixtures"

# 出力値の安全文字種（改行・空白・`::` による GitHub Actions ワークフローコマンド誤解釈の防止）。
IMAGE_REF_RE='^[a-z0-9][a-z0-9._/-]{0,127}(:[A-Za-z0-9._-]{1,128})?$'
PLATFORM_RE='^linux/(amd64|arm64)$'
ROLE_RE='^(lite|default)$'
DIGEST_RE='^sha256:[0-9a-f]{64}$'
TASK_ID_RE='^C(0[1-9]|1[0-2])$'
FIXTURE_RE='^(static|ssr_spa_static)/[0-9]{2}-[a-z0-9-]+\.html$'
ATTR_RE='^[a-z][a-z0-9-]{0,31}$'
KIND_RE='^(text|texts|attr)$'
TIMEOUT_RE='^([1-9][0-9]?|1[01][0-9]|120)$'
SAFE_RE='^[A-Za-z0-9._:/+-]{1,128}$'

# 起動したコンテナ ID（空白区切り）。EXIT trap で必ず rm -f する。
CIDS=""
# preflight で「サーバーとして動き続ける」ことを示すバナー（現行バイナリの起動行）。
BANNER='DevTools listening on '

usage() {
  cat >&2 <<'USAGE'
Usage:
  run.sh --role <lite|default> [--image <ref>] [--platform <linux/amd64|linux/arm64>]
         [--skip-build] [--task-timeout SEC] [--tasks PATH]

  --role          Container role to check (target of docker/Dockerfile.release).
  --image         Image tag (default: fandhe-browser:<role>).
  --platform      linux/amd64 or linux/arm64 (default: the docker daemon's native platform).
  --skip-build    Use the existing image; do not build (exit 2 if missing).
  --task-timeout  Per-task timeout in seconds, 1-120 (default: 20).
  --tasks         Task definition JSON (default: harness/container-smoke/tasks.json).
USAGE
}

# $1=name $2=value $3=regex。不一致は値を出力せず exit 2。
require_match() {
  if ! [[ "$2" =~ $3 ]]; then
    echo "error: $1 is invalid (must match $3)" >&2
    exit 2
  fi
}

# 標準入力の文字列を制御文字を空白へ置換し 300 文字に切り詰めて返す（stderr 用の無害化）。
sanitize_sample() {
  LC_ALL=C tr '\000-\037\177' ' ' | LC_ALL=C cut -c1-300
}

# $1=tasks.json のパス。スキーマ（5.1）を満たさなければ exit 2。
validate_tasks() {
  local path="$1" n i id fixture kind attr
  if ! jq -e 'type == "array" and length == 12' "$path" >/dev/null 2>&1; then
    echo "error: tasks file must be a JSON array of exactly 12 entries" >&2
    exit 2
  fi
  if [ "$(jq -r '[.[].id] | unique | length' "$path")" != "12" ]; then
    echo "error: task ids must be unique" >&2
    exit 2
  fi
  n=$(jq -r 'length' "$path")
  i=0
  while [ "$i" -lt "$n" ]; do
    id=$(jq -r ".[$i].id // empty" "$path")
    fixture=$(jq -r ".[$i].fixture // empty" "$path")
    kind=$(jq -r ".[$i].kind // empty" "$path")
    attr=$(jq -r ".[$i].attr // empty" "$path")
    require_match "task id" "$id" "$TASK_ID_RE"
    require_match "task fixture" "$fixture" "$FIXTURE_RE"
    require_match "task kind" "$kind" "$KIND_RE"
    if [ "$kind" = "attr" ]; then
      require_match "task attr" "$attr" "$ATTR_RE"
    fi
    if ! jq -e ".[$i] | (.selector | type == \"string\" and length > 0 and length <= 200) and (.expected | type == \"array\" and length > 0 and all(type == \"string\"))" "$path" >/dev/null 2>&1; then
      echo "error: task ${id} has an invalid selector or expected" >&2
      exit 2
    fi
    if [ ! -f "${FIXTURES_DIR}/${fixture}" ]; then
      echo "error: fixture for task ${id} does not exist" >&2
      exit 2
    fi
    i=$((i + 1))
  done
}

# 実 CLI の構文（`query [--all | --attr NAME] --file PATH SELECTOR`。TASK-47.4・#285・#824・CLI-1・PERF-7）に
# 合わせ、タスク 1 件分の argv を TASK_ARGV に設定する。ENTRYPOINT が /fandhe-browser のため先頭は query。
# $1=kind $2=selector $3=attr(空可) $4=コンテナ内のフィクスチャパス。
# 構文が変わったらこの関数だけ差し替える。
TASK_ARGV=()
build_task_argv() {
  local kind="$1" selector="$2" attr="$3" path="$4"
  TASK_ARGV=(query)
  case "$kind" in
    texts) TASK_ARGV+=(--all) ;;
    attr) TASK_ARGV+=(--attr "$attr") ;;
  esac
  TASK_ARGV+=(--file "$path" -- "$selector")
}

# $1=kind $2=実測 stdout $3=expected(JSON 配列) → 一致なら 0。
# stdout は JSON 文字列リテラルの 1 値 1 行（query.rs の write_lines）。jq -s -c で配列へ正規化して比較する。
# texts は expected 全体と完全一致。text / attr は先頭一致要素 1 件のみを返す仕様のため expected[0] と一致を見る
# 空白は連続を 1 個へ畳み前後を除いて比較する（CLI は textContent をそのまま返し、期待値は core の
# 正規化済みテキストのため。例: C12 の閉じタグなし li）。
# （attr の expected が複数のタスク C04・C05・C10 は先頭要素のみの検証になる。README「判定の限界」）。
compare_result() {
  local kind="$1" actual="$2" expected="$3" norm want
  local ws='map(gsub("\\s+"; " ") | sub("^ "; "") | sub(" $"; ""))'
  norm=$(printf '%s' "$actual" | jq -s -c "$ws" 2>/dev/null) || return 1
  if [ "$kind" = "texts" ]; then
    want=$(printf '%s' "$expected" | jq -c "$ws")
  else
    want=$(printf '%s' "$expected" | jq -c "[.[0]] | $ws")
  fi
  [ "$norm" = "$want" ]
}

# 結果受け渡し用グローバル（サブシェルだと exit・CIDS が伝わらないため）。
RC_STATE="" RC_EXIT="none" RC_OUT="" RC_ERR=""

cleanup() {
  local c
  for c in $CIDS; do
    docker rm -f "$c" >/dev/null 2>&1 || true
  done
  CIDS=""
}

# $1=image $2=platform(空可) $3=timeout、以降=コンテナへ渡す argv。
# コンテナを起動し、timeout 秒まで終了をポーリングする。結果は RC_* に設定する。
# RC_STATE: exited | running（timeout 時点でも終了せず）| error（docker 操作の失敗）
run_container() {
  local image="$1" platform="$2" tmo="$3" cid start
  shift 3
  local plat_args=()
  [ -z "$platform" ] || plat_args=(--platform "$platform")
  RC_STATE="error" RC_EXIT="none" RC_OUT="" RC_ERR=""
  if ! cid=$(docker run -d --network none --cap-drop ALL --security-opt no-new-privileges \
    --read-only --tmpfs /data:uid=65532,gid=65532,mode=0700 \
    -v "${FIXTURES_DIR}:/fixtures:ro" \
    ${plat_args[@]+"${plat_args[@]}"} "$image" "$@" 2>/dev/null); then
    return 0
  fi
  cid="${cid//$'\r'/}"
  if ! [[ "$cid" =~ ^[0-9a-f]{64}$ ]]; then
    return 0
  fi
  CIDS="${CIDS} ${cid}"
  start=$SECONDS
  RC_STATE="running"
  while [ $((SECONDS - start)) -lt "$tmo" ]; do
    if [ "$(docker inspect -f '{{.State.Running}}' "$cid" 2>/dev/null)" = "false" ]; then
      RC_STATE="exited"
      break
    fi
    sleep 1
  done
  # 最後の inspect から期限までの間に正常終了した場合を timeout と誤判定しないよう再確認する。
  if [ "$RC_STATE" = "running" ] \
    && [ "$(docker inspect -f '{{.State.Running}}' "$cid" 2>/dev/null)" = "false" ]; then
    RC_STATE="exited"
  fi
  RC_OUT=$(docker logs "$cid" 2>/dev/null || true)
  RC_ERR=$(docker logs "$cid" 2>&1 >/dev/null || true)
  if [ "$RC_STATE" = "exited" ]; then
    RC_EXIT=$(docker inspect -f '{{.State.ExitCode}}' "$cid" 2>/dev/null || echo none)
    [[ "$RC_EXIT" =~ ^[0-9]{1,3}$ ]] || RC_EXIT="none"
  fi
  # exit 137 は rm -f による後始末の結果であり失敗ではない（README 注意点）。
  docker rm -f "$cid" >/dev/null 2>&1 || true
  CIDS="${CIDS/ ${cid}/}"
  return 0
}

# $1=image → 共有ライブラリ類の件数を標準出力へ。取得失敗は exit 1。
count_shared_libs() {
  local image="$1" cid listing
  if ! cid=$(docker create "$image" 2>/dev/null); then
    echo "error: docker create failed for shared library check" >&2
    exit 1
  fi
  cid="${cid//$'\r'/}"
  CIDS="${CIDS} ${cid}"
  if ! listing=$(docker export "$cid" 2>/dev/null | tar -t 2>/dev/null); then
    # コマンド置換（サブシェル）内のため CIDS が親へ伝わらない。ここで明示的に後始末する。
    docker rm -f "$cid" >/dev/null 2>&1 || true
    echo "error: docker export failed for shared library check" >&2
    exit 1
  fi
  docker rm -f "$cid" >/dev/null 2>&1 || true
  CIDS="${CIDS/ ${cid}/}"
  printf '%s\n' "$listing" | count_lib_entries
}

# tar -t の一覧（stdin）から *.so / *.so.* と lib/ lib64/ usr/lib/ 配下の実体（末尾 / 以外）を数える。
count_lib_entries() {
  local n=0 line
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line#./}"
    case "$line" in
      */) continue ;;
      *.so | *.so.* | lib/* | lib64/* | usr/lib/* | usr/lib64/*) n=$((n + 1)) ;;
    esac
  done
  echo "$n"
}

main() {
  local role="" image="" platform="" skip_build="" tmo="20" tasks="${HERE}/tasks.json"
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --role) [ "$#" -ge 2 ] || { usage; exit 2; }; role="$2"; shift 2 ;;
      --image) [ "$#" -ge 2 ] || { usage; exit 2; }; image="$2"; shift 2 ;;
      --platform) [ "$#" -ge 2 ] || { usage; exit 2; }; platform="$2"; shift 2 ;;
      --task-timeout) [ "$#" -ge 2 ] || { usage; exit 2; }; tmo="$2"; shift 2 ;;
      --tasks) [ "$#" -ge 2 ] || { usage; exit 2; }; tasks="$2"; shift 2 ;;
      --skip-build) skip_build=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) echo "error: unknown argument" >&2; usage; exit 2 ;;
    esac
  done
  require_match "--role" "$role" "$ROLE_RE"
  [ -n "$image" ] || image="fandhe-browser:${role}"
  require_match "--image" "$image" "$IMAGE_REF_RE"
  [ -z "$platform" ] || require_match "--platform" "$platform" "$PLATFORM_RE"
  require_match "--task-timeout" "$tmo" "$TIMEOUT_RE"
  [ -f "$tasks" ] || { echo "error: tasks file not found" >&2; exit 2; }

  local tool
  for tool in docker jq tar; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required but was not found on PATH" >&2; exit 2; }
  done
  validate_tasks "$tasks"
  local server_version
  server_version=$(docker version --format '{{.Server.Version}}' 2>/dev/null | tr -d '\r') \
    || { echo "error: docker daemon is not reachable" >&2; exit 2; }
  require_match "docker server version" "$server_version" "$SAFE_RE"

  trap cleanup EXIT

  local built="no"
  if [ -z "$skip_build" ]; then
    local build_plat=()
    [ -z "$platform" ] || build_plat=(--platform "$platform")
    echo "info: building ${image} (target ${role})" >&2
    docker build -f "${REPO_ROOT}/docker/Dockerfile.release" --target "$role" \
      ${build_plat[@]+"${build_plat[@]}"} -t "$image" "$REPO_ROOT" >&2 \
      || { echo "error: docker build failed" >&2; exit 1; }
    built="yes"
  elif ! docker image inspect "$image" >/dev/null 2>&1; then
    echo "error: image not found (--skip-build given)" >&2
    exit 2
  fi

  local id_info img_id img_plat commit dirty
  id_info=$(docker image inspect --format '{{.Id}} {{.Os}}/{{.Architecture}}' "$image" 2>/dev/null) \
    || { echo "error: docker image inspect failed" >&2; exit 1; }
  img_id="${id_info%% *}"
  img_plat="${id_info#* }"
  img_plat="${img_plat//$'\r'/}"
  require_match "image id" "$img_id" "$DIGEST_RE"
  require_match "image platform" "$img_plat" '^[a-z0-9]+/[a-z0-9]+$'
  commit=$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
  [[ "$commit" =~ ^[0-9a-f]{7,40}$ ]] || commit="unknown"
  if [ "$commit" = "unknown" ]; then
    dirty="unknown"
  elif [ -z "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
    dirty=0
  else
    dirty=1
  fi

  local libs
  libs=$(count_shared_libs "$image")
  echo "container-smoke-env: role=${role} image=${image} id=${img_id} platform=${img_plat} docker_server=${server_version} commit=${commit} dirty=${dirty} built=${built} shared_libs=${libs}"
  if [ "$libs" -gt 0 ]; then
    echo "container-smoke-summary: role=${role} total=12 executed=0 success=0 result=fail reason=shared-libs-present"
    echo "error: ${libs} shared library entries found in the image" >&2
    exit 1
  fi

  # preflight: 先頭タスクの argv で起動し、一発実行されて終了するかを見る。
  local first_kind first_sel first_attr first_fx
  first_kind=$(jq -r '.[0].kind' "$tasks")
  first_sel=$(jq -r '.[0].selector' "$tasks")
  first_attr=$(jq -r '.[0].attr // ""' "$tasks")
  first_fx=$(jq -r '.[0].fixture' "$tasks")
  build_task_argv "$first_kind" "$first_sel" "$first_attr" "/fixtures/${first_fx}"
  run_container "$image" "$platform" "$tmo" "${TASK_ARGV[@]}"
  if [ "$RC_STATE" = "error" ]; then
    echo "error: failed to start the container" >&2
    exit 1
  fi
  if [ "$RC_STATE" = "running" ]; then
    if printf '%s' "$RC_ERR" | grep -qF "$BANNER"; then
      echo "container-smoke-summary: role=${role} total=12 executed=0 success=0 result=blocked reason=one-shot-subcommand-unavailable"
      echo "error: the image runs as a server and has no one-shot text/texts/attr subcommand (it predates query --file, #824)" >&2
      exit 2
    fi
    echo "container-smoke-summary: role=${role} total=12 executed=0 success=0 result=fail reason=timeout"
    exit 1
  fi

  local i=0 success=0 executed=0 id fixture kind sel attr expected status
  while [ "$i" -lt 12 ]; do
    id=$(jq -r ".[$i].id" "$tasks")
    fixture=$(jq -r ".[$i].fixture" "$tasks")
    kind=$(jq -r ".[$i].kind" "$tasks")
    sel=$(jq -r ".[$i].selector" "$tasks")
    attr=$(jq -r ".[$i].attr // \"\"" "$tasks")
    expected=$(jq -c ".[$i].expected" "$tasks")
    build_task_argv "$kind" "$sel" "$attr" "/fixtures/${fixture}"
    run_container "$image" "$platform" "$tmo" "${TASK_ARGV[@]}"
    executed=$((executed + 1))
    case "$RC_STATE" in
      running) status="timeout" ;;
      error) status="nonzero-exit" ;;
      *)
        if [ "$RC_EXIT" != "0" ]; then
          status="nonzero-exit"
        elif ! printf '%s' "$RC_OUT" | jq -s -e 'length > 0 and all(type == "string")' >/dev/null 2>&1; then
          status="invalid-output"
        elif compare_result "$kind" "$RC_OUT" "$expected"; then
          status="ok"
          success=$((success + 1))
        else
          status="mismatch"
          printf 'info: %s mismatch, actual: %s\n' "$id" "$(printf '%s' "$RC_OUT" | sanitize_sample)" >&2
        fi
        ;;
    esac
    echo "container-smoke: role=${role} id=${id} fixture=${fixture} kind=${kind} status=${status} exit=${RC_EXIT}"
    i=$((i + 1))
  done

  if [ "$success" -eq 12 ]; then
    echo "container-smoke-summary: role=${role} total=12 executed=${executed} success=${success} result=pass reason=none"
    exit 0
  fi
  echo "container-smoke-summary: role=${role} total=12 executed=${executed} success=${success} result=fail reason=task-failures"
  exit 1
}

# source された場合（自己テスト）は main を実行しない。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
