#!/usr/bin/env bash
#
# 配布コンテナ（既定 = V8 同梱 / 軽量 = boa 同梱）と比較対象（Playwright 公式イメージ）の
# イメージサイズを計測して `container-size:` 行で出力する
# （TASK-65.1・Issue #487・CTR-1・CTR-5・MS-5。親 Issue #300）。
# 呼び出し元は scripts/harness.sh の measure-container-size（Makefile の同名ターゲット）。
# 出力の読み手は後続の #488（TASK-65.2・測定レポート）と TASK-68（95% 削減の達成確認）。
# 本スクリプトは計測のみでゲートではない（閾値判定はしない。閾値未達でも exit 0）。
# 出力契約・終了コード・計測方法の理由は README.md を参照。
#
# 主指標はレイヤサイズの合計（docker history の SIZE の和）。containerd image store では
# `docker image inspect` の .Size / `docker images` の SIZE が圧縮 blob 分だけ大きくなるため、
# store 種別によらず同じ意味になるレイヤ合計を使い、.Size は参考値として併記する。
# コンテナは起動しない（build・pull・inspect・history のみ）。イメージの削除もしない。
#
# 依存は bash + docker + jq + awk + coreutils のみ（dependency-policy）。
# macOS の bash 3.2 でも動くよう連想配列・mapfile は使わない。
set -euo pipefail

# 出力値の安全文字種（改行・空白・`::` による GitHub Actions ワークフローコマンド誤解釈の防止）。
LABEL_RE='^[A-Za-z0-9._-]{1,64}$'
# イメージ参照（repo[:tag]）。先頭が英数字なので `-` 始まりのオプション注入はできない。
IMAGE_REF_RE='^[a-z0-9][a-z0-9._/-]{0,127}(:[A-Za-z0-9._-]{1,128})?$'
# 比較対象は tag 必須（固定値の記録のため）。
REFERENCE_REF_RE='^[a-z0-9][a-z0-9._/-]{0,127}:[A-Za-z0-9._-]{1,128}$'
DIGEST_RE='^sha256:[0-9a-f]{64}$'
PLATFORM_RE='^linux/(amd64|arm64)$'
# バイト数は 12 桁まで（削減率の整数演算 diff * 10000 が 64bit に収まる範囲）。
BYTES_RE='^[0-9]{1,12}$'

usage() {
  cat >&2 <<'USAGE'
Usage:
  measure.sh --reference-image <repo:tag> --reference-digest <sha256:...>
             [--default-image <ref>] [--lite-image <ref>] [--platform <os/arch>]
             [--skip-build] [--no-pull]

  --reference-image   Comparison image (tag required, e.g. mcr.microsoft.com/playwright:v1.49.0-noble).
  --reference-digest  Pinned manifest list digest of the comparison image (verified after pull).
  --default-image     Default container tag (default: fandhe-browser:default).
  --lite-image        Lite container tag (default: fandhe-browser:lite).
  --platform          linux/amd64 or linux/arm64 (default: the docker daemon's native platform).
  --skip-build        Measure existing images; do not build (exit 2 if missing).
  --no-pull           Do not pull the comparison image (exit 2 if missing).
USAGE
}

# $1=name $2=value $3=regex。不一致は値を出力せず exit 2。
require_match() {
  if ! [[ "$2" =~ $3 ]]; then
    echo "error: $1 is invalid (must match $3)" >&2
    exit 2
  fi
}

# $1=bytes → 10 進 MB（10^6）を小数第 2 位まで。小数点をカンマにするロケールで出力契約が壊れないよう C ロケールに固定する
bytes_to_mb() {
  LC_ALL=C awk -v b="$1" 'BEGIN { printf "%.2f", b / 1000000 }'
}

# docker history --human=false --format '{{.Size}}' の出力（stdin）を合算してバイト数を返す。
# CR 除去・空行無視。数値以外の行は exit 2（黙って 0 扱いにしない）。
sum_layer_bytes() {
  local line total=0
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line//$'\r'/}"
    [ -n "$line" ] || continue
    if ! [[ "$line" =~ ^[0-9]{1,15}$ ]]; then
      echo "error: unexpected layer size from docker history" >&2
      exit 2
    fi
    total=$((total + line))
  done
  echo "$total"
}

# $1=対象バイト数 $2=参照バイト数 → 参照比の削減率（%）を小数第 2 位で切り捨てて返す。
# 四捨五入だと閾値（70%・95%）を跨いで過大表示しうるため切り捨てる。対象が大きい場合は負数。
reduction_pct() {
  local n="$1" r="$2" diff sign="" p
  if ! [[ "$n" =~ $BYTES_RE ]] || ! [[ "$r" =~ $BYTES_RE ]]; then
    echo "error: invalid byte count for reduction" >&2
    exit 2
  fi
  if [ "$r" -eq 0 ]; then
    echo "error: reference size is 0" >&2
    exit 2
  fi
  if [ "$n" -gt "$r" ]; then
    diff=$((n - r))
    sign="-"
  else
    diff=$((r - n))
  fi
  p=$((diff * 10000 / r))
  [ "$p" -gt 0 ] || sign=""
  printf '%s%d.%02d' "$sign" $((p / 100)) $((p % 100))
}

# docker・jq・awk の存在と daemon 到達を確認する（不可は exit 2）。
require_docker() {
  local tool
  for tool in docker jq awk; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required but was not found on PATH" >&2; exit 2; }
  done
  docker version --format '{{.Server.Version}}' >/dev/null 2>&1 \
    || { echo "error: docker daemon is not reachable" >&2; exit 2; }
}

# $1=image ref → 存在すれば 0
image_exists() {
  docker image inspect --format '{{.Id}}' "$1" >/dev/null 2>&1
}

# 計測結果の受け渡し用グローバル（サブシェルだと exit が伝わらないため）。
M_BYTES="" M_DSIZE="" M_ID="" M_DIGEST="none"

# $1=role $2=inspect/history に渡す参照 $3=期待 platform $4=期待 RepoDigest（空なら検証しない）
# inspect と history から M_* を設定する。検証失敗は fail-closed で exit 1。
measure_image() {
  local role="$1" ref="$2" platform="$3" want_repo_digest="$4" info os arch hist
  if ! info=$(docker image inspect --format '{{json .}}' "$ref"); then
    echo "error: docker image inspect failed for role=${role}" >&2
    exit 1
  fi
  M_ID=$(printf '%s' "$info" | jq -r '.Id // empty' | tr -d '\r')
  M_DSIZE=$(printf '%s' "$info" | jq -r '.Size // empty' | tr -d '\r')
  os=$(printf '%s' "$info" | jq -r '.Os // empty' | tr -d '\r')
  arch=$(printf '%s' "$info" | jq -r '.Architecture // empty' | tr -d '\r')
  if ! [[ "$M_ID" =~ $DIGEST_RE ]]; then
    echo "error: role=${role} image id is not a sha256 digest" >&2
    exit 1
  fi
  if ! [[ "$M_DSIZE" =~ $BYTES_RE ]] || [ "$M_DSIZE" -lt 1 ]; then
    echo "error: role=${role} .Size is not a positive integer" >&2
    exit 1
  fi
  if [ "${os}/${arch}" != "$platform" ]; then
    echo "error: role=${role} platform mismatch: expected=${platform} actual=${os}/${arch}" >&2
    exit 1
  fi
  if [ -n "$want_repo_digest" ]; then
    if ! printf '%s' "$info" | jq -e --arg d "$want_repo_digest" '(.RepoDigests // []) | index($d) != null' >/dev/null; then
      echo "error: role=${role} RepoDigests does not contain the pinned digest" >&2
      exit 1
    fi
    M_DIGEST="${want_repo_digest#*@}"
  else
    M_DIGEST="none"
  fi
  if ! hist=$(docker history --human=false --format '{{.Size}}' "$ref"); then
    echo "error: docker history failed for role=${role}" >&2
    exit 1
  fi
  M_BYTES=$(printf '%s\n' "$hist" | sum_layer_bytes)
  if [ "$M_BYTES" -lt 1 ]; then
    echo "error: role=${role} layer size sum is 0" >&2
    exit 1
  fi
}

# $1=role $2=image label $3=platform $4=reference_bytes（空なら参照行）
print_row() {
  local role="$1" label="$2" platform="$3" refb="$4" line
  line="container-size: role=${role} image=${label} digest=${M_DIGEST} id=${M_ID} platform=${platform} method=history-sum bytes=${M_BYTES} mb=$(bytes_to_mb "$M_BYTES") docker_size_bytes=${M_DSIZE} docker_size_mb=$(bytes_to_mb "$M_DSIZE")"
  if [ -n "$refb" ]; then
    line="${line} reference_bytes=${refb} reduction_pct=$(reduction_pct "$M_BYTES" "$refb") docker_size_reduction_pct=$(reduction_pct "$M_DSIZE" "$refb")"
  fi
  echo "$line"
}

main() {
  local DEFAULT_IMAGE="fandhe-browser:default" LITE_IMAGE="fandhe-browser:lite"
  local REF_IMAGE="" REF_DIGEST="" PLATFORM="" SKIP_BUILD=0 NO_PULL=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --default-image|--lite-image|--reference-image|--reference-digest|--platform)
        [ $# -ge 2 ] || { echo "error: $1 requires a value" >&2; usage; exit 2; }
        case "$1" in
          --default-image) DEFAULT_IMAGE="$2" ;;
          --lite-image) LITE_IMAGE="$2" ;;
          --reference-image) REF_IMAGE="$2" ;;
          --reference-digest) REF_DIGEST="$2" ;;
          --platform) PLATFORM="$2" ;;
        esac
        shift 2 ;;
      --skip-build) SKIP_BUILD=1; shift ;;
      --no-pull) NO_PULL=1; shift ;;
      *) echo "error: unknown argument: $1" >&2; usage; exit 2 ;;
    esac
  done
  [ -n "$REF_IMAGE" ] || { echo "error: --reference-image is required" >&2; usage; exit 2; }
  [ -n "$REF_DIGEST" ] || { echo "error: --reference-digest is required" >&2; usage; exit 2; }
  require_match "--default-image" "$DEFAULT_IMAGE" "$IMAGE_REF_RE"
  require_match "--lite-image" "$LITE_IMAGE" "$IMAGE_REF_RE"
  require_match "--reference-image" "$REF_IMAGE" "$REFERENCE_REF_RE"
  require_match "--reference-digest" "$REF_DIGEST" "$DIGEST_RE"
  [ -z "$PLATFORM" ] || require_match "--platform" "$PLATFORM" "$PLATFORM_RE"

  require_docker

  if [ -z "$PLATFORM" ]; then
    PLATFORM=$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}' | tr -d '\r')
    require_match "daemon platform" "$PLATFORM" "$PLATFORM_RE"
  fi

  # 環境情報（image store 種別は .Size の意味に影響するため必ず記録する）
  local DOCKER_VER STORAGE_DRIVER STORE_KIND DRIVER_STATUS ROOT COMMIT DIRTY
  DOCKER_VER=$(docker version --format '{{.Server.Version}}' | tr -d '\r')
  STORAGE_DRIVER=$(docker info --format '{{.Driver}}' | tr -d '\r')
  DRIVER_STATUS=$(docker info --format '{{json .DriverStatus}}' | tr -d '\r')
  case "$DRIVER_STATUS" in
    *io.containerd.snapshotter*) STORE_KIND="containerd-snapshotter" ;;
    *) STORE_KIND="graph-driver" ;;
  esac
  require_match "docker server version" "$DOCKER_VER" "$LABEL_RE"
  require_match "storage driver" "$STORAGE_DRIVER" "$LABEL_RE"

  ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  COMMIT=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null | tr -d '\r') || COMMIT="unknown"
  [[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] || COMMIT="unknown"
  if [ "$COMMIT" = "unknown" ]; then
    DIRTY="unknown"
  elif [ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]; then
    DIRTY=1
  else
    DIRTY=0
  fi

  local REF_REPO REF_PIN BUILT="no"
  REF_REPO="${REF_IMAGE%:*}"
  REF_PIN="${REF_REPO}@${REF_DIGEST}"

  if [ "$SKIP_BUILD" -eq 0 ]; then
    local target img
    for target in default lite; do
      img="$DEFAULT_IMAGE"
      [ "$target" = "lite" ] && img="$LITE_IMAGE"
      echo "building: target=${target} image=${img} platform=${PLATFORM}" >&2
      # ビルド出力は標準エラーへ（標準出力は結果行専用）
      if ! docker build -f "${ROOT}/docker/Dockerfile.release" --platform "$PLATFORM" --target "$target" -t "$img" "$ROOT" 1>&2; then
        echo "error: docker build failed for target=${target}" >&2
        exit 1
      fi
    done
    BUILT="yes"
  else
    image_exists "$DEFAULT_IMAGE" || { echo "error: --skip-build but image not found: ${DEFAULT_IMAGE}" >&2; exit 2; }
    image_exists "$LITE_IMAGE" || { echo "error: --skip-build but image not found: ${LITE_IMAGE}" >&2; exit 2; }
  fi

  if ! image_exists "$REF_PIN"; then
    if [ "$NO_PULL" -eq 1 ]; then
      echo "error: --no-pull but comparison image not found locally: ${REF_PIN}" >&2
      exit 2
    fi
    echo "pulling: ${REF_PIN} platform=${PLATFORM}" >&2
    if ! docker pull --platform "$PLATFORM" "$REF_PIN" 1>&2; then
      echo "error: docker pull failed for ${REF_PIN}" >&2
      exit 1
    fi
  fi

  echo "container-size-env: docker_server=${DOCKER_VER} storage_driver=${STORAGE_DRIVER} image_store=${STORE_KIND} platform=${PLATFORM} commit=${COMMIT} dirty=${DIRTY} built=${BUILT}"

  measure_image reference "$REF_PIN" "$PLATFORM" "$REF_PIN"
  local REF_BYTES="$M_BYTES"
  print_row reference "$REF_IMAGE" "$PLATFORM" ""

  measure_image default "$DEFAULT_IMAGE" "$PLATFORM" ""
  print_row default "$DEFAULT_IMAGE" "$PLATFORM" "$REF_BYTES"

  measure_image lite "$LITE_IMAGE" "$PLATFORM" ""
  print_row lite "$LITE_IMAGE" "$PLATFORM" "$REF_BYTES"
}

# self-test から関数を source できるよう、直接実行時のみ main を呼ぶ。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
