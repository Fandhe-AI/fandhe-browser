#!/usr/bin/env bash
#
# measure.sh の自己テスト（TASK-65.1・CTR-1・CTR-5）。
# 実 daemon・ネットワークは使わず、純関数の検証と、偽 docker シムによる全体経路の検証を行う。
# 呼び出し元は scripts/harness.sh の measure-container-size（計測の前に必ず実行する）。
# シムは呼び出しを記録し、`docker run`（コンテナ起動）が一度も呼ばれないことも検証する。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./measure.sh
source "${HERE}/measure.sh"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok: $1"; }
ng() { FAIL=$((FAIL + 1)); echo "NG: $1" >&2; }

# $1=名前 $2=期待 $3=実測
expect_eq() {
  if [ "$2" = "$3" ]; then ok "$1"; else ng "$1 (expected=[$2] actual=[$3])"; fi
}

# $1=名前 $2=期待終了コード、残り=コマンド（サブシェルで実行）
expect_exit() {
  local name="$1" want="$2" got=0
  shift 2
  ( "$@" ) >/dev/null 2>&1 || got=$?
  expect_eq "$name" "$want" "$got"
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- 純関数 ---
expect_eq "sum_layer_bytes" "53174272" "$(printf '0\n0\n8192\n1495040\n51470336\n200704\n' | sum_layer_bytes)"
expect_eq "sum_layer_bytes blank+CRLF" "53174272" "$(printf '0\r\n\r\n8192\r\n1495040\n\n51470336\n200704' | sum_layer_bytes)"
expect_exit "sum_layer_bytes rejects non-numeric" 2 bash -c "source '${HERE}/measure.sh'; printf '12\nabc\n' | sum_layer_bytes"

expect_eq "reduction 53174272/3390000000" "98.43" "$(reduction_pct 53174272 3390000000)"
expect_eq "reduction truncates 72563283" "97.85" "$(reduction_pct 72563283 3390000000)"
expect_eq "reduction equal" "0.00" "$(reduction_pct 1000 1000)"
expect_eq "reduction larger than reference" "-50.00" "$(reduction_pct 1500 1000)"
expect_exit "reduction reference 0" 2 bash -c "source '${HERE}/measure.sh'; reduction_pct 10 0"

expect_eq "bytes_to_mb" "53.17" "$(bytes_to_mb 53174272)"
expect_eq "bytes_to_mb comma locale" "53.17" "$(LC_ALL=de_DE.UTF-8 bytes_to_mb 53174272)"

re_ok() { [[ "$2" =~ $1 ]]; }
# $1=名前 $2=正規表現 $3=値。一致すれば ok
expect_match() {
  if re_ok "$2" "$3"; then ok "$1"; else ng "$1"; fi
}
expect_match "image ref accepts playwright" "$IMAGE_REF_RE" "mcr.microsoft.com/playwright:v1.49.0-noble"
expect_match "image ref accepts local tag" "$IMAGE_REF_RE" "fandhe-browser:default"
for bad in "a b" "a::b" "a;b" "-x:y" "a:b"$'\n'"c" ""; do
  if re_ok "$IMAGE_REF_RE" "$bad"; then ng "image ref rejects [$bad]"; else ok "image ref rejects [${bad//$'\n'/\\n}]"; fi
done
if re_ok "$REFERENCE_REF_RE" "mcr.microsoft.com/playwright"; then ng "reference requires tag"; else ok "reference requires tag"; fi
D64="sha256:$(printf '0123456789abcdef%.0s' 1 2 3 4)"
expect_match "digest accepts 64 hex" "$DIGEST_RE" "$D64"
if re_ok "$DIGEST_RE" "sha256:abc"; then ng "digest rejects short"; else ok "digest rejects short"; fi
if re_ok "$DIGEST_RE" "sha256:$(printf 'A%.0s' $(seq 1 64))"; then ng "digest rejects upper"; else ok "digest rejects upper"; fi
expect_match "platform accepts linux/arm64" "$PLATFORM_RE" "linux/arm64"
if re_ok "$PLATFORM_RE" "windows/amd64"; then ng "platform rejects windows"; else ok "platform rejects windows"; fi

expect_eq "normalize adds :latest" "fandhe-browser:latest" "$(normalize_image_ref fandhe-browser)"
expect_eq "normalize keeps tag" "fandhe-browser:lite" "$(normalize_image_ref fandhe-browser:lite)"
expect_eq "normalize docker.io/library alias" "fandhe-browser:default" "$(normalize_image_ref docker.io/library/fandhe-browser:default)"
expect_eq "normalize library alias" "fandhe-browser:latest" "$(normalize_image_ref library/fandhe-browser)"
expect_eq "normalize keeps library/ with namespace" "library/a/b:latest" "$(normalize_image_ref library/a/b)"
expect_eq "normalize registry port without tag" "localhost:5000/img:latest" "$(normalize_image_ref localhost:5000/img)"

# --- 偽 docker シム ---
FAKE="${TMP}/fake"
BIN="${TMP}/bin"
mkdir -p "$FAKE" "$BIN"
cat >"${BIN}/docker" <<'SHIM'
#!/usr/bin/env bash
# 偽 docker。FAKE_DIR 配下の固定データを返し、呼び出しを calls.log に記録する。
echo "$*" >>"${FAKE_DIR}/calls.log"
key() { printf '%s' "$1" | tr -c 'A-Za-z0-9' '_'; }
case "$1" in
  version)
    case "$*" in
      *Server.Version*) echo "29.0.0" ;;
      *) echo "linux/amd64" ;;
    esac ;;
  info)
    case "$*" in
      *DriverStatus*) echo "${FAKE_DRIVER_STATUS:-[[\"io.containerd.snapshotter.v1\",\"overlayfs\"]]}" ;;
      *) echo "overlayfs" ;;
    esac ;;
  build|pull) exit 0 ;;
  buildx)
    # imagetools inspect --raw: index（manifest list）を返す
    [ -f "${FAKE_DIR}/index.json" ] || exit 1
    cat "${FAKE_DIR}/index.json" ;;
  image)
    ref="${*: -1}"
    f="${FAKE_DIR}/inspect-$(key "$ref").json"
    [ -f "$f" ] || exit 1
    case "$*" in
      *"{{.Id}}"*) jq -r '.Id' "$f" ;;
      *) cat "$f" ;;
    esac ;;
  history)
    ref="${*: -1}"
    f="${FAKE_DIR}/history-$(key "$ref").txt"
    [ -f "$f" ] || exit 1
    cat "$f" ;;
  *) exit 1 ;;
esac
SHIM
chmod +x "${BIN}/docker"

REF_REPO="mcr.microsoft.com/playwright"
REF_TAG="${REF_REPO}:v1.49.0-noble"
REF_DIGEST="sha256:$(printf 'a%.0s' $(seq 1 64))"
REF_PIN="${REF_REPO}@${REF_DIGEST}"
ID_REF="sha256:$(printf '1%.0s' $(seq 1 64))"
ID_DEF="sha256:$(printf '2%.0s' $(seq 1 64))"
ID_LITE="sha256:$(printf '3%.0s' $(seq 1 64))"

# $1=参照 $2=Id $3=Size $4=arch $5=RepoDigests(JSON) $6=history 内容
fake_image() {
  local k
  k="$(printf '%s' "$1" | tr -c 'A-Za-z0-9' '_')"
  jq -n --arg id "$2" --argjson size "$3" --arg arch "$4" --argjson rd "$5" \
    '{Id:$id, Size:$size, Os:"linux", Architecture:$arch, RepoDigests:$rd}' >"${FAKE}/inspect-${k}.json"
  printf '%b' "$6" >"${FAKE}/history-${k}.txt"
}

PLAT_DIGEST="sha256:$(printf 'b%.0s' $(seq 1 64))"
ZERO_DIGEST="sha256:$(printf '0%.0s' $(seq 1 64))"

reset_fake() {
  rm -f "${FAKE}"/*
  jq -n --arg d "$PLAT_DIGEST" --arg z "$ZERO_DIGEST" '{manifests:[{digest:$d,platform:{os:"linux",architecture:"amd64"}},{digest:$z,platform:{os:"linux",architecture:"arm64"}}]}' >"${FAKE}/index.json"
  fake_image "$REF_PIN" "$ID_REF" 1500000000 amd64 "[\"${REF_PIN}\"]" '1000000000\n0\n'
  fake_image "fandhe-browser:default" "$ID_DEF" 72563283 amd64 '[]' '0\n0\n8192\n1495040\n51470336\n200704\n'
  fake_image "fandhe-browser:lite" "$ID_LITE" 16178674 amd64 '[]' '12271616\n'
}

run_measure() {
  FAKE_DIR="$FAKE" PATH="${BIN}:${PATH}" bash "${HERE}/measure.sh" \
    --reference-image "$REF_TAG" --reference-digest "$REF_DIGEST" "$@"
}

# 正常系: 結果行が完全一致する
reset_fake
OUT="${TMP}/out.txt"
rc=0
run_measure --skip-build >"$OUT" 2>/dev/null || rc=$?
expect_eq "shim: exit 0" "0" "$rc"
COMMIT_NOW="$(git -C "$HERE" rev-parse HEAD 2>/dev/null || echo unknown)"
ENV_LINE="$(sed -n '1p' "$OUT")"
case "$ENV_LINE" in
  "container-size-env: docker_server=29.0.0 storage_driver=overlayfs image_store=containerd-snapshotter platform=linux/amd64 commit=${COMMIT_NOW} dirty="*" built=no") ok "shim: env line" ;;
  *) ng "shim: env line [$ENV_LINE]" ;;
esac
expect_eq "shim: reference row" \
  "container-size: role=reference image=${REF_TAG} digest=${REF_DIGEST} id=${ID_REF} platform=linux/amd64 method=history-sum bytes=1000000000 mb=1000.00 docker_size_bytes=1500000000 docker_size_mb=1500.00" \
  "$(sed -n '2p' "$OUT")"
expect_eq "shim: default row" \
  "container-size: role=default image=fandhe-browser:default digest=none id=${ID_DEF} platform=linux/amd64 method=history-sum bytes=53174272 mb=53.17 docker_size_bytes=72563283 docker_size_mb=72.56 reference_bytes=1000000000 reduction_pct=94.68 docker_size_reduction_pct=95.16" \
  "$(sed -n '3p' "$OUT")"
expect_eq "shim: lite row" \
  "container-size: role=lite image=fandhe-browser:lite digest=none id=${ID_LITE} platform=linux/amd64 method=history-sum bytes=12271616 mb=12.27 docker_size_bytes=16178674 docker_size_mb=16.18 reference_bytes=1000000000 reduction_pct=98.77 docker_size_reduction_pct=98.92" \
  "$(sed -n '4p' "$OUT")"
expect_eq "shim: stdout has 4 lines" "4" "$(wc -l <"$OUT" | tr -d '[:space:]')"
expect_eq "shim: no build/pull/run with --skip-build" "0" "$(grep -cE '^(build|pull|run) ' "${FAKE}/calls.log" || true)"

# graph driver の判定
reset_fake
FAKE_DRIVER_STATUS='[["Backing Filesystem","extfs"]]' run_measure --skip-build >"$OUT" 2>/dev/null || true
case "$(sed -n '1p' "$OUT")" in
  *"image_store=graph-driver"*) ok "shim: graph-driver detection" ;;
  *) ng "shim: graph-driver detection" ;;
esac

# ビルド経路: build が target ごとに呼ばれ、コンテナ起動はしない
reset_fake
run_measure >"$OUT" 2>/dev/null || true
expect_eq "shim: build called for 2 targets" "2" "$(grep -c '^build ' "${FAKE}/calls.log" || true)"
expect_eq "shim: never docker run" "0" "$(grep -c '^run' "${FAKE}/calls.log" || true)"
case "$(sed -n '1p' "$OUT")" in
  *"built=yes") ok "shim: built=yes" ;;
  *) ng "shim: built=yes" ;;
esac

# 比較対象が無い: --no-pull は exit 2、pull 許可でも pull 後に無ければ exit 1
reset_fake
rm -f "${FAKE}/inspect-$(printf '%s' "$REF_PIN" | tr -c 'A-Za-z0-9' '_').json"
rc=0; run_measure --skip-build --no-pull >/dev/null 2>&1 || rc=$?
expect_eq "shim: --no-pull with missing reference" "2" "$rc"
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: pull but still missing" "1" "$rc"
expect_eq "shim: pull invoked with digest ref" "1" "$(grep -c "^pull --platform linux/amd64 ${REF_PIN}\$" "${FAKE}/calls.log" || true)"

# digest 不一致
reset_fake
fake_image "$REF_PIN" "$ID_REF" 1000000000 amd64 '["mcr.microsoft.com/playwright@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"]' '1000000000\n'
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: digest mismatch fails" "1" "$rc"

# --platform 付き pull では RepoDigests に platform 固有 digest が入る（index digest は無い）
reset_fake
fake_image "$REF_PIN" "$ID_REF" 1500000000 amd64 "[\"${REF_REPO}@${PLAT_DIGEST}\"]" '1000000000\n0\n'
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: platform digest in RepoDigests accepted" "0" "$rc"
# 別 platform の digest は受理しない
fake_image "$REF_PIN" "$ID_REF" 1500000000 amd64 "[\"${REF_REPO}@${ZERO_DIGEST}\"]" '1000000000\n'
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: other-platform digest rejected" "1" "$rc"
# index を取得できなければ fail-closed
rm -f "${FAKE}/index.json"
fake_image "$REF_PIN" "$ID_REF" 1500000000 amd64 "[\"${REF_REPO}@${PLAT_DIGEST}\"]" '1000000000\n'
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: unresolvable index fails closed" "1" "$rc"

# default / lite が同一イメージ ID（別名衝突）
reset_fake
fake_image "fandhe-browser:lite" "$ID_DEF" 72563283 amd64 '[]' '51470336\n'
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: same image id for default/lite fails" "1" "$rc"

# アーキ不一致
reset_fake
fake_image "fandhe-browser:lite" "$ID_LITE" 16178674 arm64 '[]' '12271616\n'
rc=0; run_measure --skip-build >/dev/null 2>&1 || rc=$?
expect_eq "shim: arch mismatch fails" "1" "$rc"

# 引数エラー
reset_fake
expect_exit "args: unknown option" 2 run_measure --bogus
expect_exit "args: option without value" 2 run_measure --platform
expect_exit "args: invalid platform" 2 run_measure --platform windows/amd64
expect_exit "args: same default/lite ref" 2 run_measure --default-image x:y --lite-image x:y
expect_exit "args: same ref, tag omitted vs :latest" 2 run_measure --default-image fb --lite-image fb:latest
expect_exit "args: same ref, :latest vs tag omitted" 2 run_measure --default-image fb:latest --lite-image fb
expect_exit "args: docker.io/library alias collision" 2 run_measure --default-image fb:default --lite-image docker.io/library/fb:default
expect_exit "args: invalid image ref" 2 run_measure --default-image 'a b'
expect_exit "args: missing --reference-digest" 2 bash -c "FAKE_DIR='${FAKE}' PATH='${BIN}:${PATH}' bash '${HERE}/measure.sh' --reference-image '${REF_TAG}'"
expect_exit "args: invalid digest" 2 bash -c "FAKE_DIR='${FAKE}' PATH='${BIN}:${PATH}' bash '${HERE}/measure.sh' --reference-image '${REF_TAG}' --reference-digest sha256:abc"

echo "container-size self-test: pass=${PASS} fail=${FAIL}"
[ "$FAIL" -eq 0 ]
