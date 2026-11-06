#!/usr/bin/env bash
# crates.io 公開可否の検査（#1531。`make check-publish-ready`・`make ci` に含む）。旧 check-publish-private の置き換え。
#
# 全 member crate が「公開 allowlist の 8 crate」か「publish = false」のどちらかであることを保証する
# （新規 crate が黙って公開可能にならない fail-closed）。公開 crate には次を要求する。
#   - publish が制限されていない（`publish.workspace = true` で `[workspace.package]` の true を継承）
#   - description・repository・license・readme がある（crates.io は description 欠落を拒否する）
#   - path 依存（通常・build・dev すべて）に `=<依存先の version>` の version がある
#   - 公開 crate の version が互いに一致する（共通版 0.1.0 の方針）
# さらに cli の `[profile.release]` がルート `Cargo.toml` と一致すること（`cargo install` は
# ルートの profile を読まないため複製している。panic = "abort" 前提の維持）を検査する。
#
# 使い方: scripts/check-publish-ready.sh [--self-test]
#   引数なし: 実際の workspace を検査する（要 cargo・jq）
#   --self-test: 合成した cargo metadata に対する判定だけを確認する（要 jq。cargo 不要）
# 出力文字列は英語（japanese-style.md）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

# 公開 allowlist と公開順の正本は publish-crates.sh（release.yml・release.sh と共有）。
# shellcheck source=./publish-crates.sh
source "${DIR}/publish-crates.sh"

# metadata_problems <cargo metadata --no-deps の JSON>: 違反を 1 行ずつ標準出力へ出す（無ければ空）。
metadata_problems() {
  local allow_json
  allow_json="$(printf '%s\n' "${PUBLIC_CRATES[@]}" | jq -R . | jq -sc .)"
  printf '%s\n' "$1" | jq -r --argjson allow "${allow_json}" '
    . as $m
    | ($m.packages | map({key: .name, value: .version}) | from_entries) as $ver
    | ($m.packages | map(.name)) as $names
    | (
        ( $m.packages[] as $p
          | if ($allow | index($p.name)) != null then
              ( if $p.publish != null then "\($p.name): publish is restricted (public crates must inherit publish = true)" else empty end,
                if ($p.description // "") == "" then "\($p.name): description is missing" else empty end,
                if ($p.repository // "") == "" then "\($p.name): repository is missing" else empty end,
                if ($p.license // "") == "" then "\($p.name): license is missing" else empty end,
                if ($p.readme // "") == "" then "\($p.name): readme is missing" else empty end,
                ( $p.dependencies[] | select(.path != null)
                  | select(.req != "=" + ($ver[.name] // "?"))
                  | "\($p.name): path dependency \(.name) must carry version =\($ver[.name] // "?") (found \(.req))" )
              )
            else
              ( if $p.publish != [] then "\($p.name): not in the public allowlist; set publish = false" else empty end )
            end ),
        ( $allow[] | select(. as $n | ($names | index($n)) == null) | "\(.): allowlisted crate is not a workspace member" ),
        ( [ $m.packages[] | select(.name as $n | ($allow | index($n)) != null) | .version ] | unique
          | if length > 1 then "public crates do not share one version: \(join(", "))" else empty end )
      )'
}

# release_profile <Cargo.toml>: `[profile.release]` 節をコメント・空白を除いて並べ替えて出力する。
release_profile() {
  awk '/^\[profile\.release\]/ { f = 1; next } /^\[/ { f = 0 } f' "$1" \
    | sed 's/#.*//; s/[[:space:]]//g; /^$/d' | sort
}

# profile_problems <root Cargo.toml> <cli Cargo.toml>: 不一致・欠落を出力する。
profile_problems() {
  local root cli
  root="$(release_profile "$1")"
  cli="$(release_profile "$2")"
  if [ -z "${root}" ]; then
    echo "root Cargo.toml has no [profile.release] section"
  elif [ -z "${cli}" ]; then
    echo "fandhe-browser-cli has no [profile.release] section (cargo install ignores the workspace root profile)"
  elif [ "${root}" != "${cli}" ]; then
    echo "[profile.release] of fandhe-browser-cli differs from the root Cargo.toml"
  fi
}

# workflow_problems <release.yml>: inputs.crate の choice 選択肢が allowlist と一致するか。
# 一覧の転記漏れ（公開できない crate・存在しない crate が選べる状態）を防ぐ。
workflow_problems() {
  local c listed
  [ -f "$1" ] || { echo "$1 does not exist"; return; }
  for c in "${PUBLIC_CRATES[@]}"; do
    grep -Eq "^ +- ${c}$" "$1" || echo "$1: crate choice is missing ${c}"
  done
  listed="$(grep -Ec '^ +- fandhe-browser-[a-z]+$' "$1" || true)"
  if [ "${listed}" != "${#PUBLIC_CRATES[@]}" ]; then
    echo "$1: expected ${#PUBLIC_CRATES[@]} crate choices, found ${listed}"
  fi
}

self_test() {
  require_cmd jq "required by --self-test"
  local fail=0 tmp base out
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT

  # 全 allowlist crate が正しい合成 metadata。private crate 1 件を含む。
  base="$(printf '%s\n' "${PUBLIC_CRATES[@]}" | jq -R '{
      name: ., version: "0.1.0", publish: null, description: "d", repository: "r",
      license: "MIT OR Apache-2.0", readme: "README.md",
      dependencies: [{name: "fandhe-browser-profile", req: "=0.1.0", path: "/p"}, {name: "serde", req: "=1.0.1", path: null}]
    }' | jq -sc '{packages: (. + [{name: "tool", version: "0.1.0", publish: [], dependencies: [{name: "fandhe-browser-core", req: "*", path: "/p"}]}])}')"

  expect() { # $1=ラベル $2=期待する出力の部分一致（空なら出力なしを期待） $3=metadata JSON
    local label="$1" want="$2" got
    got="$(metadata_problems "$3")"
    if [ -z "${want}" ]; then
      if [ -n "${got}" ]; then echo "NG(self-test): ${label}: unexpected: ${got}" >&2; fail=1; else echo "OK(self-test): ${label}"; fi
    elif [[ "${got}" == *"${want}"* ]]; then
      echo "OK(self-test): ${label}"
    else
      echo "NG(self-test): ${label}: expected '${want}', got '${got}'" >&2; fail=1
    fi
  }

  expect "valid" "" "${base}"
  expect "missing description" "fandhe-browser-cli: description is missing" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-cli") | .description) = null')"
  expect "missing repository" "fandhe-browser-js: repository is missing" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-js") | .repository) = ""')"
  expect "missing readme" "fandhe-browser-ai: readme is missing" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-ai") | .readme) = null')"
  expect "publish = false on public crate" "fandhe-browser-mcp: publish is restricted" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-mcp") | .publish) = []')"
  expect "path dep without version" "path dependency fandhe-browser-profile must carry version" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-core") | .dependencies[0].req) = "*"')"
  expect "new crate silently publishable" "fandhe-browser-new: not in the public allowlist" \
    "$(printf '%s' "${base}" | jq -c '.packages += [{name: "fandhe-browser-new", version: "0.1.0", publish: null, dependencies: []}]')"
  expect "allowlisted crate removed" "fandhe-browser-render: allowlisted crate is not a workspace member" \
    "$(printf '%s' "${base}" | jq -c '.packages |= map(select(.name != "fandhe-browser-render"))')"
  expect "version skew" "do not share one version" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-cdp") | .version) = "0.2.0"')"

  # profile 比較
  printf '[profile.release]\n# c\npanic = "abort"\nopt-level = "z"\n\n[profile.dev]\nx = 1\n' >"${tmp}/root.toml"
  printf '[package]\nname = "x"\n[profile.release]\nopt-level = "z"   # c\npanic = "abort"\n' >"${tmp}/same.toml"
  printf '[profile.release]\nopt-level = 3\npanic = "abort"\n' >"${tmp}/diff.toml"
  printf '[package]\nname = "x"\n' >"${tmp}/none.toml"
  out="$(profile_problems "${tmp}/root.toml" "${tmp}/same.toml")"
  if [ -z "${out}" ]; then echo "OK(self-test): profile same"; else echo "NG(self-test): profile same: ${out}" >&2; fail=1; fi
  out="$(profile_problems "${tmp}/root.toml" "${tmp}/diff.toml")"
  if [[ "${out}" == *differs* ]]; then echo "OK(self-test): profile differs"; else echo "NG(self-test): profile differs: '${out}'" >&2; fail=1; fi
  out="$(profile_problems "${tmp}/root.toml" "${tmp}/none.toml")"
  if [[ "${out}" == *"has no [profile.release]"* ]]; then echo "OK(self-test): profile missing"; else echo "NG(self-test): profile missing: '${out}'" >&2; fail=1; fi

  [ "${fail}" -eq 0 ] || { echo "NG: check-publish-ready self-test failed" >&2; exit 1; }
  echo "OK: check-publish-ready self-test passed"
}

main() {
  if [ "$#" -eq 1 ] && [ "$1" = "--self-test" ]; then
    self_test
    return
  fi
  reject_args "check-publish-ready.sh [--self-test]" "$@"
  if ! has_cargo || ! has_members; then skip "no Cargo.toml or no member crate; check-publish-ready skipped"; fi
  require_cmd jq "required by check-publish-ready"
  local meta problems=""
  # JSON は標準出力のみ。cargo の警告（cli の [profile.release] 等）は標準エラーへ出るため混ぜない。
  meta="$(cargo metadata --no-deps --format-version 1)" || {
    echo "error: cargo metadata failed" >&2
    exit 1
  }
  problems="$(metadata_problems "${meta}")" || {
    echo "error: jq failed to evaluate publish settings" >&2
    exit 1
  }
  problems+="$(profile_problems Cargo.toml crates/fandhe-browser-cli/Cargo.toml)"
  problems+="$(workflow_problems .github/workflows/release.yml)"
  if [ -n "${problems}" ]; then
    echo "NG: crates.io publish readiness violations:" >&2
    printf '%s\n' "${problems}" >&2
    exit 1
  fi
  echo "OK: ${#PUBLIC_CRATES[@]} public crates are publish-ready; all other members set publish = false"
}

main "$@"
