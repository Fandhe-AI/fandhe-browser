# 類型別動作率レポート（静的・SPA・フォーム類型の 70% 達成確認）

spec 対応: TASK-72（72.2）/ MS-6 / `COMPAT-1`（関連 `COMPAT-2`・`COMPAT-4`・`MEAS-4`・`REPAIR-3`・`SEC-2`・`JS-8`）。Issue #316（親 #314。前提 #315 の `aggregate_by_type.sh`、実測 #920）。

## 結論

JS エンジン統合後（V8・ES module 有効）の実測で、`COMPAT-1` の対象 3 類型はすべて閾値 70% 以上で **達成**。未達の類型はない。

| 類型 | cat | fandhe-browser-core | 動作率 | 閾値 | 判定 | Chromium 参照 |
| ---- | --- | ------------------- | ------ | ---- | ---- | ------------- |
| (a) 静的 | static | 6/7 | 85.7% | 70% | 達成 | 7/7 |
| (b) SPA | spa | 4/5 | 80.0% | 70% | 達成 | 5/5 |
| (d) フォーム | form | 5/5 | 100.0% | 70% | 達成 | 5/5 |

参考値（本書の判定対象外）:

| 類型 | cat | fandhe-browser-core | 動作率 | Chromium 参照 | 扱い |
| ---- | --- | ------------------- | ------ | ------------- | ---- |
| (c) 遅延読み込み | lazy | 1/2 | 50.0% | 2/2 | `COMPAT-1` の対象外 |
| (e) 表 | table | 1/3 | 33.3% | 1/3 | `COMPAT-2`・TASK-73 の範囲 |
| 全体 | - | 17/22 | 77.3% | 20/22 | `COMPAT-4` の判定は TASK-74（人間担当）の範囲 |

PoC-9 の推定（統合後 (b)・(d) とも 100%）との差は (b) の 1 件（b5）。

## 入力と測定条件

- 入力: `results/matrix.json`（`matrix_sha256` は `d127d9626dbf1a018a2889223459e4cb4f90ad30dafda537e55594960ea37528`。`aggregate_by_type.sh` の出力値）
- 生成元 `results/core_results.jsonl` のメタ: `measured_at=2026-10-22T16:17:38Z`・`os=Linux`・`driver=cdp`・`page_js_executed=true`・`page_js_engine=v8`・`page_modules_executed=true`・`task_timeout_sec=45`・`total_timeout_sec=1200`
- ビルド: main `bd1c8be9` の `cargo build --release -p fandhe-browser-cli`（`REPORT.md`「22 タスク再測定と回帰ゲートの有効化（#920）」）
- 成功判定の基準は README「run_core.sh」節を参照（本書では重複させない）。実行上限・タイムアウトは既定値のまま

## 失敗の原因分類

未達類型はないが、追跡のため失敗 5 件すべてを分類する（いずれも `reason=no_match`）。

| id | 類型 | 分類 | 根拠・扱い |
| -- | ---- | ---- | ---------- |
| a6 | (a) 対象 | アクセス拒否（HTTP 403。anti-bot 系） | `results/access_check.jsonl` で 403。UA 偽装等で回避しない（`SEC-2`） |
| b5 | (b) 対象 | JS 必須（別オリジンの ES module） | 別オリジンの `type=module` を取得前に `cross_origin` で拒否（`JS-8`）。許可の要否は設計 §6 案 (b) でオーナー判断待ち（#964・#918。`REPORT.md` の該当節） |
| c1 | (c) 参考 | anti-bot | 回避しない（`SEC-2`）。Chromium 生データの成功はブロックページへの一致で実質失敗（README「Chromium 参照データの出所」） |
| e1 | (e) 参考 | 未特定 | Chromium 参照も失敗。対象 DOM が取得結果に含まれるかの切り分けは未実施 |
| e5 | (e) 参考 | 未特定（非 DOM 構造の可能性） | PoC-9 は DOM 抽出が原理的に成立しない類型と特定。判断は `COMPAT-2`・TASK-73 待ち。Chromium 参照も失敗 |

`REPORT.md` が「未特定」「推測（未検証）」とするものは確定原因として書かない（`REPAIR-3`）。

## 前回測定からの変化

`REPORT.md` の 2026-10-11 測定（JS 有効・module 無効）では static 6/7・spa 4/5・form 3/5 だった。今回は form が d2・d3（ES module 有効化で成功）により 5/5 となり、static・spa は同数。数値は `REPORT.md` の既存表からの引用で、新たな測定値は作っていない。

## 制約

- 単一環境（Linux）・単一時点の 1 回の実測で、ばらつきは未評価（b1 は 2026-10-21 の通し実測で揺れた経緯がある）
- 余裕: spa は追加で 1 件失敗すると 3/5（60%）で閾値を割る。static は 5/7（71.4%）、form は 4/5（80%）で、1 件の追加失敗では達成のまま
- Chromium 参照は別時期・別判定方法（`texts` 全件一致）で、条件は揃っていない
- macOS・Windows は未測定
- `tasks.json`・セレクタ・UA・実行上限は変更しておらず、達成のための調整はしていない

## 判定の扱い

本書は TASK-72 の類型別判定。spec 上の `COMPAT-1` のステータス更新は spec リポ側（オーナー判断）、`COMPAT-4` の判定は TASK-74 の範囲で、本書は扱わない。

## 再現手順

実測の再実行は含まない。コミット済みの入力に対する検算のみ。

```bash
bash harness/compat-practical/aggregate_by_type.sh --out "$(mktemp -d)/aggregate.json"
bash harness/compat-regression/check-matrix.sh --matrix harness/compat-practical/results/matrix.json --threshold 70 --categories static,spa,form
jq -c 'select(.type=="meta")' harness/compat-practical/results/core_results.jsonl
```

再計測した場合は `run_core.sh --page-js v8 --page-modules` → `make_matrix.sh` → 上記の順で実行し、本書を更新する。
