# RENDER-5 の再計測（`navigator.webdriver` の定義あり）

TASK-137 / MS-10 / `RENDER-5`・`SEC-5`（関連: `MEAS-3`・TASK-119）。Issue #1588 の測定の記録。
計測と整理は Claude Code が行った。**合否の判定はオーナーが行う**。本書は数字と比べ方だけを示す。

TASK-137 の担当は「人間（実測）＋ Claude Code」で、`RENDER-5` の前提は Linux の実環境である。本書の測定は、
測定機（Linux x86_64）ではなく Mac 上の Docker で行った。**測定機での実測は行っていない**。本書をその代わりと
認めるかどうかも、オーナーの判断である。

## 結論（事実）

- `navigator.webdriver` を定義した版（main・`0225de7`）は、6 サイト中 5 サイトで境界ボックスの基準を満たした。
  不合格は a1-wikipedia（5/7）で、#1297 の記録（Servo 0.7.0・定義なし）と同じサイト・同じ要素
- 定義の有無は撮影結果を変えなかった。同じ CPU の種類・同じ回の `nowd`（定義なし）と `main`（定義あり）では、
  Servo の PNG が画素単位で同一だった（SHA-256 が一致）。境界ボックスの座標の差は、6 サイトの全要素で 0 だった
  （比べられた組。b1 の Servo が失敗した実行は除く）
- この結論の範囲: 保存した 6 ページには `webdriver` を読むスクリプトが無い（`grep -rI webdriver` が 0 件）。
  確かめられたのは「定義を登録しても、読み込み・配置・描画が変わらない」ことまでである。値によって表示を変えるサイトでの影響は、この測定からは分からない
- 測定は測定機（Linux x86_64）ではなく、Mac 上の Docker（Linux。arm64 と、Rosetta による amd64）で行った。
  arm64 と amd64 では PNG が画素単位で異なるサイトがあった。それでも境界ボックスの座標は全要素で同一で、
  SSIM は 1 回を除き小数 4 桁まで同じだった

## 測定の設計

### 何と何を比べたか

主の比較は「同じ日・同じ保存ページ・同じ CPU で、定義なしとありを撮る」組。

| 名前 | 版 | `navigator.webdriver`（撮影時） | 役割 |
| ---- | -- | ------------------------------- | ---- |
| `af0c611` | `af0c611`（#1297 の測定と同じ版） | 未定義 | 前回の測定の再現 |
| `nowd` | `0225de7` から定義の登録（`engine.rs` の `add_script`）だけを外した版（ローカルのみ。コミットしない） | 未定義 | 対照。定義以外の変更（TASK-119〜126 など）を揃える |
| `main` | `0225de7`（main） | `true` | 測定の対象 |

- 撮影時の値は、要素の幅を `navigator.webdriver` で変える検査ページ（ローカルの loopback のみ）を
  `servo_bbox_driver.py` で撮って確かめた。arm64・amd64 とも、`main` は `true`、`nowd`・`af0c611` は `undefined` だった。
  定義は撮影（静的モード）にも効く（`servo-host/src/engine.rs` の `Engine::new`。SEC-5）。6 サイトのページで
  直接値を読んだわけではないが、6 つの `index.html` はすべて `head` を持つ（`head` の無い XML では未定義になる制約〔SEC-5〕には当たらない）
- 各版を arm64 と amd64 でビルドし、6 つの組を 2 回ずつ撮った（`af0c611`・arm64 は、手順の確認のための 1 回を加えた 3 回）

### 手順

RUNBOOK（`harness/render-screenshot/RUNBOOK.md`）§5.1 手順 4 と同じ。`--network none`・`--read-only`・
`--cap-drop ALL`・`no-new-privileges`・`--memory 4g`・`--pids-limit 512`・`--user measure`・
`--tmpfs /home/measure:uid=1000,gid=1000,mode=0700`。`BBOX_SELECTORS` を渡し、Servo は `servo_bbox_driver.py`、
Chromium は `chromium_bbox_driver.py` で、PNG と境界ボックスを同じセッションで取った。
算出は `measure_ssim.py --min-sites 5`（`bbox_tolerance` 0.05・`bbox_rate_min` 0.80。既定値）。

### 保存ページ（取り直した）

前回（#1297）の保存ページは測定機にしかなく、取り寄せられなかった。そのため、同じ 6 サイトを取り直した。
**前回とは入力が違う**。

- 日時: 2026-11-19 08:50Z。保存したのは `harness/render-screenshot/sites.json` の 6 サイトだけで、ほかに取得したのはそのサブリソースと各ホストの `robots.txt`（下記の通信先）
- 方法: #1002 と同じ `wget -p -k -E -H`（Debian trixie の wget 1.25。robots.txt を守り、UA は既定のまま）を、
  コンテナの中で実行した。`<site_id>/index.html` は、主ページの写しに `<base href>`（主ページの位置）を足したもの。
  前回の `index.html` の作り方は記録に無く、同じかどうかは分からない
- a1-wikipedia: #55 のオーナー判断 3 に合わせ、robots.txt の `Allow: /w/load.php?` に基づき、スタイルシート
  （`only=styles` の 2 件。224,106 bytes・7,050 bytes）を 1 秒の間隔を空けて取り、`w/styles/load-{1,2}.css` に置いて参照を書き換えた。
  サムネイル 20 件は HTTP 429 で取れなかった（#1002 の記録では 19 件）。`-p` が `<video>` の参照先として
  `upload.wikimedia.org` の動画 3 本（合計約 531 MB）を取った。`preload="none"` のため撮影には効かない
- 通信先: 6 サイトとそのサブリソースのホスト（`upload.wikimedia.org`・`www.googletagmanager.com`・`analytics.python.org` など）、
  各ホストの `robots.txt`。`thumb.wikimedia.org/robots.txt` は `commons.wikimedia.org/wiki/Main_Page` へ転送され、それも取得した
- b1-react-dev: wget の 404 は `www.googletagmanager.com/robots.txt` の 1 件だけ。実行時に JS が組み立てる URL は保存されない（#1002・#1273 と同じ）
- 保存ページはコミットしない。測定した Mac の `~/fandhe-measure/r137/pages/` にある

### セレクタ

セレクタは #55 のコメントの表から作り直した。作り直したファイルの SHA-256 は、#55 の記録
（`5b156212a30da9bac67b69ba99a41cc6c3529e1ae64ac29bdad4843b69d71427`）と一致した。つまり、前回と同じファイルである。
要素数は 42 件（#55 の記録の「41 要素」は、表の件数と合わない）。a1 `lead`・a4 `contents`・b1 `hero-section` の 3 件は、
両エンジンとも見つからず（`not_found`）、対象から外れる。#1297 と同じ。

## 結果

### main（定義あり）

| site_id | SSIM（参考） | 境界ボックス（一致/対象） | 一致率 | 境界ボックスの基準 |
| ------- | ------------ | ------------------------- | ------ | ------------------ |
| a1-wikipedia | 0.7138 | 5/7 | 0.71 | 満たさない |
| a2-mdn | 0.7251 | 7/8 | 0.88 | 満たす |
| a3-rust-book | 0.8129 | 7/7 | 1.00 | 満たす |
| a4-python-docs | 0.8459 | 5/5 | 1.00 | 満たす |
| b1-react-dev | 0.9654 | 5/5 | 1.00 | 満たす |
| c6-hacker-news | 0.7850 | 7/7 | 1.00 | 満たす |

- arm64・amd64 の 2 回ずつで、成功した撮影の値はすべて同じ。
  arm64 の 2 回目は、b1-react-dev の Servo が時間切れになった（後述）
- 基準を超えてずれた要素: a1 `content`・`footer`（偏差の比 0.346）、a2 `footer`（0.0842）。a1 の 2 要素は #1297 と同じ（#55 の記録）
- `measure_ssim.py` の機械判定は `below_threshold`（b1 の Servo が成功した回は `passing_sites=1`、失敗した回は 0）。
  この判定は SSIM 0.90 以上も求める旧基準のため、境界ボックスだけの基準（`RENDER-5` の v1）では、サイトごとの
  `bbox.passed` を数える。上の表の「基準」列がそれにあたる

### 定義の有無の比較（主の比較）

- 同じ CPU の種類・同じ回の `nowd` と `main` で、Servo の PNG の SHA-256 は、比べられたすべてのサイトで一致した。
  `af0c611` と `main` も同じ（下の例外 1 件を除く）
- b1-react-dev は、arm64 の `nowd` が 2 回とも失敗したため、arm64 では `nowd` 対 `main` の組が無い。
  b1 の比較は、arm64 の `af0c611` 対 `main`（1 回目）と、amd64 の組（2 回）による
- 境界ボックスの座標（x・y・width・height）の差は、Servo・Chromium とも全要素で 0.000 px だった
- 例外は 1 件だけ: amd64 の `af0c611` の 1 回目の a3-rust-book で、Servo の PNG だけが他と異なった（SSIM 0.8128。
  境界ボックスは同一）。この差は定義の有無と関係しない（`af0c611` も `nowd` も定義なし）

### CPU の種類（arm64・amd64）の差

| 項目 | 結果 |
| ---- | ---- |
| Servo の PNG | a1・a2・a3・b1 で arm64 と amd64 が画素単位で異なる。a4・c6 は同一 |
| Chromium の PNG | a1・a2 で異なる。a3・a4・b1・c6 は同一 |
| 境界ボックスの座標 | Servo・Chromium とも全要素で同一 |
| SSIM | 小数 4 桁まで同一。例外は、PNG が他と異なった amd64 の `af0c611` の 1 回目の a3（0.8128。他は 0.8129） |

amd64 は Rosetta のエミュレーションで動かした。コンテナ内の `/proc/cpuinfo` には `avx2` が無く、測定機の
x86_64（Core i7-13700K。AVX2 あり）と同じ命令の経路ではない。Mesa の llvmpipe は CPU の機能で生成するコードを変えるため、
画素の差はこの経路の違いでも起き得る。測定機の x86_64 では測っていない。

### 前回（#1297。Servo 0.7.0・定義なし・測定機）との比較（参考）

入力（保存ページ）が違うため、数字が一致すべきものではない。サイトごとの合否と、ずれた要素で比べる。

| site_id | #1297（SSIM・一致/対象） | 今回の main（同じ） |
| ------- | ------------------------ | ------------------- |
| a1-wikipedia | 0.7209・5/7 | 0.7138・5/7 |
| a2-mdn | 0.7251・7/8 | 0.7251・7/8 |
| a3-rust-book | 0.8129・7/7 | 0.8129・7/7 |
| a4-python-docs | 0.8459・5/5 | 0.8459・5/5 |
| b1-react-dev | 0.9654・5/5 | 0.9654・5/5 |
| c6-hacker-news | 0.7888・7/7 | 0.7850・7/7 |

- 境界ボックスの基準を満たすサイトは 5 で、前回と同じ。満たさないのも同じ a1-wikipedia
- a2・a3・a4・b1 は SSIM が小数 4 桁まで一致した。a1 と c6 は記事・一覧の中身が変わっていて、SSIM がわずかに違う

### 失敗した撮影

| 組 | 内容 |
| -- | ---- |
| b1-react-dev の Servo（arm64 のみ） | arm64 は 7 回中 4 回失敗（`af0c611` 1 回・`nowd` 2 回・`main` 1 回）。3 回は `load` の 30 秒の時間切れ、1 回（`nowd` の 2 回目）は `load` が 28.1 秒で済んだ後、`screenshot` の 20 秒の時間切れ。amd64 は 6 回中 0 回。定義の有無によらず起きる。測定機でも、0.6.0 では時々時間切れになっていた（#1273・#1291。0.7.0 の測定機での頻度は記録が無い） |
| a3-rust-book の Chromium（`nowd`・amd64 の 2 回目 1 回） | `chromium_bbox_driver.py` が `viewport size does not match the requested (1280, 800) after resizing the window` で失敗。同じ組の 1 回目と、ほかの 11 回は成功 |

時間切れを再試行で通すことはしていない。上の表の値は、成功した回のもの（同じ組の成功した回は、すべて同じ値だった）。

arm64 の b1 は、成功した回も上限に近かった。Servo 側の撮影全体（駆動スクリプトの所要）は、arm64 の成功 3 回で
14.9・28.2・28.6 秒、amd64 の 6 回で 4.4〜16.7 秒だった。

## 測定環境

| 項目 | 値 |
| ---- | -- |
| 測定日 | 2026-11-19 |
| 機械 | Apple M5・32 GiB・macOS 26.6.2 |
| Docker | OrbStack 2.2.5（Docker 29.8.2・カーネル 7.2.12-orbstack）。amd64 は Rosetta |
| イメージ | `harness/render-screenshot/container/Dockerfile`。ID: `af0c611` arm64 `c785d123ecd0`・amd64 `94b0fbe4dacc`、`nowd` arm64 `e6c95b0f0ea8`・amd64 `e67d51fbbed7`、`main` arm64 `6d14c9a01e82`・amd64 `b92f2fc5a8fd` |
| Chromium | 156.0.8078.47（`DEBIAN_SNAPSHOT=20261023T000000Z`。前回と同じ） |
| GL・フォント | Mesa 25.0.7（llvmpipe）・`fonts-dejavu-core` 2.37（前回と同じ固定） |
| 隔離 | 全実行で `isolation.isolated: true` |
| viewport | 1280x800 |

成果物（`capture-result.json`・`measure-result.json`・PNG・`bboxes.json`）は Mac の `~/fandhe-measure/r137/out/` にある。リポには入れない。

## この Mac で測ることの限界

- amd64 は Rosetta で動かしたため、測定機の x86_64（AVX2）とは命令の経路が違う。境界ボックスは 3 版・2 種類の CPU で
  一致したが、測定機で同じ値になることは確かめていない
- 保存ページは取り直した。前回と同じファイルで再現したわけではない
- b1-react-dev の時間切れは、arm64 でだけ起きた。成功した回も上限に近く、arm64 では b1 を安定して撮れない。
  測定機の x86_64 で起きる頻度は、この測定からは分からない
- 同じバイナリ・同じ CPU でも、PNG が他と異なった回が 1 回あった（amd64 の `af0c611` の 1 回目の a3。Rosetta 下の非決定性と見ている。未確認）
- 傍証: 中身が変わっていない 4 サイト（a2・a3・a4・b1）の SSIM が #1297 と小数 4 桁まで一致した。保存の方法と Mac の描画の経路が、
  測定機と同等の結果を出していることを示す。ただし、測定機で確かめたわけではない
