#!/usr/bin/env bash
# crates.io 公開可否の検査（#1531。`make check-publish-ready`・`make ci` に含む）。旧 check-publish-private の置き換え。
#
# 全 member crate が「公開 allowlist の 8 crate」か「publish = false」のどちらかであることを保証する
# （新規 crate が黙って公開可能にならない fail-closed）。9 件目の公開対象 Servo ホスト（別 workspace。#1543）は
# member であってはならず、description・repository・license（ルートと同じ）・readme を要求する。公開 crate には次を要求する。
#   - publish が制限されていない（`publish.workspace = true` で `[workspace.package]` の true を継承）
#   - description・repository・license・readme がある（crates.io は description 欠落を拒否する）
#   - path 依存（通常・build・dev すべて）に `=<依存先の version>` の version がある
#   - 公開 crate の version が互いに一致する（共通版の方針）
# また servo-host の version がルートの version と一致すること（#1540。cli の起動前検査が完全一致を要求する）。
# さらに cli と servo-host の `[profile.release]` がルート `Cargo.toml` と一致すること（`cargo install` は
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
  printf '%s\n' "$1" | jq -r --argjson allow "${allow_json}" --arg host "${HOST_CRATE}" '
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
        ( $names[] | select(. == $host) | "\(.): must not be a workspace member (separate workspace; see licensing.md)" ),
        ( [ $m.packages[] | select(.name as $n | ($allow | index($n)) != null) | .version ] | unique
          | if length > 1 then "public crates do not share one version: \(join(", "))" else empty end )
      )'
}

# host_metadata_problems <servo-host の cargo metadata --no-deps の JSON> <ルートの license>:
# Servo ホスト（別 workspace の単一 package）の公開メタデータの違反を 1 行ずつ出力する（無ければ空。#1543）。
# license はオーナー判断でルートと同じ（MIT OR Apache-2.0）。path 依存は別 workspace からは公開できず、
# 本体への依存は隔離（RENDER-1）にも反するため禁止する。
host_metadata_problems() {
  printf '%s\n' "$1" | jq -r --arg host "${HOST_CRATE}" --arg lic "$2" '
    (.packages // []) as $pk
    | if ($pk | length) != 1 or $pk[0].name != $host then
        "\($host): expected exactly one package named \($host) in servo-host/Cargo.toml"
      else
        $pk[0] as $p
        | ( if $p.publish != null then "\($host): publish is restricted" else empty end,
            if ($p.description // "") == "" then "\($host): description is missing" else empty end,
            if ($p.repository // "") == "" then "\($host): repository is missing" else empty end,
            if ($p.license // "") == "" then "\($host): license is missing"
            elif $p.license != $lic then "\($host): license \($p.license) differs from the workspace license \($lic)" else empty end,
            if ($p.readme // "") == "" then "\($host): readme is missing" else empty end,
            ( $p.dependencies[] | select(.path != null)
              | "\($host): path dependency \(.name) is not allowed (separate workspace)" ) )
      end'
}

# host_version_problems <root Cargo.toml> <servo-host Cargo.toml>: Servo ホストの [package] version が
# ルート [workspace.package] の version と一致するか（#1540。cli の起動前検査は両者の完全一致を要求する。
# 別 workspace のため自動では揃わず、ずれるとリリース済みの組でも描画系が全面的に起動拒否になる）。
section_version() { # $1=Cargo.toml $2=節見出し（完全一致）: 節内の最初の version 値
  awk -v sec="$2" '$0 == sec { f = 1; next } /^\[/ { f = 0 } f' "$1" \
    | grep -E '^version[[:space:]]*=' | head -1 | sed -E 's/^[^"]*"([^"]*)".*/\1/' || true
}
# section_value <Cargo.toml> <節見出し> <キー>: 節内の文字列値（license 用）。
section_value() {
  awk -v sec="$2" '$0 == sec { f = 1; next } /^\[/ { f = 0 } f' "$1" \
    | grep -E "^$3[[:space:]]*=" | head -1 | sed -E 's/^[^"]*"([^"]*)".*/\1/' || true
}
host_version_problems() {
  local root host
  root="$(section_version "$1" '[workspace.package]')"
  host="$(section_version "$2" '[package]')"
  if [ -z "${root}" ] || [ -z "${host}" ]; then
    echo "cannot read versions (root='${root}', servo-host='${host}')"
  elif [ "${root}" != "${host}" ]; then
    echo "servo-host version ${host} differs from the workspace version ${root} (the CLI requires an exact match; see docs/design/servo-host-design.md)"
  fi
}

# release_profile <Cargo.toml>: `[profile.release]` 節をコメント・空白を除いて並べ替えて出力する。
release_profile() {
  awk '/^\[profile\.release\]/ { f = 1; next } /^\[/ { f = 0 } f' "$1" \
    | sed 's/#.*//; s/[[:space:]]//g; /^$/d' | sort
}

# profile_problems <root Cargo.toml> <crate の Cargo.toml> <ラベル>: 不一致・欠落を出力する。
profile_problems() {
  local root cli
  root="$(release_profile "$1")"
  cli="$(release_profile "$2")"
  if [ -z "${root}" ]; then
    echo "root Cargo.toml has no [profile.release] section"
  elif [ -z "${cli}" ]; then
    echo "$3 has no [profile.release] section (cargo install ignores the workspace root profile)"
  elif [ "${root}" != "${cli}" ]; then
    echo "[profile.release] of $3 differs from the root Cargo.toml"
  fi
}

# workflow_problems <release.yml>: inputs.crate の choice 選択肢が公開対象（RELEASE_CRATES の 9 件）と一致するか。
# 一覧の転記漏れ（公開できない crate・存在しない crate が選べる状態）を防ぐ。
workflow_problems() {
  local c listed
  [ -f "$1" ] || { echo "$1 does not exist"; return; }
  for c in "${RELEASE_CRATES[@]}"; do
    grep -Eq "^ +- ${c}$" "$1" || echo "$1: crate choice is missing ${c}"
  done
  listed="$(grep -Ec '^ +- fandhe-browser-[a-z-]+$' "$1" || true)"
  if [ "${listed}" != "${#RELEASE_CRATES[@]}" ]; then
    echo "$1: expected ${#RELEASE_CRATES[@]} crate choices, found ${listed}"
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
  expect "host leaked into the workspace" "fandhe-browser-servo-host: must not be a workspace member" \
    "$(printf '%s' "${base}" | jq -c '.packages += [{name: "fandhe-browser-servo-host", version: "0.1.0", publish: [], dependencies: []}]')"
  expect "version skew" "do not share one version" \
    "$(printf '%s' "${base}" | jq -c '(.packages[] | select(.name == "fandhe-browser-cdp") | .version) = "0.2.0"')"

  # profile 比較
  printf '[profile.release]\n# c\npanic = "abort"\nopt-level = "z"\n\n[profile.dev]\nx = 1\n' >"${tmp}/root.toml"
  printf '[package]\nname = "x"\n[profile.release]\nopt-level = "z"   # c\npanic = "abort"\n' >"${tmp}/same.toml"
  printf '[profile.release]\nopt-level = 3\npanic = "abort"\n' >"${tmp}/diff.toml"
  printf '[package]\nname = "x"\n' >"${tmp}/none.toml"
  out="$(profile_problems "${tmp}/root.toml" "${tmp}/same.toml" fandhe-browser-cli)"
  if [ -z "${out}" ]; then echo "OK(self-test): profile same"; else echo "NG(self-test): profile same: ${out}" >&2; fail=1; fi
  out="$(profile_problems "${tmp}/root.toml" "${tmp}/diff.toml" fandhe-browser-servo-host)"
  if [[ "${out}" == *"[profile.release] of fandhe-browser-servo-host differs"* ]]; then echo "OK(self-test): profile differs"; else echo "NG(self-test): profile differs: '${out}'" >&2; fail=1; fi
  out="$(profile_problems "${tmp}/root.toml" "${tmp}/none.toml" fandhe-browser-cli)"
  if [[ "${out}" == *"has no [profile.release]"* ]]; then echo "OK(self-test): profile missing"; else echo "NG(self-test): profile missing: '${out}'" >&2; fail=1; fi

  # servo-host 版の一致（#1540）
  printf '[workspace.package]\nversion = "0.1.0"\n[workspace.dependencies]\nversion = "9"\n' >"${tmp}/rootv.toml"
  printf '[package]\nname = "h"\nversion = "0.1.0"\n' >"${tmp}/hostok.toml"
  printf '[package]\nname = "h"\nversion = "0.2.0"\n' >"${tmp}/hostng.toml"
  out="$(host_version_problems "${tmp}/rootv.toml" "${tmp}/hostok.toml")"
  if [ -z "${out}" ]; then echo "OK(self-test): host version same"; else echo "NG(self-test): host version same: ${out}" >&2; fail=1; fi
  out="$(host_version_problems "${tmp}/rootv.toml" "${tmp}/hostng.toml")"
  if [[ "${out}" == *"differs"* ]]; then echo "OK(self-test): host version differs"; else echo "NG(self-test): host version differs: '${out}'" >&2; fail=1; fi
  out="$(host_version_problems "${tmp}/rootv.toml" "${tmp}/none.toml")"
  if [[ "${out}" == *"cannot read"* ]]; then echo "OK(self-test): host version missing"; else echo "NG(self-test): host version missing: '${out}'" >&2; fail=1; fi

  # servo-host のメタデータ（別 workspace の単一 package）
  local hostmeta lic=$'MIT OR Apache-2.0'
  hostmeta='{"packages":[{"name":"fandhe-browser-servo-host","version":"0.1.0","publish":null,"description":"d","repository":"r","license":"MIT OR Apache-2.0","readme":"README.md","dependencies":[{"name":"servo","req":"=0.7.0","path":null}]}]}'
  hexpect() { # $1=ラベル $2=期待する部分一致（空なら出力なし） $3=metadata JSON
    local got
    got="$(host_metadata_problems "$3" "${lic}")"
    if [ -z "$2" ]; then
      if [ -n "${got}" ]; then echo "NG(self-test): $1: unexpected: ${got}" >&2; fail=1; else echo "OK(self-test): $1"; fi
    elif [[ "${got}" == *"$2"* ]]; then
      echo "OK(self-test): $1"
    else
      echo "NG(self-test): $1: expected '$2', got '${got}'" >&2; fail=1
    fi
  }
  hexpect "host valid" "" "${hostmeta}"
  hexpect "host missing description" "description is missing" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].description = null')"
  hexpect "host missing readme" "readme is missing" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].readme = null')"
  hexpect "host missing repository" "repository is missing" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].repository = ""')"
  hexpect "host missing license" "license is missing" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].license = null')"
  hexpect "host license differs" "differs from the workspace license" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].license = "MPL-2.0"')"
  hexpect "host publish = false" "publish is restricted" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].publish = []')"
  hexpect "host wrong name" "expected exactly one package" "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].name = "other"')"
  hexpect "host path dependency" "path dependency fandhe-browser-core is not allowed" \
    "$(printf '%s' "${hostmeta}" | jq -c '.packages[0].dependencies += [{name: "fandhe-browser-core", req: "=0.1.0", path: "/p"}]')"

  # release.yml の choice 選択肢（合成 yml）
  local y c
  {
    echo "        options:"
    echo "          - all"
    for c in "${RELEASE_CRATES[@]}"; do echo "          - ${c}"; done
  } >"${tmp}/rel-ok.yml"
  out="$(workflow_problems "${tmp}/rel-ok.yml")"
  if [ -z "${out}" ]; then echo "OK(self-test): workflow choices valid"; else echo "NG(self-test): workflow choices valid: ${out}" >&2; fail=1; fi
  grep -v -- '- fandhe-browser-servo-host$' "${tmp}/rel-ok.yml" >"${tmp}/rel-nohost.yml"
  out="$(workflow_problems "${tmp}/rel-nohost.yml")"
  if [[ "${out}" == *"missing fandhe-browser-servo-host"* ]]; then echo "OK(self-test): workflow choice without the host"; else echo "NG(self-test): workflow choice without the host: '${out}'" >&2; fail=1; fi
  y="$(cat "${tmp}/rel-ok.yml")"$'\n'"          - fandhe-browser-extra"
  printf '%s\n' "${y}" >"${tmp}/rel-extra.yml"
  out="$(workflow_problems "${tmp}/rel-extra.yml")"
  if [[ "${out}" == *"expected ${#RELEASE_CRATES[@]} crate choices"* ]]; then echo "OK(self-test): workflow choice with an extra entry"; else echo "NG(self-test): workflow choice with an extra entry: '${out}'" >&2; fail=1; fi

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
  local meta host_meta problems=""
  # JSON は標準出力のみ。cargo の警告（cli の [profile.release] 等）は標準エラーへ出るため混ぜない。
  meta="$(cargo metadata --no-deps --format-version 1)" || {
    echo "error: cargo metadata failed" >&2
    exit 1
  }
  # ホストは別 workspace（`--no-deps` なので依存は解決しない）
  host_meta="$(cargo metadata --no-deps --format-version 1 --manifest-path "${HOST_MANIFEST}")" || {
    echo "error: cargo metadata failed for ${HOST_MANIFEST}" >&2
    exit 1
  }
  problems="$(metadata_problems "${meta}")" || {
    echo "error: jq failed to evaluate publish settings" >&2
    exit 1
  }
  # 各検査の出力を改行で区切って連結する（改行が無いと別の検査の行が 1 行に繋がる）
  local more
  more="$(profile_problems Cargo.toml crates/fandhe-browser-cli/Cargo.toml fandhe-browser-cli)"
  [ -z "${more}" ] || problems+=$'\n'"${more}"
  more="$(profile_problems Cargo.toml "${HOST_MANIFEST}" "${HOST_CRATE}")"
  [ -z "${more}" ] || problems+=$'\n'"${more}"
  more="$(host_metadata_problems "${host_meta}" "$(section_value Cargo.toml '[workspace.package]' license)")" || {
    echo "error: jq failed to evaluate servo-host publish settings" >&2
    exit 1
  }
  [ -z "${more}" ] || problems+=$'\n'"${more}"
  more="$(host_version_problems Cargo.toml "${HOST_MANIFEST}")"
  [ -z "${more}" ] || problems+=$'\n'"${more}"
  more="$(workflow_problems .github/workflows/release.yml)"
  [ -z "${more}" ] || problems+=$'\n'"${more}"
  if [ -n "${problems}" ]; then
    echo "NG: crates.io publish readiness violations:" >&2
    printf '%s\n' "${problems}" >&2
    exit 1
  fi
  echo "OK: ${#RELEASE_CRATES[@]} release crates (${#PUBLIC_CRATES[@]} workspace members + ${HOST_CRATE}) are publish-ready; all other members set publish = false"
}

main "$@"
