#!/usr/bin/env bash
# 配布バイナリへ静的リンクされる Cargo 依存（通常依存・推移依存）の著作権表示・許諾文を
# 1 ファイルへ連結して出力する（OSS-2・TASK-76・TASK-64.1）。
# docker/Dockerfile.release の builder ステージから、cargo build の直後（レジストリに
# ソースが展開済みの状態）に呼ばれる。新規の Cargo 依存は追加せず、cargo metadata /
# cargo tree / jq（ビルド専用ツール）だけで生成する。
#
# 使い方: scripts/gen-cargo-licenses.sh <出力ファイル> <ターゲット三つ組> [features]
#   features が空なら既定 feature。ワークスペース内の crate（source なし）は本体の
#   LICENSE-MIT / LICENSE-APACHE でカバーされるため対象外。
# ライセンスファイルを持たない crate は SPDX 宣言のみを記し、警告を標準エラーへ出す。
set -euo pipefail

out="${1:?usage: gen-cargo-licenses.sh <out> <target> [features]}"
target="${2:?target triple required}"
features="${3:-}"

feat_args=""
if [ -n "${features}" ]; then feat_args="--features ${features}"; fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

# 通常依存（build / dev を除く）の "name vX.Y.Z" 一覧。
# shellcheck disable=SC2086
cargo tree --locked -p fandhe-browser-cli --edges normal --target "${target}" \
  ${feat_args} --prefix none --format '{p}' |
  awk 'NF >= 2 {print $1 " " $2}' | sort -u >"${tmp}/used.txt"

# 外部 crate（source あり）の name / version / license / manifest_path。
cargo metadata --format-version 1 --locked |
  jq -r '.packages[] | select(.source != null)
         | [.name, "v" + .version, (.license // "UNKNOWN"), .manifest_path] | @tsv' \
    >"${tmp}/meta.tsv"

tab="$(printf '\t')"
count=0
nofile=0
{
  echo "Cargo 依存の帰属表示 (fandhe-browser-cli のビルドに静的リンクされる通常依存・推移依存)"
  echo "ターゲット: ${target}"
  echo "生成元: scripts/gen-cargo-licenses.sh (cargo tree / cargo metadata)"
  echo
  while IFS="${tab}" read -r name ver lic manifest; do
    if ! grep -qxF "${name} ${ver}" "${tmp}/used.txt"; then continue; fi
    count=$((count + 1))
    dir="$(dirname "${manifest}")"
    echo "================================================================================"
    echo "${name} ${ver}"
    echo "SPDX: ${lic}"
    echo "================================================================================"
    found=0
    for f in "${dir}"/LICENSE* "${dir}"/LICENCE* "${dir}"/COPYING* "${dir}"/UNLICENSE* "${dir}"/NOTICE*; do
      if [ -f "${f}" ]; then
        found=1
        echo "--- $(basename "${f}") ---"
        cat "${f}"
        echo
      fi
    done
    if [ "${found}" = "0" ]; then
      nofile=$((nofile + 1))
      echo "(ライセンスファイルはパッケージに含まれない。上記 SPDX の宣言のみ)"
      echo "${name} ${ver}: no license file in package (SPDX: ${lic})" >&2
      echo
    fi
  done <"${tmp}/meta.tsv"
} >"${out}"

# 外部 crate が 1 件も突合できなければ失敗にする（fail-closed）。
if [ "${count}" -eq 0 ]; then
  echo "error: no external crates matched" >&2
  exit 1
fi
echo "gen-cargo-licenses: ${count} external crates (${nofile} without license file)" >&2
