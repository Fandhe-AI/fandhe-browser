#!/usr/bin/env bash
#
# Servo の 2 workspace 構成（本体 workspace と servo-host/ workspace）の分離を検証する
# 正本スクリプト（TASK-34.1・TASK-38・RENDER-1・MS-1・#992。設計は
# docs/design/servo-host-design.md §3.2・§4.2）。
# 呼び出し元は Makefile の check-render-isolation ターゲット（薄いラッパー）と
# .github/workflows/ci.yml の harness-* ジョブの [render-isolation] ステップで、判定
# ロジックの二重管理を避けるためロジックの正本はこのファイルに一本化する。
#
# 検査は 5 本立て（A〜E）:
#   A. 本体 workspace 全体の既定ビルドに Servo 系クレートが無いこと
#      （render crate 自体は Servo 非依存の IPC クライアントなので許容し、検査対象にも含める）
#   B. fandhe-browser-cli の既定 feature に Servo 系クレートが無いこと（TASK-41.5・#174。
#      cli の manifest が無い場合は NG。fail-closed・REPAIR-5・#633）
#   C. 本体の cargo metadata に servo-host が含まれず、ルート Cargo.lock に servo / mozjs 系が
#      無いこと。正本は scripts/check-servo-host-isolation.sh で、判定は複製せず呼び出す
#   D. servo-host の依存ツリーに本体 workspace の package・v8・boa_engine が無いこと
#      （逆方向の混入。package 名の完全一致で判定する。ホスト lock の keyboard-types が
#      部分文字列 boa に一致するため部分一致にしない。前方一致 fandhe-browser-* もホスト自身
#      を誤検出するため使わない）
#   E. servo-host の依存ツリーに webgl|webgpu|gstreamer|lgpl 系が無いこと
#      （既定で無効にしたはずの GPU・メディア・LGPL 系の混入検出。--target all の出力全体に適用し、
#      musl・aarch64 Windows 等に限定された依存も検出する）。mozangle だけは Windows ターゲット
#      （`no-wgl`・ANGLE。#999）に限り許すため、非 Windows ターゲットごとの cargo tree で禁止する。
#      非 Windows の対象一覧が servo-host/deny.toml の [graph] targets と一致することも検査する
#   servo-host の manifest / Cargo.lock が無い場合は D・E を NG とする（fail-closed）。
#
# 判定は cargo tree の package 名（先頭フィールド）に限る。path 依存に付く (/abs/path) を
# 見ないので、worktree のディレクトリ名に servo が入っても誤検出しない。
# `cargo tree` は既定でホストのターゲットに絞って依存グラフを表示するため、本体側の
# cfg(windows)/cfg(unix) の target 依存は当該 OS のランナーでしか現れない。よって本スクリプトは
# 3 OS（ubuntu/macos/windows）それぞれで実行する必要がある（.claude/rules/ci.md の 3 OS 必須
# ルール）。ホスト側は --target all で lock の全ターゲット分（Windows 固有の mozangle 等の OS 固有依存を含む）
# を調べる。ホストの依存クレートの取得が発生するが、ビルドは伴わない。
#
# これはクレート名に基づく簡易検出であり、リネームや再エクスポート経由の混入までは捕捉
# できない。最終的な防御線は deny.toml（本体は [licenses] に MPL-2.0 を許可しない
# cargo deny check licenses。ホストはホスト用 deny.toml）である。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 検出パターンの正本（外部入力から組み立てない固定値）。Makefile 側の
# RENDER_ISOLATION_PATTERN は本スクリプトへ委譲し二重管理をやめる。
# A・B 用: Servo 系クレートだけを対象にする。render crate（fandhe-browser-render）は
# Servo 非依存の IPC クライアントとして cli の常時依存になり得るため含めない。
SERVO_PATTERN='servo'
# E 用: ホストが既定で無効にしたはずの GPU・メディア・LGPL 系。
HOST_FORBIDDEN_PATTERN='webgl|webgpu|gstreamer|lgpl'
# E 用（非 Windows ターゲットごと）: mozangle は Windows の `no-wgl`（ANGLE）に限り許す（#999）。
HOST_MOZANGLE_PATTERN='mozangle'
# servo-host/deny.toml の [graph] targets と一致させる（check_targets_match が検査する）。
HOST_TARGETS_NON_WINDOWS=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin)
HOST_TARGETS_WINDOWS=(x86_64-pc-windows-msvc)
# D 用: メンバー以外に、ホストへ入ってはならない JS エンジンの package 名。
JS_ENGINE_NAMES=$'v8\nboa_engine'

HOST_MANIFEST='servo-host/Cargo.toml'
HOST_LOCK='servo-host/Cargo.lock'

# GitHub Actions 上では ::notice:: 形式で skip 理由を出す（ローカルではそのまま
# echo する）。黙って通過させないための証跡。
notice() {
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    echo "::notice::$1"
  else
    echo "$1"
  fi
}

# cargo tree 出力から package 名だけを 1 行ずつ取り出す。ツリー記号（├── │ └──）と
# 空白を先頭から落とし、最初の空白以降（バージョン・(/abs/path)・feature）を捨てる。
# 判定を名前に限るのは、path 依存の (/abs/path) に servo を含む worktree 名が入っても
# A・B が誤検出しないようにするため。ヒアストリングを使いパイプは挟まない（下記 detect 参照）。
# $1 = cargo tree の標準出力
tree_names() {
  sed -E 's/^[^A-Za-z0-9_]+//; s/[[:space:]].*$//' <<<"$1"
}

# $1=検査ラベル, $2=部分一致パターン（-Ei）, $3=cargo tree の標準出力
# package 名に一致があれば NG メッセージと一致名を標準エラーへ出して 1 を返す。
# A・B（Servo 系）と E（GPU・メディア・LGPL 系）が共用する。
#
# パイプ（`printf ... | grep ... | grep -q .`）は使わない。`set -o pipefail` 下では
# パイプの終了ステータスが「右端の非ゼロ終了コマンド」になる仕様のため、一致行数が
# パイプバッファ（約 64KB）を超えると後段の `grep -q .` が最初の 1 致で早期終了して
# パイプを閉じ、前段の `grep -Ei` が SIGPIPE (141) を受ける。このとき `grep -q .`
# 自体は「一致あり」で正常終了（0）するにもかかわらず、パイプ全体の終了ステータスは
# 141 になり、`if` 条件が偽と評価されて「一致なし（OK）」に誤判定される
# （Servo が大量に混入するほど検出をすり抜ける fail-open。Issue #465 レビュー指摘）。
# ヒアストリング（`<<<`）は追加のプロセスをパイプで挟まないため、この問題が起きない。
detect() {
  local label="$1" pat="$2" out="$3" names
  names="$(tree_names "$out")"
  if grep -Eqi -- "$pat" <<<"$names"; then
    echo "NG: dependency graph of ${label} contains forbidden crates (pattern: ${pat})" >&2
    grep -Ei -- "$pat" <<<"$names" >&2
    return 1
  fi
  echo "OK: dependency graph of ${label} contains no forbidden crates (pattern: ${pat})"
  return 0
}

# 検査 E 用: ターゲット三つ組が Windows か。
is_windows_target() { case "$1" in *-windows-*) return 0 ;; *) return 1 ;; esac; }

# 検査 E 用: deny.toml の [graph] targets（$1。改行区切り）と、本スクリプトの検査対象
# （$2=非 Windows・$3=Windows。空白区切り）が集合として一致するか。不一致・空は NG（fail-closed）。
check_targets_match() {
  local denyt="$1" mine want
  mine="$(printf '%s\n' $2 $3 | sort -u)"
  want="$(printf '%s\n' "$denyt" | sed '/^$/d' | sort -u)"
  if [ -z "$want" ] || [ "$mine" != "$want" ]; then
    echo "NG: servo-host targets differ from deny.toml [graph] targets" >&2
    echo "  script:    $(tr '\n' ' ' <<<"$mine")" >&2
    echo "  deny.toml: $(tr '\n' ' ' <<<"$want")" >&2
    return 1
  fi
  return 0
}

# 検査 D の判定: package 名が禁止名集合に完全一致するか。部分一致にしないのは、ホストの
# lock に keyboard-types（部分文字列 boa を含む）等があるため。ヒアストリングのみ使う。
# $1=検査ラベル, $2=禁止名（改行区切り）, $3=cargo tree の標準出力
# 禁止名集合が空なら NG（メンバー取得の失敗で素通りする fail-open を防ぐ）。
detect_exact() {
  local label="$1" forbidden="$2" out="$3" names name
  local -a args=()
  while IFS= read -r name; do
    if [ -n "$name" ]; then
      args+=(-e "$name")
    fi
  done <<<"$forbidden"
  if [ "${#args[@]}" -eq 0 ]; then
    echo "NG: forbidden package set for ${label} is empty" >&2
    return 1
  fi
  names="$(tree_names "$out")"
  if grep -Fxq "${args[@]}" <<<"$names"; then
    echo "NG: dependency graph of ${label} contains workspace members or JS engines" >&2
    grep -Fx "${args[@]}" <<<"$names" >&2
    return 1
  fi
  echo "OK: dependency graph of ${label} contains no workspace members or JS engines"
  return 0
}

# manifest 等の必須ファイルが無ければ NG を標準エラーへ出して 1 を返す。
# cli / servo-host の削除・リネーム・破損で検査が「未導入のため skip」として黙って通過
# する fail-open を防ぐ（REPAIR-5・RENDER-1・#633・#992）。
# $1 = ファイルのパス, $2 = 検査対象の表示名
require_file() {
  local path="$1" what="$2"
  if [ ! -f "$path" ]; then
    echo "NG: ${path} not found, cannot check ${what}" >&2
    return 1
  fi
  return 0
}

# 本体 workspace のメンバー package 名（改行区切り）を標準出力へ出す。
# check-servo-host-isolation.sh と同じ jq 式。Windows の jq は CRLF で出力することが
# あるため CR を落とす。失敗・空は呼び出し側で NG にする。
workspace_member_names() {
  local meta names
  meta="$(cargo metadata --no-deps --format-version 1)" || return 1
  names="$(jq -r '. as $m | $m.packages[] | select(.id as $id | $m.workspace_members | index($id)) | .name' <<<"$meta")" || return 1
  names="${names//$'\r'/}"
  if [ -z "$names" ]; then
    return 1
  fi
  printf '%s\n' "$names"
}

# 自己テスト: 合成した cargo tree 出力に対して判定だけを確認する
# （cargo を一切呼ばない。compat-regression の self-test.sh と同じく、品質ゲート
# が無言で素通りしないことの証跡を CI ログへ残す目的）。
self_test() {
  local failures=0

  # $1=説明, $2=期待（ng|ok）, 以降=判定関数と引数
  expect() {
    local desc="$1" want="$2" rc=0
    shift 2
    "$@" >/dev/null 2>&1 || rc=$?
    if [ "$want" = "ng" ] && [ "$rc" -ne 0 ]; then
      echo "OK(self-test): ${desc}"
    elif [ "$want" = "ok" ] && [ "$rc" -eq 0 ]; then
      echo "OK(self-test): ${desc}"
    else
      echo "NG(self-test): ${desc} (want=${want} rc=${rc})" >&2
      failures=$((failures + 1))
    fi
  }

  # --- 検査 A・B（Servo 系） ---
  expect "A/B: servo crate is detected (tree form)" ng detect t "$SERVO_PATTERN" "$(printf '├── servo v0.0.1\n└── tokio v1.53.1')"
  expect "A/B: servo-host crate is detected (prefix none)" ng detect t "$SERVO_PATTERN" "$(printf 'fandhe-browser-servo-host v0.1.0 (/x)\ntokio v1.53.1')"
  expect "A/B: servo_arc is detected" ng detect t "$SERVO_PATTERN" "$(printf 'servo_arc v0.4.0\n')"
  expect "A/B: render crate is allowed" ok detect t "$SERVO_PATTERN" "$(printf 'fandhe-browser-render v0.1.0 (/x/crates/fandhe-browser-render)\n')"
  expect "A/B: servo only in a path is not detected" ok detect t "$SERVO_PATTERN" "$(printf 'fandhe-browser-core v0.1.0 (/work/feat-993-servo-host/crates/fandhe-browser-core)\n├── serde v1.0.0 (/work/servo-host/x)')"
  expect "A/B: unrelated lines are not detected" ok detect t "$SERVO_PATTERN" "$(printf 'tokio v1.53.1\nserde v1.0.0\n')"

  # 大量一致時の fail-open 回帰テスト（Issue #465 レビュー指摘）。
  # パイプバッファ（約 64KB）を超える一致行数を再現するため、bash 組み込みの
  # for ループで合成する（`yes | head` 等の外部コマンドをパイプで挟むと同じ
  # SIGPIPE 経路を踏みかねないため使わない）。
  local big_servo="" big_clean="" i
  for ((i = 0; i < 3000; i++)); do
    big_servo+="├── servo_dep_${i} v0.0.1 (crates/some/long/synthetic/path/for/buffer)"$'\n'
  done
  for ((i = 0; i < 3000; i++)); do
    big_clean+="├── tokio_dep_${i} v1.0.0 (crates/some/long/synthetic/path/for/buffer)"$'\n'
  done
  expect "A/B: bulk servo lines are detected" ng detect t "$SERVO_PATTERN" "$big_servo"
  expect "A/B: bulk unrelated lines are not detected" ok detect t "$SERVO_PATTERN" "$big_clean"

  # --- 検査 D（本体メンバー・JS エンジンの逆混入。完全一致） ---
  local members=$'fandhe-browser-core\nfandhe-browser-render\nwpt-subset-runner'
  local forbidden="${members}"$'\n'"${JS_ENGINE_NAMES}"
  expect "D: workspace member core is detected" ng detect_exact t "$forbidden" "$(printf 'fandhe-browser-servo-host v0.1.0 (/x)\nfandhe-browser-core v0.1.0 (/y)\n')"
  expect "D: workspace member render is detected" ng detect_exact t "$forbidden" "$(printf 'fandhe-browser-render v0.1.0\n')"
  expect "D: wpt-subset-runner is detected" ng detect_exact t "$forbidden" "$(printf 'wpt-subset-runner v0.1.0\n')"
  expect "D: v8 is detected" ng detect_exact t "$forbidden" "$(printf 'servo v0.6.0\nv8 v1.0.0\n')"
  expect "D: boa_engine is detected" ng detect_exact t "$forbidden" "$(printf 'boa_engine v0.20.0\n')"
  expect "D: host itself is not detected" ok detect_exact t "$forbidden" "$(printf 'fandhe-browser-servo-host v0.1.0 (/x)\nservo v0.6.0\n')"
  expect "D: partial-match names are not detected" ok detect_exact t "$forbidden" "$(printf 'keyboard-types v0.7.0\nv8-like v1.0.0\nfandhe-browser-core-extra v1.0.0\n')"
  expect "D: empty forbidden set is NG (fail-closed)" ng detect_exact t "" "$(printf 'tokio v1.0.0\n')"
  expect "D: bulk lines with one member at the end are detected" ng detect_exact t "$forbidden" "${big_clean}├── fandhe-browser-core v0.1.0"$'\n'
  expect "D: bulk unrelated lines are not detected" ok detect_exact t "$forbidden" "$big_clean"

  # --- 検査 E（GPU・メディア・LGPL 系） ---
  local n
  for n in servo-webgl webgpu-sys gstreamer-sys lgpl-thing; do
    expect "E: ${n} is detected" ng detect t "$HOST_FORBIDDEN_PATTERN" "$(printf 'tokio v1.0.0\n%s v0.1.0\n' "$n")"
  done
  for n in glow surfman servo-canvas mozjs_sys mozangle; do
    expect "E: ${n} is not detected" ok detect t "$HOST_FORBIDDEN_PATTERN" "$(printf '%s v0.1.0\n' "$n")"
  done
  expect "E: gstreamer in --target all output is detected" ng detect t "$HOST_FORBIDDEN_PATTERN" "$(printf 'servo v0.6.0\nmozangle v0.7.1\ngstreamer v0.1.0\n')"
  expect "E: mozangle alone is not in the all-target pattern" ok detect t "$HOST_FORBIDDEN_PATTERN" "$(printf 'servo v0.6.0\nmozangle v0.7.1\n')"
  expect "E: mozangle on a non-Windows target is detected" ng detect t "$HOST_MOZANGLE_PATTERN" "$(printf 'servo v0.6.0\nmozangle v0.7.1\n')"
  expect "E: mozangle on a Windows target is allowed (no check)" ok is_windows_target x86_64-pc-windows-msvc
  expect "E: linux target is not Windows" ng is_windows_target x86_64-unknown-linux-gnu
  expect "E: targets match deny.toml" ok check_targets_match "$(printf 'b\na\nw\n')" "a b" "w"
  expect "E: target missing from the check list is NG" ng check_targets_match "$(printf 'a\nb\nc\n')" "a b" "w"
  expect "E: extra target in the check list is NG" ng check_targets_match "$(printf 'a\n')" "a b" "w"
  expect "E: bulk lines with one match at the end are detected" ng detect t "$HOST_FORBIDDEN_PATTERN" "${big_clean}├── gstreamer v0.1.0"$'\n'
  expect "E: bulk unrelated lines are not detected" ok detect t "$HOST_FORBIDDEN_PATTERN" "$big_clean"

  # --- 検査 C（判定の正本は check-servo-host-isolation.sh の自己テスト） ---
  expect "C: check-servo-host-isolation.sh self-test passes" ok bash scripts/check-servo-host-isolation.sh --self-test

  # --- manifest の不在は NG（fail-closed・#633・#992）。存在側は自スクリプトで代用し、
  # 実リポの有無に依存させない ---
  local err_missing
  if err_missing=$(require_file "crates/__does_not_exist__/Cargo.toml" "fandhe-browser-cli (default features)" 2>&1 >/dev/null); then
    echo "NG(self-test): missing cli manifest was not NG (fail-open regression)" >&2
    failures=$((failures + 1))
  elif [[ "$err_missing" != *"not found, cannot check fandhe-browser-cli"* ]]; then
    echo "NG(self-test): unexpected NG message for missing cli manifest: ${err_missing}" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): missing cli manifest is NG"
  fi
  if err_missing=$(require_file "servo-host/__does_not_exist__/Cargo.toml" "servo-host dependency tree" 2>&1 >/dev/null); then
    echo "NG(self-test): missing servo-host manifest was not NG (fail-open regression)" >&2
    failures=$((failures + 1))
  elif [[ "$err_missing" != *"not found, cannot check servo-host dependency tree"* ]]; then
    echo "NG(self-test): unexpected NG message for missing servo-host manifest: ${err_missing}" >&2
    failures=$((failures + 1))
  else
    echo "OK(self-test): missing servo-host manifest is NG"
  fi
  expect "existing file is not NG" ok require_file "${BASH_SOURCE[0]}" self

  if [ "$failures" -ne 0 ]; then
    echo "NG: self-test has ${failures} failure(s)" >&2
    return 1
  fi
  echo "OK: all self-tests passed"
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

# 本体の検査 A・B は CI（GITHUB_ACTIONS=true）でだけ --locked を付け、コミット済み
# Cargo.lock との食い違い（改ざん・未更新）を fail させる。ローカル実行（make 経由）は
# 既存挙動に合わせて付けない。ホストの検査 D・E は、ローカルでも servo-host/Cargo.lock を
# 書き換えないよう常に --locked を付ける。
CARGO_TREE_LOCKED_ARGS=()
if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
  CARGO_TREE_LOCKED_ARGS=(--locked)
fi

STATUS=0

# 検査 A: 本体 workspace 全体（render crate を含む）
# 列挙の失敗（find の非ゼロ終了）を「member 無し」と混同しない。失敗を握りつぶすと
# 検査 A が無言で skip されて分離ゲートが素通りする（fail-open。Issue #465 レビュー指摘）。
# crates ディレクトリ自体が無い場合は正常な「member 無し」として空扱いにし、
# それ以外の find 失敗は非ゼロ終了する。
WORKSPACE_MEMBERS=""
if [ -d crates ]; then
  if ! WORKSPACE_MEMBERS="$(find crates -mindepth 2 -maxdepth 2 -name 'Cargo.toml')"; then
    echo "NG: failed to enumerate workspace members (find crates)" >&2
    exit 1
  fi
fi
if [ -z "$WORKSPACE_MEMBERS" ]; then
  notice "skip: no member crates, skipping the workspace check"
else
  # 標準出力だけを判定対象にし、標準エラーはそのまま通す（2>&1 で合流させない）。
  # cargo tree 自体の失敗原因（--locked のロック不整合等）もログから読み取れるようにする。
  if ! OUT_A=$(cargo tree --workspace -e normal,build,dev --prefix none ${CARGO_TREE_LOCKED_ARGS[@]+"${CARGO_TREE_LOCKED_ARGS[@]}"}); then
    echo "NG: cargo tree (workspace) failed" >&2
    STATUS=1
  else
    detect "workspace (default build)" "$SERVO_PATTERN" "$OUT_A" || STATUS=1
  fi
fi

# 検査 B: fandhe-browser-cli の既定 feature（TASK-41.5・#174）。manifest 不在は NG
if ! require_file crates/fandhe-browser-cli/Cargo.toml "fandhe-browser-cli (default features)"; then
  STATUS=1
else
  if ! OUT_B=$(cargo tree -p fandhe-browser-cli -e normal,build,dev --prefix none ${CARGO_TREE_LOCKED_ARGS[@]+"${CARGO_TREE_LOCKED_ARGS[@]}"}); then
    echo "NG: cargo tree (fandhe-browser-cli) failed" >&2
    STATUS=1
  else
    detect "fandhe-browser-cli (default features)" "$SERVO_PATTERN" "$OUT_B" || STATUS=1
  fi
fi

# 検査 C: servo-host が本体 workspace・ルート Cargo.lock に混入していないこと。
# 判定の正本は check-servo-host-isolation.sh（jq 欠落は exit 2。いずれも NG 扱い）。
if ! bash scripts/check-servo-host-isolation.sh; then
  STATUS=1
fi

# 検査 D・E: servo-host 側の依存ツリー（独立した Cargo.lock を持つ別 workspace）。
# manifest / lock の不在は NG（fail-closed）。
HOST_OK=1
require_file "$HOST_MANIFEST" "servo-host dependency tree" || HOST_OK=0
require_file "$HOST_LOCK" "servo-host dependency tree" || HOST_OK=0
if [ "$HOST_OK" -eq 0 ]; then
  STATUS=1
else
  # 検査 D は --target all で lock に載る全ターゲット分を調べ、OS 固有の依存を ubuntu でも検出する。
  if ! OUT_HOST=$(cargo tree --manifest-path "$HOST_MANIFEST" --locked -e normal,build,dev --prefix none --target all); then
    echo "NG: cargo tree (servo-host) failed" >&2
    STATUS=1
  else
    # 検査 D: 本体メンバー集合は cargo metadata で得る（取得失敗・空は NG）。
    if ! command -v jq >/dev/null 2>&1; then
      echo "error: jq is required by $(basename "$0")" >&2
      exit 2
    fi
    if ! MEMBER_NAMES="$(workspace_member_names)"; then
      echo "NG: failed to get workspace members (cargo metadata / jq)" >&2
      STATUS=1
    else
      detect_exact "servo-host" "${MEMBER_NAMES}"$'\n'"${JS_ENGINE_NAMES}" "$OUT_HOST" || STATUS=1
    fi
    # 検査 E: 禁止パターンは --target all の出力全体（OUT_HOST）に適用する。
    detect "servo-host" "$HOST_FORBIDDEN_PATTERN" "$OUT_HOST" || STATUS=1
    # mozangle は非 Windows ターゲットごとに禁止する（Windows は許可）。cargo tree の失敗は NG。
    DENY_TARGETS="$(sed -n '/^targets = \[/,/^\]/p' servo-host/deny.toml | grep -o '"[^"]*"' | tr -d '"' || true)"
    check_targets_match "$DENY_TARGETS" "${HOST_TARGETS_NON_WINDOWS[*]}" "${HOST_TARGETS_WINDOWS[*]}" || STATUS=1
    for t in "${HOST_TARGETS_NON_WINDOWS[@]}"; do
      if ! OUT_T=$(cargo tree --manifest-path "$HOST_MANIFEST" --locked -e normal,build,dev --prefix none --target "$t"); then
        echo "NG: cargo tree (servo-host, target $t) failed" >&2
        STATUS=1
        continue
      fi
      detect "servo-host ($t)" "$HOST_MOZANGLE_PATTERN" "$OUT_T" || STATUS=1
    done
  fi
fi

exit "$STATUS"
