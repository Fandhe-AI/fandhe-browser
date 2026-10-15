# Linux セットアップ手順（Servo ホストのビルド前提）

対応 `TASK-38` / `MS-4` / ビヘイビア `RENDER-3`（Issue #988。設計は `docs/design/servo-host-design.md`）。
Servo ホスト（`servo-host/`・別 workspace）を Linux x86_64 でビルドするための system 依存を記します。
本体 workspace（`crates/fandhe-browser-*`）のビルドには不要です。

## 確認した範囲

次の環境で `cargo build --locked --manifest-path servo-host/Cargo.toml` と
`fandhe-browser-servo-host --selftest` の成功（終了コード 0）を確認しました。

- Ubuntu（kernel 7.0）x86_64・10 コア
- Rust: リポジトリの `rust-toolchain.toml`（stable）

| 前提 | 確認した版 | 状態 |
| ---- | ---------- | ---- |
| `make` | GNU Make（`/usr/bin/make`） | 導入済みでビルド成功（mozjs のビルドが使用） |
| Python 3 | 3.14.4 | 導入済みでビルド成功 |
| `uv` | 0.12.19 | 導入済みでビルド成功 |
| `clang` / libclang | 21.1.8（`libclang1-21`） | 導入済みでビルド成功 |
| `cmake` | 4.2.3 | 導入済みでビルド成功 |
| `pkg-config` | 導入済み | 導入済みでビルド成功 |
| apt パッケージ | `libfontconfig1-dev`・`libfreetype-dev`・`libgl1-mesa-dev`・`libx11-dev`・`libssl-dev` | 導入済みでビルド成功 |

- `mozjs_sys` は C++ のソースからビルドされます（`jsapi.o` 等をビルドしたことを確認）。ビルドには
  crates.io への接続が要ります。
- `--selftest` はネットワークへ出ません（`data:` URL のみ）。

## 未確認

- 上表のうち、どれが「無いとビルドが失敗する必須品」かは未確認です（導入済みの環境でのみ試し、
  個別に外した検証はしていません）。最小の apt パッケージ集合は未確認です。
- Ubuntu 以外のディストリビューション、aarch64 は未確認です。
- 各ツールの下限版は未確認です。
- macOS・Windows は #1000 で扱います（Windows の前提は [windows.md](./windows.md)）。

## 前提の確認コマンド

```bash
make --version | head -1
python3 --version
uv --version
clang --version | head -1
cmake --version | head -1
pkg-config --version
```

## ビルドと起動確認

```bash
cargo build --locked --manifest-path servo-host/Cargo.toml
servo-host/target/debug/fandhe-browser-servo-host --selftest
```
