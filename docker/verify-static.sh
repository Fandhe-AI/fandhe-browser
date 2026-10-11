#!/usr/bin/env bash
# 配布バイナリの静的リンク検査（TASK-64.1・64.2・64.3・CTR-4・決定 5）。
# docker/Dockerfile.release の verify-default / verify-lite ステージから呼ばれ、失敗すると
# そのステージが落ちて最終 scratch イメージにバイナリが入らない。
#
# 合否は NEEDED=0 と INTERP 無しで判定する。static-pie では file が "static-pie linked" を示し
# 動的セクションも持つため、file の文言一致や動的セクション有無は判定に使わず記録のみ行う。
# musl の ldd は静的 PIE でもローダ自身（ld-musl）の 1 行を出すため、依存を示す "=>" 行のみ失敗にする。
#
# 使い方: docker/verify-static.sh <バイナリのパス>
set -euo pipefail

bin="${1:?usage: verify-static.sh <binary>}"

echo "arch: $(uname -m)"
file "${bin}"
readelf -d "${bin}" || true
needed="$(readelf -d "${bin}" | grep -c NEEDED || true)"
interp="absent"
if readelf -l "${bin}" | grep -q INTERP; then interp="present"; fi
echo "static-link check: NEEDED=${needed}, INTERP=${interp}"
ldd_out="$(ldd "${bin}" 2>&1 || true)"
echo "ldd: ${ldd_out}"
if [ "${needed}" != "0" ] || [ "${interp}" != "absent" ]; then
  echo "error: binary is not statically linked" >&2
  exit 1
fi
if grep -q -e '=>' <<<"${ldd_out}"; then
  echo "error: ldd reports shared libraries" >&2
  exit 1
fi
test -s /etc/ssl/certs/ca-certificates.crt || {
  echo "error: CA bundle missing or empty" >&2
  exit 1
}
apk info --who-owns /etc/ssl/certs/ca-certificates.crt
# TASK-65 の正式測定との突き合わせ用に記録だけ行う。
echo "binary size: $(stat -c %s "${bin}") bytes"
