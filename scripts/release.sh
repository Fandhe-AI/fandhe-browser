#!/usr/bin/env bash
# crates.io 公開の制御（#1531）。`.github/workflows/release.yml` の verify・publish ジョブが呼ぶ。
# 公開対象 8 crate と公開順は publish-crates.sh が正本。
#
# 使い方: scripts/release.sh <plan|dry-run|publish|index-path NAME|self-test>
#   plan      入力の検証と公開状況の判定（トークン不要）。結果を RELEASE_DRYRUN・RELEASE_PENDING として
#             GITHUB_ENV（未設定なら標準出力）へ出す
#   dry-run   plan が出した RELEASE_DRYRUN の crate 群に対し `cargo publish --dry-run --locked` を実行する
#   publish   plan が出した RELEASE_PENDING を公開順に `cargo publish --no-verify` する（公開済みは飛ばす。
#             レート制限 429 は待って再試行する。要 CARGO_REGISTRY_TOKEN）
#   check-environment  Environment crates-io-release が存在し required_reviewers の保護ルールを持つことを
#             GitHub API で確かめる（mode=publish の verify で実行。要 GITHUB_TOKEN・GITHUB_REPOSITORY）
#   self-test cargo の公開 API を模した shim と合成 index で上の判定を確認する（ネットワーク不要）
# 入力は環境変数（式を run に展開しない）: RELEASE_CRATE（8 crate 名 または all）・RELEASE_VERSION・RELEASE_MODE
#
# 途中再開の方針（crates.io への公開は取り消せず、8 crate の公開は途中で止まり得るため）:
#   - crate=all: 一部が公開済みでも続行し、未公開分だけを対象にする。全件公開済みなら失敗
#   - crate=単一: 公開済みなら失敗
#   - dry-run は「未公開の全 crate」を一括で行う（未公開の workspace 依存を registry から解決できない
#     ため、単一 crate を指定しても未公開の依存ぶんを同時に検証する。cargo は一括指定の crate 同士を
#     ローカルの一時 registry で解決する）
#   - mode=publish は main ブランチ（GITHUB_REF=refs/heads/main）からだけ許す（plan で失敗させる）
#   - mode=publish で公開済みの crate がある場合、その .crate の .cargo_vcs_info.json の git.sha1 が
#     GITHUB_SHA と一致することを要求する（途中再開で別 commit の成果物が混在しないように。読めない・
#     欠落も失敗）
#   - 公開は `cargo publish --no-verify`。token のある環境で依存の build.rs・proc-macro を動かさないため。
#     検証は同じ commit の verify・verify-macos の dry-run（検証ビルドあり）が済ませている
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
  # JSON として読めない・予期しない形は「未公開」ではなく判定不能として失敗に倒す
  local verdict
  verdict="$(printf '%s\n' "${body}" | jq -s -r --arg v "$2" 'if length == 0 then error("empty index") else any(.[]; .vers == $v) end' 2>/dev/null)" || {
    echo "error: could not parse the crates.io index entry for $1" >&2
    return 1
  }
  case "${verdict}" in
    true) echo yes ;;
    false) echo no ;;
    *) echo "error: unexpected index verdict for $1: '${verdict}'" >&2; return 1 ;;
  esac
}

# fetch_crate NAME VERSION DEST: 公開済みの .crate を取得する。RELEASE_CRATE_DIR があればそこから読む（self-test 用）。
fetch_crate() {
  if [ -n "${RELEASE_CRATE_DIR:-}" ]; then
    cp "${RELEASE_CRATE_DIR}/$1-$2.crate" "$3"
    return
  fi
  curl -fsSL --max-time 120 --retry 3 -o "$3" "https://static.crates.io/crates/$1/$1-$2.crate"
}

# crate_commit NAME VERSION: 公開済み .crate の .cargo_vcs_info.json にある git.sha1 を出力する。読めなければ失敗。
crate_commit() {
  local f sha
  f="$(mktemp)"
  if ! fetch_crate "$1" "$2" "${f}" 2>/dev/null; then
    rm -f "${f}"
    return 1
  fi
  sha="$(tar -xzOf "${f}" "$1-$2/.cargo_vcs_info.json" 2>/dev/null | jq -r '.git.sha1 // empty' 2>/dev/null)" || sha=""
  rm -f "${f}"
  [[ "${sha}" =~ ^[0-9a-f]{40}$ ]] || return 1
  echo "${sha}"
}

# check_environment: Environment crates-io-release の存在と required_reviewers を確かめる。
# Environment が無いと publish ジョブの初回実行で保護ルールなしに自動作成されるため、verify の段階で止める。
# 判定できない（権限不足・応答不正・通信失敗）場合も失敗に倒す。応答は GET /repos/{owner}/{repo}/environments/{name}
# の `protection_rules[]`（`type: required_reviewers` と `reviewers[]`）。RELEASE_ENV_FILE で応答を差し替えられる（self-test 用）。
check_environment() {
  local name=crates-io-release body tmp code
  if [ -n "${RELEASE_ENV_FILE:-}" ]; then
    [ -f "${RELEASE_ENV_FILE}" ] || die "Environment ${name} does not exist"
    body="$(cat "${RELEASE_ENV_FILE}")"
  else
    : "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}" "${GITHUB_TOKEN:?GITHUB_TOKEN is required}"
    tmp="$(mktemp)"
    code="$(curl -sS --max-time 30 -o "${tmp}" -w '%{http_code}' \
      -H "Authorization: Bearer ${GITHUB_TOKEN}" -H "Accept: application/vnd.github+json" \
      -H "X-GitHub-Api-Version: 2022-11-28" \
      "${GITHUB_API_URL:-https://api.github.com}/repos/${GITHUB_REPOSITORY}/environments/${name}")" || {
      rm -f "${tmp}"
      die "could not query the Environment ${name} (network error)"
    }
    body="$(cat "${tmp}")"
    rm -f "${tmp}"
    case "${code}" in
      200) ;;
      404) die "Environment ${name} does not exist (or the token cannot see it); create it with required reviewers first" ;;
      *) die "could not read the Environment ${name} (HTTP ${code}); the token needs actions: read" ;;
    esac
  fi
  if ! printf '%s\n' "${body}" | jq -e '(.protection_rules // []) | any(.[]; .type == "required_reviewers" and ((.reviewers // []) | length) > 0)' >/dev/null 2>&1; then
    die "Environment ${name} has no required_reviewers protection rule (or the response is unreadable)"
  fi
  echo "OK: Environment ${name} has a required_reviewers protection rule"
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
  # 実公開は main の commit からだけ許す（未レビューのブランチから公開しない）。未設定も失敗（fail-closed）
  if [ "${RELEASE_MODE}" = publish ] && [ "${GITHUB_REF:-}" != "refs/heads/main" ]; then
    die "mode=publish is allowed only from refs/heads/main (GITHUB_REF='${GITHUB_REF:-}')"
  fi

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

  # 途中再開で別 commit の成果物が混在しないよう、公開済みの crate は同じ commit から公開されたものに限る
  if [ "${RELEASE_MODE}" = publish ]; then
    local got
    i=0
    for c in "${PUBLIC_CRATES[@]}"; do
      if [ "${state[$i]}" = yes ]; then
        : "${GITHUB_SHA:?GITHUB_SHA is required to resume a publish}"
        got="$(crate_commit "${c}" "${RELEASE_VERSION}")" \
          || die "could not read the source commit of published ${c} ${RELEASE_VERSION} (.cargo_vcs_info.json)"
        [ "${got}" = "${GITHUB_SHA}" ] \
          || die "published ${c} ${RELEASE_VERSION} came from commit ${got}, not ${GITHUB_SHA}; resume from that commit"
      fi
      i=$((i + 1))
    done
  fi

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
      if in_list "${d}" ${dryrun[@]+"${dryrun[@]}"}; then
        if [ "${RELEASE_MODE}" = publish ]; then
          die "${RELEASE_CRATE} depends on ${d}, which is not published at ${RELEASE_VERSION}; publish with crate=all (or the dependency first)"
        fi
        echo "::notice::${RELEASE_CRATE} depends on unpublished ${d}; mode=publish would require it to be published first"
      fi
    done
  fi
  local dryrun_s="" pending_s=""
  [ "${#dryrun[@]}" -eq 0 ] || dryrun_s="${dryrun[*]}"
  [ "${#pending[@]}" -eq 0 ] || pending_s="${pending[*]}"
  echo "release plan: dry-run=[${dryrun_s}] publish=[${pending_s}]"
  emit RELEASE_DRYRUN "${dryrun_s}"
  emit RELEASE_PENDING "${pending_s}"
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
    in_list "${c}" ${pending_list[@]+"${pending_list[@]}"} || continue
    attempt=1
    while :; do
      # 再実行・直前の失敗後に公開済みになっていれば飛ばす（途中再開）
      s="$(published "${c}" "${RELEASE_VERSION}")" || exit 1
      if [ "${s}" = yes ]; then
        echo "skip: ${c} ${RELEASE_VERSION} is already published"
        break
      fi
      echo "publishing ${c} ${RELEASE_VERSION} (attempt ${attempt}/${max})"
      # --no-verify: token を持つこの環境で依存の build.rs・proc-macro を動かさない。検証ビルドは同じ commit の
      # verify・verify-macos の dry-run が済ませている（mode=publish は main だけ・commit 一致も plan が確認する）
      if cargo publish -p "${c}" --locked --no-verify 2>&1 | tee "${out}"; then
        break
      fi
      # tee 越しの終了コードは pipefail で cargo のものになる。公開済みになっていれば成功扱い
      s="$(published "${c}" "${RELEASE_VERSION}")" || exit 1
      [ "${s}" = yes ] && break
      # cargo の表示（crates-io の Error::Api: `the remote server responded with an error (status 429 Too Many
      # Requests): ...`、Error::Code: `failed to get a 200 OK response, got 429`）に絞る。数字の偶然一致で再試行しない
      if grep -Eq 'status 429 Too Many Requests|failed to get a 200 OK response, got 429' "${out}" \
        && [ "${attempt}" -lt "${max}" ]; then
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
  mkdir -p "${tmp}/bin" "${tmp}/index" "${tmp}/crates"
  local version
  version="$("${real_cargo}" metadata --no-deps --format-version 1 --locked | jq -r '.packages[] | select(.name == "fandhe-browser-cli") | .version')"

  # cargo の shim: publish だけ模倣し（index へ書き込む。failmsg-<crate> があればその文面を出して 1 回だけ失敗する）、他は本物へ渡す。
  cat >"${tmp}/bin/cargo" <<SHIM
#!/usr/bin/env bash
if [ "\$1" = publish ]; then
  name=""; prev=""
  for a in "\$@"; do [ "\$prev" = -p ] && name="\$a"; prev="\$a"; done
  echo "\$*" >>"${tmp}/publish.log"
  if [ -f "${tmp}/failmsg-\${name}" ]; then
    cat "${tmp}/failmsg-\${name}"
    rm -f "${tmp}/failmsg-\${name}"
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
        RELEASE_CRATE_DIR="${tmp}/crates" GITHUB_REF=refs/heads/main GITHUB_SHA="${sha}" \
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
  mark() { # mark NAME [SHA]: 公開済みにする（index と、.cargo_vcs_info.json 入りの .crate を作る）
    local rel d="${tmp}/crates/src/$1-${version}"
    rel="$(index_path "$1")"
    mkdir -p "${tmp}/index/$(dirname "${rel}")" "${d}"
    echo '{"name":"'"$1"'","vers":"'"${version}"'","yanked":false}' >"${tmp}/index/${rel}"
    printf '{"git":{"sha1":"%s"},"path_in_vcs":"x"}\n' "${2:-${sha}}" >"${d}/.cargo_vcs_info.json"
    tar -czf "${tmp}/crates/$1-${version}.crate" -C "${tmp}/crates/src" "$1-${version}"
  }
  local sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
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
  run "publish from a non-main ref" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish GITHUB_REF=refs/heads/feature
  has "non-main message" "only from refs/heads/main"
  run "publish from a tag ref" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish GITHUB_REF=refs/tags/v0.1.0
  run "publish with GITHUB_REF unset" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish GITHUB_REF=
  run "dry-run from a non-main ref" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only GITHUB_REF=refs/heads/feature
  # index の JSON が壊れていたら未公開とみなさず失敗する
  mkdir -p "${tmp}/index/$(dirname "$(index_path fandhe-browser-mcp)")"
  echo 'not json' >"${tmp}/index/$(index_path fandhe-browser-mcp)"
  run "unparsable index entry" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only
  has "unparsable index message" "could not parse"
  rm -f "${tmp}/index/$(index_path fandhe-browser-mcp)"
  # 一部公開済み
  mark fandhe-browser-profile
  mark fandhe-browser-js
  run "resume all" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  has "resume skips published" "publish=\[fandhe-browser-core "
  run "single already published" fail RELEASE_CRATE=fandhe-browser-js RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only
  run "resume from another commit" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish GITHUB_SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  has "commit mismatch message" "came from commit"
  run "dry-run does not compare commits" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=dry-run-only GITHUB_SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  mark fandhe-browser-js cccccccccccccccccccccccccccccccccccccccc
  run "one published crate from another commit" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  mark fandhe-browser-js
  rm -f "${tmp}/crates/fandhe-browser-js-${version}.crate"
  run "published .crate unreadable" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  has "unreadable message" "could not read the source commit"
  printf '{"path_in_vcs":"x"}\n' >"${tmp}/crates/src/fandhe-browser-js-${version}/.cargo_vcs_info.json"
  tar -czf "${tmp}/crates/fandhe-browser-js-${version}.crate" -C "${tmp}/crates/src" "fandhe-browser-js-${version}"
  run "published .crate without git sha1" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  mark fandhe-browser-js
  # publish: 429 を 1 回挟んで再開できる（profile・js は公開済みで飛ばす）
  printf 'error: failed to publish to registry at https://crates.io\n\nCaused by:\n  the remote server responded with an error (status 429 Too Many Requests): You have published too many new crates in a short period of time. Please try again after Fri, 06 Nov 2026 12:00:00 GMT\n' >"${tmp}/failmsg-fandhe-browser-ai"
  SELF_CMD=publish
  run "publish with 429 retry" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish RELEASE_PENDING="fandhe-browser-core fandhe-browser-ai fandhe-browser-cdp fandhe-browser-render fandhe-browser-cli fandhe-browser-mcp"
  has "retry message" "rate limited"
  if [ "$(grep -c . "${tmp}/publish.log")" = 7 ] && ! grep -q 'fandhe-browser-profile' "${tmp}/publish.log"; then
    echo "OK(self-test): publish order and skip (7 attempts incl. one retry)"
  else
    echo "NG(self-test): unexpected publish log:" >&2; sed 's/^/    /' "${tmp}/publish.log" >&2; fail=1
  fi
  if grep -q -- '--no-verify' "${tmp}/publish.log" && [ "$(grep -vc -- '--no-verify' "${tmp}/publish.log")" = 0 ]; then
    echo "OK(self-test): every cargo publish uses --no-verify"
  else
    echo "NG(self-test): cargo publish without --no-verify" >&2; fail=1
  fi
  # 数字の偶然一致（429 を含む別の文面）では再試行せず失敗する
  rm -f "${tmp}/index/$(index_path fandhe-browser-cdp)"
  printf 'error: failed to publish: archive is 14290 bytes\n' >"${tmp}/failmsg-fandhe-browser-cdp"
  run "non-429 failure is not retried" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish RELEASE_PENDING="fandhe-browser-cdp"
  if grep -q 'rate limited' "${tmp}/out"; then echo "NG(self-test): non-429 failure was retried" >&2; fail=1; else echo "OK(self-test): no retry on non-429"; fi
  printf 'error: failed to get a 200 OK response, got 429\nheaders:\n' >"${tmp}/failmsg-fandhe-browser-cdp"
  run "429 (Code variant) is retried" ok RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish RELEASE_PENDING="fandhe-browser-cdp"
  has "retried" "rate limited"
  SELF_CMD=plan
  for c in "${PUBLIC_CRATES[@]}"; do mark "${c}"; done
  run "all published -> fail" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish
  has "all published message" "already published"
  SELF_CMD=publish
  run "publish requires token" fail RELEASE_CRATE=all RELEASE_VERSION="${version}" RELEASE_MODE=publish RELEASE_PENDING="fandhe-browser-core" CARGO_REGISTRY_TOKEN=

  # Environment の保護ルール検査（応答は GET environments/{name} の形）
  SELF_CMD=check-environment
  echo '{"name":"crates-io-release","protection_rules":[{"id":1,"type":"required_reviewers","reviewers":[{"type":"User","reviewer":{"login":"o","id":1}}]}]}' >"${tmp}/env-ok.json"
  echo '{"name":"crates-io-release","protection_rules":[{"id":1,"type":"wait_timer","wait_timer":5}]}' >"${tmp}/env-norule.json"
  echo '{"name":"crates-io-release","protection_rules":[{"id":1,"type":"required_reviewers","reviewers":[]}]}' >"${tmp}/env-empty.json"
  echo '{"name":"crates-io-release"}' >"${tmp}/env-none.json"
  echo '<html>' >"${tmp}/env-garbage.json"
  run "environment with required reviewers" ok RELEASE_ENV_FILE="${tmp}/env-ok.json"
  run "environment with only a wait timer" fail RELEASE_ENV_FILE="${tmp}/env-norule.json"
  run "environment with empty reviewers" fail RELEASE_ENV_FILE="${tmp}/env-empty.json"
  run "environment without protection_rules" fail RELEASE_ENV_FILE="${tmp}/env-none.json"
  run "environment response unreadable" fail RELEASE_ENV_FILE="${tmp}/env-garbage.json"
  run "environment missing (404)" fail RELEASE_ENV_FILE="${tmp}/env-absent.json"
  run "environment check without a token" fail GITHUB_REPOSITORY=o/r GITHUB_TOKEN=

  [ "${fail}" -eq 0 ] || { echo "NG: release.sh self-test failed" >&2; exit 1; }
  echo "OK: release.sh self-test passed"
}

case "${1:-}" in
  plan) plan ;;
  dry-run) dry_run ;;
  publish) publish ;;
  check-environment) check_environment ;;
  index-path) index_path "${2:?name required}" ;;
  self-test) self_test ;;
  *) echo "usage: $(basename "$0") <plan|dry-run|publish|check-environment|self-test>" >&2; exit 2 ;;
esac
