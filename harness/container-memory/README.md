# container-memory

配布コンテナを N 個同時に起動したときの集約メモリを、ホスト側の cgroup v2 から測定する
スクリプト（TASK-67・`CTR-3`・`CTR-6`・`CTR-7`・`MEAS-6`・MS-5。Issue #929・#930）。実機での実行と判定は
人間が行う（#306・#308・#319）。同じスクリプトを TASK-69（`CTR-6`。Chromium 側との比較）でも使う。

- `measure.sh`: 測定本体（bash・docker・jq・awk・coreutils のみ。Linux 専用）
- `compare.sh`: 2 つの結果 JSON（fandhe・Chromium）から、同じ N・同じ条件での削減率を算出する（bash・jq のみ）
- `self-test.sh`: 偽 docker シムと偽の cgroup / proc / meminfo によるオフライン自己テスト
  （`make check-container-memory`。`make ci` と CI の `harness-ubuntu` `[container-memory-selftest]` で実行）

`measure.sh` は `--target fandhe`（既定）と `--target chromium` の 2 つを同じ指標・同じ条件・同じ破綻判定で測る。
いま Chromium 側の値は PoC-1（macOS・RSS・1 インスタンス約 154MB）を N 倍した外挿値で、Linux のコンテナの実測ではない。
`CTR-3`・`CTR-6` が「検討中」なのはこのため。オーナー判断（2026-10-12・#306）により、Chromium も同じマシンで
同じコンテナ方式で実測する（TASK-81・#319 と同じ機会に測る）。実測の実行と判定は人間が行い、本スクリプトは測って比べるだけ。

## 前提

| 項目 | 内容 |
| ---- | ---- |
| OS | Linux（ホストの cgroup v2 と `/proc` を読むため。macOS・Windows は非対応） |
| カーネル | `memory.peak` が要るため 5.19 以降。無ければ exit 2（0 や null で代用しない） |
| Docker | cgroup v2 で動く Docker。`docker info` が `CgroupVersion=2` を返すこと |
| コマンド | `docker`・`jq`・`awk`・`bash` |
| 権限 | docker を実行できるユーザー（docker グループは root と同等の権限のため扱いに注意）。`/sys/fs/cgroup` と `/proc/<pid>/cgroup` の読み取りに root は不要。rootless Docker は memory controller の delegation が必要 |
| 測定対象イメージ | ローカルに存在すること（pull しない）。`docker build -f docker/Dockerfile.release -t fandhe-browser:default .`（軽量は `--target lite`） |
| Chromium の参照イメージ | `--target chromium` のとき。`harness/container-size/` と同じ Playwright 公式イメージ（`scripts/versions.sh` の `CONTAINER_SIZE_REFERENCE_IMAGE` / `_DIGEST`。`CTR-1`）を tag と digest で固定して使う。無ければ digest 指定で pull し（約 3GB）、取得後に照合する。不一致は fail-closed |
| 補助イメージ | `node:22-alpine`（`scripts/versions.sh`。digest 指定で pull し、取得後に RepoDigests と照合する（`docker.io/library/node@...` 表記・プラットフォーム別 manifest digest も許容）。不一致は fail-closed）。Cargo の依存ではなく、実行時に pull するだけで配布物には含めない |
| ネットワーク | loaded 条件は公開 URL とブリッジ経由の外向き通信が要る |

**専用マシンで実行すること。** N を増やすとホストのメモリを使い切り、ホスト OOM で他の作業が落ちる危険がある。
上限（`--host-mem-limit-pct`・`--max-n`）を設けているが、保証ではない。

## 実行手順

```bash
# 自己テスト + 測定（make 経由。追加引数は CONTAINER_MEMORY_ARGS）
make measure-container-memory CONTAINER_MEMORY_ARGS="--n-list 2 --no-doubling --conditions idle"

# 直接実行
harness/container-memory/measure.sh \
  --helper-image node:22-alpine --helper-digest sha256:... \
  --conditions idle,loaded --url https://example.com/ --out /tmp/container-memory.json
```

| 引数 | 既定 | 内容 |
| ---- | ---- | ---- |
| `--image` | `fandhe-browser:default` | 測定対象。軽量は `fandhe-browser:lite` などタグを変えて同じ手順で測る |
| `--n-list` | `10,50,100,200,500` | 狭義単調増加の同時起動数 |
| `--no-doubling` | 無効 | 指定しないと列の末尾の後も倍々（1000, 2000, …）に増やす |
| `--max-n` | 8192 | 倍々の上限。到達すると `stop_reason=max_n` |
| `--conditions` | `idle,loaded` | `loaded` は `--url` が必須 |
| `--url` | なし | loaded でナビゲートする公開 http(s) URL。`file:`・loopback・プライベート・IPv6 リテラルは拒否（fandhe の SSRF ガードと同方針） |
| `--ready-timeout` | 30 | 起動待ちの上限秒数（`CTR-7`。#306 のオーナー判断で 30 秒） |
| `--host-mem-limit-pct` | 80 | ホストの使用メモリがこの割合に達したら、次へ進まずに止まる |
| `--settle` | 5 | 起動後、cgroup を読む前の待ち秒数 |
| `--navigate-concurrency` | 8 | loaded の補助コンテナの同時実行数 |
| `--helper-image` / `--helper-digest` | `scripts/versions.sh` の値（make 経由） | 補助イメージ。直接実行では必須（環境変数 `CONTAINER_MEMORY_HELPER_IMAGE` / `_DIGEST` でも可） |
| `--out` | 標準出力 | 一時ファイル経由で書く |

### Chromium 側の測定と比較（Issue #930）

```bash
# Chromium を測る（参照イメージのピンは make 経由なら scripts/versions.sh の値が渡る）
make measure-container-memory CONTAINER_MEMORY_ARGS="--target chromium --conditions idle,loaded --url https://example.com/ --out /tmp/chromium.json"
# fandhe（default / lite）は従来どおり --target fandhe（既定）で測り、同じ条件の 2 つを比べる
make compare-container-memory CONTAINER_MEMORY_COMPARE_ARGS="--fandhe /tmp/fandhe.json --chromium /tmp/chromium.json --out /tmp/compare.json"
```

| 引数 | 内容 |
| ---- | ---- |
| `--target` | `fandhe`（既定）または `chromium`。`chromium` は `--image` と併用できない（container-size のピンとずれないため。exit 2） |
| `--reference-image` / `--reference-digest` | Chromium の参照イメージ（repo:tag と digest）。既定は環境変数 `CONTAINER_SIZE_REFERENCE_IMAGE` / `_DIGEST`。無ければ exit 2 |
| `--n-list`（chromium の既定） | `1,5,10,50,100,200,500`（fandhe の `10,50,100,200,500` と重なる N で比べられる） |

Chromium の起動条件（fandhe と揃える点・非対称な点を含めて結果の `launch` に記録する）:

- Chromium 本体のパスは実行ごとに 1 回、`docker run --rm --network none ... --entrypoint sh <参照イメージ> -c 'ls -d /ms-playwright/chromium-[0-9]*/chrome-linux/chrome'` で解決する
  （リビジョン番号を埋め込まない）。結果が正規表現に合う 1 行でなければ exit 2。
- 起動は `--init --user pwuser --entrypoint <解決したパス>` に `--headless=new --disable-gpu --no-first-run --no-default-browser-check
  --no-sandbox --disable-dev-shm-usage --user-data-dir=/tmp/fandhe-ctrmem-profile --remote-debugging-address=127.0.0.1
  --remote-debugging-port=9333 about:blank` を続ける。CDP のポートを fandhe と同じ 9333 にそろえ、起動待ち・ナビゲートの補助コンテナを共通化している。
- `--no-sandbox`: Chromium の sandbox は user namespace または SYS_ADMIN を要し、`--cap-drop ALL` の下では動かないため。
  代わりの統制は `--cap-drop ALL`・`no-new-privileges`・非 root（`pwuser`）・CDP は loopback のみ（`-p`・`--network host` は使わない）・
  ナビゲート先は SSRF ガードを通った公開 http(s) URL だけ・専用マシンで実行・実行 ID ラベルでの後始末。
- `--disable-dev-shm-usage`: Docker の `/dev/shm` は既定で 64MB しかなく、renderer が落ちて `exited` の破綻に誤計上されるのを防ぐため。
- `--init`: Chromium を PID 1 にしない（zombie の回収）。tini は約 1MB だが fandhe 側には付けないため非対称になる（結果の `launch.init` に記録）。
- loaded のナビゲートは `navigate.mjs` の `--page-target` 経由（browser WebSocket では `Page.navigate` を使えないため）。
- 小さい N でもホストの上限割合（`--host-mem-limit-pct`）に届く前提で、届いたら止まって `complete=false` と測れた最大の N を残す。

結果 JSON への追加（`schema: container-memory/v1` のまま）: `target`、`image.pinned_digest`（chromium のみ）、`launch`、
`conditions[].max_ok_n`（完走かつ破綻 0 のステップの最大 N。該当なしは null。破綻点の読み取り用）、`behaviors` に `CTR-6`。

`compare.sh` の規則:

- 両方に実測があり、両方とも完走・破綻 0・全コンテナ測定済み（`measured == started == n`）の N にだけ削減率を出す。
  同じ N の実測が無い、または破綻・不完全のときは数値を出さず `reason` を出す（外挿値を実測として扱わない）。
- 実行環境（`arch`・`kernel`・`mem_total_bytes`・`cgroup_driver`）や条件（`ready_timeout_s`・`settle_s`・`host_mem_limit_pct`、
  loaded では `url`・`navigate_concurrency`）が食い違う 2 つは `comparable: false` と `reasons[]` を出し、削減率を 1 件も出さない（exit 0）。
- 削減率は `(1 − fandhe / chromium) × 100` を小数第 2 位で切り捨てる（`memory_peak` と `memory_current` の合計で計算。負の値はそのまま）。
- `reference_thresholds` に `CTR-3`（50%）・`CTR-6`（90%）の参考判定（N=100・`memory_peak`）を出す。合否は人間が確定する。
- `--poc1-chromium-per-instance-bytes`・`--poc8-fandhe-n100-bytes` を渡したときだけ、別オブジェクト `extrapolation`（`kind: extrapolated`）に
  外挿値と実測との差を出す。`reduction_pct` には影響しない。
- 終了コード: 0 = 比較した（比較不能でも 0。`comparable` を見る）、2 = 使用エラー・入力 JSON の検査不合格。

`--cgroup-root`・`--proc-root`・`--meminfo` は自己テスト用の入口で、通常は指定しない。
docker の実行ファイルは環境変数 `DOCKER_BIN` で差し替えられる（自己テスト用）。

## 測定方法

1. 各 N（条件ごとに idle → loaded）で、測定対象を N 個、逐次 `docker run -d` で起動する。ラベル
   `fandhe-ctrmem.run=<実行 ID>` を付け、`--cap-drop ALL`・`no-new-privileges` で起動する。`--rm` は付けない
   （OOM で落ちたコンテナが消えると `State.OOMKilled` を読めなくなるため）。
2. 起動の都度、`docker inspect` の PID から `/proc/<pid>/cgroup` の `0::` 行を読んで cgroup パスを確定する
   （systemd と cgroupfs でパス形式が違うため推測しない）。
3. 起動待ち（`CTR-7`）: 測定対象と network namespace を共有する補助コンテナ（`--network container:<cid>`）で
   `http://127.0.0.1:9333/json/version` に到達するまで待つ。イメージは scratch でシェルが無いため、コンテナ内で
   コマンドを実行する方式は採れない。本体の待ち受け（loopback）は変えず、`-p`・`--network host` も使わない
   （`SEC-2`・`SEC-4`）。所要時間は「ホストで記録した `docker run` 直前」から「補助コンテナの終了」まで。
   補助コンテナ自体の起動時間を含むため上側に寄った値になるが、破綻を見逃す側には倒れない。
4. loaded 条件: 起動待ちを通過した各コンテナで、補助コンテナが `harness/multi-instance-memory/navigate.mjs`
   （読み取り専用で mount）を使い、CDP で `--url` を 1 回ナビゲートする。失敗は `navigate_failed` に数える。
5. `--settle` 秒待ってから、各コンテナの `memory.peak`・`memory.current`・`memory.events` の `oom_kill` と
   `State.Running`・`State.OOMKilled` を読む。稼働中なのに cgroup を読めない場合は exit 1（fail-closed）。
6. そのステップのコンテナを削除し（実行 ID のラベルに一致するものだけ）、残りが 0 件であることを確認して次の N へ進む。

破綻（OOM kill・`docker run` の失敗・起動待ちの超過・停止・navigate 失敗）が 1 件でも出た N で止まり、
それ以上 N を増やさない。破綻は 1 コンテナにつき `oom_kill` > `exited` > `ready_timeout` > `navigate_failed` の
優先順位で 1 つに数える。

## 出力の読み方

標準出力（または `--out`）に JSON（`schema: container-memory/v1`）を 1 つ出す。

| キー | 内容 |
| ---- | ---- |
| `args` | 使った引数（`ready_timeout_s`・`host_mem_limit_pct`・補助イメージの digest 付き参照を含む） |
| `image` | `ref`・`id`（config digest）・`repo_digests`（ローカルビルドは空） |
| `environment` | `kernel`・`arch`・`docker_server_version`・`docker_client_version`・`cgroup_version`・`cgroup_driver`・`mem_total_bytes`・`timing_resolution`（`ms`、`EPOCHREALTIME` が無い bash では `s`） |
| `conditions[].stop_reason` | `breakdown`（破綻）・`host_memory_limit`・`max_n`（倍々の上限）・`completed`（`--no-doubling` で列を完走） |
| `conditions[].steps[]` | N ごとの結果。`complete=false` はホストメモリ上限で起動を打ち切ったステップ |
| `steps[].started` / `ready` / `measured` | 起動に成功した数 / 起動待ちを通過した数 / メモリ値を読めた数 |
| `steps[].breakdowns` | `oom_kill`・`start_failed`・`ready_timeout`・`exited`・`navigate_failed` と `total`。ナビゲート未実行（ホスト上限）は `navigate_not_run`（破綻に数えず complete=false で表す） |
| `steps[].oom_detail` | `memory.events` の `oom_kill` 合計と `State.OOMKilled` の件数（両方を記録） |
| `steps[].memory_peak_bytes` / `memory_current_bytes` | `sum`・`avg`（整数の切り捨て）・`max`・`count` |
| `steps[].ready_time` | 起動待ちの所要の `max`・`avg`（単位は常に `ms`。値がなければ `null`） |
| `steps[].host_mem_used_pct` | 後始末の前のホストの使用率（(MemTotal − MemAvailable) / MemTotal） |

終了コード: 0 = 測定が完了した（破綻があっても結果として 0）、1 = 測定の失敗（稼働中コンテナの cgroup を読めない・
後始末の検証失敗・補助イメージの digest 不一致など）、2 = 使用エラーまたは前提不足。

## 後始末

終了時・中断時（EXIT・INT・TERM）に、バックグラウンドの補助コンテナを止め、実行 ID のラベルに一致する
コンテナだけを `docker rm -f` で削除する。他のコンテナには触れない。強制終了（SIGKILL）された場合は残りうるため、
次で確認して手で消す。

```bash
docker ps -a --filter label=fandhe-ctrmem.run
docker ps -aq --filter label=fandhe-ctrmem.run | xargs -r docker rm -f   # このハーネスのコンテナ全部を消してよいときだけ
```

## レポートテンプレート（実行と判定はオーナー。#306・#308・#319）

実測値・外挿値・両者の差・削減率を分けて書く。fandhe は default / lite の両方、Chromium は `--target chromium` の結果を使う。

```markdown
## 実測（measure.sh。同じマシン・同じ条件）
- 実行日・マシン（CPU・メモリ・OS・カーネル・Docker のバージョン）:
- 引数（各結果の `args` を貼る）:
- fandhe default / lite・chromium それぞれ（`image.ref`・`image.id`・chromium は `image.pinned_digest` と `launch`）:

| 対象 | 条件 | N | memory.peak 合計 | 平均 | memory.current 合計 | 平均 | max_ok_n | stop_reason | 破綻の内訳 |
| ---- | ---- | - | ---------------- | ---- | ------------------- | ---- | -------- | ----------- | ---------- |

## 外挿値（実測ではない。出典を併記）
- PoC-1: Chromium 1 インスタンス ≒ 154MB（macOS・RSS）× N =
- PoC-8: fandhe 100 コンテナで 359.6MB（Docker Desktop arm64・memory.peak）。比例計算で約 97.7% 削減

## 実測と外挿の差（compare.sh の `extrapolation.*.measured_minus_extrapolated_pct`）
- 指標（RSS と memory.peak）と環境（macOS と Linux コンテナ）が違うことを前提に、差の理由:

## 削減率（compare.sh の `comparisons`。算出しなかった行も `reason` ごと残す）

| 条件 | N | memory.peak 削減率 | memory.current 削減率 | reason |
| ---- | - | ------------------ | --------------------- | ------ |

## 判定（オーナー記入）
- CTR-3（100 コンテナで 50% 以上削減）・CTR-7（破綻点 = 各結果の `max_ok_n` と `stop_reason`）:
- TASK-69: CTR-6（90% 以上削減）:
```

注記: `memory.peak` の合計では、共有イメージレイヤの page cache が最初に触れたコンテナにだけ計上される。両側とも同じ方式のため
補正せず、ここに注記するに留める。
