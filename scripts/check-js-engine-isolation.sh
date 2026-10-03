#!/usr/bin/env bash
#
# JS エンジンのビルド構成ごとに、依存グラフへ混入してはならないエンジン crate が
# 含まれていないことを検証する正本スクリプト（TASK-32.4・JS-1・MS-3・Issue #168）。
# 呼び出し元は Makefile の check-js-engine-isolation ターゲット（薄いラッパー）と
# .github/workflows/ci.yml の js-engine-isolation ジョブ（3 OS matrix）で、判定
# ロジックはこのファイルに一本化する（scripts/check-render-isolation.sh と同じ作法）。
#
# 検査は 2 本立て（対象は fandhe-browser-cli。feature は cli -> core -> js と連鎖する）:
#   A. 軽量ビルド（--no-default-features --features js-boa）: v8 が 0 件であること。
#      陽性対照として boa_engine が 1 件以上あること（feature 連鎖が切れて
#      「エンジンなし」のまま通過する fail-open を防ぐ）
#   B. エンジンなし（--no-default-features）: v8・boa_engine が 0 件であること
#
# `cargo tree` は既定でホストのターゲットに絞るため、target 依存は当該 OS の
# ランナーでしか現れない。よって 3 OS それぞれで実行する（.claude/rules/ci.md）。
# 判定は `--format '{p}'` のパッケージ行頭一致（`^v8 v`）で行い、名前の一部に
# v8 を含むだけの無関係な crate を誤検出しない。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 検出パターンの正本（外部入力から組み立てない固定値）
PATTERN_V8='^v8 v'
PATTERN_ANY_ENGINE='^(v8|boa_engine) v'
PATTERN_BOA='^boa_engine v'
PATTERN_CLI_ROOT='^fandhe-browser-cli v'
CLI_MANIFEST='crates/fandhe-browser-cli/Cargo.toml'

# $1=ラベル, $2=禁止パターン, $3=cargo tree の標準出力
# 一致があれば NG と一致行を標準エラーへ出して 1 を返す。パイプは使わない
# （pipefail + SIGPIPE による fail-open を避けるため。check-render-isolation.sh 参照）。
detect() {
  local label="$1" pattern="$2" out="$3"
  if grep -Eq -- "$pattern" <<<"$out"; then
    echo "NG: ${label} の依存グラフに禁止エンジン crate が含まれています" >&2
    grep -E -- "$pattern" <<<"$out" >&2
    return 1
  fi
  echo "OK: ${label} の依存グラフに禁止エンジン crate は含まれていません"
  return 0
}

# 出力が空・別 package のものでないこと（空出力の素通り防止）。$1=ラベル, $2=出力
require_cli_root() {
  local label="$1" out="$2"
  if ! grep -Eq -- "$PATTERN_CLI_ROOT" <<<"$out"; then
    echo "NG: ${label} の出力に fandhe-browser-cli のルート行がありません" >&2
    return 1
  fi
  return 0
}

# 陽性対照: 必須 crate が含まれること。$1=ラベル, $2=必須パターン, $3=出力
require_present() {
  local label="$1" pattern="$2" out="$3"
  if ! grep -Eq -- "$pattern" <<<"$out"; then
    echo "NG: ${label} の依存グラフに期待するエンジン crate がありません（feature 連鎖の断絶）" >&2
    return 1
  fi
  return 0
}

require_cli_manifest() {
  local manifest="$1"
  if [ ! -f "$manifest" ]; then
    echo "NG: ${manifest} が見つからないため fandhe-browser-cli の検査を実行できません" >&2
    return 1
  fi
  return 0
}

self_test() {
  local failures=0

  if detect "self-test(v8混入)" "$PATTERN_V8" "$(printf 'fandhe-browser-cli v0.1.0\nv8 v152.2.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): v8 混入行を検出できませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): v8 混入行を正しく検出しました"
  fi

  if detect "self-test(boa混入)" "$PATTERN_ANY_ENGINE" "$(printf 'boa_engine v0.22.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): エンジンなし構成での boa_engine 混入を検出できませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): エンジンなし構成での boa_engine 混入を正しく検出しました"
  fi

  if ! detect "self-test(無関係)" "$PATTERN_V8" "$(printf 'windows-sys v0.61.0\nrustix v1.0.0\nboa_engine v0.22.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): 無関係な行・boa のみで v8 を誤検出しました" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 無関係な行・boa のみでは v8 を誤検出しませんでした"
  fi

  if ! detect "self-test(名前の一部)" "$PATTERN_ANY_ENGINE" "$(printf 'foo-v8-bar v1.0.0\nmyv8 v1.0.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): 名前の一部に v8 を含むだけの行を誤検出しました" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 名前の一部に v8 を含むだけの行は誤検出しませんでした"
  fi

  local big_v8="" i
  for ((i = 0; i < 3000; i++)); do
    big_v8+="v8 v152.2.${i}"$'\n'
  done
  if detect "self-test(大量v8混入)" "$PATTERN_V8" "$big_v8" >/dev/null 2>&1; then
    echo "NG(self-test): 大量の v8 混入行を検出できませんでした（fail-open 回帰）" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 大量の v8 混入行を正しく検出しました"
  fi

  if require_cli_root "self-test(空出力)" "" >/dev/null 2>&1; then
    echo "NG(self-test): 空出力を NG にできませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 空出力を正しく NG にしました"
  fi

  if require_present "self-test(陽性対照欠落)" "$PATTERN_BOA" "$(printf 'fandhe-browser-cli v0.1.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): 陽性対照の欠落を NG にできませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 陽性対照の欠落を正しく NG にしました"
  fi

  if require_cli_manifest "crates/__does_not_exist__/Cargo.toml" >/dev/null 2>&1; then
    echo "NG(self-test): cli manifest 不在を NG にできませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): cli manifest 不在を正しく NG にしました"
  fi

  if [ "$failures" -ne 0 ]; then
    echo "NG: self-test に ${failures} 件の失敗があります" >&2
    return 1
  fi
  echo "OK: self-test はすべて成功しました"
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

# CI ではコミット済み Cargo.lock との食い違いを fail させる（完全性の検証）
CARGO_TREE_LOCKED_ARGS=()
if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
  CARGO_TREE_LOCKED_ARGS=(--locked)
fi

STATUS=0

if ! require_cli_manifest "$CLI_MANIFEST"; then
  exit 1
fi

# 検査 A: 軽量ビルド（js-boa のみ）。標準出力だけを判定対象にする
LABEL_A="軽量ビルド（js-boa のみ）"
if ! OUT_A=$(cargo tree -p fandhe-browser-cli --no-default-features --features js-boa -e normal,build,dev --prefix none --format '{p}' ${CARGO_TREE_LOCKED_ARGS[@]+"${CARGO_TREE_LOCKED_ARGS[@]}"}); then
  echo "NG: cargo tree（${LABEL_A}）の実行に失敗しました" >&2
  STATUS=1
else
  require_cli_root "$LABEL_A" "$OUT_A" || STATUS=1
  require_present "$LABEL_A" "$PATTERN_BOA" "$OUT_A" || STATUS=1
  detect "$LABEL_A" "$PATTERN_V8" "$OUT_A" || STATUS=1
fi

# 検査 B: エンジンなし
LABEL_B="エンジンなし"
if ! OUT_B=$(cargo tree -p fandhe-browser-cli --no-default-features -e normal,build,dev --prefix none --format '{p}' ${CARGO_TREE_LOCKED_ARGS[@]+"${CARGO_TREE_LOCKED_ARGS[@]}"}); then
  echo "NG: cargo tree（${LABEL_B}）の実行に失敗しました" >&2
  STATUS=1
else
  require_cli_root "$LABEL_B" "$OUT_B" || STATUS=1
  detect "$LABEL_B" "$PATTERN_ANY_ENGINE" "$OUT_B" || STATUS=1
fi

exit "$STATUS"
