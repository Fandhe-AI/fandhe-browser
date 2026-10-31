# syntax=docker/dockerfile:1
#
# egress 隔離の docker `--network none` 経路の実測用イメージ（Issue #1386・TASK-38・RENDER-1・RENDER-3）。
#
# 役割:
#   `harness/egress-isolation/measure.py` の docker-none モードが使うイメージ。配布コンテナ
#   （Debian slim 系。設計 docs/design/servo-host-egress-isolation.md §13 項目 8）と同じ glibc
#   （Debian trixie・2.41）でホストをビルドする。measure.py は baseline / bwrap-netns / docker-none の
#   全モードで同一バイナリ（sha256 一致。`binary_match`）を要求するため、このイメージ内でビルドした
#   バイナリを `prepare.sh extract` で取り出し、全モードの `--host` に渡す。
#   逆（ホストの新しい glibc でビルドしたバイナリをこのイメージへ COPY）は GLIBC_2.43 等を要求して
#   起動できない（#1377 の再実測 run 2）ため行わない。
#
# 配布しない:
#   実測用のローカル成果物。イメージを push・配布しない（servo-host の MPL-2.0 部分を含むため。
#   NOTICE・LICENSE-THIRD-PARTY は変更しない。licensing.md）。
#
# ビルド・取り出し（リポジトリのルートをコンテキストにする。無視設定は Dockerfile.dockerignore）:
#   bash harness/egress-isolation/container/prepare.sh build fandhe-egress-isolation:local
#   bash harness/egress-isolation/container/prepare.sh extract fandhe-egress-isolation:local "$(mktemp -d)"
#
# 固定:
#   ベースイメージのダイジェストは harness/render-screenshot/container/Dockerfile と同じ値にそろえる
#   （test_container.py がドリフトを検査する）。更新手順: `crane digest rust:1-slim-trixie` /
#   `crane digest debian:trixie-slim` の値を両方の Dockerfile へ同時に転記する。

# --- build: runtime と同じ Debian trixie でビルドする（glibc をそろえる不変条件）
FROM rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build

# Servo ホストのビルド依存（render-screenshot の Dockerfile・ci.yml の servo-host-ubuntu と同じ集合）。
# mozjs_sys は C と C++ のコンパイラが同じ系統であることを要求するため gcc / g++ に固定する（#1289）。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libfontconfig1-dev libfreetype-dev libgl1-mesa-dev libx11-dev libssl-dev \
        g++ llvm clang cmake pkg-config python3 make curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENV CC=gcc CXX=g++

WORKDIR /src
COPY . .

# servo-host は別 workspace（別 Cargo.lock）。ビルドするのはホストだけ（cli は不要）。
# キャッシュマウント内の成果物は RUN の外に残らないため、cp は同じ RUN で行う。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/servo-host/target \
    cargo build --release --locked --manifest-path servo-host/Cargo.toml \
    && mkdir -p /out \
    && cp servo-host/target/release/fandhe-browser-servo-host /out/

# --- runtime
FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS runtime

# python3 は内側ランナー（in_sandbox.py）用。libegl1・libgl1-mesa-dri は Servo のソフトウェア描画用。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        python3 ca-certificates fontconfig fonts-dejavu-core \
        libegl1 libgl1-mesa-dri libfontconfig1 libfreetype6 libx11-6 \
    && rm -rf /var/lib/apt/lists/*

# 配置先は measure.DOCKER_HOST_PATH と一致させる（test_container.py が検査する）。
COPY --from=build /out/fandhe-browser-servo-host /opt/fandhe/bin/fandhe-browser-servo-host

# 動的リンクの不足を測定の前に検出する（fail-closed）。
RUN ! ldd /opt/fandhe/bin/fandhe-browser-servo-host | grep -F "not found"

# 非 root。measure.py が `--user` で実行ユーザーを上書きする。
RUN useradd --uid 1000 --user-group --create-home --home-dir /home/measure \
        --shell /usr/sbin/nologin measure
USER measure
ENV HOME=/home/measure
WORKDIR /home/measure
