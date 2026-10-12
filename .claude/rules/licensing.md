# ライセンス規約（リポ固有。OSS 系ビヘイビア）

## 本体ライセンス

- 本体は MIT OR Apache-2.0 のデュアルライセンス（`LICENSE-MIT` / `LICENSE-APACHE`）
- 各 crate の `Cargo.toml` に `license = "MIT OR Apache-2.0"` を記載する（`[workspace.package]` で共通化）

## 依存ライセンス

- 依存は MIT / Apache-2.0 / BSD / ISC / Unlicense / Zlib 等の permissive ライセンスを基本とする
- GPL / LGPL / AGPL 系の依存は導入しない
- MPL-2.0 は本体の workspace（`crates/fandhe-browser-*`）では許可しない（本体の `deny.toml` に足さない）。許可するのは Servo ホスト（次節）の workspace の `deny.toml` だけにする
- `deny.toml` の許可リストに対して `cargo deny check licenses`（`make deny`・`make ci` に含まれる）を実行し、許可外ライセンスを検出したら fail させる

## Servo（MPL-2.0）の隔離

オーナー判断（2026-10-12・Issue #915）により、Servo は本体の workspace に入れず、別 workspace・別 `Cargo.lock` の子プロセス（以下「Servo ホスト」）として組み込む。本体の依存グラフから MPL-2.0 を完全に外す変更で、隔離を緩めるものではない。採用版は `servo =0.6.0`。配置・プロセス間通信・CI は設計（Issue #935）で決め、実装は TASK-38 の issue で行う。

- 本体の workspace の依存グラフに Servo 系クレートを入れない。`fandhe-browser-render` を Servo に依存させない（TASK-33。この crate の役割は Issue #935 で決める）
- 既定ビルドに Servo が含まれないことの検証（`RENDER-1`・`scripts/check-render-isolation.sh`）は、従来どおり維持する
- Servo ホスト側の `deny.toml` で許可する例外は、`MPL-2.0`・`MPL-2.0 AND BSD-3-Clause` と、`CDLA-Permissive-2.0`（`webpki-roots` の 1 件に限る）だけ。本体の `deny.toml` は変えない（`OSS-2`・`OSS-3`）
- FreeType は FTL を選び、`NOTICE` に帰属表示を載せる
- Servo のソースを改変しない（改変するとその差分に MPL のソース開示義務が生じる）。改変が必要になった場合はユーザーに判断を仰ぐ
- `NOTICE`（または `LICENSE-THIRD-PARTY`）に MPL の適用範囲（Servo ホストの配布物）・ソース入手方法を記載する。記載は Servo ホストの実装と同時に行う
- Servo ホストの workspace ができるまで、本リポジトリに MPL-2.0 の依存は無い。それまでの間、どの crate にも MPL-2.0 の依存を足さない

## 非 Cargo 資産

- ビルド時に埋め込むデータ（`include_str!` 等で取り込む事前生成 JSON）や外部テストスイートは `cargo deny` の対象外になる。導入時にライセンスを手動確認し、`NOTICE` に帰属表示を記載する

## subagent への適用

- ライセンス判断が必要な事項（新規ライセンスの許可・Servo 改変・帰属表示の要否）は Agent が決めず、ユーザーへ報告する
