# CSSOM プロファイル読み込みコスト計測レポート

TASK-100.6・Issue #270・`PLUG-8`・MS-8。Chrome / Safari プロファイルの読み込みによる
バイナリ増分とアイドル RSS 増分を実測した記録。

## 結論

| 受入基準 | 上限 | 実測（strip なし） | 実測（strip あり） | 判定 |
| -------- | ---- | ------------------ | ------------------ | ---- |
| バイナリ増分 | 10% 以内 | +62,528 B（+4.03%） | +57,176 B（+4.25%） | 達成 |
| アイドル RSS 増分 | 1.2MiB 以内 | +200 KiB（0.195MiB） | +192 KiB（0.188MiB） | 達成 |

PoC-16 の実測（+3.73%・+0.14MB）と同程度の水準。

## 手法

- core の example 2 本を同一リビジョン・同一 release ビルドで作る。
  control は `cssom_profile` を参照せず、profiled は `load_profile` で Chrome / Safari を読み込み
  各 1 回 `property_support` を照会する（cascade / apply は含めない）。共通処理は固定 HTML の
  パースとスタイル収集
- RSS は各プロセスが処理後に `/proc/self/status` の `VmRSS` を自己報告した値。
  5 回を control / profiled 交互に実行し、中央値の差を取る
- `1.2MB` は PoC-16 の換算に合わせ MiB（1228.8KiB）と解釈した。spec 側で単位が未明示
- 再現: `make measure-cssom-profile-cost`（詳細は `benches/cssom_profile_cost/README.md`）

## 計測環境

Linux x86_64・`rustc 1.98.1 (48a229cea 2026-09-01)`・計測時コミット `2119e371d14b`（`origin/main`）。

## 結果（実行結果をそのまま転記）

| 項目 | strip なし control | strip なし profiled | strip あり control | strip あり profiled |
| ---- | ------------------ | ------------------- | ------------------ | ------------------- |
| バイナリ (B) | 1,551,464 | 1,613,992 | 1,344,992 | 1,402,168 |
| RSS 試行 (KiB) | 3292, 3276, 3196, 3268, 3308 | 3400, 3496, 3476, 3436, 3492 | 3212, 3296, 3300, 3284, 3296 | 3488, 3392, 3496, 3404, 3496 |
| RSS 中央値 (KiB) | 3276 | 3476 | 3296 | 3488 |

生データ: `benches/cssom_profile_cost/results/cssom-profile-cost.json`・`cssom-profile-cost-strip.json`。

## 限界

- 分母は core の最小処理だけをリンクしたプローブ。出荷バイナリ（V8 同梱）は分母が大きく
  比率は小さくなる方向だが、絶対増分（約 57〜63KB）を併記している
- RSS 計測は Linux のみ（macOS・Windows は未計測）
- cli は現状 `cssom_profile` を参照しない（`--profile` の実配線は未実装）。
  実配線後に出荷バイナリで再計測する必要がある
