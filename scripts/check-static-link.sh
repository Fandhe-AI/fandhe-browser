#!/usr/bin/env bash
# musl 配布バイナリの動的リンクなし検査の判定ロジック正本（TASK-93.3・PLUG-1・CTR-4・container-cloud 決定 5）。
#
# 役割: ELF バイナリが動的リンクを持たない（static / static-pie）ことを file・readelf・ldd で判定する。
# 呼び出し元は 3 か所で、判定ロジックはここだけに置く。
#   - docker/verify-static.sh        Dockerfile.release の verify-* ステージ（出荷物そのものの関門）
#   - scripts/harness.sh check-static-link   musl 既定・軽量ビルドを作って検査（make check-static-link）
#   - .github/workflows/ci.yml の harness-ubuntu [static-link] ステップ（上記 harness 経由）
#
# 対象は musl ビルドの配布バイナリ（TASK-64 の既定・軽量コンテナ向け）。`*-linux-gnu` のネイティブ配布物は
# 対象外（方針は #363 で人間が判断する）。検出できるのはロード時の動的リンクだけで、dlopen による
# 実行時ロードは検出しない（PLUG-1 の残りはレビューと docs/design/plugin-boundary-policy.md で担保する）。
# glibc の ldd は対象のローダを起動しうるため、信頼できる（同じジョブでビルドした）バイナリにだけ使う。
#
# 判定（全て満たしたときだけ pass）: readelf -d の (NEEDED) が 0 件 / readelf -l に INTERP が無い /
#   ldd に "=>" 行が無い。file の文言は記録のみで判定に使わない
#   （古い file は static-pie でも dynamically linked と出すため。旧 verify-static.sh と同じ方針）。
# 非 ELF・不在・ツール出力の取得失敗は fail-open にせず NG とする。
#
# 使い方: check-static-link.sh <binary>...   |   check-static-link.sh --self-test
# 終了コード: 0 = 全件 pass / 1 = 1 件でも NG / 2 = 使用エラー・必須ツール不在
set -euo pipefail
export LC_ALL=C

# --- 純関数（テキスト入力のみ。--self-test が合成データで検証する） ---

# readelf -d の出力から (NEEDED) 行数を返す。
count_needed() {
  local n=0 line
  while IFS= read -r line; do
    case "${line}" in *"(NEEDED)"*) n=$((n + 1)) ;; esac
  done <<<"$1"
  printf '%s\n' "${n}"
}

# readelf -lW の出力に INTERP セグメントがあれば present、無ければ absent。
has_interp() {
  local line
  while IFS= read -r line; do
    case "${line}" in
      "  INTERP "* | " INTERP "* | "INTERP "*) printf 'present\n'; return 0 ;;
    esac
  done <<<"$1"
  printf 'absent\n'
}

# ldd の出力から共有ライブラリ依存（"=>" を含む行）の数を返す。
# musl の ldd は静的 PIE でもローダ自身の 1 行を出すため、終了コードや行数では判定しない。
count_ldd_deps() {
  local n=0 line
  while IFS= read -r line; do
    case "${line}" in *"=>"*) n=$((n + 1)) ;; esac
  done <<<"$1"
  printf '%s\n' "${n}"
}

# file -b の出力を static-pie / static / other に分類する。
classify_file() {
  case "$1" in
    *"static-pie linked"*) printf 'static-pie\n' ;;
    *"statically linked"*) printf 'static\n' ;;
    *) printf 'other\n' ;;
  esac
}

# judge <needed> <interp> <ldd_deps> <file_class>: pass / fail
judge() {
  if [ "$1" = "0" ] && [ "$2" = "absent" ] && [ "$3" = "0" ]; then
    printf 'pass\n'
  else
    printf 'fail\n'
  fi
}

# 結果行へ出す名前を許可文字へ正規化する（ログ注入の防止）。
safe_name() {
  local b="${1##*/}"
  printf '%s\n' "${b//[^A-Za-z0-9._-]/_}"
}

# --- 実検査 ---

require_tools() {
  local t
  for t in file readelf ldd; do
    command -v "${t}" >/dev/null 2>&1 || {
      echo "error: required command not found on PATH: ${t}" >&2
      return 1
    }
  done
}

# check_binary <path>: 結果行を stdout へ出し、pass なら 0・NG なら 1 を返す。
check_binary() {
  local bin="$1" name
  case "${bin}" in -*) bin="./${bin}" ;; esac
  name="$(safe_name "${bin}")"
  if [ ! -f "${bin}" ]; then
    echo "error: not a regular file: ${name}" >&2
    return 1
  fi
  local fout fclass dyn phdr needed interp ldd_deps ldd_out result
  fout="$(file -b "${bin}" 2>&1)" || fout="(file failed)"
  echo "file: ${fout}"
  fclass="$(classify_file "${fout}")"
  # readelf は非 ELF・破損で非 0 になる。握りつぶすと fail-open になるため明示的に NG にする。
  if ! dyn="$(readelf -d "${bin}" 2>&1)"; then
    echo "error: readelf -d failed (not a readable ELF?): ${name}" >&2
    return 1
  fi
  if ! phdr="$(readelf -lW "${bin}" 2>&1)"; then
    echo "error: readelf -l failed (not a readable ELF?): ${name}" >&2
    return 1
  fi
  needed="$(count_needed "${dyn}")"
  if [ "${needed}" != "0" ]; then
    grep -F '(NEEDED)' <<<"${dyn}" || true
  fi
  interp="$(has_interp "${phdr}")"
  if [ "${interp}" = "absent" ]; then
    # 終了コードは libc 間で異なるため使わず、出力の "=>" 行だけを見る。
    ldd_out="$(ldd "${bin}" 2>&1 || true)"
    ldd_deps="$(count_ldd_deps "${ldd_out}")"
  else
    # INTERP があるバイナリを glibc の ldd で起動しない。
    echo "ldd: skipped (INTERP present)"
    ldd_deps="skipped"
  fi
  case "${ldd_deps}" in skipped) result="fail" ;; *) result="$(judge "${needed}" "${interp}" "${ldd_deps}" "${fclass}")" ;; esac
  echo "static-link: bin=${name} needed=${needed} interp=${interp} ldd_deps=${ldd_deps} file=${fclass} result=${result}"
  [ "${result}" = "pass" ]
}

# --- 自己テスト ---

self_test() {
  local fails=0
  expect() { # expect <label> <expected> <actual>
    if [ "$2" = "$3" ]; then
      echo "ok: $1"
    else
      echo "FAIL: $1 (expected '$2', got '$3')" >&2
      fails=$((fails + 1))
    fi
  }

  local dyn_two dyn_static big i
  dyn_two=$' 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]\n 0x0000000000000001 (NEEDED)             Shared library: [libm.so.6]'
  dyn_static=$'Dynamic section at offset 0x1000 contains 2 entries:\n 0x000000000000001e (FLAGS)              STATIC_TLS'
  big=""
  for ((i = 0; i < 3000; i++)); do big+=$' 0x0000000000000001 (NEEDED)             Shared library: [libx.so]\n'; done
  expect "needed: two entries" 2 "$(count_needed "${dyn_two}")"
  expect "needed: static-pie dynamic section" 0 "$(count_needed "${dyn_static}")"
  expect "needed: no dynamic section" 0 "$(count_needed 'There is no dynamic section in this file.')"
  expect "needed: 3000 lines (large output)" 3000 "$(count_needed "${big}")"

  expect "interp: present" present "$(has_interp $'Program Headers:\n  PHDR   0x40\n  INTERP 0x318 0x318\n      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]')"
  expect "interp: absent" absent "$(has_interp $'Program Headers:\n  LOAD   0x0\n  DYNAMIC 0x1\n  GNU_PROPERTY 0x2')"

  expect "ldd: glibc dynamic" 2 "$(count_ldd_deps $'\tlibc.so.6 => /lib/libc.so.6 (0x1)\n\tlibm.so.6 => /lib/libm.so.6 (0x2)\n\t/lib64/ld-linux-x86-64.so.2 (0x3)')"
  expect "ldd: statically linked" 0 "$(count_ldd_deps $'\tstatically linked')"
  expect "ldd: not a dynamic executable" 0 "$(count_ldd_deps $'\tnot a dynamic executable')"
  expect "ldd: musl loader line only" 0 "$(count_ldd_deps $'\t/lib/ld-musl-x86_64.so.1 (0x7f0000000000)')"

  expect "file: static-pie" static-pie "$(classify_file 'ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped')"
  expect "file: static" static "$(classify_file 'ELF 64-bit LSB executable, x86-64, statically linked, stripped')"
  expect "file: dynamic" other "$(classify_file 'ELF 64-bit LSB pie executable, x86-64, dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2')"

  expect "judge: clean" pass "$(judge 0 absent 0 static-pie)"
  expect "judge: needed" fail "$(judge 1 absent 0 static-pie)"
  expect "judge: interp" fail "$(judge 0 present 0 static-pie)"
  expect "judge: ldd deps" fail "$(judge 0 absent 1 static-pie)"
  expect "judge: file class is informational" pass "$(judge 0 absent 0 other)"

  if [ "$(uname -s)" != "Linux" ]; then
    echo "note: not Linux; skipping real-binary cases" >&2
  else
    require_tools || fails=$((fails + 1))
    # 実物の陰性対照: 動的 ELF を 1 つ以上 fail と判定できなければ、何でも pass する退行を検出できない。
    local cand detected=0 out
    for cand in "${BASH:-}" "$(command -v readelf || true)" /bin/ls; do
      [ -f "${cand}" ] || continue
      if out="$(check_binary "${cand}" 2>&1)"; then
        echo "note: ${cand} was judged static (skipping as negative control)" >&2
      else
        case "${out}" in *"result=fail"*) detected=$((detected + 1)) ;; esac
      fi
    done
    if [ "${detected}" -ge 1 ]; then
      echo "ok: negative control (dynamic ELF detected: ${detected})"
    else
      echo "FAIL: no dynamic ELF was detected as fail (negative control)" >&2
      fails=$((fails + 1))
    fi
    # 非 ELF（スクリプト自身）と不在パスは fail-open にしない。
    if check_binary "${BASH_SOURCE[0]}" >/dev/null 2>&1; then
      echo "FAIL: non-ELF input was accepted" >&2
      fails=$((fails + 1))
    else
      echo "ok: non-ELF rejected"
    fi
    if check_binary "/nonexistent/static-link-self-test" >/dev/null 2>&1; then
      echo "FAIL: missing path was accepted" >&2
      fails=$((fails + 1))
    else
      echo "ok: missing path rejected"
    fi
    if bash "${BASH_SOURCE[0]}" >/dev/null 2>&1; then
      echo "FAIL: no-argument call did not fail" >&2
      fails=$((fails + 1))
    else
      rc=$?
      expect "usage exit code" 2 "${rc}"
    fi
  fi

  if [ "${fails}" -ne 0 ]; then
    echo "error: self-test failed (${fails})" >&2
    return 1
  fi
  echo "OK: self-test passed"
}

main() {
  if [ "$#" -eq 0 ]; then
    echo "usage: check-static-link.sh <binary>... | --self-test" >&2
    exit 2
  fi
  if [ "$1" = "--self-test" ]; then
    [ "$#" -eq 1 ] || { echo "usage: check-static-link.sh --self-test" >&2; exit 2; }
    self_test
    return
  fi
  require_tools || exit 2
  local rc=0 b
  for b in "$@"; do
    check_binary "${b}" || rc=1
  done
  exit "${rc}"
}

main "$@"
