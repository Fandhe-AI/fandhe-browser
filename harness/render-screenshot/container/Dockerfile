# syntax=docker/dockerfile:1
#
# RENDER-5 計測用の隔離コンテナ（Issue #1281・TASK-37・関連 MEAS-3）。
#
# 役割:
#   保存済みページを `docker run --network none` のコンテナ内で loopback から配り、
#   Servo（cli `screenshot` + servo-host）と Chromium の両方に同じ
#   `http://pages.test:<port>/...` を渡して撮る。#55 のオーナー判断（2026-10-27）で、
#   この隔離は RUNBOOK 5.1 の再開条件を満たすものとして認められた。
#
# 配布しない:
#   計測用のローカル成果物。イメージを push・配布しない（servo-host の MPL-2.0 部分を
#   含むため。NOTICE・LICENSE-THIRD-PARTY は変更しない。licensing.md）。
#
# ビルド（リポジトリのルートをコンテキストにする。無視設定は Dockerfile.dockerignore）:
#   docker build -f harness/render-screenshot/container/Dockerfile \
#     -t fandhe-render-screenshot:local .
#
# 状態: Linux 実機（x86_64）で 2026-10-27 にビルド成功・RUNBOOK 5.1 の隔離撮影で 6 サイトを撮影済み
#   （#1289）。CI ではビルドしない。合否判定は #55 で人間が行う。
#
# 固定:
#   ベースイメージはダイジェスト、Chromium は版（CHROMIUM_VERSION）と Debian の
#   スナップショット（DEBIAN_SNAPSHOT）で固定する。apt の署名検証は外さない
#   （`check-valid-until=no` だけを付ける。スナップショットの Release は期限切れのため）。
#   更新手順: `crane digest rust:1-slim-trixie` / `crane digest debian:trixie-slim` の値を
#   転記し、版は https://snapshot.debian.org/mr/package/chromium/ で確かめる。

ARG DEBIAN_SNAPSHOT=20261023T000000Z
ARG CHROMIUM_VERSION=156.0.8078.47-1~deb13u1

# --- build: ホストの glibc とバイナリを揃えるため、runtime と同じ Debian trixie でビルドする
FROM rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build

# Servo ホストのビルド依存（docs/setup/linux.md・ci.yml の servo-host-ubuntu と同じ集合）。
# mozjs_sys は C と C++ のコンパイラが同じ系統であることを要求する。slim イメージには g++ が
# 無く、clang を入れると c++ が clang++ になって失敗するため（#1289）、CI の ubuntu と同じく
# gcc / g++ でビルドし、clang（libclang）は bindgen 用に使う。系統は ENV で明示する。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libfontconfig1-dev libfreetype-dev libgl1-mesa-dev libx11-dev libssl-dev \
        g++ llvm clang cmake pkg-config python3 make curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENV CC=gcc CXX=g++

WORKDIR /src
COPY . .

# servo-host は別 workspace（別 Cargo.lock）。cli は本体 workspace。
RUN cargo build --release --locked --manifest-path servo-host/Cargo.toml \
    && cargo build --release --locked -p fandhe-browser-cli \
    && mkdir -p /out \
    && cp servo-host/target/release/fandhe-browser-servo-host /out/ \
    && cp target/release/fandhe-browser /out/

# --- runtime
FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS runtime
ARG DEBIAN_SNAPSHOT
ARG CHROMIUM_VERSION

# スナップショットへ向ける（署名検証は有効のまま）。
RUN printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      'Suites: trixie trixie-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      'Suites: trixie-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      > /etc/apt/sources.list.d/debian.sources \
    && echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99snapshot

# Chromium は版を固定して導入し、食い違えばビルドを失敗させる。フォントは両エンジンで同じ
# ものを使う（SSIM のノイズを減らす）。libegl1・libgl1-mesa-dri は Servo のソフトウェア描画用。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        "chromium=${CHROMIUM_VERSION}" \
        python3 ca-certificates fontconfig fonts-dejavu-core \
        libegl1 libgl1-mesa-dri libfontconfig1 libfreetype6 libx11-6 \
    && rm -rf /var/lib/apt/lists/* \
    && chromium --version | tee /tmp/chromium-version \
    && grep -F "${CHROMIUM_VERSION%%-*}" /tmp/chromium-version \
    && rm /tmp/chromium-version

# cli は同じディレクトリの servo-host を解決する（FANDHE_BROWSER_RENDER_HOST が無いとき）。
COPY --from=build /out/fandhe-browser /out/fandhe-browser-servo-host /opt/fandhe/bin/
COPY harness/render-screenshot/capture_screenshots.py \
     harness/render-screenshot/egress_guard.py \
     harness/render-screenshot/isolation_check.py \
     harness/render-screenshot/serve_pages.py \
     harness/render-screenshot/measure_ssim.py \
     harness/render-screenshot/servo_bbox_driver.py \
     harness/render-screenshot/sites.saved-pages.json \
     harness/render-screenshot/container/run-in-container.sh \
     /opt/render-screenshot/

# 非 root。cli の既定プロファイル置き場（HOME 配下）を書き込めるようにする。uid / gid は
# RUNBOOK の `--tmpfs /home/measure:uid=1000,gid=1000` と対で 1000 に固定する（#1289）。
RUN useradd --uid 1000 --user-group --create-home --home-dir /home/measure \
        --shell /usr/sbin/nologin measure \
    && chmod 0555 /opt/render-screenshot/run-in-container.sh
USER measure
ENV HOME=/home/measure
WORKDIR /home/measure

ENTRYPOINT ["/opt/render-screenshot/run-in-container.sh"]
