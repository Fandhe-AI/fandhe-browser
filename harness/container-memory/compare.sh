#!/usr/bin/env bash
#
# measure.sh が出す 2 つの結果 JSON（fandhe-browser 側と Chromium 側）から、同じ N・同じ条件での
# メモリ削減率を算出して JSON で出力する（TASK-67・Issue #930・CTR-3・CTR-6・CTR-7・MEAS-6・MS-5）。
# 呼び出し元は人間（実測後の集計）と scripts/harness.sh の compare-container-memory。
# 合否の確定（CTR-3 の 50%・CTR-6 の 90%）は人間が行う。本スクリプトは参考値を出すだけ。
#
# 方針:
#   - 削減率は「両方に実測があり、両方とも完走（complete）・破綻 0・全コンテナ測定済み」の N だけに出す。
#     同じ N の実測が無ければ数値を出さず reason を出す（外挿値を実測として扱わない）。
#   - 実行環境（arch・kernel・メモリ量・cgroup ドライバ）や測定条件（起動待ち・settle・ホスト上限・loaded の URL）が
#     食い違う 2 つの結果は比較できない。comparable=false と理由だけを出し、削減率は 1 件も出さない。
#   - 外挿値（PoC-1 の Chromium 1 インスタンスあたり RSS など）は引数で渡されたときだけ別オブジェクト
#     `extrapolation`（kind=extrapolated）に出し、削減率の計算には使わない。
#   - 削減率 = (1 - fandhe / chromium) * 100 を小数第 2 位で切り捨てる（floor。閾値をまたいで過大に見せない。
#     負の値は負のまま出す）。
#   - 入力 JSON は非信頼データとして jq で型・値域を検査し、値をシェルの文字列へ展開しない。
#
# 依存は bash + jq + coreutils のみ（dependency-policy）。
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
Usage:
  compare.sh --fandhe <result.json> --chromium <result.json> [--out <path>]
             [--poc1-chromium-per-instance-bytes <n>] [--poc8-fandhe-n100-bytes <n>]

  --fandhe        Result of measure.sh --target fandhe (or a #929 result without "target").
  --chromium      Result of measure.sh --target chromium.
  --out           Write the JSON here instead of stdout.
  --poc1-chromium-per-instance-bytes
                  PoC-1 extrapolation input (Chromium per-instance memory; macOS RSS). Reported separately.
  --poc8-fandhe-n100-bytes
                  PoC-8 extrapolation input (fandhe-browser 100 containers total; Docker Desktop arm64). Reported separately.
Exit codes: 0 = compared (comparable or not; see "comparable"), 2 = usage error or invalid input.
USAGE
}

die() {
  echo "error: $*" >&2
  exit 2
}

need_value() {
  [ "$#" -ge 2 ] || die "option $1 requires a value"
}

# 結果 JSON の構造検査。$1=jq に渡す想定 target（fandhe|chromium）。値は出力しない。
# fandhe 側は #929 の出力（target 未定義）も受け付ける。
# shellcheck disable=SC2016 # jq プログラムの $want は jq 変数（シェル展開しない）
VALIDATE_JQ='
  def num: type == "number" and . >= 0 and . == floor;
  def step_ok: (.n | num) and (.n >= 1) and (.complete | type == "boolean") and (.started | num) and (.measured | num)
    and (.breakdowns.total | num) and (.memory_peak_bytes.sum | num) and (.memory_current_bytes.sum | num);
  type == "object" and .schema == "container-memory/v1"
  and ((.target // "fandhe") == $want)
  and (.environment | type == "object")
  and (.args | type == "object")
  and (.conditions | type == "array") and (.conditions | length) > 0
  and ([.conditions[] | (.condition == "idle" or .condition == "loaded") and (.steps | type == "array") and ([.steps[] | step_ok] | all)] | all)
  and ([.conditions[].condition] | (length == (unique | length)))
  and ([.conditions[] | .steps | [.[].n] | (length == (unique | length))] | all)
'

main() {
  local FANDHE="" CHROMIUM="" OUT="" POC1="" POC8=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --fandhe) need_value "$@"; FANDHE="$2"; shift 2 ;;
      --chromium) need_value "$@"; CHROMIUM="$2"; shift 2 ;;
      --out) need_value "$@"; OUT="$2"; shift 2 ;;
      --poc1-chromium-per-instance-bytes) need_value "$@"; POC1="$2"; shift 2 ;;
      --poc8-fandhe-n100-bytes) need_value "$@"; POC8="$2"; shift 2 ;;
      -h | --help) usage; exit 0 ;;
      *) usage; die "unknown option" ;;
    esac
  done

  [ -n "$FANDHE" ] || die "--fandhe is required"
  [ -n "$CHROMIUM" ] || die "--chromium is required"
  local v
  for v in "$POC1" "$POC8"; do
    [ -z "$v" ] || [[ "$v" =~ ^[1-9][0-9]{0,14}$ ]] || die "PoC extrapolation inputs must be positive decimal integers"
  done
  command -v jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"
  [ -r "$FANDHE" ] || die "--fandhe is not readable"
  [ -r "$CHROMIUM" ] || die "--chromium is not readable"

  jq -e --arg want fandhe "$VALIDATE_JQ" "$FANDHE" >/dev/null 2>&1 || die "--fandhe is not a valid container-memory/v1 result for target fandhe"
  jq -e --arg want chromium "$VALIDATE_JQ" "$CHROMIUM" >/dev/null 2>&1 || die "--chromium is not a valid container-memory/v1 result for target chromium"

  local result
  result=$(jq -n --slurpfile f "$FANDHE" --slurpfile c "$CHROMIUM" \
    --argjson poc1 "${POC1:-null}" --argjson poc8 "${POC8:-null}" '
    $f[0] as $f | $c[0] as $c |

    # --- 比較可能性 ---
    def conds($r): [$r.conditions[].condition];
    ([conds($f)[] | select(. as $x | conds($c) | index($x) != null)]) as $common
    | def env_diff($k): if ($f.environment[$k]) == ($c.environment[$k]) then empty else "environment_mismatch:" + $k end;
      def arg_diff($k): if ($f.args[$k]) == ($c.args[$k]) then empty else "args_mismatch:" + $k end;
    ([env_diff("arch"), env_diff("kernel"), env_diff("mem_total_bytes"), env_diff("cgroup_driver"),
      arg_diff("ready_timeout_s"), arg_diff("settle_s"), arg_diff("host_mem_limit_pct"),
      (if ($common | index("loaded")) != null then arg_diff("url") else empty end),
      (if ($common | length) == 0 then "no_common_condition" else empty end)]) as $reasons
    | ($reasons | length == 0) as $comparable

    # --- 1 ステップの要約と完全性 ---
    | def step($r; $cond; $n): [$r.conditions[] | select(.condition == $cond) | .steps[] | select(.n == $n)][0];
      def summary($s): if $s == null then null else
        {n: $s.n, complete: $s.complete, started: $s.started, measured: $s.measured, breakdowns_total: $s.breakdowns.total,
         memory_peak_sum_bytes: $s.memory_peak_bytes.sum, memory_current_sum_bytes: $s.memory_current_bytes.sum} end;
      def full($s): $s.complete == true and $s.breakdowns.total == 0 and $s.measured == $s.n and $s.started == $s.n;
      def why($side; $s): if $s.complete != true then $side + "_step_incomplete"
        elif $s.breakdowns.total > 0 then $side + "_breakdown"
        else $side + "_partial_measurement" end;
      # (1 - f/c) * 100 を小数第 2 位で切り捨て（(c-f)*10000/c の floor。整数除算相当で閾値をまたがない）
      def red($fb; $cb): ((($cb - $fb) * 10000 / $cb) | floor) / 100;

    ([$common[] as $cond
      | ([$f.conditions[] | select(.condition == $cond) | .steps[].n] + [$c.conditions[] | select(.condition == $cond) | .steps[].n]) | unique | .[] as $n
      | step($f; $cond; $n) as $fs | step($c; $cond; $n) as $cs
      | {condition: $cond, n: $n, fandhe: summary($fs), chromium: summary($cs)}
        + (if $comparable | not then {reduction_pct: null, reason: "not_comparable"}
           elif $fs == null then {reduction_pct: null, reason: "no_fandhe_measurement_at_n"}
           elif $cs == null then {reduction_pct: null, reason: "no_chromium_measurement_at_n"}
           elif (full($fs) | not) then {reduction_pct: null, reason: why("fandhe"; $fs)}
           elif (full($cs) | not) then {reduction_pct: null, reason: why("chromium"; $cs)}
           elif ($cs.memory_peak_bytes.sum == 0 or $cs.memory_current_bytes.sum == 0) then {reduction_pct: null, reason: "chromium_zero_bytes"}
           else {reduction_pct: {memory_peak: red($fs.memory_peak_bytes.sum; $cs.memory_peak_bytes.sum),
                                 memory_current: red($fs.memory_current_bytes.sum; $cs.memory_current_bytes.sum)},
                 reason: null} end)]) as $rows

    # --- 参考情報（合否は人間が確定する。N=100 が対象） ---
    | ([{id: "CTR-3", min_reduction_pct: 50}, {id: "CTR-6", min_reduction_pct: 90}]) as $th
    | ([$rows[] | select(.n == 100)
        | . as $r | {condition, n, basis: "memory_peak",
            reduction_pct: (.reduction_pct.memory_peak // null), reason,
            reference_threshold_met: (if .reduction_pct == null then null
              else ([$th[] | {key: .id, value: ($r.reduction_pct.memory_peak >= .min_reduction_pct)}] | from_entries) end)}]) as $refj

    # --- 外挿値（別オブジェクト。reduction_pct の計算には使わない） ---
    | (if $poc1 == null and $poc8 == null then null else
        {kind: "extrapolated",
         note: "Not measurements. Never used for reduction_pct. PoC-1 is macOS RSS (1 instance); PoC-8 is Docker Desktop arm64 memory.peak. Different metric and environment from this run.",
         poc1_chromium: (if $poc1 == null then null else
           {source: "PoC-1", metric: "rss_macos", per_instance_bytes: $poc1,
            rows: [$rows[] | select(.chromium != null and .chromium.complete) | . as $r
              | {condition, n, chromium_extrapolated_bytes: ($poc1 * .n), chromium_measured_peak_sum_bytes: .chromium.memory_peak_sum_bytes,
                 measured_minus_extrapolated_pct: ((((.chromium.memory_peak_sum_bytes - ($poc1 * .n)) * 10000 / ($poc1 * .n)) | round) / 100)}]} end),
         poc8_fandhe: (if $poc8 == null then null else
           {source: "PoC-8", metric: "memory_peak_docker_desktop_arm64", n: 100, fandhe_extrapolated_bytes: $poc8,
            rows: [$rows[] | select(.n == 100 and .fandhe != null and .fandhe.complete) | . as $r
              | {condition, fandhe_measured_peak_sum_bytes: .fandhe.memory_peak_sum_bytes,
                 measured_minus_extrapolated_pct: ((((.fandhe.memory_peak_sum_bytes - $poc8) * 10000 / $poc8) | round) / 100)}]} end)} end) as $ext

    | {schema: "container-memory-compare/v1", task: "TASK-67", behaviors: ["CTR-3", "CTR-6", "CTR-7", "MEAS-6"],
       comparable: $comparable, reasons: $reasons,
       inputs: {fandhe: {image: $f.image.ref, target: ($f.target // "fandhe"), environment: $f.environment,
                         max_ok_n: [$f.conditions[] | {condition, max_ok_n, stop_reason}]},
                chromium: {image: $c.image.ref, pinned_digest: $c.image.pinned_digest, target: $c.target, environment: $c.environment,
                           max_ok_n: [$c.conditions[] | {condition, max_ok_n, stop_reason}], launch: ($c.launch // null)}},
       skipped_conditions: ([conds($f)[], conds($c)[]] | unique | map(select(. as $x | $common | index($x) == null))),
       comparisons: $rows,
       reference_thresholds: {note: "Reference only (N=100, memory.peak). The pass/fail decision belongs to the owner (#306, #308, #319, TASK-69).",
                              CTR_3_min_pct: 50, CTR_6_min_pct: 90, results: $refj},
       extrapolation: $ext}') || die "failed to compute the comparison"

  if [ -n "$OUT" ]; then
    local tmp
    tmp=$(mktemp "${OUT}.XXXXXX") || die "cannot create the output file"
    printf '%s\n' "$result" >"$tmp"
    mv "$tmp" "$OUT"
  else
    printf '%s\n' "$result"
  fi
}

main "$@"
