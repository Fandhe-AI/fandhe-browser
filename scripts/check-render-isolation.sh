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
#   B. fandhe-browser-cli の既定 feature（TASK-41.5・#174 で crate 追加済み。
#      cli の manifest が無い場合は NG とする。fail-closed・REPAIR-5・#633）
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
# 一致があれば NG メッセージと一致行を標準エラーへ出して 1 を返す。
#
# パイプ（`printf ... | grep ... | grep -q .`）は使わない。`set -o pipefail` 下では
# パイプの終了ステータスが「右端の非ゼロ終了コマンド」になる仕様のため、一致行数が
# パイプバッファ（約 64KB）を超えると後段の `grep -q .` が最初の 1 致で早期終了して
# パイプを閉じ、前段の `grep -Ei` が SIGPIPE (141) を受ける。このとき `grep -q .`
# 自体は「一致あり」で正常終了（0）するにもかかわらず、パイプ全体の終了ステータスは
# 141 になり、`if` 条件が偽と評価されて「一致なし（OK）」に誤判定される
# （Servo が大量に混入するほど検出をすり抜ける fail-open。Issue #465 レビュー指摘）。
# ヒアストリング（`<<<`）は追加のプロセスをパイプで挟まないため、この問題が起きない。
detect() {
  local label="$1" out="$2"
  if grep -Eqi -- "$PATTERN" <<<"$out"; then
    echo "NG: ${label} の依存グラフに Servo 系クレートが含まれています" >&2
    grep -Ei -- "$PATTERN" <<<"$out" >&2
    return 1
  fi
  echo "OK: ${label} の依存グラフに Servo 系クレートは含まれていません"
  return 0
}

# 検査 B の前提確認: cli の manifest が無ければ NG を標準エラーへ出して 1 を返す。
# cli の削除・リネーム・manifest 破損で検査 B が「未導入のため skip」として
# 黙って通過する fail-open を防ぐ（REPAIR-5・RENDER-1・#633）。
# $1 = cli manifest のパス。検査 B 本体と self_test() から呼ばれる。
require_cli_manifest() {
  local manifest="$1"
  if [ ! -f "$manifest" ]; then
    echo "NG: ${manifest} が見つからないため fandhe-browser-cli（既定 feature）の検査を実行できません" >&2
    return 1
  fi
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

  # 大量一致時の fail-open 回帰テスト（Issue #465 レビュー指摘）。
  # パイプバッファ（約 64KB）を超える一致行数を再現するため、bash 組み込みの
  # for ループで合成する（`yes | head` 等の外部コマンドをパイプで挟むと同じ
  # SIGPIPE 経路を踏みかねないため使わない）。
  local big_servo="" big_clean="" i
  for ((i = 0; i < 3000; i++)); do
    big_servo+="├── servo_dep_${i} v0.0.1 (crates/some/long/synthetic/path/for/buffer)"$'\n'
  done
  for ((i = 0; i < 3000; i++)); do
    big_clean+="├── tokio_dep_${i} v1.0.0 (crates/some/long/synthetic/path/for/buffer)"$'\n'
  done

  if detect "self-test(大量servo混入)" "$big_servo" >/dev/null 2>&1; then
    echo "NG(self-test): 大量の servo 混入行を検出できませんでした（fail-open 回帰）" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 大量の servo 混入行を正しく検出しました"
  fi

  if ! detect "self-test(大量の無関係な行)" "$big_clean" >/dev/null 2>&1; then
    echo "NG(self-test): 大量の無関係な行だけなのに誤検出しました" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 大量の無関係な行だけの場合は誤検出しませんでした"
  fi

  # cli manifest の不在は NG（fail-closed・#633）。存在側は自スクリプトで代用し、
  # 実リポの cli の有無に依存させない。
  local err_missing
  if err_missing=$(require_cli_manifest "crates/__does_not_exist__/Cargo.toml" 2>&1 >/dev/null); then
    echo "NG(self-test): cli manifest 不在を NG にできませんでした（fail-open 回帰）" >&2
    failures=$((failures + 1))
  elif [[ "$err_missing" != *"が見つからないため fandhe-browser-cli"* ]]; then
    echo "NG(self-test): cli manifest 不在の NG メッセージが想定と異なります: ${err_missing}" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): cli manifest 不在を正しく NG にしました"
  fi

  if ! require_cli_manifest "${BASH_SOURCE[0]}" >/dev/null 2>&1; then
    echo "NG(self-test): 存在する manifest を誤って NG にしました" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): 存在する manifest は NG にしませんでした"
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
# 列挙の失敗（find の非ゼロ終了）を「member 無し」と混同しない。失敗を握りつぶすと
# 検査 A が無言で skip されて分離ゲートが素通りする（fail-open。Issue #465 レビュー指摘）。
# crates ディレクトリ自体が無い場合は正常な「member 無し」として空扱いにし、
# それ以外の find 失敗は非ゼロ終了する。
WORKSPACE_MEMBERS=""
if [ -d crates ]; then
  if ! WORKSPACE_MEMBERS="$(find crates -mindepth 2 -maxdepth 2 -name 'Cargo.toml' -not -path 'crates/fandhe-browser-render/Cargo.toml')"; then
    echo "NG: workspace member の列挙（find crates）に失敗しました" >&2
    exit 1
  fi
fi
if [ -z "$WORKSPACE_MEMBERS" ]; then
  notice "skip: fandhe-browser-render 以外の member crate が無いため workspace 検査をスキップ"
else
  # 標準出力だけを判定対象にし、標準エラーはそのまま通す（2>&1 で合流させない）。
  # fandhe-browser-render が未作成の段階では「excluded package(s) ... not found」
  # の警告が標準エラーへ出るのみで終了コードは 0・標準出力には現れないため
  # （Makefile 旧実装からの継承挙動）、素通しすることで cargo tree 自体の失敗
  # 原因（--locked のロック不整合等）もログから読み取れるようにする。
  if ! OUT_A=$(cargo tree --workspace -e normal,build,dev --exclude fandhe-browser-render ${CARGO_TREE_LOCKED_ARGS[@]+"${CARGO_TREE_LOCKED_ARGS[@]}"}); then
    echo "NG: cargo tree（workspace）の実行に失敗しました" >&2
    STATUS=1
  else
    detect "workspace（既定ビルド）" "$OUT_A" || STATUS=1
  fi
fi

# 検査 B: fandhe-browser-cli の既定 feature（TASK-41.5・#174）。manifest 不在は NG
if ! require_cli_manifest crates/fandhe-browser-cli/Cargo.toml; then
  STATUS=1
else
  if ! OUT_B=$(cargo tree -p fandhe-browser-cli -e normal,build,dev ${CARGO_TREE_LOCKED_ARGS[@]+"${CARGO_TREE_LOCKED_ARGS[@]}"}); then
    echo "NG: cargo tree（fandhe-browser-cli）の実行に失敗しました" >&2
    STATUS=1
  else
    detect "fandhe-browser-cli（既定 feature）" "$OUT_B" || STATUS=1
  fi
fi

exit "$STATUS"
