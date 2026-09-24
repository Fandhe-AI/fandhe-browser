# ライセンス規約（リポ固有。OSS 系ビヘイビア）

## 本体ライセンス

- 本体は MIT OR Apache-2.0 のデュアルライセンス（`LICENSE-MIT` / `LICENSE-APACHE`）
- 各 crate の `Cargo.toml` に `license = "MIT OR Apache-2.0"` を記載する（`[workspace.package]` で共通化）

## 依存ライセンス

- 依存は MIT / Apache-2.0 / BSD / ISC / Unlicense / Zlib 等の permissive ライセンスを基本とする
- GPL / LGPL / AGPL 系の依存は導入しない
- MPL-2.0 は `fandhe-browser-render`（Servo）配下に限定する
- `deny.toml` を導入した後は `cargo deny check licenses` を CI で実行し、許可外ライセンスを検出したら fail させる

## Servo（MPL-2.0）の隔離

- Servo は `fandhe-browser-render` crate 内に閉じ込め、feature gate `rendering` 無効時の依存グラフに含めない
- Servo のソースを改変しない（改変するとその差分に MPL のソース開示義務が生じる）。改変が必要になった場合はユーザーに判断を仰ぐ
- `NOTICE`（または `LICENSE-THIRD-PARTY`）に MPL の適用範囲・ソース入手方法を記載する

## 非 Cargo 資産

- ビルド時に埋め込むデータ（`include_str!` 等で取り込む事前生成 JSON）や外部テストスイートは `cargo deny` の対象外になる。導入時にライセンスを手動確認し、`NOTICE` に帰属表示を記載する

## subagent への適用

- ライセンス判断が必要な事項（新規ライセンスの許可・Servo 改変・帰属表示の要否）は Agent が決めず、ユーザーへ報告する
