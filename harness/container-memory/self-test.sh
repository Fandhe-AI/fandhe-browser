#!/usr/bin/env bash
#
# measure.sh の自己テスト（TASK-67・CTR-3・CTR-7・MEAS-6）。
# 実 docker・実 cgroup・ネットワークは使わない。純関数の検証と、偽 docker シム + 偽の cgroup / proc /
# meminfo ツリーによる全体経路の検証を行う。呼び出し元は scripts/harness.sh の
# check-container-memory（make ci）と measure-container-memory（測定の前に必ず実行）、CI の
# harness-ubuntu `[container-memory-selftest]`。Linux 専用（測定対象が Linux のホスト cgroup のため）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 実 docker を誤って呼ばないよう、source の前に環境から外す（測定の呼び出しは必ずシムを渡す）
unset DOCKER_BIN
# shellcheck source=./measure.sh
# shellcheck disable=SC1091
source "${HERE}/measure.sh"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok: $1"; }
ng() { FAIL=$((FAIL + 1)); echo "NG: $1" >&2; }

# $1=名前 $2=期待 $3=実測
expect_eq() {
  if [ "$2" = "$3" ]; then ok "$1"; else ng "$1 (expected=[$2] actual=[$3])"; fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- 純関数 ---
expect_eq "expand_n_list doubling to max" "2 4 8" "$(expand_n_list 2 8 1)"
expect_eq "expand_n_list no doubling" "2 3" "$(expand_n_list 2,3 8 0)"
expect_eq "expand_n_list doubling stops below max" "10 50 100" "$(expand_n_list 10,50 199 1)"
expect_eq "expand_n_list max below next doubling" "10 50" "$(expand_n_list 10,50 99 1)"

printf 'MemTotal:       1000000 kB\nMemFree: 1 kB\nMemAvailable:    600000 kB\n' >"$TMP/meminfo40"
printf 'MemTotal:       1000000 kB\nMemAvailable:    150000 kB\n' >"$TMP/meminfo85"
expect_eq "mem_used_pct 40" "40.0" "$(mem_used_pct "$TMP/meminfo40")"
expect_eq "mem_used_pct 85" "85.0" "$(mem_used_pct "$TMP/meminfo85")"
expect_eq "mem_used_pct comma locale" "40.0" "$(LC_ALL=de_DE.UTF-8 mem_used_pct "$TMP/meminfo40")"
printf 'MemTotal: 1000 kB\n' >"$TMP/meminfo-bad"
if mem_used_pct "$TMP/meminfo-bad" >/dev/null 2>&1; then ng "mem_used_pct rejects missing MemAvailable"; else ok "mem_used_pct rejects missing MemAvailable"; fi
if pct_ge 80.0 80; then ok "pct_ge equal"; else ng "pct_ge equal"; fi
if pct_ge 79.9 80; then ng "pct_ge below"; else ok "pct_ge below"; fi

printf '12:memory:/legacy\n0::/system.slice/docker-abc123.scope\n' >"$TMP/cg-systemd"
expect_eq "cgroup_rel_path systemd" "/system.slice/docker-abc123.scope" "$(cgroup_rel_path "$TMP/cg-systemd")"
printf '0::/docker/abc123\r\n' >"$TMP/cg-fs"
expect_eq "cgroup_rel_path cgroupfs + CRLF" "/docker/abc123" "$(cgroup_rel_path "$TMP/cg-fs")"
for bad in '0::/' '0::/a/../b' '1:memory:/x' '0::relative'; do
  printf '%s\n' "$bad" >"$TMP/cg-bad"
  if cgroup_rel_path "$TMP/cg-bad" >/dev/null 2>&1; then ng "cgroup_rel_path rejects [$bad]"; else ok "cgroup_rel_path rejects [$bad]"; fi
done

printf 'low 0\nhigh 0\nmax 0\noom 2\noom_kill 3\n' >"$TMP/events"
expect_eq "read_oom_kill value" "3" "$(read_oom_kill "$TMP/events")"
printf 'low 0\n' >"$TMP/events0"
expect_eq "read_oom_kill missing line is 0" "0" "$(read_oom_kill "$TMP/events0")"
printf '123456\n' >"$TMP/val"
expect_eq "read_cg_value" "123456" "$(read_cg_value "$TMP/val")"
printf 'max\n' >"$TMP/valmax"
if read_cg_value "$TMP/valmax" >/dev/null 2>&1; then ng "read_cg_value rejects non-numeric"; else ok "read_cg_value rejects non-numeric"; fi
if [[ "$(now_ms)" =~ ^[0-9]{13,}$ ]]; then ok "now_ms is epoch milliseconds"; else ng "now_ms is epoch milliseconds"; fi

# --- 偽 docker シム ---
SHIM="$TMP/docker-shim"
cat >"$SHIM" <<'SHIMEOF'
#!/usr/bin/env bash
# 偽 docker。呼び出しを calls.log に記録し、環境変数のシナリオに従って応答する。
set -u
S="$SHIM_DIR"
echo "$*" >>"$S/calls.log"
args="$*"
kof() { local h="${1: -8}"; echo $((16#$h)); }
case "$1" in
  version)
    case "$args" in *Server*) echo "27.0.1" ;; *Client*) echo "27.0.2" ;; esac
    ;;
  info)
    case "$args" in *CgroupVersion*) echo "${FAKE_CGV:-2}" ;; *CgroupDriver*) echo "systemd" ;; esac
    ;;
  image)
    ref="${!#}"
    if [ "$1 $2" = "image inspect" ]; then
      if [ -n "${FAKE_NO_IMAGE:-}" ] && [[ "$ref" != *@* ]]; then exit 1; fi
      case "$args" in
        *json*)
          if [[ "$ref" == *@* ]]; then
            echo "{\"Id\":\"sha256:helper\",\"Os\":\"linux\",\"Architecture\":\"amd64\",\"RepoDigests\":[\"${FAKE_HELPER_DIGEST_REF:-$ref}\"]}"
          else
            echo '{"Id":"sha256:imageid0123","RepoDigests":[]}'
          fi
          ;;
        *) echo "sha256:x" ;;
      esac
    fi
    ;;
  pull) exit 0 ;;
  buildx)
    # registry の index（--raw）。linux/amd64 のプラットフォーム別 manifest digest を返す
    pd="${FAKE_PLAT_DIGEST:-sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee}"
    echo "{\"manifests\":[{\"digest\":\"$pd\",\"platform\":{\"os\":\"linux\",\"architecture\":\"amd64\"}}]}"
    ;;
  ps)
    want=""
    for a in "$@"; do case "$a" in label=*) want="${a#label=}" ;; esac; done
    [ -f "$S/containers" ] && awk -v w="$want" '$2 == w { print substr($1, 1, 12) }' "$S/containers"
    exit 0
    ;;
  rm)
    shift
    for id in "$@"; do
      [ "$id" = "-f" ] && continue
      if [ -f "$S/containers" ]; then
        grep -v "^${id}" "$S/containers" >"$S/containers.new" || true
        mv "$S/containers.new" "$S/containers"
      fi
    done
    ;;
  inspect)
    cid="${!#}"
    k=$(kof "$cid")
    case "$args" in
      *State.Pid*) echo $((1000 + k)) ;;
      *State.Running*)
        if [ "$k" = "${OOM_K:-0}" ]; then echo "false true 137"
        elif [ "$k" = "${EXITED_K:-0}" ]; then echo "false false 1"
        else echo "true false 0"; fi
        ;;
    esac
    ;;
  run)
    if [[ " $args " == *" -d "* ]]; then
      n=$(cat "$S/counter"); k=$((n + 1)); echo "$k" >"$S/counter"
      [ "$k" = "${RUN_FAIL_K:-0}" ] && exit 125
      cid=$(printf '%064x' "$k")
      label=""
      for a in "$@"; do case "$a" in fandhe-ctrmem.run=*) label="$a" ;; esac; done
      echo "$cid $label" >>"$S/containers"
      pid=$((1000 + k))
      mkdir -p "$FAKE_PROC/$pid"
      echo "0::/system.slice/docker-$cid.scope" >"$FAKE_PROC/$pid/cgroup"
      if [ "$k" != "${EXITED_K:-0}" ]; then
        d="$FAKE_CG/system.slice/docker-$cid.scope"
        mkdir -p "$d"
        [ -n "${NO_PEAK:-}" ] || echo $((1000 * (((k - 1) % 2) + 1))) >"$d/memory.peak"
        echo 600 >"$d/memory.current"
        oom=0; [ "$k" = "${OOM_K:-0}" ] && oom=1
        printf 'low 0\nhigh 0\nmax 0\noom 0\noom_kill %s\n' "$oom" >"$d/memory.events"
      fi
      echo "$cid"
    else
      net=""
      for a in "$@"; do case "$a" in container:*) net="${a#container:}" ;; esac; done
      k=$(kof "$net")
      if [[ "$args" == *navigate.mjs* ]]; then
        # ナビゲート開始でホストのメモリ使用率が上限を超える状況を模擬する
        [ -z "${NAV_RAISE_MEM:-}" ] || cp "$NAV_RAISE_MEM" "$SHIM_DIR/meminfo"
        sleep "${NAV_DELAY:-0}"
        [ "$k" = "${NAV_FAIL_K:-0}" ] && exit 1
      else
        sleep "${READY_DELAY:-0}"
        [ "$k" = "${READY_FAIL_K:-0}" ] && exit 124
      fi
    fi
    ;;
esac
exit 0
SHIMEOF
chmod +x "$SHIM"

DIGEST="sha256:$(printf '0123456789abcdef%.0s' 1 2 3 4)"
SCENE_N=0
# 新しいシナリオ用の状態ディレクトリを作る（S 以下に cgroup・proc・meminfo の偽ツリーを置く）
new_scene() {
  SCENE_N=$((SCENE_N + 1))
  S="$TMP/scene${SCENE_N}"
  mkdir -p "$S/cg" "$S/proc"
  : >"$S/calls.log"
  : >"$S/containers"
  echo 0 >"$S/counter"
  echo "cpu memory io" >"$S/cg/cgroup.controllers"
  cp "$TMP/meminfo40" "$S/meminfo"
  export SHIM_DIR="$S" FAKE_CG="$S/cg" FAKE_PROC="$S/proc"
  unset FAKE_CGV FAKE_NO_IMAGE FAKE_HELPER_DIGEST_REF FAKE_PLAT_DIGEST NAV_RAISE_MEM RUN_FAIL_K OOM_K EXITED_K NO_PEAK READY_DELAY READY_FAIL_K NAV_DELAY NAV_FAIL_K
}

# $@ = measure.sh の追加引数。終了コードを返し、標準出力を $S/out.json、標準エラーを $S/err に残す
run_measure() {
  [ "$SHIM" != "docker" ] || { echo "refusing to run against a real docker" >&2; exit 1; }
  DOCKER_BIN="$SHIM" bash "${HERE}/measure.sh" \
    --cgroup-root "$S/cg" --proc-root "$S/proc" --meminfo "$S/meminfo" \
    --helper-image node:22-alpine --helper-digest "$DIGEST" --settle 0 "$@" >"$S/out.json" 2>"$S/err"
}

# $1=名前 $2=期待終了コード、残り=measure.sh の引数
expect_rc() {
  local name="$1" want="$2" got=0
  shift 2
  run_measure "$@" || got=$?
  expect_eq "$name" "$want" "$got"
}

# $1=名前 $2=期待値 $3=jq フィルタ（$S/out.json に適用）
expect_jq() {
  expect_eq "$1" "$2" "$(jq -r "$3" "$S/out.json" 2>/dev/null || echo JQ_ERROR)"
}

# --- 入力検証（exit 2） ---
new_scene
expect_rc "usage: unknown option" 2 --bogus
expect_rc "usage: missing value" 2 --n-list
for v in "" 0 010 abc "5,3" "1,1" "1,,2" "1,2,"; do expect_rc "n-list rejects [$v]" 2 --n-list "$v"; done
expect_rc "n-list above max-n" 2 --n-list 10,20 --max-n 15
expect_rc "max-n zero" 2 --max-n 0
for v in 0 3601 030 abc; do expect_rc "ready-timeout rejects [$v]" 2 --n-list 1 --ready-timeout "$v"; done
for v in 0 100 101 080; do expect_rc "host-mem-limit-pct rejects [$v]" 2 --n-list 1 --host-mem-limit-pct "$v"; done
for v in -1 3601 05; do expect_rc "settle rejects [$v]" 2 --n-list 1 --settle "$v"; done
for v in 0 65; do expect_rc "navigate-concurrency rejects [$v]" 2 --n-list 1 --navigate-concurrency "$v"; done
for v in "" foo "idle,idle" "idle,loaded,idle" "idle,"; do expect_rc "conditions rejects [$v]" 2 --n-list 1 --conditions "$v"; done
expect_rc "loaded requires url" 2 --n-list 1 --conditions loaded
for u in "file:///etc/passwd" "http://localhost/" "http://127.0.0.1/" "http://10.0.0.1/" "http://192.168.1.1/" \
  "http://172.16.0.1/" "http://169.254.169.254/" "http://[::1]/" "ftp://example.com/" "https://user@example.com/" "https://a b/"; do
  expect_rc "url rejects [$u]" 2 --n-list 1 --conditions loaded --url "$u"
done
expect_rc "image ref invalid" 2 --n-list 1 --image "a b"
expect_rc "image ref option injection" 2 --n-list 1 --image "-x:y"
expect_rc "digest invalid" 2 --n-list 1 --helper-digest "sha256:zz"
expect_rc "helper image needs tag" 2 --n-list 1 --helper-image node
new_scene
export FAKE_NO_IMAGE=1
expect_rc "image not found" 2 --n-list 1 --conditions idle
new_scene
export FAKE_CGV=1
expect_rc "cgroup v1 rejected" 2 --n-list 1 --conditions idle
new_scene
echo "cpu io" >"$S/cg/cgroup.controllers"
expect_rc "memory controller missing" 2 --n-list 1 --conditions idle
new_scene
export NO_PEAK=1
expect_rc "memory.peak missing" 2 --n-list 1 --conditions idle
expect_eq "memory.peak missing leaves no containers" "0" "$(wc -l <"$S/containers" | tr -d ' ')"
new_scene
WRONG_DIGEST="sha256:$(printf 'f%.0s' $(seq 64))"
export FAKE_HELPER_DIGEST_REF="node@${WRONG_DIGEST}"
expect_rc "helper digest mismatch is fail-closed" 1 --n-list 1 --conditions idle

# docker pull 後は `docker.io/library/node@...` など別表記で RepoDigests に入ることがある（同じ digest なら許容）
new_scene
export FAKE_HELPER_DIGEST_REF="docker.io/library/node@${DIGEST}"
expect_rc "helper digest in another repo form is accepted" 0 --n-list 1 --no-doubling --conditions idle
# プラットフォーム別 manifest digest で保持される場合も許容する（index から引いた digest と一致するとき）
new_scene
PLAT_DIGEST="sha256:$(printf 'c%.0s' $(seq 64))"
export FAKE_HELPER_DIGEST_REF="docker.io/library/node@${PLAT_DIGEST}" FAKE_PLAT_DIGEST="$PLAT_DIGEST"
expect_rc "helper platform digest is accepted" 0 --n-list 1 --no-doubling --conditions idle
new_scene
export FAKE_HELPER_DIGEST_REF="docker.io/library/node@${WRONG_DIGEST}"
expect_rc "unrelated digest is still rejected" 1 --n-list 1 --conditions idle

# --- 正常系（idle） ---
new_scene
printf 'foreign-container label-of-someone-else\n' >"$S/containers.foreign"
echo "aaaaaaaaaaaa foreign=1" >>"$S/containers"
expect_rc "idle run" 0 --n-list 2,3 --no-doubling --conditions idle
expect_jq "idle: schema" "container-memory/v1" '.schema'
expect_jq "idle: stop_reason" "completed" '.conditions[0].stop_reason'
expect_jq "idle: step count" "2" '.conditions[0].steps | length'
expect_jq "idle: n=2 peak sum" "3000" '.conditions[0].steps[0].memory_peak_bytes.sum'
expect_jq "idle: n=2 peak avg" "1500" '.conditions[0].steps[0].memory_peak_bytes.avg'
expect_jq "idle: n=2 peak max" "2000" '.conditions[0].steps[0].memory_peak_bytes.max'
expect_jq "idle: n=2 peak count" "2" '.conditions[0].steps[0].memory_peak_bytes.count'
expect_jq "idle: n=3 peak sum" "4000" '.conditions[0].steps[1].memory_peak_bytes.sum'
expect_jq "idle: n=3 peak avg floors" "1333" '.conditions[0].steps[1].memory_peak_bytes.avg'
expect_jq "idle: n=3 current sum" "1800" '.conditions[0].steps[1].memory_current_bytes.sum'
expect_jq "idle: started" "3" '.conditions[0].steps[1].started'
expect_jq "idle: ready" "3" '.conditions[0].steps[1].ready'
expect_jq "idle: measured" "3" '.conditions[0].steps[1].measured'
expect_jq "idle: complete" "true" '.conditions[0].steps[1].complete'
expect_jq "idle: breakdown total" "0" '.conditions[0].steps[1].breakdowns.total'
expect_jq "idle: host pct recorded" "40.0" '.conditions[0].steps[1].host_mem_used_pct'
expect_jq "idle: ready_timeout_s default" "30" '.args.ready_timeout_s'
expect_jq "idle: host limit default" "80" '.args.host_mem_limit_pct'
expect_jq "idle: helper pinned" "node@${DIGEST}" '.args.helper_image'
expect_jq "idle: image id" "sha256:imageid0123" '.image.id'
expect_jq "idle: kernel" "$(uname -r)" '.environment.kernel'
expect_jq "idle: docker server" "27.0.1" '.environment.docker_server_version'
expect_jq "idle: docker client" "27.0.2" '.environment.docker_client_version'
expect_jq "idle: cgroup version" "2" '.environment.cgroup_version'
expect_jq "idle: cgroup driver" "systemd" '.environment.cgroup_driver'
expect_jq "idle: mem total" "1024000000" '.environment.mem_total_bytes'
expect_jq "idle: ready_time unit is ms" "ms" '.conditions[0].steps[0].ready_time.unit'
expect_jq "idle: only idle condition" "1" '.conditions | length'
expect_eq "idle: docker run -d count" "5" "$(grep -c '^run -d' "$S/calls.log")"
expect_eq "idle: ran with hardening flags" "5" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--cap-drop ALL --security-opt no-new-privileges')"
expect_eq "idle: never publishes ports or host network" "0" "$(grep -cE -- ' -p |--publish|--network host|--net host' "$S/calls.log" || true)"
expect_eq "idle: no --rm on measured containers" "0" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--rm' || true)"
expect_eq "cleanup: only the foreign container remains" "aaaaaaaaaaaa foreign=1" "$(cat "$S/containers")"
expect_eq "cleanup: every ps uses the run label filter" "0" "$(grep '^ps ' "$S/calls.log" | grep -vc -- '--filter label=fandhe-ctrmem.run=' || true)"
expect_eq "cleanup: rm never targets the foreign container" "0" "$(grep '^rm ' "$S/calls.log" | grep -c 'aaaaaaaaaaaa' || true)"
if grep -q '^rm -f ' "$S/calls.log"; then ok "cleanup: rm -f was called"; else ng "cleanup: rm -f was called"; fi

# --- 出力ファイル ---
new_scene
run_measure --n-list 1 --no-doubling --conditions idle --out "$S/result.json" || true
expect_eq "--out writes the file" "container-memory/v1" "$(jq -r .schema "$S/result.json")"
expect_eq "--out leaves stdout empty" "0" "$(wc -c <"$S/out.json" | tr -d ' ')"
expect_eq "--out leaves no temp files" "1" "$(find "$S" -maxdepth 1 -name 'result.json*' | wc -l | tr -d ' ')"

# --- OOM kill ---
new_scene
export OOM_K=3
expect_rc "oom run" 0 --n-list 2,3,5 --no-doubling --conditions idle
expect_jq "oom: stops at breakdown" "breakdown" '.conditions[0].stop_reason'
expect_jq "oom: no step after the breakdown" "2" '.conditions[0].steps | length'
expect_jq "oom: oom_kill counted" "1" '.conditions[0].steps[1].breakdowns.oom_kill'
expect_jq "oom: total" "1" '.conditions[0].steps[1].breakdowns.total'
expect_jq "oom: memory.events recorded" "1" '.conditions[0].steps[1].oom_detail.memory_events_oom_kill_total'
expect_jq "oom: State.OOMKilled recorded" "1" '.conditions[0].steps[1].oom_detail.state_oom_killed_count'

# --- 起動失敗 ---
new_scene
export RUN_FAIL_K=2
expect_rc "start failure run" 0 --n-list 2,4 --no-doubling --conditions idle
expect_jq "start failure: counted" "1" '.conditions[0].steps[0].breakdowns.start_failed'
expect_jq "start failure: started" "1" '.conditions[0].steps[0].started'
expect_jq "start failure: stops" "breakdown" '.conditions[0].stop_reason'
expect_jq "start failure: no more steps" "1" '.conditions[0].steps | length'

# --- 停止（OOM 以外） ---
new_scene
export EXITED_K=2
expect_rc "exited run" 0 --n-list 2 --no-doubling --conditions idle
expect_jq "exited: counted" "1" '.conditions[0].steps[0].breakdowns.exited'
expect_jq "exited: memory excluded from measured" "1" '.conditions[0].steps[0].measured'
expect_jq "exited: stops" "breakdown" '.conditions[0].stop_reason'

# --- 起動待ちの上限超過（CTR-7） ---
new_scene
export READY_DELAY=2
expect_rc "ready timeout run" 0 --n-list 1 --no-doubling --conditions idle --ready-timeout 1
expect_jq "ready timeout: counted" "1" '.conditions[0].steps[0].breakdowns.ready_timeout'
expect_jq "ready timeout: ready excludes it" "0" '.conditions[0].steps[0].ready'
expect_jq "ready timeout: arg recorded" "1" '.args.ready_timeout_s'
new_scene
export READY_FAIL_K=1
expect_rc "ready helper failure run" 0 --n-list 1 --no-doubling --conditions idle
expect_jq "ready helper failure: counted" "1" '.conditions[0].steps[0].breakdowns.ready_timeout'

# --- ホストメモリ上限 ---
new_scene
cp "$TMP/meminfo85" "$S/meminfo"
expect_rc "host limit run" 0 --n-list 2,4 --no-doubling --conditions idle
expect_jq "host limit: stop_reason" "host_memory_limit" '.conditions[0].stop_reason'
expect_jq "host limit: step incomplete" "false" '.conditions[0].steps[0].complete'
expect_jq "host limit: nothing started" "0" '.conditions[0].steps[0].started'
expect_jq "host limit: stops after the first step" "1" '.conditions[0].steps | length'
expect_jq "host limit: empty aggregates are null" "null" '.conditions[0].steps[0].memory_peak_bytes.max'
expect_jq "host limit: empty ready_time max is null" "null" '.conditions[0].steps[0].ready_time.max'
expect_eq "host limit: no container was started" "0" "$(grep -c '^run -d' "$S/calls.log" || true)"
new_scene
cp "$TMP/meminfo85" "$S/meminfo"
expect_rc "host limit raised" 0 --n-list 1 --no-doubling --conditions idle --host-mem-limit-pct 90
expect_jq "host limit raised: completes" "completed" '.conditions[0].stop_reason'

# --- 倍々の増加と max_n ---
new_scene
expect_rc "doubling run" 0 --n-list 1 --max-n 4 --conditions idle
expect_jq "doubling: steps" "1 2 4" '[.conditions[0].steps[].n] | map(tostring) | join(" ")'
expect_jq "doubling: stops at max_n" "max_n" '.conditions[0].stop_reason'
expect_jq "doubling: arg recorded" "true" '.args.doubling'

# --- loaded ---
new_scene
expect_rc "loaded run" 0 --n-list 2 --no-doubling --conditions loaded --url https://example.com/page
expect_jq "loaded: condition" "loaded" '.conditions[0].condition'
expect_jq "loaded: completes" "completed" '.conditions[0].stop_reason'
expect_jq "loaded: url recorded" "https://example.com/page" '.args.url'
expect_eq "loaded: navigate called once per container" "2" "$(grep -c 'navigate.mjs' "$S/calls.log")"
expect_eq "loaded: navigate gets the URL and loopback endpoint" "2" "$(grep 'navigate.mjs' "$S/calls.log" | grep -c 'node /h/navigate.mjs http://127.0.0.1:9333 https://example.com/page$')"
expect_eq "loaded: navigate.mjs mounted read-only" "2" "$(grep 'navigate.mjs' "$S/calls.log" | grep -c 'target=/h/navigate.mjs,readonly')"
expect_eq "loaded: navigate runs in the target's namespace" "2" "$(grep 'navigate.mjs' "$S/calls.log" | grep -c -- '--network container:')"
new_scene
export NAV_RAISE_MEM="$TMP/meminfo85"
expect_rc "navigate host limit run" 0 --n-list 3 --no-doubling --conditions loaded --navigate-concurrency 1 --url https://example.com/
expect_jq "navigate host limit: step incomplete" "false" '.conditions[0].steps[0].complete'
expect_jq "navigate host limit: stop_reason" "host_memory_limit" '.conditions[0].stop_reason'
expect_eq "navigate host limit: navigation aborted after the first batch" "1" "$(grep -c 'navigate.mjs' "$S/calls.log")"
expect_eq "navigate host limit: containers removed" "0" "$(wc -l <"$S/containers" | tr -d ' ')"
new_scene
export NAV_FAIL_K=1
expect_rc "navigate failure run" 0 --n-list 2 --no-doubling --conditions loaded --url https://example.com/
expect_jq "navigate failure: counted" "1" '.conditions[0].steps[0].breakdowns.navigate_failed'
expect_jq "navigate failure: stops" "breakdown" '.conditions[0].stop_reason'
new_scene
expect_rc "both conditions" 0 --n-list 1 --no-doubling --url https://example.com/
expect_jq "both conditions: order" "idle loaded" '[.conditions[].condition] | join(" ")'

# --- 中断時の後始末 ---
new_scene
export READY_DELAY=5
DOCKER_BIN="$SHIM" bash "${HERE}/measure.sh" --cgroup-root "$S/cg" --proc-root "$S/proc" --meminfo "$S/meminfo" \
  --helper-image node:22-alpine --helper-digest "$DIGEST" --settle 0 --n-list 3 --no-doubling --conditions idle \
  >"$S/out.json" 2>"$S/err" &
MPID=$!
for _ in $(seq 50); do
  [ "$(grep -c '^run -d' "$S/calls.log" || true)" -ge 3 ] && break
  sleep 0.1
done
kill -TERM "$MPID"
GOT=0
wait "$MPID" || GOT=$?
if [ "$GOT" -ne 0 ]; then ok "SIGTERM: exits non-zero ($GOT)"; else ng "SIGTERM: exits non-zero"; fi
expect_eq "SIGTERM: containers removed" "0" "$(wc -l <"$S/containers" | tr -d ' ')"
if grep -q '^rm -f ' "$S/calls.log"; then ok "SIGTERM: cleanup called rm -f"; else ng "SIGTERM: cleanup called rm -f"; fi
expect_eq "SIGTERM: no result emitted" "0" "$(wc -c <"$S/out.json" | tr -d ' ')"

# --- navigate.mjs の構文 ---
if command -v node >/dev/null 2>&1; then
  if node --check "${ROOT}/harness/multi-instance-memory/navigate.mjs" 2>/dev/null; then ok "navigate.mjs syntax"; else ng "navigate.mjs syntax"; fi
fi

echo "container-memory self-test: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
