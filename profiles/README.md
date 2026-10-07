# profiles

CSSOM 層の feature gating（`--profile chrome|safari`）が参照する、ブラウザ別の CSS 機能対応可否データ。
TASK-100.1（Issue #265）・`PLUG-8`・`MS-8` に対応する。

Rust 側の読み込み・照会は `fandhe-browser-core` の `cssom_profile` モジュール（TASK-100.2）で実装済み。公開 API（`profile_gate`）は TASK-100.3 で実装済み。gating 処理は後続（TASK-100.4）で実装する。
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
| `features` | feature ID → `{ supported, sinceVersion }`（122 件） |
| `cssProperties` | CSS プロパティ名 → feature ID（318 件。両ファイルで同一） |

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

入力は PoC-16（`PLUG-8`・TASK-100.1・`MS-8`）の生成物 `map-result.json`（`docs/spec/03-poc/browser-behavior-profile/proto/`。元スクリプトは `build_profiles.mjs`・`build_gating_table.mjs`）。
`b(CSSOM)` 層の feature 112 件と、プロパティ対応 320 件から下記 59 件を除いたものを出力した。
ビルド・テストは `docs/spec` を読まない（本ディレクトリの JSON のみを参照する）。

TASK-100.8（Issue #738）で、除外した 59 件のうち 57 件を web-features 3.39.0（Apache-2.0。tarball sha256 `0e14419d109792a2143dd5879145513076e265d1372f85af93b19c11681c14ac`）から引き直して追加した。

- 規則: `css.properties.<prop>`（ドット 2 個の完全一致キー）を `compat_features` に持つ feature をプロパティ本体の feature とする。ベンダープレフィクス付きは接頭辞を剥がした名前で引く（既存の接頭辞付きエントリと同じ流儀）
- 既存の feature・プロパティのエントリ（手動補正を含む）は変更せず、追加のみ行った。全再生成はしない
- 新規 feature 10 件の `supported`・`sinceVersion` は同バージョンの `status.support` による
- 生成スクリプト・取得物はリポジトリに置かない

## キュレーション記録

（`PLUG-8`・TASK-100.1・`MS-8`）

PoC のプロパティ → feature 対応は、`css.properties.<prop>.<サブ値>` 形式の compat key でも最初に見つけた feature をプロパティ全体の所属としている。
そのまま使うと `width`・`margin` 等が `anchor-positioning`（Chrome 非対応）扱いとなり全ページから除去されるため、次の 59 件を除外した。

| 規則 | 内容 | 件数 |
| ---- | ---- | ---- |
| R1 | サブ値・拡張の feature に誤って割り当てられたプロパティ | 41 |
| R2 | ベンダープレフィクス付きで、剥がした feature の可否を継承しているだけのもの | 6 |
| R3 | サブ機能の対応開始版がプロパティ全体へ適用されてしまうもの（`content`・`align-content`・`text-transform`・`transform-origin`・`transition`） | 5 |
| R4 | 同上（基本プロパティをサブ機能 feature へ割り当てたもの）。`overflow`・`overflow-x`・`overflow-y`（`overflow-clip`）、`outline`（`outline`）、`gap`（`flexbox-gap`）、`-webkit-transition`・`-moz-transition`（`transition-behavior`） | 7 |

- R1: `anchor-positioning` 配下のうち `position-area` 以外の 37 件（`width`・`height`・`margin*`・`top`/`left`/`right`/`bottom`・`inset*`・`min-*`/`max-*`・`*-size`・`align-items`・`justify-items`・`place-items`）、`container-type`・`break-inside`・`text-overflow`・`counter-reset`
- R2: `-webkit-`/`-moz-`/`-ms-`/`-khtml-user-select`、`-webkit-text-size-adjust`、`-ms-text-size-adjust`

維持した非対応エントリ（プロパティ名そのものが feature の本体）:

- Chrome 非対応: `hanging-punctuation`・`speak`
- Safari 非対応: `interpolate-size`・`speak`・`text-size-adjust`

出典値の手動補正（実ブラウザの対応状況に合わせ、gating で有効な宣言を除去しないため）:

- Chrome `anchor-positioning`: 非対応から対応（125）へ。`position-area` が除去されるのを防ぐ
- Safari `overscroll-behavior`（`overscroll-behavior-y` を含む）: 対応（16）
- Safari `page-break-aliases`（`page-break-*`）: 対応（1）
- Safari `user-select`: 対応（3。`-webkit-` 接頭辞付きでの対応を含む。`sinceVersion` は接頭辞付きの初出）

## 再マッピング記録

（`PLUG-8`・TASK-100.8・Issue #738・`MS-8`。上記 R1〜R4 の除外 59 件が対象）

57 件を本体 feature へ付け直し、2 件は除外を維持した。再マッピング先は Chrome / Safari 共通。

| 再マッピング先 feature | プロパティ |
| ---------------------- | ---------- |
| `logical-properties` | `block-size`・`inline-size`・`inset`・`inset-block`・`inset-block-end`・`inset-block-start`・`inset-inline`・`inset-inline-end`・`inset-inline-start`・`margin-block`・`margin-block-end`・`margin-block-start`・`margin-inline`・`margin-inline-end`・`margin-inline-start`・`max-block-size`・`max-inline-size`・`min-block-size`・`min-inline-size`（19） |
| `margin`（新規） | `margin`・`margin-top`・`margin-right`・`margin-bottom`・`margin-left` |
| `physical-properties`（新規） | `top`・`right`・`bottom`・`left` |
| `width-height`（新規） | `width`・`height` |
| `min-max-width-height`（新規） | `min-width`・`min-height`・`max-width`・`max-height` |
| `flexbox` | `align-items`・`justify-items`・`place-items`・`align-content` |
| `container-queries`（新規） | `container-type` |
| `page-breaks`（新規） | `break-inside` |
| `text-overflow`（新規） | `text-overflow` |
| `counters` | `counter-reset` |
| `content`（新規） | `content` |
| `text-transform`（新規） | `text-transform` |
| `transforms2d` | `transform-origin` |
| `transitions` | `transition`・`-webkit-transition`・`-moz-transition` |
| `overflow-shorthand`（新規） | `overflow`・`overflow-x`・`overflow-y` |
| `outline` | `outline` |
| `grid` | `gap` |
| `user-select` | `-webkit-user-select`・`-moz-user-select`・`-ms-user-select`・`-khtml-user-select` |

再マッピングしなかった 2 件（未掲載のまま素通し）:

- `-webkit-text-size-adjust`・`-ms-text-size-adjust`: 完全一致キーがなく、剥がした先の `text-size-adjust` は Safari 非対応。Safari は `-webkit-` 接頭辞形を実装しているため、写すと Safari で有効な宣言を除去してしまう

注意: `sinceVersion` は feature 単位の値で、プロパティ単体の初出版ではない（例: `outline`・`overflow-shorthand`）。
再マッピング後も非対応扱いのプロパティは変わらず（Chrome: `hanging-punctuation`・`speak` / Safari: `interpolate-size`・`speak`・`text-size-adjust`）。

## 件数・カバレッジ

- feature 122 件、プロパティ 318 件
- 対象サイト由来の CSS トークン 358 件に対するマッピング率: PoC 実測 89.4%（320 件）、キュレーション後 72.9%（261 件）、再マッピング後 88.8%（318 / 358。親 #264 の基準 85% 以上を満たす）
- 未回復は 2 件（上記）
- TASK-100.6 の再計測（再マッピング後）: バイナリ増分 4.41%（strip なし）・4.69%（strip あり）、RSS 増分 0.168 MiB・0.145 MiB。上限（10% / 1.2 MiB）以内
