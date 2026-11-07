#!/usr/bin/env bash
# crates.io 公開対象 crate の正本（#1531）。source して使う（直接実行しない）。
# 使用元: scripts/check-publish-ready.sh（allowlist 検査）・scripts/release.sh（公開順の制御）。
# `.github/workflows/release.yml` の inputs.crate の choice 選択肢は、この一覧と一致させる
# （一致は check-publish-ready.sh が検査する）。
#
# 公開対象は 9 件: 本体 workspace の member 8 crate（PUBLIC_CRATES）と、別 workspace の Servo ホスト
# （HOST_CRATE。#1543・RENDER-1）。ホストは member ではないため `-p` では扱えず、
# `--manifest-path servo-host/Cargo.toml` で指定する（crate_select_args）。

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

# Servo ホスト（別 workspace・MPL-2.0 隔離。本体の crate に依存せず、本体の crate もホストに依存しない）。
# PUBLIC_CRATES に入れないのは、member ではないことを check-publish-ready.sh が別途検査するため。
# shellcheck disable=SC2034 # source した側で参照する
HOST_CRATE=fandhe-browser-servo-host
# shellcheck disable=SC2034
HOST_MANIFEST=servo-host/Cargo.toml

# 公開順の全件。ホストは本体の 8 crate の後ろ（公開順の正本）。
# shellcheck disable=SC2034
RELEASE_CRATES=("${PUBLIC_CRATES[@]}" "${HOST_CRATE}")

# crate_select_args NAME: cargo の対象指定を配列 CRATE_SEL に設定する（ホストは `--manifest-path
# servo-host/Cargo.toml`、それ以外は `-p NAME`）。macOS の bash 3.2 でも動くよう mapfile は使わない。
# shellcheck disable=SC2034
crate_select_args() {
  if [ "$1" = "${HOST_CRATE}" ]; then
    CRATE_SEL=(--manifest-path "${HOST_MANIFEST}")
  else
    CRATE_SEL=(-p "$1")
  fi
}
