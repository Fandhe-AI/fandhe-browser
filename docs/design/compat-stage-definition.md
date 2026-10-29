# 段階的互換の段階定義と段階 0 の受け入れ確認（TASK-99 下書き）

- 状態: **下書き（オーナー判断前）**。本書の「現状」「達成と判定できる」は案であり、確定ではない。段階基準の妥当性と段階 0 の受け入れ判断は人間（オーナー）が行う（TASK-99 の担当は人間）
- 対応タスク: TASK-99 / MS-9（前提 TASK-72・TASK-74 は MS-6）
- 対応ビヘイビア: `PLUG-7`（関連 `PLUG-8`・`PLUG-9`・`COMPAT-1`・`COMPAT-4`・`RENDER-4`・`RENDER-5`・`RENDER-7`）
- 関連 Issue: #392（本タスク）・#318（TASK-74）・#314（TASK-72）・#317（TASK-73）・#57（TASK-40）
- 作成日: 2026-10-29

## 目的

除外事項「フル Chromium 互換」を、プラグインによる段階的対応方針（2026-09-23 ユーザー決定）として運用するため、互換の到達水準を段階 0〜3 の測定可能な指標として整理する（`PLUG-7`）。

「フル互換」自体は方向であって検証可能な目標ではない。本書が約束するのは、各段階に置いた指標と閾値だけである。

spec の内容は ID とパスで参照し、本文は転記しない。SSOT は `docs/spec/04-behavior/plugin-extension.md`（`PLUG-7`〜`PLUG-9`）・`compat-level.md`（`COMPAT-1`・`COMPAT-4`）・`rendering-layer.md`（`RENDER-4`・`RENDER-5`・`RENDER-7`）、タスク定義は `docs/spec/05-tasks.md` の TASK-99。spec と本書が食い違う場合は spec を正とする。

## 段階の一覧

| 段階 | 到達水準（指標・閾値） | 測り方 | 根拠 | 現状 |
| ---- | ---------------------- | ------ | ---- | ---- |
| 0 基本互換 | 全体動作率 70% 以上、かつ静的・SPA・フォーム類型の類型別動作率がそれぞれ 70% 以上 | `make check-compat-regression`（`harness/compat-regression/check-matrix.sh --threshold 70 --categories static,spa,form`）。実測は `harness/compat-practical/run_core.sh`、類型別集計は `aggregate_by_type.sh` | `PLUG-7`・`COMPAT-1`・`COMPAT-4` | 実測は基準内（案: 達成と判定できる）。判断はオーナー（下記「段階 0 の受け入れ確認」） |
| 1 CSSOM 層の feature gating | カバレッジ率 85% 以上、プロファイル読み込みのバイナリ増分 10% 以内、アイドル RSS 増分 1.2MB 以内。識別面（UA 等）は変更しない | プロファイル: `profiles/README.md`。コスト: `make measure-cssom-profile-cost`（`docs/design/cssom-profile-cost-report.md`） | `PLUG-7`・`PLUG-8`・`SEC-1`・`SEC-2` | 指標は実測で基準内。ただし cli の `--profile` 配線が未実装で、`PLUG-8` は spec 上「検討中」。達成とは書かない |
| 2 JS/Web API 層の feature gating | 未定義（保留）。`PLUG-9` の再挑戦条件を満たした後に指標化する | 条件の確認は `docs/design/plug9-retry-watch.md` のトリガーで行う | `PLUG-7`・`PLUG-9`・TASK-102 | 保留（指標なし）。条件確認の記録は 2026-10-10 時点で止まっている |
| 3 レイアウト／描画忠実度 | spec の `PLUG-7` 原文: SSIM 0.90 以上かつ境界ボックス 80% 以上一致。`RENDER-5`（2026-10-29 確定）: v1 の基準は境界ボックス（80% 以上が ±5% 以内で一致するサイトが代表 5 サイト以上）、SSIM 0.90 以上は v2 の目標 | `harness/render-screenshot/RUNBOOK.md`（`capture_screenshots.py` → `measure_ssim.py`） | `PLUG-7`・`RENDER-4`・`RENDER-5`・`RENDER-7` | v1 基準（境界ボックス）は 6 サイト中 5 サイトで合格。SSIM 0.90 以上は 1/6 サイトで v2 の目標。`PLUG-7` 原文との差は下記 (c) |

`PLUG-7` を読んだ範囲では、段階 n の達成を段階 n+1 の前提とする記述は無い（各段階の指標は独立していると読める）。

## 段階 0 の受け入れ確認

### 入力

- コミット済みの実マトリクス `harness/compat-practical/results/matrix.json`（28 サイト。2026-10-26 測定。`harness/compat-practical/type-report.md` が `matrix_sha256` と測定条件を記録）
- 測定条件（`type-report.md` から）: V8・ES module 有効・`driver=cdp`・`os=Linux`・main `7d602f6`

### 類型別動作率（`COMPAT-1`・TASK-72・#314）

出典は `type-report.md`。

| 類型 | cat | fandhe-browser-core | 動作率 | 閾値 | 判定 |
| ---- | --- | ------------------- | ------ | ---- | ---- |
| (a) 静的 | static | 6/7 | 85.7% | 70% | 閾値以上 |
| (b) SPA | spa | 4/5 | 80.0% | 70% | 閾値以上 |
| (d) フォーム | form | 7/8 | 87.5% | 70% | 閾値以上 |

参考（`COMPAT-1` の対象外）: lazy 2/3（66.7%）、table 2/5（40.0%）、全体 21/28（75.0%）。

### 全体動作率（`COMPAT-4`・TASK-74・#318）

`COMPAT-4` の spec 原文は PoC-9 と同一の 22 タスクが基準（下限 16/22）。対象サイト群はその後 28 件に拡張された（TASK-89・#68）。#318 では 22 件を判定分母、28 件を併記とした。

| 分母 | 成功 | 動作率 | 下限（70%） |
| ---- | ---- | ------ | ----------- |
| 22 件（判定。`COMPAT-4` 原文の基準） | 17/22 | 77.3% | 16/22 |
| 28 件（併記） | 21/28 | 75.0% | 20/28 |

- 28 件の値はコミット済み `matrix.json`（`fandhe_browser_core` が true の件数）から数えた。`type-report.md` の全体行（21/28・75.0%）とも一致する
- 22 件の値は次の導出による。ファイルに直接の記載はない
  - 28 件のうち後から追加した 6 件は c4・d5・d7・d8・e3・e6（`type-report.md`「前回測定からの変化」）
  - `matrix.json` 上でこの 6 件の成功は c4・d5・d7・e3 の 4 件（d8・e6 は失敗）
  - よって 22 件の成功は 21 − 4 = 17、17/22 = 77.3%
- 下限は 22 × 0.7 = 15.4 → 16 件、28 × 0.7 = 19.6 → 20 件（切り上げ）
- 失敗は 7 件: a6・b5・c1・d8・e1・e5・e6（原因分類は `type-report.md`「失敗の原因分類」）。22 件側の失敗は a6・b5・c1・e1・e5 の 5 件
- b5 は `COMPAT-4` の許容事項（anti-bot・認証）とは別の原因として記録されている（別オリジンの ES module を `JS-8` の `cross_origin` で拒否。`type-report.md`）。両者を混同しない
- 再現性（#318 の判断記録による。`REPORT.md` には 2026-10-29 の再測定の記録が無い）: 2026-10-26 の実測と、2026-10-29 の再測定 2 回（TASK-87 の Linux VM・`165eda0`・V8・ES module 有効）の計 3 回で、全タスクの成否が一致した。#318 は 2026-10-29 にオーナー判断で達成として close 済み

### 段階 0 の判定案

`PLUG-7` の期待を満たす形で書くと、次のようになる（案。確定はオーナー判断）。

- 全体: 22 件基準で 17/22（77.3%）≥ 70%、28 件でも 21/28（75.0%）≥ 70%。**満たす**
- 類型別（`COMPAT-1` の対象の static・spa・form）: いずれも 70% 以上。**満たす**
- 余裕が小さい点: spa は 1 件の追加失敗で 3/5（60%）となり閾値を割る。単一環境（Linux）・単一時点の実測で、macOS・Windows は未測定（いずれも `type-report.md`「制約」）

以上から、案としては「段階 0 は達成と判定できる」。ただし次の事項はオーナー判断が前提になる（下記「オーナー判断が要る点」の (b)・(d)）。

### spec 側のステータス

`COMPAT-4` は #318 の判断（2026-10-29）を受けて、spec リポの PR（Fandhe-AI/fandhe-browser-spec#37）で確定にする。`COMPAT-1` は spec 上で「検討中」のまま。ステータス更新は spec リポ側の作業で、本リポからは `docs/spec` を編集しない。

## 段階 1〜3 の整理

### 段階 1（CSSOM 層の feature gating）

- 指標は `PLUG-8` が定義する。実装は TASK-100（MS-8）
- カバレッジ率: 88.8%（318 / 358）。出典 `profiles/README.md`「件数・カバレッジ」。基準 85% 以上を超える。PoC-16 の実測は 89.4%（320 件）
- 読み込みコスト: バイナリ増分 +4.03%（strip なし）・+4.25%（strip あり）、アイドル RSS 増分 +200 KiB（0.195MiB）・+192 KiB（0.188MiB）。出典 `docs/design/cssom-profile-cost-report.md`。基準（10% 以内・1.2MiB 以内）の内
- 限界: 計測は Linux のみ。cli が `cssom_profile` を参照せず、`--profile` の実配線は未実装のため、出荷バイナリでの再計測が要る（同レポート「限界」）
- 識別面（UA 文字列・フィンガープリント）は変更しない（`SEC-2`。`docs/design/anti-bot-review.md`）

### 段階 2（JS/Web API 層の feature gating）

- `PLUG-9`（Could・検討中）は、数値目標を定義せず保留している。再挑戦条件は (1) 抽出手法の精度向上、(2) V8 統合（`JS-2`）後の WPT サブセット（257 件）実測（`docs/design/plug9-retry-watch.md`）
- 条件確認の最新記録は 2026-10-10（両条件とも未充足。#770・#787 が OPEN だった）。その後、V8・ES module 有効のページ内 JS 実行が実測で成立している（`type-report.md`）ため、条件 (2) の前提は動いた可能性がある。ただし本書は確認していない。`plug9-retry-watch.md` のトリガーに従って再確認が要る
- 段階 2 の指標化は条件充足後であり、本書では閾値を置かない

### 段階 3（レイアウト／描画忠実度）

- 指標は `RENDER-4`・`RENDER-5` が定義する。判断の経緯は `docs/design/rendering-fallback-plan.md`（TASK-40・#57）
- `RENDER-4`: 確定（達成）。Linux で 7 回とも 0.1〜0.8 秒で PNG を取得（#50）。Servo ホスト（`servo =0.7.0`）でも代表 6 サイトで取得
- `RENDER-5` の v1 基準（境界ボックス）: 6 サイト中 5 サイトで合格（基準は 5 サイト以上）。SSIM（参考値）が 0.90 以上なのは 1/6 サイト（b1-react-dev 0.9654。他は 0.7209〜0.8459）。出典は同書「根拠」
- `RENDER-7`: 段階導入。v1 はスクリーンショットと境界ボックスを Servo ホストの描画の範囲で提供し、見た目の一致は v2 の目標とする
- 動的ダッシュボード類型の扱い（#317・TASK-73 の判断。記録は `docs/design/dashboard-scope-decision.md`〔#1314〕）: 段階導入。v1 は Servo ホストのスクリーンショットと境界ボックスまでで、非 DOM 描画からのデータ抽出は対象外
- `PLUG-7` の注記: 段階 3 は「コア v1 では対象外」であり、`README.md` の除外事項「Servo 以外のレンダリングエンジンの本格採用」と緊張関係がある。Servo ホストは別 workspace・別プロセスに隔離されている（`RENDER-1`）

## オーナー判断が要る点

### (a) 各段階の閾値は妥当か

- 段階 0: 全体 70%・類型別 70%（`COMPAT-1`・`COMPAT-4` の原文）
- 段階 1: カバレッジ 85%・バイナリ増分 10%・RSS 増分 1.2MB（`PLUG-8` の原文）
- 段階 3: 境界ボックス 80% 以上が ±5% 以内（5 サイト以上）。SSIM 0.90 は v2 の目標

本書は spec の閾値をそのまま採用している。変更する場合は spec リポ側の作業になる。

### (b) `COMPAT-4` の分母（22 件か 28 件か）

spec 原文は 22 件基準（下限 16/22）。対象サイト群は 28 件になった。#318 では 22 件を判定分母、28 件を併記とした。本書でも同じ扱い（22 件で判定、28 件を併記）でよいか。

どちらの分母でも 70% 以上（17/22 = 77.3%、21/28 = 75.0%）で、判定は変わらない。差が出るのは、将来の再測定で失敗が増えた場合の下限（16/22 か 20/28 か）の解釈である。

### (c) 段階 3 の指標は SSIM を v2 目標にした #57 の判断に合わせてよいか

`PLUG-7` の原文は段階 3 を「SSIM 0.90 以上・境界ボックス 80% 以上一致」と読める書き方だが、`RENDER-5`（2026-10-29 確定）は SSIM を v2 の目標とし、v1 の合否には使わない。本書の段階 3 は `RENDER-5` に合わせ、次の 2 水準に分けて書く案である。

- v1 水準: 境界ボックス 80% 以上が ±5% 以内で一致するサイトが代表 5 サイト以上（現状 5/6 で合格）
- v2 水準: 画像全体の SSIM 0.90 以上（現状 1/6。主要ブラウザ同士の SSIM 測定で 0.90 の妥当性を確かめてから目標を確定する。`rendering-fallback-plan.md` 決定事項 5）

この読み替えでよいか。`PLUG-7` の文言を `RENDER-5` に合わせる更新が要る場合は、spec リポ側の課題としてオーナーが扱う（本リポからは編集しない）。

### (d) table 類型（e）の扱い

table 類型は core で 2/5（40.0%）で、70% に届かない。`COMPAT-1` の対象は静的・SPA・フォームで、table は `COMPAT-2`・TASK-73 の範囲に当たる。

- 案: 段階 0 の対象類型を `COMPAT-1` の static・spa・form に限り、table は段階 3 側（#317 の判断どおり、Servo ホスト経由のスクリーンショットと境界ボックスまで）の課題として扱う
- この場合の含意: 全体動作率（`COMPAT-4`）には table の失敗（e1・e5・e6）が含まれるため、全体では 70% 以上を保ちつつ、類型別の判定からは table を外す形になる
- 代替案: table も段階 0 の類型別判定に加える。その場合は現状 40% で未達になり、段階 0 は達成と書けない
- lazy 類型（c）は 2/3（66.7%）で `COMPAT-1` の対象外。本書では参考値の扱いとする。回帰ゲートも lazy・table を判定対象に含めていない（`.claude/rules/ci.md`）

どの類型を段階 0 の対象とするかは、基準の変更に当たるためオーナー判断が要る。

## 出典

- `docs/spec/05-tasks.md` TASK-99、`docs/spec/06-roadmap.md` MS-9
- `docs/spec/04-behavior/plugin-extension.md` `PLUG-7`〜`PLUG-9`、`compat-level.md` `COMPAT-1`・`COMPAT-4`、`rendering-layer.md` `RENDER-4`・`RENDER-5`・`RENDER-7`
- `harness/compat-practical/type-report.md`・`harness/compat-practical/results/matrix.json`
- `profiles/README.md`・`docs/design/cssom-profile-cost-report.md`
- `docs/design/plug9-retry-watch.md`・`docs/design/rendering-fallback-plan.md`・`harness/render-screenshot/RUNBOOK.md`
- Issue: #392・#318・#314・#317・#57
