#!/usr/bin/env bash
# 実測ハーネス・ベンチ・計測系ターゲットの実体。Makefile の各ターゲットはここへ 1 行で委譲する。
# 使い方: scripts/harness.sh <name>
#   check-compat-practical   実測ハーネス（22 タスク定義・アクセス確認・run_core）の自己テスト（MEAS-4。要 jq・node）
#   check-compat-regression  対象サイト群の動作率回帰チェック（COMPAT-1・COMPAT-4。要 jq）
#   bench-record             competitor_lightpanda ベンチを実行し結果を時系列で記録（要 jq）
#   check-bench-record       record.sh の自己テスト（要 jq）
#   check-ci-changes         CI の docs-only 判定スクリプトの自己テスト
#   check-binary-size        既定 feature のリリースバイナリサイズ検査（RENDER-2。要 jq）
#   check-static-link        musl 配布バイナリ（既定・軽量）の動的リンクなし検査（PLUG-1・CTR-4・TASK-93.3。Linux のみ）
#   measure-mcp-envelope     方式 B 本体と MCP エンベロープの測定（PLUG-5。unix のみ）
#   check-mcp-envelope       上記ハーネスの e2e smoke（unix のみ。0 件実行は NG）
#   measure-plugin-overhead  mcp プラグインのアイドル RSS 増分・cold start の測定（PLUG-3・TASK-95.1・95.2。unix のみ）
#   check-plugin-overhead    上記の e2e smoke（unix のみ。0 件実行は NG）
#   measure-js-binary-size   JS エンジン構成別バイナリサイズ計測（JS-3・PERF-1。要 jq）
#   measure-container-size   既定・軽量コンテナと比較対象（Playwright 公式）のイメージサイズ計測（CTR-1・CTR-5。要 docker・jq）
#   measure-cssom-profile-cost  CSSOM プロファイル読み込みのコスト計測（PLUG-8。要 jq）
#   trace-playwright | check-playwright-trace | check-puppeteer-connect   CDP トレース収集・自己テスト（要 node）
#   test-harness             harness/render-screenshot のユニット・結合テスト（要 python3）
#
# 上書き可能な環境変数（`make <target> VAR=value` も可）: BINARY_SIZE_PACKAGE・BINARY_SIZE_LIMIT_BYTES・
# JS_BINARY_SIZE_STRIP（非空で --strip）・CSSOM_PROFILE_COST_ARGS・MCP_ENVELOPE_ARGS・
# CONTAINER_SIZE_REFERENCE_IMAGE・CONTAINER_SIZE_REFERENCE_DIGEST・CONTAINER_SIZE_DEFAULT_IMAGE・
# CONTAINER_SIZE_LITE_IMAGE・CONTAINER_SIZE_PLATFORM・CONTAINER_SIZE_SKIP_BUILD・CONTAINER_SIZE_NO_PULL（非空で有効）・
# MCP_ENVELOPE_TARGET_DIR・STATIC_LINK_TARGET_DIR・PLAYWRIGHT_VERSION・PLAYWRIGHT_INTEGRITY（既定値は scripts/versions.sh）。
# 各ハーネスの詳細は harness/*/README.md・benches/*/README.md を参照。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

[ "$#" -eq 1 ] || { echo "usage: $(basename "$0") <name>" >&2; exit 2; }
name="$1"

# 空白区切りの追加引数（意図的に単語分割する）
: "${MCP_ENVELOPE_ARGS:=}"
: "${PLUGIN_OVERHEAD_ARGS:=}"
: "${JS_BINARY_SIZE_STRIP:=}"
: "${CONTAINER_SIZE_DEFAULT_IMAGE:=fandhe-browser:default}"
: "${CONTAINER_SIZE_LITE_IMAGE:=fandhe-browser:lite}"
: "${CONTAINER_SIZE_PLATFORM:=}"
: "${CONTAINER_SIZE_SKIP_BUILD:=}"
: "${CONTAINER_SIZE_NO_PULL:=}"
: "${CSSOM_PROFILE_COST_ARGS:=--out benches/cssom_profile_cost/results/cssom-profile-cost.json}"

# 相対 CARGO_TARGET_DIR でも resolve_bin（絶対パス必須）を通すため絶対化する。
mcp_target_dir() {
  local t="${MCP_ENVELOPE_TARGET_DIR:-${CARGO_TARGET_DIR:-${ROOT_DIR}/target}}"
  case "${t}" in
    /*) printf '%s\n' "${t}" ;;
    *) printf '%s\n' "${ROOT_DIR}/${t}" ;;
  esac
}

unix_only() {
  case "$(uname -s)" in
    Linux|Darwin) return 0 ;;
    *) return 1 ;;
  esac
}

case "${name}" in
  check-compat-practical)
    require_cmd jq "required by ${name}"
    require_cmd node "Node.js 22 or later is required by ${name}"
    bash harness/compat-practical/self-test.sh
    bash harness/compat-practical/access_check.sh --validate-only
    bash harness/compat-practical/run_core.sh --validate-only
    bash harness/compat-practical/make_matrix.sh --validate-only
    ;;
  check-compat-regression)
    # self-test（合成データで「閾値未満で fail する」ことをログに残す）を先に実行してから実マトリクスを判定する。
    # 実マトリクスは TASK-71.3（#312）が生成予定でまだ無いため当面 --allow-missing を渡す
    # （#312 で導入され次第このフラグを削除し fail-closed に戻す）。
    require_cmd jq "required by ${name}"
    bash harness/compat-regression/self-test.sh
    bash harness/compat-regression/check-matrix.sh \
      --matrix harness/compat-practical/results/matrix.json \
      --threshold 70 \
      --categories static,spa,form \
      --all-categories \
      --allow-missing
    ;;
  bench-record)
    # LIGHTPANDA_BIN・FANDHE_BROWSER_BIN・CHROMIUM_IDLE_RSS_KB を事前に export しておくと実測値を得る
    # （未設定の対象は skipped になる。benches/competitor_lightpanda/README.md「現状の限界」）。
    require_cmd jq "required by ${name}"
    bash benches/competitor_lightpanda/record.sh \
      --history benches/competitor_lightpanda/results/history.jsonl \
      --source local
    ;;
  check-bench-record)
    require_cmd jq "required by ${name}"
    bash benches/competitor_lightpanda/self-test.sh
    ;;
  check-ci-changes)
    bash scripts/ci-changes.sh --self-test
    require_cmd jq "required by ${name}"
    bash scripts/ci-prune-target.sh --self-test
    ;;
  check-binary-size)
    # `make ci` には含めない（cargo build --release のコストが大きいため）。CI では ci.yml の
    # harness-* ジョブ [binary-size] ステップが 3 OS で担う。
    require_cmd jq "required by ${name}"
    bash harness/binary-size/self-test.sh
    bash harness/binary-size/check-binary-size.sh \
      --package "${BINARY_SIZE_PACKAGE}" \
      --limit "${BINARY_SIZE_LIMIT_BYTES}"
    ;;
  check-static-link)
    # `make ci` には含めない（release ビルド 2 回と musl ツールチェーンを要するため。check-binary-size と同じ扱い）。
    # CI では ci.yml の harness-ubuntu [static-link] ステップが担う。macOS は #364、Windows は #365 が別手段で担う。
    # 出荷物そのものの関門は docker/Dockerfile.release の verify-* ステージ（判定ロジックは同じ scripts/check-static-link.sh）。
    [ "$(uname -s)" = "Linux" ] || skip "${name} supports Linux only (macOS: #364, Windows: #365)"
    case "$(uname -m)" in
      x86_64) triple="x86_64-unknown-linux-musl"; musl_cc="musl-gcc" ;;
      aarch64) triple="aarch64-unknown-linux-musl"; musl_cc="aarch64-linux-musl-gcc" ;;
      *) echo "error: unsupported architecture for ${name}: $(uname -m)" >&2; exit 2 ;;
    esac
    require_cmd cargo "required by ${name}"
    require_cmd rustc "required by ${name}"
    require_cmd file "required by ${name}"
    require_cmd readelf "required by ${name}"
    require_cmd ldd "required by ${name}"
    host_triple="$(rustc -vV | sed -n 's/^host: //p')"
    if [ "${host_triple}" != "${triple}" ]; then
      require_cmd "${musl_cc}" "install musl-tools; ring needs a musl C compiler"
    fi
    if [ ! -d "$(rustc --print target-libdir --target "${triple}")" ]; then
      echo "error: rust target ${triple} is not installed (run: rustup target add ${triple})" >&2
      exit 1
    fi
    bash scripts/check-static-link.sh --self-test
    # 出力先は target/ の外に置ける（CI は cache 対象外にするため RUNNER_TEMP 配下を渡す）。相対なら ROOT_DIR 基準。
    st_dir="${STATIC_LINK_TARGET_DIR:-${CARGO_TARGET_DIR:-${ROOT_DIR}/target}}"
    case "${st_dir}" in
      /*) ;;
      *) st_dir="${ROOT_DIR}/${st_dir}" ;;
    esac
    # V8 prebuilt の SHA-256 ピンは docker/release-build.sh を正本とし、三つ組の行の直後 1 行から厳格に抽出する。
    v8_pin="$(awk -v t="${triple})" '$1 == t {getline; print; exit}' docker/release-build.sh)"
    pin_re='^[[:space:]]*RUSTY_V8_ARCHIVE_SHA256=([0-9a-f]{64}) ;;$'
    if [[ ! "${v8_pin}" =~ ${pin_re} ]]; then
      echo "error: could not extract RUSTY_V8_ARCHIVE_SHA256 for ${triple} from docker/release-build.sh" >&2
      exit 1
    fi
    v8_sha="${BASH_REMATCH[1]}"
    sl_bin="${st_dir}/${triple}/release/fandhe-browser"
    sl_rc=0
    # 2 構成は同じ出力パスへ上書きされるため、各ビルドの直後に検査する。片方が NG でももう片方を実行する。
    for flavor in default lite; do
      echo "static-link: flavor=${flavor} target=${triple}"
      if [ "${flavor}" = "default" ]; then
        RUSTY_V8_ARCHIVE_SHA256="${v8_sha}" cargo build --release --locked -p fandhe-browser-cli \
          --bin fandhe-browser --target "${triple}" --target-dir "${st_dir}" || { sl_rc=1; continue; }
      else
        cargo build --release --locked -p fandhe-browser-cli --bin fandhe-browser \
          --target "${triple}" --target-dir "${st_dir}" --no-default-features --features js-boa || { sl_rc=1; continue; }
      fi
      echo "static-link: flavor=${flavor} size=$(stat -c %s "${sl_bin}")"
      bash scripts/check-static-link.sh "${sl_bin}" || sl_rc=1
    done
    exit "${sl_rc}"
    ;;
  measure-mcp-envelope)
    unix_only || { echo "error: ${name} supports unix only" >&2; exit 1; }
    # cargo bench は他 package の bin をビルドしないため mcp バイナリを先に release ビルドして渡す。
    cargo build --release -p fandhe-browser-mcp
    # shellcheck disable=SC2086 # 引数文字列は意図的に単語分割する
    FANDHE_BROWSER_MCP_BIN="$(mcp_target_dir)/release/fandhe-browser-mcp" \
      cargo bench -p fandhe-browser-ai --bench mcp_envelope -- ${MCP_ENVELOPE_ARGS}
    ;;
  check-mcp-envelope)
    unix_only || skip "${name} supports unix only (Profile is unix-only)"
    cargo build -p fandhe-browser-mcp
    out="$(mktemp)"
    trap 'rm -f "${out}"' EXIT
    FANDHE_BROWSER_MCP_BIN="$(mcp_target_dir)/debug/fandhe-browser-mcp" \
      cargo test -p fandhe-browser-ai --test mcp_envelope_e2e -- --ignored 2>&1 | tee "${out}"
    if ! grep -q "test result: ok. 1 passed" "${out}"; then
      echo "error: mcp_envelope_e2e did not pass exactly 1 test" >&2
      exit 1
    fi
    ;;
  measure-plugin-overhead)
    unix_only || { echo "error: ${name} supports unix only" >&2; exit 1; }
    # cargo bench は他 package の bin をビルドしないため mcp バイナリを先に release ビルドして渡す。
    cargo build --release -p fandhe-browser-mcp
    # shellcheck disable=SC2086 # 引数文字列は意図的に単語分割する
    FANDHE_BROWSER_MCP_BIN="$(mcp_target_dir)/release/fandhe-browser-mcp" \
      cargo bench -p fandhe-browser-ai --bench plugin_overhead -- ${PLUGIN_OVERHEAD_ARGS}
    ;;
  check-plugin-overhead)
    unix_only || skip "${name} supports unix only (Profile is unix-only)"
    cargo build -p fandhe-browser-mcp
    out="$(mktemp)"
    trap 'rm -f "${out}"' EXIT
    FANDHE_BROWSER_MCP_BIN="$(mcp_target_dir)/debug/fandhe-browser-mcp" \
      cargo test -p fandhe-browser-ai --test plugin_overhead_e2e -- --ignored 2>&1 | tee "${out}"
    if ! grep -q "test result: ok. 2 passed" "${out}"; then
      echo "error: plugin_overhead_e2e did not pass exactly 2 tests" >&2
      exit 1
    fi
    ;;
  measure-js-binary-size)
    # 計測のみでゲートではない（閾値判定は #470）。V8 を含むリリースビルドを 3 回行うため
    # `make ci` には含めない。JS_BINARY_SIZE_STRIP が非空なら strip=symbols で計測する。
    require_cmd jq "required by ${name}"
    bash harness/binary-size/measure-js-engine-configs-self-test.sh
    strip_arg=()
    [ -z "${JS_BINARY_SIZE_STRIP}" ] || strip_arg=(--strip)
    bash harness/binary-size/measure-js-engine-configs.sh \
      --package "${BINARY_SIZE_PACKAGE}" \
      ${strip_arg[@]+"${strip_arg[@]}"}
    ;;
  measure-container-size)
    # 計測のみでゲートではない（70% 判定は #488・95% 判定は TASK-68）。docker build（release）と
    # 比較対象の pull（数 GB）を伴うため `make ci` には含めない。値は配列で渡し単語分割しない。
    require_cmd docker "required by ${name}"
    require_cmd jq "required by ${name}"
    bash harness/container-size/self-test.sh
    cs_args=()
    [ -z "${CONTAINER_SIZE_PLATFORM}" ] || cs_args+=(--platform "${CONTAINER_SIZE_PLATFORM}")
    [ -z "${CONTAINER_SIZE_SKIP_BUILD}" ] || cs_args+=(--skip-build)
    [ -z "${CONTAINER_SIZE_NO_PULL}" ] || cs_args+=(--no-pull)
    bash harness/container-size/measure.sh \
      --reference-image "${CONTAINER_SIZE_REFERENCE_IMAGE}" \
      --reference-digest "${CONTAINER_SIZE_REFERENCE_DIGEST}" \
      --default-image "${CONTAINER_SIZE_DEFAULT_IMAGE}" \
      --lite-image "${CONTAINER_SIZE_LITE_IMAGE}" \
      ${cs_args[@]+"${cs_args[@]}"}
    ;;
  measure-cssom-profile-cost)
    # release ビルドを伴う一度きりの記録で RSS がランナー差で揺れるため `make ci` には含めない。
    require_cmd jq "required by ${name}"
    bash benches/cssom_profile_cost/self-test.sh
    # shellcheck disable=SC2086 # 引数文字列は意図的に単語分割する
    bash benches/cssom_profile_cost.sh ${CSSOM_PROFILE_COST_ARGS}
    ;;
  trace-playwright)
    bash harness/playwright-trace/run.sh "${PLAYWRIGHT_VERSION}" "${PLAYWRIGHT_INTEGRITY}" \
      harness/playwright-trace/results/newpage-trace.jsonl --force
    ;;
  check-playwright-trace)
    bash harness/playwright-trace/self-test.sh
    ;;
  check-puppeteer-connect)
    bash harness/puppeteer-connect/self-test.sh
    ;;
  test-harness)
    # harness/render-screenshot は Rust workspace に属さない Python 標準ライブラリのみのスクリプト
    # （TASK-37.1・RENDER-5）。Cargo.toml の有無に関係なく実行できる。
    require_cmd python3 "see https://www.python.org/"
    python3 -B -m unittest discover -s harness/render-screenshot -p 'test_*.py' -v
    ;;
  *)
    echo "error: unknown harness: ${name}" >&2
    exit 2
    ;;
esac
