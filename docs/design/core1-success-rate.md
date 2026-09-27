# CORE-1 成功率測定レポート

- **対応ビヘイビア**: CORE-1（Must。`docs/spec/04-behavior/core-dom.md`）
- **対応タスク**: TASK-26（サブ 26.1・26.2）
- **マイルストーン**: MS-3
- **参考**: PoC-2（ローカル 11 件 + 実サイト 5 件 = 16/16 = 100%）
- **基準コミット**: `5043175`（`origin/main` HEAD。測定は `test/137-core1-success-rate` ブランチ上で実施）
- **測定日**: 2026-09-27
- **測定環境**: Linux 7.0.0-34-generic / rustc 1.98.1 / cargo 1.98.1

## 目的・前提

CORE-1 は「静的類型と SSR/SPA 静的類型の代表タスクについて、core crate 単体
（JS 非実行）での抽出成功率が 70% 以上であること」を求める（Must）。PoC-2 の
実測ではローカル 11 件・実サイト 5 件の合計 16/16 = 100% だった。

TASK-26.1（Issue #136・PR #458）で、この測定を本実装の core crate に対して
再現するための CLI（`compat_tasks`）とローカルフィクスチャ
（`harness/compat_fixtures/`）が整備された。本レポート（TASK-26.2・
Issue #137）は、その CLI を実際に実行し、結果と 70% 達成可否（met /
not-met）を記録する。

## 測定方法

```bash
# ローカルフィクスチャ（ネットワーク不要・決定的・CI 対象）
cargo run -p fandhe-browser-core --example compat_tasks -- local

# 実サイト（手動実行専用・ネットワークを使う・CI からは実行しない）
cargo run -p fandhe-browser-core --example compat_tasks -- real

# local と同じ判定ロジックを検証する結合テスト（3 OS CI で常時実行）
cargo test -p fandhe-browser-core --test compat_fixtures
```

`status` の意味・`attempted` / `reachable` の分母の違い・`excluded` の扱いは
`harness/compat_fixtures/README.md` を参照する（本レポートでは再掲しない）。

### 判定ルール

- **主判定**: `local` モードの類型別成功率（`success/reachable`）。static・
  ssr_spa_static のいずれも 70% 以上なら met。`local` は決定的で、結合テスト
  `compat_fixtures.rs` によって 3 OS CI でも常時回帰検証される
- **参考値**: `real` モードの類型別成功率（`reachable` を分母。実サイトは
  時点依存のため参考値扱い）
- **総合値**: `local` と `real`（`reachable` のみ）を類型ごとに合算した成功率
  （PoC-2 の「ローカル＋実サイト」集計に対応）
- **最悪値**: 取得失敗（`http-<code>`・`fetch-error:<種別>`）を失敗とみなし、
  `attempted` を分母にした `real` と総合の成功率も併記する（分母から外す
  ことで失敗を隠していないかを確認するため）
- `excluded`（X01・CSR シェル）はどの分母にも含めず、件数のみ記録する
  （JS 統合後の再評価は TASK-30・`JS-2` の範囲）

## local 結果

| id | category | kind | status | sample |
| -- | -------- | ---- | ------ | ------ |
| S01 | static | Texts | ok | Rust の非同期ランタイム入門 / 相田 藍子 |
| S02 | static | Texts | ok | キーボード / マウス / モニタ |
| S03 | static | Form | ok | fields=4 names=[username, password, csrf_token] |
| S04 | static | Attr("data-sku") | ok | sku-001 / sku-002 / sku-003 |
| S05 | static | Attr("href") | ok | / / /about / /contact |
| S06 | static | Attr("alt") | ok | ソファで眠る猫 / 公園を走る犬 |
| S07 | static | Texts | ok | こんにちは、世界 🌏 / Bonjour le monde 🥐 / Здравствуй, мир ✨ |
| S08 | static | Texts | ok | りんご / みかん（説明文が閉じタグなしでネストしている） |
| P01 | ssr_spa_static | Texts | ok | サーバーサイドレンダリングの基礎 / ハイドレーションとは何か / SPA とプリレンダリングの違い |
| P02 | ssr_spa_static | Texts | ok | 120000 / 98000 |
| P03 | ssr_spa_static | Form | ok | fields=3 names=[plan, notify_email, bio] |
| P04 | ssr_spa_static | Texts | ok | プリレンダリングされたトップページ |
| P05 | ssr_spa_static | Attr("href") | ok | /posts/1 / /posts/2 / /posts/3 |
| X01 | excluded | Texts | expected-empty | （空。CSR シェルのため対象外） |

要約行（CLI 出力そのまま）。

| 類型 | success/attempted | success/reachable | rate | 判定 |
| ---- | ------------------ | ------------------ | ---- | ---- |
| static | 8/8 | 8/8 | 100.0% | **met** |
| ssr_spa_static | 5/5 | 5/5 | 100.0% | **met** |

- `excluded`: 1 task（X01）。成功率の分母に含めない
- CLI 終了コード: `0`（static・ssr_spa_static とも met のため）

## real 結果

| id | category | kind | status |
| -- | -------- | ---- | ------ |
| R-S01 | static | Texts | ok |
| R-S02 | static | Texts | ok |
| R-S03 | static | Texts | ok |
| R-S04 | static | Texts | ok |
| R-S05 | static | Texts | ok |
| R-S06 | static | Attr("href") | ok |
| R-S07 | static | Form | ok |
| R-S08 | static | Texts | ok |
| R-P01 | ssr_spa_static | Texts | ok |
| R-P02 | ssr_spa_static | Texts | ok |
| R-P03 | ssr_spa_static | Texts | ok |
| R-P04 | ssr_spa_static | Texts | ok |
| R-P05 | ssr_spa_static | Texts | ok |

`sample` 列（第三者サイトの非信頼テキスト。内容が実行時点で変わり得る）は
転記しない。到達できなかったタスクは 0 件（全 13 件が `ok`）。

要約行（CLI 出力そのまま。参考値）。

| 類型 | success/attempted | success/reachable | rate | 判定 |
| ---- | ------------------ | ------------------ | ---- | ---- |
| static | 8/8 | 8/8 | 100.0% | met |
| ssr_spa_static | 5/5 | 5/5 | 100.0% | met |

`real` モードの終了コードは常にプロセス成功（`0`）で固定されており（非決定的
なため CI 判定に使わない値）、達成可否は上記の `rate=`/`met`|`not-met` を
読んで判断した。取得失敗（`http-<code>`・`fetch-error:<種別>`）は今回の実行
では 0 件だったため、`attempted` を分母にした最悪値は `reachable` を分母に
した値と一致する（static 8/8・ssr_spa_static 5/5、いずれも 100.0%）。

## 総合（local + real）

| 類型 | success | reachable（分母） | rate | 判定 |
| ---- | ------- | ------------------ | ---- | ---- |
| static | 16 | 16 | 100.0% | **met** |
| ssr_spa_static | 10 | 10 | 100.0% | **met** |

最悪値（`attempted` を分母。取得失敗を分母に含める）も、今回は取得失敗が
0 件のため同じ値（static 16/16・ssr_spa_static 10/10、いずれも 100.0%）。

## CI での担保

`cargo test -p fandhe-browser-core --test compat_fixtures` を実行し、
12 件のテストがすべて通過することを確認した（local の数値が回帰テストで
守られていることの証拠）。

```text
running 12 tests
...
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 判定

- **static**: local 8/8（100.0%）・real 8/8（100.0%・参考値）・総合 16/16
  （100.0%）→ **met**（目標 70% を上回る）
- **ssr_spa_static**: local 5/5（100.0%）・real 5/5（100.0%・参考値）・
  総合 10/10（100.0%）→ **met**（目標 70% を上回る）
- 主判定（local）・総合・最悪値のいずれで見ても両類型とも 70% を上回り、
  CORE-1 の受入基準（静的類型・SSR/SPA 静的類型ともに成功率 70% 以上）を
  **達成（met）** した
- not-met の類型は無かった

## 限界・留意事項

- `real` の値は時点依存の参考値であり、対象サイトの構造変更・一時的な
  到達不能によって将来の再測定で変わり得る
- フォームの送信値組み立て（`tasks::collect_form_values`）は harness 専用の
  計測補助関数であり、core の公開 API ではない。`<select>` は対象外
  （`dom-api-scope.md` CORE-5 (4)・未判定）
- 本測定は JS を実行しない（CSR シェルである X01 は `excluded` として分母外。
  JS 統合後の再評価は TASK-30・ビヘイビア `JS-2` の範囲）
- セレクタは本 crate が対応するサブセットに限られる
  （`harness/compat_fixtures/README.md` の `selector-unsupported` 参照）

## 申し送り

- 今回は実サイト 13 件すべてに到達でき、差し替え・除外の検討は発生しなかった
  （`harness/compat_fixtures/README.md` の「実サイトのタスク表を差し替える
  場合」に該当なし）
- JS 統合（TASK-30・`JS-2`）後、CSR シェル（X01）を含めた再測定を検討する
  余地がある（本 Issue の範囲外。判断はユーザーへ報告する）
