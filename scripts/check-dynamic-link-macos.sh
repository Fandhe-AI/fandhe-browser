#!/usr/bin/env bash
# macOS 配布バイナリの動的リンク検査の判定ロジック正本（TASK-93.4・PLUG-1・#364）。
#
# 役割: Mach-O バイナリが `otool -L` で列挙する依存 dylib を、許可リスト
#   （scripts/macos-dylib-allowlist.txt）と照合する。Linux の musl 向け check-static-link.sh と対になる。
# 呼び出し元: scripts/harness.sh check-dynamic-link-macos（make 経由）。CI は harness-macos の
#   [dynamic-link-macos] ステップが harness 経由で実行し、harness-ubuntu は --self-test（合成データ）だけを回す。
#
# モード（#363 のオーナー判断 2026-10-12: 実測前は記録のみで導入し、実測後に許可リストを確定して fail-closed 化）:
#   enforce（既定・fail-closed）: 許可リスト外の依存があれば NG
#   record: 許可リスト外は ::warning:: で記録し 0 で終える（切り替えは呼び出し側が --mode を外すだけ）
# モードが影響するのは「許可リスト外」の扱いだけ。検査不能（非 Mach-O・不在・otool 失敗・依存 0 件・
#   許可リスト不正）はモードに関係なく NG（fail-closed）。
#
# 検出できるのはロード時の動的リンク（LC_LOAD_DYLIB 系）だけで、dlopen による実行時ロードは
# 検出しない（PLUG-1 の残りはレビューと docs/design/plugin-boundary-policy.md で担保する）。
# otool -L は対象を実行せず静的に読むだけ。
#
# 使い方: check-dynamic-link-macos.sh [--mode record|enforce] [--allowlist <file>] <binary>...
#         check-dynamic-link-macos.sh --self-test
# 終了コード: 0 = 全件 pass（record は許可リスト外でも 0）/ 1 = NG / 2 = 使用エラー・必須ツール不在
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- 純関数（テキスト入力のみ。--self-test が合成データで検証する） ---

# ログへ出す名前・パスを許可文字へ正規化する（ログ・workflow command 注入の防止）。
safe_name() {
  local b="${1}"
  b="${b//[^A-Za-z0-9._\/+@-]/_}"
  b="${b//::/__}"
  printf '%s\n' "${b}"
}

# otool -L の出力から依存パスを 1 行ずつ返す（重複は除く）。
# 依存行は "(compatibility version" を含む行。1 行目の "<path>:" と fat の "(architecture …):" は除外される。
parse_otool_deps() {
  local line path seen=$'\n'
  while IFS= read -r line; do
    case "${line}" in
      *" (compatibility version"*) ;;
      *) continue ;;
    esac
    path="${line%% (compatibility version*}"
    path="${path#"${path%%[![:space:]]*}"}"
    [ -n "${path}" ] || continue
    case "${seen}" in *$'\n'"${path}"$'\n'*) continue ;; esac
    seen+="${path}"$'\n'
    printf '%s\n' "${path}"
  done <<<"$1"
}

# load_allowlist <file>: 有効エントリを 1 行ずつ返す。不正エントリ・読めない・0 件は非 0。
load_allowlist() {
  local f="$1" line n=0 rc=0
  if [ ! -f "${f}" ] || [ ! -r "${f}" ]; then
    echo "error: allowlist not readable: $(safe_name "${f}")" >&2
    return 1
  fi
  while IFS= read -r line || [ -n "${line}" ]; do
    line="${line%$'\r'}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    case "${line}" in '' | '#'*) continue ;; esac
    case "${line}" in
      /*) ;;
      *) echo "error: invalid allowlist entry (must start with /): $(safe_name "${line}")" >&2; rc=1; continue ;;
    esac
    case "${line}" in
      *..*) echo "error: invalid allowlist entry (contains ..): $(safe_name "${line}")" >&2; rc=1; continue ;;
    esac
    printf '%s\n' "${line}"
    n=$((n + 1))
  done <"${f}"
  [ "${rc}" -eq 0 ] || return 1
  [ "${n}" -gt 0 ] || { echo "error: allowlist has no entries" >&2; return 1; }
}

# is_allowed <path> <allowlist_text>: 完全一致、または末尾 / のエントリの接頭辞一致なら 0。
is_allowed() {
  local p="$1" e
  case "${p}" in /*) ;; *) return 1 ;; esac
  case "${p}" in *..*) return 1 ;; esac
  while IFS= read -r e; do
    [ -n "${e}" ] || continue
    case "${e}" in
      */) case "${p}" in "${e}"?*) return 0 ;; esac ;;
      *) [ "${p}" = "${e}" ] && return 0 ;;
    esac
  done <<<"$2"
  return 1
}

# judge <deps> <outside>: pass / outside / error
judge() {
  if [ "$1" -eq 0 ]; then
    printf 'error\n'
  elif [ "$2" -gt 0 ]; then
    printf 'outside\n'
  else
    printf 'pass\n'
  fi
}

# --- 実検査 ---

require_tools() {
  local t
  for t in file otool; do
    command -v "${t}" >/dev/null 2>&1 || {
      echo "error: required command not found on PATH: ${t}" >&2
      return 1
    }
  done
}

# check_binary <path> <mode> <allowlist_text>: 結果行を stdout へ出す。pass は 0、NG は 1。
check_binary() {
  local bin="$1" mode="$2" allow="$3" name fout oout dep deps=0 outside=0 result flag
  case "${bin}" in -*) bin="./${bin}" ;; esac
  name="$(safe_name "${bin##*/}")"
  if [ ! -f "${bin}" ]; then
    echo "error: not a regular file: ${name}" >&2
    echo "dynamic-link-macos: bin=${name} mode=${mode} deps=0 outside=0 result=error"
    return 1
  fi
  fout="$(file -b "${bin}" 2>&1)" || fout="(file failed)"
  case "${fout}" in
    *Mach-O*) ;;
    *)
      echo "error: not a Mach-O file: ${name}" >&2
      echo "dynamic-link-macos: bin=${name} mode=${mode} deps=0 outside=0 result=error"
      return 1
      ;;
  esac
  if ! oout="$(otool -L "${bin}" 2>&1)"; then
    echo "error: otool -L failed: ${name}" >&2
    echo "dynamic-link-macos: bin=${name} mode=${mode} deps=0 outside=0 result=error"
    return 1
  fi
  while IFS= read -r dep; do
    [ -n "${dep}" ] || continue
    deps=$((deps + 1))
    if is_allowed "${dep}" "${allow}"; then
      flag=yes
    else
      flag=no
      outside=$((outside + 1))
    fi
    echo "dylib: bin=${name} path=$(safe_name "${dep}") allowed=${flag}"
  done < <(parse_otool_deps "${oout}")
  result="$(judge "${deps}" "${outside}")"
  echo "dynamic-link-macos: bin=${name} mode=${mode} deps=${deps} outside=${outside} result=${result}"
  case "${result}" in
    pass) return 0 ;;
    outside)
      if [ "${mode}" = "record" ]; then
        echo "::warning::dynamic-link-macos: ${outside} dylib(s) outside the allowlist in ${name} (record mode; not failing, see #363)"
        return 0
      fi
      echo "error: ${outside} dylib(s) outside the allowlist in ${name}" >&2
      return 1
      ;;
    *)
      echo "error: no dependencies parsed from otool -L output: ${name}" >&2
      return 1
      ;;
  esac
}

# --- 自己テスト ---

self_test() {
  local fails=0 rc
  expect() { # expect <label> <expected> <actual>
    if [ "$2" = "$3" ]; then
      echo "ok: $1"
    else
      echo "FAIL: $1 (expected '$2', got '$3')" >&2
      fails=$((fails + 1))
    fi
  }

  local single fat fw rpath
  single=$'/tmp/bin:\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1351.0.0)\n\t/usr/lib/libc++.1.dylib (compatibility version 1.0.0, current version 1900.180.0)'
  fat=$'/tmp/bin (architecture x86_64):\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n/tmp/bin (architecture arm64):\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n\t/usr/lib/libobjc.A.dylib (compatibility version 1.0.0, current version 228.0.0)'
  fw=$'/tmp/bin:\n\t/System/Library/Frameworks/Security.framework/Versions/A/Security (compatibility version 1.0.0, current version 61040.0.0)'
  rpath=$'/tmp/bin:\n\t@rpath/libfoo.dylib (compatibility version 0.0.0, current version 0.0.0)\n\t/opt/homebrew/lib/libssl.3.dylib (compatibility version 3.0.0, current version 3.0.0)'

  expect "parse: single arch" $'/usr/lib/libSystem.B.dylib\n/usr/lib/libc++.1.dylib' "$(parse_otool_deps "${single}")"
  expect "parse: fat dedup" $'/usr/lib/libSystem.B.dylib\n/usr/lib/libobjc.A.dylib' "$(parse_otool_deps "${fat}")"
  expect "parse: framework path" "/System/Library/Frameworks/Security.framework/Versions/A/Security" "$(parse_otool_deps "${fw}")"
  expect "parse: rpath and homebrew" $'@rpath/libfoo.dylib\n/opt/homebrew/lib/libssl.3.dylib' "$(parse_otool_deps "${rpath}")"
  expect "parse: no deps" "" "$(parse_otool_deps $'/tmp/bin:')"
  expect "parse: not an object file" "" "$(parse_otool_deps $'error: /tmp/x: is not an object file')"

  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 # tmp は関数 local のため、trap 登録時に値を展開する
  trap "rm -rf '${tmp}'" EXIT
  printf '%s\n' '# comment' '' '/usr/lib/libSystem.B.dylib' '  /System/Library/Frameworks/Security.framework/  ' >"${tmp}/ok.txt"
  local allow
  allow="$(load_allowlist "${tmp}/ok.txt")"
  expect "allowlist: parsed entries" $'/usr/lib/libSystem.B.dylib\n/System/Library/Frameworks/Security.framework/' "${allow}"
  local bad
  for bad in '@rpath/' '@loader_path/x' '@executable_path/x' 'relative/x' '/usr/lib/../x'; do
    printf '%s\n' "${bad}" >"${tmp}/bad.txt"
    if load_allowlist "${tmp}/bad.txt" >/dev/null 2>&1; then
      echo "FAIL: invalid allowlist entry accepted: ${bad}" >&2
      fails=$((fails + 1))
    else
      echo "ok: allowlist rejects ${bad}"
    fi
  done
  : >"${tmp}/empty.txt"
  if load_allowlist "${tmp}/empty.txt" >/dev/null 2>&1; then
    echo "FAIL: empty allowlist accepted" >&2
    fails=$((fails + 1))
  else
    echo "ok: empty allowlist rejected"
  fi
  if load_allowlist "${tmp}/missing.txt" >/dev/null 2>&1; then
    echo "FAIL: missing allowlist accepted" >&2
    fails=$((fails + 1))
  else
    echo "ok: missing allowlist rejected"
  fi

  is_allowed /usr/lib/libSystem.B.dylib "${allow}" && rc=yes || rc=no
  expect "allowed: exact" yes "${rc}"
  is_allowed /usr/lib/libSystem.B.dylib.evil "${allow}" && rc=yes || rc=no
  expect "allowed: exact is not prefix" no "${rc}"
  is_allowed /System/Library/Frameworks/Security.framework/Versions/A/Security "${allow}" && rc=yes || rc=no
  expect "allowed: directory prefix" yes "${rc}"
  is_allowed /System/Library/Frameworks/SecurityX.framework/X "${allow}" && rc=yes || rc=no
  expect "allowed: prefix needs boundary" no "${rc}"
  is_allowed @rpath/libfoo.dylib "${allow}" && rc=yes || rc=no
  expect "allowed: @rpath is never allowed" no "${rc}"
  is_allowed /opt/homebrew/lib/libssl.3.dylib "${allow}" && rc=yes || rc=no
  expect "allowed: homebrew outside" no "${rc}"

  expect "judge: pass" pass "$(judge 2 0)"
  expect "judge: outside" outside "$(judge 2 1)"
  expect "judge: zero deps is error" error "$(judge 0 0)"
  expect "safe_name: strips injection" "__warning__x_y" "$(safe_name '::warning::x y')"

  # 偽の otool・file を PATH 先頭に置き、record / enforce の終了コード差を実検査関数で確かめる。
  mkdir "${tmp}/fakebin"
  cat >"${tmp}/fakebin/file" <<'FAKE'
#!/bin/sh
echo "Mach-O 64-bit executable arm64"
FAKE
  cat >"${tmp}/fakebin/otool" <<'FAKE'
#!/bin/sh
printf '%s:\n' "$2"
printf '\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n'
printf '\t/opt/homebrew/lib/libssl.3.dylib (compatibility version 3.0.0, current version 3.0.0)\n'
FAKE
  chmod +x "${tmp}/fakebin/file" "${tmp}/fakebin/otool"
  : >"${tmp}/fakebin/target"
  local out
  out="$(PATH="${tmp}/fakebin:${PATH}" bash "${BASH_SOURCE[0]}" --mode enforce --allowlist "${tmp}/ok.txt" "${tmp}/fakebin/target" 2>&1)" && rc=0 || rc=$?
  expect "enforce: outside exits 1" 1 "${rc}"
  case "${out}" in *"result=outside"*) echo "ok: enforce reports result=outside" ;; *) echo "FAIL: enforce output: ${out}" >&2; fails=$((fails + 1)) ;; esac
  out="$(PATH="${tmp}/fakebin:${PATH}" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${tmp}/fakebin/target" 2>&1)" && rc=0 || rc=$?
  expect "record: outside exits 0" 0 "${rc}"
  case "${out}" in
    *"::warning::"*"result=outside"* | *"result=outside"*"::warning::"*) echo "ok: record warns and reports outside (never pass)" ;;
    *) echo "FAIL: record output: ${out}" >&2; fails=$((fails + 1)) ;;
  esac
  case "${out}" in *"::error"*) echo "FAIL: record emitted ::error" >&2; fails=$((fails + 1)) ;; *) echo "ok: record emits no ::error" ;; esac
  printf '%s\n' '/usr/lib/libSystem.B.dylib' '/opt/homebrew/lib/libssl.3.dylib' >"${tmp}/wide.txt"
  PATH="${tmp}/fakebin:${PATH}" bash "${BASH_SOURCE[0]}" --mode enforce --allowlist "${tmp}/wide.txt" "${tmp}/fakebin/target" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "enforce: all allowed exits 0" 0 "${rc}"
  PATH="${tmp}/fakebin:${PATH}" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/empty.txt" "${tmp}/fakebin/target" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "record: broken allowlist still fails" 1 "${rc}"
  bash "${BASH_SOURCE[0]}" --mode bogus x >/dev/null 2>&1 && rc=0 || rc=$?
  expect "usage: bad mode exits 2" 2 "${rc}"
  bash "${BASH_SOURCE[0]}" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "usage: no args exits 2" 2 "${rc}"

  if [ "$(uname -s)" != "Darwin" ]; then
    echo "note: not Darwin; skipping real-binary cases" >&2
  else
    local real
    real="$(parse_otool_deps "$(otool -L /bin/ls)")"
    if [ -n "${real}" ]; then
      echo "ok: /bin/ls deps parsed"
    else
      echo "FAIL: no deps parsed from /bin/ls" >&2
      fails=$((fails + 1))
    fi
    bash "${BASH_SOURCE[0]}" --mode record "${BASH_SOURCE[0]}" >/dev/null 2>&1 && rc=0 || rc=$?
    expect "real: non-Mach-O is error" 1 "${rc}"
    bash "${BASH_SOURCE[0]}" --mode record /nonexistent/dynamic-link-self-test >/dev/null 2>&1 && rc=0 || rc=$?
    expect "real: missing path is error" 1 "${rc}"
  fi

  if [ "${fails}" -ne 0 ]; then
    echo "error: self-test failed (${fails})" >&2
    return 1
  fi
  echo "OK: self-test passed"
}

main() {
  local mode=enforce allowlist="${SCRIPT_DIR}/macos-dylib-allowlist.txt"
  if [ "${1:-}" = "--self-test" ]; then
    [ "$#" -eq 1 ] || { echo "usage: check-dynamic-link-macos.sh --self-test" >&2; exit 2; }
    self_test
    return
  fi
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --mode)
        [ "$#" -ge 2 ] || { echo "usage: --mode needs record|enforce" >&2; exit 2; }
        mode="$2"
        shift 2
        ;;
      --allowlist)
        [ "$#" -ge 2 ] || { echo "usage: --allowlist needs a file" >&2; exit 2; }
        allowlist="$2"
        shift 2
        ;;
      --) shift; break ;;
      *) break ;;
    esac
  done
  case "${mode}" in record | enforce) ;; *) echo "usage: --mode must be record or enforce" >&2; exit 2 ;; esac
  if [ "$#" -eq 0 ]; then
    echo "usage: check-dynamic-link-macos.sh [--mode record|enforce] [--allowlist <file>] <binary>... | --self-test" >&2
    exit 2
  fi
  require_tools || exit 2
  local allow rc=0 b
  allow="$(load_allowlist "${allowlist}")" || { echo "dynamic-link-macos: result=error (allowlist)"; exit 1; }
  for b in "$@"; do
    check_binary "${b}" "${mode}" "${allow}" || rc=1
  done
  exit "${rc}"
}

main "$@"
