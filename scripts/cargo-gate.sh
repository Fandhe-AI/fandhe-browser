#!/usr/bin/env bash
# Cargo 系ローカルゲート。Cargo.toml / member crate が未整備の段階では誤って落とさず skip する
# （workspace 作成後に自動で有効化される冪等セルフヒール）。
# 使い方: scripts/cargo-gate.sh <gate>
#   check-workspace-manifest  cargo verify-project（member の有無に依らず Cargo.toml のみで判定）
#   fmt | fmt-check | lint | test                 既定 feature の整形・clippy・test
#   lint-rendering | test-rendering               --features rendering（Servo 込み。feature 未定義の間は skip）
#   check-render-isolation | check-js-engine-isolation   依存グラフ分離検証（正本は各 check-*-isolation.sh）
#   check-publish-private     全 member crate が publish = false であること
#
# member crate が 0 件の仮想 workspace では fmt / clippy / test / tree が「対象パッケージが無い」
# エラーで落ちる（cargo の仕様）。一方 Cargo.toml 自体の妥当性は member に依存せず検証できるため
# check-workspace-manifest だけは has_cargo のみで判定する。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

[ "$#" -eq 1 ] || { echo "usage: $(basename "$0") <gate>" >&2; exit 2; }
gate="$1"

# need_members <gate>: Cargo.toml と member crate が揃っていなければ skip。
need_members() {
  if ! has_cargo || ! has_members; then
    skip "no Cargo.toml or no member crate in the workspace; ${gate} skipped"
  fi
}

# cargo_metadata: `cargo metadata --no-deps` の出力を返す。失敗は feature 未定義と区別して fail-closed。
cargo_metadata() {
  local meta
  meta="$(cargo metadata --no-deps --format-version 1 2>&1)" || {
    echo "${meta}" >&2
    echo "error: cargo metadata failed" >&2
    exit 1
  }
  printf '%s\n' "${meta}"
}

# rendering feature は fandhe-browser-render crate 追加まで workspace に存在しない。
# 未定義の間は skip（HAS_CARGO 判定と同じ「未整備段階は skip」方針）。
rendering_gate() {
  local sub="$1" meta
  need_members
  meta="$(cargo_metadata)"
  if ! printf '%s\n' "${meta}" | grep -q '"rendering":'; then
    skip "the rendering feature is not defined; ${gate} skipped"
  fi
  if [ "${sub}" = lint ]; then
    cargo clippy --workspace --all-targets --features rendering -- -D warnings
  else
    cargo test --workspace --features rendering
  fi
}

case "${gate}" in
  check-workspace-manifest)
    has_cargo || skip "no Cargo.toml; ${gate} skipped"
    out="$(cargo verify-project 2>&1)" || {
      echo "${out}" >&2
      echo "error: Cargo.toml is not a valid manifest for cargo" >&2
      exit 1
    }
    if ! printf '%s\n' "${out}" | grep -q '"success"'; then
      echo "${out}" >&2
      echo "error: cargo verify-project did not report success" >&2
      exit 1
    fi
    ;;
  fmt) need_members; cargo fmt --all ;;
  fmt-check) need_members; cargo fmt --all --check ;;
  # 既定 feature のみで検証する（RENDER-1・licensing.md）。Servo（rendering feature）は既定ビルドの
  # 依存グラフへ混入させない方針のため `--all-features` は付けない。
  lint) need_members; cargo clippy --workspace --all-targets -- -D warnings ;;
  test) need_members; cargo test --workspace ;;
  lint-rendering) rendering_gate lint ;;
  test-rendering) rendering_gate test ;;
  check-render-isolation)
    # render crate 自身を除いた member が無い間は検査対象が無いため skip。判定ロジックの正本は
    # scripts/check-render-isolation.sh（CI の harness-* ジョブも直接呼ぶ）。
    has_cargo || skip "no Cargo.toml; ${gate} skipped"
    others=0
    for f in crates/*/Cargo.toml; do
      [ -f "$f" ] && [ "$f" != crates/fandhe-browser-render/Cargo.toml ] && others=1
    done
    [ "${others}" -eq 1 ] || skip "no member crate other than fandhe-browser-render; ${gate} skipped"
    bash scripts/check-render-isolation.sh
    ;;
  check-js-engine-isolation)
    has_cargo && [ -f crates/fandhe-browser-cli/Cargo.toml ] \
      || skip "no Cargo.toml or no fandhe-browser-cli; ${gate} skipped"
    bash scripts/check-js-engine-isolation.sh
    ;;
  check-publish-private)
    # Cargo の既定は publish = true（公開可能）で member が明記しない限り継承されないため、
    # コメントでの申し送りに頼らず機械的に検証する。`cargo metadata` では公開可能な crate は
    # `"publish": null`、非公開（false / 空リスト）は `"publish": []` になる。
    need_members
    require_cmd jq "required by ${gate}"
    meta="$(cargo_metadata)"
    bad="$(printf '%s\n' "${meta}" | jq -r '.packages[] | select(.publish != []) | .name')" || {
      echo "error: jq failed to evaluate publish settings" >&2
      exit 1
    }
    if [ -n "${bad}" ]; then
      echo "error: these crates do not set publish = false (or publish.workspace = true):" >&2
      printf '%s\n' "${bad}" >&2
      exit 1
    fi
    ;;
  *)
    echo "error: unknown gate: ${gate}" >&2
    exit 2
    ;;
esac
