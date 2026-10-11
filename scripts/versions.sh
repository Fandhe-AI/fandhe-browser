#!/usr/bin/env bash
# scripts/*.sh が共有するツール版・上限値の正本。直接実行せず lib.sh 経由で source される。
#
# `: "${VAR:=default}"` 形式のため、環境変数または `make <target> VAR=value` で上書きできる
# （Makefile 時代の `:=` / `?=` と同じ上書き経路を維持する）。

# lint ツールの固定バージョン。CI（.github/workflows/ci.yml の lint-docs ジョブ）に合わせる
# （CI 側が正。乖離したらこちらを追従させる）。
# EC_NPM_VERSION のみ npm ラッパーパッケージの版（CI は Go バイナリ release タグ v3.8.0 を
# 直接取得するため版番号体系が異なる。ローカル再現用の近似として npm 最新安定を固定する）。
: "${MARKDOWNLINT_VERSION:=0.49.1}"
: "${YAMLLINT_VERSION:=1.38.0}"
: "${EC_NPM_VERSION:=6.1.1}"
: "${COMMITLINT_VERSION:=21.2.1}"
: "${COMMITLINT_CONFIG_VERSION:=21.2.0}"

# 導入系ツールの固定バージョン（`=x.y.z` 完全固定方針に合わせ exact 固定）。
# CARGO_DENY_VERSION は Dockerfile の先行導入・ci.yml の rust-lint ジョブと値を同期させる。
: "${LEFTHOOK_VERSION:=2.1.10}"
: "${CARGO_DENY_VERSION:=0.20.2}"

# check-binary-size（TASK-34.2・RENDER-2。harness/binary-size/README.md 参照）の対象 package と
# 上限バイト数。上限値の根拠（Chromium 比 80% 以上削減・Chromium 実測 457.4MB の 20% 等）は
# harness/binary-size/README.md「上限値と根拠」を参照。
: "${BINARY_SIZE_PACKAGE:=fandhe-browser-cli}"
: "${BINARY_SIZE_LIMIT_BYTES:=91480000}"

# trace-playwright（TASK-43.1・CDP-2。harness/playwright-trace/README.md 参照）が npm から一時導入する
# playwright-core の版（exact 固定）。版を上げるときは PLAYWRIGHT_INTEGRITY も更新する
# （`npm view playwright-core@<版> dist.integrity`）。run.sh が版・integrity の一致を検証する。
: "${PLAYWRIGHT_VERSION:=1.63.0}"
: "${PLAYWRIGHT_INTEGRITY:=sha512-rYCsBF/M5HjUch52bbtVONEFjv6Xu8sm8h72dNlR5bzIE1fvC/bxgspzkjSfU+MweEMmPM8KJebG6nnyxo5mCg==}"
