# TASK-23 改善後の削減率・参照安定性の再測定レポート

- **対応ビヘイビア**: AISNAP-1・AISNAP-4・AISNAP-5・AISNAP-10・AISNAP-15（関連: AISNAP-3 保持チェック・PLUG-4・PLUG-5）
- **対応タスク**: TASK-23（23.8）
- **マイルストーン**: MS-2
- **改善前の基準コミット**: `5786055`（TASK-23.3 の直前。PLUG-5 の改善前のみ `82ab0dd` のレポート値）
- **改善後の基準コミット**: `5383534`（TASK-23.5・#852 を含む `origin/main` HEAD）
- **関連 Issue**: #830（本 Issue）・#825（親）・#826・#827・#836・#828・#829（改善）・#97・#131・#113・#387（達成可否の判断。人間担当）

本レポートは測定結果の記録であり、目標値（85%・トークン中央値 1,000 未満・破損率 10% 以下）の達成可否の判断は含まない。判断は #97・#131・#113・#387 が行う。

## 測定方法

- 改善後: 次のコマンドを `5383534` で実行した出力を転記した。
  - `cargo bench -p fandhe-browser-ai --bench token_reduction`（AISNAP-1・3・4・5）
  - `cargo bench -p fandhe-browser-ai --bench raw_dom_serialize_reduction`（AISNAP-15）
  - `cargo test -p fandhe-browser-ai --test mcp_token_reduction plug4_mcp_token_reduction_report_prints_markdown -- --nocapture`（PLUG-4）
  - `make measure-mcp-envelope MCP_ENVELOPE_ARGS="--iterations 100 --markdown"`（PLUG-5）
  - `cargo test -p fandhe-browser-ai --lib reference_stability -- --nocapture`（AISNAP-10）
- 改善前: 古いコードを再ビルドせず、`5786055` に固定されていたテスト値とレポート値を `git show` で引いた。
- 入力は `crates/fandhe-browser-ai/benches/fixtures/` と `tests/fixtures/stability/` の合成 fixture だけ。**正式値は合成 fixture の値のみ**で、PoC-4・PoC-15 の実ページの値は手法差の参考にとどまる（bench は `docs/spec` を読めない）。
- 定義の差: AISNAP-1・4・5・15 は `render_snapshot` の行形式テキストを数える。PLUG-4・PLUG-5 はホストが返す JSON 木を数える。同じ snapshot でも値が異なる（例: 対生 HTML 平均 52.1% と PLUG-4 平均 14.2%）。ページ集合も異なる（17 ページ・5 ページ）。
- トークナイザは `cl100k_base`。

## 改善前後の対照表

| 指標 | ID | 改善前（`5786055`） | 改善後（`5383534`） |
| ---- | -- | ------------------- | ------------------- |
| 対生 HTML 削減率の平均（17 ページ） | AISNAP-1 | -24.5%（最小 -146.9・最大 91.4） | 52.1%（最小 18.8・最大 96.7） |
| snapshot トークンの中央値（17 ページ） | AISNAP-4 | 4387 | 1724 |
| 対生 DOM 削減率の平均（17 ページ） | AISNAP-15 | -56.4%（最小 -147.2・最大 19.0） | 40.2%（最小 -28.8・最大 69.3） |
| wikipedia-article の対生 DOM 削減率 | AISNAP-5 | 19.0%（snapshot 45300） | 62.5%（snapshot 20954） |
| wikipedia-article の対生 HTML 削減率（参考） | AISNAP-5 | 19.8% | 62.9% |
| 情報保持 | AISNAP-3 | 7/7 | 7/7 |
| 参照破損率（ヘルパー判定・全 18 ケース） | AISNAP-10 | 33.3%（6/18） | 33.3%（6/18） |
| 参照破損率（ヘルパー判定・PoC-4 相当 14 ケース） | AISNAP-10 | 35.7%（5/14） | 35.7%（5/14） |
| 参照破損率（ref 同一性・診断値・全 18 ケース） | AISNAP-10 | 0.0%（0/18） | 0.0%（0/18） |
| MCP text 削減率の平均（5 サイト） | PLUG-4 | -75.7% | 14.2% |
| 方式 B 本体の対生 HTML 削減率の平均（5 サイト） | PLUG-5 | -73.2% | 16.6% |
| エンベロープ比（EnvelopePct）の平均（最小〜最大） | PLUG-5 | 19.5%（12.0〜32.1） | 29.5%（12.3〜60.3） |
| レイテンシ増分 全体 p50 / p95 | PLUG-5 | 1.330 / 3.867 ms | 0.940 / 2.571 ms |

- PLUG-5 のレイテンシは環境依存で、改善前は別コミット・別の実行での計測のため、参考比較にとどまる。EnvelopePct の比率が上がったのは分母（本体）が縮んだためで、エンベロープの絶対量（平均 3486.2 → 2266.2 トークン）は減っている。
- 参照破損率が変わらないのは、判定がシグネチャ（role+name）の重複で決まり、改善が重複の有無を変えなかったため。06-quote-text の同シグネチャ候補は 71 から 10、10-ec-price は 286 から 22 に減った（無名 generic の折り畳み、TASK-23.3）が、判定は Ambiguous のまま。

## 17 ページ別（改善後）

| name | rawHtmlTokens | rawDomTokens | snapshotTokens | 対生 HTML 削減率（%） | 対生 DOM 削減率（%） |
| ---- | ------------- | ------------ | -------------- | --------------------- | -------------------- |
| checkboxes-form | 160 | 108 | 62 | 61.3 | 42.6 |
| dashboard-table | 897 | 846 | 673 | 25.0 | 20.4 |
| dropdown-form | 188 | 136 | 100 | 46.8 | 26.5 |
| ec-product-list | 8055 | 7947 | 4275 | 46.9 | 46.2 |
| example-minimal | 103 | 52 | 32 | 68.9 | 38.5 |
| hn-list | 10220 | 10172 | 6514 | 36.3 | 36.0 |
| inputs-form | 171 | 120 | 49 | 71.3 | 59.2 |
| large-table | 25738 | 25706 | 7904 | 69.3 | 69.3 |
| login-form | 284 | 233 | 106 | 62.7 | 54.5 |
| mdn-docs | 19005 | 18963 | 13726 | 27.8 | 27.6 |
| python-portal | 8626 | 8554 | 5575 | 35.4 | 34.8 |
| quotes-list | 1762 | 1676 | 924 | 47.6 | 44.9 |
| reddit-list | 33111 | 33051 | 14492 | 56.2 | 56.2 |
| ssr-next-prerendered | 3650 | 2300 | 2963 | 18.8 | -28.8 |
| ssr-nuxt-hydrated-list | 3560 | 2536 | 1724 | 51.6 | 32.0 |
| wiki-portal-nav | 51272 | 4348 | 1695 | 96.7 | 61.0 |
| wikipedia-article | 56482 | 55917 | 20954 | 62.9 | 62.5 |

改善前のページ別の値は、固定されていた一部（example-minimal 118・login-form 376・wiki-portal-nav 4387 トークンなど）以外は残っていないため載せない。

## 所見

- 事実のみを記す。目標値との合否は判断しない。
- 対生 HTML 削減率の平均は -24.5% から 52.1% へ、トークン中央値は 4387 から 1724 へ変わった。全 17 ページで snapshot が生 HTML より小さい。
- 対生 DOM では ssr-next-prerendered が -28.8% で、snapshot が生 DOM より大きい。
- wikipedia-article の対生 DOM 削減率は 19.0% から 62.5% へ変わった。
- 保持チェックは 7/7 を維持した。
- 参照破損率は変化なし。ヘルパー判定 33.3%、ref 同一性 0.0%。どちらを正とするかは #113 の判断。
- 改善後の ref の表示長は 8 桁（TASK-23.5）。

## 追補: TASK-17 隣接テキスト補完後の変化（#834）

上の表・所見・集計は `5383534` の測定値のままで、本節の値は含めない。#834（TASK-17）で、ラベルのない checkbox・radio の name を隣接テキストから補うようにした後の差分だけを、ここに別記する。基準コミットは `5383534` と異なり、再測定したのは次の 1 ページのみ。他ページ・集計値（平均・中央値）は再測定していない。

| name | rawHtmlTokens | rawDomTokens | snapshotTokens | 対生 HTML 削減率（%） | 対生 DOM 削減率（%） |
| ---- | ------------- | ------------ | -------------- | --------------------- | -------------------- |
| checkboxes-form（`5383534`） | 160 | 108 | 62 | 61.3 | 42.6 |
| checkboxes-form（#834 後） | 160 | 108 | 72 | 55.0 | 33.3 |

- snapshot が 10 トークン増えたのは、name を持たなかった checkbox・radio に隣接テキストの name が付いたため。

## 追補: name の出所（placeholder）付記後の変化（#874）

上の表・所見・集計は `5383534` の測定値のままで、本節の値は含めない。#874（`AISNAP-1`）で、簡約表現に accessible name の出所を付記するようにした後の差分だけを、ここに別記する。付記するのは name が `placeholder` 由来のときだけ（測定用テキストでは quoted name の直後に ` (placeholder)`、`/ai/snapshot` の JSON では `name_source: "placeholder"`）。label・aria-label・子孫テキスト由来の出力は変えていない。

| name | rawHtmlTokens | rawDomTokens | snapshotTokens | 対生 HTML 削減率（%） | 対生 DOM 削減率（%） |
| ---- | ------------- | ------------ | -------------- | --------------------- | -------------------- |
| reddit-list（`5383534`） | 33111 | 33051 | 14492 | 56.2 | 56.2 |
| reddit-list（#874 後） | 33111 | - | 14495 | 56.2 | - |

- 影響を受けるフィクスチャは `reddit-list` の `<input placeholder="search">`（`textbox "search"` 行）のみ。出力の差分は 1 行で、`golden-refs.json` を含む他の `reduced/` 生成物は変わらない。
- snapshot が 3 トークン増えた（` (placeholder)` の分）。削減率は小数 1 桁で変わらず、平均（50.7%）の判定に影響しない。`mcp_token_reduction` の PINNED 5 サイトは placeholder 属性を持たず不変。ref のシグネチャは出所を使わないため ref も不変（`AISNAP-10`）。
- `python-portal` の検索 input は `<label for>` があるため name の出所は `Label` で、本変更では出力が変わらない（AISNAP-9 の extract-06 は本変更では回復しない。`aisnap9-comparison-report.md` の原因説明の見直しは後続課題）。

## 引き継ぎ

- #97（TASK-14 の 85% 目標）・#131（TASK-23）: 対照表の AISNAP-1・4・5・15 の値を判断の材料にする。
- #113（TASK-17）: AISNAP-10 の破損率に変化がないことを、判断の材料にする。
- #387（TASK-97）: PLUG-4・PLUG-5 の改善前後の値を材料にする。

## 制約・スコープ外

- 応答形式の見直し（`AISNAP-6`・TASK-19）、判定ヘルパーの契約変更、実ページ（PoC-4・PoC-15 相当）と Windows での再計測は扱わない。
- 個別レポート: [PLUG-4](./mcp-token-reduction-report.md)・[PLUG-5](./mcp-envelope-report.md)・[AISNAP-10](./reference-stability-report.md)。
