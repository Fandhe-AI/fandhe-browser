# 参照破損率の測定レポート（AISNAP-10）

- **対応ビヘイビア**: AISNAP-10（Must・検討中。要素参照の破損率 10% 以下）
- **対応タスク**: TASK-17（17.3）
- **マイルストーン**: MS-2
- **参考**: PoC-4（`stability_check.mjs`。改善前 26.7% = 4/15）
- **基準コミット**: `865879f`（本測定時点の `origin/main` HEAD）
- **関連 Issue**: #110（判定ヘルパー）・#111（フィクスチャ）・#112（本 Issue・算出とレポート）・#113（達成可否の判断。人間担当）

本レポートは測定結果の記録であり、10% 以下の達成可否・どの指標を正とするかの判断は含まない（#113）。

## 測定方法

- 実行: `cargo test -p fandhe-browser-ai --lib reference_stability -- --nocapture`
- 実装: `crates/fandhe-browser-ai/src/reference_stability/measure.rs`（crate 内 `#[cfg(test)]` モジュール）
- 入力: `crates/fandhe-browser-ai/tests/fixtures/stability/` の 19 ケース（合成ページ。実サイトではない）
- 判定: `check_reidentification`（TASK-17.1）。変化前の ref と role+name を取り、変化後に同じ role+name の ref 保持要素が 1 件で ref も同一なら安定

### 集計ルール

| ヘルパーの結果 | 扱い |
| -------------- | ---- |
| `Stable` | 非破損 |
| `RefChanged`・`SignatureChanged`・`MissingAfter`・`Ambiguous`（変化後）・`OmittedFromSnapshot` | 破損 |
| `NotInSnapshot` かつ `ref_expected: false` | 分母から除外 |
| 上記以外の組み合わせ・`TargetNotFound`・`Ambiguous`（変化前） | ケース定義の誤りとしてテスト失敗 |

破損率 = 破損 / 測定対象（除外を引いた分母）。小数第 1 位へ整数演算で丸める。

## 結果

| 集計 | 破損/分母 | 破損率 |
| ---- | --------- | ------ |
| 全ケース（ヘルパー判定） | 6/18 | 33.3% |
| PoC-4 相当 15 ケース（ヘルパー判定） | 5/14 | 35.7% |
| 全ケース（ref 同一性・診断） | 0/18 | 0.0% |
| PoC-4 相当 15 ケース（ref 同一性・診断） | 0/14 | 0.0% |

ref 同一性は「変化前の ref が変化後も同じ対象を一意に指すか」だけを見る診断値で、シグネチャの一意性は要求しない。ただし「同じ対象」は変化後も同一セレクタが一意に選ぶ要素で判定するため、変化後にセレクタが一致しなくなったケースは ref が残っていても破損側に数える（セレクタ依存の診断値）。

### ケース別

| case | PoC | role | name | 判定 | 変化後の同シグネチャ候補数 | ref 同一性 |
| ---- | --- | ---- | ---- | ---- | -------------------------- | ---------- |
| 01-login-submit | 1 | button | Login | Stable | 1 | Same |
| 02-login-username | 2 | textbox | Username | Stable | 1 | Same |
| 03-dropdown-select | 3 | combobox | （空） | Stable | 1 | Same |
| 04-checkbox-first | 4 | checkbox | （空） | Ambiguous | 2 | Same |
| 05-number-input | 5 | spinbutton | Number | Stable | 1 | Same |
| 06-quote-text | 6 | generic | （空） | Ambiguous | 71 | Same |
| 07-quote-tag-link | 7 | link | alpha | Ambiguous | 2 | Same |
| 08-hn-first-title | 8 | link | （長い名前） | Stable | 1 | Same |
| 09-hn-more-link | 9 | link | More | Stable | 1 | Same |
| 10-ec-price | 10 | generic | （空） | Ambiguous | 286 | Same |
| 11-ec-product-link | 11 | link | （長い名前） | Stable | 1 | Same |
| 12-table-header-cell | 12 | columnheader | Last Name | Ambiguous | 2 | Same |
| 13-table-data-cell | 13 | cell | Beta | 除外（`ref_expected: false`） | - | ref なし |
| 14-python-download-link | 14 | link | Download | Stable | 1 | Same |
| 15-wiki-language-link | 15 | link | （長い名前） | Stable | 1 | Same |
| 16-login-password | - | textbox | Password | Stable | 1 | Same |
| 17-hn-second-title | - | link | （長い名前） | Stable | 1 | Same |
| 18-quotes-author-link | - | link | (about) | Ambiguous | 10 | Same |
| 19-quotes-login-link | - | link | Login | Stable | 1 | Same |

## PoC-4 との手法差

| 数値 | 手法 |
| ---- | ---- |
| 26.7%（4/15。PoC-4 改善前） | 変化後に対象が ref を持ち、シグネチャが一致するかで判定。一意性は求めず、ref を持たない 13 を破損に数える |
| 33.3%・35.7%（本測定・ヘルパー判定） | role+name シグネチャの一意性と ref 文字列の一致を要求。`ref_expected: false` は分母から除外 |
| 0.0%（本測定・ref 同一性） | シグネチャの一意性を要求せず、ref が同じ対象を一意に指すかだけを見る |
| 参考 1/15 = 6.7% | ref 同一性ベースで 13 を破損に数えた場合（13 は ref を持たない） |

## 所見（判断はしない）

- 破損 6 件はすべて `Ambiguous`（変化後に同じ role+name の候補が複数）。いずれも ref は変化前後で同一で、対象は ref なら一意に指せている。シグネチャの重複は変化前から存在する
- 06・10 は role が generic で名前が空のデータ葉で、候補が 71・286 件に上る
- 04 は名前なしの checkbox、07・12・18 は同名リンク・見出しセルが複数あるページ

## #113 への引き継ぎ

- 10% 以下の達成可否、ヘルパー判定と ref 同一性のどちらを正とするか、TASK-15 / TASK-16 への差し戻し要否は未判断
- 判定ヘルパーの契約（`Ambiguous` を ref で解決するか）の変更は #110 の契約変更に当たり、本 Issue では行っていない
- 測定テストは現状の値を具体値で固定している。値が変わる改修が入るとテストが落ち、本レポートの更新が必要になる

## 制約

- spec の成果物パス `crates/fandhe-browser-ai/tests/reference_stability.rs` とは異なり、判定ヘルパーが crate 内 `#[cfg(test)]` のため `src/reference_stability/measure.rs` に置いた。spec 側のパス記述の修正要否は未判断
- フィクスチャは合成ページで、実サイトの結果ではない
