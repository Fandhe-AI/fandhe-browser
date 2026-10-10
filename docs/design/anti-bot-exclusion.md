# anti-bot 除外対象サイトの明記確認記録（TASK-85 草案）

**対象**: TASK-85 / SEC-1・SEC-3 / MS-7
**状態**: 草案（確認の最終判断は人間担当。オーナー判断待ち）
**関連 Issue**: #349

## 目的

anti-bot 機構により機械的にブロックされるサイト（PoC-9: Reddit 等）と、robots.txt で対象パスが Disallow のサイト（TASK-70）が、MVP の対象用途から明示的に除外されていることを、spec の除外事項・対象サイト群カタログ・本リポ README と突き合わせて確認する。

## 除外の根拠

| 区分 | 内容 | 根拠 |
| ---- | ---- | ---- |
| anti-bot / UA・ネットワークポリシーでブロックされるサイト | 認証突破・anti-bot 回避機構は実装しないため、対象用途から除外する（PoC-9: Reddit 等は Chromium のフル JS 実行環境でも回避されない） | `security-policy.md` SEC-1・SEC-2、`04-behavior/README.md` 除外事項 |
| robots.txt で対象パスが Disallow のサイト | 実測対象の選定基準として除外する（RFC 9309 準拠で判定。PoC-9 試行済み 23 サイトには遡及適用しない。ブラウザ実行時に robots.txt を解釈する機能は定めない） | `security-policy.md` SEC-3、TASK-70 |

## 突き合わせ表

| 確認箇所 | 除外の明記 | 結果 |
| -------- | ---------- | ---- |
| spec `04-behavior/README.md` 除外事項「anti-bot 回避・フィンガープリンティング偽装」 | Reddit 等（SEC-1）と robots.txt Disallow サイト（SEC-3）を除外と明記 | 明記あり |
| `docs/design/site-catalog-task70.md`（TASK-70 更新版カタログ） | 下表の除外サイトを理由つきで「除外」と記載 | 明記あり（ただし Reddit は後述の差分あり） |
| 本リポ `README.md` | 「対象外サイト（コア v1）」項目を本 PR で追記 | 追記前は記載なし。追記後は明記あり |

## カタログ上の除外サイト（TASK-70）

| ID | サイト | 除外理由 |
| -- | ------ | -------- |
| a8 | Medium のブログ記事 | 403 ブロック（SEC-1） |
| b6 | Figma Community | 403 ブロック（SEC-1） |
| b7 | Airbnb の検索結果 | robots.txt Disallow `/s/*/*`（SEC-3） |
| b8 | Twitter の公開プロフィール | robots.txt Disallow `/`（SEC-3） |
| c3 | Pinterest のピン一覧 | robots.txt Disallow `/`（SEC-3） |
| c5 | Instagram の公開プロフィール | robots.txt Disallow `/`（SEC-3） |

## 確認結果と差分

1. spec 除外事項と SEC-1・SEC-3 の記述は、上記カタログの除外 6 サイトと整合している
2. 本リポ README の除外事項への反映は、本 PR で追加した（TASK-85 の確認観点。Issue #349 受け入れ条件 2）
3. 差分: spec 除外事項は Reddit を除外例に挙げるが、カタログ c1（Reddit の無限スクロールフィード）は「到達（200）」のまま除外扱いではない（備考に PoC-9 でブロックページ・ログイン画面の先例ありと記載）。Reddit を除外一覧に加えるか、200 到達の間は対象に残すかは未決定（オーナー判断待ち）
4. 参考: c2（Unsplash の画像検索）は 401 で未到達だが、除外ではなく未到達として記載されている。anti-bot 除外に含めるかは未決定（オーナー判断待ち）
5. 既知の課題（`site-catalog-task70.md` 残課題）: robots.txt を除外基準とする記述の SEC 系への追記は spec 側の対応事項として報告済みで、SEC-3 が確定済みであることを今回確認した

## 確認記録

| 日付 | 確認内容 | 結果 |
| ---- | -------- | ---- |
| 2026-10-10 | spec 除外事項・カタログ・README の突き合わせ（草案作成） | 差分 2 件（上記 3・4）。オーナー判断待ち |
