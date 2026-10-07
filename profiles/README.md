# profiles

CSSOM 層の feature gating（`--profile chrome|safari`）が参照する、ブラウザ別の CSS 機能対応可否データ。
TASK-100.1（Issue #265）・`PLUG-8` に対応する。

Rust 側の読み込み・照会 API・gating 処理は後続（TASK-100.2〜100.4）で実装する。
本ディレクトリはデータの配置のみを担う。

## 線引き

- 本データは「CSS 機能の有無」だけを表す。`navigator.userAgent` 等の識別面・フィンガープリントは変更しない（`SEC-1`・`SEC-2`）
- JS / Web API 層（PoC-16 の `a(API)` 76 件）は含めない（`PLUG-9` で保留中）
- `fandhe-browser-profile` crate のユーザープロファイル（Cookie・ストレージ）とは無関係

## ファイル

| ファイル | 内容 |
| -------- | ---- |
| `chrome.json` | Chrome の対応可否 |
| `safari.json` | Safari の対応可否 |

## スキーマ（両ファイル共通・`schemaVersion: 1`）

| キー | 内容 |
| ---- | ---- |
| `browser` | `chrome` または `safari` |
| `snapshotDate` | 出典データの取得日（再生成で差分が揺れないよう固定） |
| `sources` | 出典（名前・バージョン・ライセンス・URL） |
| `features` | feature ID → `{ supported, sinceVersion }`（112 件） |
| `cssProperties` | CSS プロパティ名 → feature ID（273 件。両ファイルで同一） |

- `supported`: web-features の `status.support.<browser>` が存在すれば true。部分対応も true に含まれる
- `sinceVersion`: 文字列または null。`"≤4"` のような表記を含むため数値として解釈しない
- `cssProperties` に載っていないプロパティは gating しない（素通し）。除外は安全側の操作である
- キーは昇順ソート。データ内の文字列は英語・ASCII のみ

## 出典・帰属表示

| 出典 | バージョン | ライセンス |
| ---- | ---------- | ---------- |
| web-features | 3.39.0 | Apache-2.0 |
| caniuse-lite | 1.0.30001810 | CC-BY-4.0 |

Compatibility data derived from caniuse.com (CC-BY-4.0).

`NOTICE` への正式記載は TASK-103（Issue #395）・`OSS-7` で扱う。

## 生成手順

入力は PoC-16 の生成物 `map-result.json`（`docs/spec/03-poc/browser-behavior-profile/proto/`。元スクリプトは `build_profiles.mjs`・`build_gating_table.mjs`）。
`b(CSSOM)` 層の feature 112 件と、プロパティ対応 320 件から下記 47 件を除いたものを出力した。
ビルド・テストは `docs/spec` を読まない（本ディレクトリの JSON のみを参照する）。

## キュレーション記録

PoC のプロパティ → feature 対応は、`css.properties.<prop>.<サブ値>` 形式の compat key でも最初に見つけた feature をプロパティ全体の所属としている。
そのまま使うと `width`・`margin` 等が `anchor-positioning`（Chrome 非対応）扱いとなり全ページから除去されるため、次の 47 件を除外した。

| 規則 | 内容 | 件数 |
| ---- | ---- | ---- |
| R1 | サブ値・拡張の feature に誤って割り当てられたプロパティ | 41 |
| R2 | ベンダープレフィクス付きで、剥がした feature の可否を継承しているだけのもの | 6 |

- R1: `anchor-positioning` 配下のうち `position-area` 以外の 37 件（`width`・`height`・`margin*`・`top`/`left`/`right`/`bottom`・`inset*`・`min-*`/`max-*`・`*-size`・`align-items`・`justify-items`・`place-items`）、`container-type`・`break-inside`・`text-overflow`・`counter-reset`
- R2: `-webkit-`/`-moz-`/`-ms-`/`-khtml-user-select`、`-webkit-text-size-adjust`、`-ms-text-size-adjust`

維持した非対応エントリ（プロパティ名そのものが feature の本体）:

- Chrome 非対応: `hanging-punctuation`・`position-area`・`speak`
- Safari 非対応: `interpolate-size`・`overscroll-behavior-y`・`page-break-after`・`page-break-before`・`page-break-inside`・`speak`・`text-size-adjust`・`user-select`

出典値そのもの（例: web-features 3.39.0 が Chrome の anchor-positioning を非対応とする点）は改変していない。

## 件数・カバレッジ

- feature 112 件、プロパティ 273 件
- 対象サイト由来の CSS トークン 358 件に対するマッピング率: PoC 実測 89.4%（320 件）、キュレーション後 76.3%（273 件）
- 除外 47 件を正しい feature へ再マッピングすれば回復しうる（web-features の再取得が必要）
