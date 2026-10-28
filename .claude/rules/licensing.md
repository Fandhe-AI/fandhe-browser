# ライセンス規約（リポ固有。OSS 系ビヘイビア）

## 本体ライセンス

- 本体は MIT OR Apache-2.0 のデュアルライセンス（`LICENSE-MIT` / `LICENSE-APACHE`）
- 各 crate の `Cargo.toml` に `license = "MIT OR Apache-2.0"` を記載する（`[workspace.package]` で共通化）

## 依存ライセンス

- 依存は MIT / Apache-2.0 / BSD / ISC / Unlicense / Zlib 等の permissive ライセンスを基本とする
- GPL / LGPL / AGPL 系の依存は導入しない
- MPL-2.0 は本体の workspace（`crates/fandhe-browser-*`）では許可しない（本体の `deny.toml` に足さない）。許可するのは Servo ホスト（次節）の workspace の `deny.toml` だけにする
- `deny.toml` の許可リストに対して `cargo deny check licenses`（`make deny`・`make ci` に含まれる）を実行し、許可外ライセンスを検出したら fail させる
- cargo-deny が許可外ライセンスの合否を fail-closed で判定するのに対し、cargo-about（`make license-inventory`・`about.toml`・`servo-host/about.toml`）は全依存の解決後ライセンス一覧を棚卸しの一次資料として生成し、GPL 系が 0 件であることを確認する（OSS-4・TASK-78.2）。`about.toml` の `accepted`・`targets` は `deny.toml` と同期する

## Servo（MPL-2.0）の隔離

オーナー判断（2026-10-12・Issue #915）により、Servo は本体の workspace に入れず、別 workspace・別 `Cargo.lock` の子プロセス（以下「Servo ホスト」）として組み込む。本体の依存グラフから MPL-2.0 を完全に外す変更で、隔離を緩めるものではない。採用版は `servo =0.6.0`（0.6 LTS 系列。#915）。ただし #1297（オーナー承認 2026-10-28）により、MDN のハング（上流 servo/servo#45915。修正 #47693 は 0.7.0 に含まれ 0.6 LTS には戻らない。#1291）の修正を取り込み IntersectionObserver の pref を有効にするため、現在は一時的に `servo =0.7.0`（LTS ではない月次版）へ上げている。緩和するのは LTS の条件だけで、隔離（`RENDER-1`）と MPL の扱い（`OSS-2`・`OSS-3`）は変えない。月次版にはセキュリティ修正が戻らないため、修正を含む次の LTS（6 か月ごと）が出たら LTS 系列へ戻す。それまでは上流の新しい版を追う。spec 本文の採用版の記載は spec リポ側で更新する（本リポからは編集しない）。設計は `docs/design/servo-host-design.md`（Issue #935）で確定し（オーナー判断 2026-10-15）、`servo-host/` に置く。

- 本体の workspace の依存グラフに Servo 系クレートを入れない。`fandhe-browser-render` を Servo に依存させない（TASK-33。render は Servo ホストへの IPC クライアント）
- 既定ビルドに Servo が含まれないことの検証（`RENDER-1`・`scripts/check-render-isolation.sh`）は、従来どおり維持する
- Servo ホスト側の `deny.toml` で許可する例外は、`MPL-2.0`・`MPL-2.0 AND BSD-3-Clause` と、`CDLA-Permissive-2.0`（`webpki-roots` の 1 件に限る）だけ。本体の `deny.toml` は変えない（`OSS-2`・`OSS-3`）
- FreeType は FTL を選び、`NOTICE` に帰属表示を載せる
- Servo のソースを改変しない（改変するとその差分に MPL のソース開示義務が生じる）。改変が必要になった場合はユーザーに判断を仰ぐ
- `NOTICE`（または `LICENSE-THIRD-PARTY`）に MPL の適用範囲（Servo ホストの配布物）・ソース入手方法を記載する。`NOTICE`（MPL の適用範囲の節）と `LICENSE-THIRD-PARTY` に記載済み（#996）
- MPL-2.0 の依存は `servo-host/` の workspace に限り、本体の crate には足さない

## 非 Cargo 資産

- ビルド時に埋め込むデータ（`include_str!` 等で取り込む事前生成 JSON）や外部テストスイートは `cargo deny` の対象外になる。導入時にライセンスを手動確認し、`NOTICE` に帰属表示を記載する

## subagent への適用

- ライセンス判断が必要な事項（新規ライセンスの許可・Servo 改変・帰属表示の要否）は Agent が決めず、ユーザーへ報告する
