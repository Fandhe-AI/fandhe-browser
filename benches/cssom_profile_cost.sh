#!/usr/bin/env bash
# CSSOM プロファイル（Chrome / Safari）読み込みのバイナリ増分・アイドル RSS 増分を計測する
# （TASK-100.6・Issue #270・`PLUG-8`・MS-8）。
#
# 呼び出し元: Makefile の `measure-cssom-profile-cost`。
# 手法: core の example 2 本（control: cssom_profile 非参照 / profiled: 両プロファイル読み込み）を
# 同じ release ビルドで作り、ファイルサイズ差と、プロセス自身が報告する RSS の差
# （交互実行・中央値）を取る。PoC-16 の計測手法を踏襲した書き直しで、`docs/spec` は読まない。
# 上限: バイナリ増分 10% 以内・アイドル RSS 増分 1.2MiB（= 1228.8KiB）以内。
# RSS 計測は Linux のみ（/proc 由来）。他 OS では偽の値を出さず exit 2 にする（REPAIR-3）。
#
# 使い方:
#   benches/cssom_profile_cost.sh [--trials N] [--out FILE.json] [--strip] [--input SAMPLE.json]
# 終了コード: 0 = 両方上限内 / 1 = いずれか超過（計測値は出力する） / 2 = 使い方・環境・入力の誤り
set -euo pipefail

TRIALS=5
OUT=""
STRIP=0
INPUT=""
SIZE_LIMIT_PERCENT=10
# 1.2MiB を 10 倍した KiB 値（整数演算で比較するため）。
RSS_LIMIT_KIB_X10=12288

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --trials)
      [ $# -ge 2 ] || die "--trials requires a value"
      TRIALS="$2"
      shift 2
      ;;
    --out)
      [ $# -ge 2 ] || die "--out requires a value"
      OUT="$2"
      shift 2
      ;;
    --input)
      [ $# -ge 2 ] || die "--input requires a value"
      INPUT="$2"
      shift 2
      ;;
    --strip)
      STRIP=1
      shift
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

case "$TRIALS" in
  '' | *[!0-9]*) die "--trials must be an integer" ;;
esac
{ [ "$TRIALS" -ge 1 ] && [ "$TRIALS" -le 50 ]; } || die "--trials must be in 1..50"
if [ -n "$OUT" ]; then
  case "$OUT" in
    *.json) ;;
    *) die "--out must end with .json" ;;
  esac
fi
command -v jq >/dev/null 2>&1 || die "jq is required"

SAMPLES_JSON=""
COMMIT="unknown"
RUSTC="unknown"
TARGET_OS="$(uname -s)"
TARGET_ARCH="$(uname -m)"

if [ -n "$INPUT" ]; then
  [ -f "$INPUT" ] || die "input file not found"
  SAMPLES_JSON="$(jq -c '.' "$INPUT")" || die "input is not valid JSON"
else
  [ "$TARGET_OS" = "Linux" ] || die "RSS measurement is only supported on Linux (got $TARGET_OS)"
  command -v cargo >/dev/null 2>&1 || die "cargo is required"
  if [ "$STRIP" -eq 1 ]; then
    export CARGO_PROFILE_RELEASE_STRIP=symbols
  fi
  locked=""
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    locked="--locked"
  fi
  # shellcheck disable=SC2086
  build_json="$(cargo build --release $locked -p fandhe-browser-core \
    --example cssom_profile_cost_control --example cssom_profile_cost_profiled \
    --message-format=json-render-diagnostics)" || die "cargo build failed"
  find_bin() {
    printf '%s\n' "$build_json" | jq -r --arg n "$1" \
      'select(.reason=="compiler-artifact" and .target.name==$n and .executable!=null) | .executable' | tail -n 1
  }
  CONTROL_BIN="$(find_bin cssom_profile_cost_control)"
  PROFILED_BIN="$(find_bin cssom_profile_cost_profiled)"
  { [ -n "$CONTROL_BIN" ] && [ -x "$CONTROL_BIN" ]; } || die "control binary not found"
  { [ -n "$PROFILED_BIN" ] && [ -x "$PROFILED_BIN" ]; } || die "profiled binary not found"
  CONTROL_SIZE="$(wc -c <"$CONTROL_BIN" | tr -d ' ')"
  PROFILED_SIZE="$(wc -c <"$PROFILED_BIN" | tr -d ' ')"

  # 1 回実行し、厳格な形式の出力行から rss_kib だけを取り出す。
  run_probe() {
    local bin="$1" variant="$2" line rss
    line="$("$bin")" || die "probe $variant exited non-zero"
    if [[ "$line" =~ ^cssom-profile-cost:\ variant=${variant}\ rss_kib=([0-9]{1,9})\ profiles=[02]$ ]]; then
      rss="${BASH_REMATCH[1]}"
    else
      die "unexpected probe output for $variant"
    fi
    printf '%s' "$rss"
  }
  c_samples=""
  p_samples=""
  i=0
  while [ "$i" -lt "$TRIALS" ]; do
    c="$(run_probe "$CONTROL_BIN" control)"
    p="$(run_probe "$PROFILED_BIN" profiled)"
    c_samples="${c_samples}${c_samples:+,}${c}"
    p_samples="${p_samples}${p_samples:+,}${p}"
    i=$((i + 1))
  done
  SAMPLES_JSON="$(jq -cn \
    --argjson cs "$CONTROL_SIZE" --argjson ps "$PROFILED_SIZE" \
    --argjson cr "[$c_samples]" --argjson pr "[$p_samples]" \
    '{control:{binarySizeBytes:$cs,rssSamplesKib:$cr},profiled:{binarySizeBytes:$ps,rssSamplesKib:$pr}}')"
  COMMIT="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
  RUSTC="$(rustc -V 2>/dev/null || echo unknown)"
fi

# 入力の検証（型・範囲）。サイズは 1 以上の整数、RSS は 1..10^9 の整数が 1 件以上。
valid="$(printf '%s' "$SAMPLES_JSON" | jq -r '
  def okint(lo; hi): type=="number" and . == floor and . >= lo and . <= hi;
  def okvar: (.binarySizeBytes|okint(1;1e12))
    and (.rssSamplesKib|type=="array" and length>=1 and length<=50 and all(.[]; okint(1;1e9)));
  if (.control|type=="object") and (.profiled|type=="object") and (.control|okvar) and (.profiled|okvar)
  then "ok" else "bad" end' 2>/dev/null || echo bad)"
[ "$valid" = "ok" ] || die "invalid sample data"

# 集計と判定は jq の整数演算で行う（中央値は偶数個なら中央 2 値の平均）。
RESULT="$(printf '%s' "$SAMPLES_JSON" | jq \
  --argjson trials "$TRIALS" --argjson strip "$STRIP" \
  --arg commit "$COMMIT" --arg rustc "$RUSTC" --arg os "$TARGET_OS" --arg arch "$TARGET_ARCH" \
  --argjson sizeLimit "$SIZE_LIMIT_PERCENT" --argjson rssLimitX10 "$RSS_LIMIT_KIB_X10" '
  def median: sort | length as $n
    | if $n % 2 == 1 then .[($n-1)/2] else (.[$n/2-1] + .[$n/2]) / 2 end;
  (.control.rssSamplesKib|median) as $cm
  | (.profiled.rssSamplesKib|median) as $pm
  | (.profiled.binarySizeBytes - .control.binarySizeBytes) as $sd
  | ($pm - $cm) as $rd
  | {
    schemaVersion: 1,
    task: "TASK-100.6",
    behavior: "PLUG-8",
    method: "control (no cssom_profile) vs profiled (load Chrome+Safari) probes, same release build, alternating runs, median RSS; idle = after load with tables retained",
    trials: (.control.rssSamplesKib|length),
    strippedSymbols: ($strip == 1),
    control: {binarySizeBytes: .control.binarySizeBytes, rssSamplesKib: .control.rssSamplesKib, rssMedianKib: $cm},
    profiled: {binarySizeBytes: .profiled.binarySizeBytes, rssSamplesKib: .profiled.rssSamplesKib, rssMedianKib: $pm},
    delta: {
      binarySizeBytes: $sd,
      binarySizePercent: ($sd * 100 / .control.binarySizeBytes * 100 | round / 100),
      rssKib: $rd,
      rssMiB: ($rd / 1024 * 1000 | round / 1000)
    },
    limits: {binarySizePercent: $sizeLimit, rssMiB: 1.2},
    withinLimit: {
      binarySize: ($sd * 100 <= $sizeLimit * .control.binarySizeBytes),
      rss: ($rd * 10 <= $rssLimitX10)
    },
    environment: {commit: $commit, rustc: $rustc, os: $os, arch: $arch}
  }')" || die "aggregation failed"

printf '%s\n' "$RESULT"
if [ -n "$OUT" ]; then
  printf '%s\n' "$RESULT" >"$OUT"
fi

if [ "$(printf '%s' "$RESULT" | jq -r '.withinLimit.binarySize and .withinLimit.rss')" = "true" ]; then
  exit 0
fi
echo "error: limit exceeded (see withinLimit)" >&2
exit 1
