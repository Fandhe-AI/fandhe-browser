# プラグイン境界ポリシー

対応 `TASK-93`（93.1）/ `MS-9` / ビヘイビア `PLUG-1`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

確定済みのビヘイビア `PLUG-1` が定めるプラグイン境界の方針と、その根拠を public 側に記録する。
本ドキュメントは新規の実測・実装を行うものではなく、PoC-15（プラグイン境界 MCP の PoC）の机上評価を出典明記のうえ整理したものである。
spec 本文中の旧称 `rust-browser` は `fandhe-browser` に読み替える。

## 方針（`PLUG-1`）

| 区分 | 方式 | 内容 |
| ---- | ---- | ---- |
| コンパイル時プラグイン | crate + Cargo feature + 配布プロファイル | ビルド時に同梱の有無を決める。実行時にコードを追加ロードしない |
| 実行時拡張 | 別プロセス境界のみ | stdio 等で接続する。ホストとは HTTP・stdio・JSON Schema の契約だけで繋がる |
| 採用しない方式 | `dylib` 動的ロード・WASM ランタイム同梱 | 下記「不採用方式と根拠」を参照 |

- プラグインからの入力は境界を越えても untrusted として検証する（[security](../../.claude/rules/security.md)「プラグイン境界」）
- 実行時に外部コードをホストのプロセス内へロードする経路を設けない。例外は設けない

## 不採用方式と根拠（PoC-15 机上評価）

| 方式 | 問題点 |
| ---- | ------ |
| `dylib` 動的ロード | Rust には安定 ABI が無い。`struct` のレイアウトや `enum` の表現はコンパイラのバージョン・最適化フラグ間で保証されず、ホストとプラグインを別ビルドして `dlopen` で結合すると、同一 `rustc`・同一依存バージョンを強制しない限り未定義動作のリスクがある。また `.so` / `.dylib` をイメージへ同梱する必要が生じ、`CTR-1`・`CTR-5` が前提とする単一バイナリでの配布を壊す |
| WASM ランタイム同梱 | ランタイム分のサイズ増分が `CTR-1`・`CTR-5` の削減率目標を侵食しうる。また分離がプロセス単位ではなくインプロセスのサンドボックスであり、メモリ空間を共有してプラグインの障害がホストへ波及しうる点で、「実行時拡張は別プロセス境界のみ」という切り分けと方針が異なる |

- WASM ランタイムのサイズ増分（数 MB から十数 MB 程度）は一般的な目安であり、PoC-15 では未実測である。`JS-3` の `boa` 増分（+7.94MB）と同オーダー以上になりうるという比較も目安に過ぎず、実測値として扱わない
- 別プロセス境界を支持する実測として、PoC-15 の Rust 版参照プラグインは `PLUG-3` の目標内に収まった。数値は [plugin-tiering.md](plugin-tiering.md) と `PLUG-3` を参照する
- 出典は PoC-15（`plugin-boundary-mcp`）の「結果 (5) 実行時動的ロードの机上評価」と「要件への示唆 5」

## ロード時動的リンクと実行時ロードの区別

- `ldd`・`readelf -d` の `NEEDED`・`otool -L`・`dumpbin /dependents` が検出するのはロード時の動的リンクである。`dlopen` / `LoadLibrary` による実行時ロードは別の機構で、これらでは検出できない
- 後続の CI 検査（#362・#364・#365）が担うのは前者である。後者は本ポリシーとレビュー（`AGENTS.md` のレビュー観点「プラグイン境界」・[security](../../.claude/rules/security.md)）で防ぐ。CI 検査が `dlopen` まで網羅するものとは扱わない
- コンテナ配布物（musl・`scratch`）は動的リンク 0 件が前提である（`CTR-4`）。ネイティブ配布物（`*-linux-gnu`・`*-apple-darwin`・`*-pc-windows-msvc`）は OS のシステムライブラリへの動的リンクが避けられず、その扱いは判断待ち（#363）である。本ドキュメントでは決めない

## 本リポジトリでの現状

確認日 2026-10-11、基準コミット `e98a3cf4`（`origin/main`）。

| 観点 | 現状 |
| ---- | ---- |
| コンパイル時プラグインの例 | `fandhe-browser-cli` の feature `js-v8`（既定）・`js-boa`・`rendering`（`JS-1`・`RENDER-1`）。feature と配布プロファイルの対応表は #361 で追加する |
| 実行時拡張の例 | `fandhe-browser-mcp`（別バイナリ・stdio）。ホストとは HTTP と [host-api.schema.json](host-api.schema.json) の契約のみで接続する（`PLUG-2`・`PLUG-3`、依存方向は `REPAIR-1`） |
| WASM ランタイム | `Cargo.lock` に `wasmtime`・`wasmer`・`wasmi` なし（`Cargo.lock` の `name` 行を grep して 0 件。確認コマンドは下記） |
| `libloading` | `Cargo.lock` に存在するが、`v8` の build-dependency 経由（`v8` → `bindgen` → `clang-sys` → `libloading`）のビルド時のみ。通常依存のグラフには現れない |

WASM ランタイムの確認コマンドは次のとおり（一致数 0 が合格）。

```bash
grep -c -i -E '^name = "(wasmtime|wasmer|wasmi)"' Cargo.lock
```

`libloading` の確認コマンドは次のとおり。

```bash
cargo tree --offline --locked --workspace -e normal,build,dev -i libloading
cargo tree --offline --locked --workspace --all-features --target all -e normal -i libloading
```

前者は上記の build-dependency 経路のみを示し、後者は `nothing to print` となる（一致 0 件は警告で、これが合格の状態）。
これを CI でゲート化するかは #362 以降の検討候補であり、現時点では未実装である。

## 関連ビヘイビアとの整合

| ID | 本ポリシーとの関係 |
| -- | ------------------ |
| `CTR-1`・`CTR-4`・`CTR-5` | 単一バイナリでの配布が前提。動的ロードする共有ライブラリを同梱しない |
| `JS-1`・`JS-3` | JS エンジンは feature でコンパイル時に同梱を選ぶ |
| `RENDER-1` | レンダリングは feature gate `rendering` 配下に隔離する |
| `REPAIR-1` | crate 境界と依存方向を保ち、プラグインを別 crate・別プロセスに分ける |
| `PLUG-2`・`PLUG-3`・`PLUG-6` | ホスト API 契約、参照プラグインの目標、公式サポート区分（[plugin-tiering.md](plugin-tiering.md)） |

## 後続 Issue への申し送り

| Issue | 内容 | 状態 |
| ----- | ---- | ---- |
| #361 | feature と配布プロファイルの対応表（`TASK-93.2`） | 未作成 |
| #363 | 3 OS ネイティブ配布物の動的リンク方針（人間判断） | 判断待ち |
| #362 | Linux 向け動的リンクなし検査スクリプト（`TASK-93.3`） | 未実装 |
| #364 | macOS 向け動的リンク検査スクリプト（`TASK-93.4`） | 未実装 |
| #365 | Windows 向け動的リンク検査スクリプト（`TASK-93.5`） | 未実装 |

親 Issue は #359（`TASK-93`）。

## 制約・未実施事項

- PoC-15 の「結果 (5)」は机上評価で、実装・実測を伴わない
- WASM ランタイムのサイズ増分は未実測の目安である
- PoC-15 の実測は macOS ローカルのみ。3 OS 横断の確認は XOS 系へ申し送り済み
- 本ドキュメントは新規の実測を含まない
- 動的リンクの CI による機械検査は未実装である（#362・#364・#365）

## 関連

- [plugin-tiering.md](plugin-tiering.md): 公式サポート区分と `PLUG-3` の目標分離
- [host-api.schema.json](host-api.schema.json): ホスト API の契約
- [render-feature-gate.md](render-feature-gate.md): `rendering` feature gate
- [security](../../.claude/rules/security.md)・`AGENTS.md`: プラグイン境界のレビュー観点
- Issue #359（親）・#360（本ドキュメント）・#361〜#365
