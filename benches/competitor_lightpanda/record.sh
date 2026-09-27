#!/usr/bin/env bash
#
# competitor_lightpanda ベンチ（PERF-1・PERF-3・PERF-6・AISNAP-1。TASK-84.1・
# Issue #211）の結果を時系列で JSONL へ追記する記録スクリプト（TASK-84.2・
# Issue #212）。ベンチ本体は結果 JSON を stdout に出すだけで記録先を持たない
# ため（../competitor_lightpanda.rs 参照）、本スクリプトが実行（または
# --input で既存の JSON を読み込み）→ 検証 → 実行環境のメタデータで包んで
# JSONL の 1 行として追記する、までを担う。
#
# harness/compat-regression/check-matrix.sh・self-test.sh と同じ方針（bash +
# jq のみ。新規 Cargo 依存・新規サードパーティ action は追加しない。
# dependency-policy.md）で、3 OS（Linux/macOS/Windows の git-bash）で同一に
# 動くことを前提にする。
#
# 呼び出し元: Makefile の bench-record / check-bench-record ターゲット、
# .github/workflows/bench-competitor.yml（定期実行）。
#
# 現状の限界（REPAIR-3「実装済みを装わない」）: fandhe-browser-cli
# （TASK-41.5）が未実装で CI に対象バイナリが無いため、CI 実行では
# lightpanda・fandhe-browser の両方が skipped になり、実際の数値は記録
# されない。数値を記録できるのは対象バイナリを用意したローカル・計測マシンで
# 実行したときに限る（benches/competitor_lightpanda/README.md 参照）。
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: record.sh --history <path.jsonl> [--input <file>] [--bench-exit-code <n>] [--source local|ci]

  --history <path>        Path to the JSONL history file to append to (required, must end in .jsonl).
  --input <file>          Read bench result JSON from this file instead of running `cargo bench`.
                           When given, --bench-exit-code should also be given (default: 0).
  --bench-exit-code <n>   The exit code the bench run produced (only meaningful with --input).
  --source local|ci       Where this run happened (default: local). Recorded as "source" in the JSONL line.
EOF
}

HISTORY=""
INPUT=""
BENCH_EXIT_CODE=""
SOURCE="local"

while [ $# -gt 0 ]; do
  case "$1" in
    --history)
      [ $# -ge 2 ] || { echo "error: --history requires a value" >&2; usage; exit 2; }
      HISTORY="$2"
      shift 2
      ;;
    --input)
      [ $# -ge 2 ] || { echo "error: --input requires a value" >&2; usage; exit 2; }
      INPUT="$2"
      shift 2
      ;;
    --bench-exit-code)
      [ $# -ge 2 ] || { echo "error: --bench-exit-code requires a value" >&2; usage; exit 2; }
      BENCH_EXIT_CODE="$2"
      shift 2
      ;;
    --source)
      [ $# -ge 2 ] || { echo "error: --source requires a value" >&2; usage; exit 2; }
      SOURCE="$2"
      shift 2
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [ -z "$HISTORY" ]; then
  echo "error: --history is required" >&2
  usage
  exit 2
fi
if [[ "$HISTORY" != *.jsonl ]]; then
  echo "error: --history must end in .jsonl (got: $HISTORY)" >&2
  exit 2
fi
if [ -L "$HISTORY" ]; then
  # シンボリックリンク経由で記録先の外へ書き込む経路を作らない
  # （security.md「プロファイル境界」の精神を記録先パスにも適用）。
  echo "error: --history must not be a symlink: $HISTORY" >&2
  exit 2
fi
HISTORY_DIR="$(dirname -- "$HISTORY")"
if [ ! -d "$HISTORY_DIR" ]; then
  echo "error: parent directory of --history does not exist: $HISTORY_DIR" >&2
  exit 2
fi
if [ -n "$SOURCE" ] && [ "$SOURCE" != "local" ] && [ "$SOURCE" != "ci" ]; then
  echo "error: --source must be 'local' or 'ci' (got: $SOURCE)" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required but was not found on PATH" >&2
  exit 2
fi

# 自前で作った一時ファイルだけを trap で削除する（--input で渡された
# ファイルは呼び出し元の所有物であり、本スクリプトが削除してはならない。
# 一時ファイル配置規則: cwd や --history のディレクトリではなく OS の
# 一時ディレクトリを使う）。
OWN_TMP_OUTPUT=""
cleanup() {
  if [ -n "$OWN_TMP_OUTPUT" ] && [ -f "$OWN_TMP_OUTPUT" ]; then
    rm -f "$OWN_TMP_OUTPUT"
  fi
}
trap cleanup EXIT

if [ -n "$INPUT" ]; then
  if [ ! -f "$INPUT" ]; then
    echo "error: --input file not found: $INPUT" >&2
    exit 2
  fi
  TMP_OUTPUT="$INPUT"
  if [ -z "$BENCH_EXIT_CODE" ]; then
    BENCH_EXIT_CODE=0
  fi
else
  TMP_OUTPUT=$(mktemp "${TMPDIR:-/tmp}/competitor-lightpanda-bench.XXXXXX")
  OWN_TMP_OUTPUT="$TMP_OUTPUT"
  set +e
  cargo bench -q -p fandhe-browser-core --bench competitor_lightpanda >"$TMP_OUTPUT"
  BENCH_EXIT_CODE=$?
  set -e
fi

# 先頭ゼロ（`007` 等）は bash の 8 進数リテラルと誤読されるおそれがあるため
# 拒否し、終了コードの有効範囲（POSIX の 0-255）に収める
# （harness/compat-regression/check-matrix.sh の --threshold 検証と同じ方針）。
if ! [[ "$BENCH_EXIT_CODE" =~ ^(0|[1-9][0-9]{0,2})$ ]] || [ "$BENCH_EXIT_CODE" -gt 255 ]; then
  echo "error: --bench-exit-code must be an integer between 0 and 255 with no leading zero (got: $BENCH_EXIT_CODE)" >&2
  exit 2
fi

# 入力サイズの上限（無制限な巨大ファイルによる jq への過大入力・DoS を防ぐ。
# coding-rust.md「長さ・件数を上限検証してからアロケーションに使う」）。
SIZE=$(wc -c <"$TMP_OUTPUT" | tr -d '[:space:]')
if [ "$SIZE" -gt 1048576 ]; then
  echo "error: bench output exceeds the 1 MiB size limit ($SIZE bytes); not appending to history" >&2
  exit 2
fi

# ベンチが失敗（設定エラー等）すると JSON を一切出さないことがある
# （../competitor_lightpanda.rs モジュールドキュメントの終了コード契約
# 参照）。壊れた・空の出力を履歴へ追記して汚さないよう、"トップレベルが
# JSON オブジェクトであること" を明示的に検証する。
if ! jq -e 'type == "object"' "$TMP_OUTPUT" >/dev/null 2>&1; then
  echo "error: bench output is not a single JSON object; not appending to history" >&2
  echo "  (this can happen when the bench's own configuration is invalid; see the bench's stderr output)" >&2
  exit 2
fi

# gitCommit: CI では GITHUB_SHA、それ以外は git rev-parse HEAD を使う。
# 取得できない・40 桁 hex に一致しない場合は "unknown" にする（fail-closed
# にはせず、記録自体は続行する。git 管理外の一時 checkout 等でも動かすため）。
GIT_COMMIT="${GITHUB_SHA:-}"
if [ -z "$GIT_COMMIT" ]; then
  # git は Windows ネイティブ実行時に CRLF を出力しうる（jq と同じ既知の挙動。
  # harness/compat-regression/check-matrix.sh 冒頭コメント・PR #452 参照）。
  # 末尾の \r を除去してから 40 桁 hex 判定する（除去し忘れると Windows でだけ
  # 正当なコミットハッシュが常に "unknown" 扱いになる）。
  GIT_COMMIT="$(git rev-parse HEAD 2>/dev/null | tr -d '\r' || true)"
fi
if ! [[ "$GIT_COMMIT" =~ ^[0-9a-f]{40}$ ]]; then
  GIT_COMMIT="unknown"
fi

RECORDED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# OS/arch は `uname` で取得し、既知の値へ正規化する。既知でない値はそのまま
# 小文字化して残す（未知環境でも記録自体は続行する。fail-closed にしない）。
UNAME_S="$(uname -s)"
case "$UNAME_S" in
  Linux*) OS="linux" ;;
  Darwin*) OS="darwin" ;;
  MINGW* | MSYS* | CYGWIN*) OS="windows" ;;
  *) OS="$(printf '%s' "$UNAME_S" | tr '[:upper:]' '[:lower:]')" ;;
esac
UNAME_M="$(uname -m)"
case "$UNAME_M" in
  x86_64 | amd64) ARCH="x86_64" ;;
  arm64 | aarch64) ARCH="aarch64" ;;
  *) ARCH="$(printf '%s' "$UNAME_M" | tr '[:upper:]' '[:lower:]')" ;;
esac

RUN_ID_ARGS=(--argjson runId null)
if [ -n "${GITHUB_RUN_ID:-}" ]; then
  RUN_ID_ARGS=(--arg runId "$GITHUB_RUN_ID")
fi

# jq --arg/--argjson で全ての値を渡し、シェルでの文字列連結を一切しない
# （jq インジェクション対策。harness/compat-regression/check-matrix.sh と
# 同じ方針。security.md「インジェクション」）。jq は Windows ネイティブ
# 実行時に CRLF を出力しうる（check-matrix.sh 冒頭コメント・PR #452 と同じ
# 既知の挙動）ため、`tr -d '\r'` を挟んで末尾の \r を除去してから履歴へ
# 追記する（除去し忘れると history.jsonl の各行に \r が混入し、
# coding-rust.md「内部データファイルの改行は LF 固定」に反する）。
# `set -o pipefail`（冒頭）済みのため、jq の非ゼロ終了は tr を挟んでも
# パイプライン全体の失敗として検出できる。
if ! LINE=$(jq -c -n \
  --argjson schemaVersion 1 \
  --arg recordedAt "$RECORDED_AT" \
  --arg gitCommit "$GIT_COMMIT" \
  --arg os "$OS" \
  --arg arch "$ARCH" \
  --arg source "$SOURCE" \
  --argjson benchExitCode "$BENCH_EXIT_CODE" \
  --slurpfile result "$TMP_OUTPUT" \
  "${RUN_ID_ARGS[@]}" \
  '{
    schemaVersion: $schemaVersion,
    recordedAt: $recordedAt,
    gitCommit: $gitCommit,
    os: $os,
    arch: $arch,
    source: $source,
    runId: $runId,
    benchExitCode: $benchExitCode,
    result: $result[0]
  }' 2>&1 | tr -d '\r'); then
  echo "error: failed to build the JSONL line: $LINE" >&2
  exit 2
fi

printf '%s\n' "$LINE" >>"$HISTORY"
echo "recorded to $HISTORY"

exit "$BENCH_EXIT_CODE"
