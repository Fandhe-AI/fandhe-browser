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
#       （loaded 条件の fandhe 側）root・iptables・node 22 以降（navigate.mjs。npm 依存なし）。
# loaded 条件は公開 URL を使う。fandhe の Page.navigate は loopback・プライベートアドレス・file: を
# SSRF ガードで拒否するため、netns ごとに veth ペア＋ホスト側 NAT（MASQUERADE）で外向き経路を用意する。
# 終了コード: 0 成功 / 1 計測失敗（起動後のプロセス消失・smaps 読み取り不可など）/ 2 使用エラー・前提不足。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# fandhe-browser は待ち受けアドレスが 127.0.0.1:9333 固定で変更手段が無い（server.rs の DEFAULT_ADDR）
FANDHE_PORT=9333
MAX_N=200
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

usage() {
  cat <<'USAGE'
usage: measure.sh --fandhe-bin PATH [--chromium-bin PATH | --skip-chromium] [options]

  -n, --instances N            instances per target (1..200, default 50)
  --fandhe-bin PATH            fandhe-browser executable (release build recommended)
  --chromium-bin PATH          Chromium/Chrome executable (headless)
  --chromium-extra-args "..."  extra flags for Chromium (e.g. "--no-sandbox"; split on spaces)
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
  --no-ready-check             skip the /json/version readiness poll (self-test only)
  --allow-shared-port          allow N>1 in host mode (self-test with a fake binary that binds no port)
  --out FILE                   write JSON to FILE (default stdout)
USAGE
}

die() { echo "error: $*" >&2; exit 2; }
log() { echo "[multi-instance-memory] $*" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    -n|--instances) N="${2:-}"; shift 2 ;;
    --fandhe-bin) FANDHE_BIN="${2:-}"; shift 2 ;;
    --chromium-bin) CHROMIUM_BIN="${2:-}"; shift 2 ;;
    --chromium-extra-args) CHROMIUM_EXTRA="${2:-}"; shift 2 ;;
    --skip-chromium) SKIP_CHROMIUM=1; shift ;;
    --skip-fandhe) SKIP_FANDHE=1; shift ;;
    --url) URL="${2:-}"; shift 2 ;;
    --conditions) CONDITIONS="${2:-}"; shift 2 ;;
    --settle) SETTLE="${2:-}"; shift 2 ;;
    --dns) EGRESS_DNS="${2:-}"; shift 2 ;;
    --net-mode) NET_MODE="${2:-}"; shift 2 ;;
    --poc1-chromium-kib) POC1_CHROMIUM_KIB="${2:-}"; shift 2 ;;
    --poc1-fandhe-kib) POC1_FANDHE_KIB="${2:-}"; shift 2 ;;
    --no-ready-check) NO_READY_CHECK=1; shift ;;
    --allow-shared-port) ALLOW_SHARED_PORT=1; shift ;;
    --out) OUT="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

# ---- 入力検証（起動前にすべて確定させる） ----
[ "$(uname -s)" = "Linux" ] && [ -r /proc/self/smaps_rollup ] \
  || die "Linux only: /proc/<pid>/smaps_rollup is required (macOS/Windows are not supported)"
[[ "$N" =~ ^[0-9]+$ ]] && [ "$N" -ge 1 ] && [ "$N" -le "$MAX_N" ] \
  || die "--instances must be an integer in 1..$MAX_N (got '$N')"
[[ "$SETTLE" =~ ^[0-9]+$ ]] || die "--settle must be a non-negative integer"
case "$NET_MODE" in auto|netns|host) ;; *) die "--net-mode must be auto, netns or host" ;; esac
# PoC-1 の基準値は PSS 専用で、正の有限値のみ受理する（0・負・非数は除算の分母になるため計測前に拒否）
[ -z "$POC1_CHROMIUM_KIB" ] && [ -z "$POC1_FANDHE_KIB" ] \
  || { [ -n "$POC1_CHROMIUM_KIB" ] && [ -n "$POC1_FANDHE_KIB" ]; } \
  || die "--poc1-chromium-kib and --poc1-fandhe-kib must be given together"
for kib in "$POC1_CHROMIUM_KIB" "$POC1_FANDHE_KIB"; do
  [ -n "$kib" ] || continue
  if ! [[ "$kib" =~ ^[0-9]+(\.[0-9]+)?$ ]] || ! awk -v v="$kib" 'BEGIN{ exit !(v + 0 > 0 && v + 0 < 1e15) }'; then
    die "--poc1-*-kib must be a positive finite number of KiB (PSS), got '$kib'"
  fi
done
[[ "$EGRESS_DNS" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--dns must be an IPv4 address"
[ "$SKIP_CHROMIUM" -eq 0 ] || [ "$SKIP_FANDHE" -eq 0 ] || die "nothing to measure"
if [ "$SKIP_FANDHE" -eq 0 ]; then
  [ -n "$FANDHE_BIN" ] && [ -x "$FANDHE_BIN" ] || die "--fandhe-bin must be an executable file"
fi
if [ "$SKIP_CHROMIUM" -eq 0 ]; then
  [ -n "$CHROMIUM_BIN" ] && [ -x "$CHROMIUM_BIN" ] \
    || die "--chromium-bin must be an executable file (or pass --skip-chromium)"
fi
IFS=',' read -r -a COND_LIST <<<"$CONDITIONS"
for c in "${COND_LIST[@]}"; do
  case "$c" in idle) ;; loaded) [ -n "$URL" ] || die "--url is required for the loaded condition" ;;
    *) die "unknown condition: $c" ;; esac
done
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
for tool in ps awk curl sleep kill; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done
if printf '%s\n' "${COND_LIST[@]}" | grep -qx loaded && [ "$SKIP_FANDHE" -eq 0 ]; then
  command -v node >/dev/null 2>&1 || die "node (22+) is required for the loaded condition"
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
    if (exec 3<>"/dev/tcp/127.0.0.1/$FANDHE_PORT") 2>/dev/null; then
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

WORK="$(mktemp -d "${TMPDIR:-/tmp}/fandhe-mim.XXXXXX")"
LEADERS=()   # 現サイクルの起動リーダー PID（setsid により pgid == pid）
RESULTS=()   # JSON 断片
declare -A RES_PSS
VETH_HOST_IFS=()     # 作成したホスト側 veth（削除対象）
NAT_RULES=()         # 追加した iptables ルール（"表 チェーン 条件..." 形式。削除時に -D へ置換）
IP_FORWARD_ORIG=""   # 変更前の net.ipv4.ip_forward（空なら未変更）

# ---- 外向き経路（veth＋NAT）----
# 追加物はすべて下記の teardown_egress で逆順に削除する。ホスト固有の値は持たない。
setup_nat() {
  local sub="$EGRESS_BASE.0.0/16"
  IP_FORWARD_ORIG="$(cat /proc/sys/net/ipv4/ip_forward)"
  if [ "$IP_FORWARD_ORIG" != "1" ]; then
    log "enabling net.ipv4.ip_forward (restored to $IP_FORWARD_ORIG on exit)"
    echo 1 >/proc/sys/net/ipv4/ip_forward
  else
    IP_FORWARD_ORIG=""
  fi
  add_rule nat POSTROUTING -s "$sub" ! -d "$sub" -j MASQUERADE
  add_rule filter FORWARD -s "$sub" -j ACCEPT
  add_rule filter FORWARD -d "$sub" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
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
cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  kill_all
  teardown_nat
  rm -rf "$WORK"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$EGRESS" -eq 1 ] && [ "$NET_MODE" = "netns" ]; then setup_nat; fi

# ---- 起動 ----
launch_fandhe() { # $1=index
  local data="$WORK/data/$1" logf="$WORK/log-fandhe-$1.txt"
  mkdir -p "$data"
  if [ "$NET_MODE" = "netns" ] && [ "$EGRESS" -eq 1 ]; then
    # 外向き経路あり: mount namespace も分け、netns 内だけ resolv.conf を差し替える（ホストの
    # スタブリゾルバ 127.0.0.53 等は netns から到達できないため）。root 前提（setup 時に確認済み）
    printf 'nameserver %s\n' "$EGRESS_DNS" >"$WORK/resolv-$1.conf"
    # sh -c の $0/$1 は後続の引数で、ここでは展開させない
    # shellcheck disable=SC2016
    XDG_DATA_HOME="$data" setsid unshare -n -m \
      sh -c 'ip link set lo up && mount --bind "$1" /etc/resolv.conf && exec "$0"' \
      "$FANDHE_BIN" "$WORK/resolv-$1.conf" >"$logf" 2>&1 &
    LEADERS+=("$!")
    attach_egress "$1" "$!"
    return 0
  elif [ "$NET_MODE" = "netns" ]; then
    # sh -c の $0 は後続の引数（fandhe バイナリ）で、ここでは展開させない
    # shellcheck disable=SC2016
    XDG_DATA_HOME="$data" setsid unshare "${NS_FLAGS[@]}" \
      sh -c 'ip link set lo up && exec "$0"' "$FANDHE_BIN" >"$logf" 2>&1 &
  else
    XDG_DATA_HOME="$data" setsid "$FANDHE_BIN" >"$logf" 2>&1 &
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
}

launch_chromium() { # $1=index $2=url
  local i="$1" page="$2" udir="$WORK/chromium/$1"
  local -a extra=()
  mkdir -p "$udir"
  if [ -n "$CHROMIUM_EXTRA" ]; then read -r -a extra <<<"$CHROMIUM_EXTRA"; fi
  setsid "$CHROMIUM_BIN" --headless=new --disable-gpu --no-first-run \
    --no-default-browser-check "--user-data-dir=$udir" "${extra[@]}" "$page" \
    >"$WORK/log-chromium-$i.txt" 2>&1 &
  LEADERS+=("$!")
}

in_ns() { # $1=leader pid、以降=コマンド。netns 時のみ namespace 内で実行する
  local pid="$1"; shift
  if [ "$NET_MODE" = "netns" ]; then nsenter -t "$pid" "${NSENTER_FLAGS[@]}" "$@"; else "$@"; fi
}

wait_fandhe_ready() { # 全インスタンスの /json/version 応答を待つ
  [ "$NO_READY_CHECK" -eq 0 ] || return 0
  local pid deadline
  for pid in "${LEADERS[@]}"; do
    deadline=$((SECONDS + READY_TIMEOUT))
    until in_ns "$pid" curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$FANDHE_PORT/json/version" 2>/dev/null; do
      kill -0 "$pid" 2>/dev/null || { echo "error: fandhe instance (pid $pid) exited during startup" >&2; return 1; }
      [ "$SECONDS" -lt "$deadline" ] || { echo "error: fandhe instance (pid $pid) not ready in ${READY_TIMEOUT}s" >&2; return 1; }
      sleep 0.2
    done
  done
}

navigate_all() { # fandhe の全インスタンスで同一 URL を CDP 経由で読み込む
  local pid
  for pid in "${LEADERS[@]}"; do
    in_ns "$pid" node "$SCRIPT_DIR/navigate.mjs" "http://127.0.0.1:$FANDHE_PORT" "$URL" >/dev/null \
      || { echo "error: navigation failed for pid $pid" >&2; return 1; }
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

sample_cycle() { # $1=target $2=condition。結果を RES_* と RESULTS へ
  local target="$1" cond="$2" pss=0 rss=0 procs=0 unreadable=0 alive=0 leader p v
  for leader in "${LEADERS[@]}"; do
    kill -0 "$leader" 2>/dev/null && alive=$((alive + 1))
  done
  [ "$alive" -eq "$N" ] || { echo "error: only $alive/$N $target instances alive before sampling" >&2; return 1; }
  for leader in "${LEADERS[@]}"; do
    while read -r p; do
      [ -n "$p" ] || continue
      if v="$(awk '/^Pss:/{a=$2} /^Rss:/{b=$2} END{ if (a=="") exit 1; print a, b }' "/proc/$p/smaps_rollup" 2>/dev/null)"; then
        pss=$((pss + ${v%% *})); rss=$((rss + ${v##* })); procs=$((procs + 1))
      elif kill -0 "$p" 2>/dev/null; then
        unreadable=$((unreadable + 1))   # 生存しているのに読めない（権限）。合算が過少になるため失敗扱い
      fi
    done < <(tree_pids "$leader")
  done
  [ "$unreadable" -eq 0 ] || { echo "error: $unreadable live processes had unreadable smaps_rollup" >&2; return 1; }
  RES_PSS["$target:$cond"]="$pss"
  RESULTS+=("$(printf '{"target":"%s","condition":"%s","instances":%d,"processes":%d,"pss_kib":%d,"rss_kib":%d,"pss_per_instance_kib":%s}' \
    "$target" "$cond" "$N" "$procs" "$pss" "$rss" "$(awk -v a="$pss" -v n="$N" 'BEGIN{printf "%.1f", a/n}')")")
  log "$target/$cond: processes=$procs pss=${pss}KiB rss=${rss}KiB"
}

run_cycle() { # $1=target $2=condition
  local target="$1" cond="$2" i page="about:blank"
  LEADERS=()
  log "start $N x $target ($cond)"
  for ((i = 1; i <= N; i++)); do
    if [ "$target" = "fandhe" ]; then launch_fandhe "$i"
    else
      [ "$cond" != "loaded" ] || page="$URL"
      launch_chromium "$i" "$page"
    fi
  done
  if [ "$target" = "fandhe" ]; then wait_fandhe_ready; fi
  if [ "$target" = "fandhe" ] && [ "$cond" = "loaded" ]; then navigate_all; fi
  sleep "$SETTLE"
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
{
  printf '{"schema_version":1,"task":"TASK-81","behaviors":["PERF-4","MEAS-6"],"metric":"pss_kib",'
  printf '"instances":%d,"net_mode":"%s","url":"%s","settle_sec":%d,' "$N" "${NET_MODE}" "$(json_escape "$URL")" "$SETTLE"
  printf '"environment":{"kernel":"%s","arch":"%s","cpus":%s,"mem_total_kib":%s},' \
    "$(uname -r)" "$(uname -m)" "$(nproc 2>/dev/null || echo 0)" "$(awk '/^MemTotal:/{print $2}' /proc/meminfo)"
  printf '"results":[%s],"comparisons":[%s]}\n' "$(join_by "${RESULTS[@]}")" "$(join_by "${COMPARISONS[@]:-}")"
} >"${OUT:-/dev/stdout}"
