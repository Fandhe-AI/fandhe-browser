#!/usr/bin/env bash
# 配布バイナリの静的リンク検査（TASK-64.1・64.2・64.3・TASK-93.3・CTR-4・PLUG-1・決定 5）。
# docker/Dockerfile.release の verify-default / verify-lite ステージから呼ばれ、失敗すると
# そのステージが落ちて最終 scratch イメージにバイナリが入らない（出荷物そのものの関門）。
#
# ELF の動的リンク判定（file・readelf・ldd）は scripts/check-static-link.sh に委譲する。判定ロジックの
# 正本は 1 本にし、CI の [static-link]（cargo 直ビルド）と同じ基準を出荷物にも適用する。非 ELF 入力は
# fail-closed で NG になる。ここに残すのは Alpine 固有の検査（CA バンドル・apk の所有パッケージ・サイズ記録）のみ。
#
# 使い方: docker/verify-static.sh <バイナリのパス>
set -euo pipefail

bin="${1:?usage: verify-static.sh <binary>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "arch: $(uname -m)"
bash "${here}/../scripts/check-static-link.sh" "${bin}"
test -s /etc/ssl/certs/ca-certificates.crt || {
  echo "error: CA bundle missing or empty" >&2
  exit 1
}
apk info --who-owns /etc/ssl/certs/ca-certificates.crt
# TASK-65 の正式測定との突き合わせ用に記録だけ行う。
echo "binary size: $(stat -c %s "${bin}") bytes"
