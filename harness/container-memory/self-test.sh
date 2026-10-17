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
          if [[ "$ref" == mcr.microsoft.com/*@* ]]; then
            echo "{\"Id\":\"sha256:refid\",\"Os\":\"linux\",\"Architecture\":\"amd64\",\"RepoDigests\":[\"${FAKE_REF_DIGEST_REF:-$ref}\"]}"
          elif [[ "$ref" == *@* ]]; then
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
    if [[ "$args" == *"--entrypoint sh "* ]]; then
      # Chromium バイナリのパス解決（FAKE_CHROME_PATHS は改行区切り。未設定は 1 行の正常値）
      printf '%b\n' "${FAKE_CHROME_PATHS-/ms-playwright/chromium-1148/chrome-linux/chrome}"
      exit 0
    fi
    if [[ " $args " == *" -d "* ]]; then
      n=$(cat "$S/counter"); k=$((n + 1)); echo "$k" >"$S/counter"
      [ "$k" != "${RUN_RAISE_MEM_K:-0}" ] || cp "$RUN_RAISE_MEM_FILE" "$S/meminfo"
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
REF_DIGEST="sha256:$(printf 'abcdef0123456789%.0s' 1 2 3 4)"
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
  unset FAKE_CGV FAKE_NO_IMAGE FAKE_HELPER_DIGEST_REF FAKE_PLAT_DIGEST NAV_RAISE_MEM RUN_FAIL_K OOM_K EXITED_K NO_PEAK READY_DELAY READY_FAIL_K NAV_DELAY NAV_FAIL_K FAKE_REF_DIGEST_REF FAKE_CHROME_PATHS RUN_RAISE_MEM_K RUN_RAISE_MEM_FILE
}

# $@ = measure.sh の追加引数。終了コードを返し、標準出力を $S/out.json、標準エラーを $S/err に残す
run_measure() {
  [ "$SHIM" != "docker" ] || { echo "refusing to run against a real docker" >&2; exit 1; }
  DOCKER_BIN="$SHIM" bash "${HERE}/measure.sh" \
    --cgroup-root "$S/cg" --proc-root "$S/proc" --meminfo "$S/meminfo" \
    --helper-image node:22-alpine --helper-digest "$DIGEST" \
    --reference-image mcr.microsoft.com/playwright:v1.49.0-noble --reference-digest "$REF_DIGEST" \
    --settle 0 "$@" >"$S/out.json" 2>"$S/err"
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

# --- fandhe の回帰（既定 target の起動引数は #929 から変えない） ---
new_scene
expect_rc "fandhe target run" 0 --n-list 2 --no-doubling --conditions loaded --url https://example.com/
expect_jq "fandhe: target" "fandhe" '.target'
expect_jq "fandhe: max_ok_n" "2" '.conditions[0].max_ok_n'
expect_jq "fandhe: launch entrypoint null" "null" '.launch.entrypoint'
expect_jq "fandhe: pinned_digest null" "null" '.image.pinned_digest'
expect_jq "fandhe: CTR-6 listed" "true" '.behaviors | index("CTR-6") != null'
expect_eq "fandhe: no --entrypoint/--no-sandbox/--init" "0" "$(grep '^run -d' "$S/calls.log" | grep -cE -- '--entrypoint|--no-sandbox|--init' || true)"
expect_eq "fandhe: navigate has no --page-target" "0" "$(grep 'navigate.mjs' "$S/calls.log" | grep -c -- '--page-target' || true)"
expect_eq "fandhe: no path resolution" "0" "$(grep -c -- '--entrypoint sh' "$S/calls.log" || true)"

# --- chromium target ---
new_scene
expect_rc "chromium run" 0 --target chromium --n-list 2 --no-doubling --conditions idle,loaded --url https://example.com/p
REFPIN="mcr.microsoft.com/playwright@${REF_DIGEST}"
expect_jq "chromium: target" "chromium" '.target'
expect_jq "chromium: pinned digest" "$REFPIN" '.image.pinned_digest'
expect_jq "chromium: image ref is the pin" "$REFPIN" '.image.ref'
expect_jq "chromium: launch entrypoint" "/ms-playwright/chromium-1148/chrome-linux/chrome" '.launch.entrypoint'
expect_jq "chromium: launch user" "pwuser" '.launch.user'
expect_jq "chromium: launch init" "true" '.launch.init'
expect_jq "chromium: launch args has no-sandbox" "true" '.launch.args | index("--no-sandbox") != null'
expect_jq "chromium: both conditions complete" "completed completed" '[.conditions[].stop_reason] | join(" ")'
expect_jq "chromium: max_ok_n idle" "2" '.conditions[0].max_ok_n'
expect_jq "chromium: max_ok_n loaded" "2" '.conditions[1].max_ok_n'
expect_eq "chromium: run -d count" "4" "$(grep -c '^run -d' "$S/calls.log")"
expect_eq "chromium: entrypoint" "4" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--entrypoint /ms-playwright/chromium-1148/chrome-linux/chrome')"
expect_eq "chromium: user pwuser" "4" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--user pwuser')"
expect_eq "chromium: init" "4" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--init')"
expect_eq "chromium: no-sandbox and CDP port 9333" "4" "$(grep '^run -d' "$S/calls.log" | grep -- '--no-sandbox' | grep -c -- '--remote-debugging-port=9333')"
expect_eq "chromium: loopback CDP address" "4" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--remote-debugging-address=127.0.0.1')"
expect_eq "chromium: hardening kept" "4" "$(grep '^run -d' "$S/calls.log" | grep -c -- '--cap-drop ALL --security-opt no-new-privileges')"
expect_eq "chromium: image is repo@digest" "4" "$(grep '^run -d' "$S/calls.log" | grep -c -- " ${REFPIN} ")"
expect_eq "chromium: never publishes ports or host network" "0" "$(grep -cE -- ' -p |--publish|--network host|--net host' "$S/calls.log" || true)"
expect_eq "chromium: navigate has --page-target" "2" "$(grep 'navigate.mjs' "$S/calls.log" | grep -c -- 'http://127.0.0.1:9333 https://example.com/p --page-target$')"
expect_eq "chromium: path resolved once with fixed command" "1" "$(grep -c -- "--entrypoint sh .* -c ls -d /ms-playwright/chromium-\[0-9\]\*/chrome-linux/chrome" "$S/calls.log")"
expect_eq "chromium: path resolution is sandboxed" "1" "$(grep -- '--entrypoint sh' "$S/calls.log" | grep -c -- '--network none --cap-drop ALL --security-opt no-new-privileges')"

# 既定の N 列（ホスト上限で最初のステップから止まるので何も起動しない）
new_scene
cp "$TMP/meminfo85" "$S/meminfo"
expect_rc "chromium default n-list" 0 --target chromium --conditions idle --no-doubling
expect_jq "chromium default n-list: values" "1 5 10 50 100 200 500" '.args.n_list | map(tostring) | join(" ")'
new_scene
cp "$TMP/meminfo85" "$S/meminfo"
expect_rc "fandhe default n-list" 0 --conditions idle --no-doubling
expect_jq "fandhe default n-list: values" "10 50 100 200 500" '.args.n_list | map(tostring) | join(" ")'
new_scene
expect_rc "chromium rejects --image" 2 --target chromium --image fandhe-browser:default --n-list 1
expect_rc "target invalid" 2 --target firefox --n-list 1
for v in "sha256:zz" ""; do
  expect_rc "reference digest rejects [$v]" 2 --target chromium --n-list 1 --reference-digest "$v"
done
expect_rc "reference image needs tag" 2 --target chromium --n-list 1 --reference-image mcr.microsoft.com/playwright
new_scene
expect_eq "chromium: reference pin required" "2" "$(env -u CONTAINER_SIZE_REFERENCE_IMAGE -u CONTAINER_SIZE_REFERENCE_DIGEST DOCKER_BIN="$SHIM" bash "${HERE}/measure.sh" \
  --cgroup-root "$S/cg" --proc-root "$S/proc" --meminfo "$S/meminfo" --helper-image node:22-alpine --helper-digest "$DIGEST" \
  --target chromium --n-list 1 >/dev/null 2>&1; echo $?)"
new_scene
export FAKE_REF_DIGEST_REF="mcr.microsoft.com/playwright@${WRONG_DIGEST}"
expect_rc "reference digest mismatch is fail-closed" 1 --target chromium --n-list 1 --conditions idle
new_scene
export FAKE_CHROME_PATHS=""
expect_rc "chromium path: zero results" 2 --target chromium --n-list 1 --conditions idle
new_scene
export FAKE_CHROME_PATHS="/ms-playwright/chromium-1148/chrome-linux/chrome\n/ms-playwright/chromium-1200/chrome-linux/chrome"
expect_rc "chromium path: two results" 2 --target chromium --n-list 1 --conditions idle
new_scene
export FAKE_CHROME_PATHS="/ms-playwright/chromium_headless_shell-1148/chrome-linux/headless_shell"
expect_rc "chromium path: headless_shell is rejected" 2 --target chromium --n-list 1 --conditions idle
new_scene
export FAKE_CHROME_PATHS="/tmp/x;rm -rf /"
expect_rc "chromium path: unexpected path" 2 --target chromium --n-list 1 --conditions idle
expect_eq "chromium path: nothing started on failure" "0" "$(grep -c '^run -d' "$S/calls.log" || true)"

# Chromium は小さい N でもホスト上限に届く。上限を守って止まり、直前の N が max_ok_n になる
new_scene
export RUN_RAISE_MEM_K=3 RUN_RAISE_MEM_FILE="$TMP/meminfo85"
expect_rc "chromium host limit run" 0 --target chromium --n-list 2,4,8 --no-doubling --conditions idle
expect_jq "chromium host limit: stop_reason" "host_memory_limit" '.conditions[0].stop_reason'
expect_jq "chromium host limit: step incomplete" "false" '.conditions[0].steps[1].complete'
expect_jq "chromium host limit: stops launching" "1" '.conditions[0].steps[1].started'
expect_jq "chromium host limit: max_ok_n is the previous N" "2" '.conditions[0].max_ok_n'
expect_eq "chromium host limit: no later step" "2" "$(jq '.conditions[0].steps | length' "$S/out.json")"
new_scene
export OOM_K=3
expect_rc "chromium oom run" 0 --target chromium --n-list 2,3,5 --no-doubling --conditions idle
expect_jq "chromium oom: stops at breakdown" "breakdown" '.conditions[0].stop_reason'
expect_jq "chromium oom: max_ok_n is the previous N" "2" '.conditions[0].max_ok_n'
new_scene
export OOM_K=1
expect_rc "chromium breakdown at first N" 0 --target chromium --n-list 1,2 --no-doubling --conditions idle
expect_jq "chromium breakdown at first N: max_ok_n null" "null" '.conditions[0].max_ok_n'

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
expect_jq "navigate host limit: not-run is not a failure" "0" '.conditions[0].steps[0].breakdowns.navigate_failed'
expect_jq "navigate host limit: not-run counted separately" "2" '.conditions[0].steps[0].navigate_not_run'
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

# --- compare.sh（削減率の算出。Docker 不要） ---
CMP="${HERE}/compare.sh"
# $1=target $2=condition $3=steps（JSON 配列）
mk_result() {
  jq -n --arg t "$1" --arg c "$2" --argjson steps "$3" '
    {schema: "container-memory/v1", target: $t,
     args: {image: "x", ready_timeout_s: 30, settle_s: 5, host_mem_limit_pct: 80, url: "https://example.com/"},
     image: {ref: "x", pinned_digest: null},
     environment: {arch: "x86_64", kernel: "6.8.0", mem_total_bytes: 1024, cgroup_driver: "systemd"},
     conditions: [{condition: $c, stop_reason: "completed", max_ok_n: null, steps: $steps}]}'
}
# $1=n $2=peak sum $3=current sum $4=complete $5=breakdown total
mk_step() {
  jq -n -c --argjson n "$1" --argjson p "$2" --argjson cur "$3" --argjson comp "${4:-true}" --argjson tot "${5:-0}" \
    '{n: $n, complete: $comp, started: $n, measured: $n, breakdowns: {total: $tot},
      memory_peak_bytes: {sum: $p}, memory_current_bytes: {sum: $cur}}'
}
CS="$TMP/cmp"
mkdir -p "$CS"
# 残り=compare.sh の引数。出力を $CS/out.json に残し終了コードを返す
run_cmp() {
  local rc=0
  bash "$CMP" "$@" >"$CS/out.json" 2>"$CS/err" || rc=$?
  return "$rc"
}
cmp_rc() { local name="$1" want="$2" got=0; shift 2; run_cmp "$@" || got=$?; expect_eq "$name" "$want" "$got"; }
cmp_jq() { expect_eq "$1" "$2" "$(jq -r "$3" "$CS/out.json" 2>/dev/null || echo JQ_ERROR)"; }

mk_result fandhe idle "[$(mk_step 100 500 400)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 100 1000 800)]" >"$CS/c.json"
cmp_rc "compare: basic" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: comparable" "true" '.comparable'
cmp_jq "compare: peak 500/1000 is 50.00" "50" '.comparisons[0].reduction_pct.memory_peak'
cmp_jq "compare: current 400/800 is 50.00" "50" '.comparisons[0].reduction_pct.memory_current'
cmp_jq "compare: schema" "container-memory-compare/v1" '.schema'
cmp_jq "compare: reference CTR-3 met" "true" '.reference_thresholds.results[0].reference_threshold_met["CTR-3"]'
cmp_jq "compare: reference CTR-6 not met" "false" '.reference_thresholds.results[0].reference_threshold_met["CTR-6"]'
cmp_jq "compare: no extrapolation by default" "null" '.extrapolation'

# 切り捨て（1/3 は 66.66。66.67 に丸めない）
mk_result fandhe idle "[$(mk_step 100 1 1)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 100 3 3)]" >"$CS/c.json"
cmp_rc "compare: floor" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: 1/3 floors to 66.66" "66.66" '.comparisons[0].reduction_pct.memory_peak'
# fandhe の方が大きい場合は負の値のまま出す
mk_result fandhe idle "[$(mk_step 100 1500 1500)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 100 1000 1000)]" >"$CS/c.json"
cmp_rc "compare: negative" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: negative reduction" "-50" '.comparisons[0].reduction_pct.memory_peak'
cmp_jq "compare: negative is not meeting thresholds" "false" '.reference_thresholds.results[0].reference_threshold_met["CTR-3"]'

# 共通の N が無ければ理由だけを出し、数値を出さない
mk_result fandhe idle "[$(mk_step 10 500 500)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 5 1000 1000)]" >"$CS/c.json"
cmp_rc "compare: no common n" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: no common n: no numbers" "0" '[.comparisons[] | select(.reduction_pct != null)] | length'
cmp_jq "compare: n=10 lacks chromium" "no_chromium_measurement_at_n" '.comparisons[] | select(.n == 10) | .reason'
cmp_jq "compare: n=5 lacks fandhe" "no_fandhe_measurement_at_n" '.comparisons[] | select(.n == 5) | .reason'

# 破綻・不完全・chromium のメモリ 0
mk_result fandhe idle "[$(mk_step 10 500 500 false 0),$(mk_step 20 500 500 true 1),$(mk_step 30 500 500 true 0)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 10 1000 1000),$(mk_step 20 1000 1000),$(mk_step 30 1000 1000 false 0),$(mk_step 40 0 0)]" >"$CS/c.json"
cmp_rc "compare: reasons" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: fandhe incomplete" "fandhe_step_incomplete" '.comparisons[] | select(.n == 10) | .reason'
cmp_jq "compare: fandhe breakdown" "fandhe_breakdown" '.comparisons[] | select(.n == 20) | .reason'
cmp_jq "compare: chromium incomplete" "chromium_step_incomplete" '.comparisons[] | select(.n == 30) | .reason'
mk_result fandhe idle "[$(mk_step 40 10 10)]" >"$CS/f.json"
cmp_rc "compare: chromium zero" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: chromium zero bytes" "chromium_zero_bytes" '.comparisons[] | select(.n == 40) | .reason'
mk_result chromium idle "[$(mk_step 20 1000 1000 true 2)]" >"$CS/c.json"
mk_result fandhe idle "[$(mk_step 20 500 500)]" >"$CS/f.json"
cmp_rc "compare: chromium breakdown" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: chromium breakdown reason" "chromium_breakdown" '.comparisons[0].reason'

# 環境・条件が食い違えば比較しない
mk_result fandhe idle "[$(mk_step 100 500 500)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 100 1000 1000)]" >"$CS/c.json"
for k in 'environment.kernel="6.9.0"' 'environment.arch="aarch64"' 'environment.mem_total_bytes=2048' \
  'environment.cgroup_driver="cgroupfs"' 'args.ready_timeout_s=60' 'args.settle_s=1' 'args.host_mem_limit_pct=70'; do
  jq --argjson v "${k#*=}" "(.${k%%=*}) = \$v" "$CS/c.json" >"$CS/c2.json"
  cmp_rc "compare: mismatch ${k%%=*}" 0 --fandhe "$CS/f.json" --chromium "$CS/c2.json"
  cmp_jq "compare: mismatch ${k%%=*} comparable=false" "false" '.comparable'
  # shellcheck disable=SC2016 # jq 式の $x は jq 変数
  cmp_jq "compare: mismatch ${k%%=*} reason" "${k%%=*}" '.reasons[0] | sub("^(environment|args)_mismatch:"; "") as $x | if startswith("environment") then "environment." + $x else "args." + $x end'
  cmp_jq "compare: mismatch ${k%%=*} has no reductions" "0" '[.comparisons[] | select(.reduction_pct != null)] | length'
done
# loaded では url の食い違いも不一致。idle だけなら url は問わない
mk_result fandhe loaded "[$(mk_step 100 500 500)]" >"$CS/f.json"
mk_result chromium loaded "[$(mk_step 100 1000 1000)]" >"$CS/c.json"
jq '.args.url = "https://example.org/"' "$CS/c.json" >"$CS/c2.json"
cmp_rc "compare: url mismatch (loaded)" 0 --fandhe "$CS/f.json" --chromium "$CS/c2.json"
cmp_jq "compare: url mismatch makes it incomparable" "args_mismatch:url" '.reasons[0]'
cmp_rc "compare: loaded same url" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: loaded same url comparable" "true" '.comparable'
mk_result fandhe idle "[$(mk_step 100 500 500)]" >"$CS/f.json"
mk_result chromium loaded "[$(mk_step 100 1000 1000)]" >"$CS/c.json"
cmp_rc "compare: no common condition" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
cmp_jq "compare: no common condition reason" "no_common_condition" '.reasons[0]'
cmp_jq "compare: skipped conditions listed" "idle loaded" '.skipped_conditions | join(" ")'

# 入力の検査（exit 2）
mk_result fandhe idle "[$(mk_step 100 500 500)]" >"$CS/f.json"
mk_result chromium idle "[$(mk_step 100 1000 1000)]" >"$CS/c.json"
cmp_rc "compare: target swapped" 2 --fandhe "$CS/c.json" --chromium "$CS/f.json"
cmp_rc "compare: both fandhe" 2 --fandhe "$CS/f.json" --chromium "$CS/f.json"
jq 'del(.target)' "$CS/f.json" >"$CS/f-legacy.json"
cmp_rc "compare: #929 output without target is fandhe" 0 --fandhe "$CS/f-legacy.json" --chromium "$CS/c.json"
jq 'del(.target)' "$CS/c.json" >"$CS/c-legacy.json"
cmp_rc "compare: chromium needs target" 2 --fandhe "$CS/f.json" --chromium "$CS/c-legacy.json"
jq '.schema = "container-memory/v2"' "$CS/f.json" >"$CS/bad.json"
cmp_rc "compare: bad schema" 2 --fandhe "$CS/bad.json" --chromium "$CS/c.json"
printf 'not json' >"$CS/bad.json"
cmp_rc "compare: not json" 2 --fandhe "$CS/bad.json" --chromium "$CS/c.json"
jq '.conditions[0].steps[0].memory_peak_bytes.sum = "1"' "$CS/f.json" >"$CS/bad.json"
cmp_rc "compare: string where number expected" 2 --fandhe "$CS/bad.json" --chromium "$CS/c.json"
jq '.conditions[0].steps[0].memory_peak_bytes.sum = -5' "$CS/f.json" >"$CS/bad.json"
cmp_rc "compare: negative bytes" 2 --fandhe "$CS/bad.json" --chromium "$CS/c.json"
jq '.conditions += .conditions' "$CS/f.json" >"$CS/bad.json"
cmp_rc "compare: duplicate condition" 2 --fandhe "$CS/bad.json" --chromium "$CS/c.json"
cmp_rc "compare: missing file" 2 --fandhe "$CS/none.json" --chromium "$CS/c.json"
cmp_rc "compare: missing --chromium" 2 --fandhe "$CS/f.json"
cmp_rc "compare: unknown option" 2 --bogus
cmp_rc "compare: bad poc1" 2 --fandhe "$CS/f.json" --chromium "$CS/c.json" --poc1-chromium-per-instance-bytes "1;x"
cmp_rc "compare: bad poc8" 2 --fandhe "$CS/f.json" --chromium "$CS/c.json" --poc8-fandhe-n100-bytes 0

# 外挿値は引数があるときだけ別オブジェクトに出し、削減率に影響しない
cmp_rc "compare: baseline for extrapolation" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json"
BASE_RED=$(jq -c '.comparisons[0].reduction_pct' "$CS/out.json")
cmp_rc "compare: with extrapolation" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json" \
  --poc1-chromium-per-instance-bytes 8 --poc8-fandhe-n100-bytes 400
cmp_jq "compare: extrapolation kind" "extrapolated" '.extrapolation.kind'
cmp_jq "compare: extrapolation does not change reduction" "$BASE_RED" '.comparisons[0].reduction_pct | tojson'
cmp_jq "compare: PoC-1 extrapolated bytes" "800" '.extrapolation.poc1_chromium.rows[0].chromium_extrapolated_bytes'
cmp_jq "compare: PoC-1 measured minus extrapolated" "25" '.extrapolation.poc1_chromium.rows[0].measured_minus_extrapolated_pct'
cmp_jq "compare: PoC-8 measured minus extrapolated" "25" '.extrapolation.poc8_fandhe.rows[0].measured_minus_extrapolated_pct'
cmp_rc "compare: only PoC-1" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json" --poc1-chromium-per-instance-bytes 8
cmp_jq "compare: extrapolation only PoC-1" "null" '.extrapolation.poc8_fandhe'
# --out は一時ファイル経由
cmp_rc "compare: --out" 0 --fandhe "$CS/f.json" --chromium "$CS/c.json" --out "$CS/res.json"
expect_eq "compare: --out writes the file" "container-memory-compare/v1" "$(jq -r .schema "$CS/res.json")"
expect_eq "compare: --out leaves stdout empty" "0" "$(wc -c <"$CS/out.json" | tr -d ' ')"
expect_eq "compare: --out leaves no temp files" "1" "$(find "$CS" -maxdepth 1 -name 'res.json*' | wc -l | tr -d ' ')"

# --- navigate.mjs の構文 ---
if command -v node >/dev/null 2>&1; then
  if node --check "${ROOT}/harness/multi-instance-memory/navigate.mjs" 2>/dev/null; then ok "navigate.mjs syntax"; else ng "navigate.mjs syntax"; fi
fi

echo "container-memory self-test: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
