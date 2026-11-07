# fandhe-browser-servo-host

`fandhe-browser` の `screenshot` が使う Servo 描画ホストです。本体（`fandhe-browser-cli`）とは別プロセスで動き、本体は IPC（stdio）でこのホストに描画を依頼します。Servo（MPL-2.0）を本体の依存グラフから隔離するための構成です（`RENDER-1`）。

## 導入

本体とホストは同じ版を入れます。

```sh
cargo install fandhe-browser-cli --version <版> --locked
cargo install fandhe-browser-servo-host --version <版> --locked
```

本体はホストを次の順で探します。PATH は探しません。

1. 環境変数 `FANDHE_BROWSER_RENDER_HOST`（絶対パス）
2. 本体のバイナリと同じディレクトリ

同じディレクトリに入れば環境変数は不要です。`--root` などで別の場所に入れたときは `FANDHE_BROWSER_RENDER_HOST` に絶対パスを渡します。設定ファイルからのホストパス指定は未配線です。

導入の確認には `fandhe-browser-servo-host --selftest` を使えます（stdout が `selftest ok:` で始まれば成功）。

## 対象 OS

macOS と Linux です。Windows は対象外です。

- `cargo install` は `libEGL.dll`・`libGLESv2.dll` を置かない
- Windows には egress 隔離の実装がなく、GA の対象外である（#1524）

## ビルドに必要なもの

共通: Rust stable。Servo のフルビルドのため、時間とディスクを多く使います（CI の実測で macOS の release ビルドは約 24 分）。`servo 0.7.0` の `rust-version` は 1.88.0 です。

正本は [`.github/workflows/ci.yml`](https://github.com/Fandhe-AI/fandhe-browser/blob/main/.github/workflows/ci.yml) の `servo-host-macos` と `servo-host-ubuntu` です。

### macOS

clang・cmake・pkg-config・python3・make と `llvm-objdump` が要ります（`mozjs_sys` の configure が要求します）。`llvm-objdump` は次のように渡します。

```sh
export LLVM_OBJDUMP="$(xcrun --find llvm-objdump 2>/dev/null || xcrun --find objdump)"
```

### Linux

- apt: `libfontconfig1-dev libfreetype-dev libgl1-mesa-dev libx11-dev libssl-dev llvm`
- ツール: clang・cmake・pkg-config・python3・make・llvm-objdump
- clang と同じ major の `libclang-<major>-dev` を入れ、`LIBCLANG_PATH=/usr/lib/llvm-<major>/lib` に固定する（別の版を拾うと `mozjs_sys` が失敗します）
- Debian bookworm 標準の clang 14・15 は `mozjs` の bindgen で失敗し、clang-19 が要ります
- freetype は 2.12 以上が要ります（Ubuntu 22.04 の 2.11.1 は不可）

上記以外の下限版は未確認です。

## 隔離

外部サイトを描画するなら egress 隔離の `required` を勧めます。設定ファイルの絶対パスを `FANDHE_BROWSER_CONFIG` で渡します（暫定の環境変数です）。

```toml
[rendering]
egress_isolation = "required"
```

- 既定の `best-effort` は、隔離できないときに警告だけ出して隔離なしで動きます
- Linux は `/usr/bin/bwrap`（`bubblewrap` パッケージ。root 所有で group・other が書き込めないこと）だけを信頼します。macOS は `/usr/bin/sandbox-exec` を使います
- 環境によっては bwrap が使えず（Ubuntu の AppArmor の userns 制限など）、そのとき `required` は起動しません
- 隔離中は WebSocket が使えず、v1 ではファイルシステムを隔離しません

詳細と限界は [egress 隔離の設計](https://github.com/Fandhe-AI/fandhe-browser/blob/main/docs/design/servo-host-egress-isolation.md) を参照してください。

## ライセンス

この crate のソースは `MIT OR Apache-2.0` です。ビルドしたバイナリは Servo などの MPL-2.0 の部品を含み、再配布するときは MPL-2.0 の義務が生じます。適用範囲とソースの入手方法は同梱の `NOTICE` を参照してください。

## セキュリティ

採用している Servo `0.7.0` は LTS ではない月次版で、セキュリティ修正が戻りません（#1300）。修正を含む LTS が出たら LTS 系列へ戻す予定です。
