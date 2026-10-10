#!/usr/bin/env bash
#
# feature 無効（既定）でリリースビルドしたバイナリのサイズが RENDER-2（基準は
# CORE-2 と同じ「Chromium 比 80% 以上削減」）の水準を保っているか検査する
# （TASK-34.2・Issue #466）。呼び出し元は Makefile の check-binary-size ターゲット、
# および `.github/workflows/ci.yml` の `harness-*` ジョブの `[binary-size]` ステップ（TASK-34.3・#467）。
# 上限値の根拠・出力形式の契約・終了コードは同じディレクトリの README.md を参照。
#
# 依存は bash + cargo + jq + rustc + coreutils のみ（新規 Cargo 依存を避けるため。
# .claude/rules/dependency-policy.md）。3 OS（Linux/macOS/Windows）の git-bash 上で
# 同一に動くことを前提にする。
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  check-binary-size.sh --file <path> --limit <bytes> [--host <triple>] [--package <name>] [--bin <name>]
  check-binary-size.sh --package <name> --limit <bytes> [--host <triple>]

  --file <path>       Judge a single existing file (self-test mode; skips build).
  --package <name>    Cargo package to build in release mode and judge its bin target(s).
  --limit <bytes>     Maximum allowed size in bytes, 1-15 digits (required).
  --host <triple>     Target triple label for the output line (default: `rustc -vV` host).
  --bin <name>        Bin label for --file mode (default: basename of --file).
EOF
}

FILE=""
PACKAGE=""
LIMIT=""
HOST=""
BIN=""

while [ $# -gt 0 ]; do
  case "$1" in
    --file)
      [ $# -ge 2 ] || { echo "error: --file requires a value" >&2; usage; exit 2; }
      FILE="$2"
      shift 2
      ;;
    --package)
      [ $# -ge 2 ] || { echo "error: --package requires a value" >&2; usage; exit 2; }
      PACKAGE="$2"
      shift 2
      ;;
    --limit)
      [ $# -ge 2 ] || { echo "error: --limit requires a value" >&2; usage; exit 2; }
      LIMIT="$2"
      shift 2
      ;;
    --host)
      [ $# -ge 2 ] || { echo "error: --host requires a value" >&2; usage; exit 2; }
      HOST="$2"
      shift 2
      ;;
    --bin)
      [ $# -ge 2 ] || { echo "error: --bin requires a value" >&2; usage; exit 2; }
      BIN="$2"
      shift 2
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [ -z "$FILE" ] && [ -z "$PACKAGE" ]; then
  echo "error: either --file or --package is required" >&2
  usage
  exit 2
fi
if [ -n "$FILE" ] && [ -n "$PACKAGE" ]; then
  echo "error: --file and --package are mutually exclusive" >&2
  usage
  exit 2
fi
# --bin は --file モード専用の契約（README.md の Usage 参照。package モードは
# 対象 package の bin target をすべて判定する仕様のため、--bin を渡されても
# 黙って無視すると「指定した 1 個だけを計測した」と CLI 利用者に誤認させる。
# codex レビュー指摘, PR #563）。
if [ -n "$PACKAGE" ] && [ -n "$BIN" ]; then
  echo "error: --bin is only valid with --file (--package judges every bin target of the package)" >&2
  usage
  exit 2
fi

# 上限値は 1-15 桁の 10 進数のみ許可し、0 は拒否する（bash の算術比較が
# オーバーフローしないようにするための桁数制限。README.md 参照）。
if ! [[ "$LIMIT" =~ ^[0-9]{1,15}$ ]] || [ "$LIMIT" -eq 0 ]; then
  echo "error: --limit must be a positive integer with at most 15 digits (got: $LIMIT)" >&2
  exit 2
fi

# host / package / bin ラベルは binary-size: 行としてそのまま CI ログへ出力される
# ため、改行・制御文字・`::` を含む値を許すと GitHub Actions のワークフロー
# コマンドとして誤解釈されるおそれがある（harness/compat-regression の cat 検証・
# codex review 指摘, PR #452 と同じ考え方）。安全な文字種に限定する。
LABEL_RE='^[A-Za-z0-9._-]{1,64}$'
if [ -n "$HOST" ] && ! [[ "$HOST" =~ $LABEL_RE ]]; then
  echo "error: --host must match ${LABEL_RE} (got: $HOST)" >&2
  exit 2
fi
if [ -n "$PACKAGE" ] && ! [[ "$PACKAGE" =~ $LABEL_RE ]]; then
  echo "error: --package must match ${LABEL_RE} (got: $PACKAGE)" >&2
  exit 2
fi
if [ -n "$BIN" ] && ! [[ "$BIN" =~ $LABEL_RE ]]; then
  echo "error: --bin must match ${LABEL_RE} (got: $BIN)" >&2
  exit 2
fi

if [ -z "$HOST" ]; then
  if ! command -v rustc >/dev/null 2>&1; then
    echo "error: rustc is required to determine --host but was not found on PATH" >&2
    exit 2
  fi
  # `rustc -vV` の `host: <triple>` 行から取得する。Windows ネイティブ実行時に
  # 末尾へ \r が残りうるため tr で除去する（compat-regression と同じ対策）。
  HOST=$(rustc -vV | sed -n 's/^host: //p' | tr -d '\r')
  if [ -z "$HOST" ] || ! [[ "$HOST" =~ $LABEL_RE ]]; then
    echo "error: failed to determine host triple from 'rustc -vV' (got: '$HOST')" >&2
    exit 2
  fi
fi

FAIL=0

# $1=file $2=package(label) $3=bin(label)
judge() {
  local file="$1" package="$2" bin="$3" bytes result
  if [ ! -e "$file" ]; then
    echo "error: file not found: $file" >&2
    exit 2
  fi
  if [ ! -f "$file" ]; then
    echo "error: not a regular file: $file" >&2
    exit 2
  fi
  bytes=$(wc -c <"$file" | tr -d '[:space:]')
  if [ "$bytes" -le "$LIMIT" ]; then
    result="pass"
  else
    result="fail"
    FAIL=1
  fi
  echo "binary-size: host=${HOST} package=${package} bin=${bin} bytes=${bytes} limit=${LIMIT} result=${result}"
}

if [ -n "$FILE" ]; then
  BIN="${BIN:-$(basename "$FILE")}"
  if ! [[ "$BIN" =~ $LABEL_RE ]]; then
    echo "error: basename of --file must match ${LABEL_RE} for the bin label (got: $BIN); pass --bin explicitly" >&2
    exit 2
  fi
  judge "$FILE" "file" "$BIN"
  if [ "$FAIL" -ne 0 ]; then
    exit 1
  fi
  exit 0
fi

# --package モード: metadata 確認 → リリースビルド → 実行ファイル抽出 → 判定。
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required but was not found on PATH" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required but was not found on PATH" >&2
  exit 2
fi

# cargo metadata の JSON は常に stdout のみへ出る（警告・診断は stderr）。
# 2>&1 で混ぜると warning 行が JSON へ混入して jq のパースが壊れるため、
# stdout だけを METADATA に取り込み、stderr はそのまま呼び出し元の端末 /
# CI ログへ流す（cargo build 側の同種の修正と同じ方針。レビュー指摘対応）。
if ! METADATA=$(cargo metadata --no-deps --format-version 1); then
  echo "error: cargo metadata failed (see stderr above)" >&2
  exit 2
fi

# $pkg は jq の --arg で渡し、フィルタ文字列へ連結しない（jq インジェクション
# 対策。security.md「インジェクション」観点。compat-regression と同じ方針）。
#
# `.packages[]` を名前だけで照合すると、workspace 外の依存に同名 package が
# 存在する場合に指定した workspace package と異なる ID を拾いうる
# （`--no-deps` により通常は workspace member のみが `.packages[]` に載るが、
# 将来 `--no-deps` を外す変更が入っても取り違えないよう、`.workspace_members`
# に id が含まれる package だけへ明示的に限定する。codex レビュー指摘, PR
# #563）。
PKG_FOUND=$(printf '%s' "$METADATA" | jq -r --arg pkg "$PACKAGE" '
    .workspace_members as $wm
    | [.packages[] | select(.name == $pkg) | select(.id as $id | $wm | index($id) != null)]
    | length
  ' | tr -d '\r')
if [ "$PKG_FOUND" -eq 0 ]; then
  # package 不在は既定値かどうかに関係なく入力・使用エラー（exit 2）。cli の
  # 削除・リネームや package 名の誤記が「未導入のためスキップ」として黙って
  # 通過する fail-open を防ぐ（fail-closed・REPAIR-5・RENDER-2・#633）。
  echo "error: package $PACKAGE not found in workspace (check --package / BINARY_SIZE_PACKAGE for a typo)" >&2
  exit 2
fi
# cargo は同一 workspace 内で同名 package を許さないため通常起こり得ないが、
# 上記の workspace_members 限定フィルタが 2 件以上一致した場合は前提が崩れて
# いる（cargo の挙動変化等）ため、誤った ID を選ばず fail-closed にする。
if [ "$PKG_FOUND" -gt 1 ]; then
  echo "error: multiple workspace members named $PACKAGE found in cargo metadata (ambiguous; cargo should reject duplicate package names within one workspace)" >&2
  exit 2
fi

# cargo build の compiler-artifact は依存 crate のビルドも含めて全て流れてくる
# ため、`.executable != null` だけで絞ると依存の build script（build-script-build
# 等。これも "kind": ["custom-build"] の実行ファイルとして .executable を持つ）
# まで拾ってしまい、それらのサイズで誤って不合格・出力の package/bin ラベルが
# 実体と一致しなくなる（codex / cursor レビュー指摘, PR #563）。cargo metadata
# の package id（上記と同じ workspace member 限定フィルタで解決）で対象
# package に限定し、target.kind に "bin" を含むものだけを対象とする。
PKG_ID=$(printf '%s' "$METADATA" | jq -r --arg pkg "$PACKAGE" '
    .workspace_members as $wm
    | [.packages[] | select(.name == $pkg) | select(.id as $id | $wm | index($id) != null) | .id]
    | first
  ' | tr -d '\r')
if [ -z "$PKG_ID" ] || [ "$PKG_ID" = "null" ]; then
  echo "error: failed to resolve package id for $PACKAGE from cargo metadata" >&2
  exit 2
fi

BIN_COUNT=$(printf '%s' "$METADATA" | jq -r --arg pkgid "$PKG_ID" '
    [.packages[] | select(.id == $pkgid) | .targets[] | select(.kind | index("bin"))] | length
  ' | tr -d '\r')
if [ "$BIN_COUNT" -eq 0 ]; then
  echo "error: package $PACKAGE has no bin target" >&2
  exit 2
fi

# `--message-format=json-render-diagnostics` は rustc の診断表示形式を変えるだけで、
# cargo 自身のステータス行（`   Compiling foo v0.1.0`・`    Finished release ...`）は
# 依然プレーンテキストのまま stderr へ出る。2>&1 で BUILD_JSON に混ぜると、cold
# build（Compiling 行が先に来る）で jq のパースが 1 行目から失敗して停止し、
# `< <(...)` のプロセス置換がその非ゼロ終了を握り潰すため「実行ファイル 0 件」に
# 化けて原因不明の誤検知になる（warm cache では Compiling 行が出ないか artifact 行
# より前に来ないことがあるため偶然通ってしまう）。JSON は常に stdout のみへ出る
# ため、stdout だけを BUILD_JSON に取り込み、stderr はそのまま呼び出し元の端末 /
# CI ログへ流す（レビュー指摘対応）。
if ! BUILD_JSON=$(cargo build --release -p "$PACKAGE" --message-format=json-render-diagnostics); then
  echo "error: cargo build --release -p $PACKAGE failed (see stderr above)" >&2
  exit 2
fi

# jq は不正行を無視せず全体を落とすため、compiler-artifact 以外の行（診断メッセージ
# 等）が混じっても `select` で無視できるよう `-c` ではなく通常の行区切り出力を使う。
# .package_id で対象 package に限定し、.target.kind に "bin" を含むものだけを
# 実行ファイル候補にする（依存の build script 混入対策。上記 PKG_ID 参照）。
#
# jq をパイプの右辺にしたままプロセス置換 `< <(...)` へ渡すと、途中で jq が
# 失敗しても非ゼロ終了がプロセス置換に握り潰され、途中まで解析できた分だけで
# 後続の件数確認（0 件チェック）を通過し「一部だけ判定して合格」と誤認しうる
# （codex / cursor レビュー指摘, PR #563）。jq の出力をコマンド置換 `$(...)` で
# 直接受け取り、`if ! ...` でその終了コードを明示的に確認してから使う。
if ! EXTRACTED=$(
  printf '%s\n' "$BUILD_JSON" \
    | jq -r --arg pkgid "$PKG_ID" '
        select(.reason? == "compiler-artifact"
          and (.executable? != null)
          and (.package_id? == $pkgid)
          and ((.target.kind? // []) | index("bin")))
        | .executable
      '
); then
  echo "error: failed to parse 'cargo build' JSON output for package $PACKAGE (jq failed)" >&2
  exit 2
fi
EXTRACTED=$(printf '%s' "$EXTRACTED" | tr -d '\r')

# `mapfile`/`readarray` は bash 4 以降の組み込みで、macOS（GitHub Actions
# macos-latest 含む）が同梱する /bin/bash 3.2 には存在せず command not found
# になる（cursor レビュー指摘, PR #563。compat-regression の while-read 方式に
# 合わせる）。3 OS で同一に動く `while IFS= read -r` ループへ置き換える。
EXECUTABLES=()
while IFS= read -r exe_line; do
  [ -n "$exe_line" ] && EXECUTABLES+=("$exe_line")
done <<EXECUTABLES_EOF
$EXTRACTED
EXECUTABLES_EOF
if [ "${#EXECUTABLES[@]}" -eq 0 ]; then
  echo "error: no executable found in 'cargo build' output for package $PACKAGE" >&2
  exit 2
fi

for exe in "${EXECUTABLES[@]}"; do
  # Windows（Git Bash）の cargo は Windows 形式パスを返すことがあるため、
  # cygpath があれば POSIX パスへ変換する（README.md「Windows 対策」参照）。
  if command -v cygpath >/dev/null 2>&1; then
    exe=$(cygpath -u "$exe")
  fi
  bin_label=$(basename "$exe")
  bin_label="${bin_label%.exe}"
  if ! [[ "$bin_label" =~ $LABEL_RE ]]; then
    echo "error: bin basename must match ${LABEL_RE} for the bin label (got: $bin_label)" >&2
    exit 2
  fi
  judge "$exe" "$PACKAGE" "$bin_label"
done

if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
exit 0
