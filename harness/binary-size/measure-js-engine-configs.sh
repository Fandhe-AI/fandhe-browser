#!/usr/bin/env bash
#
# JS エンジン構成別（既定 V8 / 軽量 boa / エンジンなし）のリリースバイナリサイズを
# 計測して `js-binary-size:` 行で出力する（TASK-31.1・Issue #469・JS-1・JS-3・PERF-1・MS-3）。
# 呼び出し元は Makefile の measure-js-binary-size ターゲット。出力は後続の #470
# （TASK-31.2・測定レポート）が読み、CORE-2 / PERF-1 の達成可否を判断する。
# 本スクリプトは計測のみでゲートではない（閾値判定は #470、既定ビルドの上限ゲートは
# check-binary-size.sh）。出力形式の契約・終了コードは README.md を参照。
#
# 依存は bash + cargo + rustc + jq + awk + coreutils のみ（dependency-policy）。
# macOS の bash 3.2 でも動くよう連想配列・mapfile は使わない。
set -euo pipefail

# 出力値の安全文字種（改行・空白・`::` による GitHub Actions ワークフロー
# コマンド誤解釈の防止。check-binary-size.sh と同じ考え方）。
LABEL_RE='^[A-Za-z0-9._-]{1,64}$'

usage() {
  cat >&2 <<'USAGE'
Usage:
  measure-js-engine-configs.sh [--package <name>] [--strip]

  --package <name>  Cargo package to build (default: fandhe-browser-cli).
  --strip           Build with CARGO_PROFILE_RELEASE_STRIP=symbols (Cargo.toml is not edited).
USAGE
}

# 構成表（JS-1）。引数は外部入力から組み立てず固定値にする。
# 両エンジン同梱などの行を足す場合はここと下の case へ 1 行ずつ追加する。
CONFIGS="default boa none"

# $1=config → 空白区切りの cargo 追加引数
config_cargo_args() {
  case "$1" in
    default) echo "" ;;
    boa) echo "--no-default-features --features js-boa" ;;
    none) echo "--no-default-features" ;;
    *) return 1 ;;
  esac
}

# $1=config → features ラベル
config_label() {
  case "$1" in
    default) echo "default" ;;
    boa) echo "no-default-js-boa" ;;
    none) echo "no-default" ;;
    *) return 1 ;;
  esac
}

# $1=config → 期待するエンジン（v8 / boa / none）
config_expected_engines() {
  case "$1" in
    default) echo "v8" ;;
    boa) echo "boa" ;;
    none) echo "none" ;;
    *) return 1 ;;
  esac
}

# cargo build の JSON（stdin）から、ビルド対象に含まれた JS エンジン crate を
# 判定して v8 / boa / v8+boa / none を返す（陽性対照。feature 連鎖の断絶検出）。
# crate 名は完全一致（`myv8` 等の誤検出を避ける）。
observed_engines() {
  local out
  if ! out=$(jq -r '
      select(.reason? == "compiler-artifact"
        and ((.target.kind? // []) | index("lib"))
        and ((.target.name? == "v8") or (.target.name? == "boa_engine")))
      | .target.name
    '); then
    return 2
  fi
  local v8=0 boa=0 line
  while IFS= read -r line; do
    [ "$line" = "v8" ] && v8=1
    [ "$line" = "boa_engine" ] && boa=1
  done <<OUT_EOF
$out
OUT_EOF
  if [ "$v8" -eq 1 ] && [ "$boa" -eq 1 ]; then echo "v8+boa"
  elif [ "$v8" -eq 1 ]; then echo "v8"
  elif [ "$boa" -eq 1 ]; then echo "boa"
  else echo "none"; fi
}

# $1=bytes → 10 進 MB（10^6）を小数第 2 位まで。小数点をカンマにするロケールで mb=42,92 となり出力契約が壊れないよう C ロケールに固定する
bytes_to_mb() {
  LC_ALL=C awk -v b="$1" 'BEGIN { printf "%.2f", b / 1000000 }'
}

# $1=name $2=value。LABEL_RE に合わない値は出力せず exit 2。
require_label() {
  if ! [[ "$2" =~ $LABEL_RE ]]; then
    echo "error: $1 must match ${LABEL_RE} (got: $2)" >&2
    exit 2
  fi
}

# $1=config $2=cargo 追加引数 $3=strip(0/1) $4=package → cargo build の JSON を stdout へ
build_config() {
  local config="$1" args="$2" strip="$3" package="$4"
  local locked=()
  [ "${GITHUB_ACTIONS:-}" = "true" ] && locked=(--locked)
  # 引数は固定値の単語分割（クォートしない）で渡す。
  # shellcheck disable=SC2086
  if [ "$strip" -eq 1 ]; then
    CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release ${locked[@]+"${locked[@]}"} -p "$package" $args --message-format=json-render-diagnostics
  else
    cargo build --release ${locked[@]+"${locked[@]}"} -p "$package" $args --message-format=json-render-diagnostics
  fi
}

main() {
  local PACKAGE="fandhe-browser-cli" STRIP=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --package)
        [ $# -ge 2 ] || { echo "error: --package requires a value" >&2; usage; exit 2; }
        PACKAGE="$2"; shift 2 ;;
      --strip) STRIP=1; shift ;;
      *) echo "error: unknown argument: $1" >&2; usage; exit 2 ;;
    esac
  done
  require_label "--package" "$PACKAGE"

  local tool
  for tool in cargo rustc jq awk; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required but was not found on PATH" >&2; exit 2; }
  done

  # 環境情報（Windows の \r を除去して検証）
  local RUSTC_V CARGO_V HOST RUSTC_REL RUSTC_COMMIT CARGO_REL OS_NAME
  RUSTC_V=$(rustc -vV | tr -d '\r') || { echo "error: rustc -vV failed" >&2; exit 2; }
  CARGO_V=$(cargo -vV | tr -d '\r') || { echo "error: cargo -vV failed" >&2; exit 2; }
  HOST=$(printf '%s\n' "$RUSTC_V" | sed -n 's/^host: //p')
  RUSTC_REL=$(printf '%s\n' "$RUSTC_V" | sed -n 's/^release: //p')
  RUSTC_COMMIT=$(printf '%s\n' "$RUSTC_V" | sed -n 's/^commit-hash: //p')
  CARGO_REL=$(printf '%s\n' "$CARGO_V" | sed -n 's/^release: //p')
  OS_NAME=$(uname -s | tr -d '\r')
  require_label host "$HOST"
  require_label rustc "$RUSTC_REL"
  require_label rustc_commit "$RUSTC_COMMIT"
  require_label cargo "$CARGO_REL"
  require_label os "$OS_NAME"

  local STRIP_LABEL="none"
  [ "$STRIP" -eq 1 ] && STRIP_LABEL="symbols"

  local METADATA
  if ! METADATA=$(cargo metadata --no-deps --format-version 1); then
    echo "error: cargo metadata failed (see stderr above)" >&2
    exit 2
  fi
  local PKG_FOUND PKG_ID BIN_COUNT
  PKG_FOUND=$(printf '%s' "$METADATA" | jq -r --arg pkg "$PACKAGE" '
      .workspace_members as $wm
      | [.packages[] | select(.name == $pkg) | select(.id as $id | $wm | index($id) != null)]
      | length' | tr -d '\r')
  if [ "$PKG_FOUND" -ne 1 ]; then
    echo "error: package $PACKAGE must match exactly one workspace member (found $PKG_FOUND)" >&2
    exit 2
  fi
  PKG_ID=$(printf '%s' "$METADATA" | jq -r --arg pkg "$PACKAGE" '
      .workspace_members as $wm
      | [.packages[] | select(.name == $pkg) | select(.id as $id | $wm | index($id) != null) | .id]
      | first' | tr -d '\r')
  BIN_COUNT=$(printf '%s' "$METADATA" | jq -r --arg pkgid "$PKG_ID" '
      [.packages[] | select(.id == $pkgid) | .targets[] | select(.kind | index("bin"))] | length' | tr -d '\r')
  if [ "$BIN_COUNT" -eq 0 ]; then
    echo "error: package $PACKAGE has no bin target" >&2
    exit 2
  fi

  local MISMATCH=0 config args label expected build_json observed extracted exe bytes bin_label
  for config in $CONFIGS; do
    args=$(config_cargo_args "$config")
    label=$(config_label "$config")
    expected=$(config_expected_engines "$config")
    echo "measuring: config=${config} (cargo build --release -p ${PACKAGE} ${args})" >&2

    # 3 構成とも同じ target/release/<bin> を上書きするため、ビルド直後に計測する。
    if ! build_json=$(build_config "$config" "$args" "$STRIP" "$PACKAGE"); then
      echo "error: cargo build failed for config=${config} (see stderr above)" >&2
      exit 2
    fi

    if ! observed=$(printf '%s\n' "$build_json" | observed_engines); then
      echo "error: failed to parse cargo build JSON for config=${config}" >&2
      exit 2
    fi
    if [ "$observed" != "$expected" ]; then
      echo "error: engine mismatch for config=${config}: expected=${expected} observed=${observed} (feature chain cli -> core -> js may be broken)" >&2
      MISMATCH=1
    fi

    if ! extracted=$(printf '%s\n' "$build_json" | jq -r --arg pkgid "$PKG_ID" '
        select(.reason? == "compiler-artifact"
          and (.executable? != null)
          and (.package_id? == $pkgid)
          and ((.target.kind? // []) | index("bin")))
        | .executable'); then
      echo "error: failed to parse cargo build JSON for executables (config=${config})" >&2
      exit 2
    fi
    extracted=$(printf '%s' "$extracted" | tr -d '\r')
    if [ -z "$extracted" ]; then
      echo "error: no executable found for package $PACKAGE (config=${config})" >&2
      exit 2
    fi
    while IFS= read -r exe; do
      [ -n "$exe" ] || continue
      if command -v cygpath >/dev/null 2>&1; then
        exe=$(cygpath -u "$exe")
      fi
      if [ ! -f "$exe" ]; then
        echo "error: executable not found: $exe" >&2
        exit 2
      fi
      bin_label=$(basename "$exe")
      bin_label="${bin_label%.exe}"
      require_label bin "$bin_label"
      bytes=$(wc -c <"$exe" | tr -d '[:space:]')
      echo "js-binary-size: config=${config} features=${label} engines=${observed} os=${OS_NAME} host=${HOST} target=${HOST} rustc=${RUSTC_REL} rustc_commit=${RUSTC_COMMIT} cargo=${CARGO_REL} profile=release strip=${STRIP_LABEL} package=${PACKAGE} bin=${bin_label} bytes=${bytes} mb=$(bytes_to_mb "$bytes")"
    done <<EXE_EOF
$extracted
EXE_EOF
  done

  if [ "$MISMATCH" -ne 0 ]; then
    exit 1
  fi
}

# self-test から関数を source できるよう、直接実行時のみ main を呼ぶ。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
