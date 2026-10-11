#!/usr/bin/env bash
# 配布バイナリへ静的リンクされる Cargo 依存（通常依存・推移依存）の著作権表示・許諾文を
# 1 ファイルへ連結して出力する（OSS-2・TASK-76・TASK-64.1）。
# docker/Dockerfile.release の builder ステージから、cargo build の直後（レジストリに
# ソースが展開済みの状態）に呼ばれる。新規の Cargo 依存は追加せず、cargo metadata /
# cargo tree / jq（ビルド専用ツール）だけで生成する。
#
# 使い方: scripts/gen-cargo-licenses.sh <出力ファイル> <ターゲット三つ組> [cargo-feature-flags]
#   cargo-feature-flags は cargo に渡す feature フラグ文字列で、空なら既定 feature。
#   許可するのは `--no-default-features` と `--features <名前[,名前]>` だけ
#   （例: "--no-default-features --features js-boa" は軽量コンテナ向け。それ以外は exit 2）。
#   TASK-64.2 で docker/release-build.sh（build-default / build-lite 両ステージ）から呼ぶ形になった。
#   ワークスペース内の crate（source なし）は本体の
#   LICENSE-MIT / LICENSE-APACHE でカバーされるため対象外。
# ライセンスファイルを持たない crate は docker/license-supplements/ に同梱した上流正本の
# 文書（タグ付きリビジョンから取得したもの）で補う。補う文書も無い crate が 1 件でもあれば
# 失敗にする（fail-closed。著作権表示・許諾文を欠いたイメージを生成しない）。
set -euo pipefail

out="${1:?usage: gen-cargo-licenses.sh <out> <target> [cargo-feature-flags]}"
target="${2:?target triple required}"
flags="${3:-}"

# feature フラグ文字列を許可リストで検証して配列へ入れる（未クォート展開による引数注入を防ぐ）。
feat_args=()
read -r -a tokens <<<"${flags}"
i=0
while [ "${i}" -lt "${#tokens[@]}" ]; do
  case "${tokens[$i]}" in
    --no-default-features) feat_args+=(--no-default-features) ;;
    --features)
      i=$((i + 1))
      val="${tokens[$i]:-}"
      if ! [[ "${val}" =~ ^[A-Za-z0-9_-]+(,[A-Za-z0-9_-]+)*$ ]]; then
        echo "error: invalid feature list: '${val}'" >&2
        exit 2
      fi
      feat_args+=(--features "${val}")
      ;;
    *)
      echo "error: unsupported cargo flag: '${tokens[$i]}'" >&2
      exit 2
      ;;
  esac
  i=$((i + 1))
done

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

# 通常依存（build / dev を除く）の "name vX.Y.Z" 一覧。
cargo tree --locked -p fandhe-browser-cli --edges normal --target "${target}" \
  ${feat_args[@]+"${feat_args[@]}"} --prefix none --format '{p}' |
  awk 'NF >= 2 {print $1 " " $2}' | sort -u >"${tmp}/used.txt"

# 外部 crate（source あり）の name / version / license / manifest_path。
# 軽量ビルドの boa 系は既定 feature の metadata に現れないため --all-features で全 crate を列挙し、used.txt で絞る。
cargo metadata --format-version 1 --locked --all-features |
  jq -r '.packages[] | select(.source != null)
         | [.name, "v" + .version, (.license // "UNKNOWN"), .manifest_path] | @tsv' \
    >"${tmp}/meta.tsv"

tab="$(printf '\t')"
# リポジトリルート（本スクリプトの 1 階層上）。補完文書の置き場の解決に使う。
root="$(cd "$(dirname "$0")/.." && pwd)"
supp_root="${root}/docker/license-supplements"
missing=0
count=0
nofile=0
{
  echo "Cargo 依存の帰属表示 (fandhe-browser-cli のビルドに静的リンクされる通常依存・推移依存)"
  echo "ターゲット: ${target}"
  echo "ビルド構成: ${flags:-default}"
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
      # fandhe-backend-* は同一リポジトリ（同一タグ）の crate 群で補完文書を共有する。
      key="${name}-${ver#v}"
      case "${name}" in fandhe-backend-*) key="fandhe-backend-${ver#v}" ;; esac
      sdir="${supp_root}/${key}"
      sfound=0
      for f in "${sdir}"/*; do
        if [ -f "${f}" ]; then
          sfound=1
          echo "--- $(basename "${f}") (上流リポジトリの該当タグから取得した補完文書) ---"
          cat "${f}"
          echo
        fi
      done
      if [ "${sfound}" = "0" ]; then
        missing=$((missing + 1))
        echo "error: ${name} ${ver}: no license file in package and no supplement in docker/license-supplements/${key}" >&2
      fi
    fi
  done <"${tmp}/meta.tsv"
} >"${out}"

# ライセンス文書を用意できない依存があれば失敗にする（fail-closed）。
if [ "${missing}" -ne 0 ]; then
  echo "error: ${missing} crate(s) without license documents" >&2
  exit 1
fi
# 外部 crate が 1 件も突合できなければ失敗にする（fail-closed）。
if [ "${count}" -eq 0 ]; then
  echo "error: no external crates matched" >&2
  exit 1
fi
echo "gen-cargo-licenses: ${count} external crates (${nofile} supplemented from docker/license-supplements)" >&2
