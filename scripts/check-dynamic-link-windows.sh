#!/usr/bin/env bash
# Windows 配布バイナリの動的リンク検査の判定ロジック正本（TASK-93.5・PLUG-1・#365）。
#
# 役割: PE バイナリが `dumpbin -dependents` で列挙するロード時依存 DLL（通常の import と delay load）を、
#   許可リスト（scripts/windows-dll-allowlist.txt）と照合する。check-static-link.sh（Linux musl）・
#   check-dynamic-link-macos.sh（macOS）と対になる。
# 呼び出し元: scripts/harness.sh check-dynamic-link-windows（make 経由）。CI は harness-windows の
#   [dynamic-link-windows] ステップが harness 経由で実行し、harness-ubuntu は --self-test（合成データ）だけを回す。
#
# モード（#363 のオーナー判断 2026-10-12: 実測前は記録のみで導入し、実測後に許可リストを確定して fail-closed 化）:
#   enforce（スクリプト単体の既定・fail-closed）: 許可リスト外の依存があれば NG
#   record: 許可リスト外は ::warning:: で記録し 0 で終える（harness 側の既定。期間限定の例外）
# モードが影響するのは「許可リスト外」の扱いだけ。検査不能（非 PE・不在・dumpbin 失敗・依存 0 件・
#   許可リスト不正・dumpbin 不在）はモードに関係なく NG（fail-closed）。
# CRT 系 DLL が現れた場合は +crt-static（#917）が効いていない兆候として、モードによらず専用 warning を出す。
#
# 検出できるのはロード時の動的リンクだけで、LoadLibrary による実行時ロードは検出しない
# （PLUG-1 の残りはレビューと docs/design/plugin-boundary-policy.md で担保する）。
# dumpbin は対象を実行せず静的に読むだけ。
#
# 使い方: check-dynamic-link-windows.sh [--mode record|enforce] [--allowlist <file>] <binary>...
#         check-dynamic-link-windows.sh --self-test
# 環境変数: DUMPBIN（dumpbin.exe のパス。自己テストが偽物を差し込むのにも使う）
# 終了コード: 0 = 全件 pass（record は許可リスト外でも 0）/ 1 = NG / 2 = 使用エラー・必須ツール不在
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- 純関数（テキスト入力のみ。--self-test が合成データで検証する） ---

# ログへ出す名前を許可文字へ正規化する（ログ・workflow command 注入の防止）。
safe_name() {
  local b="${1}"
  b="${b//[^A-Za-z0-9._+-]/_}"
  b="${b//::/__}"
  printf '%s\n' "${b}"
}

# dumpbin 出力から依存を "<kind>\t<name>"（kind は load / delay）で返す（小文字化・重複除去）。
# 名前が許可文字・長さに合わない行があれば "invalid\t<safe>" を出す（呼び出し側が error 扱いにする）。
parse_dumpbin_deps() {
  local line kind="" name seen=$'\n' key
  while IFS= read -r line; do
    line="${line//$'\r'/}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    case "${line}" in
      "Image has the following dependencies:") kind=load; continue ;;
      "Image has the following delay load dependencies:") kind=delay; continue ;;
      "Summary"*) kind=""; continue ;;
      "Image has the following"* | "Section contains the following"*) kind=""; continue ;;
    esac
    [ -n "${kind}" ] || continue
    [ -n "${line}" ] || continue
    name="$(printf '%s' "${line}" | tr 'A-Z' 'a-z')"
    if ! printf '%s\n' "${name}" | grep -Eq '^[a-z0-9._-]{1,128}\.dll$'; then
      printf 'invalid\t%s\n' "$(safe_name "${line}")"
      continue
    fi
    key="${kind}	${name}"
    case "${seen}" in *$'\n'"${key}"$'\n'*) continue ;; esac
    seen+="${key}"$'\n'
    printf '%s\t%s\n' "${kind}" "${name}"
  done <<<"$1"
}

# dumpbin 出力が PE（EXE / DLL）の File Type を含むなら 0。
parse_file_type() {
  local line
  while IFS= read -r line; do
    line="${line//$'\r'/}"
    case "${line}" in
      "File Type: EXECUTABLE IMAGE"* | "File Type: DLL"*) return 0 ;;
    esac
  done <<<"$1"
  return 1
}

# load_allowlist <file>: 有効エントリ（小文字）を 1 行ずつ返す。不正エントリ・読めない・0 件は非 0。
load_allowlist() {
  local f="$1" line n=0 rc=0
  if [ ! -f "${f}" ] || [ ! -r "${f}" ]; then
    echo "error: allowlist not readable: $(safe_name "${f}")" >&2
    return 1
  fi
  while IFS= read -r line || [ -n "${line}" ]; do
    line="${line//$'\r'/}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    case "${line}" in '' | '#'*) continue ;; esac
    line="$(printf '%s' "${line}" | tr 'A-Z' 'a-z')"
    if ! printf '%s\n' "${line}" | grep -Eq '^[a-z0-9._-]{1,128}\.dll$' || [[ "${line}" == *..* ]]; then
      echo "error: invalid allowlist entry (plain lowercase *.dll name only): $(safe_name "${line}")" >&2
      rc=1
      continue
    fi
    printf '%s\n' "${line}"
    n=$((n + 1))
  done <"${f}"
  [ "${rc}" -eq 0 ] || return 1
  [ "${n}" -gt 0 ] || { echo "error: allowlist has no entries" >&2; return 1; }
}

# is_allowed <name> <allowlist_text>: 完全一致のみ 0（接頭辞ルールは設けない）。
is_allowed() {
  local e
  while IFS= read -r e; do
    [ -n "${e}" ] || continue
    [ "$1" = "${e}" ] && return 0
  done <<<"$2"
  return 1
}

# is_forbidden_crt <name>: +crt-static の退行（CRT の動的リンク）に当たる名前なら 0。
is_forbidden_crt() {
  case "$1" in
    vcruntime140*.dll | msvcp140*.dll | ucrtbase.dll | api-ms-win-crt-*.dll) return 0 ;;
  esac
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

# dumpbin の場所を解決して stdout に返す。環境変数 DUMPBIN -> PATH -> vswhere の順。見つからなければ非 0。
resolve_dumpbin() {
  local p pf86 vsw found
  if [ -n "${DUMPBIN:-}" ]; then
    printf '%s\n' "${DUMPBIN}"
    return 0
  fi
  if p="$(command -v dumpbin 2>/dev/null)" && [ -n "${p}" ]; then
    printf '%s\n' "${p}"
    return 0
  fi
  pf86="$(printenv 'ProgramFiles(x86)' 2>/dev/null || true)"
  vsw="${pf86}/Microsoft Visual Studio/Installer/vswhere.exe"
  if [ -n "${pf86}" ] && [ -x "${vsw}" ]; then
    found="$(MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 "${vsw}" -latest -products '*' \
      -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
      -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' 2>/dev/null | tr -d '\r' | tail -n 1 || true)"
    if [ -n "${found}" ]; then
      if command -v cygpath >/dev/null 2>&1; then
        found="$(cygpath -u "${found}")"
      fi
      printf '%s\n' "${found}"
      return 0
    fi
  fi
  return 1
}

# to_dumpbin_path <path>: dumpbin へ渡すパス表記を返す。cygpath があれば Windows 形式（cygpath -w）、なければそのまま。
# run_dumpbin と自己テストの期待値が同じ変換を共有し、食い違いを防ぐ。
to_dumpbin_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

# run_dumpbin <dumpbin> <binary>: dumpbin -nologo -dependents の出力を返す。
# Git Bash の引数パス変換（/dependents 等）を避けるためダッシュ形式で呼び、変換も無効化する。
run_dumpbin() {
  local db="$1" bin="$2" winbin
  winbin="$(to_dumpbin_path "${bin}")"
  MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 "${db}" -nologo -dependents "${winbin}" 2>&1
}

# check_binary <path> <mode> <allowlist_text> <dumpbin>: 結果行を stdout へ出す。pass は 0、NG は 1。
check_binary() {
  local bin="$1" mode="$2" allow="$3" db="$4" name oout kind dll deps=0 outside=0 crt=0 invalid=0 result flag
  case "${bin}" in -*) bin="./${bin}" ;; esac
  name="$(safe_name "${bin##*/}")"
  if [ ! -f "${bin}" ]; then
    echo "error: not a regular file: ${name}" >&2
    echo "dynamic-link-windows: bin=${name} mode=${mode} deps=0 outside=0 forbidden_crt=0 result=error"
    return 1
  fi
  if ! oout="$(run_dumpbin "${db}" "${bin}")"; then
    echo "error: dumpbin failed: ${name}" >&2
    echo "dynamic-link-windows: bin=${name} mode=${mode} deps=0 outside=0 forbidden_crt=0 result=error"
    return 1
  fi
  if ! parse_file_type "${oout}"; then
    echo "error: not a PE image (no File Type line): ${name}" >&2
    echo "dynamic-link-windows: bin=${name} mode=${mode} deps=0 outside=0 forbidden_crt=0 result=error"
    return 1
  fi
  while IFS=$'\t' read -r kind dll; do
    [ -n "${kind}" ] || continue
    if [ "${kind}" = "invalid" ]; then
      invalid=$((invalid + 1))
      continue
    fi
    deps=$((deps + 1))
    if is_allowed "${dll}" "${allow}"; then
      flag=yes
    else
      flag=no
      outside=$((outside + 1))
    fi
    if is_forbidden_crt "${dll}"; then
      crt=$((crt + 1))
    fi
    echo "dll: bin=${name} kind=${kind} name=${dll} allowed=${flag}"
  done < <(parse_dumpbin_deps "${oout}")
  if [ "${invalid}" -gt 0 ]; then
    echo "error: ${invalid} unparsable dependency line(s) in dumpbin output: ${name}" >&2
    echo "dynamic-link-windows: bin=${name} mode=${mode} deps=${deps} outside=${outside} forbidden_crt=${crt} result=error"
    return 1
  fi
  result="$(judge "${deps}" "${outside}")"
  echo "dynamic-link-windows: bin=${name} mode=${mode} deps=${deps} outside=${outside} forbidden_crt=${crt} result=${result}"
  if [ "${crt}" -gt 0 ]; then
    echo "::warning::dynamic-link-windows: CRT DLL(s) found in ${name} (+crt-static not effective? see #917)"
  fi
  case "${result}" in
    pass) return 0 ;;
    outside)
      if [ "${mode}" = "record" ]; then
        echo "::warning::dynamic-link-windows: ${outside} DLL(s) outside the allowlist in ${name} (record mode; not failing, see #363)"
        return 0
      fi
      echo "error: ${outside} DLL(s) outside the allowlist in ${name}" >&2
      return 1
      ;;
    *)
      echo "error: no dependencies parsed from dumpbin output: ${name}" >&2
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

  local hdr load both crlf
  hdr=$'Microsoft (R) COFF/PE Dumper Version 14.40\n\nDump of file x.exe\n\nFile Type: EXECUTABLE IMAGE\n\n'
  load="${hdr}"$'  Image has the following dependencies:\n\n    KERNEL32.dll\n    ntdll.dll\n    KERNEL32.dll\n\n  Summary\n\n        1000 .data\n          10 KERNEL32.dll\n'
  both="${hdr}"$'  Image has the following dependencies:\n\n    kernel32.dll\n\n  Image has the following delay load dependencies:\n\n    CRYPT32.dll\n\n  Summary\n'
  crlf="${load//$'\n'/$'\r\n'}"

  expect "parse: load only, lowercase, dedup, Summary cut" $'load\tkernel32.dll\nload\tntdll.dll' "$(parse_dumpbin_deps "${load}")"
  expect "parse: load and delay" $'load\tkernel32.dll\ndelay\tcrypt32.dll' "$(parse_dumpbin_deps "${both}")"
  expect "parse: CRLF input" $'load\tkernel32.dll\nload\tntdll.dll' "$(parse_dumpbin_deps "${crlf}")"
  expect "parse: no dependency section" "" "$(parse_dumpbin_deps "${hdr}")"
  expect "parse: invalid name is flagged" $'invalid\tbad_name_x' "$(parse_dumpbin_deps $'  Image has the following dependencies:\n    bad name x\n')"
  parse_file_type "${hdr}" && rc=yes || rc=no
  expect "file type: EXECUTABLE IMAGE" yes "${rc}"
  parse_file_type $'Dump of file x\nfatal error LNK1107: invalid or corrupt file' && rc=yes || rc=no
  expect "file type: non-PE rejected" no "${rc}"

  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 # tmp は関数 local のため、trap 登録時に値を展開する
  trap "rm -rf '${tmp}'" EXIT
  printf '%s\r\n' '# comment' '' '  KERNEL32.DLL  ' 'ntdll.dll' >"${tmp}/ok.txt"
  local allow
  allow="$(load_allowlist "${tmp}/ok.txt")"
  expect "allowlist: parsed (comment, CRLF, space, case)" $'kernel32.dll\nntdll.dll' "${allow}"
  local bad
  for bad in 'a/b.dll' 'a\b.dll' '..\x.dll' 'api-ms-win-crt-*.dll' 'foo' 'foo.exe'; do
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

  is_allowed kernel32.dll "${allow}" && rc=yes || rc=no
  expect "allowed: exact" yes "${rc}"
  is_allowed kernel32.dll.evil "${allow}" && rc=yes || rc=no
  expect "allowed: no suffix match" no "${rc}"
  is_allowed api-ms-win-crt-runtime-l1-1-0.dll "${allow}" && rc=yes || rc=no
  expect "allowed: no api-ms-win-crt prefix rule" no "${rc}"
  is_forbidden_crt vcruntime140.dll && rc=yes || rc=no
  expect "crt: vcruntime140" yes "${rc}"
  is_forbidden_crt api-ms-win-crt-heap-l1-1-0.dll && rc=yes || rc=no
  expect "crt: api-ms-win-crt-*" yes "${rc}"
  is_forbidden_crt kernel32.dll && rc=yes || rc=no
  expect "crt: kernel32 is not crt" no "${rc}"

  expect "judge: pass" pass "$(judge 2 0)"
  expect "judge: outside" outside "$(judge 2 1)"
  expect "judge: zero deps is error" error "$(judge 0 0)"
  expect "safe_name: strips injection" "__warning__x_y" "$(safe_name '::warning::x y')"

  # 偽の dumpbin を DUMPBIN で差し込み、record / enforce の終了コード差と呼び出し引数を実検査関数で確かめる。
  cat >"${tmp}/fake-dumpbin" <<FAKE
#!/bin/sh
printf '%s\n' "\$*" >"${tmp}/args.log"
printf 'Dump of file x\n\nFile Type: EXECUTABLE IMAGE\n\n  Image has the following dependencies:\n\n    KERNEL32.dll\n    VCRUNTIME140.dll\n\n  Summary\n'
FAKE
  chmod +x "${tmp}/fake-dumpbin"
  : >"${tmp}/target"
  local out
  out="$(DUMPBIN="${tmp}/fake-dumpbin" bash "${BASH_SOURCE[0]}" --mode enforce --allowlist "${tmp}/ok.txt" "${tmp}/target" 2>&1)" && rc=0 || rc=$?
  expect "enforce: outside exits 1" 1 "${rc}"
  case "${out}" in *"result=outside"*) echo "ok: enforce reports result=outside" ;; *) echo "FAIL: enforce output: ${out}" >&2; fails=$((fails + 1)) ;; esac
  case "${out}" in *"CRT DLL(s) found"*) echo "ok: CRT warning emitted" ;; *) echo "FAIL: no CRT warning: ${out}" >&2; fails=$((fails + 1)) ;; esac
  expect "dumpbin args: dash-style options" "-nologo -dependents $(to_dumpbin_path "${tmp}/target")" "$(cat "${tmp}/args.log")"
  # Windows 形式パス変換の契約: 偽 cygpath（-w の結果に W: を前置）が PATH にあるとき、dumpbin へ変換後のパスが渡る。
  mkdir -p "${tmp}/fakebin"
  cat >"${tmp}/fakebin/cygpath" <<'FAKECYG'
#!/bin/sh
printf 'W:%s\n' "$2"
FAKECYG
  chmod +x "${tmp}/fakebin/cygpath"
  PATH="${tmp}/fakebin:${PATH}" DUMPBIN="${tmp}/fake-dumpbin" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${tmp}/target" >/dev/null 2>&1 || true
  expect "dumpbin args: cygpath -w converted path" "-nologo -dependents W:${tmp}/target" "$(cat "${tmp}/args.log")"
  out="$(DUMPBIN="${tmp}/fake-dumpbin" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${tmp}/target" 2>&1)" && rc=0 || rc=$?
  expect "record: outside exits 0" 0 "${rc}"
  case "${out}" in
    *"::warning::"*"result=outside"* | *"result=outside"*"::warning::"*) echo "ok: record warns and reports outside (never pass)" ;;
    *) echo "FAIL: record output: ${out}" >&2; fails=$((fails + 1)) ;;
  esac
  case "${out}" in *"::error"*) echo "FAIL: record emitted ::error" >&2; fails=$((fails + 1)) ;; *) echo "ok: record emits no ::error" ;; esac
  printf '%s\n' 'kernel32.dll' 'vcruntime140.dll' >"${tmp}/wide.txt"
  DUMPBIN="${tmp}/fake-dumpbin" bash "${BASH_SOURCE[0]}" --mode enforce --allowlist "${tmp}/wide.txt" "${tmp}/target" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "enforce: all allowed exits 0" 0 "${rc}"
  DUMPBIN="${tmp}/fake-dumpbin" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/empty.txt" "${tmp}/target" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "record: broken allowlist still fails" 1 "${rc}"
  cat >"${tmp}/fake-nonpe" <<'FAKE'
#!/bin/sh
echo "fatal error LNK1107: invalid or corrupt file: cannot read at 0x2B8"
FAKE
  chmod +x "${tmp}/fake-nonpe"
  DUMPBIN="${tmp}/fake-nonpe" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${tmp}/target" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "record: non-PE is error" 1 "${rc}"
  cat >"${tmp}/fake-nodeps" <<'FAKE'
#!/bin/sh
printf 'File Type: EXECUTABLE IMAGE\n\n  Summary\n'
FAKE
  chmod +x "${tmp}/fake-nodeps"
  DUMPBIN="${tmp}/fake-nodeps" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${tmp}/target" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "record: zero deps is error" 1 "${rc}"
  DUMPBIN="${tmp}/fake-dumpbin" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${tmp}/nonexistent" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "record: missing path is error" 1 "${rc}"
  bash "${BASH_SOURCE[0]}" --mode bogus x >/dev/null 2>&1 && rc=0 || rc=$?
  expect "usage: bad mode exits 2" 2 "${rc}"
  bash "${BASH_SOURCE[0]}" >/dev/null 2>&1 && rc=0 || rc=$?
  expect "usage: no args exits 2" 2 "${rc}"

  # 実物の陰性対照は Windows で dumpbin が解決できるときだけ回す。
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      local realdb
      if realdb="$(resolve_dumpbin)"; then
        local real
        real="$(parse_dumpbin_deps "$(run_dumpbin "${realdb}" "/c/Windows/System32/cmd.exe" || true)")"
        if [ -n "${real}" ]; then
          echo "ok: cmd.exe deps parsed"
        else
          echo "FAIL: no deps parsed from cmd.exe" >&2
          fails=$((fails + 1))
        fi
        DUMPBIN="${realdb}" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" "${BASH_SOURCE[0]}" >/dev/null 2>&1 && rc=0 || rc=$?
        expect "real: non-PE is error" 1 "${rc}"
        DUMPBIN="${realdb}" bash "${BASH_SOURCE[0]}" --mode record --allowlist "${tmp}/ok.txt" /nonexistent/dynamic-link-self-test >/dev/null 2>&1 && rc=0 || rc=$?
        expect "real: missing path is error" 1 "${rc}"
      else
        echo "note: dumpbin not found; skipping real-binary cases" >&2
      fi
      ;;
    *) echo "note: not Windows; skipping real-binary cases" >&2 ;;
  esac

  if [ "${fails}" -ne 0 ]; then
    echo "error: self-test failed (${fails})" >&2
    return 1
  fi
  echo "OK: self-test passed"
}

main() {
  local mode=enforce allowlist="${SCRIPT_DIR}/windows-dll-allowlist.txt"
  if [ "${1:-}" = "--self-test" ]; then
    [ "$#" -eq 1 ] || { echo "usage: check-dynamic-link-windows.sh --self-test" >&2; exit 2; }
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
    echo "usage: check-dynamic-link-windows.sh [--mode record|enforce] [--allowlist <file>] <binary>... | --self-test" >&2
    exit 2
  fi
  local db allow rc=0 b
  db="$(resolve_dumpbin)" || { echo "error: dumpbin not found (set DUMPBIN, or install MSVC build tools)" >&2; exit 2; }
  allow="$(load_allowlist "${allowlist}")" || { echo "dynamic-link-windows: result=error (allowlist)"; exit 1; }
  for b in "$@"; do
    check_binary "${b}" "${mode}" "${allow}" "${db}" || rc=1
  done
  exit "${rc}"
}

main "$@"
