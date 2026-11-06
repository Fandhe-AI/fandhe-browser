#!/usr/bin/env bash
# crates.io 公開の制御（#1531）。`.github/workflows/release.yml` の verify・publish ジョブが呼ぶ。
# 公開対象 8 crate と公開順は publish-crates.sh が正本。
#
# 使い方: scripts/release.sh <plan|dry-run|publish|index-path NAME|self-test>
#   plan      入力の検証と公開状況の判定（トークン不要）。結果を RELEASE_DRYRUN・RELEASE_PENDING として
#             GITHUB_ENV（未設定なら標準出力）へ出す
#   dry-run   plan が出した RELEASE_DRYRUN の crate 群に対し `cargo publish --dry-run --locked` を実行する
#   publish   plan が出した RELEASE_PENDING を公開順に `cargo publish` する（公開済みは飛ばす。
#             レート制限 429 は待って再試行する。要 CARGO_REGISTRY_TOKEN）
#   self-test cargo の公開 API を模した shim と合成 index で上の判定を確認する（ネットワーク不要）
# 入力は環境変数（式を run に展開しない）: RELEASE_CRATE（8 crate 名 または all）・RELEASE_VERSION・RELEASE_MODE
#
# 途中再開の方針（crates.io への公開は取り消せず、8 crate の公開は途中で止まり得るため）:
#   - crate=all: 一部が公開済みでも続行し、未公開分だけを対象にする。全件公開済みなら失敗
#   - crate=単一: 公開済みなら失敗
#   - dry-run は「未公開の全 crate」を一括で行う（未公開の workspace 依存を registry から解決できない
#     ため、単一 crate を指定しても未公開の依存ぶんを同時に検証する。cargo は一括指定の crate 同士を
#     ローカルの一時 registry で解決する）
#   - mode=publish の単一指定は、その crate の workspace 依存（dev 依存を含む）が公開済みであること
#     を要求する（公開時の依存解決に必要）
# レート制限: crates.io の新規 crate 公開は burst 5 件、以後 10 分に 1 件（crates.io の rate_limiter.rs）。
# 8 crate の初回公開では 6 件目以降が 429 になるため、待って再試行する。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
# shellcheck source=./publish-crates.sh
source "${DIR}/publish-crates.sh"

SEMVER_RE='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
INDEX_URL="${RELEASE_INDEX_URL:-https://index.crates.io}"

die() {
  echo "::error::$*" >&2
  exit 1
}

# index_path NAME: sparse index 上の相対パス（crates.io の規則）。
index_path() {
  local n="$1"
  case "${#n}" in
    1) echo "1/${n}" ;;
    2) echo "2/${n}" ;;
    3) echo "3/${n:0:1}/${n}" ;;
    *) echo "${n:0:2}/${n:2:2}/${n}" ;;
  esac
}

# fetch_index REL: index ファイルの本文を出力する。0=取得 3=存在しない（未公開） その他=エラー。
# RELEASE_INDEX_DIR を指定すると、ネットワークの代わりにそのディレクトリを index として読む（self-test 用）。
fetch_index() {
  local rel="$1" tmp code
  if [ -n "${RELEASE_INDEX_DIR:-}" ]; then
    [ -f "${RELEASE_INDEX_DIR}/${rel}" ] || return 3
    cat "${RELEASE_INDEX_DIR}/${rel}"
    return 0
  fi
  tmp="$(mktemp)"
  code="$(curl -sS --max-time 30 --retry 3 -o "${tmp}" -w '%{http_code}' "${INDEX_URL}/${rel}")" || {
    rm -f "${tmp}"
    return 2
  }
  case "${code}" in
    200) cat "${tmp}"; rm -f "${tmp}"; return 0 ;;
    404) rm -f "${tmp}"; return 3 ;;
    *) rm -f "${tmp}"; echo "error: unexpected HTTP ${code} from the crates.io index" >&2; return 2 ;;
  esac
}

# published NAME VERSION: yes / no を出力する。判定できなければ失敗（fail-closed）。
# yank 済みの版も再公開できないため公開済みとして扱う。
published() {
  local body rc=0
  body="$(fetch_index "$(index_path "$1")")" || rc=$?
  case "${rc}" in
    0) ;;
    3) echo no; return 0 ;;
    *) echo "error: could not read the crates.io index for $1" >&2; return 1 ;;
  esac
  if printf '%s\n' "${body}" | jq -s -e --arg v "$2" 'any(.[]; .vers == $v)' >/dev/null 2>&1; then
    echo yes
  else
    echo no
  fi
}

# emit KEY VALUE: GITHUB_ENV があればそこへ、無ければ標準出力へ出す。
emit() {
  echo "$1=$2"
  if [ -n "${GITHUB_ENV:-}" ]; then echo "$1=$2" >>"${GITHUB_ENV}"; fi
}

in_list() { # in_list NEEDLE LIST...
  local n="$1" x
  shift
  for x in "$@"; do [ "${x}" = "${n}" ] && return 0; done
  return 1
}

plan() {
  : "${RELEASE_CRATE:?RELEASE_CRATE is required}" "${RELEASE_VERSION:?RELEASE_VERSION is required}" "${RELEASE_MODE:?RELEASE_MODE is required}"
  require_cmd jq "required by release.sh"
  [[ "${RELEASE_VERSION}" =~ ${SEMVER_RE} ]] || die "invalid version format: '${RELEASE_VERSION}' (expected semver, e.g. 0.1.0)"
  case "${RELEASE_MODE}" in dry-run-only | publish) ;; *) die "invalid mode: '${RELEASE_MODE}'" ;; esac

  local targets=()
  if [ "${RELEASE_CRATE}" = all ]; then
    targets=("${PUBLIC_CRATES[@]}")
  elif in_list "${RELEASE_CRATE}" "${PUBLIC_CRATES[@]}"; then
    targets=("${RELEASE_CRATE}")
  else
    die "crate '${RELEASE_CRATE}' is not in the public crate list"
  fi

  local meta c v
  meta="$(cargo metadata --no-deps --format-version 1 --locked)" || die "cargo metadata failed"
  for c in "${targets[@]}"; do
    v="$(printf '%s\n' "${meta}" | jq -r --arg n "${c}" '.packages[] | select(.name == $n) | .version')"
    [ "${v}" = "${RELEASE_VERSION}" ] \
      || die "version mismatch: ${c} Cargo.toml has '${v}' but input version is '${RELEASE_VERSION}'"
  done

  # 全 8 crate の公開状況（未公開の全 crate は dry-run 対象。途中再開でも一括で検証する）
  local dryrun=() pending=() s
  local state=()
  for c in "${PUBLIC_CRATES[@]}"; do
    s="$(published "${c}" "${RELEASE_VERSION}")" || exit 1
    state+=("${s}")
    [ "${s}" = no ] && dryrun+=("${c}")
  done
  local i=0
  for c in "${PUBLIC_CRATES[@]}"; do
    if in_list "${c}" "${targets[@]}" && [ "${state[$i]}" = no ]; then pending+=("${c}"); fi
    i=$((i + 1))
  done

  if [ "${RELEASE_CRATE}" = all ]; then
    [ "${#pending[@]}" -gt 0 ] || die "all public crates are already published at ${RELEASE_VERSION}"
    [ "${#pending[@]}" -eq "${#PUBLIC_CRATES[@]}" ] \
      || echo "::notice::resuming: already published crates are skipped (pending: ${pending[*]})"
  else
    [ "${#pending[@]}" -gt 0 ] || die "${RELEASE_CRATE} ${RELEASE_VERSION} is already published"
    # 単一指定: workspace 依存（dev 依存を含む）が公開済みでないと、公開時の依存解決に失敗する
    local deps d
    deps="$(printf '%s\n' "${meta}" | jq -r --arg n "${RELEASE_CRATE}" '.packages[] | select(.name == $n) | .dependencies[] | select(.path != null) | .name' | sort -u)"
    for d in ${deps}; do
      if in_list "${d}" "${dryrun[@]}"; then
        if [ "${RELEASE_MODE}" = publish ]; then
          die "${RELEASE_CRATE} depends on ${d}, which is not published at ${RELEASE_VERSION}; publish with crate=all (or the dependency first)"
        fi
        echo "::notice::${RELEASE_CRATE} depends on unpublished ${d}; mode=publish would require it to be published first"
      fi
    done
  fi
  echo "release plan: dry-run=[${dryrun[*]}] publish=[${pending[*]}]"
  emit RELEASE_DRYRUN "${dryrun[*]}"
  emit RELEASE_PENDING "${pending[*]}"
}

dry_run() {
  : "${RELEASE_DRYRUN:?run plan first}"
  local args=() c
  for c in ${RELEASE_DRYRUN}; do args+=(-p "${c}"); done
  [ "${#args[@]}" -gt 0 ] || die "nothing to dry-run"
  cargo publish --dry-run --locked "${args[@]}"
}

publish() {
  : "${RELEASE_PENDING:?run plan first}" "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is required}"
  local max="${RELEASE_MAX_ATTEMPTS:-12}" wait_s="${RELEASE_RETRY_SLEEP:-300}" c s attempt out
  local pending_list=()
  read -r -a pending_list <<<"${RELEASE_PENDING}"
  out="$(mktemp)"
  for c in "${PUBLIC_CRATES[@]}"; do
    in_list "${c}" "${pending_list[@]}" || continue
    attempt=1
    while :; do
      # 再実行・直前の失敗後に公開済みになっていれば飛ばす（途中再開）
      s="$(published "${c}" "${RELEASE_VERSION}")" || exit 1
      if [ "${s}" = yes ]; then
        echo "skip: ${c} ${RELEASE_VERSION} is already published"
        break
      fi
      echo "publishing ${c} ${RELEASE_VERSION} (attempt ${attempt}/${max})"
      if cargo publish -p "${c}" --locked 2>&1 | tee "${out}"; then
        break
      fi
      # tee 越しの終了コードは pipefail で cargo のものになる。公開済みになっていれば成功扱い
      s="$(published "${c}" "${RELEASE_VERSION}")" || exit 1
      [ "${s}" = yes ] && break
      if grep -q '429' "${out}" && [ "${attempt}" -lt "${max}" ]; then
        echo "rate limited by crates.io; waiting ${wait_s}s before retrying ${c}"
        sleep "${wait_s}"
        attempt=$((attempt + 1))
        continue
      fi
      rm -f "${out}"
      die "failed to publish ${c} ${RELEASE_VERSION}"
    done
  done
  rm -f "${out}"
  echo "publish finished: ${RELEASE_PENDING}"
}

self_test() {
  require_cmd jq "required by self-test"
  local tmp fail=0 real_cargo
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT
  real_cargo="$(command -v cargo)"
  mkdir -p "${tmp}/bin" "${tmp}/index"
  local version
  version="$("${real_cargo}" metadata --no-deps --format-version 1 --locked | jq -r '.packages[] | select(.name == "fandhe-browser-cli") | .version')"

  # cargo の shim: publish だけ模倣し（index へ書き込む。FAIL429_<crate> があれば 1 回だけ 429）、他は本物へ渡す。
  cat >"${tmp}/bin/cargo" <<SHIM
#!/usr/bin/env bash
if [ "\$1" = publish ]; then
  name=""; prev=""
  for a in "\$@"; do [ "\$prev" = -p ] && name="\$a"; prev="\$a"; done
  echo "\$*" >>"${tmp}/publish.log"
  if [ -f "${tmp}/fail429-\${name}" ]; then
    rm -f "${tmp}/fail429-\${name}"
    echo "error: failed to publish: 429 Too Many Requests"
    exit 101
  fi
  rel="\$(bash "${DIR}/release.sh" index-path "\${name}")"
  mkdir -p "${tmp}/index/\$(dirname "\${rel}")"
  echo '{"name":"'"\${name}"'","vers":"${version}","yanked":false}' >>"${tmp}/index/\${rel}"
  exit 0
fi
exec "${real_cargo}" "\$@"
SHIM
  chmod +x "${tmp}/bin/cargo"

  # run LABEL EXPECT(ok|fail) ARGS... : 環境を整えて release.sh を呼ぶ。出力は ${tmp}/out
  run() {
    local label="$1" expect="$2" rc=0
    shift 2
    ( cd "${ROOT_DIR}" && env PATH="${tmp}/bin:${PATH}" RELEASE_INDEX_DIR="${tmp}/index" RELEASE_RETRY_SLEEP=0 \
        CARGO_REGISTRY_TOKEN=dummy "$@" bash "${DIR}/release.sh" "${SELF_CMD}" ) >"${tmp}/out" 2>&1 || rc=$?
    if { [ "${expect}" = ok ] && [ "${rc}" -eq 0 ]; } || { [ "${expect}" = fail ] && [ "${rc}" -ne 0 ]; }; then
      echo "OK(self-test): ${label}"
    else
      echo "NG(self-test): ${label}: expected ${expect}, rc=${rc}" >&2
      sed 's/^/    /' "${tmp}/out" >&2
      fail=1
    fi
  }
  has() { # has LABEL PATTERN
    if grep -Eq "$2" "${tmp}/out"; then echo "OK(self-test): $1"; else echo "NG(self-test): $1: '$2' not in output" >&2; sed 's/^/    /' "${tmp}/out" >&2; fail=1; fi
  }
  mark() { # mark NAME: 公開済みにする
    local rel
    rel="$(index_path "$1")"
    mkdir -p "${tmp}/index/$(dirname "${rel}")"
    echo '{"name":"'"$1"'","vers":"'"${version}"'","yanked":false}' >"${tmp}/index/${rel}"
  }
  SELF_CMD=plan

  # 入力ガード
  run "bad semver" fail RELEASE_CRATE=all RELEASE_VERSION=1.0 RELEASE_MODE=dry-run-only
  run "unknown crate" fail RELEASE_CRATE=wpt-subset-runner RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only
  run "version mismatch" fail RELEASE_CRATE=all RELEASE_VERSION=9.9.9 RELEASE_MODE=dry-run-only
  # 何も公開されていない
  run "plan all (nothing published)" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only
  has "all pending in order" "publish=\[fandhe-browser-profile fandhe-browser-js fandhe-browser-core fandhe-browser-ai fandhe-browser-cdp fandhe-browser-render fandhe-browser-cli fandhe-browser-mcp\]"
  run "single cli, dry-run-only, deps unpublished" ok RELEASE_CRATE=fandhe-browser-cli RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only
  run "single cli, publish, deps unpublished" fail RELEASE_CRATE=fandhe-browser-cli RELEASE_VERSION="${version}" RELEASE_MODE=publish
  run "single profile, publish" ok RELEASE_CRATE=fandhe-browser-profile RELEASE_VERSION="${version}" RELEASE_MODE=publish
  # 一部公開済み
  mark fandhe-browser-profile
  mark fandhe-browser-js
  run "resume all" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  has "resume skips published" "publish=\[fandhe-browser-core "
  run "single already published" fail RELEASE_CRATE=fandhe-browser-js RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only
  # publish: 429 を 1 回挟んで再開できる（profile・js は公開済みで飛ばす）
  : >"${tmp}/fail429-fandhe-browser-ai"
  SELF_CMD=publish
  run "publish with 429 retry" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish RELEASE_PENDING="fandhe-browser-core fandhe-browser-ai fandhe-browser-cdp fandhe-browser-render fandhe-browser-cli fandhe-browser-mcp"
  has "retry message" "rate limited"
  if [ "$(grep -c . "${tmp}/publish.log")" = 7 ] && ! grep -q 'fandhe-browser-profile' "${tmp}/publish.log"; then
    echo "OK(self-test): publish order and skip (7 attempts incl. one retry)"
  else
    echo "NG(self-test): unexpected publish log:" >&2; sed 's/^/    /' "${tmp}/publish.log" >&2; fail=1
  fi
  SELF_CMD=plan
  run "all published -> fail" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  SELF_CMD=publish
  run "publish requires token" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish RELEASE_PENDING="fandhe-browser-core" CARGO_REGISTRY_TOKEN=

  [ "${fail}" -eq 0 ] || { echo "NG: release.sh self-test failed" >&2; exit 1; }
  echo "OK: release.sh self-test passed"
}

case "${1:-}" in
  plan) plan ;;
  dry-run) dry_run ;;
  publish) publish ;;
  index-path) index_path "${2:?name required}" ;;
  self-test) self_test ;;
  *) echo "usage: $(basename "$0") <plan|dry-run|publish|self-test>" >&2; exit 2 ;;
esac
