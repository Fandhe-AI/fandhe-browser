# multi-instance-memory

fandhe-browser と Chromium ヘッドレスを N 個（既定 50）同時起動し、全プロセスの PSS を合算して
集約メモリを比較する計測ハーネス。TASK-81（Issue #319）、ビヘイビア `PERF-4`・`MEAS-6`、MS-6。
ID から SSOT（`docs/spec` の `04-behavior/`）を参照すること
（[spec-reference](../../.claude/rules/spec-reference.md)）。

**測定の実行と判定はオーナーが行う**（TASK-81 の担当は人間）。本ディレクトリはその準備（計測スクリプト）
までで、出力の `reduction_pct`・`reference_threshold_met` は参考値であり合否の確定ではない。

## 対象と判定基準

- 対象: fandhe-browser N 個 と Chromium ヘッドレス N 個の集約メモリ（`PERF-4`）
- 判定基準: 集約メモリが Chromium 比で **50% 以上削減**（`PERF-4`）
- 計測方法: `PERF-4`・`PERF-6` の計測方法に関する注記（2026-09-28）により、RSS の単純合算ではなく
  **PSS**（共有ページを按分する）で測る。JS 評価の子プロセス分離（`js-engine.md`、#503）でプロセスツリーに
  親子が並存しうるため。本スクリプトは `/proc/<pid>/smaps_rollup` の `Pss` を、起動した各インスタンスの
  プロセスツリー（子孫と同一プロセスグループ）全体で合算する。`rss_kib` は参考として併記する
- 2 条件（1 インスタンスの定義）:
  - `idle`: 起動して `--settle` 秒待ったアイドル状態（fandhe は JS 子プロセスが遅延起動のため未使用）
  - `loaded`: 全インスタンスで同一の**公開 URL**（`--url`。http(s) のみ）を読み込み、`--settle` 秒待った状態。
    両者とも CDP の `Page.navigate`（`navigate.mjs`）で遷移させる。Chromium は `about:blank` で起動し
    （idle・loaded で起動条件を揃える）、`--remote-debugging-port`（インスタンス i は 9400+i、127.0.0.1 限定）の
    page ターゲットへ `--page-target` モードで接続して遷移する。完了は `Page.navigate` 応答の `loaderId` と
    一致する `Page.lifecycleEvent`（`name=="load"`）で判定し、遅れて届く `about:blank` の load では解決しない。
    起動前に 9401..9400+N の使用中を拒否し（使用中は終了コード 2）、起動後は `/json/version` の応答元が
    今回起動したプロセスツリー所有の LISTEN ソケットであること（`/proc/<pid>/net/tcp` の inode と fd の照合）を
    確認する。fandhe 側の固定ポート 9333 も同じ所有確認を行う。
    `Page.navigate` が `result.errorText` を返した場合（DNS・TLS・接続失敗、fandhe では SSRF ガードによる拒否も）は
    `navigate.mjs` が非 0 で終了し、計測は失敗（終了コード 1）になる。エラーページの PSS を loaded として出さない

## 前提

- 数値引数（`-n`・`--settle`・`--poc1-*-kib`・`--dns` の各オクテット）は先頭ゼロなしの十進表記のみ受理する
  （`050`・`08` 等は bash 算術と `printf %d` で八進数扱いになるため使用エラー＝終了コード 2）。
  `--chromium-extra-args` に `--remote-debugging-*`・`--user-data-dir` は指定できない（検査した接続先とずれるため）

- **Linux 専用**（`/proc/<pid>/smaps_rollup`）。macOS・Windows では終了コード 2 で終わる
- bash・ps・awk・curl に加え、`loaded` 条件は node 22 以降（fandhe・Chromium 双方の遷移確認に使う）（npm 依存なし）
- fandhe-browser はリリースビルドを推奨（`cargo build --release -p fandhe-browser-cli`）。
  JS エンジン構成を変えた場合は構成を記録に残す
- Chromium/Chrome の実行ファイル（`--chromium-bin`）。root で実行する場合やコンテナ内では
  `--chromium-extra-args "--no-sandbox"` が必要になることがある
- 同一バイナリ・同一マシンで Chromium と同一セッションに測る（`MEAS-6` の前提は Linux 実機。
  リソース不足でスワップが起きると PSS 以外の指標に影響するため、空きメモリを事前に確認する）

## ネットワーク分離（重要な制約）

`fandhe-browser` は待ち受けアドレスが `127.0.0.1:9333` 固定で、CLI 引数・環境変数による変更手段が
ない（`crates/fandhe-browser-cli/src/server.rs` の `DEFAULT_ADDR`。サブコマンドは TASK-47・`CLI-1`
で追加予定）。このため N>=2 のままでは 2 個目以降がポート衝突で起動できない。本ハーネスは各インスタンスを
別 network namespace に入れて衝突を避ける。

| `--net-mode` | 動作 |
| ------------ | ---- |
| `auto`（既定） | N=1 は `host`、N>=2 は `netns` |
| `netns` | `unshare -n`（root）または `unshare -Urn`（非特権 user namespace）で分離し、namespace 内で `lo` を上げて起動。準備確認は `nsenter` 経由 |
| `host` | N=1 のみ。起動前に 9333 の使用中を検査し、使用中なら終了コード 2 |

**loaded 条件の外向き経路**: 本番の `Page.navigate` は `FetchOptions::default()` で loopback・
プライベートアドレス・`file:` を SSRF ガードにより拒否する（`net::ERR_...` を `errorText` で返す）。
また netns は `lo` のみで外部へ出られない。このため loaded 条件（fandhe 側）の netns モードでは、
インスタンス i ごとに veth ペア（ホスト側 `fmh<i>`・netns 側 `fmp<i>`）を作り、`10.213.<i>.0/30`
（スクリプト冒頭の `EGRESS_BASE`。既存ネットワークと衝突する場合は書き換える）でホストと接続し、
ホスト側で NAT する。

- 追加物: `net.ipv4.ip_forward=1`（元が 0 なら終了時に戻す）・`iptables` の `nat POSTROUTING MASQUERADE` と
  `filter FORWARD` の ACCEPT 2 本（コメント `fandhe-mim` 付き）・veth・netns 内の `resolv.conf` 差し替え
  （mount namespace 内の bind mount。ホストのスタブリゾルバは netns から届かないため `--dns`、
  既定 `1.1.1.1` を使う）
- 後始末: `trap` が追加したルールを逆順に `-D` で削除し、veth を削除し、`ip_forward` を元に戻す。
  異常終了後は `iptables -S | grep fandhe-mim`・`ip link show type veth` で残留を確認する
- root 必須（非特権 user namespace では veth・iptables を扱えない）。idle のみの実行は従来どおり非特権でも可
- Chromium 側はホストのネットワークをそのまま使い、同じ公開 URL を取得する（両者で URL を共通にする）

`netns` の要件は root か、非特権 user namespace が許可された環境（Ubuntu 24.04 以降は既定で制限される
ことがあるため `sudo` で実行するか sysctl を確認する）。いずれも満たせない場合は終了コード 2 で止まる。
各インスタンスのプロファイルは `XDG_DATA_HOME` を個別の一時ディレクトリにして分離する。

## 実行方法

```bash
# 1. 準備: loaded 条件の対象として、固定内容の公開 URL を 1 つ決める
#    （Chromium・fandhe の全インスタンスが同じ URL を取得する。内容・サイズ・取得日時を記録に残す。
#      loopback・プライベートアドレス・file: は SSRF ガードで拒否されるため使えない）

# 2. 計測（loaded は veth/NAT のため root。sudo で実行。出力は JSON）
sudo harness/multi-instance-memory/measure.sh -n 50 \
  --fandhe-bin target/release/fandhe-browser \
  --chromium-bin /usr/bin/chromium --chromium-extra-args "--no-sandbox" \
  --url <公開 URL（例: https://example.com/）> \
  --poc1-chromium-kib <PoC-1 の Chromium 1 インスタンスあたり PSS (KiB)> \
  --poc1-fandhe-kib <PoC-1 の fandhe 1 インスタンスあたり PSS (KiB)> \
  --out result-50.json
```

- インスタンス数の上限は 1〜200（範囲外は終了コード 2）。まず `-n 2` や `-n 10` で通し確認してから `-n 50`
- 終了コード: 0 成功 / 1 計測失敗（起動後のプロセス消失・生存プロセスの smaps 読み取り不可・準備確認の
  タイムアウト）/ 2 使用エラー・前提不足
- fail-closed: 起動した N 個が計測直前に 1 つでも欠けていれば失敗にする（少ない数で測って削減率を過大に
  見せないため）
- 後始末: `trap` により正常終了・失敗・Ctrl-C のいずれでも、起動した全プロセスグループへ TERM/KILL を送り、
  一時ディレクトリを削除する。実行前後に `pgrep -af 'fandhe-browser|chrom'` で残留がないことを確認する

## 出力 JSON

| キー | 内容 |
| ---- | ---- |
| `results[]` | `target`（fandhe/chromium）・`condition`（idle/loaded）・`processes`（合算対象数）・`pss_kib`・`rss_kib`・`pss_per_instance_kib` |
| `comparisons[]` | 条件ごとの `reduction_pct` = (1 - fandhe_pss / chromium_pss) x 100、`reference_threshold_met`（50% 以上か。参考値） |
| `comparisons[].poc1_extrapolation` | `--poc1-*-kib` 指定時のみ。PoC-1 の 1 インスタンスあたり値 x N の線形外挿値と、実測の乖離率（`*_delta_pct`。正は実測が外挿より大きい） |
| `environment` | カーネル・アーキテクチャ・CPU 数・総メモリ（ホスト名・IP は含めない） |

## PoC-1 線形外挿との比較方法

1. PoC-1（`docs/spec/03-poc`）の 1/5/10 インスタンス実測から、**PSS** の 1 インスタンスあたり値を
   Chromium・fandhe それぞれ取り出す。比較入力は PSS 専用で、RSS 値は渡さない（PoC-1 に PSS が無い
   場合は PoC-1 側を PSS で再計測するか、外挿比較を省いて報告に理由を書く）
2. `--poc1-chromium-kib`・`--poc1-fandhe-kib` を**両方**渡すと、`poc1_extrapolation` に N 倍の外挿値と乖離率が出る。
   基準値は正の有限値のみ受理し、0・負・非数は計測開始前に終了コード 2 で拒否する
3. 外挿より実測が小さい（負の乖離）場合は共有ページ按分の効果、大きい場合はインスタンス間の干渉
   （CPU・キャッシュ・メモリ圧迫）を疑う。評価はレポートに記述する

## 自己テスト

```bash
FANDHE_BIN=target/debug/fandhe-browser harness/multi-instance-memory/self-test.sh
```

小さな N（2）で偽バイナリを起動し、プロセスツリー走査・PSS 合算・JSON 出力・残留なしを確認する。
あわせて PoC-1 基準値・`--url`（loopback/private/`file:` 拒否）の入力検証、URL の JSON エスケープの
往復、`navigate.mjs` の `errorText` 非 0 終了を確認する。veth/NAT（root）は自己テストの対象外で、
実機でのオーナー実行時に確認する。
実バイナリ（N=1）と、可能なら netns の N=2 も確認する。CI には組み込まない。
終了コード: 0 全検証済み / 1 失敗 / 3 Chromium 不在（Chromium 経路のみ未検証・他は合格。skip で 0 にしない）/ 4 Linux 以外。

## レポートテンプレート

```markdown
# TASK-81 50 インスタンス集約メモリ実測（PERF-4・MEAS-6）

- 実施日 / 実施者:
- 環境: カーネル / CPU 数 / 総メモリ / 分離方式（netns・root の有無）
- fandhe-browser: コミット / ビルド（release・feature） / バイナリサイズ
- Chromium: バージョン / 追加フラグ
- ページ: URL（fixture の内容・サイズ） / settle 秒数 / N

| 条件 | fandhe PSS (KiB) | Chromium PSS (KiB) | 削減率 | 50% 以上 |
| ---- | ---------------- | ------------------ | ------ | -------- |
| idle | | | | |
| loaded | | | | |

## PoC-1 線形外挿との差分

| 対象 | 外挿 (KiB) | 実測 (KiB) | 乖離率 | 考察 |
| ---- | ---------- | ---------- | ------ | ---- |

## 判定（オーナー）

- Conditional Go 条件 4 に対する判定:
- 備考（プロセス数・JS 子プロセスの有無・異常値）:
```
