#!/usr/bin/env bash
#
# compat-practical ハーネス共通関数（TASK-71.1・MEAS-4）。
# access_check.sh と、後続の run_core.sh（TASK-71.2・#311）から `source` して使う。
# 本ファイルは関数定義のみで、source した時点では何も実行しない。

# resolve_bin [<path>]
#   実行対象バイナリ（cli crate の `fandhe-browser`）の絶対パスを解決・検証する。
#   優先順位: 引数 <path> > 環境変数 FANDHE_BROWSER_BIN > <repo>/target/release/fandhe-browser
#   成功時: 解決した絶対パスを stdout へ 1 行出して return 0。
#   失敗時（不在・ディレクトリ・実行不可）: 英語のエラーを stderr へ出して return 2。
#   バイナリは起動しない。引数なしで起動すると CDP サーバーが立つうえ、CLI
#   サブコマンドは TASK-47（CLI-1）で追加予定のため（REPAIR-3: 実行できると装わない）。
resolve_bin() {
  local lib_dir repo_root cand dir
  lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  repo_root="$(cd "$lib_dir/../.." && pwd)"
  cand="${1:-${FANDHE_BROWSER_BIN:-$repo_root/target/release/fandhe-browser}}"
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      # Windows ではビルド成果物に .exe が付く。拡張子なしの指定は .exe 側も見る
      if [ ! -e "$cand" ] && [ -e "$cand.exe" ]; then
        cand="$cand.exe"
      fi
      ;;
  esac
  if [ ! -e "$cand" ]; then
    echo "error: binary not found: $cand" >&2
    return 2
  fi
  if [ ! -f "$cand" ]; then
    echo "error: not a regular file: $cand" >&2
    return 2
  fi
  if [ ! -x "$cand" ]; then
    echo "error: binary is not executable: $cand" >&2
    return 2
  fi
  dir="$(cd "$(dirname "$cand")" && pwd)"
  echo "$dir/$(basename "$cand")"
}
