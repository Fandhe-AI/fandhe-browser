# プラグイン境界ポリシー

対応 `TASK-93`（93.1・93.2・93.3・93.h1）/ `MS-9` / ビヘイビア `PLUG-1`（[spec-reference](../../.claude/rules/spec-reference.md)）。

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
- CI 検査（Linux は #362 で実装済み。macOS・Windows は後続の #364・#365）が担うのは前者である。後者は本ポリシーとレビュー（`AGENTS.md` のレビュー観点「プラグイン境界」・[security](../../.claude/rules/security.md)）で防ぐ。CI 検査が `dlopen` まで網羅するものとは扱わない
- コンテナ配布物（musl・`scratch`）は動的リンク 0 件が前提である（`CTR-4`）。ネイティブ配布物（`*-linux-gnu`・`*-apple-darwin`・`*-pc-windows-msvc`）は OS のシステムライブラリへの動的リンクが避けられず、その扱いは #363 のオーナー判断で許可リスト方式に決まった（「3 OS ネイティブ配布物の動的リンク方針」）

## 本リポジトリでの現状

確認日 2026-10-11、基準コミット `e98a3cf4`（`origin/main`）。

| 観点 | 現状 |
| ---- | ---- |
| コンパイル時プラグインの例 | `fandhe-browser-cli` の feature `js-v8`（既定）・`js-boa`・`rendering`（`JS-1`・`RENDER-1`）。feature と配布プロファイルの対応は下記「feature と配布プロファイルの対応」を参照 |
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
これを CI でゲート化するかは検討候補であり、現時点では未実装である（#362 の検査の対象外）。

## Linux（musl）配布バイナリの動的リンクなし検査

対応 `TASK-93`（93.3）/ `PLUG-1` / `CTR-4`。Issue #362。

- 判定の正本は `scripts/check-static-link.sh`。次の 4 点を全て満たしたときだけ pass とする（container-cloud「コンテナイメージの構成」決定 5）
  - `readelf -d` の `(NEEDED)` が 0 件
  - `readelf -l` に `INTERP` セグメントが無い
  - `ldd` に共有ライブラリ依存を示す `=>` 行が無い（`INTERP` があるバイナリには `ldd` を実行しない）
  - `file` の文言（`static-pie linked` / `statically linked`）は記録のみで判定に使わない（古い `file` は static-pie でも `dynamically linked` と出すため）
- 非 ELF・不在・`readelf` の失敗は NG（fail-closed）。`--self-test` は合成データと、実物の動的 ELF を検出できる陰性対照を含む
- 実行方法は `make check-static-link`（Linux のみ・要 musl ターゲットと `musl-tools`）。musl 三つ組で `fandhe-browser-cli` の既定（V8）と軽量（`--no-default-features --features js-boa`）を release ビルドして検査する。V8 prebuilt の SHA-256 ピンは `docker/release-build.sh` の値を使う。`make ci` には含めない
- CI では `harness-ubuntu` の `[static-link]` ステップが実行する。ビルド先は `target/` の外で、cache には入れない
- 出荷物そのものの関門は従来どおり `docker/Dockerfile.release` の `verify-*` ステージで、同じ正本スクリプトを呼ぶ。軽量コンテナの Docker ビルドは boa 系 crate のライセンス文書不足で `verify-lite` に到達しない（`docs/design/container-size-measurement.md`）ため、軽量の検査は cargo 直ビルド経路が担う
- 対象外: `*-linux-gnu` のネイティブ配布物（方針は「3 OS ネイティブ配布物の動的リンク方針」で決定済み・検査は未実装。担当 Issue は未起票）。`dlopen` による実行時ロード（レビューで防ぐ）

## 3 OS ネイティブ配布物の動的リンク方針

対応 `TASK-93`（93.h1）/ `PLUG-1`。Issue #363（オーナー判断 2026-10-12）・#916。確認日 2026-10-22、基準コミット `8373eaf1`（`origin/main`）。

### 決定事項

| 配布物 | 方式 | 検査手段 | 担当 Issue | 状態 |
| ------ | ---- | -------- | ---------- | ---- |
| `*-linux-musl`（コンテナ） | 動的リンク 0 件 | `readelf -d`・`ldd` | #362 | 実装済み（前節） |
| `*-linux-gnu`（ネイティブ） | 許可リスト | `readelf -d`・`ldd` | 未起票 | 方針のみ決定・検査は未実装 |
| `*-apple-darwin` | 許可リスト | `otool -L` | #364 | 方針のみ決定・検査は未実装 |
| `*-pc-windows-msvc` | 許可リスト（`+crt-static` 前提） | `dumpbin /dependents` | #365 | 方針のみ決定・検査は未実装 |

- ネイティブ 3 OS は OS のシステムライブラリへの動的リンクが避けられないため、許可リスト方式を採る。許可リストに無い依存（第三者共有ライブラリ・`dylib` プラグイン等）を検出したら失敗させる方針である
- OS 外のパス（macOS の `@rpath`・`@loader_path`・`/usr/local`・`/opt/homebrew` 等）は許可しない

### Windows の `+crt-static`

- Windows は `-C target-feature=+crt-static` で CRT を静的化し、許可リストを OS 標準 DLL のみにする（VC++ 再頒布可能パッケージに依存しない）
- 根拠は 2 点
  - `v8 =152.2.0` の `build.rs` は、msvc で `crt-static` が有効なら `libcpmt`（静的）、無効なら `msvcprt`（dylib）をリンクする。静的化で C++ ランタイム DLL 依存を外せる（本リポで読める一次資料）
  - 固定版 `v8` の prebuilt が静的 CRT（`/MT`）でビルドされている（#363 のオーナー記述。本リポでのリンク可否は未検証）
- 現時点では未設定である（`.cargo/config.toml` は無い）。Windows ターゲット限定の設定方法・リンク確認・サイズ差の記録は #917 が担う（`REPAIR-3`）

### 暫定許可リスト案（実測前の案であり確定値ではない）

出典の区分は (S) spec `TASK-93` の例示、(V) `v8` の `build.rs`、(D) OS バインディングを持つ依存（`reqwest` 経由の `rustls-platform-verifier`・`security-framework` 等）、(R) Rust std の link 指定。実際に現れるかは実測で確定する。

| OS | ライブラリ | 出典 |
| -- | ---------- | ---- |
| `*-linux-gnu` | `libc.so.6`・`libm.so.6`・`libgcc_s.so.1` | S |
| `*-linux-gnu` | 動的ローダー（`ld-linux-x86-64.so.2`・`ld-linux-aarch64.so.1`。`INTERP` にも現れる） | S |
| `*-linux-gnu` | `libpthread.so.0`・`libdl.so.2`・`librt.so.1`（glibc 2.34 未満で分離。新しい glibc では `libc` に統合され現れない可能性） | R |
| `*-apple-darwin` | `/usr/lib/libSystem.B.dylib` | S・R |
| `*-apple-darwin` | `Security.framework`・`CoreFoundation.framework` | D |
| `*-pc-windows-msvc` | `kernel32.dll` | S・R |
| `*-pc-windows-msvc` | `ntdll.dll`・`ws2_32.dll`・`advapi32.dll`・`userenv.dll`・`bcryptprimitives.dll`・`api-ms-win-core-synch-l1-2-0.dll` | R |
| `*-pc-windows-msvc` | `winmm.dll`・`dbghelp.dll` | V |
| `*-pc-windows-msvc` | 証明書ストア系（`crypt32.dll` 等。名前は実測で確定） | D |

- `libstdc++` は含めない。`v8` の既定 feature `use_custom_libcxx` が有効で、`build.rs` は `stdc++` を dylib 指定しない
- macOS の `libc++.1.dylib` も期待リストに入れない（同じく dylib 指定が出ない）。現れた場合は実測で扱いを判断する
- Windows で許可しないもの（`+crt-static` で外れる想定）: `vcruntime140.dll`・`msvcp140.dll`・`ucrtbase.dll`・`api-ms-win-crt-*.dll`

### 確定手順

1. #364・#365 の検査を、記録のみ（許可リスト外があっても失敗させない）で CI に導入する
2. main・`workflow_dispatch` の 3 OS 実行で実測値を集める
3. 実測値で許可リストを確定し、本節の「実測前の案」を外す
4. 検査を fail-closed に切り替える

記録のみの期間は [ci](../../.claude/rules/ci.md) の fail-closed 原則に対する、オーナー判断（#363）に基づく期間限定の例外である。切り替え条件は「許可リストの確定」で、テストの弱体化として恒久化しない。

### 検査の範囲

検査が見るのはロード時の動的リンクのみである。`dlopen`・`LoadLibrary` による実行時ロードは検出対象外で、本ポリシーとレビューで防ぐ（「ロード時動的リンクと実行時ロードの区別」）。

## feature と配布プロファイルの対応

対応 `TASK-93`（93.2）/ `PLUG-1`。確認日 2026-10-11、基準コミット `c48cbf40`（`origin/main`）。

- 起点は `fandhe-browser-cli`。非空の `default` を持つ crate は cli だけで、feature は `cli → core → js` の順に転送される（ai・cdp・`wpt_subset_runner` は core への転送のみ）
- 出典は spec「JS エンジンの切替方式」のビルド構成表と「コンテナイメージの構成」（`JS-1`・`CTR-1`・`CTR-4`）。詳細は ID から spec を参照する

### 表 A: 配布プロファイルと feature の組み合わせ

| 配布プロファイル | 有効な cli feature | cli のビルド指定 | 同梱エンジン | `rendering` | 配布経路 | 検証 |
| ---------------- | ------------------ | ---------------- | ------------ | ----------- | -------- | ---- |
| 互換（既定） | `js-v8` | 指定なし | V8 | 無効 | `docker/Dockerfile.release` の `--target default` | `[default]`（`rust-features-*`）・`[binary-size]`（`harness-*`） |
| 軽量 | `js-boa` | `--no-default-features --features js-boa` | boa | 無効 | `docker/Dockerfile.release` の `--target lite` | `[js-boa]`（`rust-features-*`）・`[js-engine-isolation]` 検査 A |
| 両エンジン同梱（配布しない） | `js-v8` + `js-boa` | `--features js-boa` | V8・boa | 無効 | 配布しない | `[default]` の両エンジンテスト・`rust-test-*` の `--all-features` |
| エンジンなし（配布しない） | なし | `--no-default-features` | なし | 無効 | 配布の要否は未決 | `[js-boa]` の `cargo check`（cli エンジンなし）・`[js-engine-isolation]` 検査 B |

- 軽量は `--no-default-features` が必須。`--features js-boa` だけでは両エンジン同梱になり `CTR-1` に反する
- 同梱していないエンジンを設定で指定すると設定エラーで起動せず、別エンジンへのフォールバックはしない（`JS-1`「設定ファイルとの関係・エラー挙動」）
- 軽量コンテナは現時点で main からビルドできない。`scripts/gen-cargo-licenses.sh` が boa 系 9 crate のライセンス文書不足で fail-closed するため（[container-size-measurement.md](container-size-measurement.md)「限界・残課題」）。表の「軽量」は feature 構成の定義であり、配布可能な状態を示すものではない（`REPAIR-3`）
- ネイティブ配布物の配布経路は未整備（`release.yml` は `workflow_dispatch` 限定でバイナリ配布を行わない）。動的リンク方針は「3 OS ネイティブ配布物の動的リンク方針」で決定済み
- サイズの出典は [js-binary-size-report.md](js-binary-size-report.md)（#368 の release プロファイル変更前の値）と [container-size-measurement.md](container-size-measurement.md)（lite は由来不明イメージによる暫定値）。数値は本表に転記しない

### 表 B: feature 別の一覧

| feature | 定義 crate | 役割 | 含まれる配布プロファイル |
| ------- | ---------- | ---- | ------------------------ |
| `js-v8` | cli（既定）→ core → js。ai・cdp・`wpt_subset_runner` は転送のみ | V8 を同梱する | 互換 |
| `js-boa` | 同上 | boa を同梱する | 軽量 |
| `rendering` | cli のみ | render crate を optional 依存として有効化する | なし。render は Servo 未同梱の雛形で、オプトイン配布は `RENDER-3`・TASK-35。コンテナ配布の対象外 |
| `test-support` | js・cdp | テスト専用の入口 | なし（dev-dependencies でのみ有効化） |

- `rendering` は JS エンジンの feature と独立に組み合わせられる。検証は `[render-isolation]`・`make lint-rendering`・`make test-rendering`
- `fandhe-browser-mcp` は feature ではなく別バイナリ・別プロセスの実行時拡張で、本表の対象外（「本リポジトリでの現状」の「実行時拡張の例」を参照）

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
| #361 | feature と配布プロファイルの対応表（`TASK-93.2`） | 作成済み（「feature と配布プロファイルの対応」） |
| #363 | 3 OS ネイティブ配布物の動的リンク方針（人間判断） | 判断済み（オーナー判断 2026-10-12）。記録は #916 |
| #916 | 上記方針の記録（`TASK-93.h1`） | 作成済み（「3 OS ネイティブ配布物の動的リンク方針」） |
| #917 | Windows の `+crt-static` 設定とリンク確認 | 未実装 |
| #362 | Linux 向け動的リンクなし検査スクリプト（`TASK-93.3`） | 実装済み（「Linux（musl）配布バイナリの動的リンクなし検査」） |
| #364 | macOS 向け動的リンク検査スクリプト（`TASK-93.4`） | 未実装（記録のみで導入し、実測後に fail-closed 化） |
| #365 | Windows 向け動的リンク検査スクリプト（`TASK-93.5`） | 未実装（記録のみで導入し、実測後に fail-closed 化） |

親 Issue は #359（`TASK-93`）。

## 制約・未実施事項

- PoC-15 の「結果 (5)」は机上評価で、実装・実測を伴わない
- WASM ランタイムのサイズ増分は未実測の目安である
- PoC-15 の実測は macOS ローカルのみ。3 OS 横断の確認は XOS 系へ申し送り済み
- 本ドキュメントは新規の実測を含まない
- 動的リンクの CI による機械検査は Linux（musl）のみ実装済みで、macOS・Windows は未実装である（#364・#365）
- 3 OS ネイティブの許可リストは実測前の暫定案である。`+crt-static` は方針として採用済みだが未設定で、本リポでのリンク可否は未検証である（#917）。`*-linux-gnu` ネイティブの検査を担う Issue は未起票である

## 関連

- [plugin-tiering.md](plugin-tiering.md): 公式サポート区分と `PLUG-3` の目標分離
- [host-api.schema.json](host-api.schema.json): ホスト API の契約
- [render-feature-gate.md](render-feature-gate.md): `rendering` feature gate
- [security](../../.claude/rules/security.md)・`AGENTS.md`: プラグイン境界のレビュー観点
- [js-binary-size-report.md](js-binary-size-report.md)・[container-size-measurement.md](container-size-measurement.md): エンジン構成別・コンテナのサイズ計測
- Issue #363（動的リンク方針の判断）・#916（その記録）・#917（Windows `+crt-static`）
- Issue #359（親）・#360（本ドキュメント）・#361（対応表）・#362〜#365
