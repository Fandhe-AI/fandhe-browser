#!/usr/bin/env bash
#
# 2 つのバイナリの cold start（起動からプロセス終了まで）を同一セッションで交互に計測し、
# 中央値・p95 と増分を JSON で出力する（TASK-104・Issue #397、ビヘイビア PERF-7）。
# 想定用途: 「プラグイン登録機構 off」(A) と「on」(B) の 2 構成の比較。計測の実行と判定は
# オーナーが行う。出力の `reference_threshold_met` は参考値で合否の確定ではない。
#
# 依存追加なし。タイマーは bash 5 の EPOCHREALTIME > GNU date の %N > perl (Time::HiRes) の順に
# 選ぶ（macOS の bash 3.2 でも perl が使える）。タイマー方式は JSON の `timer` に記録する。
# 計測値にはシェルの fork/exec とプロセス終了待ちが含まれる。この共通オーバーヘッドは A/B で
# 同じため増分では相殺されるが、絶対値は PoC-9 / PoC-15 の測定方法とは一致しない（README 参照）。
# 終了コード: 0 成功 / 1 計測失敗（対象コマンドが非 0 終了など）/ 2 使用エラー・前提不足。
set -euo pipefail
# awk の printf が LC_NUMERIC（カンマ小数）で不正な JSON を出さないよう C ロケールへ固定する
export LC_ALL=C

N=50
WARMUP=3
BIN_A=""
BIN_B=""
LABEL_A="A"
LABEL_B="B"
THRESHOLD_MS="3"
OUT=""
CMD_ARGS=()

usage() {
  cat <<'USAGE'
usage: measure.sh --bin-a PATH --bin-b PATH [options] [-- ARGS...]

  --bin-a PATH / --bin-b PATH  binaries to compare (A = baseline, B = candidate)
  --label-a NAME / --label-b NAME   labels in the JSON (default A / B)
  -n, --runs N                 measured runs per binary (1..1000, default 50)
  --warmup N                   discarded warm-up runs per binary (0..100, default 3)
  --threshold-ms MS            reference threshold for the median increase (default 3)
  --out FILE                   write JSON to FILE (default stdout)
  -- ARGS...                   arguments passed to both binaries (a one-shot command that exits)
USAGE
}

die() { echo "error: $*" >&2; exit 2; }
need_value() { [ "$2" -ge 2 ] || die "option $1 requires a value"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-a) need_value "$1" "$#"; BIN_A="$2"; shift 2 ;;
    --bin-b) need_value "$1" "$#"; BIN_B="$2"; shift 2 ;;
    --label-a) need_value "$1" "$#"; LABEL_A="$2"; shift 2 ;;
    --label-b) need_value "$1" "$#"; LABEL_B="$2"; shift 2 ;;
    -n|--runs) need_value "$1" "$#"; N="$2"; shift 2 ;;
    --warmup) need_value "$1" "$#"; WARMUP="$2"; shift 2 ;;
    --threshold-ms) need_value "$1" "$#"; THRESHOLD_MS="$2"; shift 2 ;;
    --out) need_value "$1" "$#"; OUT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; CMD_ARGS=("$@"); break ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

# 先頭ゼロは拒否する（bash 算術で八進数扱いになる・JSON 数値として不正になるため）
[[ "$N" =~ ^[1-9][0-9]{0,3}$ ]] && [ "$N" -le 1000 ] || die "--runs must be an integer in 1..1000 without leading zeros"
[[ "$WARMUP" =~ ^(0|[1-9][0-9]{0,2})$ ]] && [ "$WARMUP" -le 100 ] || die "--warmup must be an integer in 0..100 without leading zeros"
[[ "$THRESHOLD_MS" =~ ^(0|[1-9][0-9]*)(\.[0-9]+)?$ ]] || die "--threshold-ms must be a number without leading zeros"
for l in "$LABEL_A" "$LABEL_B"; do
  [[ "$l" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || die "labels must match ^[A-Za-z0-9._-]{1,64}$"
done
for b in "$BIN_A" "$BIN_B"; do
  [ -n "$b" ] && [ -x "$b" ] || die "--bin-a/--bin-b must be executable files (got '$b')"
done
command -v awk >/dev/null 2>&1 || die "awk not found"

# ---- タイマー選択 ----
TIMER=""
if [ -n "${COLD_START_TIMER:-}" ]; then
  # 自己テストで方式を強制するための上書き（epochrealtime・date-ns・perl）
  TIMER="$COLD_START_TIMER"
  case "$TIMER" in epochrealtime|date-ns|perl) ;; *) die "COLD_START_TIMER must be epochrealtime, date-ns or perl" ;; esac
elif [ -n "${EPOCHREALTIME:-}" ]; then
  TIMER="epochrealtime"
elif [ "$(date +%s%N 2>/dev/null || true)" != "" ] && [[ "$(date +%s%N 2>/dev/null)" =~ ^[0-9]{19}$ ]]; then
  TIMER="date-ns"
elif command -v perl >/dev/null 2>&1 && perl -MTime::HiRes -e 1 2>/dev/null; then
  TIMER="perl"
else
  die "no high-resolution timer (need bash 5, GNU date or perl with Time::HiRes)"
fi

# 1 回実行して所要マイクロ秒を標準出力へ。対象コマンドの非 0 終了は失敗（成功を装わない）。
run_once_us() { # $1=binary
  local bin="$1" t0 t1 rc=0 line
  case "$TIMER" in
    epochrealtime)
      t0="${EPOCHREALTIME/[.,]/}"
      "$bin" ${CMD_ARGS[@]+"${CMD_ARGS[@]}"} >/dev/null 2>&1 || rc=$?
      t1="${EPOCHREALTIME/[.,]/}"
      ;;
    date-ns)
      t0="$(date +%s%N)"
      "$bin" ${CMD_ARGS[@]+"${CMD_ARGS[@]}"} >/dev/null 2>&1 || rc=$?
      t1="$(date +%s%N)"
      t0=$((t0 / 1000)); t1=$((t1 / 1000))
      ;;
    perl)
      # perl 自身が前後の時刻を取り fork+exec で対象を実行する（perl の起動時間は計測に含まれない）
      # fork/waitpid の失敗とシグナル終了（$? & 127）も失敗として rc を非 0 にする
      line="$(perl -MTime::HiRes=time -e '
        my $t = time;
        my $pid = fork();
        if (!defined $pid) { print "0 255\n"; exit 0; }
        if (!$pid) { open STDOUT, ">", "/dev/null"; open STDERR, ">&", \*STDOUT; exec { $ARGV[0] } @ARGV; exit 127; }
        my $w = waitpid($pid, 0);
        my $el = (time - $t) * 1e6;
        my $rc = ($w != $pid) ? 255 : ($? & 127) ? 128 + ($? & 127) : ($? >> 8);
        printf "%d %d\n", $el, $rc;' -- "$bin" ${CMD_ARGS[@]+"${CMD_ARGS[@]}"})"
      t0=0; t1="${line%% *}"; rc="${line##* }"
      ;;
  esac
  [ "$rc" -eq 0 ] || { echo "error: $bin exited with status $rc" >&2; return 1; }
  echo $((t1 - t0))
}

# A/B を交互に実行して時間ドリフトを両者に均等に乗せる（同一セッション。PERF-7）
for ((i = 0; i < WARMUP; i++)); do
  run_once_us "$BIN_A" >/dev/null
  run_once_us "$BIN_B" >/dev/null
done
SAMPLES_A=(); SAMPLES_B=()
for ((i = 0; i < N; i++)); do
  # bash 3.2 は配列追加内のコマンド置換の失敗で set -e が止まらないため、一時変数で明示確認する
  sample=""
  sample="$(run_once_us "$BIN_A")" || exit 1
  SAMPLES_A+=("$sample")
  sample="$(run_once_us "$BIN_B")" || exit 1
  SAMPLES_B+=("$sample")
done

# 統計: 中央値・p95（nearest-rank）・最小・最大・平均をミリ秒で出す
stats_json() { # $1=label、標準入力=マイクロ秒の列
  sort -n | awk -v label="$1" '
    { v[NR] = $1 + 0; sum += $1 }
    END {
      n = NR
      med = (n % 2) ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2
      r = int(0.95 * n); if (r < 0.95 * n) r++; if (r < 1) r = 1
      printf "{\"label\":\"%s\",\"runs\":%d,\"median_ms\":%.3f,\"p95_ms\":%.3f,\"min_ms\":%.3f,\"max_ms\":%.3f,\"mean_ms\":%.3f}",
        label, n, med / 1000, v[r] / 1000, v[1] / 1000, v[n] / 1000, sum / n / 1000
    }'
}
JSON_A="$(printf '%s\n' "${SAMPLES_A[@]}" | stats_json "$LABEL_A")"
JSON_B="$(printf '%s\n' "${SAMPLES_B[@]}" | stats_json "$LABEL_B")"

field() { grep -o "\"$2\":[0-9.]*" <<<"$1" | head -1 | cut -d: -f2; }
MED_A="$(field "$JSON_A" median_ms)"; MED_B="$(field "$JSON_B" median_ms)"
P95_A="$(field "$JSON_A" p95_ms)"; P95_B="$(field "$JSON_B" p95_ms)"
COMPARISON="$(awk -v ma="$MED_A" -v mb="$MED_B" -v pa="$P95_A" -v pb="$P95_B" -v th="$THRESHOLD_MS" 'BEGIN {
  d = mb - ma
  printf "{\"median_increase_ms\":%.3f,\"p95_increase_ms\":%.3f,\"reference_threshold_ms\":%s,\"reference_threshold_met\":%s}", d, pb - pa, th, (d <= th) ? "true" : "false"
}')"

# 引数を JSON 文字列へエスケープする（" \ と U+0000-U+001F。C ロケールのバイト単位）
json_escape() { # $1=文字列
  local s="$1" out="" c i code
  for ((i = 0; i < ${#s}; i++)); do
    c="${s:i:1}"
    case "$c" in
      '"') out+='\"' ;;
      "\\") out+="\\\\" ;;
      $'\n') out+='\n' ;;
      $'\t') out+='\t' ;;
      $'\r') out+='\r' ;;
      $'\b') out+='\b' ;;
      $'\f') out+='\f' ;;
      *)
        printf -v code '%d' "'$c"
        if [ "$code" -lt 32 ] && [ "$code" -gt 0 ]; then
          printf -v c '\\u%04x' "$code"
        fi
        out+="$c"
        ;;
    esac
  done
  printf '%s' "$out"
}
CMD_JSON=""
for a in ${CMD_ARGS[@]+"${CMD_ARGS[@]}"}; do
  CMD_JSON+="${CMD_JSON:+,}\"$(json_escape "$a")\""
done

{
  printf '{"schema_version":1,"task":"TASK-104","behaviors":["PERF-7"],"timer":"%s","warmup":%d,"args":[%s],' \
    "$TIMER" "$WARMUP" "$CMD_JSON"
  printf '"environment":{"os":"%s","arch":"%s"},' "$(uname -s)" "$(uname -m)"
  printf '"a":%s,"b":%s,"comparison":%s}\n' "$JSON_A" "$JSON_B" "$COMPARISON"
} >"${OUT:-/dev/stdout}"
