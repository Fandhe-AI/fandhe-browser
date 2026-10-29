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
# CARGO_ABOUT_VERSION は make license-inventory（scripts/about.sh。TASK-78.2）の cargo-about。exact 固定で、不一致なら再導入する。
: "${CARGO_ABOUT_VERSION:=0.9.2}"

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

# endpoint-swap（TASK-46・CDP-4。harness/endpoint-swap/README.md 参照）が測定のときだけ npm から一時導入する
# puppeteer-core の版（exact 固定）。docs/design/puppeteer-compat.md の調査版に合わせる。最上位の版・integrity だけを
# 固定検証し、推移的依存（ws・chromium-bidi 等）は導入時の解決結果を結果の meta に記録する。
# Playwright 側は上の PLAYWRIGHT_VERSION / PLAYWRIGHT_INTEGRITY を共用する。
: "${PUPPETEER_VERSION:=25.12.0}"
: "${PUPPETEER_INTEGRITY:=sha512-z6LQUt5SH7jwGtGA+nMtJuEjZIsqPiwQ+EFHCYuVDQrJpvifNmmGuyDvTzAKPyFzihxgHc5f3aMzu2kn8Ps37A==}"

# measure-container-size（TASK-65.1・CTR-1・CTR-5。harness/container-size/README.md 参照）の比較対象。
# tag は CTR-1 が指定する Playwright 公式イメージ。digest は manifest list（amd64・arm64 共通）で、
# 取得時は digest 指定で pull し、取得後に RepoDigests と照合する（不一致は fail-closed）。
# 更新手順: `docker buildx imagetools inspect <image>` の Digest を転記する。
: "${CONTAINER_SIZE_REFERENCE_IMAGE:=mcr.microsoft.com/playwright:v1.49.0-noble}"
: "${CONTAINER_SIZE_REFERENCE_DIGEST:=sha256:0fc07c73230cb7c376a528d7ffc83c4bdcdcd3fc7efbe54a2eed72b1ec118377}"

# measure-container-memory（TASK-67・CTR-3・CTR-7。harness/container-memory/README.md 参照）の補助イメージ。
# 測定対象と network namespace を共有して CDP の起動待ち（busybox の sh・wget）と loaded のナビゲート（node 22）
# を行う。Cargo の依存ではなく、実行時に pull するだけで配布物には含めない。digest は manifest list で、
# 取得時は digest 指定で pull し、取得後に RepoDigests と照合する（不一致は fail-closed）。
# 更新手順: `docker buildx imagetools inspect node:22-alpine` の Digest を転記する。
: "${CONTAINER_MEMORY_HELPER_IMAGE:=node:22-alpine}"
: "${CONTAINER_MEMORY_HELPER_DIGEST:=sha256:0a7108bf6c7bf5de370ffb1a3ed6be93d405b43ff159f681a8d18c0e2bc2e402}"
