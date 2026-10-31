#!/usr/bin/env bash
# egress 隔離ハーネス（harness/egress-isolation/）の自己テスト。
# Issue #1377・TASK-38・RENDER-3・SEC-2。実ホスト・bwrap・docker は使わない（偽ホストと loopback のみ）。
# 実行件数が 0 件なら失敗にする（fail-closed）。要 python3。Linux 以外では egress_guard 依存のテストが skip される。
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

out="$(python3 -B -m unittest discover -s harness/egress-isolation -p 'test_*.py' -v 2>&1)" || {
  printf '%s\n' "$out" >&2
  echo "egress-isolation self-test: FAILED" >&2
  exit 1
}
printf '%s\n' "$out"
ran="$(printf '%s\n' "$out" | sed -n 's/^Ran \([0-9][0-9]*\) tests\? in .*/\1/p' | tail -n 1)"
if [ -z "$ran" ] || [ "$ran" -lt 1 ]; then
  echo "egress-isolation self-test: no tests were run" >&2
  exit 1
fi
echo "egress-isolation self-test: ok ($ran tests)"
