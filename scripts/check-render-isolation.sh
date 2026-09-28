#!/usr/bin/env bash
#
# 既定ビルド（feature 指定なし）の依存グラフに Servo 系クレートが混入していない
# ことを検証する正本スクリプト（TASK-34.1・RENDER-1・MS-1）。
# 呼び出し元は Makefile の check-render-isolation ターゲット（薄いラッパー）と
# .github/workflows/ci.yml の render-isolation ジョブ（3 OS matrix）で、判定
# ロジックの二重管理を避けるためロジックの正本はこのファイルに一本化する。
#
# 検査は 2 本立て:
#   A. workspace 全体（render crate 自身を --exclude して走査の根から外す）
#   B. fandhe-browser-cli の既定 feature（TASK-41.5・#174 で crate 追加予定。
#      未追加の間は「検査対象の依存グラフ自体が無い」ため skip する）
#
# `cargo tree` は既定でホストのターゲットに絞って依存グラフを表示するため、
# cfg(windows)/cfg(unix) の target 依存は当該 OS のランナーでしか現れない。
# よって本スクリプトは 3 OS（ubuntu/macos/windows）それぞれで実行する必要がある
# （.claude/rules/ci.md の 3 OS 必須ルール）。
#
# これはクレート名文字列に基づく簡易検出であり、リネームや再エクスポート経由の
# 混入までは捕捉できない（servo_arc・selectors 等の周辺クレート名の誤検出可能性
# を含め、パターンの拡張は本スクリプトのスコープ外）。最終的な防御線は
# deny.toml（[licenses] に Servo（MPL-2.0）が許可されていないことを検出して
# fail-closed する cargo deny check licenses）である。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 検出パターンの正本（外部入力から組み立てない固定値）。Makefile 側の
# RENDER_ISOLATION_PATTERN は本スクリプトへ委譲し二重管理をやめる。
PATTERN='servo|fandhe-browser-render'

# GitHub Actions 上では ::notice:: 形式で skip 理由を出す（ローカルではそのまま
# echo する）。黙って通過させないための証跡。
notice() {
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    echo "::notice::$1"
  else
    echo "$1"
  fi
}

# $1=検査ラベル, $2=cargo tree の標準出力
# 一致があれば NG メッセージと一致行を標準エラーへ出して 1 を返す
# （`| grep -q .` で挟むことで set -e 下でも「一致なし」を異常終了させない）。
detect() {
  local label="$1" out="$2"
  if printf '%s\n' "$out" | grep -Ei "$PATTERN" | grep -q .; then
    echo "NG: ${label} の依存グラフに Servo 系クレートが含まれています" >&2
    printf '%s\n' "$out" | grep -Ei "$PATTERN" >&2
    return 1
  fi
  echo "OK: ${label} の依存グラフに Servo 系クレートは含まれていません"
  return 0
}

# 自己テスト: 合成した cargo tree 出力に対して detect() の判定だけを確認する
# （cargo を一切呼ばない。compat-regression の self-test.sh と同じく、品質ゲート
# が無言で素通りしないことの証跡を CI ログへ残す目的）。
self_test() {
  local failures=0

  if detect "self-test(servo混入)" "$(printf '├── servo v0.0.1\n└── tokio v1.53.1')" >/dev/null 2>&1; then
    echo "NG(self-test): servo 混入行を検出できませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): servo 混入行を正しく検出しました"
  fi

  if detect "self-test(render crate 混入)" "$(printf 'fandhe-browser-render v0.1.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): fandhe-browser-render 混入行を検出できませんでした" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): fandhe-browser-render 混入行を正しく検出しました"
  fi

  if ! detect "self-test(無関係な行のみ)" "$(printf 'tokio v1.53.1\nserde v1.0.0\n')" >/dev/null 2>&1; then
    echo "NG(self-test): 無関係な行だけなのに誤検出しました" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 無関係な行だけの場合は誤検出しませんでした"
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

# CI（GITHUB_ACTIONS=true）ではコミット済み Cargo.lock との食い違いを fail させ、
# ロックの改ざん・未更新を検出する（完全性の検証）。ローカル実行（make 経由）は
# 既存挙動に合わせて --locked を付けない。
CARGO_TREE_LOCKED_ARGS=()
if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
  CARGO_TREE_LOCKED_ARGS=(--locked)
fi

STATUS=0

# 検査 A: workspace 全体（render crate 自身を除く）
WORKSPACE_MEMBERS="$(find crates -mindepth 2 -maxdepth 2 -name 'Cargo.toml' -not -path 'crates/fandhe-browser-render/Cargo.toml' 2>/dev/null || true)"
if [ -z "$WORKSPACE_MEMBERS" ]; then
  notice "skip: fandhe-browser-render 以外の member crate が無いため workspace 検査をスキップ"
else
  if ! OUT_A=$(cargo tree --workspace -e normal,build,dev --exclude fandhe-browser-render "${CARGO_TREE_LOCKED_ARGS[@]}" 2>/dev/null); then
    echo "NG: cargo tree（workspace）の実行に失敗しました" >&2
    STATUS=1
  else
    detect "workspace（既定ビルド）" "$OUT_A" || STATUS=1
  fi
fi

# 検査 B: fandhe-browser-cli の既定 feature（TASK-41.5・#174 で crate 追加予定）
if [ ! -f crates/fandhe-browser-cli/Cargo.toml ]; then
  notice "skip: crates/fandhe-browser-cli/Cargo.toml が未追加のため cli 既定 feature 検査をスキップ（#174 完了後に自動で有効化）"
else
  if ! OUT_B=$(cargo tree -p fandhe-browser-cli -e normal,build,dev "${CARGO_TREE_LOCKED_ARGS[@]}" 2>/dev/null); then
    echo "NG: cargo tree（fandhe-browser-cli）の実行に失敗しました" >&2
    STATUS=1
  else
    detect "fandhe-browser-cli（既定 feature）" "$OUT_B" || STATUS=1
  fi
fi

exit "$STATUS"
