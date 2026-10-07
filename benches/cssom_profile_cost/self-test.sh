#!/usr/bin/env bash
# benches/cssom_profile_cost.sh の集計・判定経路の自己テスト（TASK-100.6・Issue #270・`PLUG-8`）。
# 呼び出し元: Makefile の `measure-cssom-profile-cost`。合成サンプルを `--input` で渡し、
# 中央値・差分・上限判定・終了コードを具体値で検証する（ビルド・実行は伴わない）。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/../cssom_profile_cost.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail=0
check() { # check <name> <cond-exit-code>
  if [ "$2" -eq 0 ]; then echo "ok: $1"; else echo "NG: $1" >&2; fail=1; fi
}

# sample <file> <csize> <psize> <crss-json-array> <prss-json-array>
sample() {
  printf '{"control":{"binarySizeBytes":%s,"rssSamplesKib":%s},"profiled":{"binarySizeBytes":%s,"rssSamplesKib":%s}}\n' \
    "$2" "$4" "$3" "$5" >"$1"
}

# run <expected-exit> <name> <args...>
run() {
  local want="$1" name="$2" got=0
  shift 2
  bash "$script" "$@" >"$tmp/out.json" 2>/dev/null || got=$?
  [ "$got" -eq "$want" ]
  check "$name (exit $want)" $?
}

jqeq() { # jqeq <name> <filter> <expected>
  [ "$(jq -r "$2" "$tmp/out.json")" = "$3" ]
  check "$1" $?
}

# 上限内: +3.00%、RSS 中央値差 144KiB
sample "$tmp/ok.json" 2000000 2060000 "[1000,1100,1200,1000,1000]" "[1144,1144,1144,1300,1100]"
run 0 within-limit --input "$tmp/ok.json"
jqeq "size delta" '.delta.binarySizeBytes' 60000
jqeq "size percent" '.delta.binarySizePercent' 3
jqeq "rss median control" '.control.rssMedianKib' 1000
jqeq "rss median profiled" '.profiled.rssMedianKib' 1144
jqeq "rss delta" '.delta.rssKib' 144
jqeq "within both" '.withinLimit.binarySize and .withinLimit.rss' true

# 境界: ちょうど 10% / ちょうど 1228.8KiB 相当（差 1228.8 は整数にならないため 1228 は合格・1229 は超過）
sample "$tmp/b1.json" 2000000 2200000 "[1000]" "[2228]"
run 0 boundary-ok --input "$tmp/b1.json"
sample "$tmp/b2.json" 2000000 2200001 "[1000]" "[2229]"
run 1 boundary-over --input "$tmp/b2.json"
jqeq "size flag false" '.withinLimit.binarySize' false
jqeq "rss flag false" '.withinLimit.rss' false

# RSS のみ超過 / サイズのみ超過
sample "$tmp/r.json" 2000000 2010000 "[1000]" "[3000]"
run 1 rss-only --input "$tmp/r.json"
jqeq "rss-only: size true" '.withinLimit.binarySize' true
jqeq "rss-only: rss false" '.withinLimit.rss' false
sample "$tmp/s.json" 2000000 2300000 "[1000]" "[1000]"
run 1 size-only --input "$tmp/s.json"
jqeq "size-only: size false" '.withinLimit.binarySize' false
jqeq "size-only: rss true" '.withinLimit.rss' true

# 偶数個の中央値・負の差分をそのまま記録する
sample "$tmp/n.json" 2000000 1990000 "[1000,1200,1100,1300]" "[900,1000,1100,1000]"
run 0 negative-delta --input "$tmp/n.json"
jqeq "even median control" '.control.rssMedianKib' 1150
jqeq "even median profiled" '.profiled.rssMedianKib' 1000
jqeq "negative size delta" '.delta.binarySizeBytes' -10000
jqeq "negative rss delta" '.delta.rssKib' -150

# 不正入力・不正引数は exit 2
sample "$tmp/neg.json" 2000000 2060000 "[-1]" "[1000]"
run 2 negative-sample --input "$tmp/neg.json"
sample "$tmp/str.json" 2000000 2060000 '["x"]' "[1000]"
run 2 non-numeric-sample --input "$tmp/str.json"
sample "$tmp/empty.json" 2000000 2060000 "[]" "[1000]"
run 2 empty-samples --input "$tmp/empty.json"
printf 'not json' >"$tmp/bad.json"
run 2 invalid-json --input "$tmp/bad.json"
run 2 missing-input --input "$tmp/none.json"
run 2 trials-zero --trials 0 --input "$tmp/ok.json"
run 2 trials-too-many --trials 51 --input "$tmp/ok.json"
run 2 trials-non-numeric --trials abc --input "$tmp/ok.json"
run 2 out-not-json --out "$tmp/x.txt" --input "$tmp/ok.json"
run 2 unknown-arg --bogus

exit "$fail"
