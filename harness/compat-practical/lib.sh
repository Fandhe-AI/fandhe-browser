#!/usr/bin/env bash
#
# compat-practical ハーネス共通関数（TASK-71.1・MEAS-4）。
# access_check.sh と、後続の run_core.sh（TASK-71.2・#311）から `source` して使う。
# 本ファイルは関数定義のみで、source した時点では何も実行しない。

# jq ラッパー。Windows（Git Bash）のネイティブ jq は行末を CRLF で出力し、`$(...)` や
# `read` へ CR が混入して等値判定・URL・件数が壊れるため、出力から CR を除く。
# 呼び出し先の jq が非 0 で終わった場合は pipefail 前提でその終了コードを返す。
# （各スクリプトは `set -o pipefail` 済みで本ファイルを source する）
jq() {
  command jq "$@" | tr -d '\r'
}

# is_public_ipv4 <addr>
#   厳密な 10 進ドット表記（先頭ゼロなし。8 進・16 進・短縮表記は curl が別アドレスへ解釈するため拒否）
#   の IPv4 で、かつ公開アドレスなら return 0。ループバック・プライベート・リンクローカル・
#   CGNAT・予約・ドキュメント用・マルチキャスト等は return 1（SSRF 対策。SEC 系）。
is_public_ipv4() {
  local re='^(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})$'
  local a b c d
  [[ "$1" =~ $re ]] || return 1
  a=${BASH_REMATCH[1]}; b=${BASH_REMATCH[2]}; c=${BASH_REMATCH[3]}; d=${BASH_REMATCH[4]}
  if [ "$a" -gt 255 ] || [ "$b" -gt 255 ] || [ "$c" -gt 255 ] || [ "$d" -gt 255 ]; then
    return 1
  fi
  if [ "$a" -eq 0 ] || [ "$a" -eq 10 ] || [ "$a" -eq 127 ] || [ "$a" -ge 224 ]; then return 1; fi
  if [ "$a" -eq 100 ] && [ "$b" -ge 64 ] && [ "$b" -le 127 ]; then return 1; fi
  if [ "$a" -eq 169 ] && [ "$b" -eq 254 ]; then return 1; fi
  if [ "$a" -eq 172 ] && [ "$b" -ge 16 ] && [ "$b" -le 31 ]; then return 1; fi
  if [ "$a" -eq 192 ] && [ "$b" -eq 168 ]; then return 1; fi
  # 192.0.0.0/24（IETF プロトコル割当。192.0.0.8 等の特殊用途を含む）と 192.0.2.0/24（ドキュメント用）
  if [ "$a" -eq 192 ] && [ "$b" -eq 0 ] && { [ "$c" -eq 0 ] || [ "$c" -eq 2 ]; }; then return 1; fi
  # 192.88.99.0/24（6to4 リレーエニーキャスト。廃止済みの特殊用途）
  if [ "$a" -eq 192 ] && [ "$b" -eq 88 ] && [ "$c" -eq 99 ]; then return 1; fi
  if [ "$a" -eq 198 ] && { [ "$b" -eq 18 ] || [ "$b" -eq 19 ]; }; then return 1; fi
  if [ "$a" -eq 198 ] && [ "$b" -eq 51 ] && [ "$c" -eq 100 ]; then return 1; fi
  if [ "$a" -eq 203 ] && [ "$b" -eq 0 ] && [ "$c" -eq 113 ]; then return 1; fi
  return 0
}

# is_public_ip <addr>
#   IPv4 は is_public_ipv4。IPv6 はグローバルユニキャスト（2000::/3）のみ許可し、
#   ドキュメント用（2001:db8::/32・3fff::/20）・6to4（2002::/16）・IETF プロトコル割当（2001::/23）は拒否。IPv4 射影（::ffff:a.b.c.d）は IPv4 として判定する。
is_public_ip() {
  local ip g
  ip="$(printf '%s' "$1" | tr 'A-Z' 'a-z')"
  case "$ip" in
    ::ffff:*.*) is_public_ipv4 "${ip#::ffff:}" ;;
    2001:db8:* | 2001:0db8:* | 2002:*) return 1 ;;
    2001:* | 3fff:*)
      # 2001::/23（IETF プロトコル割当。Teredo 2001::/32・ORCHID 等の特殊用途を含む）と
      # 3fff::/20（ドキュメント用）は 2 番目のグループの値で判定して拒否する
      g="${ip#*:}"; g="${g%%:*}"; g=$((16#${g:-0}))
      case "$ip" in
        2001:*) [ "$g" -lt 512 ] && return 1 ;;
        *) [ "$g" -lt 4096 ] && return 1 ;;
      esac
      return 0 ;;
    [23][0-9a-f][0-9a-f][0-9a-f]:*) return 0 ;;
    *:*) return 1 ;;
    *) is_public_ipv4 "$ip" ;;
  esac
}

# url_host <url>
#   https URL の authority から小文字のホスト名（ポート除去）を stdout へ出す。末尾ドットは除かない
#   （除くと検証したホスト名と curl の接続先ホスト名が食い違い、--resolve の固定を迂回されるため）。
url_host() {
  local rest authority host
  rest="${1#https://}"
  authority="${rest%%[/?#]*}"
  host="${authority%%:*}"
  host="$(printf '%s' "$host" | tr 'A-Z' 'a-z')"
  printf '%s\n' "$host"
}

# url_check <url>
#   取得してよい URL かを検証する。許可なら return 0。拒否なら理由（英語）を stdout へ出して return 1。
#   https のみ・userinfo なし・ポート 443 のみ・IPv6 リテラル不可・localhost 系/内部ドメイン不可・
#   数値風ホスト（IP リテラル）は公開 IPv4 の厳密な 10 進表記に限る（SSRF 対策。SEC 系）。
#   ホスト名は小文字化後 ASCII の DNS ラベル（英数字・ハイフン、ドット区切り）に限る。末尾ドット・
#   連続ドット・パーセントエンコード・非 ASCII（IDN）・バックスラッシュ等は curl が正規化して別の
#   ホスト名へ解釈し得る（--resolve の固定キーとずれる）ため拒否し、「検証したホスト名」と
#   「curl が実際に接続するホスト名」が常に一致することを保証する。
#   DNS 解決後のアドレス検証は呼び出し側（access_check.sh）が行う。
url_check() {
  local url="$1" rest authority host port
  case "$url" in
    https://*) ;;
    *) echo "scheme is not https"; return 1 ;;
  esac
  rest="${url#https://}"
  authority="${rest%%[/?#]*}"
  case "$authority" in
    *@*) echo "userinfo is not allowed"; return 1 ;;
    \[*) echo "IPv6 literal host is not allowed"; return 1 ;;
  esac
  host="${authority%%:*}"
  if [ "$host" != "$authority" ]; then
    port="${authority#*:}"
    if [ "$port" != "443" ]; then echo "port is not 443"; return 1; fi
  fi
  host="$(url_host "$url")"
  [ -n "$host" ] || { echo "empty host"; return 1; }
  if ! [[ "$host" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$ ]] \
    || [ "${#host}" -gt 253 ]; then
    echo "host name is not a canonical ASCII DNS name"; return 1
  fi
  case "$host" in
    localhost | *.localhost | *.local | *.internal | *.localdomain | *.lan | *.home.arpa)
      echo "internal host name is not allowed"; return 1 ;;
  esac
  if [[ "$host" =~ ^(0x[0-9a-f]+|[0-9]+)(\.(0x[0-9a-f]+|[0-9]+))*$ ]]; then
    if ! is_public_ipv4 "$host"; then echo "non-public or non-canonical IP literal"; return 1; fi
  fi
  return 0
}

# resolve_host_ips <host> [<limit_sec>]
#   <limit_sec> 指定時は解決に秒数の上限をかけ、超過したら解決コマンドの PID を kill して何も出さず return 0（呼び出し側が拒否する）。
#   ホストの IP を 1 行 1 件で stdout へ出す。解決手段は getent（Linux）→ dscacheutil（macOS）→
#   powershell の [System.Net.Dns]（Windows の Bash 環境。getent / dscacheutil が無いため）の順。
#   いずれも無い・解決できないときは何も出さず、呼び出し側（access_check.sh）が接続先を固定できない
#   ホストとして取得前に拒否する（fail-closed）。
#   PowerShell へはホスト名を環境変数で渡しコマンド文字列へ連結しない（インジェクション対策）。
#   名前解決の失敗（PowerShell の GetHostAddresses は DNS 失敗で例外 → 非 0 終了）は pipefail 下でも
#   `|| true` で吸収し、常に return 0 で「何も出さない」ことで呼び出し側の fail-closed 拒否へ繋ぐ。
#   併せてホスト名を DNS ラベル文字（英数字・ハイフン・ドット）に限って検証する。
resolve_host_ips() {
  local host="$1" limit="${2:-}" kind="" ps="" raw pid start
  if command -v getent >/dev/null 2>&1; then
    kind=getent
  elif command -v dscacheutil >/dev/null 2>&1; then
    kind=dscache
  else
    if command -v powershell.exe >/dev/null 2>&1; then
      ps=powershell.exe
    elif command -v powershell >/dev/null 2>&1; then
      ps=powershell
    fi
    if [ -n "$ps" ] && [[ "$host" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$ ]]; then
      kind=ps
    fi
  fi
  [ -n "$kind" ] || return 0
  local ps_script='[System.Net.Dns]::GetHostAddresses($env:FANDHE_RESOLVE_HOST) | ForEach-Object { $_.IPAddressToString }'
  if [ -z "$limit" ]; then
    case "$kind" in
      getent) getent ahosts "$host" 2>/dev/null | _resolve_parse getent || true ;;
      dscache) dscacheutil -q host -a name "$host" 2>/dev/null | _resolve_parse dscache || true ;;
      ps) FANDHE_RESOLVE_HOST="$host" "$ps" -NoProfile -NonInteractive -Command "$ps_script" 2>/dev/null \
        | _resolve_parse ps || true ;;
    esac
    return 0
  fi
  # 期限付き: 解決コマンド自身をバックグラウンドで直接起動し（サブシェル・パイプを挟まない）、
  # その PID を期限超過時に kill する。外部 timeout コマンドは OS ごとの有無が違うため使わない
  raw="$(mktemp)" || return 0
  case "$kind" in
    getent) getent ahosts "$host" >"$raw" 2>/dev/null & ;;
    dscache) dscacheutil -q host -a name "$host" >"$raw" 2>/dev/null & ;;
    ps) FANDHE_RESOLVE_HOST="$host" "$ps" -NoProfile -NonInteractive -Command "$ps_script" >"$raw" 2>/dev/null & ;;
  esac
  pid=$!
  start=$SECONDS
  while kill -0 "$pid" 2>/dev/null; do
    if [ $((SECONDS - start)) -ge "$limit" ]; then
      kill "$pid" 2>/dev/null || true
      sleep 0.1
      kill -9 "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      rm -f "$raw"
      return 0
    fi
    sleep 0.1
  done
  wait "$pid" 2>/dev/null || true
  _resolve_parse "$kind" <"$raw" || true
  rm -f "$raw"
  return 0
}

# _resolve_parse <kind>
#   解決コマンドの出力（stdin）から IP を 1 行 1 件へ整形する。kind は getent / dscache / ps。
_resolve_parse() {
  case "$1" in
    getent) awk '{print $1}' | sort -u ;;
    dscache) awk '/^(ip_address|ipv6_address):/ {print $2}' | sort -u ;;
    ps) tr -d '\r' | awk 'NF {print $1}' | sort -u ;;
  esac
}

# resolve_bin [<path>]
#   実行対象バイナリ（cli crate の `fandhe-browser`）の絶対パスを解決・検証する。
#   優先順位: 引数 <path> > 環境変数 FANDHE_BROWSER_BIN > <repo>/target/release/fandhe-browser
#   成功時: 解決した絶対パスを stdout へ 1 行出して return 0。
#   失敗時（不在・ディレクトリ・実行不可）: 英語のエラーを stderr へ出して return 2。
#   resolve_bin 自体はバイナリを起動しない（起動するのは run_core.sh）。引数なしで起動すると CDP サーバーが立つうえ、CLI
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

# sha256_of <file>
#   ファイルの SHA-256（16 進小文字）を stdout へ 1 行出す。sha256sum（Linux・Windows の Bash 環境）
#   → shasum（macOS）の順。run_core.sh（実測メタの tasks_sha256 記録）と make_matrix.sh
#   （実測が現行 tasks.json に対するものかの照合。TASK-71.3）が共有する。
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}
