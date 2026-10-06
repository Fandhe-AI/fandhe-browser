# ページ軽微変化フィクスチャ

`AISNAP-10`（要素参照の破損率 10% 以下）の測定入力。TASK-17.2・Issue #111・`MS-2`。
PoC-4（`stability_check.mjs`）と同じ手法（バナー追加などの軽微変化の前後で、同じ要素を
再特定できるか）を、変化後 HTML を事前に具体化した形で再現する。

## 方針

- `benches/fixtures/` の自作合成ページを `before.html` として複製し、1 箇所だけ要素を
  挿入したものを `after.html` とする。差分は挿入要素のみ。実サイトのスナップショットではない
- 挿入文言はダミー。リンク先は `example.com` か相対パスのみで、資格情報風の値は含めない
- 対象要素は CSS セレクタで前後とも 1 件に一致する（core のセレクタは擬似クラス・部分一致属性
  未対応のため、PoC のセレクタを合成ページの実マークアップに合わせて置き換えた）
- 元マークアップに一意セレクタが無い対象は、前後の対象要素へ同じ
  `data-stability-target="NN"` 属性を付けて選ぶ（`id` / `class` / `href` など role・name の
  算出に関わる属性は変更しない）

## 構成

```text
stability/
├── cases.json            # ケース定義（配列）
└── NN-<name>/{before.html,after.html}
```

`cases.json` のキー: `id`（ディレクトリ名）・`page`（元の `benches/fixtures` ファイル名、拡張子なし）・
`selector`・`mutation`（変化の種類と位置）・`poc_case`（対応する PoC-4 ケース番号。追加ケースは `null`）。

## ケース一覧

| id | page | 対象 | 変化 | マーカー | PoC |
| -- | ---- | ---- | ---- | -------- | --- |
| 01-login-submit | login-form | 送信ボタン | body 先頭に div | - | 1 |
| 02-login-username | login-form | username 入力 | form 先頭に p | - | 2 |
| 03-dropdown-select | dropdown-form | select | body 先頭に h2 | - | 3 |
| 04-checkbox-first | checkboxes-form | 先頭 checkbox | body 先頭にスキップリンク | あり | 4 |
| 05-number-input | inputs-form | number 入力 | body 先頭に nav | - | 5 |
| 06-quote-text | quotes-list | 先頭の引用本文 | body 先頭に div | あり | 6 |
| 07-quote-tag-link | quotes-list | 先頭のタグリンク | footer 末尾に p | あり | 7 |
| 08-hn-first-title | hn-list | 1 件目のタイトルリンク | body 先頭に div | - | 8 |
| 09-hn-more-link | hn-list | More リンク | body 末尾に p | - | 9 |
| 10-ec-price | ec-product-list | 先頭の価格 | body 先頭に nav（テキストのみ） | あり | 10 |
| 11-ec-product-link | ec-product-list | 先頭の商品名リンク | body 先頭に div | - | 11 |
| 12-table-header-cell | dashboard-table | table1 の先頭ヘッダセル | body 先頭に h1 | あり | 12 |
| 13-table-data-cell | dashboard-table | table1 の先頭データセル | body 先頭に p | あり | 13 |
| 14-python-download-link | python-portal | Download リンク | body 先頭に div | - | 14 |
| 15-wiki-language-link | wiki-portal-nav | 言語リンク | body 先頭に div | - | 15 |
| 16-login-password | login-form | password 入力 | body 末尾に p | - | - |
| 17-hn-second-title | hn-list | 2 件目のタイトルリンク | body 先頭に div | - | - |
| 18-quotes-author-link | quotes-list | 先頭の author リンク | body 先頭に div | - | - |
| 19-quotes-login-link | quotes-list | Login リンク | body 先頭に div | - | - |

## PoC との差・利用側への引き継ぎ

- 15: 合成版 `wiki-portal-nav` に `nav` 要素が無く（PoC の `nav a` は 0 件）、言語リンク
  `#js-link-box-x0` で代替した
- 06・10・12・13 は本文・セル系の対象で、snapshot で ref を持つかは本 PR では確認していない。
  ref を持たない場合に備え、リンク・入力系の追加ケース 16〜19 を足している。ref の有無の扱いは
  再特定判定（#112）側で決める
- 本ディレクトリは入力資産のみ。破損率の算出・判定（#110・#112）と閾値 10% の妥当性判断（#113）は対象外
- 棚卸しは `tests/stability_fixtures.rs` が固定する
