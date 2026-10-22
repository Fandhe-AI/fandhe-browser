# 依存ツリー棚卸しレポート

- **対応ビヘイビア**: `OSS-4`（関連: `OSS-2`・`OSS-3`・`OSS-7`・`RENDER-1`）
- **対応タスク**: TASK-78（TASK-78.4・#486）。前段は TASK-78.1（#483・PR #1053）・TASK-78.2（#484・PR #1055）・TASK-78.3（#485・PR #866）
- **マイルストーン**: MS-4
- **実施日**: 2026-10-16（UTC）
- **基準コミット**: `fb1c2299`（`origin/main`）
- **実行環境**: Linux 7.0.0-34-generic / `x86_64-unknown-linux-gnu` / rustc 1.99.0 / cargo 1.99.0
- **ツール版**: cargo-deny 0.20.2・cargo-about 0.9.2・jq 1.8.1（固定版の正本は `scripts/versions.sh`）
- **状態**: 棚卸し完了。解決後ライセンスの GPL 系は本体・Servo ホストとも 0 件。判断事項は末尾「要判断事項」のとおりオーナーへ報告する（Agent は決めていない）

## 目的・前提

`OSS-4` の期待値「間接依存を含めて GPL 系の混入が 0 件」の証跡として、TASK-78.1〜78.3 の結果を 1 本にまとめる。`OSS-4` は spec 上まだ「検討中」で、本レポートは確定に移すための証跡を出すだけである。ステータスの変更は spec リポ側で判断する。

| ツール | 役割 |
| ------ | ---- |
| cargo-deny（`scripts/deny.sh`） | 許可外ライセンスの合否判定（fail-closed） |
| cargo-about（`scripts/about.sh`） | crate ごとの解決後ライセンスの一次資料 |
| `scripts/gen-cargo-licenses.sh` | 配布用の許諾文全文の生成（`OSS-2`）。本レポートの対象外 |

構成の現状として、`rendering` feature は #236 で廃止された。Servo は #915 の判断で別 workspace の `servo-host/` に隔離され、MPL-2.0 を許可するのは `servo-host/deny.toml` だけである（[licensing](../../.claude/rules/licensing.md)）。#483 の受入基準にある「`--features rendering`」「MPL-2.0 は `fandhe-browser-render` 配下に限る」はこの変更より前の記述なので、本レポートは現行構成に読み替えて記録する。spec 側の TASK-78.1 の文言を直すかどうかは「要判断事項」に挙げる。

## cargo deny の実行結果

| 項目 | 本体 workspace | Servo ホスト（`servo-host/`） |
| ---- | -------------- | ----------------------------- |
| 設定 | `deny.toml` | `servo-host/deny.toml` |
| 対象 feature | `all-features = true`（既定・エンジンなし・`js-boa`・`js-v8`・両エンジン同梱・`test-support` を包含。各構成の個別実測は #1053） | `all-features = false` |
| ターゲット | 8 件（linux gnu / musl・darwin・windows-msvc の x86_64 / aarch64） | 5 件（aarch64-windows と musl は設計 §7 の検証まで外す） |
| dev / build 依存 | `include-dev = true` | `include-dev = true` |
| コマンド | `cargo deny --locked check licenses`、`make deny` | `scripts/deny.sh servo-host` |
| 結果 | `licenses ok`（`make deny` は advisories・bans・licenses・sources すべて ok） | advisories・bans・licenses・sources すべて ok |
| 警告・例外 | `BSD-2-Clause` の unmatched license allowance（情報）。advisories の ignore は `RUSTSEC-2024-0436`（`paste`。#152 で追跡） | `webpki-roots` の `CDLA-Permissive-2.0` 例外。MPL-2.0 の許可はここだけ |

- 本体の `licenses ok` は MPL-2.0 が混入していない証跡になる（本体の allow に MPL-2.0 が無いため、混入すれば fail する。`RENDER-1`）。
- `BSD-2-Clause` は Cargo 依存では使われていない。Cargo 外の rapidhash-v8 が BSD-2-Clause のため `deny.toml` に残っており、`about.toml` と同期する約束があるので本 PR では触らない。
- #1053 の知見として、`cargo deny list` は `[graph] all-features` を反映しない。一覧形式で証跡を取るときは `--all-features` を明示する。

## cargo-about の実行結果

実行コマンドは `LICENSE_INVENTORY_DIR=<worktree 外の一時ディレクトリ> scripts/about.sh`（`make license-inventory` 相当）。判定は解決後ライセンスで行い、GPL 系が 1 件でもあれば exit 1 になる。

| 項目 | 本体 workspace | Servo ホスト |
| ---- | -------------- | ------------ |
| 生成日時（UTC） | 2026-10-16T08:02:51Z | 2026-10-16T08:03:22Z |
| features | all-features | default |
| ターゲット | 8 件（deny と同じ） | 5 件（deny と同じ） |
| 外部 crate 数 | 299 | 619 |
| 解決後 GPL 系 | 0 件 | 0 件 |
| 宣言 SPDX 式に GPL 系を含む crate | なし | なし |
| MPL-2.0 | 0 件 | 96 件 |

### 解決後ライセンスの集計

表の数値は crate 数ではなく、ライセンスごとの出現数である。複数ライセンスを解決された crate（デュアルライセンス等）は該当する各ライセンスに重複して計上するため、合計は「外部 crate 数」と一致しない（本体 323 対 299、Servo ホスト 685 対 619）。

| ライセンス | 本体 | Servo ホスト |
| ---------- | ---- | ------------ |
| MIT | 267 | 508 |
| Unicode-3.0 | 26 | 37 |
| ISC | 19 | 5 |
| Apache-2.0 | 8 | 11 |
| BSD-3-Clause | 2 | 23 |
| Zlib | 1 | 3 |
| MPL-2.0 | 0 | 96 |
| BSD-2-Clause | 0 | 1 |
| CDLA-Permissive-2.0 | 0 | 1 |

MPL-2.0 が Servo ホストにしか無いことは、`RENDER-1` の隔離（本体の依存グラフに Servo 系を入れない）と整合する。

### 制約

- crate ごとの全件表は載せない（Servo ホストだけで約 600 行）。`make license-inventory` で再生成できる。
- `[private] ignore = true` により workspace crate は一覧から外れる。これらには本体の MIT OR Apache-2.0 が適用されるので網羅性は欠けない。設定の理由は、cargo-about 0.9.2 が非 ASCII の manifest で panic するためである。
- `--offline` で実行するため、LICENSE を同梱しない crate は既定の許諾文で補われる。許諾文の完全性は `gen-cargo-licenses.sh` の責務である。

## v8 prebuilt 同梱部品の照合結果

Issue #866（TASK-78.3）の結果を要約する。再実測はしていない。詳細は [v8-prebuilt-components.md](./v8-prebuilt-components.md) を参照。

- 対象は v8 crate 152.2.0。実測済みの prebuilt は x86_64 の linux-gnu・linux-musl・windows-msvc の 3 件。
- 照合結果:

  | 区分 | 件数 | 内容 |
  | ---- | ---- | ---- |
  | A: 一致 | 4 | V8・libc++・libc++abi・ICU（libc++abi は MSVC には同梱されない） |
  | B: 差分 | 9 | `NOTICE` に追記した部品 |
  | C: Cargo 側で扱う | - | `temporal_rs` 等 |
  | D: 同梱を確認できず | 10 | 非同梱とは断定していない |

- 区分 C は、本体の cargo-about 一覧に `temporal_rs` 0.2.6・`temporal_capi` 0.2.6・`timezone_provider` 0.2.6（いずれも MIT 解決）、`zoneinfo64` 0.3.0 と `icu_*` 2.3.x（Unicode-3.0）が現れることを確認した。Cargo 依存の帰属表示で扱える。
- **`NOTICE` の更新: #866 で実施済み**。第 1 節に区分 B の 9 部品を追加し、libc++abi の同梱条件を実測に合わせ、`LICENSE-THIRD-PARTY` に節 5〜13 を追加した。本 PR では `NOTICE` を追加更新しない。

### 未解消の事項

- macOS（aarch64-apple-darwin）の prebuilt は未実測。
- aarch64-unknown-linux-musl の prebuilt は部品照合が未実施（SHA-256 のピンのみ）。
- 最終バイナリ（リンク後）に残る部品の確認は未実施。アーカイブの中身はリンクされ得る部品の上限にすぎない。
- `Cargo.toml` の v8 コメントが古い。「既定ビルド（`js-v8` 無効）」とあるが、現在の cli の既定 feature は `js-v8`。本 PR では直していない。

## 棚卸しの対象外

- `OSS-7` / TASK-103 の npm 由来データ（caniuse-lite・web-features・WPT）は cargo-deny / cargo-about の対象外で、手動レビューで扱う（記録は [data-license-review.md](data-license-review.md)）。
- `NOTICE` 第 2 節末尾の未整備事項（Servo ホストが静的リンクする HarfBuzz と SpiderMonkey 由来の third-party 部品、Servo ホストの LICENSE-CARGO-DEPENDENCIES）は別 Issue で扱う。

## 結論

本体 workspace と Servo ホストのどちらでも、解決後ライセンスの GPL / LGPL / AGPL 系は 0 件。MPL-2.0 は Servo ホストにしか無い。v8 prebuilt の差分は #866 で `NOTICE` に反映済み。

## 要判断事項

オーナーへ報告する。承認は得ていない。

1. spec の TASK-78.1 の文言（`rendering` feature の記述）を更新するか。
2. `OSS-4` のステータスを変更するか。
3. #866 の要判断事項 1〜4（`v8-prebuilt-components.md` 末尾）。
4. macOS・aarch64-musl の prebuilt 実測を後続 Issue にするか。
5. `deny.toml` の未使用 allowance（`BSD-2-Clause`）を整理するか。
6. `Cargo.toml` の古い v8 コメントを直すか。

## 再現手順

```sh
cargo deny --locked check licenses
make deny
LICENSE_INVENTORY_DIR="$(mktemp -d)" scripts/about.sh   # 出力先は worktree の外にする
```
