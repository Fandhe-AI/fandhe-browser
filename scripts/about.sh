#!/usr/bin/env bash
# cargo about generate による全依存ツリーの「解決後ライセンス一覧」生成（TASK-78.2・OSS-4）。
# cargo-deny（scripts/deny.sh。許可外ライセンスの合否判定）を補う棚卸しの一次資料で、後続の
# docs/design/dependency-license-inventory.md（TASK-78.4）が記録に使う。配布用の許諾文全文を作る
# scripts/gen-cargo-licenses.sh（OSS-2）とは目的が異なる。
# 使い方: scripts/about.sh [servo-host]
#   引数なし: 本体 workspace と Servo ホスト（servo-host/。別 workspace）の両方を処理する
#   servo-host: ホストのみ処理する（それ以外の引数は exit 2）
# 出力: ${LICENSE_INVENTORY_DIR:-_/license-inventory}/{main,servo-host}.json と *-summary.md（gitignore 済み）
# 判定（fail-closed）: 解決後ライセンスに GPL 系（GPL/LGPL/AGPL）が 1 件でもあれば exit 1。外部 crate 0 件も exit 1。
# 注意:
#   - 生の SPDX 式には LGPL 等を OR で含む crate があるが、cargo-about は accepted から選ぶため解決後は
#     permissive になる。判定は生の式ではなく解決後の ID で行う。
#   - [private] ignore = true が必須（about.toml 参照。cargo-about 0.9.2 の非 ASCII manifest での panic 回避）。
#   - --offline のため LICENSE 同梱の無い crate は既定の許諾文で補われる。本一覧は種別の棚卸しが目的で、
#     許諾文の完全性は gen-cargo-licenses.sh の責務。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

mode="all"
case "$#:${1:-}" in
  0:) ;;
  1:servo-host) mode="servo-host" ;;
  *)
    echo "usage: $(basename "$0") [servo-host]" >&2
    exit 2
    ;;
esac

ensure_cargo_about() {
  export PATH="${HOME}/.cargo/bin:${PATH}"
  if [ "$(cargo about --version 2>/dev/null || true)" != "cargo-about ${CARGO_ABOUT_VERSION}" ]; then
    echo "installing cargo-about ${CARGO_ABOUT_VERSION}"
    cargo install "cargo-about@${CARGO_ABOUT_VERSION}" --locked --force
  fi
  if [ "$(cargo about --version 2>/dev/null || true)" != "cargo-about ${CARGO_ABOUT_VERSION}" ]; then
    echo "error: cargo-about ${CARGO_ABOUT_VERSION} is not available" >&2
    return 1
  fi
}

# inventory <label> <config> <manifest-or-empty> <all-features:0|1>
inventory() {
  local label="$1" config="$2" manifest="$3" allfeat="$4"
  local outdir="${LICENSE_INVENTORY_DIR:-_/license-inventory}"
  local json="${outdir}/${label}.json" summary="${outdir}/${label}-summary.md"
  mkdir -p "${outdir}"

  local -a mflag=() fflag=()
  [ -n "${manifest}" ] && mflag=(--manifest-path "${manifest}")
  [ "${allfeat}" = "1" ] && fflag=(--all-features)

  echo "== cargo about (${label})"
  cargo fetch --locked "${mflag[@]+"${mflag[@]}"}"
  cargo about generate --format json --workspace --locked --offline --fail \
    --config "${config}" "${mflag[@]+"${mflag[@]}"}" "${fflag[@]+"${fflag[@]}"}" -o "${json}"

  local features="default"
  [ "${allfeat}" = "1" ] && features="all-features"
  local targets
  targets="$(sed -n '/^targets *= *\[/,/^\]/p' "${config}" | grep -o '"[^"]*"' | tr -d '"' | paste -sd, -)"

  jq -r \
    --arg ver "${CARGO_ABOUT_VERSION}" --arg label "${label}" --arg features "${features}" \
    --arg targets "${targets}" --arg manifest "${manifest:-Cargo.toml}" --arg date "$(date -u +%FT%TZ)" '
    ([.licenses[] | . as $l | .used_by[] | {key: (.crate.name + " " + .crate.version), id: $l.id}]
      | group_by(.key) | map({key: .[0].key, ids: (map(.id) | unique | join(", "))})
      | map({(.key): .ids}) | add // {}) as $res
    | "# License inventory (\($label))\n",
      "- generated: \($date)",
      "- cargo-about: \($ver)",
      "- manifest: \($manifest)",
      "- features: \($features)",
      "- targets: \($targets)",
      "- external crates: \(.crates | length)\n",
      "## Resolved licenses\n",
      "| license | crates |", "| --- | --- |",
      (.overview[] | "| \(.name) (\(.id)) | \(.count) |"),
      "\n## Crates\n",
      "| crate | version | declared (SPDX) | resolved |", "| --- | --- | --- | --- |",
      (.crates | sort_by(.package.name) | .[]
        | "| \(.package.name) | \(.package.version) | \(.package.license // "-") | \($res["\(.package.name) \(.package.version)"] // "-") |"),
      "\n## Declared expression includes GPL family (informational; resolved license is what counts)\n",
      ([.crates[] | select((.package.license // "") | test("GPL")) | "- \(.package.name) \(.package.version): \(.package.license)"] | if length == 0 then ["- none"] else . end | .[])
    ' "${json}" >"${summary}"

  local n bad
  n="$(jq '.crates | length' "${json}")"
  if [ "${n}" -eq 0 ]; then
    echo "error: license-inventory(${label}): no external crates found" >&2
    return 1
  fi
  bad="$(jq -r '[(.overview[].id), (.licenses[].id)] | unique | map(select(test("GPL"))) | join(", ")' "${json}")"
  if [ -n "${bad}" ]; then
    echo "error: license-inventory(${label}): GPL-family resolved license(s): ${bad}" >&2
    jq -r '.licenses[] | select(.id | test("GPL")) | .used_by[] | "  \(.crate.name) \(.crate.version)"' "${json}" >&2
    return 1
  fi
  echo "license-inventory(${label}): ${n} external crates, GPL-family licenses: 0 (${summary})"
}

main_inventory() {
  if ! has_cargo || [ ! -f about.toml ] || ! has_members; then
    echo "skip: Cargo.toml, about.toml or a member crate is missing; main inventory skipped"
    return 0
  fi
  inventory main about.toml "" 1
}

servo_host_inventory() {
  if [ ! -f servo-host/Cargo.toml ]; then
    echo "skip: servo-host/Cargo.toml is missing; servo-host inventory skipped"
    return 0
  fi
  if [ ! -f servo-host/about.toml ]; then
    echo "error: servo-host/Cargo.toml exists but servo-host/about.toml is missing" >&2
    return 1
  fi
  inventory servo-host servo-host/about.toml servo-host/Cargo.toml 0
}

require_cmd jq "needed to post-process cargo-about output"
ensure_cargo_about
if [ "${mode}" = "all" ]; then
  main_inventory
fi
servo_host_inventory
