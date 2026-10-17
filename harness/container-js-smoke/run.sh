#!/usr/bin/env bash
#
# 既定コンテナ（V8 同梱・musl 静的・scratch）の中で JS が実行できることを確かめる
# （TASK-66.3・Issue #304・CTR-2・JS-1・MS-5。親 Issue #301）。
# 呼び出し元は scripts/harness.sh の check-container-js-smoke（Makefile の同名ターゲット）。
#
# 本スクリプトは引数検証・任意の docker build・イメージ情報の出力・環境行の出力だけを担い、
# ワーカープロトコルの送受信と判定は driver.mjs が行う（実バイナリを JS ワーカーモードで起動する）。
# 軽量コンテナ（boa）は対象外（既定コンテナの V8 musl 実行の確認が目的）。
# 出力契約・終了コード・既知の限界・実行結果は README.md を参照。
#
# コンテナ実行条件は driver.mjs が固定する: --network none・--cap-drop ALL・no-new-privileges・--read-only・
# 非 root（イメージ既定の UID 65532）。ボリュームマウント・ポート公開はしない（security.md）。
#
# 依存は bash + docker + node（>=18）+ coreutils のみ（dependency-policy）。bash 3.2 でも動く。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"

# 出力値の安全文字種（改行・空白・`::` による GitHub Actions ワークフローコマンド誤解釈の防止）。
IMAGE_REF_RE='^[a-z0-9][a-z0-9._/-]{0,127}(:[A-Za-z0-9._-]{1,128})?$'
PLATFORM_RE='^linux/(amd64|arm64)$'
DIGEST_RE='^sha256:[0-9a-f]{64}$'
TIMEOUT_RE='^([1-9][0-9]?|1[01][0-9]|120)$'
SAFE_RE='^[A-Za-z0-9._:/+-]{1,128}$'

usage() {
  cat >&2 <<'USAGE'
Usage:
  run.sh [--image <ref>] [--platform <linux/amd64|linux/arm64>] [--skip-build] [--case-timeout SEC]

  --image         Image tag (default: fandhe-browser:default).
  --platform      linux/amd64 or linux/arm64 (default: the docker daemon's native platform).
  --skip-build    Use the existing image; do not build (exit 2 if missing).
  --case-timeout  Per-case timeout in seconds, 1-120 (default: 20).
USAGE
}

# $1=name $2=value $3=regex。不一致は値を出力せず exit 2。
require_match() {
  if ! [[ "$2" =~ $3 ]]; then
    echo "error: $1 is invalid (must match $3)" >&2
    exit 2
  fi
}

main() {
  local image="fandhe-browser:default" platform="" skip_build="" tmo="20"
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --image) [ "$#" -ge 2 ] || { usage; exit 2; }; image="$2"; shift 2 ;;
      --platform) [ "$#" -ge 2 ] || { usage; exit 2; }; platform="$2"; shift 2 ;;
      --case-timeout) [ "$#" -ge 2 ] || { usage; exit 2; }; tmo="$2"; shift 2 ;;
      --skip-build) skip_build=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) echo "error: unknown argument" >&2; usage; exit 2 ;;
    esac
  done
  require_match "--image" "$image" "$IMAGE_REF_RE"
  [ -z "$platform" ] || require_match "--platform" "$platform" "$PLATFORM_RE"
  require_match "--case-timeout" "$tmo" "$TIMEOUT_RE"

  local tool
  for tool in docker node; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required but was not found on PATH" >&2; exit 2; }
  done
  local node_major
  node_major=$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)
  if ! [[ "$node_major" =~ ^[0-9]+$ ]] || [ "$node_major" -lt 18 ]; then
    echo "error: node >= 18 is required" >&2
    exit 2
  fi
  local server_version
  server_version=$(docker version --format '{{.Server.Version}}' 2>/dev/null | tr -d '\r') \
    || { echo "error: docker daemon is not reachable" >&2; exit 2; }
  require_match "docker server version" "$server_version" "$SAFE_RE"

  local built="no"
  if [ -z "$skip_build" ]; then
    local build_plat=()
    [ -z "$platform" ] || build_plat=(--platform "$platform")
    echo "info: building ${image} (target default)" >&2
    docker build -f "${REPO_ROOT}/docker/Dockerfile.release" --target default \
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
  echo "container-js-smoke-env: role=default image=${image} id=${img_id} platform=${img_plat} docker_server=${server_version} commit=${commit} dirty=${dirty} built=${built} protocol=5 engine=v8"

  local drv_args=(--image "$image" --case-timeout "$tmo")
  [ -z "$platform" ] || drv_args+=(--platform "$platform")
  node "${HERE}/driver.mjs" "${drv_args[@]}"
}

# source された場合は main を実行しない。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
