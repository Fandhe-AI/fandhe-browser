# トークン削減率の実ページ測定手順

- **対応ビヘイビア**: AISNAP-1・AISNAP-4・AISNAP-15
- **対応タスク**: TASK-14・TASK-23
- **マイルストーン**: MS-2
- **関連 Issue**: #927（本手順の実装）・#97・#131（達成可否の判断。人間担当）

## 目的と位置づけ

合成 fixture 17 ページの削減率は 85% 目標に未達で、最終判定は実ページ測定で行う（オーナー判断 2026-10-12・#97）。
本書は、人間がローカルに保存した実ページ HTML を、既存の 2 ベンチ（`token_reduction`・`raw_dom_serialize_reduction`）で測るための手順を定める。
合成 fixture は回帰指標として残す。**本書と実装は測定の口を用意するだけで、達成可否の判断は #97・#131 が行う。**

## 実ページ HTML の扱い

- 実ページ HTML は **リポジトリにコミットしない**。著作権・ライセンスの判断が要るため、リポジトリ外か `.gitignore` 済みのパスに置く。
- 取得時にログイン済みセッションの Cookie・個人情報・トークンを含めない。公開ページを未ログインで取得する。
- 結果に貼る TSV にはページ名（匿名化可）だけを載せ、HTML 本体は載せない。

## HTML の用意

1. ブラウザの「ページを保存」または `curl` で、単一の HTML ファイルを取得する。
2. UTF-8 の `*.html`（拡張子は小文字の `.html`）として、1 つのディレクトリの直下に置く。サブディレクトリは走査しない。
3. 数 MB を超えるページは BPE の計数に時間がかかる。

## 実行コマンド

`FANDHE_BROWSER_BENCH_HTML_DIR` に入力ディレクトリを渡す。未指定のときは従来どおり同梱の合成 fixture を測る。相対パスは `cargo bench` の cwd（`fandhe-browser-ai` の package ルート `crates/fandhe-browser-ai/`）基準で絶対化するため、シェルの cwd からの相対指定は意図と異なるディレクトリを指す。絶対パスを渡すこと。

```bash
FANDHE_BROWSER_BENCH_HTML_DIR=/path/to/pages cargo bench -p fandhe-browser-ai --bench token_reduction
FANDHE_BROWSER_BENCH_HTML_DIR=/path/to/pages cargo bench -p fandhe-browser-ai --bench raw_dom_serialize_reduction
```

## 出力の読み方

- `input` 行は入力の種別（`fixtures` / `external`）と、絶対化後のディレクトリを示す。
- ページ別の TSV に続き、`pages`・`meanReductionPct`・`medianReductionPct`・`minReductionPct`・`maxReductionPct` を出す。`token_reduction` は `medianSnapshotTokens`（AISNAP-4 の参考値）も出す。
- `met` は 85% に対する参考表示で、終了コードには反映しない。
- 外部ディレクトリ指定時、同梱 fixture の名前に依存する `AISNAP-3`（情報保持チェック）と `AISNAP-5`（巨大静的ページ）は実行せず、`skipped` 行を出す。

## エラー終了する条件

次の場合は `error:` を標準エラーへ出して非 0 で終了する（0 件を成功にしない）。

- 環境変数が空文字
- 指定パスが存在しない、またはディレクトリではない
- ディレクトリ直下に `*.html` が 1 件も無い
- 生 HTML・生 DOM のトークン数が 0 のページがある

## 結果の記録様式

結果は [再測定レポート](token-reduction-remeasurement-report.md) と同じ構成で記録する。

| 項目 | 記載内容 |
| ---- | -------- |
| 実施日 | 測定した日付 |
| コミット SHA | 測定に使った `origin/main` のコミット |
| ページ数 | `pages` の値 |
| ページ名 | 匿名化可 |
| TSV 抜粋 | ページ別の行（HTML 本体は載せない） |
| 平均・中央値 | `meanReductionPct`・`medianReductionPct` |
| 取得元 | 取得元とライセンス上の扱い |

達成可否の判定は #97・#131 が行う。この文書や PR では判定しない。
