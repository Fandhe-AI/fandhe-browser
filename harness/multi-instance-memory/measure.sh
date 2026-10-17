#!/usr/bin/env bash
#
# 50 インスタンス同時起動時の集約メモリ（PSS）計測（TASK-81・Issue #319、ビヘイビア PERF-4・MEAS-6）。
# fandhe-browser（`fandhe-browser-cli` のサーバー起動）と Chromium ヘッドレスを N 個ずつ起動し、
# 各プロセスツリーの /proc/<pid>/smaps_rollup の Pss を合算して JSON で出力する。
# 計測の実行と判定（Chromium 比 50% 以上削減か）はオーナーが行う。本スクリプトの
# `reduction_pct` と `reference_threshold_met` は参考値で、合否の確定ではない。
#
# Linux 専用（/proc/<pid>/smaps_rollup を使う）。macOS・Windows では終了コード 2 で終わる。
# 依存: bash・ps・awk・curl・sleep・kill・（netns 分離時）unshare・nsenter・ip、
#       （loaded 条件）node 22 以降（navigate.mjs。npm 依存なし。fandhe・Chromium 双方の遷移確認）、
#       （loaded 条件の fandhe 側）root・iptables。
# loaded 条件は公開 URL を使う。fandhe の Page.navigate は loopback・プライベートアドレス・file: を
# SSRF ガードで拒否するため、netns ごとに veth ペア＋ホスト側 NAT（MASQUERADE）で外向き経路を用意する。
# ページ JS（`--page-js v8|boa`）: 既定の fandhe-browser はページ JS 無効（`[js] execute_page_scripts = false`）。
# 指定時はインスタンスごとに固定文面の設定ファイルを $WORK/config/<i>.toml に作り、起動時の環境変数
# FANDHE_BROWSER_CONFIG で渡す（host・netns・外向き経路の 3 経路とも同じ env 前置配列を使う）。
# 呼び出し側の FANDHE_BROWSER_CONFIG は、結果に記録されない設定が混ざるのを防ぐため、設定されていれば
# （空文字でも）fandhe を測る前に終了コード 2 で拒否する。loaded 条件では JS ワーカー子プロセスが各
# インスタンスで起動したこと（environ のマーカー or 回収済み子の cminflt 増加）を確認し、確認できなければ失敗にする。
# 遷移開始から --settle 終了までの PSS 合算を --peak-interval-ms 以上の間隔で標本化し peak_pss_kib に記録する
# （ワーカーは遅延起動でスクリプト後に終了するため、settle 後の pss_kib には映らない）。
# 終了コード: 0 成功 / 1 計測失敗（起動後のプロセス消失・smaps 読み取り不可など）/ 2 使用エラー・前提不足。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# fandhe-browser は待ち受けアドレスが 127.0.0.1:9333 固定で変更手段が無い（server.rs の DEFAULT_ADDR）
FANDHE_PORT=9333
MAX_N=200
# Chromium のリモートデバッグポート（インスタンス i は CHROMIUM_PORT_BASE+i。127.0.0.1 限定）。
# loaded 条件で fandhe と同様に取得成功を CDP で確認するために使う。N<=200 なので 9401..9600
CHROMIUM_PORT_BASE=9400
# 外向き経路（loaded 条件）用のプライベート帯。インスタンス i に 10.213.i.0/30 を割り当てる
# （ホスト側 .1・netns 側 .2）。既存ネットワークと衝突する環境では EGRESS_BASE を書き換える。
EGRESS_BASE="10.213"
EGRESS_DNS="1.1.1.1"

N=50
FANDHE_BIN=""
CHROMIUM_BIN=""
CHROMIUM_EXTRA=""
URL=""
CONDITIONS="idle,loaded"
SETTLE=5
READY_TIMEOUT=60
NET_MODE="auto"
OUT=""
SKIP_CHROMIUM=0
SKIP_FANDHE=0
NO_READY_CHECK=0
ALLOW_SHARED_PORT=0
POC1_CHROMIUM_KIB=""
POC1_FANDHE_KIB=""
EGRESS=0
PAGE_JS=""            # "" | v8 | boa
PAGE_JS_GIVEN=0
PEAK_INTERVAL_MS=50   # ピーク標本化の最小間隔（下限。実際の間隔は peak_max_gap_ms に記録する）

usage() {
  cat <<'USAGE'
usage: measure.sh --fandhe-bin PATH [--chromium-bin PATH | --skip-chromium] [options]

  -n, --instances N            instances per target (1..200, default 50)
  --fandhe-bin PATH            fandhe-browser executable (release build recommended)
  --chromium-bin PATH          Chromium/Chrome executable (headless)
  --chromium-extra-args "..."  extra flags for Chromium (e.g. "--no-sandbox"; split on spaces; recorded in the JSON)
  --skip-chromium              measure fandhe-browser only
  --skip-fandhe                measure Chromium only
  --url URL                    public http(s) URL for the "loaded" condition (required when loaded is
                               selected; loopback/private/file: are rejected by fandhe's SSRF guard)
  --dns IP                     resolver used inside netns for the loaded condition (default 1.1.1.1)
  --conditions LIST            comma list of idle,loaded (default idle,loaded)
  --settle SEC                 wait after start/navigation before sampling (default 5)
  --net-mode auto|netns|host   fandhe network isolation (default auto; see README)
  --poc1-chromium-kib KIB      PoC-1 per-instance PSS of Chromium (positive number), for linear extrapolation
  --poc1-fandhe-kib KIB        PoC-1 per-instance PSS of fandhe-browser (positive number; give both or neither)
  --page-js v8|boa             enable page JS in fandhe-browser via a generated config file (FANDHE_BROWSER_CONFIG);
                               the engine must be compiled into the binary. The caller's FANDHE_BROWSER_CONFIG must be unset
  --peak-interval-ms MS        minimum interval of the peak PSS sampler, 10..1000 (default 50)
  --no-ready-check             skip the /json/version readiness poll (self-test only)
  --allow-shared-port          allow N>1 in host mode (self-test with a fake binary that binds no port)
  --out FILE                   write JSON to FILE (default stdout)
USAGE
}

die() { echo "error: $*" >&2; exit 2; }
log() { echo "[multi-instance-memory] $*" >&2; }

# 127.0.0.1:PORT に待ち受け中のプロセスがあるか（呼び出し元の network namespace で判定）
port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# 値を取る全オプションは値の有無を先に確認する（無いまま shift 2 すると set -e で終了コード 1 になるため）
need_value() { [ "$#" -ge 2 ] || die "option '$1' requires a value"; }
while [ $# -gt 0 ]; do
  case "$1" in
    -n|--instances) need_value "$@"; N="$2"; shift 2 ;;
    --fandhe-bin) need_value "$@"; FANDHE_BIN="$2"; shift 2 ;;
    --chromium-bin) need_value "$@"; CHROMIUM_BIN="$2"; shift 2 ;;
    --chromium-extra-args) need_value "$@"; CHROMIUM_EXTRA="$2"; shift 2 ;;
    --skip-chromium) SKIP_CHROMIUM=1; shift ;;
    --skip-fandhe) SKIP_FANDHE=1; shift ;;
    --url) need_value "$@"; URL="$2"; shift 2 ;;
    --conditions) need_value "$@"; CONDITIONS="$2"; shift 2 ;;
    --settle) need_value "$@"; SETTLE="$2"; shift 2 ;;
    --dns) need_value "$@"; EGRESS_DNS="$2"; shift 2 ;;
    --net-mode) need_value "$@"; NET_MODE="$2"; shift 2 ;;
    --poc1-chromium-kib) need_value "$@"; POC1_CHROMIUM_KIB="$2"; shift 2 ;;
    --poc1-fandhe-kib) need_value "$@"; POC1_FANDHE_KIB="$2"; shift 2 ;;
    --page-js) need_value "$@"; PAGE_JS="$2"; PAGE_JS_GIVEN=1; shift 2 ;;
    --peak-interval-ms) need_value "$@"; PEAK_INTERVAL_MS="$2"; shift 2 ;;
    --no-ready-check) NO_READY_CHECK=1; shift ;;
    --allow-shared-port) ALLOW_SHARED_PORT=1; shift ;;
    --out) need_value "$@"; OUT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

# ---- 入力検証（起動前にすべて確定させる） ----
[ "$(uname -s)" = "Linux" ] && [ -r /proc/self/smaps_rollup ] \
  || die "Linux only: /proc/<pid>/smaps_rollup is required (macOS/Windows are not supported)"
# 数値引数は先頭ゼロなしの十進整数のみ受理する（bash 算術・printf %d は先頭 0 を八進数として扱い、
# 050 が 40、08 が算術エラー、sleep と JSON で値が食い違うため。正規化せず使用エラーにする）
is_uint() { [[ "$1" =~ ^(0|[1-9][0-9]{0,8})$ ]]; }
is_uint "$N" && [ "$N" -ge 1 ] && [ "$N" -le "$MAX_N" ] \
  || die "--instances must be a decimal integer in 1..$MAX_N without leading zeros (got '$N')"
is_uint "$SETTLE" && [ "$SETTLE" -le 3600 ] \
  || die "--settle must be a decimal integer in 0..3600 without leading zeros (got '$SETTLE')"
# 内部の待機上限（自己テスト用の環境変数上書き）も同じ規則で検証する
if [ -n "${MEASURE_READY_TIMEOUT_SEC:-}" ]; then
  is_uint "$MEASURE_READY_TIMEOUT_SEC" && [ "$MEASURE_READY_TIMEOUT_SEC" -ge 1 ] \
    || die "MEASURE_READY_TIMEOUT_SEC must be a positive decimal integer without leading zeros"
  READY_TIMEOUT="$MEASURE_READY_TIMEOUT_SEC"
fi
is_uint "$PEAK_INTERVAL_MS" && [ "$PEAK_INTERVAL_MS" -ge 10 ] && [ "$PEAK_INTERVAL_MS" -le 1000 ] \
  || die "--peak-interval-ms must be a decimal integer in 10..1000 without leading zeros (got '$PEAK_INTERVAL_MS')"
if [ "$PAGE_JS_GIVEN" -eq 1 ]; then
  case "$PAGE_JS" in v8|boa) ;; *) die "--page-js must be v8 or boa" ;; esac
fi
case "$NET_MODE" in auto|netns|host) ;; *) die "--net-mode must be auto, netns or host" ;; esac
# PoC-1 の基準値は PSS 専用で、正の有限値のみ受理する（0・負・非数は除算の分母になるため計測前に拒否）
[ -z "$POC1_CHROMIUM_KIB" ] && [ -z "$POC1_FANDHE_KIB" ] \
  || { [ -n "$POC1_CHROMIUM_KIB" ] && [ -n "$POC1_FANDHE_KIB" ]; } \
  || die "--poc1-chromium-kib and --poc1-fandhe-kib must be given together"
for kib in "$POC1_CHROMIUM_KIB" "$POC1_FANDHE_KIB"; do
  [ -n "$kib" ] || continue
  if ! [[ "$kib" =~ ^(0|[1-9][0-9]{0,14})(\.[0-9]{1,6})?$ ]] || ! awk -v v="$kib" 'BEGIN{ exit !(v + 0 > 0 && v + 0 < 1e15) }'; then
    die "--poc1-*-kib must be a positive decimal number of KiB (PSS) without leading zeros, got '$kib'"
  fi
done
octet='(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])'
[[ "$EGRESS_DNS" =~ ^$octet\.$octet\.$octet\.$octet$ ]] \
  || die "--dns must be a dotted-decimal IPv4 address without leading zeros"
[ "$SKIP_CHROMIUM" -eq 0 ] || [ "$SKIP_FANDHE" -eq 0 ] || die "nothing to measure"
[ "$PAGE_JS_GIVEN" -eq 0 ] || [ "$SKIP_FANDHE" -eq 0 ] || die "--page-js cannot be combined with --skip-fandhe"
if [ "$SKIP_FANDHE" -eq 0 ] && [ -n "${FANDHE_BROWSER_CONFIG+x}" ]; then
  die "FANDHE_BROWSER_CONFIG is set in the caller's environment; unset it (measure.sh passes its own config only with --page-js)"
fi
if [ "$SKIP_FANDHE" -eq 0 ]; then
  [ -n "$FANDHE_BIN" ] && [ -x "$FANDHE_BIN" ] || die "--fandhe-bin must be an executable file"
fi
if [ "$SKIP_CHROMIUM" -eq 0 ]; then
  [ -n "$CHROMIUM_BIN" ] && [ -x "$CHROMIUM_BIN" ] \
    || die "--chromium-bin must be an executable file (or pass --skip-chromium)"
fi
IFS=',' read -r -a COND_LIST <<<"$CONDITIONS"
# 空（--conditions ""）だと検証・計測ループを素通りして空の results を成功扱いで出すため、1 条件以上を必須にする
[ "${#COND_LIST[@]}" -ge 1 ] || die "--conditions must list at least one condition (idle, loaded)"
for c in "${COND_LIST[@]}"; do
  case "$c" in idle) ;; loaded) [ -n "$URL" ] || die "--url is required for the loaded condition" ;;
    *) die "unknown condition: $c" ;; esac
done
# Chromium の後勝ちフラグで、検査したポート・user-data-dir から接続先がずれるのを防ぐ
case " $CHROMIUM_EXTRA " in
  *--remote-debugging-*|*--user-data-dir*) die "--chromium-extra-args must not set --remote-debugging-* or --user-data-dir (measure.sh owns them)" ;;
esac
# 追加引数は 1 回だけ配列に分割し、起動（launch_chromium）と JSON 記録（chromium_extra_args）の双方で
# 同じ配列を使う（記録と実際に渡した引数を一致させる。glob 展開はしない）
CHROMIUM_EXTRA_ARR=()
CHROMIUM_NO_SANDBOX=false
if [ "$SKIP_CHROMIUM" -eq 0 ] && [ -n "$CHROMIUM_EXTRA" ]; then
  read -r -a CHROMIUM_EXTRA_ARR <<<"$CHROMIUM_EXTRA"
  for a in "${CHROMIUM_EXTRA_ARR[@]}"; do
    [ "$a" != "--no-sandbox" ] || CHROMIUM_NO_SANDBOX=true
  done
fi
if [ -n "$URL" ]; then
  case "$URL" in http://*|https://*) ;; *) die "--url must be a public http(s):// URL (file: is rejected by the SSRF guard)" ;; esac
  # 明らかな loopback・プライベートホストは fandhe の SSRF ガードが拒否するため、計測前に弾く
  url_host="${URL#*://}"; url_host="${url_host%%[/?#]*}"; url_host="${url_host##*@}"
  case "$url_host" in
    localhost|localhost:*|*.localhost|*.localhost:*|127.*|10.*|192.168.*|169.254.*|0.*|\[*) \
      die "--url host '$url_host' is loopback/private/IPv6-literal; use a public URL (SSRF guard)" ;;
    172.1[6-9].*|172.2[0-9].*|172.3[01].*) die "--url host '$url_host' is private; use a public URL" ;;
  esac
fi
for tool in ps awk curl sleep kill tr grep; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done
if [ -n "$PAGE_JS" ] && ! printf '%s\n' "${COND_LIST[@]}" | grep -qx loaded; then
  log "warning: --page-js without the loaded condition: no page is loaded, so no JS worker is started or checked"
fi
if printf '%s\n' "${COND_LIST[@]}" | grep -qx loaded && [ "$SKIP_FANDHE" -eq 0 ]; then
  command -v node >/dev/null 2>&1 || die "node (22+) is required for the loaded condition"
fi
if printf '%s\n' "${COND_LIST[@]}" | grep -qx loaded && [ "$SKIP_CHROMIUM" -eq 0 ]; then
  command -v node >/dev/null 2>&1 || die "node (22+) is required for the loaded condition (chromium navigation check)"
fi

# ---- fandhe のネットワーク分離方式 ----
# 固定ポートのため N>1 では各インスタンスを別 network namespace に入れる（unshare -n。root か
# 非特権 user namespace が必要）。N=1 は host のまま起動できる。
NS_FLAGS=()      # unshare 用
NSENTER_FLAGS=() # nsenter 用
resolve_net_mode() {
  [ "$SKIP_FANDHE" -eq 0 ] || return 0
  if [ "$NET_MODE" = "auto" ]; then
    if [ "$N" -eq 1 ]; then NET_MODE="host"; else NET_MODE="netns"; fi
  fi
  if [ "$NET_MODE" = "host" ]; then
    [ "$N" -eq 1 ] || [ "$ALLOW_SHARED_PORT" -eq 1 ] || die "host mode supports only N=1 (fixed port $FANDHE_PORT); use --net-mode netns"
    if port_in_use "$FANDHE_PORT"; then
      die "127.0.0.1:$FANDHE_PORT is already in use; stop the other process first"
    fi
    return 0
  fi
  local t
  for t in unshare nsenter ip; do
    command -v "$t" >/dev/null 2>&1 || die "netns mode needs unshare, nsenter and ip ($t not found)"
  done
  if [ "$(id -u)" -eq 0 ]; then
    NS_FLAGS=(-n); NSENTER_FLAGS=(-n)
  elif unshare -Urn true 2>/dev/null; then
    NS_FLAGS=(-Urn); NSENTER_FLAGS=(-U -n)
  else
    die "netns mode needs root or unprivileged user namespaces (unshare -Urn failed); see README"
  fi
}
resolve_net_mode
# Chromium のデバッグポート 9401..(9400+N) は host の名前空間で待ち受けるため、起動前に全ポートの
# 使用中を拒否する（既存プロセスの CDP へ繋いで別プロセスを遷移させ成功扱いになるのを防ぐ）。
# run_cycle でもサイクル開始時に再検査する。
check_chromium_ports_free() {
  local i
  for ((i = 1; i <= N; i++)); do
    if port_in_use "$((CHROMIUM_PORT_BASE + i))"; then
      echo "error: 127.0.0.1:$((CHROMIUM_PORT_BASE + i)) is already in use; stop the other process first" >&2
      return 2
    fi
  done
}
if [ "$SKIP_CHROMIUM" -eq 0 ]; then check_chromium_ports_free || exit 2; fi
# 外向き経路は loaded 条件で fandhe を測るときのみ必要（veth・iptables・ip_forward は root 必須）
if [ "$SKIP_FANDHE" -eq 0 ] && printf '%s\n' "${COND_LIST[@]}" | grep -qx loaded; then
  EGRESS=1
  [ "$NET_MODE" = "netns" ] || [ "$N" -eq 1 ] \
    || die "loaded condition with N>1 needs netns mode"
  if [ "$NET_MODE" = "netns" ]; then
    [ "$(id -u)" -eq 0 ] || die "loaded condition in netns mode needs root (veth pairs, NAT and ip_forward); idle-only runs can use unprivileged userns"
    command -v iptables >/dev/null 2>&1 || die "iptables not found (needed for NAT in loaded condition)"
  fi
fi

# Chromium は TMPDIR 配下の org.chromium.Chromium.XXXXXX に Unix ソケット（sun_path は 108 バイト上限）を
# 置く。launch_chromium は TMPDIR を $WORK/t/<i> に向けるため、作業ディレクトリの親が長いと上限に近づく。
# 実際の配置は Chromium のバージョン依存で本スクリプトからは確認できないため、使用エラーにはせず警告に留める
if [ "$SKIP_CHROMIUM" -eq 0 ] && [ "$(printf '%s' "${TMPDIR:-/tmp}" | wc -c)" -gt 38 ]; then
  log "warning: TMPDIR is long (over 38 bytes); Chromium's Unix socket path may exceed the 108-byte limit. Use a shorter TMPDIR"
fi
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fandhe-mim.XXXXXX")"
LEADERS=()   # 現サイクルの起動リーダー PID（setsid により pgid == pid）
RESULTS=()   # JSON 断片
declare -A RES_PSS
VETH_HOST_IFS=()     # 作成したホスト側 veth（削除対象）
NAT_RULES=()         # 追加した iptables ルール（"表 チェーン 条件..." 形式。削除時に -D へ置換）
SAMPLER_PID=""       # ピーク標本化プロセス（LEADERS には入れない）
PEAK_FILE=""         # サンプラーの出力ファイル（$WORK 内）
declare -a CMIN0     # 区間開始時の各リーダーの cminflt
PK_PEAK=0; PK_PROCS=0; PK_SAMPLES=0; PK_WINDOW=0; PK_GAP=0
SAMPLER_DIED=0       # stop_peak がサンプラーの早期終了を検出したら 1
JS_WORKER_JSON=""    # 現サイクルの js_worker 断片（空なら出さない）
IP_FORWARD_ORIG=""   # 変更前の net.ipv4.ip_forward（空なら未変更）

# ---- 外向き経路（veth＋NAT）----
# 追加物はすべて下記の teardown_egress で逆順に削除する。ホスト固有の値は持たない。
setup_nat() {
  IP_FORWARD_ORIG="$(cat /proc/sys/net/ipv4/ip_forward)"
  if [ "$IP_FORWARD_ORIG" != "1" ]; then
    log "enabling net.ipv4.ip_forward (restored to $IP_FORWARD_ORIG on exit)"
    echo 1 >/proc/sys/net/ipv4/ip_forward
  else
    IP_FORWARD_ORIG=""
  fi
}
# インスタンスごとの NAT・FORWARD ルール。実際に作成した veth（-i/-o）と当該 /30 に限定し、
# 10.213.0.0/16 全体や他のインターフェースへは効かせない。後始末は teardown_nat が NAT_RULES を逆順に -D する。
add_instance_rules() { # $1=ホスト側 veth 名 $2=サブネット（例 10.213.1.0/30）
  local host_if="$1" sub="$2"
  add_rule nat POSTROUTING -s "$sub" ! -d "$sub" ! -o "$host_if" -j MASQUERADE
  add_rule filter FORWARD -i "$host_if" -s "$sub" -j ACCEPT
  add_rule filter FORWARD -o "$host_if" -d "$sub" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
}
add_rule() { # $1=table $2=chain、以降=条件
  local table="$1" chain="$2"; shift 2
  iptables -t "$table" -I "$chain" "$@" -m comment --comment fandhe-mim
  NAT_RULES+=("$table|$chain|$*")
}
teardown_egress() {
  local i
  for i in "${VETH_HOST_IFS[@]:-}"; do
    [ -n "$i" ] && ip link del "$i" 2>/dev/null || true
  done
  VETH_HOST_IFS=()
}
teardown_nat() {
  local idx spec table chain rest
  for ((idx = ${#NAT_RULES[@]} - 1; idx >= 0; idx--)); do
    spec="${NAT_RULES[$idx]}"; table="${spec%%|*}"; rest="${spec#*|}"; chain="${rest%%|*}"; rest="${rest#*|}"
    # shellcheck disable=SC2086
    iptables -t "$table" -D "$chain" $rest -m comment --comment fandhe-mim 2>/dev/null || true
  done
  NAT_RULES=()
  if [ -n "$IP_FORWARD_ORIG" ]; then echo "$IP_FORWARD_ORIG" >/proc/sys/net/ipv4/ip_forward || true; IP_FORWARD_ORIG=""; fi
}

# ---- 後始末（trap）: 全プロセスグループを停止し作業ディレクトリを削除する ----
kill_all() {
  local pid
  stop_peak
  for pid in "${LEADERS[@]:-}"; do
    [ -n "$pid" ] || continue
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 1
  for pid in "${LEADERS[@]:-}"; do
    [ -n "$pid" ] || continue
    kill -KILL -- "-$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  LEADERS=()
  teardown_egress
}
# 起動失敗の診断（対象プロセスのログ末尾を stderr へ出す）。ログは $WORK 内にあり終了時の cleanup で
# 消えるため、失敗を返す直前（wait_*・navigate_*・sample_cycle）で呼ぶ。Chromium の stderr にはページ由来の
# 文字列が混じりうるため、端末エスケープ（C0 制御文字）を除去し、行数・行長・インスタンス数を制限する。
LOG_TAIL_LINES=20
LOG_TAIL_MAX_INSTANCES=3
LOG_TAIL_MAX_COLS=500
show_log_tail() { # $1=ラベル $2=ログファイル
  local label="$1" logf="$2"
  echo "--- last $LOG_TAIL_LINES lines of $label log ($(basename "$logf")) ---" >&2
  if [ -s "$logf" ]; then
    { tail -n "$LOG_TAIL_LINES" "$logf" 2>/dev/null \
      | LC_ALL=C tr -d '\000-\010\013-\037\177' \
      | cut -c1-"$LOG_TAIL_MAX_COLS" >&2; } || true
  else
    echo "(log is empty)" >&2
  fi
  echo "--- end of $label log ---" >&2
}
show_dead_logs() { # $1=target。リーダーが消失したインスタンスのログ末尾を上限件数まで出す
  local target="$1" i shown=0 dead=0
  for ((i = 1; i <= N; i++)); do
    kill -0 "${LEADERS[$((i - 1))]:-0}" 2>/dev/null && continue
    dead=$((dead + 1))
    if [ "$shown" -lt "$LOG_TAIL_MAX_INSTANCES" ]; then
      shown=$((shown + 1))
      show_log_tail "$target instance $i (pid ${LEADERS[$((i - 1))]:-?})" "$WORK/log-$target-$i.txt"
    fi
  done
  if [ "$dead" -gt "$shown" ]; then
    echo "... and $((dead - shown)) more dead $target instance(s) (logs not shown)" >&2
  fi
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  kill_all
  teardown_nat
  # 削除は自分が mktemp -d で作った $WORK に限る（Chromium の一時ディレクトリも $WORK/t 配下）
  if [ -n "${WORK:-}" ] && [ -d "$WORK" ]; then rm -rf -- "$WORK"; fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$EGRESS" -eq 1 ] && [ "$NET_MODE" = "netns" ]; then setup_nat; fi

# ---- 起動 ----
launch_fandhe() { # $1=index
  local data="$WORK/data/$1" logf="$WORK/log-fandhe-$1.txt"
  # 3 経路共通の環境変数。--page-js 指定時だけ設定ファイルを足す（未指定時は従来と同じ環境）。
  # env・setsid・unshare・sh -c exec はいずれも exec するため、リーダー PID（$!）は変わらない
  local -a lenv=("XDG_DATA_HOME=$data")
  mkdir -p "$data"
  if [ -n "$PAGE_JS" ]; then
    mkdir -p "$WORK/config"
    printf '[js]\nengine = "%s"\nexecute_page_scripts = true\n' "$PAGE_JS" >"$WORK/config/$1.toml"
    lenv+=("FANDHE_BROWSER_CONFIG=$WORK/config/$1.toml")
  fi
  if [ "$NET_MODE" = "netns" ] && [ "$EGRESS" -eq 1 ]; then
    # 外向き経路あり: mount namespace も分け、netns 内だけ resolv.conf を差し替える（ホストの
    # スタブリゾルバ 127.0.0.53 等は netns から到達できないため）。root 前提（setup 時に確認済み）
    printf 'nameserver %s\n' "$EGRESS_DNS" >"$WORK/resolv-$1.conf"
    # sh -c の $0/$1 は後続の引数で、ここでは展開させない
    # shellcheck disable=SC2016
    env "${lenv[@]}" setsid unshare -n -m \
      sh -c 'ip link set lo up && mount --bind "$1" /etc/resolv.conf && exec "$0"' \
      "$FANDHE_BIN" "$WORK/resolv-$1.conf" >"$logf" 2>&1 &
    LEADERS+=("$!")
    attach_egress "$1" "$!"
    return 0
  elif [ "$NET_MODE" = "netns" ]; then
    # sh -c の $0 は後続の引数（fandhe バイナリ）で、ここでは展開させない
    # shellcheck disable=SC2016
    env "${lenv[@]}" setsid unshare "${NS_FLAGS[@]}" \
      sh -c 'ip link set lo up && exec "$0"' "$FANDHE_BIN" >"$logf" 2>&1 &
  else
    env "${lenv[@]}" setsid "$FANDHE_BIN" >"$logf" 2>&1 &
  fi
  LEADERS+=("$!")
}

# 起動済みリーダーの netns へ veth の片側を移し、アドレス・既定ルートを設定する（root）。
# リーダー PID が新しい netns に入る（/proc/PID/ns/net がホストと異なる）のを待ってから移す。
# 待たずに移すとホスト自身の ns に残ってしまうため。
attach_egress() { # $1=index $2=leader pid
  local pid="$2" host_if="fmh$1" peer_if="fmp$1" self_ns deadline
  local host_ip="$EGRESS_BASE.$1.1" peer_ip="$EGRESS_BASE.$1.2"
  self_ns="$(readlink /proc/self/ns/net)"
  deadline=$((SECONDS + 10))
  while [ "$(readlink "/proc/$pid/ns/net" 2>/dev/null)" = "$self_ns" ]; do
    [ "$SECONDS" -lt "$deadline" ] || { echo "error: netns of pid $pid not created" >&2; return 1; }
    sleep 0.05
  done
  ip link add "$host_if" type veth peer name "$peer_if"
  VETH_HOST_IFS+=("$host_if")
  ip link set "$peer_if" netns "$pid"
  ip addr add "$host_ip/30" dev "$host_if"
  ip link set "$host_if" up
  nsenter -t "$pid" -n ip addr add "$peer_ip/30" dev "$peer_if"
  nsenter -t "$pid" -n ip link set "$peer_if" up
  nsenter -t "$pid" -n ip route add default via "$host_ip"
  add_instance_rules "$host_if" "$EGRESS_BASE.$1.0/30"
}

# 常に about:blank で起動する（idle・loaded で起動条件を揃える）。loaded の URL は CDP で遷移させ、
# errorText とロード完了を確認する（launch 引数の URL では取得失敗を検知できないため）。
launch_chromium() { # $1=index
  local i="$1" udir="$WORK/chromium/$1"
  local tdir="$WORK/t/$1"
  # Chromium が TMPDIR 直下に作る org.chromium.Chromium.* を $WORK 内に閉じ込め、終了・失敗・中断の
  # いずれでも cleanup が $WORK ごと消す。測定条件を変えないため、TMPDIR を渡すのは Chromium だけ
  mkdir -p "$udir" "$tdir"
  TMPDIR="$tdir" setsid "$CHROMIUM_BIN" --headless=new --disable-gpu --no-first-run \
    --no-default-browser-check "--user-data-dir=$udir" \
    --remote-debugging-address=127.0.0.1 "--remote-debugging-port=$((CHROMIUM_PORT_BASE + i))" \
    "${CHROMIUM_EXTRA_ARR[@]}" about:blank \
    >"$WORK/log-chromium-$i.txt" 2>&1 &
  LEADERS+=("$!")
}

in_ns() { # $1=leader pid、以降=コマンド。netns 時のみ namespace 内で実行する
  local pid="$1"; shift
  if [ "$NET_MODE" = "netns" ]; then nsenter -t "$pid" "${NSENTER_FLAGS[@]}" "$@"; else "$@"; fi
}

# 127.0.0.1:PORT の LISTEN ソケットが、リーダーのプロセスツリー内のプロセスの所有であることを確認する。
# /proc/<leader>/net/tcp はリーダーの network namespace のテーブルで、inode をツリー内 fd と照合する。
# 応答した相手が既存の別プロセスだった場合（ポート競合）を成功扱いにしないための検査。
listener_owned_by_tree() { # $1=leader $2=port
  local leader="$1" port="$2" hexport inode p fd
  hexport="$(printf '%04X' "$port")"
  inode="$(awk -v lp="0100007F:$hexport" '$2 == lp && $4 == "0A" { print $10; exit }' "/proc/$leader/net/tcp" 2>/dev/null)" || return 1
  [ -n "$inode" ] && [ "$inode" != "0" ] || return 1
  while read -r p; do
    [ -n "$p" ] || continue
    for fd in "/proc/$p"/fd/*; do
      [ "$(readlink "$fd" 2>/dev/null)" = "socket:[$inode]" ] && return 0
    done
  done < <(tree_pids "$leader")
  return 1
}

# CDP の /json/version が応答するまで待ち、応答した待ち受けが今回起動したツリーの所有であることを確認する
wait_cdp_ready() { # $1=label $2=leader $3=port $4=netns 内か(1/0) $5=ログファイル
  local label="$1" pid="$2" port="$3" use_ns="$4" logf="$5" deadline
  deadline=$((SECONDS + READY_TIMEOUT))
  while :; do
    if [ "$use_ns" -eq 1 ]; then
      in_ns "$pid" curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$port/json/version" 2>/dev/null && break
    else
      curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$port/json/version" 2>/dev/null && break
    fi
    kill -0 "$pid" 2>/dev/null || { echo "error: $label (pid $pid) exited during startup" >&2; show_log_tail "$label" "$logf"; return 1; }
    [ "$SECONDS" -lt "$deadline" ] || { echo "error: $label (pid $pid) not ready in ${READY_TIMEOUT}s" >&2; show_log_tail "$label" "$logf"; return 1; }
    sleep 0.2
  done
  listener_owned_by_tree "$pid" "$port" \
    || { echo "error: 127.0.0.1:$port is not owned by $label (pid $pid); another process answered" >&2; return 1; }
}

wait_fandhe_ready() { # 全インスタンスの /json/version 応答と待ち受けの所有を確認する
  [ "$NO_READY_CHECK" -eq 0 ] || return 0
  local i
  for ((i = 1; i <= N; i++)); do
    wait_cdp_ready "fandhe instance $i" "${LEADERS[$((i - 1))]}" "$FANDHE_PORT" 1 "$WORK/log-fandhe-$i.txt" || return 1
  done
}

wait_chromium_ready() { # loaded 条件で全 Chromium インスタンスの CDP 応答と所有を確認する
  local i
  for ((i = 1; i <= N; i++)); do
    wait_cdp_ready "chromium instance $i" "${LEADERS[$((i - 1))]}" "$((CHROMIUM_PORT_BASE + i))" 0 "$WORK/log-chromium-$i.txt" || return 1
  done
}

navigate_all() { # fandhe の全インスタンスで同一 URL を CDP 経由で読み込む
  local i pid
  for ((i = 1; i <= N; i++)); do
    pid="${LEADERS[$((i - 1))]}"
    in_ns "$pid" node "$SCRIPT_DIR/navigate.mjs" "http://127.0.0.1:$FANDHE_PORT" "$URL" >/dev/null \
      || { echo "error: navigation failed for pid $pid" >&2; show_log_tail "fandhe instance $i" "$WORK/log-fandhe-$i.txt"; return 1; }
  done
}

navigate_chromium_all() { # Chromium の全インスタンスで同一 URL を CDP 経由で読み込み、失敗時は中断する
  local i pid
  for ((i = 1; i <= N; i++)); do
    pid="${LEADERS[$((i - 1))]}"
    kill -0 "$pid" 2>/dev/null || { echo "error: chromium instance $i (pid $pid) exited before navigation" >&2; show_log_tail "chromium instance $i" "$WORK/log-chromium-$i.txt"; return 1; }
    node "$SCRIPT_DIR/navigate.mjs" "http://127.0.0.1:$((CHROMIUM_PORT_BASE + i))" "$URL" --page-target >/dev/null \
      || { echo "error: navigation failed for chromium instance $i" >&2; show_log_tail "chromium instance $i" "$WORK/log-chromium-$i.txt"; return 1; }
  done
}

# ---- 計測 ----
# リーダーの子孫と同一プロセスグループの全 PID を列挙する（Chromium の zygote・renderer 等を含む）。
tree_pids() { # $1=leader
  ps -eo pid=,ppid=,pgid= | awk -v root="$1" '
    { pid[NR]=$1; ppid[NR]=$2; pg[NR]=$3; n=NR }
    END {
      set[root]=1
      for (i=1;i<=n;i++) if (pg[i]==root) set[pid[i]]=1
      do { ch=0
        for (i=1;i<=n;i++) if (!(pid[i] in set) && (ppid[i] in set)) { set[pid[i]]=1; ch=1 }
      } while (ch)
      for (p in set) print p
    }'
}

# /proc/<pid>/stat の ")" より後ろ（第 3 フィールド state 以降）を返す。comm に空白・括弧を含みうるため
# 最後の ")" で切る。プロセスが消えていれば失敗
stat_after_comm() { # $1=pid
  local s
  s="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
  printf '%s\n' "${s##*) }"
}
# プロセス状態（R/S/Z 等の 1 文字）。消失なら失敗
proc_state() { # $1=pid
  local rest
  rest="$(stat_after_comm "$1")" || return 1
  printf '%s\n' "${rest%% *}"
}
# 回収済みの子のマイナーフォールト累計 cminflt（stat 第 11 フィールド = state 以降の 9 番目）
proc_cminflt() { # $1=pid
  local rest
  rest="$(stat_after_comm "$1")" || return 1
  # shellcheck disable=SC2086
  set -- $rest
  printf '%s\n' "${9:-0}"
}

# ピーク標本化（バックグラウンド）。ティックごとに ps を 1 回だけ実行し、全リーダーのツリー（子孫＋同一
# プロセスグループ）の PSS を合算して "S <ms> <pss> <procs> <unreadable>" を出す。ゾンビ（回収待ちの終了済み子）
# とティック中に消えたプロセスは数えない。target が fandhe のときは、リーダー以外のツリー内プロセスの
# environ に FANDHE_BROWSER_JS_WORKER キーがあれば "W <instance>" を出す（キーの有無のみ判定し内容は出力しない）。
peak_sampler() { # $1=出力ファイル $2=間隔 ms $3=target、残り=リーダー PID
  trap - EXIT INT TERM   # 親の cleanup（リーダー kill・$WORK 削除）を継承して走らせない
  local out="$1" interval_ms="$2" target="$3" sp=""; shift 3
  local -A confirmed=()
  trap '[ -z "$sp" ] || kill "$sp" 2>/dev/null; exit 0' TERM
  local leaders="$*" ms kind a b c d idx pid
  while :; do
    ms=$((${EPOCHREALTIME/./} / 1000))
    while read -r kind a b c d; do
      case "$kind" in
        S) printf 'S %s %s %s %s\n' "$a" "$b" "$c" "$d" >>"$out" ;;
        C) # 候補: a=instance b=pid
          [ -z "${confirmed[$a]:-}" ] || continue
          if tr '\0' '\n' <"/proc/$b/environ" 2>/dev/null | grep -q '^FANDHE_BROWSER_JS_WORKER='; then
            confirmed[$a]=1; printf 'W %s\n' "$a" >>"$out"
          fi ;;
      esac
    done < <(ps -eo pid=,ppid=,pgid=,stat= | awk -v leaders="$leaders" -v ms="$ms" -v want_c="$([ "$target" = fandhe ] && echo 1 || echo 0)" '
      { pid[NR]=$1; ppid[NR]=$2; pg[NR]=$3; st[NR]=$4; n=NR }
      END {
        nl = split(leaders, L, " ")
        for (k = 1; k <= nl; k++) {
          for (i = 1; i <= n; i++) if (pid[i] == L[k] || pg[i] == L[k]) if (!(pid[i] in idx)) idx[pid[i]] = k
        }
        do { ch = 0
          for (i = 1; i <= n; i++) if (!(pid[i] in idx) && (ppid[i] in idx)) { idx[pid[i]] = idx[ppid[i]]; ch = 1 }
        } while (ch)
        pss = 0; procs = 0; unread = 0
        for (i = 1; i <= n; i++) {
          if (!(pid[i] in idx)) continue
          if (st[i] ~ /^Z/) continue
          f = "/proc/" pid[i] "/smaps_rollup"; got = 0; v = 0
          while ((r = (getline line < f)) > 0) if (line ~ /^Pss:/) { split(line, F, " "); v = F[2]; got = 1 }
          close(f)
          if (got) { pss += v; procs++ }
          else {
            sf = "/proc/" pid[i] "/stat"; ok = ((getline sl < sf) > 0); close(sf)
            if (ok) { sub(/^.*\) /, "", sl); if (substr(sl, 1, 1) != "Z") unread++ }
          }
          if (want_c && pid[i] != L[idx[pid[i]]]) print "C", idx[pid[i]], pid[i]
        }
        print "S", ms, pss, procs, unread
      }')
    sleep "$(awk -v m="$interval_ms" 'BEGIN{printf "%.3f", m/1000}')" &
    sp=$!
    wait "$sp" 2>/dev/null || true
    sp=""
  done
}

# 標本化の開始（fandhe は各リーダーの cminflt 基準値も保存する）。区間: 遷移の開始から --settle の終わりまで
start_peak() { # $1=target
  local target="$1" i
  PEAK_FILE="$WORK/peak-$target.txt"
  : >"$PEAK_FILE"
  CMIN0=()
  if [ "$target" = "fandhe" ]; then
    for ((i = 1; i <= N; i++)); do CMIN0[i]="$(proc_cminflt "${LEADERS[$((i - 1))]}" || echo 0)"; done
  fi
  peak_sampler "$PEAK_FILE" "$PEAK_INTERVAL_MS" "$target" "${LEADERS[@]}" &
  SAMPLER_PID=$!
}
stop_peak() {
  [ -n "$SAMPLER_PID" ] || return 0
  local k
  # settle が 0 でも最低 1 ティックは取る（最大約 2 秒待つ）
  for ((k = 0; k < 40; k++)); do
    grep -q '^S ' "$PEAK_FILE" 2>/dev/null && break
    sleep 0.05
  done
  # サンプラーは無限ループのため、TERM 前に既に終了していれば異常終了（標本が欠けるので summarize_peak で失敗にする）
  kill -0 "$SAMPLER_PID" 2>/dev/null || SAMPLER_DIED=1
  kill -TERM "$SAMPLER_PID" 2>/dev/null || true
  wait "$SAMPLER_PID" 2>/dev/null || true
  SAMPLER_PID=""
}

# 標本化の集計。読めない生存プロセスがあれば失敗（過少計上を成功扱いにしない）
summarize_peak() {
  local line unread
  if [ "$SAMPLER_DIED" -ne 0 ]; then
    SAMPLER_DIED=0
    echo "error: peak sampler exited abnormally before it was stopped" >&2
    return 1
  fi
  line="$(awk '$1=="S" && NF>=5 {
      t++; if (!seen || $3>peak) { peak=$3; procs=$4 } seen=1; unread+=$5
      if (t>1 && $2-prev>gap) gap=$2-prev
      if (t==1) first=$2
      prev=$2 }
    END { printf "%d %d %d %d %d %d", peak, procs, t, (t>0)?prev-first:0, gap, unread }' "$PEAK_FILE" 2>/dev/null)" || { echo "error: peak sample aggregation failed" >&2; return 1; }
  read -r PK_PEAK PK_PROCS PK_SAMPLES PK_WINDOW PK_GAP unread <<<"$line"
  [ "${PK_SAMPLES:-0}" -gt 0 ] || { echo "error: no peak samples were collected" >&2; return 1; }
  [ "$unread" -eq 0 ] || { echo "error: peak sampler found $unread unreadable smaps_rollup reads on live processes" >&2; return 1; }
}

# fandhe/loaded/--page-js: 各インスタンスで JS ワーカーが起動したことを確認する。根拠は
# (a) サンプラーが environ のマーカーを観測 / (b) 区間内に回収した子の cminflt 増加（子の種類は区別しない。
# 現在の fandhe の子プロセスはワーカーのみ）。確認できないインスタンスがあれば失敗
confirm_js_worker() {
  JS_WORKER_JSON=""
  local i cm obs=0 reap=0 ok=0 missing=""
  local -A seen_w=()
  while read -r k a; do [ "$k" = W ] && seen_w[$a]=1; done <"$PEAK_FILE"
  for ((i = 1; i <= N; i++)); do
    local by_obs=0 by_reap=0
    [ -z "${seen_w[$i]:-}" ] || by_obs=1
    cm="$(proc_cminflt "${LEADERS[$((i - 1))]}" || echo 0)"
    [ "$cm" -le "${CMIN0[i]:-0}" ] || by_reap=1
    obs=$((obs + by_obs)); reap=$((reap + by_reap))
    if [ "$by_obs" -eq 1 ] || [ "$by_reap" -eq 1 ]; then ok=$((ok + 1))
    elif [ "$(printf '%s' "$missing" | wc -w)" -lt 20 ]; then missing="$missing $i"
    fi
  done
  if [ "$ok" -ne "$N" ]; then
    echo "error: page JS worker process was not observed for fandhe instance(s):${missing:- (more than 20)} ($ok/$N confirmed)" >&2
    show_dead_logs fandhe
    show_log_tail "fandhe instance 1" "$WORK/log-fandhe-1.txt"
    return 1
  fi
  JS_WORKER_JSON="$(printf ',"js_worker":{"confirmed_instances":%d,"observed_by_sampler":%d,"confirmed_by_reap_counter":%d}' "$ok" "$obs" "$reap")"
}

sample_cycle() { # $1=target $2=condition。結果を RES_* と RESULTS へ
  local target="$1" cond="$2" pss=0 rss=0 procs=0 unreadable=0 lost_leader=0 alive=0 leader leader_read p v
  for leader in "${LEADERS[@]}"; do
    kill -0 "$leader" 2>/dev/null && alive=$((alive + 1))
  done
  [ "$alive" -eq "$N" ] || { echo "error: only $alive/$N $target instances alive before sampling" >&2; show_dead_logs "$target"; return 1; }
  for leader in "${LEADERS[@]}"; do
    # リーダー自身の smaps_rollup 読み取り成功を必須にする（読めない・消失は失敗扱い）
    leader_read=0
    while read -r p; do
      [ -n "$p" ] || continue
      if v="$(awk '/^Pss:/{a=$2} /^Rss:/{b=$2} END{ if (a=="") exit 1; print a, b }' "/proc/$p/smaps_rollup" 2>/dev/null)"; then
        pss=$((pss + ${v%% *})); rss=$((rss + ${v##* })); procs=$((procs + 1))
        [ "$p" != "$leader" ] || leader_read=1
      elif kill -0 "$p" 2>/dev/null && [ "$(proc_state "$p" 2>/dev/null || echo Z)" != "Z" ]; then
        # ゾンビ（終了済みで親の回収待ち）はメモリを持たないため数えない
        unreadable=$((unreadable + 1))   # 生存しているのに読めない（権限）。合算が過少になるため失敗扱い
      fi
    done < <(tree_pids "$leader")
    [ "$leader_read" -eq 1 ] || lost_leader=$((lost_leader + 1))
  done
  [ "$unreadable" -eq 0 ] || { echo "error: $unreadable live processes had unreadable smaps_rollup" >&2; return 1; }
  # 集計中にリーダーが消失するとその PID が黙って除外されるため、終了時にも全リーダーの生存を再確認する
  [ "$lost_leader" -eq 0 ] || { echo "error: $lost_leader $target leader(s) unreadable or gone during sampling" >&2; show_dead_logs "$target"; return 1; }
  alive=0
  for leader in "${LEADERS[@]}"; do
    kill -0 "$leader" 2>/dev/null && alive=$((alive + 1))
  done
  [ "$alive" -eq "$N" ] || { echo "error: only $alive/$N $target instances alive after sampling" >&2; show_dead_logs "$target"; return 1; }
  RES_PSS["$target:$cond"]="$pss"
  # peak_pss_kib は標本化の最大値と settle 後の pss_kib の大きい方（peak >= pss を常に成り立たせる）
  local peak="$PK_PEAK" peak_procs="$PK_PROCS"
  if [ "$pss" -gt "$peak" ]; then peak="$pss"; peak_procs="$procs"; fi
  RESULTS+=("$(printf '{"target":"%s","condition":"%s","instances":%d,"processes":%d,"pss_kib":%d,"rss_kib":%d,"pss_per_instance_kib":%s,"peak_pss_kib":%d,"peak_processes":%d,"peak_samples":%d,"peak_window_ms":%d,"peak_max_gap_ms":%d%s}' \
    "$target" "$cond" "$N" "$procs" "$pss" "$rss" "$(awk -v a="$pss" -v n="$N" 'BEGIN{printf "%.1f", a/n}')" \
    "$peak" "$peak_procs" "$PK_SAMPLES" "$PK_WINDOW" "$PK_GAP" "$JS_WORKER_JSON")")
  log "$target/$cond: processes=$procs pss=${pss}KiB rss=${rss}KiB peak_pss=${peak}KiB (samples=$PK_SAMPLES)"
}

run_cycle() { # $1=target $2=condition
  local target="$1" cond="$2" i
  LEADERS=()
  # host モードの fandhe（固定ポート）と Chromium は、サイクルごとに起動前の使用中を再検査する
  if [ "$target" = "chromium" ]; then check_chromium_ports_free || return 2; fi
  if [ "$target" = "fandhe" ] && [ "$NET_MODE" = "host" ] && port_in_use "$FANDHE_PORT"; then
    echo "error: 127.0.0.1:$FANDHE_PORT is already in use; stop the other process first" >&2
    return 2
  fi
  log "start $N x $target ($cond)"
  for ((i = 1; i <= N; i++)); do
    if [ "$target" = "fandhe" ]; then launch_fandhe "$i"
    else launch_chromium "$i"
    fi
  done
  if [ "$target" = "fandhe" ]; then wait_fandhe_ready; fi
  if [ "$target" = "chromium" ] && [ "$cond" = "loaded" ]; then wait_chromium_ready; fi
  # 標本化の区間: 遷移の開始から --settle の終わりまで（idle は settle のみ）
  start_peak "$target"
  if [ "$target" = "fandhe" ] && [ "$cond" = "loaded" ]; then navigate_all; fi
  if [ "$target" = "chromium" ] && [ "$cond" = "loaded" ]; then navigate_chromium_all; fi
  sleep "$SETTLE"
  stop_peak
  summarize_peak || { kill_all; return 1; }
  JS_WORKER_JSON=""
  if [ "$target" = "fandhe" ] && [ "$cond" = "loaded" ] && [ -n "$PAGE_JS" ]; then confirm_js_worker; fi
  sample_cycle "$target" "$cond"
  kill_all
}

TARGETS=()
[ "$SKIP_FANDHE" -eq 1 ] || TARGETS+=(fandhe)
[ "$SKIP_CHROMIUM" -eq 1 ] || TARGETS+=(chromium)
for cond in "${COND_LIST[@]}"; do
  for target in "${TARGETS[@]}"; do
    run_cycle "$target" "$cond"
  done
done

# ---- 比較（参考値。合否判定はオーナー） ----
COMPARISONS=()
if [ "$SKIP_FANDHE" -eq 0 ] && [ "$SKIP_CHROMIUM" -eq 0 ]; then
  for cond in "${COND_LIST[@]}"; do
    f="${RES_PSS[fandhe:$cond]}"; c="${RES_PSS[chromium:$cond]}"
    COMPARISONS+=("$(awk -v f="$f" -v c="$c" -v cond="$cond" -v n="$N" -v pc="$POC1_CHROMIUM_KIB" -v pf="$POC1_FANDHE_KIB" 'BEGIN{
      red = (c > 0) ? (1 - f / c) * 100 : 0
      printf "{\"condition\":\"%s\",\"fandhe_pss_kib\":%d,\"chromium_pss_kib\":%d,\"reduction_pct\":%.2f,\"reference_threshold_pct\":50,\"reference_threshold_met\":%s", cond, f, c, red, (red >= 50) ? "true" : "false"
      if (pc != "" && pf != "") {
        ec = pc * n; ef = pf * n
        printf ",\"poc1_extrapolation\":{\"chromium_kib\":%.0f,\"fandhe_kib\":%.0f,\"chromium_delta_pct\":%.2f,\"fandhe_delta_pct\":%.2f}", ec, ef, (c - ec) / ec * 100, (f - ef) / ef * 100
      }
      printf "}" }')")
  done
fi

# JSON 文字列として必要なエスケープ（" \ と制御文字 U+0000..U+001F）を行い、元の値を保存する
json_escape() {
  LC_ALL=C JE_IN="$1" awk 'BEGIN {
    for (i = 1; i < 32; i++) esc[sprintf("%c", i)] = sprintf("\\u%04x", i)
    esc["\n"] = "\\n"; esc["\r"] = "\\r"; esc["\t"] = "\\t"; esc["\b"] = "\\b"; esc["\f"] = "\\f"
    esc["\\"] = "\\\\"; esc["\""] = "\\\""
    s = ENVIRON["JE_IN"]; out = ""
    for (i = 1; i <= length(s); i++) { c = substr(s, i, 1); out = out ((c in esc) ? esc[c] : c) }
    printf "%s", out }'
}

join_by() { local IFS=","; echo "$*"; }
# 追加引数の JSON 配列（--skip-chromium では Chromium に何も渡さないため空配列で型を安定させる）
EXTRA_JSON=""
for a in "${CHROMIUM_EXTRA_ARR[@]}"; do
  EXTRA_JSON="${EXTRA_JSON:+$EXTRA_JSON,}\"$(json_escape "$a")\""
done
{
  printf '{"schema_version":1,"task":"TASK-81","behaviors":["PERF-4","MEAS-6"],"metric":"pss_kib",'
  printf '"instances":%s,"net_mode":"%s","url":"%s","settle_sec":%s,' "$N" "${NET_MODE}" "$(json_escape "$URL")" "$SETTLE"
  if [ -n "$PAGE_JS" ]; then printf '"page_js_enabled":true,"page_js_engine":"%s",' "$PAGE_JS"
  else printf '"page_js_enabled":false,"page_js_engine":null,'; fi
  printf '"peak_interval_ms":%s,' "$PEAK_INTERVAL_MS"
  printf '"chromium_extra_args":[%s],"chromium_no_sandbox":%s,' "$EXTRA_JSON" "$CHROMIUM_NO_SANDBOX"
  printf '"environment":{"kernel":"%s","arch":"%s","cpus":%s,"mem_total_kib":%s},' \
    "$(uname -r)" "$(uname -m)" "$(nproc 2>/dev/null || echo 0)" "$(awk '/^MemTotal:/{print $2}' /proc/meminfo)"
  printf '"results":[%s],"comparisons":[%s]}\n' "$(join_by "${RESULTS[@]}")" "$(join_by "${COMPARISONS[@]:-}")"
} >"${OUT:-/dev/stdout}"
