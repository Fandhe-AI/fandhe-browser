#!/usr/bin/env bash
#
# cache 保存前に workspace メンバー（fandhe-browser-* 等）のビルド成果物を target/ から
# 取り除き、依存 crate の成果物だけを cache に残すためのスクリプト（.github/workflows/ci.yml
# の rust-test-* / rust-features-* / harness-* が cache 保存直前に呼ぶ）。
# 変更頻度の高い workspace 成果物を cache に入れると blob が肥大するため。
# 実装は Fandhe-AI/actions の rust-base-ci.yml（@latest）の同名 prune ステップを、
# ci.yml へインライン化する際にスクリプトとして切り出したもの（ロジックは同一）。
# 削除対象は target/ 配下で、symlink・想定外の名前は fail-closed で中止する。
# harness-* の binary-size が生成する release プロファイルも debug と同様に扱う
# （元の rust-base-ci.yml は debug のみ。ci.yml 側の要件で release を追加）。
# 要 jq・cargo（ジョブ側で導入済みの前提）。
set -euo pipefail

if ! command -v jq >/dev/null 2>&1; then
  echo "::error::jq not found (required by the cache prune step)"
  exit 1
fi

workspace="$(pwd -P)"
target_dir="${workspace}/target"
if [ ! -d "${target_dir}" ]; then
  echo "target does not exist; skipping prune"
  exit 0
fi
if [ -L "${target_dir}" ]; then
  echo "::error::target is a symlink; aborting prune"
  exit 1
fi

# メンバー package 名 + 全 target 名（lib/bin/test/bench/example）。
# --no-deps は依存解決・ネットワーク不要（workspace 内のみを見る）。
raw=()
while IFS= read -r n; do
  [ -n "${n}" ] && raw+=("${n}")
done < <(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | .name, (.targets[] | select(.kind | index("custom-build") | not) | .name)' \
  | sort -u)
if [ "${#raw[@]}" -eq 0 ]; then
  echo "::error::failed to read workspace members from cargo metadata"
  exit 1
fi

# Cargo のパッケージ名は非ASCIIの Unicode 英数字（+ `_`・`-`）も許容する
# （crates.io 公開時の ASCII 限定は別レイヤーの制約。cargo リファレンス:
# https://doc.rust-lang.org/cargo/reference/manifest.html#the-name-field）。
# 許可文字は正規表現メタ文字を含まないため、後段で alt_pkgs / alt_names として
# 無エスケープのまま正規表現へ埋め込んでも安全（#133 レビュー指摘対応）。
pkgs=(); names=()
for n in "${raw[@]}"; do
  if ! jq -n --arg n "${n}" '$n | test("^[\\p{L}\\p{N}_-]+$")' | grep -q '^true$'; then
    echo "::error::unexpected crate / target name detected; aborting prune"
    echo "name: ${n}"
    exit 1
  fi
  pkgs+=("${n}")
  names+=("${n}" "${n//-/_}")
done
mapfile_pkgs=(); while IFS= read -r n; do mapfile_pkgs+=("${n}"); done < <(printf '%s\n' "${pkgs[@]}" | sort -u)
pkgs=("${mapfile_pkgs[@]}")
mapfile_names=(); while IFS= read -r n; do mapfile_names+=("${n}"); done < <(printf '%s\n' "${names[@]}" | sort -u)
names=("${mapfile_names[@]}")

alt_pkgs="$(IFS='|'; printf '%s' "${pkgs[*]}")"
alt_names="$(IFS='|'; printf '%s' "${names[*]}")"
re_unit="^(${alt_pkgs})-[0-9a-f]{16}$"                 # .fingerprint/ build/（package 名・ハイフンのまま）
re_dep="^(lib)?(${alt_names})-[0-9a-f]{16}(\..+)?$"    # deps/（target 名・`_` 正規化）
re_top="^(lib)?(${alt_names})(\.(d|rlib|rmeta|so|a|dylib))?$"  # <profile> 直下の uplift バイナリ（lib prefix・rlib 等の拡張子込み）

removed=0
safe_rm() {  # $1=削除対象パス $2=許可される親ディレクトリ（境界チェック用）
  local p="$1" base="$2"
  case "${p}" in
    "${base}"/*) ;;
    *)
      echo "::error::deletion target is outside the expected directory"
      echo "${p}"
      exit 1
      ;;
  esac
  if [ -L "${p}" ]; then
    echo "::error::refusing to delete a symlink"
    echo "${p}"
    exit 1
  fi
  rm -rf -- "${p}"
  removed=$((removed + 1))
}
prune_entries() {  # $1=dir $2=regex $3=base（境界チェック用）
  local dir="$1" re="$2" base="$3" e b
  [ -d "${dir}" ] || return 0
  while IFS= read -r -d '' e; do
    b="${e##*/}"
    if [[ "${b}" =~ ${re} ]]; then safe_rm "${e}" "${base}"; fi
  done < <(find "${dir}" -mindepth 1 -maxdepth 1 -print0)
}
prune_profile() {  # $1=profile dir（target/debug または target/<triple>/debug）
  local profile="$1"
  [ -d "${profile}" ] || return 0
  if [ -L "${profile}" ]; then
    echo "::error::${profile} is a symlink; aborting prune"
    exit 1
  fi
  for d in examples incremental; do
    [ -e "${profile}/${d}" ] && safe_rm "${profile}/${d}" "${profile}"
  done
  prune_entries "${profile}/.fingerprint" "${re_unit}" "${profile}"
  prune_entries "${profile}/build" "${re_unit}" "${profile}"
  prune_entries "${profile}/deps" "${re_dep}" "${profile}"
  while IFS= read -r -d '' e; do
    b="${e##*/}"
    if [[ "${b}" =~ ${re_top} ]] || [[ "${b}" == *.d ]]; then safe_rm "${e}" "${profile}"; fi
  done < <(find "${profile}" -mindepth 1 -maxdepth 1 -type f -print0)
}

# 既定の target/debug に加え、.cargo/config.toml の build.target 指定時に
# 生成される target/<triple>/debug（および release） のメンバー成果物も prune 対象に含める
# （#133 レビュー指摘対応。target/debug 固定だとターゲット別ディレクトリの
# 成果物が縮小対象から漏れ、キャッシュに残ったままになる）。
profiles=("${target_dir}/debug" "${target_dir}/release")
while IFS= read -r -d '' d; do
  profiles+=("${d}")
done < <(find "${target_dir}" -mindepth 2 -maxdepth 2 -type d \( -name debug -o -name release \) -print0 2>/dev/null)

any_profile=0
for profile in "${profiles[@]}"; do
  if [ -d "${profile}" ]; then
    any_profile=1
    prune_profile "${profile}"
  fi
done
if [ "${any_profile}" -eq 0 ]; then
  echo "target/debug and target/release do not exist; skipping prune"
  exit 0
fi

echo "prune done: removed ${removed} entries"
echo "packages: ${pkgs[*]}"
