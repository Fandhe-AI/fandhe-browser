#!/usr/bin/env bash
# 計測用コンテナのエントリポイント（Issue #1281・TASK-37・RENDER-5）。
#
# 役割: `--network none` のコンテナ内で、保存済みページの loopback 配信、隔離確認、
# 両エンジンの撮影を順に実行する。`Dockerfile` の ENTRYPOINT から呼ばれる。
# 資格情報は扱わない。入力は環境変数のみ（既定値は下記）。
#
#   PAGES_DIR   保存済みページのルート（読み取り専用マウント想定。既定 /pages）
#   OUT_DIR     出力先（既定 /out）
#   PAGES_PORT  配信ポート（sites.saved-pages.json の URL と揃える。既定 8080）
#   EXTRA_ARGS  capture_screenshots.py へ渡す追加引数（空白区切り。例: "--settle-ms 8000"）
#
# 前提: `docker run --add-host pages.test:127.0.0.1` で別名が /etc/hosts に入っていること
# （入っていなければ isolation_check の pages_host 項目が fail-closed で検出する）。
set -euo pipefail

PAGES_DIR="${PAGES_DIR:-/pages}"
OUT_DIR="${OUT_DIR:-/out}"
PAGES_PORT="${PAGES_PORT:-8080}"
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

# 隔離確認。不合格ならここで止まる（set -e）。記録は OUT_DIR に残す。
python3 "${SCRIPT_DIR}/isolation_check.py" \
  --sites "${SCRIPT_DIR}/sites.saved-pages.json" \
  --report "${OUT_DIR}/isolation-report.json"

# 待ち時間の非対称（RENDER-5 の比較上の注意）: Chromium は `--virtual-time-budget={settle_ms}` で
# 指定時間ぶん仮想時間を進めるが、Servo 側の cli `screenshot` には待ち時間の指定口が無い。
# Servo ホストは load 完了と最初のフレームを待ってから撮る（servo-host/src/page.rs の
# `navigate`）。そのため EXTRA_ARGS の --settle-ms は Chromium にだけ効く。保存済みページは
# ネットワーク待ちがなく静的に近いが、同条件の比較とは言えない。指定口の追加は
# Servo ホストのプロトコル変更を伴うため別 Issue で扱う。
SERVO_CMD="[\"${BIN_DIR}/fandhe-browser\",\"screenshot\",\"{url}\",\"{out}\"]"

# EXTRA_ARGS は意図的に単語分割する（引数は呼び出し側が与える。eval は使わない）。
# shellcheck disable=SC2086
python3 "${SCRIPT_DIR}/capture_screenshots.py" \
  --saved-pages \
  --allow-unproxied-engine \
  --sites "${SCRIPT_DIR}/sites.saved-pages.json" \
  --out-dir "${OUT_DIR}" \
  --servo-cmd "${SERVO_CMD}" \
  --chromium-bin /usr/bin/chromium \
  ${EXTRA_ARGS:-}
