#!/usr/bin/env bash
# crates.io 公開対象 crate の正本（#1531）。source して使う（直接実行しない）。
# 使用元: scripts/check-publish-ready.sh（allowlist 検査）・scripts/release.sh（公開順の制御）。
# `.github/workflows/release.yml` の inputs.crate の choice 選択肢は、この一覧と一致させる
# （一致は check-publish-ready.sh が検査する）。

# 公開順。依存の下位から並べる（profile・js -> core -> ai・cdp・render -> cli）。mcp は他の
# workspace crate に依存しない独立 crate。`cargo publish` は依存先が registry の index に
# 現れるまで待つが、順序を誤ると初回公開が失敗するため、この順序を崩さない。
# shellcheck disable=SC2034 # source した側で参照する
PUBLIC_CRATES=(
  fandhe-browser-profile
  fandhe-browser-js
  fandhe-browser-core
  fandhe-browser-ai
  fandhe-browser-cdp
  fandhe-browser-render
  fandhe-browser-cli
  fandhe-browser-mcp
)
