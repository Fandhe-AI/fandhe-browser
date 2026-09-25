# 対象サイト群カタログ（TASK-70 更新版）

**判定日**: 2026-09-25  
**判定者**: プロジェクトオーナー  
**対象**: TASK-70 / COMPAT-3 / MS-1  
**元カタログ**: `03-poc/baseline-measurement/README.md`「対象サイト群カタログ（36 サイト、類型別）」（PoC-1）

## 概要

元カタログ 36 サイトのうち、PoC-9 で試行済みの 23 サイトに加え、残り 13 サイトのアクセス可否を TASK-70 で確認し、除外判断を反映した更新版カタログである。除外は 403 ブロック（SEC-1）と robots.txt Disallow の 2 種の理由で行った。

## 確認方法

curl による UA `fandhe-browser-access-check/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)`・タイムアウト 8 秒・リダイレクト追跡（最大 10）・リトライなし・ヘッダ偽装なし（SEC 系の偽装禁止方針に準拠）で確認した。robots.txt は RFC 9309 準拠（`User-agent: *` グループ、パス最長一致、同長なら Allow 優先、`*` ワイルドカード、末尾 `$` 終端一致、404 は制限なし）で判定した。当初は Disallow 前方一致のみ・Allow 無視の簡易判定を用い docs.google.com を誤って Disallow と判定したため、上記方式で全件再判定した。

## 更新済みカタログ

36 サイト全件を元カタログの類型 (a)〜(e) ごとに掲載する。ID は元カタログの通し番号（例: a8 = (a) の 8 番目）。「確認元」は PoC-9（`03-poc/practical-compat-level/harness/results/access_check.txt`）または TASK-70（本ドキュメント）を示す。

### (a) 静的中心（ニュース・ドキュメント・ブログ）

| ID | サイト | URL | 確認元 | 状態 | 備考 |
| --- | --- | --- | --- | --- | --- |
| a1 | Wikipedia（Rust 記事） | <https://en.wikipedia.org/wiki/Rust_(programming_language)> | PoC-9 | 到達（200） | |
| a2 | MDN | <https://developer.mozilla.org/en-US/docs/Web/JavaScript> | PoC-9 | 到達（200） | |
| a3 | Rust 公式ブック | <https://doc.rust-lang.org/book/> | PoC-9 | 到達（200） | |
| a4 | Python ドキュメント | <https://docs.python.org/3/> | PoC-9 | 到達（200） | |
| a5 | BBC News | <https://www.bbc.com/news> | PoC-9 | 到達（200） | |
| a6 | Stack Overflow | <https://stackoverflow.com/questions/tagged/rust> | PoC-9 | 到達（200） | |
| a7 | GitHub 上の Markdown 表示 | <https://github.com/rust-lang/rust/blob/master/README.md> | PoC-9 | 到達（200） | |
| a8 | Medium のブログ記事 | <https://medium.com/topic/technology>（代表 URL） | TASK-70（m1） | 除外（SEC-1: 403） | |

### (b) SPA / クライアントサイドレンダリング（React/Vue 系）

| ID | サイト | URL | 確認元 | 状態 | 備考 |
| --- | --- | --- | --- | --- | --- |
| b1 | React 公式サイト | <https://react.dev> | PoC-9 | 到達（200） | |
| b2 | Vue 公式サイト | <https://vuejs.org> | PoC-9 | 到達（200） | |
| b3 | Svelte 公式サイト | <https://svelte.dev> | PoC-9 | 到達（200） | |
| b4 | Notion 公開ページ | <https://www.notion.so> | PoC-9 | 到達（200） | |
| b5 | Trello 公開ボード | <https://trello.com> | PoC-9 | 到達（200） | |
| b6 | Figma Community | <https://www.figma.com/community> | TASK-70（m2） | 除外（SEC-1: 403） | |
| b7 | Airbnb の検索結果 | <https://www.airbnb.com/s/Tokyo/homes>（代表 URL） | TASK-70（m3） | 除外（robots.txt Disallow: `/s/*/*`） | カタログ形式 `/s/Tokyo` だと紹介ページ（Allow）へ転送され検索結果 SPA にはならない |
| b8 | Twitter の公開プロフィール | <https://twitter.com/twitter>（代表 URL） | TASK-70（m4） | 除外（robots.txt Disallow: `/`） | x.com へ転送 |

### (c) 遅延読み込み・無限スクロール

| ID | サイト | URL | 確認元 | 状態 | 備考 |
| --- | --- | --- | --- | --- | --- |
| c1 | Reddit の無限スクロールフィード | <https://www.reddit.com/r/programming/> | PoC-9 | 到達（200） | PoC-9 では 200 でもブロックページ・ログイン画面の先例あり |
| c2 | Unsplash の画像検索 | <https://unsplash.com/s/photos/nature> | PoC-9 | 未到達（401） | |
| c3 | Pinterest のピン一覧 | <https://www.pinterest.com/search/pins/?q=design> | TASK-70（m5） | 除外（robots.txt Disallow: `/`） | |
| c4 | Product Hunt のプロダクト一覧 | <https://www.producthunt.com> | TASK-70（m6） | 到達（200） | |
| c5 | Instagram の公開プロフィール | <https://www.instagram.com/instagram>（代表 URL） | TASK-70（m7） | 除外（robots.txt Disallow: `/`） | |
| c6 | Hacker News | <https://news.ycombinator.com> | PoC-9 | 到達（200） | |

### (d) フォーム入力・認証フロー

| ID | サイト | URL | 確認元 | 状態 | 備考 |
| --- | --- | --- | --- | --- | --- |
| d1 | the-internet の QA テスト用ログインフォーム | <https://the-internet.herokuapp.com/login> | PoC-9 | 到達（200） | |
| d2 | Sauce Demo の QA テスト用 EC サイト | <https://www.saucedemo.com> | PoC-9 | 到達（200） | |
| d3 | demoqa の複合フォーム入力練習サイト | <https://demoqa.com/automation-practice-form> | PoC-9 | 到達（200） | |
| d4 | W3Schools の埋め込みフォーム | <https://www.w3schools.com/html/html_forms.asp> | PoC-9 | 到達（200） | |
| d5 | accounts.google.com のログイン画面 | <https://accounts.google.com> | TASK-70（m8） | 到達（200、対象自体がログイン画面） | |
| d6 | GitHub のログイン画面 | <https://github.com/login> | PoC-9 | 到達（200） | |
| d7 | Google Forms の公開サンプル | <https://docs.google.com/forms/d/e/1FAIpQLSd0iBLPh4suZoGW938EU1WIxzObQv_jXto0nT2U8HH2KsI5dg/viewform> | TASK-70（m9） | 到達（200） | 製品トップ（`docs.google.com/forms/`）は 1 回目でログイン画面へ転送。サンプルは Google Forms API 公式ガイドの JSON 例に載るフォームで、消失リスクを注記 |
| d8 | Typeform の公開サンプル | <https://form.typeform.com/to/HLjqXS5W> | TASK-70（m10） | 到達（200） | 製品トップ（`www.typeform.com`）は 1 回目で到達（200）。サンプルは Typeform 公式 GitHub の embed-demo。`tutorials.typeform.com/to/nzthWI` は移動済みで不採用 |

### (e) 動的テーブル・ダッシュボード

| ID | サイト | URL | 確認元 | 状態 | 備考 |
| --- | --- | --- | --- | --- | --- |
| e1 | GitHub の Issue 一覧テーブル | <https://github.com/rust-lang/rust/issues> | PoC-9 | 到達（200） | |
| e2 | DataTables のデモ | <https://datatables.net/examples/basic_init/zero_configuration.html> | PoC-9 | 到達（200） | |
| e3 | Google スプレッドシートの公開ビュー | <https://docs.google.com/spreadsheets/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms/htmlview> | TASK-70（m11） | 到達（200） | 製品トップ（`docs.google.com/spreadsheets/`）は 1 回目で製品紹介ページへ転送。サンプルは Google Sheets API 公式クイックスタートの「Class Data」 |
| e4 | Airtable の公開ビュー | <https://airtable.com/appf9QLP4vUH4aK8w/shrpFRNJwchpnDrGb> | TASK-70（m12） | 到達（200、描画未確認） | 製品トップ（`airtable.com`）は 1 回目で到達（200）。サンプルは robots.txt で明示許可された唯一の共有ビュー。出典はコミュニティ投稿のみで、応答 HTML に共有ビュー ID が含まれずログイン関連文字列を含むため公開ビューとして描画されたかは未確認 |
| e5 | Grafana Play の公開ダッシュボードデモ | <https://play.grafana.org> | PoC-9 | 到達（200） | |
| e6 | Kaggle のデータセット一覧 | <https://www.kaggle.com/datasets> | TASK-70（m13） | 到達（200） | |

合計 36 サイト（(a) 8・(b) 8・(c) 6・(d) 8・(e) 6）。PoC-9 の 23 サイトは robots.txt を確認しておらず、robots.txt 判定は TASK-70 の 13 サイトのみで実施済みである（判定結果は次節「TASK-70 の詳細結果」を参照）。

## TASK-70 の詳細結果

以下は TASK-70 で確認した残り 13 サイトの結果である。

### 1 回目（カタログ URL または代表 URL）

| ID | 類型 | サイト | 試行 URL | 最終ステータス | 最終 URL | 判定 | robots（再判定） |
| --- | --- | --- | --- | --- | --- | --- | --- |
| m1 | (a) | Medium のブログ記事 | <https://medium.com/topic/technology>（代表 URL。カタログはプレースホルダ） | 403 | 同左 | ブロック | Allow |
| m2 | (b) | Figma Community | <https://www.figma.com/community> | 403 | 同左 | ブロック | Allow |
| m3 | (b) | Airbnb の検索結果 | <https://www.airbnb.com/s/Tokyo/homes>（代表 URL） | 200 | 同左 | 到達 | Disallow（`Disallow: /s/*/*`） |
| m4 | (b) | Twitter の公開プロフィール | <https://twitter.com/twitter>（代表 URL） | 200 | <https://x.com/twitter> | 到達 | Disallow（`Disallow: /`） |
| m5 | (c) | Pinterest | <https://www.pinterest.com/search/pins/?q=design> | 200 | 同左 | 到達 | Disallow（`Disallow: /`） |
| m6 | (c) | Product Hunt | <https://www.producthunt.com> | 200 | 同左 | 到達 | Allow |
| m7 | (c) | Instagram の公開プロフィール | <https://www.instagram.com/instagram>（代表 URL） | 200 | 同左 | 到達 | Disallow（`Disallow: /`） |
| m8 | (d) | accounts.google.com のログイン画面 | <https://accounts.google.com> | 200 | accounts.google.com/v3/signin/identifier... | 到達（対象自体がログイン画面） | Allow |
| m9 | (d) | Google Forms の公開サンプル | <https://docs.google.com/forms/>（製品トップ） | 200 | accounts.google.com のログイン画面へ転送 | ログイン誘導 | Allow（`Allow: /forms`） |
| m10 | (d) | Typeform の公開サンプル | <https://www.typeform.com>（製品トップ） | 200 | 同左 | 到達（製品トップ） | Allow |
| m11 | (e) | Google スプレッドシートの公開ビュー | <https://docs.google.com/spreadsheets/>（製品トップ） | 200 | <https://workspace.google.com/products/sheets/> | 製品紹介ページへ転送 | Allow（`Allow: /spreadsheet`） |
| m12 | (e) | Airtable の公開ビュー | <https://airtable.com>（製品トップ） | 200 | <https://www.airtable.com/> | 到達（製品トップ） | Allow |
| m13 | (e) | Kaggle のデータセット一覧 | <https://www.kaggle.com/datasets> | 200 | 同左 | 到達 | 制限なし（robots.txt が 404） |

補足: Airbnb はカタログ形式 `/s/Tokyo` だと <https://www.airbnb.com/tokyo-japan/stays>（紹介ページ、Allow）へ転送され、検索結果 SPA にはならない。

### 公開サンプル URL での再確認（2 回目、同条件）

| ID | サンプル URL | 最終ステータス | robots | 出典・注記 |
| --- | --- | --- | --- | --- |
| m9 | <https://docs.google.com/forms/d/e/1FAIpQLSd0iBLPh4suZoGW938EU1WIxzObQv_jXto0nT2U8HH2KsI5dg/viewform> | 200（転送なし、title "Famous Black Women"） | Allow（`Allow: /forms`） | Google Forms API 公式ガイド（<https://developers.google.com/workspace/forms/api/guides>）の JSON 例 `responderUri` に載るフォーム。Google が保守するサンプルとは明示されておらず、消える可能性がある |
| m10 | <https://form.typeform.com/to/HLjqXS5W> | 200（title "embed-next demo (repo)"） | Allow（`*` グループなし） | Typeform 公式 GitHub の Typeform/embed-demo（demo-html/widget-html/index.html の data-tf-widget） |
| m11 | <https://docs.google.com/spreadsheets/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms/htmlview> | 200（title "Example Spreadsheet"） | Allow（`Allow: /spreadsheet`） | Google Sheets API 公式クイックスタートの「Class Data」（SAMPLE_SPREADSHEET_ID） |
| m12 | <https://airtable.com/appf9QLP4vUH4aK8w/shrpFRNJwchpnDrGb> | 200 | Allow（robots.txt で明示許可された唯一の共有ビュー。`/shr*`・`/app*/shr*` は Disallow） | 出典はコミュニティ投稿のみ（Omni で作成したインターフェースの例として紹介）。応答 HTML に共有ビュー ID が含まれずログイン関連文字列を含むため、公開ビューとして描画されたかは未確認 |

補足: <https://tutorials.typeform.com/to/nzthWI>（ヘルプセンター記載）は "This typeform has moved" で不採用。

## 判断事項（オーナー判断、2026-09-25）

1. 403 の m1 Medium・m2 Figma は SEC-1 に従い MVP 対象から除外する
2. robots.txt で対象パスが Disallow のサイトは除外する: m3 Airbnb の検索結果・m4 Twitter・m5 Pinterest・m7 Instagram
3. m9〜m12 は上記の公開サンプル URL を採用する（m9 は消失リスク、m12 は描画未確認を注記）
4. robots.txt を理由とする除外基準は SEC-1 にないため、spec 側（SEC 系）への追記が必要な課題として報告する（本リポでは spec を変更しない）
5. 本カタログは `docs/design/` に置く（`03-poc` は実施記録のため編集しない）

## 残課題

1. spec 側の課題として、SEC 系（SEC-1）に robots.txt 準拠を除外基準として追記する必要がある。本リポでは spec を変更しないため、spec リポ側へ報告する
2. robots.txt 基準を PoC-9 の 23 サイトへ遡及適用するかは未判断である
3. 各サイトでの代表タスク実行（COMPAT-3 の「タスク実行」部分）は未実施で、TASK-30 以降の実装を待つ
4. m9（Google Forms 公開サンプル）は Google が保守するサンプルとは明示されておらず消える可能性があり、m12（Airtable 公開ビュー）は公開ビューとして描画されたか未確認である

## 関連ビヘイビア・タスク

- **対応ビヘイビア**: COMPAT-3、SEC-1
- **参考ビヘイビア**: MEAS-2
- **対応タスク**: TASK-70（本カタログの作成）
- **マイルストーン**: MS-1
- **参照**: `03-poc/baseline-measurement/README.md`「対象サイト群カタログ（36 サイト、類型別）」（PoC-1）、`03-poc/practical-compat-level/harness/results/access_check.txt`（PoC-9）
