#!/usr/bin/env bash
#
# 配布コンテナを N 個同時に起動したときの集約メモリを、ホスト側の cgroup v2 から測定して
# JSON で出力する（TASK-67・Issue #929・CTR-3・CTR-7・MEAS-6・MS-5。実測の実行と判定は #306・#308）。
# 呼び出し元は scripts/harness.sh の measure-container-memory（Makefile の同名ターゲット）。
# 同じスクリプトを TASK-69（Chromium 側との比較）でも使う。出力の読み手は後続の測定レポート。
# `--target chromium` で比較対象の Chromium（CTR-1 と同じ Playwright 公式イメージ。tag + digest 固定）を
# 同じ指標・同じ条件・同じ破綻判定で測る（Issue #930・CTR-6）。2 つの結果の比較は compare.sh。
#
# 測定の要点（詳細は README.md）:
#   - fandhe の測定対象イメージは scratch でシェルが無いため、コンテナ内でコマンドを実行しない。
#     `docker inspect` で得たコンテナの PID から `/proc/<pid>/cgroup` を引き、ホスト側の
#     `memory.peak` / `memory.current` / `memory.events` を読む。
#   - 起動待ち（CTR-7）と loaded 条件の CDP 到達は、対象と network namespace を共有する補助コンテナ
#     （`--network container:<cid>`）だけで行う。本体の待ち受け（127.0.0.1:9333）は変えず、`-p` も
#     `--network host` も使わない（SEC-2・SEC-4）。
#   - 破綻（OOM kill・起動失敗・起動待ち超過・停止・navigate 失敗）を N ごとに数え、1 件でも出たら
#     それ以上 N を増やさない。ホストの使用メモリが上限の割合に達した場合も止まる。
#   - 起動したコンテナはラベル（fandhe-ctrmem.run=<実行 ID>）で識別し、終了時・中断時にそのラベルに
#     一致するものだけを削除する。他のコンテナには触れない。
#
# 依存は bash + docker + jq + awk + coreutils のみ（dependency-policy）。Linux 専用。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/../.." && pwd)"
# loaded 条件で補助コンテナへ読み取り専用で渡す（harness/multi-instance-memory の流用。コピーしない）
NAVIGATE_JS="${ROOT}/harness/multi-instance-memory/navigate.mjs"
# 自己テストが偽 docker シムへ差し替える入口
DOCKER="${DOCKER_BIN:-docker}"
LABEL_KEY="fandhe-ctrmem.run"
CDP_ENDPOINT="http://127.0.0.1:9333"

IMAGE_REF_RE='^[a-z0-9][a-z0-9._/-]{0,127}(:[A-Za-z0-9._-]{1,128})?$'
HELPER_REF_RE='^[a-z0-9][a-z0-9._/-]{0,127}:[A-Za-z0-9._-]{1,128}$'
# Chromium バイナリの絶対パス。解決結果をこの形に限り、docker run の --entrypoint へ渡す。
CHROMIUM_PATH_RE='^/ms-playwright/chromium-[0-9]{1,6}/chrome-linux/chrome$'
# Chromium コンテナの CDP は fandhe 側と同じ 9333 に揃える（起動待ち・ナビゲートの補助コンテナを共通化するため）。
CHROMIUM_USER="pwuser"
CHROMIUM_PROFILE_DIR="/tmp/fandhe-ctrmem-profile"
DIGEST_RE='^sha256:[0-9a-f]{64}$'
# 公開 http(s) URL のみ。userinfo・IPv6 リテラル・空白は構文の時点で拒否する。
URL_RE='^https?://[A-Za-z0-9][A-Za-z0-9.-]*(:[0-9]{1,5})?([/?#][^[:space:]]*)?$'
N_LIST_RE='^[1-9][0-9]{0,5}(,[1-9][0-9]{0,5})*$'
CID_RE='^[0-9a-f]{12,64}$'

usage() {
  cat >&2 <<'USAGE'
Usage:
  measure.sh [--target fandhe|chromium] [--image <ref>] [--n-list <a,b,...>] [--no-doubling] [--max-n <n>]
             [--conditions idle,loaded] [--url <public http(s) url>]
             [--ready-timeout <sec>] [--host-mem-limit-pct <p>] [--settle <sec>]
             [--navigate-concurrency <k>] [--helper-image <repo:tag>] [--helper-digest <sha256:...>]
             [--reference-image <repo:tag>] [--reference-digest <sha256:...>]
             [--out <path>]

  --target                 fandhe (default) or chromium. chromium measures the pinned reference image
                           (Playwright official image) with the same metrics, conditions and breakdown rules.

  --image                  Image under test (default: fandhe-browser:default). Must exist locally.
                           Not allowed with --target chromium (the pinned reference image is used).
  --n-list                 Strictly increasing container counts
                           (default: 10,50,100,200,500; chromium: 1,5,10,50,100,200,500).
  --no-doubling            Do not keep doubling N after the end of the list.
  --max-n                  Upper bound for doubling (default: 8192).
  --conditions             idle and/or loaded (default: idle,loaded). loaded requires --url.
  --url                    Public http(s) URL navigated once per container in the loaded condition.
  --ready-timeout          Seconds to wait for each container's CDP endpoint (default: 30; CTR-7).
  --host-mem-limit-pct     Stop when host memory use reaches this percent, 1..99 (default: 80).
  --settle                 Seconds to wait before reading cgroup counters, 0..3600 (default: 5).
  --navigate-concurrency   Parallel navigate helpers in the loaded condition, 1..64 (default: 8).
  --helper-image           Helper image repo:tag (default: $CONTAINER_MEMORY_HELPER_IMAGE).
  --helper-digest          Pinned digest of the helper image (default: $CONTAINER_MEMORY_HELPER_DIGEST).
  --reference-image        Chromium reference image repo:tag (default: $CONTAINER_SIZE_REFERENCE_IMAGE).
  --reference-digest       Pinned digest of the reference image (default: $CONTAINER_SIZE_REFERENCE_DIGEST).
  --out                    Write the JSON here instead of stdout.
Test-only inputs (normally not set): --cgroup-root --proc-root --meminfo
USAGE
}

die() {
  echo "error: $*" >&2
  exit 2
}

# `--opt` の直後に値があることを確認する（$1=opt 名、残り=残りの引数）。
need_value() {
  [ "$#" -ge 2 ] || die "option $1 requires a value"
}

# $1=name $2=value $3=regex。不一致は値を出力せず exit 2。
require_match() {
  [[ "$2" =~ $3 ]] || die "$1 is invalid"
}

# $1=name $2=value $3=min $4=max。先頭ゼロなしの 10 進整数のみ（0 自体は許可）。
require_int() {
  [[ "$2" =~ ^(0|[1-9][0-9]{0,6})$ ]] || die "$1 must be a decimal integer without leading zeros"
  { [ "$2" -ge "$3" ] && [ "$2" -le "$4" ]; } || die "$1 must be between $3 and $4"
}

# --- 純関数（自己テストが source して直接検証する） ---

# $1=カンマ区切りの N 列 $2=max-n $3=倍々フラグ(1/0) → 空白区切りの実行列を返す。
# 倍々は列の末尾の 2 倍から max-n 以下の範囲で続ける。
expand_n_list() {
  local list="$1" max="$2" dbl="$3" out="" last=0 n
  local IFS=,
  for n in $list; do
    out="${out} ${n}"
    last=$n
  done
  if [ "$dbl" = 1 ]; then
    n=$((last * 2))
    while [ "$n" -le "$max" ]; do
      out="${out} ${n}"
      n=$((n * 2))
    done
  fi
  echo "${out# }"
}

# $1=meminfo ファイル → 使用率（%）を小数第 1 位で返す。(MemTotal - MemAvailable) / MemTotal。
mem_used_pct() {
  LC_ALL=C awk '/^MemTotal:/ { t = $2 } /^MemAvailable:/ { a = $2 }
    END { if (t > 0 && a != "") printf "%.1f", (t - a) * 100 / t; else exit 1 }' "$1"
}

# $1=使用率 $2=上限 → 使用率 >= 上限なら 0。
pct_ge() {
  LC_ALL=C awk -v a="$1" -v b="$2" 'BEGIN { exit !(a + 0 >= b + 0) }'
}

# $1=/proc/<pid>/cgroup のパス → cgroup v2 の `0::` 行のパス（先頭 `/`）を返す。
# systemd ドライバと cgroupfs ドライバでパス形式が違うため、推測で組み立てずここから引く。
cgroup_rel_path() {
  local line rel=""
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line//$'\r'/}"
    case "$line" in
      0::*)
        rel="${line#0::}"
        break
        ;;
    esac
  done <"$1" || return 1
  [[ "$rel" =~ ^/[A-Za-z0-9._@:/-]+$ ]] || return 1
  case "$rel" in *..*) return 1 ;; esac
  printf '%s' "$rel"
}

# $1=memory.events のパス → oom_kill の値（行が無ければ 0）。
read_oom_kill() {
  local v
  v=$(awk '$1 == "oom_kill" { print $2; exit }' "$1") || return 1
  v="${v//$'\r'/}"
  [ -n "$v" ] || v=0
  [[ "$v" =~ ^[0-9]{1,15}$ ]] || return 1
  printf '%s' "$v"
}

# $1=cgroup のカウンタファイル → 数値を返す。読めない・数値でなければ失敗。
read_cg_value() {
  local v
  [ -r "$1" ] || return 1
  IFS= read -r v <"$1" || return 1
  v="${v//$'\r'/}"
  [[ "$v" =~ ^[0-9]{1,18}$ ]] || return 1
  printf '%s' "$v"
}

# 現在時刻（ミリ秒）。EPOCHREALTIME（bash 5）が無い環境は秒精度（結果の timing_resolution に記録する）。
# 小数点がカンマのロケールでも壊れないよう、区切り文字を `[.,]` で除去する。
now_ms() {
  if [ -n "${EPOCHREALTIME:-}" ]; then
    local t="${EPOCHREALTIME/[.,]/}"
    echo $((t / 1000))
  else
    echo $(($(date +%s) * 1000))
  fi
}

# --- 後始末 ---

# 自分の実行 ID のラベルに一致するコンテナだけを削除し、残りが 0 件であることを確認する。
# フィルタ無しの docker rm やワイルドカードは使わない。
remove_run_containers() {
  [ -n "${RUN_ID:-}" ] || return 0
  local out line
  local -a list=()
  out=$("$DOCKER" ps -aq --filter "label=${LABEL_KEY}=${RUN_ID}") || return 1
  while IFS= read -r line; do
    line="${line//$'\r'/}"
    [ -n "$line" ] || continue
    [[ "$line" =~ $CID_RE ]] || return 1
    list+=("$line")
  done <<<"$out"
  if [ "${#list[@]}" -gt 0 ]; then
    "$DOCKER" rm -f "${list[@]}" >/dev/null || return 1
  fi
  out=$("$DOCKER" ps -aq --filter "label=${LABEL_KEY}=${RUN_ID}") || return 1
  [ -z "${out//[[:space:]]/}" ]
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  local pids p
  pids=$(jobs -p 2>/dev/null || true)
  # バックグラウンドのサブシェルだけでなく、その子の docker クライアントも止めてから回収する。
  # 回収（wait）前にラベル検索へ進むと、作成中の補助コンテナを取りこぼすため。
  for p in $pids; do
    if command -v pkill >/dev/null 2>&1; then pkill -TERM -P "$p" 2>/dev/null || true; fi
    kill "$p" 2>/dev/null || true
  done
  [ -z "$pids" ] || wait 2>/dev/null || true
  if ! remove_run_containers && { sleep 1; ! remove_run_containers; }; then
    echo "error: failed to remove containers labeled ${LABEL_KEY}=${RUN_ID:-}; remove them manually" >&2
    [ "$rc" -ne 0 ] || rc=1
  fi
  [ -z "${WORK:-}" ] || rm -rf "$WORK"
  exit "$rc"
}

# $1=イメージの inspect JSON $2=ピン（`repo@sha256:...`）$3=ピンした digest。補助イメージと Chromium の
# 参照イメージの両方で使う。RepoDigests にピン、同 digest を別のリポジトリ表記（`docker.io/library/node@...`）で
# 持つもの、または pull で入るプラットフォーム別 manifest digest のいずれかがあれば 0。プラットフォーム digest は
# registry の index から os/arch 一致の manifest を引いて得る。取得できなければ不一致扱い（fail-closed）。
digest_matches() {
  local info="$1" pin="$2" digest="$3" raw pd os arch
  if printf '%s' "$info" | jq -e --arg d "$pin" --arg h "$digest" \
    '(.RepoDigests // []) | any(. == $d or endswith("@" + $h))' >/dev/null; then
    return 0
  fi
  os=$(printf '%s' "$info" | jq -r '.Os // empty' | tr -d '\r')
  arch=$(printf '%s' "$info" | jq -r '.Architecture // empty' | tr -d '\r')
  { [ -n "$os" ] && [ -n "$arch" ]; } || return 1
  raw=$("$DOCKER" buildx imagetools inspect --raw "$pin" 2>/dev/null) || return 1
  pd=$(printf '%s' "$raw" | jq -r --arg os "$os" --arg arch "$arch" \
    '[.manifests[]? | select(.platform.os == $os and .platform.architecture == $arch) | .digest][0] // empty' | tr -d '\r')
  [[ "$pd" =~ ^sha256:[0-9a-f]{64}$ ]] || return 1
  printf '%s' "$info" | jq -e --arg p "@${pd}" '(.RepoDigests // []) | any(endswith($p))' >/dev/null
}

# 参照イメージ内の Chromium 本体のパスを実行ごとに 1 回解決する（リビジョン番号はハードコードしない）。
# 固定文字列のコマンドだけを実行し、結果は正規表現に合う行ちょうど 1 行でなければ exit 2。
# headless_shell ディレクトリは CHROMIUM_PATH_RE に当たらない。結果は CHROMIUM_PATH に入れる。
resolve_chromium_path() {
  local out line count=0 found=""
  out=$("$DOCKER" run --rm --network none --cap-drop ALL --security-opt no-new-privileges \
    --entrypoint sh "$REF_PIN" -c 'ls -d /ms-playwright/chromium-[0-9]*/chrome-linux/chrome' 2>/dev/null </dev/null) ||
    die "could not resolve the Chromium binary path in the reference image"
  while IFS= read -r line; do
    line="${line//$'\r'/}"
    [ -n "$line" ] || continue
    count=$((count + 1))
    found="$line"
  done <<<"$out"
  [ "$count" -eq 1 ] || die "the reference image must contain exactly one Chromium binary"
  [[ "$found" =~ $CHROMIUM_PATH_RE ]] || die "the resolved Chromium binary path is unexpected"
  CHROMIUM_PATH="$found"
}

# --- 1 ステップ（条件 $1・N=$2） ---

# 結果は STEP_COMPLETE（1/0）・STEP_TOTAL（破綻の合計）・STEP_HOST_PCT に入れ、
# ステップの JSON を $WORK/steps-<条件>.ndjson へ追記する（サブシェルだと exit が伝わらないため globals）。
run_step() {
  local cond="$1" n="$2" sd i pct t0 cid pid cgrel cg
  sd=$(mktemp -d "${WORK}/step.XXXXXX")
  : >"$sd/start.tsv"
  : >"$sd/ready.tsv"
  : >"$sd/nav.tsv"
  : >"$sd/rows.tsv"
  local started=0 start_failed=0
  STEP_COMPLETE=1

  for ((i = 1; i <= n; i++)); do
    pct=$(mem_used_pct "$MEMINFO") || {
      echo "error: cannot read host memory from the meminfo file" >&2
      exit 1
    }
    if pct_ge "$pct" "$HOST_LIMIT_PCT"; then
      STEP_COMPLETE=0
      break
    fi
    t0=$(now_ms)
    # --rm を付けない（OOM で落ちたコンテナが消えると State.OOMKilled を読めなくなる）。
    # -p も --network host も使わない（CDP は loopback のまま。SEC-2・SEC-4）。
    # chromium は PID 1 にしない・非 root・--no-sandbox（--cap-drop ALL では sandbox が動かない）で起動する。
    # 引数は配列で渡し、シェルを介さない。
    local -a run_opts=() run_args=()
    if [ "$TARGET" = chromium ]; then
      run_opts=(--init --user "$CHROMIUM_USER" --entrypoint "$CHROMIUM_PATH")
      run_args=("${CHROMIUM_ARGS[@]}")
    fi
    if ! cid=$("$DOCKER" run -d --label "${LABEL_KEY}=${RUN_ID}" --label "fandhe-ctrmem.step=${cond}-${n}" \
      --cap-drop ALL --security-opt no-new-privileges ${run_opts[@]+"${run_opts[@]}"} \
      "$IMAGE" ${run_args[@]+"${run_args[@]}"} 2>"$sd/run.err"); then
      start_failed=$((start_failed + 1))
      continue
    fi
    cid="${cid//$'\r'/}"
    [[ "$cid" =~ $CID_RE ]] || {
      echo "error: unexpected output from docker run" >&2
      exit 1
    }
    started=$((started + 1))
    pid=$("$DOCKER" inspect --format '{{.State.Pid}}' "$cid") || {
      echo "error: docker inspect failed for a started container" >&2
      exit 1
    }
    pid="${pid//$'\r'/}"
    [[ "$pid" =~ ^[0-9]{1,10}$ ]] || {
      echo "error: unexpected PID from docker inspect" >&2
      exit 1
    }
    cg="-"
    if [ "$pid" -gt 0 ] && cgrel=$(cgroup_rel_path "${PROC_ROOT}/${pid}/cgroup"); then
      cg="${CGROUP_ROOT}${cgrel}"
      if [ -z "$PEAK_CHECKED" ] && [ -d "$cg" ]; then
        # memory.peak は kernel 5.19 以降。0 や null で代用すると測定が無意味になるため前提不足として止める
        [ -r "${cg}/memory.peak" ] || die "memory.peak is not available in the container cgroup (kernel 5.19 or later is required)"
        PEAK_CHECKED=1
      fi
    fi
    printf '%s\t%s\t%s\t%s\n' "$i" "$cid" "$t0" "$cg" >>"$sd/start.tsv"
    # 起動待ち: 対象と network namespace を共有する補助コンテナで CDP の /json/version を待つ。
    # 所要時間は docker run 直前から補助コンテナの終了まで（補助の起動時間を含むため上側に寄る）。
    (
      rc=0
      "$DOCKER" run --rm --label "${LABEL_KEY}=${RUN_ID}" --network "container:${cid}" \
        --cap-drop ALL --security-opt no-new-privileges --read-only --user 65532:65532 \
        "$HELPER_PIN" timeout "$((READY_TIMEOUT + 5))" sh -c \
        'until wget -q -T 2 -O /dev/null http://127.0.0.1:9333/json/version; do sleep 0.2; done' \
        >/dev/null 2>&1 || rc=$?
      printf '%s\t%s\t%s\n' "$i" "$(now_ms)" "$rc" >>"$sd/ready.tsv"
    ) &
  done
  wait

  # loaded: 起動待ちを通過したコンテナに 1 回ナビゲートする（navigate の時間は CTR-7 の判定に使わない）
  if [ "$cond" = loaded ]; then
    local idx cc st cgx rend rrc rms line inflight=0
    # shellcheck disable=SC2034 # 4 列目（cgroup パス）は loaded の起動対象選別では使わない
    while IFS=$'\t' read -r idx cc st cgx; do
      rend="" rrc=""
      line=$(awk -F'\t' -v i="$idx" '$1 == i { print $2, $3 }' "$sd/ready.tsv")
      read -r rend rrc <<<"$line" || true
      [ -n "$rend" ] || continue
      rms=$((rend - st))
      { [ "$rrc" = 0 ] && [ "$rms" -le $((READY_TIMEOUT * 1000)) ]; } || continue
      # ナビゲートで各コンテナのメモリが増えるため、バッチごとにホストの使用率を確認する。
      # 上限に達したら追加のナビゲートを中止し、不完全ステップ（complete=false）として記録する。
      if [ "$inflight" -eq 0 ]; then
        pct=$(mem_used_pct "$MEMINFO") || {
          echo "error: cannot read host memory from the meminfo file" >&2
          exit 1
        }
        if pct_ge "$pct" "$HOST_LIMIT_PCT"; then
          STEP_COMPLETE=0
          break
        fi
      fi
      (
        rc=0
        "$DOCKER" run --rm --label "${LABEL_KEY}=${RUN_ID}" --network "container:${cc}" \
          --cap-drop ALL --security-opt no-new-privileges --read-only --user 65532:65532 \
          --mount "type=bind,source=${NAVIGATE_JS},target=/h/navigate.mjs,readonly" \
          "$HELPER_PIN" node /h/navigate.mjs "$CDP_ENDPOINT" "$URL" ${NAV_EXTRA[@]+"${NAV_EXTRA[@]}"} >/dev/null 2>&1 || rc=$?
        printf '%s\t%s\n' "$idx" "$rc" >>"$sd/nav.tsv"
      ) &
      inflight=$((inflight + 1))
      if [ "$inflight" -ge "$NAV_CONC" ]; then
        wait
        inflight=0
      fi
    done <"$sd/start.tsv"
    wait
  fi

  [ "$SETTLE" -eq 0 ] || sleep "$SETTLE"

  # 各コンテナを読む。status は優先順位 oom_kill > exited > ready_timeout > navigate_failed で 1 つに決める
  local idx cid st cg rend rrc rms rok running oomk exitc peak cur oomn p c line status nrc
  local oom_events_total=0 oom_flag_count=0
  while IFS=$'\t' read -r idx cid st cg; do
    [ "$cg" != "-" ] || cg=""
    rend="" rrc="" rms="null" rok=0
    line=$(awk -F'\t' -v i="$idx" '$1 == i { print $2, $3 }' "$sd/ready.tsv")
    read -r rend rrc <<<"$line" || true
    if [ -n "$rend" ]; then
      rms=$((rend - st))
      if [ "$rrc" = 0 ] && [ "$rms" -le $((READY_TIMEOUT * 1000)) ]; then rok=1; fi
    fi
    line=$("$DOCKER" inspect --format '{{.State.Running}} {{.State.OOMKilled}} {{.State.ExitCode}}' "$cid" </dev/null) || {
      echo "error: docker inspect failed for a started container" >&2
      exit 1
    }
    # shellcheck disable=SC2034 # ExitCode は読み捨て（状態の判定は Running・OOMKilled で行う）
    read -r running oomk exitc <<<"${line//$'\r'/}" || true
    peak="null" cur="null" oomn=0
    if [ -n "$cg" ] && [ -d "$cg" ]; then
      if p=$(read_cg_value "${cg}/memory.peak"); then peak=$p; fi
      if c=$(read_cg_value "${cg}/memory.current"); then cur=$c; fi
      # 稼働中コンテナの memory.events が読めない・解析できない場合は「OOM なし」にせず測定エラーにする
      # （コンテナ内の子プロセスだけが OOM kill された場合は State.OOMKilled で検出できないため。fail-closed）
      if [ -r "${cg}/memory.events" ] && oomn=$(read_oom_kill "${cg}/memory.events"); then
        :
      else
        oomn=0
        if [ "$running" = true ]; then
          echo "error: cannot read memory.events of a running container (fail-closed)" >&2
          exit 1
        fi
      fi
    fi
    if [ "$running" = true ] && { [ "$peak" = null ] || [ "$cur" = null ]; }; then
      echo "error: cannot read cgroup memory counters of a running container (fail-closed)" >&2
      exit 1
    fi
    status=ok
    nrc=$(awk -F'\t' -v i="$idx" '$1 == i { print $2 }' "$sd/nav.tsv")
    if [ "$oomn" -gt 0 ] || [ "$oomk" = true ]; then
      status=oom_kill
    elif [ "$running" != true ]; then
      status=exited
    elif [ "$rok" -eq 0 ]; then
      status=ready_timeout
    elif [ "$cond" = loaded ] && [ -z "$nrc" ]; then
      # ホストメモリ上限でナビゲートが未実行（STEP_COMPLETE=0）。失敗ではないため破綻には数えない
      status=navigate_not_run
    elif [ "$cond" = loaded ] && [ "$nrc" != 0 ]; then
      status=navigate_failed
    fi
    oom_events_total=$((oom_events_total + oomn))
    [ "$oomk" != true ] || oom_flag_count=$((oom_flag_count + 1))
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$status" "$rok" "$rms" "$peak" "$cur" >>"$sd/rows.tsv"
  done <"$sd/start.tsv"

  STEP_HOST_PCT=$(mem_used_pct "$MEMINFO") || {
    echo "error: cannot read host memory from the meminfo file" >&2
    exit 1
  }

  jq -Rn -c --argjson n "$n" --argjson complete "$STEP_COMPLETE" --argjson started "$started" \
    --argjson start_failed "$start_failed" --argjson host "$STEP_HOST_PCT" --arg unit "ms" \
    --argjson oom_events "$oom_events_total" --argjson oom_flag "$oom_flag_count" '
    [inputs | split("\t")] as $r
    | def cnt($s): [$r[] | select(.[1] == $s)] | length;
      def agg($i): [$r[] | .[$i] | select(. != "null") | tonumber] as $v
        | {sum: ($v | add // 0),
           avg: (if ($v | length) > 0 then (($v | add) / ($v | length) | floor) else null end),
           max: (if ($v | length) > 0 then ($v | max) else null end), count: ($v | length)};
      def rt: [$r[] | .[3] | select(. != "null") | tonumber] as $v
        | {max: (if ($v | length) > 0 then ($v | max) else null end),
           avg: (if ($v | length) > 0 then (($v | add) / ($v | length) | floor) else null end),
           unit: $unit};
      {oom_kill: cnt("oom_kill"), start_failed: $start_failed, ready_timeout: cnt("ready_timeout"),
       exited: cnt("exited"), navigate_failed: cnt("navigate_failed")} as $b
      | cnt("navigate_not_run") as $nnr
    | {n: $n, complete: ($complete == 1), started: $started,
       ready: ([$r[] | select(.[2] == "1")] | length),
       measured: ([$r[] | select(.[4] != "null")] | length),
       breakdowns: ($b + {total: ($b | add)}),
       navigate_not_run: $nnr,
       oom_detail: {memory_events_oom_kill_total: $oom_events, state_oom_killed_count: $oom_flag},
       memory_peak_bytes: agg(4), memory_current_bytes: agg(5), ready_time: rt,
       host_mem_used_pct: $host}' <"$sd/rows.tsv" >>"${WORK}/steps-${cond}.ndjson"
  STEP_TOTAL=$(tail -n 1 "${WORK}/steps-${cond}.ndjson" | jq -r '.breakdowns.total')

  # 後始末の前に使用率を記録済み。ステップごとにコンテナを入れ替えてゼロから始める
  remove_run_containers || {
    echo "error: failed to remove the containers of this step" >&2
    exit 1
  }
  rm -rf "$sd"
}

main() {
  local IMAGE="fandhe-browser:default" N_LIST="" DOUBLING=1 MAX_N=8192 IMAGE_SET=0
  TARGET="fandhe" NAV_EXTRA=()
  local REF_IMAGE="${CONTAINER_SIZE_REFERENCE_IMAGE:-}" REF_DIGEST="${CONTAINER_SIZE_REFERENCE_DIGEST:-}"
  local CONDITIONS="idle,loaded" OUT=""
  URL="" READY_TIMEOUT=30 HOST_LIMIT_PCT=80 SETTLE=5 NAV_CONC=8
  local HELPER_IMAGE="${CONTAINER_MEMORY_HELPER_IMAGE:-}" HELPER_DIGEST="${CONTAINER_MEMORY_HELPER_DIGEST:-}"
  CGROUP_ROOT="/sys/fs/cgroup" PROC_ROOT="/proc" MEMINFO="/proc/meminfo"

  while [ "$#" -gt 0 ]; do
    case "$1" in
      --target) need_value "$@"; TARGET="$2"; shift 2 ;;
      --image) need_value "$@"; IMAGE="$2"; IMAGE_SET=1; shift 2 ;;
      --reference-image) need_value "$@"; REF_IMAGE="$2"; shift 2 ;;
      --reference-digest) need_value "$@"; REF_DIGEST="$2"; shift 2 ;;
      --n-list) need_value "$@"; N_LIST="$2"; shift 2 ;;
      --no-doubling) DOUBLING=0; shift ;;
      --max-n) need_value "$@"; MAX_N="$2"; shift 2 ;;
      --conditions) need_value "$@"; CONDITIONS="$2"; shift 2 ;;
      --url) need_value "$@"; URL="$2"; shift 2 ;;
      --ready-timeout) need_value "$@"; READY_TIMEOUT="$2"; shift 2 ;;
      --host-mem-limit-pct) need_value "$@"; HOST_LIMIT_PCT="$2"; shift 2 ;;
      --settle) need_value "$@"; SETTLE="$2"; shift 2 ;;
      --navigate-concurrency) need_value "$@"; NAV_CONC="$2"; shift 2 ;;
      --helper-image) need_value "$@"; HELPER_IMAGE="$2"; shift 2 ;;
      --helper-digest) need_value "$@"; HELPER_DIGEST="$2"; shift 2 ;;
      --out) need_value "$@"; OUT="$2"; shift 2 ;;
      --cgroup-root) need_value "$@"; CGROUP_ROOT="$2"; shift 2 ;;
      --proc-root) need_value "$@"; PROC_ROOT="$2"; shift 2 ;;
      --meminfo) need_value "$@"; MEMINFO="$2"; shift 2 ;;
      -h | --help) usage; exit 0 ;;
      *) usage; die "unknown option" ;;
    esac
  done

  # --- 入力検証（値そのものは出力しない） ---
  require_match "--target" "$TARGET" '^(fandhe|chromium)$'
  if [ -z "$N_LIST" ]; then
    if [ "$TARGET" = chromium ]; then N_LIST="1,5,10,50,100,200,500"; else N_LIST="10,50,100,200,500"; fi
  fi
  if [ "$TARGET" = chromium ]; then
    # container-size のピンとずれないよう、測定対象イメージを引数で差し替えさせない
    [ "$IMAGE_SET" -eq 0 ] || die "--image cannot be used with --target chromium (the pinned reference image is used)"
    [ -n "$REF_IMAGE" ] || die "--reference-image is required for --target chromium (or set CONTAINER_SIZE_REFERENCE_IMAGE)"
    [ -n "$REF_DIGEST" ] || die "--reference-digest is required for --target chromium (or set CONTAINER_SIZE_REFERENCE_DIGEST)"
    require_match "--reference-image" "$REF_IMAGE" "$HELPER_REF_RE"
    require_match "--reference-digest" "$REF_DIGEST" "$DIGEST_RE"
    REF_PIN="${REF_IMAGE%:*}@${REF_DIGEST}"
    NAV_EXTRA=(--page-target)
    CHROMIUM_ARGS=(--headless=new --disable-gpu --no-first-run --no-default-browser-check --no-sandbox
      --disable-dev-shm-usage "--user-data-dir=${CHROMIUM_PROFILE_DIR}"
      --remote-debugging-address=127.0.0.1 --remote-debugging-port=9333 about:blank)
  fi
  require_match "--image" "$IMAGE" "$IMAGE_REF_RE"
  # 検証済みのピンを測定対象にする（IMAGE_REF_RE は `@digest` 形式を許さないため検証の後で代入する）
  [ "$TARGET" != chromium ] || IMAGE="$REF_PIN"
  require_match "--n-list" "$N_LIST" "$N_LIST_RE"
  require_int "--max-n" "$MAX_N" 1 1000000
  require_int "--ready-timeout" "$READY_TIMEOUT" 1 3600
  require_int "--host-mem-limit-pct" "$HOST_LIMIT_PCT" 1 99
  require_int "--settle" "$SETTLE" 0 3600
  require_int "--navigate-concurrency" "$NAV_CONC" 1 64
  [ -n "$HELPER_IMAGE" ] || die "--helper-image is required (or set CONTAINER_MEMORY_HELPER_IMAGE)"
  [ -n "$HELPER_DIGEST" ] || die "--helper-digest is required (or set CONTAINER_MEMORY_HELPER_DIGEST)"
  require_match "--helper-image" "$HELPER_IMAGE" "$HELPER_REF_RE"
  require_match "--helper-digest" "$HELPER_DIGEST" "$DIGEST_RE"

  local prev=0 v
  local IFS_SAVE="$IFS"
  IFS=,
  # shellcheck disable=SC2086 # カンマ区切りの単語分割は意図的
  for v in $N_LIST; do
    { [ "$v" -gt "$prev" ] && [ "$v" -le "$MAX_N" ]; } || { IFS="$IFS_SAVE"; die "--n-list must be strictly increasing and at most --max-n"; }
    prev=$v
  done
  IFS="$IFS_SAVE"

  [[ "$CONDITIONS" =~ ^(idle|loaded)(,(idle|loaded))?$ ]] || die "--conditions must be idle, loaded or idle,loaded"
  local -a CONDS=()
  local c
  IFS_SAVE="$IFS"
  IFS=,
  for c in $CONDITIONS; do CONDS+=("$c"); done
  IFS="$IFS_SAVE"
  if [ "${#CONDS[@]}" -eq 2 ] && [ "${CONDS[0]}" = "${CONDS[1]}" ]; then die "--conditions must not repeat a condition"; fi
  local want_loaded=0
  for c in "${CONDS[@]}"; do [ "$c" != loaded ] || want_loaded=1; done
  if [ "$want_loaded" -eq 1 ]; then
    [ -n "$URL" ] || die "--url is required for the loaded condition"
    require_match "--url" "$URL" "$URL_RE"
    # fandhe の Page.navigate は loopback・プライベート・file: を SSRF ガードで拒否するため測定前に弾く
    local url_host="${URL#*://}"
    url_host="${url_host%%[/?#]*}"
    case "$url_host" in
      localhost | localhost:* | *.localhost | *.localhost:* | 127.* | 10.* | 192.168.* | 169.254.* | 0.* | 172.1[6-9].* | 172.2[0-9].* | 172.3[01].*)
        die "--url host is loopback or private; use a public URL (SSRF guard)" ;;
    esac
  fi

  # --- 前提の検査（失敗は exit 2） ---
  local tool
  for tool in jq awk uname mktemp "$DOCKER"; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required but was not found on PATH"
  done
  [ "$(uname -s)" = Linux ] || die "this harness supports Linux only (cgroup v2 on the host)"
  "$DOCKER" version --format '{{.Server.Version}}' >/dev/null 2>&1 || die "docker daemon is not reachable"
  local cg_ver cg_drv
  cg_ver=$("$DOCKER" info --format '{{.CgroupVersion}}' 2>/dev/null) || die "docker info failed"
  cg_drv=$("$DOCKER" info --format '{{.CgroupDriver}}' 2>/dev/null) || die "docker info failed"
  cg_ver="${cg_ver//$'\r'/}"
  cg_drv="${cg_drv//$'\r'/}"
  [ "$cg_ver" = 2 ] || die "cgroup v2 is required (docker reports a different version)"
  [[ "$cg_drv" =~ ^[A-Za-z0-9_-]{1,32}$ ]] || cg_drv="unknown"
  grep -qw memory "${CGROUP_ROOT}/cgroup.controllers" 2>/dev/null || die "the memory controller is not available under the cgroup root"
  [ -r "$MEMINFO" ] || die "meminfo is not readable"
  mem_used_pct "$MEMINFO" >/dev/null || die "meminfo has no MemTotal/MemAvailable"
  [ -r "$NAVIGATE_JS" ] || die "navigate.mjs was not found"
  if [ "$TARGET" = fandhe ]; then
    "$DOCKER" image inspect --format '{{.Id}}' "$IMAGE" >/dev/null 2>&1 || die "image not found locally (this harness does not pull the image under test)"
  fi

  HELPER_PIN="${HELPER_IMAGE%:*}@${HELPER_DIGEST}"
  if ! "$DOCKER" image inspect --format '{{.Id}}' "$HELPER_PIN" >/dev/null 2>&1; then
    echo "pulling helper image: ${HELPER_PIN}" >&2
    "$DOCKER" pull "$HELPER_PIN" >&2 || { echo "error: docker pull failed for the helper image" >&2; exit 1; }
  fi
  local helper_info
  helper_info=$("$DOCKER" image inspect --format '{{json .}}' "$HELPER_PIN") || { echo "error: helper image inspect failed" >&2; exit 1; }
  if ! digest_matches "$helper_info" "$HELPER_PIN" "$HELPER_DIGEST"; then
    echo "error: helper image digest does not match the pinned digest (fail-closed)" >&2
    exit 1
  fi
  CHROMIUM_PATH=""
  if [ "$TARGET" = chromium ]; then
    # 比較対象は digest 指定で取得し、取得後に照合する（container-size と同じ参照イメージ。CTR-1）
    if ! "$DOCKER" image inspect --format '{{.Id}}' "$REF_PIN" >/dev/null 2>&1; then
      echo "pulling reference image: ${REF_PIN}" >&2
      "$DOCKER" pull "$REF_PIN" >&2 || { echo "error: docker pull failed for the reference image" >&2; exit 1; }
    fi
    local ref_info
    ref_info=$("$DOCKER" image inspect --format '{{json .}}' "$REF_PIN") || { echo "error: reference image inspect failed" >&2; exit 1; }
    if ! digest_matches "$ref_info" "$REF_PIN" "$REF_DIGEST"; then
      echo "error: reference image digest does not match the pinned digest (fail-closed)" >&2
      exit 1
    fi
    resolve_chromium_path
  fi

  TIMING_UNIT="ms"
  [ -n "${EPOCHREALTIME:-}" ] || TIMING_UNIT="s"
  IMAGE_INFO=$("$DOCKER" image inspect --format '{{json .}}' "$IMAGE") || { echo "error: image inspect failed" >&2; exit 1; }

  WORK=$(mktemp -d)
  RUN_ID=$(printf '%s%04x%04x' "$(date +%s)" "$RANDOM" "$RANDOM")
  PEAK_CHECKED=""
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  local n_seq
  n_seq=$(expand_n_list "$N_LIST" "$MAX_N" "$DOUBLING")
  local cond n reason
  for cond in "${CONDS[@]}"; do
    : >"${WORK}/steps-${cond}.ndjson"
    reason=""
    for n in $n_seq; do
      echo "step: condition=${cond} n=${n}" >&2
      run_step "$cond" "$n"
      if [ "$STEP_COMPLETE" -eq 0 ]; then reason=host_memory_limit; break; fi
      if [ "$STEP_TOTAL" -gt 0 ]; then reason=breakdown; break; fi
      if pct_ge "$STEP_HOST_PCT" "$HOST_LIMIT_PCT"; then reason=host_memory_limit; break; fi
    done
    if [ -z "$reason" ]; then
      if [ "$DOUBLING" -eq 1 ]; then reason=max_n; else reason=completed; fi
    fi
    jq -n -c --arg c "$cond" --arg r "$reason" --slurpfile s "${WORK}/steps-${cond}.ndjson" \
      '{condition: $c, stop_reason: $r, steps: $s}' >"${WORK}/cond-${cond}.json"
  done

  local mem_total kernel arch dsrv dcli
  mem_total=$(LC_ALL=C awk '/^MemTotal:/ { print $2 * 1024; exit }' "$MEMINFO")
  kernel=$(uname -r)
  arch=$(uname -m)
  dsrv=$("$DOCKER" version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)
  dcli=$("$DOCKER" version --format '{{.Client.Version}}' 2>/dev/null || echo unknown)

  local CARGS_JSON="[]"
  [ "$TARGET" != chromium ] || CARGS_JSON=$(printf '%s\n' "${CHROMIUM_ARGS[@]}" | jq -R . | jq -s -c .)
  : >"${WORK}/conds.ndjson"
  for cond in "${CONDS[@]}"; do cat "${WORK}/cond-${cond}.json" >>"${WORK}/conds.ndjson"; done
  jq -n --slurpfile conds "${WORK}/conds.ndjson" \
    --arg image "$IMAGE" --arg nl "$N_LIST" --argjson dbl "$DOUBLING" --argjson maxn "$MAX_N" \
    --arg condstr "$CONDITIONS" --arg url "$URL" --argjson rt "$READY_TIMEOUT" --argjson hl "$HOST_LIMIT_PCT" \
    --argjson settle "$SETTLE" --argjson nc "$NAV_CONC" --arg helper "$HELPER_PIN" --argjson info "$IMAGE_INFO" \
    --arg target "$TARGET" --arg cpath "$CHROMIUM_PATH" --arg cuser "$CHROMIUM_USER" --arg refpin "${REF_PIN:-}" \
    --argjson cargs "$CARGS_JSON" \
    --arg kernel "$kernel" --arg arch "$arch" --arg dsrv "$dsrv" --arg dcli "$dcli" --arg cgv "$cg_ver" \
    --arg cgd "$cg_drv" --argjson mem "${mem_total:-0}" --arg res "$TIMING_UNIT" '
    # max_ok_n: 完走（complete）かつ破綻 0 のステップの最大 N。該当が無ければ null（CTR-7 の破綻点の読み取り用）
    def ok_max: [.steps[] | select(.complete and .breakdowns.total == 0) | .n] | max;
    {schema: "container-memory/v1", task: "TASK-67", target: $target,
     behaviors: ["CTR-3", "CTR-6", "CTR-7", "MEAS-6"],
     args: {image: $image, n_list: ($nl | split(",") | map(tonumber)), doubling: ($dbl == 1), max_n: $maxn,
            conditions: ($condstr | split(",")), url: (if $url == "" then null else $url end),
            ready_timeout_s: $rt, host_mem_limit_pct: $hl, settle_s: $settle, navigate_concurrency: $nc,
            helper_image: $helper},
     image: {ref: $image, id: $info.Id, repo_digests: ($info.RepoDigests // []),
             pinned_digest: (if $target == "chromium" then $refpin else null end)},
     launch: (if $target == "chromium"
              then {entrypoint: $cpath, user: $cuser, init: true, args: $cargs}
              else {entrypoint: null, user: null, init: false, args: []} end),
     environment: {kernel: $kernel, arch: $arch, docker_server_version: $dsrv, docker_client_version: $dcli,
                   cgroup_version: $cgv, cgroup_driver: $cgd, mem_total_bytes: $mem, timing_resolution: $res},
     conditions: ($conds | map(. + {max_ok_n: ok_max}))}' >"${WORK}/result.json"

  if [ -n "$OUT" ]; then
    local tmp
    tmp=$(mktemp "${OUT}.XXXXXX") || die "cannot create the output file"
    cp "${WORK}/result.json" "$tmp"
    mv "$tmp" "$OUT"
  else
    cat "${WORK}/result.json"
  fi
}

# self-test から関数を source できるよう、直接実行時のみ main を呼ぶ。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
