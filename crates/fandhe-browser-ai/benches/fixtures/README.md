# AI 最適化 API 測定用フィクスチャ

TASK-14（MS-2）の測定対象となる、14 ページ相当以上・5 類型横断のフィクスチャ集（TASK-14.1・`AISNAP-4`・Issue #91）。
後続の #93〜#96（生 HTML トークン数・削減率 `AISNAP-1`・情報保持 `AISNAP-3`・巨大静的ページ `AISNAP-5`）が入力に使う。
一覧と類型件数は `tests/fixtures_inventory.rs` が固定している。

## 方針と制約

- すべて **自作の合成フィクスチャ**であり、実サイトのスナップショットではない。PoC-4 の各ページの構造的特徴（入れ子 table・多数のナビリンク・巨大 inline script/style など）と桁感を模している。
- 第三者著作物（CC BY-SA の記事・投稿コンテンツ等）を public リポへ取り込むにはライセンス判断と `NOTICE` 帰属表示が要るため、逐語コピーはしていない（`harness/compat_fixtures/README.md` と同じ方針）。
- そのため PoC-4 の実測値（平均 87.0%・中央値 772・巨大静的 92.3%）との厳密な連続比較はできず、同等規模での参考比較になる（`REPAIR-3`）。実サイトの取り込みはライセンス判断待ち。
- 本文はダミー文で、リンク先は `example.com` か相対パス。トークン・Cookie 風の値は含めない。
- spec は「14 ページ」とするが PoC の実ファイル・測定表は 15 件で、食い違っている（spec 側の課題）。
- `tests/fixtures_inventory.rs` が UTF-8・LF・パース成功・snapshot 構築成功を検証する。

## 一覧

| ファイル | 類型 | 模した PoC ページ | 構造の特徴 | 区分 |
| -------- | ---- | ---------------- | ---------- | ---- |
| `example-minimal.html` | 静的 | example | 最小ページ（約 0.4KB） | PoC 対応 |
| `mdn-docs.html` | 静的 | mdn-docs | サイドバー nav・コード例・見出し階層（約 68KB） | PoC 対応 |
| `python-portal.html` | 静的 | python-portal | 多段 nav・ニュース / イベント一覧（約 25KB） | PoC 対応 |
| `wiki-portal-nav.html` | 静的 | wiki-portal | 言語リンク多数・検索フォーム・大きな inline script/style（約 133KB） | PoC 対応 |
| `wikipedia-article.html` | 巨大静的 | wikipedia-article | 目次・infobox 表・40 節・脚注 200 件・navbox（約 238KB） | PoC 対応 |
| `hn-list.html` | 一覧/表 | hn-list | レイアウト用入れ子 table・30 件（約 31KB） | PoC 対応 |
| `reddit-list.html` | 一覧/表 | reddit-list | div ベースの投稿 60 件・投票 UI・サイドバー（約 113KB） | PoC 対応 |
| `ec-product-list.html` | 一覧/表 | books-list | 商品カード 20 件・価格・カテゴリ nav（約 26KB） | PoC 対応 |
| `quotes-list.html` | 一覧/表 | quotes-list | 引用 10 件・タグリンク・次ページ（約 6KB） | PoC 対応 |
| `dashboard-table.html` | 一覧/表 | tables | ソート可能ヘッダ・行内操作リンクの表 2 つ（約 3KB） | PoC 対応 |
| `large-table.html` | 一覧/表 | large-table | 50 列 x 50 行 = 2,500 セル（約 50KB） | PoC 対応 |
| `login-form.html` | フォーム | login | username / password / submit | PoC 対応 |
| `inputs-form.html` | フォーム | inputs | `input[type=number]` | PoC 対応 |
| `dropdown-form.html` | フォーム | dropdown | `select#dropdown` と option | PoC 対応 |
| `checkboxes-form.html` | フォーム | checkboxes | checkbox 2 個（1 つは checked） | PoC 対応 |
| `ssr-next-prerendered.html` | SSR/SPA 静的 | なし | プリレンダ済みマークアップ + `__NEXT_DATA__` 形式 JSON | 追加 |
| `ssr-nuxt-hydrated-list.html` | SSR/SPA 静的 | なし | ハイドレーション済み一覧 + 状態埋め込み script | 追加 |

類型別の件数: 静的 4・巨大静的 1・一覧/表 6・フォーム 4・SSR/SPA 静的 2（計 17）。PoC-4 対応セットは 15 件で、追加の 2 件は SSR/SPA 静的類型を埋めるためのもの。

## 情報保持チェック用の必須要素（`AISNAP-3`・#95）

| ファイル | 要素 |
| -------- | ---- |
| `login-form.html` | `button[type=submit]`・`label` 付きの入力 2 つ |
| `ec-product-list.html` | `p.price_color` 20 個 |
| `inputs-form.html` | `input[type=number]` |
| `dashboard-table.html` | `table#table1` と `thead` 内の `th` |
| `dropdown-form.html` | `select#dropdown` と option 4 個 |
| `hn-list.html` | `.athing` 30 行の `.titleline > a` |
| `checkboxes-form.html` | `input[type=checkbox]` 2 個 |

判定の実体は `benches/token_reduction/retention_check.rs`（TASK-14.4）。`cargo bench -p fandhe-browser-ai --bench token_reduction` の `# retention check (AISNAP-3)` セクションに 7 種の判別結果を出力し、`cargo test -p fandhe-browser-ai --test token_reduction_retention_check` で具体値を固定している。

## 生成パラメータ

大きなフィクスチャは決定的な使い捨てスクリプトで生成し、生成物のみをコミットしている（スクリプトは保存していない）。
大規模表は 50 列 x 50 行、記事は 40 節 x 5 段落と脚注 200 件、reddit 風一覧は 60 件、hn 風一覧は 30 件、商品一覧は 20 件。

## 削減率の測定

`cargo bench -p fandhe-browser-ai --bench token_reduction` で生 HTML と snapshot のトークン量・削減率・平均を出力する（TASK-14.3・`AISNAP-1`・Issue #94）。
snapshot のテキスト化は測定用の暫定形式で、`/ai/snapshot` の確定応答形式ではない（TASK-19・`AISNAP-6`）。85% 目標の判定は #97 の担当。
