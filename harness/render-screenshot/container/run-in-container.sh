#!/usr/bin/env bash
# 計測用コンテナのエントリポイント（Issue #1281・TASK-37・RENDER-5）。
#
# 役割: `--network none` のコンテナ内で、保存済みページの loopback 配信、隔離確認、
# 両エンジンの撮影を順に実行する。`Dockerfile` の ENTRYPOINT から呼ばれる。
# 資格情報は扱わない。入力は環境変数のみ（既定値は下記）。
#
#   PAGES_DIR   保存済みページのルート（読み取り専用マウント想定。既定 /pages）
#   OUT_DIR     出力先（既定 /out）
#   EXTRA_ARGS  capture_screenshots.py へ渡す追加引数（空白区切り。例: "--min-sites 5"）
#   BBOX_SELECTORS  任意。境界ボックス取得用セレクタファイルの**コンテナ内パス**（読み取り専用マウント想定。
#               Issue #1282）。指定すると Servo の撮影を cli `screenshot` から `servo_bbox_driver.py` に
#               切り替え、PNG と `<site_id>.bboxes.json` を同じホストセッションで書く。未指定なら従来どおり
#
# 配信ポートは 8080 固定（sites.saved-pages.json の URL と一致させるため、環境変数では変えられない）。
#
# 前提: `docker run --add-host pages.test:127.0.0.1` で別名が /etc/hosts に入っていること
# （入っていなければ isolation_check の pages_host 項目が fail-closed で検出する）。
set -euo pipefail

PAGES_DIR="${PAGES_DIR:-/pages}"
OUT_DIR="${OUT_DIR:-/out}"
PAGES_PORT=8080
SCRIPT_DIR="/opt/render-screenshot"
BIN_DIR="/opt/fandhe/bin"

mkdir -p "${OUT_DIR}"

python3 "${SCRIPT_DIR}/serve_pages.py" --root "${PAGES_DIR}" --port "${PAGES_PORT}" &
SERVER_PID=$!
cleanup() {
  kill "${SERVER_PID}" 2>/dev/null || true
  wait "${SERVER_PID}" 2>/dev/null || true
}
trap cleanup EXIT

# サーバーの起動待ち（最大 10 秒）。bash の /dev/tcp で loopback の接続だけを確かめる。
for _ in $(seq 1 50); do
  if (exec 3<>"/dev/tcp/127.0.0.1/${PAGES_PORT}") 2>/dev/null; then
    break
  fi
  sleep 0.2
done

# 隔離確認。記録は OUT_DIR に残す。不合格でもここでは止めず（set -e を回避して終了コードだけ記録）、
# 次の capture_screenshots.py が同じ確認を再実行し、エンジンを起動せず `captures: []` の結果 JSON を
# 書いて終了コード 2 で終わる（RUNBOOK の契約。--min-sites 不足の終了コード 1 と区別できる）。
python3 "${SCRIPT_DIR}/isolation_check.py" \
  --sites "${SCRIPT_DIR}/sites.saved-pages.json" \
  --report "${OUT_DIR}/isolation-report.json" || echo "warning: isolation_check failed; capture_screenshots.py will refuse to launch engines" >&2

# 待ち時間（RENDER-5 の同条件比較）: Servo の cli `screenshot` には待ち時間の指定口が無く、load 完了と
# 最初のフレームを待ってから撮る（servo-host/src/page.rs の `navigate`）。そのため保存済みページモードの
# Chromium も `--virtual-time-budget` を付けず load 後に撮り、両エンジンを同じ時点で比べる。
# `--settle-ms` は両エンジンとも無効（`{settle_ms}` を使うテンプレートは capture_screenshots.py が拒否する）。
# Servo に待ち時間の指定口を足すにはホストのプロトコル変更を伴うため別 Issue で扱う。
SERVO_CMD="[\"${BIN_DIR}/fandhe-browser\",\"screenshot\",\"{url}\",\"{out}\"]"
BBOX_ARGS=()
if [ -n "${BBOX_SELECTORS:-}" ]; then
  # JSON 配列へ埋め込むため、引用符・バックスラッシュを含むパスは拒否する（eval は使わない）。
  case "${BBOX_SELECTORS}" in
    *'"'* | *\\*)
      echo "error: BBOX_SELECTORS must not contain double quotes or backslashes" >&2
      exit 2
      ;;
  esac
  if [ ! -f "${BBOX_SELECTORS}" ]; then
    echo "error: BBOX_SELECTORS is not a regular file: ${BBOX_SELECTORS}" >&2
    exit 2
  fi
  SERVO_CMD="[\"python3\",\"${SCRIPT_DIR}/servo_bbox_driver.py\",\"--host-bin\",\"${BIN_DIR}/fandhe-browser-servo-host\",\"--selectors\",\"${BBOX_SELECTORS}\",\"--assume-isolated\",\"{url}\",\"{out}\"]"
  # 最悪ケース（hello + load + screenshot + 要素ごとの問い合わせ + shutdown）が既定の 90 秒を超え得るため。
  # ドライバは各 RPC で TIMEOUT_SLACK_S（5 秒）を余分に待つので、スラック込みで
  # 78 秒（10 + 35 + 25 + 8）+ 要素数 x 10 秒。300 秒は 20 要素まで。要素が多いなら EXTRA_ARGS で延ばす。
  # argparse は後勝ちなので EXTRA_ARGS で上書きできる。
  BBOX_ARGS=(--timeout-sec 300)
fi

# EXTRA_ARGS は意図的に単語分割する（引数は呼び出し側が与える。eval は使わない）。
# shellcheck disable=SC2086
python3 "${SCRIPT_DIR}/capture_screenshots.py" \
  --saved-pages \
  --allow-unproxied-engine \
  --sites "${SCRIPT_DIR}/sites.saved-pages.json" \
  --out-dir "${OUT_DIR}" \
  --servo-cmd "${SERVO_CMD}" \
  --chromium-bin /usr/bin/chromium \
  "${BBOX_ARGS[@]}" \
  ${EXTRA_ARGS:-}
