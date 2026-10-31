#!/usr/bin/env bash
# egress 隔離の docker `--network none` 実測の準備（Issue #1386・TASK-38・RENDER-1・RENDER-3）。
#
# 役割: Dockerfile（同ディレクトリ）でイメージをビルドし、イメージ内でビルドしたホストを取り出して
#   検証する。取り出したバイナリは measure.py の全モードの `--host` に渡す（sha256 一致が必須）。
#   measure.py は呼ばない。判断は行わず、測定の前提（同一バイナリ・glibc 要求・起動可否）を確かめるだけ。
#
# 使い方:
#   prepare.sh build <tag>
#   prepare.sh extract <tag> <out-dir>   # <out-dir> は既存・空・絶対パス
set -euo pipefail

readonly HOST_IN_IMAGE="/opt/fandhe/bin/fandhe-browser-servo-host"
# measure.DOCKER_IMAGE_RE と同じ（test_container.py が一致を検査する）。
readonly TAG_RE='^[a-z0-9][a-z0-9._/:@-]{0,127}$'

die() { echo "prepare.sh: $*" >&2; exit 1; }

check_tag() {
  [[ "$1" =~ $TAG_RE ]] || die "invalid image tag"
}

cmd="${1:-}"
case "$cmd" in
  build)
    [ "$#" -eq 2 ] || die "usage: prepare.sh build <tag>"
    check_tag "$2"
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
    cd "$root"
    docker build -f harness/egress-isolation/container/Dockerfile -t "$2" .
    ;;
  extract)
    [ "$#" -eq 3 ] || die "usage: prepare.sh extract <tag> <out-dir>"
    tag="$2"; out="$3"
    check_tag "$tag"
    [[ "$out" = /* ]] || die "out-dir must be an absolute path"
    [ -d "$out" ] || die "out-dir must exist"
    [ -z "$(ls -A "$out")" ] || die "out-dir must be empty"

    cid="$(docker create "$tag" /bin/true)"
    trap 'docker rm -f "$cid" >/dev/null 2>&1 || true' EXIT
    docker cp "$cid:$HOST_IN_IMAGE" "$out/"
    host="$out/fandhe-browser-servo-host"
    [ -x "$host" ] || die "extracted host is not executable"

    local_sha="$(sha256sum "$host" | cut -d' ' -f1)"
    image_sha="$(docker run --rm --network none --entrypoint sha256sum "$tag" "$HOST_IN_IMAGE" | cut -d' ' -f1)"
    [ "$local_sha" = "$image_sha" ] || die "sha256 mismatch: extracted=$local_sha image=$image_sha"
    echo "sha256: $local_sha"

    if command -v objdump >/dev/null 2>&1; then
      max="$(objdump -T "$host" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -n 1)"
      echo "max GLIBC requirement: ${max:-none}"
    else
      echo "max GLIBC requirement: not recorded (objdump not found)"
    fi

    "$host" --version
    "$host" --selftest
    echo "host --version / --selftest: ok"
    echo
    echo "next:"
    echo "  python3 -I -B harness/egress-isolation/measure.py --host $host \\"
    echo "    --modes baseline,bwrap-netns,docker-none --docker-image $tag --out \"\$(mktemp -d)\""
    ;;
  *)
    die "usage: prepare.sh build <tag> | extract <tag> <out-dir>"
    ;;
esac
