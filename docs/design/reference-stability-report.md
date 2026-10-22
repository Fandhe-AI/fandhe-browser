# 参照破損率の測定レポート（AISNAP-10）

- **対応ビヘイビア**: AISNAP-10（Must・検討中。要素参照の破損率 10% 以下）
- **対応タスク**: TASK-17（17.3）
- **マイルストーン**: MS-2
- **参考**: PoC-4（`stability_check.mjs`。改善前 26.7% = 4/15）
- **基準コミット**: `410d00e`（TASK-17 #834 適用前の `origin/main` HEAD。当初の測定は `865879f`）
- **関連 Issue**: #110（判定ヘルパー）・#111（フィクスチャ）・#112（本 Issue・算出とレポート）・#113（達成可否の判断。人間担当）・#830（TASK-23.8 の再測定）・#833（データ葉の値テキスト）・#834（ラベルのない checkbox・radio の隣接テキスト）

本レポートの「結果」「ケース別」「所見」は、2026-10-22 時点の判定（出現順の区別子（name の末尾に付く #n）〔1436f1b0・#1221〕と、変化後に DOM・セレクタを使わない判定表〔#1228〕）で測定した現行値である。それ以前の判定（区別子なし）で得た値は、比較用に「旧判定による値」の節へ残す。判定を変えたことで数値が変わっている。

本レポートは測定結果の記録であり、10% 以下の達成可否・どの指標を正とするかの判断は含まない（#113）。

## 測定方法

- 実行: `cargo test -p fandhe-browser-ai --lib reference_stability -- --nocapture`
- 実装: `crates/fandhe-browser-ai/src/reference_stability/measure.rs`（crate 内 `#[cfg(test)]` モジュール）
- 入力: `crates/fandhe-browser-ai/tests/fixtures/stability/` の 19 ケース（合成ページ。実サイトではない）
- 判定: `check_reidentification`（TASK-17.1）。変化前の ref と role+name を取り、変化後は、変化前の対象と前後の ref 保持要素の列だけで判定する（変化後のセレクタ・DOM・位置は使わない。判定表は [reference-disambiguation.md](./reference-disambiguation.md) の「判定表」）。role・name・ref が同一で、順序に依存する同一性が崩れていなければ安定

### 集計ルール

| ヘルパーの結果 | 扱い |
| -------------- | ---- |
| `Stable` | 非破損 |
| `RefChanged`・`SignatureChanged`・`MissingAfter`・`Ambiguous`（変化後）・`OmittedFromSnapshot` | 破損 |
| `NotInSnapshot` かつ `ref_expected: false` | 分母から除外 |
| 上記以外の組み合わせ・`TargetNotFound`・`Ambiguous`（変化前） | ケース定義の誤りとしてテスト失敗 |

破損率 = 破損 / 測定対象（除外を引いた分母）。小数第 1 位へ整数演算で丸める。

## 結果（現行の判定）

実行: `cargo test -p fandhe-browser-ai --all-features --lib reference_stability::measure -- --nocapture`（2026-10-22）。

| 集計 | 破損/分母 | 破損率 |
| ---- | --------- | ------ |
| 全ケース（ヘルパー判定） | 0/18 | 0.0% |
| PoC-4 相当 15 ケース（ヘルパー判定） | 0/14 | 0.0% |
| 全ケース（ref 同一性・診断） | 0/18 | 0.0% |
| PoC-4 相当 15 ケース（ref 同一性・診断） | 0/14 | 0.0% |

ref 同一性は「変化前の ref が変化後も同じ対象を一意に指すか」だけを見る診断値で、シグネチャの一意性は要求しない。ただし「同じ対象」は変化後も同一セレクタが一意に選ぶ要素で判定するため、変化後にセレクタが一致しなくなったケースは ref が残っていても破損側に数える（セレクタ依存の診断値）。

### ケース別

| case | PoC | role | name | 判定 | 変化後の同シグネチャ候補数 | ref 同一性 |
| ---- | --- | ---- | ---- | ---- | -------------------------- | ---------- |
| 01-login-submit | 1 | button | Login | Stable | 1 | Same |
| 02-login-username | 2 | textbox | Username | Stable | 1 | Same |
| 03-dropdown-select | 3 | combobox | （空） | Stable | 1 | Same |
| 04-checkbox-first | 4 | checkbox | checkbox 1 | Stable | 1 | Same |
| 05-number-input | 5 | spinbutton | Number | Stable | 1 | Same |
| 06-quote-text | 6 | generic | （長い名前） | Stable | 1 | Same |
| 07-quote-tag-link | 7 | link | alpha | Stable | 1 | Same |
| 08-hn-first-title | 8 | link | （長い名前） | Stable | 1 | Same |
| 09-hn-more-link | 9 | link | More | Stable | 1 | Same |
| 10-ec-price | 10 | generic | £10.00 | Stable | 1 | Same |
| 11-ec-product-link | 11 | link | （長い名前） | Stable | 1 | Same |
| 12-table-header-cell | 12 | columnheader | Last Name | Stable | 1 | Same |
| 13-table-data-cell | 13 | cell | Beta | 除外（`ref_expected: false`） | - | ref なし |
| 14-python-download-link | 14 | link | Download | Stable | 1 | Same |
| 15-wiki-language-link | 15 | link | （長い名前） | Stable | 1 | Same |
| 16-login-password | - | textbox | Password | Stable | 1 | Same |
| 17-hn-second-title | - | link | （長い名前） | Stable | 1 | Same |
| 18-quotes-author-link | - | link | (about) | Stable | 1 | Same |
| 19-quotes-login-link | - | link | Login | Stable | 1 | Same |

## 旧判定による値（区別子導入前）

区別子導入前（`1436f1b0` の親。#833・#834 適用後、2026-10-11 から 2026-10-22 まで）の判定で得た値。ケース別では 07・12・18 が `Ambiguous`（変化後の同シグネチャ候補数は 07 が 2、12 が 2、18 が 10）で、ref 同一性はいずれも Same だった。出所は、`1436f1b0` が `measure.rs` の具体値テスト（`aisnap_10_measure_break_rate`）を書き換えた差分（旧値は 07・12・18 を `Ambiguous`、集計を 3/18）である。

| 集計 | 旧判定 | 現行 |
| ---- | ------ | ---- |
| 全ケース（ヘルパー判定） | 3/18 = 16.7% | 0/18 = 0.0% |
| PoC-4 相当（ヘルパー判定） | 2/14 = 14.3% | 0/14 = 0.0% |
| 全ケース（ref 同一性） | 0/18 = 0.0% | 0/18 = 0.0% |

下の「TASK-23 改善前後」「TASK-17 の出力 name 補完」の表も、それぞれの時点の判定による値で、現行値ではない。

## TASK-23 改善前後（#830）

| 集計 | 改善前（`5786055`） | 改善後（`5383534`） |
| ---- | ------------------- | ------------------- |
| 全ケース（ヘルパー判定） | 6/18 = 33.3% | 6/18 = 33.3% |
| PoC-4 相当（ヘルパー判定） | 5/14 = 35.7% | 5/14 = 35.7% |
| 全ケース（ref 同一性） | 0/18 = 0.0% | 0/18 = 0.0% |
| 06-quote-text の同シグネチャ候補数 | 71 | 10 |
| 10-ec-price の同シグネチャ候補数 | 286 | 22 |

TASK-23 の時点では破損率に変化はなかった。達成可否の判断はしない（#113）。全体の対照表は [TASK-23 再測定レポート](./token-reduction-remeasurement-report.md) を参照。

## TASK-17 の出力 name 補完（#833・#834）

出力 `Node::name` だけを補い、ref シグネチャ（accname）は変えない。ref の文字列は変わらない。

| 集計 | 補完前（`5383534`） | 補完後 |
| ---- | ------------------- | ------ |
| 全ケース（ヘルパー判定） | 6/18 = 33.3% | 3/18 = 16.7% |
| PoC-4 相当（ヘルパー判定） | 5/14 = 35.7% | 2/14 = 14.3% |
| 全ケース（ref 同一性） | 0/18 = 0.0% | 0/18 = 0.0% |

- 06・10 は #833（データ葉の値テキストを name に出力）で Stable になった。本 PR より前に `origin/main` へ入っており、本レポートが未同期だった分を併せて反映した
- 04 は #834（直後の兄弟テキスト `checkbox 1` を name に出力）で Stable になった

## PoC-4 との手法差

| 数値 | 手法 |
| ---- | ---- |
| 26.7%（4/15。PoC-4 改善前） | 変化後に対象が ref を持ち、シグネチャが一致するかで判定。一意性は求めず、ref を持たない 13 を破損に数える |
| 0.0%・0.0%（本測定・ヘルパー判定の現行値。区別子導入前は 16.7%・14.3%、補完前は 33.3%・35.7%） | role+name シグネチャの一意性と ref 文字列の一致を要求。`ref_expected: false` は分母から除外 |
| 0.0%（本測定・ref 同一性） | シグネチャの一意性を要求せず、ref が同じ対象を一意に指すかだけを見る |
| 参考 1/15 = 6.7% | ref 同一性ベースで 13 を破損に数えた場合（13 は ref を持たない） |

## 所見（判断はしない）

- 現行の判定では、18 ケースすべてが `Stable`（1 件は除外）。07・12・18 は旧判定で `Ambiguous` だったが、区別子で出力 name が一意になり、現行の判定表では行 A2（ref が出現順に依存せず、role・name も一致）に当たった。実測では、07・12・18 のほか 01・02・03・05・08・09・11・14・15・16・17・19 が A2、04・06・10 が A3（出現順に依存する ref だが、順序を決める集合が前後で同じ）だった
- 現行の判定表は、順序を決める集合が変わる変化（同名要素の前方挿入、識別属性の無い重複要素への後ろへの追加など）を `Ambiguous` にし、名前の変更を `MissingAfter` にする。これらは本フィクスチャの 19 ケースには現れないため、0/18 は上記の変化に対する耐性を示す値ではない。既知の限界は [参照再特定の設計](./reference-disambiguation.md) の「判定表」に記載した
- 06・10 は role が generic のデータ葉。無名 generic の折り畳み（TASK-23.3）で同シグネチャの候補が 71・286 件から 10・22 件に減り、その後 #833 の値テキストで出力 name が付き、区別子で候補が 1 件になった
- 04 は #834 で直後のテキストが name に付き、現行の判定で Stable（行 A3）

## #113 への引き継ぎ

- 同じ role+name の重複の解消方式（設計）は [参照再特定の設計](./reference-disambiguation.md)（#926）

- 10% 以下の達成可否、ヘルパー判定と ref 同一性のどちらを正とするか、TASK-15 / TASK-16 への差し戻し要否は未判断
- 判定ヘルパーの契約（`Ambiguous` を ref で解決するか）の変更は #110 の契約変更に当たり、本 Issue では行っていない
- 判定を変えたことで、測定値は 3/18（16.7%）から 0/18（0.0%）に変わった。どの判定・どの指標を正とするかは未判断
- 測定テストは現状の値を具体値で固定している。値が変わる改修が入るとテストが落ち、本レポートの更新が必要になる

## 制約

- spec の成果物パス `crates/fandhe-browser-ai/tests/reference_stability.rs` とは異なり、判定ヘルパーが crate 内 `#[cfg(test)]` のため `src/reference_stability/measure.rs` に置いた。spec 側のパス記述の修正要否は未判断
- フィクスチャは合成ページで、実サイトの結果ではない
