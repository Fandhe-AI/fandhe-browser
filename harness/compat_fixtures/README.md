# compat_fixtures

CORE-1（`core-dom.md`。代表タスクの成功率 70% 以上）を、本実装の core crate
で再測定するためのローカルフィクスチャと、それを実行するスクリプトの道具立て
（TASK-26（26.1）・Issue #136・MS-3）。

測定結果を `docs/design/core1-success-rate.md` へレポートし、70% 達成可否を
記録する作業は TASK-26.2（Issue #137）が本ディレクトリと以下のスクリプトを
使って行う。本 Issue（26.1）ではレポートを作らない。

## ディレクトリ構成

```text
harness/compat_fixtures/
├── README.md              # 本ファイル
├── static/                # 静的類型のフィクスチャ（8 件: 01〜08）
└── ssr_spa_static/        # SSR/SPA 静的レンダリング類型のフィクスチャ
                            # （5 件: 01〜05・対象外 1 件: 06）
```

各ファイルは自作の最小フィクスチャで、`docs/spec`（private submodule）の
ファイルを複製したものではない。実サイトの HTML もコミットしない
（第三者著作物の混入防止・実行時点で内容が変わり得るテキストの固定化回避）。

## 実行方法

```bash
# ローカルフィクスチャに対する測定（ネットワーク不要・決定的・CI 対象）
cargo run -p fandhe-browser-core --example compat_tasks -- local

# 実サイトに対する測定（手動実行専用。ネットワークを使う。CI からは実行しない）
cargo run -p fandhe-browser-core --example compat_tasks -- real
```

同じ実行・判定ロジックは結合テスト `crates/fandhe-browser-core/tests/compat_fixtures.rs`
からも呼ばれ、`cargo test --workspace`（3 OS CI）で常時検証される。

## 判定ルール

- **local**: 各タスクの抽出結果が、あらかじめ定めた期待値（テキスト列・
  属性値列・フォームの `(name, value)` 列）と完全一致するかで判定する
  （`tasks::judge`）。
- **real**: ページ内容は変わり得るため具体値までは固定せず、抽出結果が
  「空でないこと」だけを判定する（`Expected::NonEmpty`）。

## ステータスの意味

CLI の出力（タブ区切り: `id / category / kind / status / sample`）に現れる
`status` は次のいずれか。

| status | 意味 |
| ------ | ---- |
| `ok` | 期待どおりの結果が得られた（成功としてカウント） |
| `mismatch` | 結果が期待値と異なる |
| `expected-empty` | 結果が期待どおり空だった（対象外タスクの記録用。分母に含めない） |
| `unexpected-empty` | 非空を期待したが空だった（`real` モード用） |
| `selector-unsupported` | セレクタが本 crate の対応サブセット外（harness 側の定義ミス。CORE-1 の抽出失敗とは区別する） |
| `http-<code>` | 取得先が 2xx 以外を返した（`real` モード用。取得自体に失敗しており分母から除く） |
| `fetch-error:<種別>` | 取得自体に失敗した（`real` モード用。分母から除く。`<種別>`: `timeout` / `too-many-redirects` / `response-too-large` / `disallowed-scheme` / `disallowed-address` / `too-many-dns-resolutions` / `network` / `other`） |
| `parse-error` | HTML のパースに失敗した（`real` モード用。core crate 自体の不具合のため CORE-1 の失敗として分母に含める） |
| `query-error` | セレクタは解析できたが照合中に内部エラーが発生した（例: `MatchCacheLimitExceeded`。`selector-unsupported` とは区別する。core crate 自体の不具合のため CORE-1 の失敗として分母に含める） |

## 分母の違い（attempted / reachable）

`attempted` はタスクの試行数、`reachable` は HTML の取得に成功して比較まで
たどり着けた数（`Status::is_reachable`）。`real` モードで HTTP エラー・
fetch エラーになったタスクはそもそも比較できなかったため `attempted` には
数えるが `reachable`・成功率の分母には含めない。一方、HTML の取得自体には
成功した `parse-error`（core のパース失敗）・`query-error`（core の照合
失敗）は core crate 自体の不具合であり CORE-1 の失敗として扱う必要がある
ため `reachable` の分母に含めたうえで失敗として計上する（取得失敗と同様に
除外すると、パース・照合の失敗が分母から抜け落ち、成功率を過大評価して
しまう）。`local` モードはネットワークを使わないため `attempted == reachable`
になる。

`excluded`（`Category::Excluded`。CSR シェル `ssr_spa_static/06-csr-shell.html`）
は類型別の成功率の分母に一切含めず、出力の最後に別行で件数のみ表示する
（PoC-2 の扱いに合わせる。`<div id="root">` は空で、JS 実行なしでは中身が
描画されないことを確認する用途。JS 統合後の再評価は TASK-30・ビヘイビア
`JS-2` の範囲）。

## フォーム値の組み立てについて

`form` タスクの送信値組み立て（`tasks::collect_form_values`）は、core の
公開 API ではない、本 harness 専用の計測補助関数である。core には
`<form>` の送信値を組み立てる API がまだ無い
（`dom-api-scope.md` CORE-5 (4)・未判定）。対象は `input`（text・password・
hidden・email・search・number、または `checked` 付きの checkbox・radio）・
`textarea` に限り、`disabled` 属性付き・`name` 属性なし・
submit/button/reset/file/image は除外する。**`<select>` は対象外**
（同じく CORE-5 (4)・未判定）。

## 実サイトのタスク表を差し替える場合

サーバーの一時障害や JS 必須化で使えなくなったサイトは、同じ類型
（`static` / `ssr_spa_static`）の別サイトへ差し替え、差し替えた理由をこの
節に追記する。成功率を上げるためにセレクタを恣意的に調整しない
（ページ本来の見出し・一覧を指すセレクタに限る）。

現時点では差し替えの記録なし。

## 26.2（Issue #137）への申し送り

- `-- local` の出力（終了コード・類型別件数・成功率）と `-- real` の出力
  （参考値）を `docs/design/core1-success-rate.md` へ転記し、CORE-1 の
  受入基準（70% 以上）の達成可否を判定・記録するのは 26.2 の担当
- `real` モードの終了コードは常に 0（ネットワーク依存で非決定的なため、
  CI 判定には使わない値）。70% 達成可否は出力の `rate=`/`met`|`not-met`
  列を読んで判断する
