# CSSOM プロファイル読み込みコスト計測

Chrome / Safari プロファイルの読み込みによるバイナリ増分・アイドル RSS 増分を計測する
（TASK-100.6・Issue #270・`PLUG-8`・MS-8）。結果と考察は
[計測レポート](../../docs/design/cssom-profile-cost-report.md) を参照。

## 構成

| ファイル | 役割 |
| -------- | ---- |
| `../cssom_profile_cost.sh` | ビルド・サイズ計測・RSS 計測・集計・判定・JSON 出力 |
| `control.rs` | 基準プローブ（`cssom_profile` を参照しない） |
| `profiled.rs` | Chrome / Safari を `load_profile` で読み込み、各 1 回 `property_support` を照会 |
| `common.rs` | 両プローブ共通の最小処理（固定 HTML のパース・スタイル収集）と RSS 自己報告 |
| `self-test.sh` | 集計・判定・終了コードの自己テスト（合成サンプル。ビルドなし） |
| `results/` | 実測の生データ（strip なし・あり） |

プローブは `fandhe-browser-core` の example として登録している（計測専用で出荷物ではない）。

## 手法

- control / profiled を同じ `cargo build --release` で作り、ファイルサイズ差を取る
- 各プローブは処理後（profiled は両プロファイル読み込み完了後・テーブル保持のまま）に
  `/proc/self/status` の `VmRSS` を自己報告する。これを「アイドル RSS」とする
- 5 回（`--trials`。1〜50）を交互に実行し、中央値の差を取る
- 上限は `PLUG-8` に従い、バイナリ 10% 以内・RSS 1.2MiB（1228.8KiB）以内。
  単位は PoC-16 の換算に合わせ MiB と解釈する

## 使い方

```bash
make measure-cssom-profile-cost                      # strip なし（results/cssom-profile-cost.json）
make measure-cssom-profile-cost CSSOM_PROFILE_COST_ARGS="--strip --out benches/cssom_profile_cost/results/cssom-profile-cost-strip.json"
```

要 bash・cargo・jq。ネットワークは使わない。

## 終了コード

| コード | 意味 |
| ------ | ---- |
| 0 | バイナリ・RSS とも上限内 |
| 1 | いずれか超過（計測値は出力する） |
| 2 | 使い方・環境・入力の誤り（Linux 以外での RSS 計測を含む。偽の値は出さない） |

## 限界

- 分母は core の最小処理のみをリンクしたプローブで、出荷バイナリ `fandhe-browser`（V8 同梱）ではない。
  比率に加えて絶対バイト数を併記する
- RSS 計測は Linux のみ
- cli が `--profile` を実配線した後は、出荷バイナリでの再計測が必要
