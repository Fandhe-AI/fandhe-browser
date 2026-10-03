#!/usr/bin/env bash
#
# measure-js-engine-configs.sh の自己テスト（TASK-31.1・#469）。ビルドは行わず、
# 合成した cargo JSON・fixture workspace で判定ロジックを検証する（高速）。
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT="$HERE/measure-js-engine-configs.sh"
# shellcheck source=/dev/null
source "$SCRIPT"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
FAILS=0

check() { # $1=name $2=expected $3=actual
  if [ "$2" = "$3" ]; then echo "ok: $1"; else echo "FAIL: $1 (expected='$2' actual='$3')" >&2; FAILS=$((FAILS + 1)); fi
}

art() { # $1=crate name $2=kind
  printf '{"reason":"compiler-artifact","target":{"name":"%s","kind":["%s"]},"executable":null}\n' "$1" "$2"
}

check "v8 only" "v8" "$(art v8 lib | observed_engines)"
check "boa only" "boa" "$(art boa_engine lib | observed_engines)"
check "neither" "none" "$(art serde lib | observed_engines)"
check "both" "v8+boa" "$( { art v8 lib; art boa_engine lib; } | observed_engines)"
check "myv8 not detected" "none" "$(art myv8 lib | observed_engines)"
check "v8 build script ignored" "none" "$(art v8 custom-build | observed_engines)"
check "empty input" "none" "$(printf '' | observed_engines)"

check "mb 42920000" "42.92" "$(bytes_to_mb 42920000)"
check "mb 999999" "1.00" "$(bytes_to_mb 999999)"
check "mb 0" "0.00" "$(bytes_to_mb 0)"
check "mb locale independent" "42.92" "$(LC_ALL=de_DE.UTF-8 LC_NUMERIC=de_DE.UTF-8 bytes_to_mb 42920000)"

check "default expects v8" "v8" "$(config_expected_engines default)"
check "boa expects boa" "boa" "$(config_expected_engines boa)"
check "none expects none" "none" "$(config_expected_engines none)"
check "boa args" "--no-default-features --features js-boa" "$(config_cargo_args boa)"
check "none args" "--no-default-features" "$(config_cargo_args none)"
check "default args empty" "" "$(config_cargo_args default)"

rc() { set +e; "$@" >/dev/null 2>&1; local r=$?; set -e; echo "$r"; }
check "label rejects newline" "2" "$(rc bash -c "source '$SCRIPT'; require_label x \$'a\nb'")"
check "label rejects ::" "2" "$(rc bash -c "source '$SCRIPT'; require_label x 'a::b'")"
check "label rejects space" "2" "$(rc bash -c "source '$SCRIPT'; require_label x 'a b'")"
check "label accepts triple" "0" "$(rc bash -c "source '$SCRIPT'; require_label x x86_64-unknown-linux-gnu")"
check "unknown arg exits 2" "2" "$(rc bash "$SCRIPT" --bogus)"
check "bad package exits 2" "2" "$(rc bash "$SCRIPT" --package 'a b')"

# cli が存在しない孤立 workspace では exit 2（fail-closed）。実ビルドはしない。
mkdir -p "$TMP/ws"
printf '[workspace]\nmembers = []\nresolver = "2"\n' >"$TMP/ws/Cargo.toml"
check "missing package exits 2" "2" "$(cd "$TMP/ws" && rc bash "$SCRIPT" --package fandhe-browser-cli)"

if [ "$FAILS" -ne 0 ]; then echo "$FAILS check(s) failed" >&2; exit 1; fi
echo "all ok"
