# container-memory

配布コンテナを N 個同時に起動したときの集約メモリを、ホスト側の cgroup v2 から測定する
スクリプト（TASK-67・`CTR-3`・`CTR-7`・`MEAS-6`・MS-5。Issue #929）。実機での実行と判定は
人間が行う（#306・#308）。同じスクリプトを TASK-69（Chromium 側との比較）でも使う。

- `measure.sh`: 測定本体（bash・docker・jq・awk・coreutils のみ。Linux 専用）
- `self-test.sh`: 偽 docker シムと偽の cgroup / proc / meminfo によるオフライン自己テスト
  （`make check-container-memory`。`make ci` と CI の `harness-ubuntu` `[container-memory-selftest]` で実行）

この Issue の範囲は fandhe-browser 側の測定スクリプトまで。Chromium 側の測定と比較は後続。

## 前提

| 項目 | 内容 |
| ---- | ---- |
| OS | Linux（ホストの cgroup v2 と `/proc` を読むため。macOS・Windows は非対応） |
| カーネル | `memory.peak` が要るため 5.19 以降。無ければ exit 2（0 や null で代用しない） |
| Docker | cgroup v2 で動く Docker。`docker info` が `CgroupVersion=2` を返すこと |
| コマンド | `docker`・`jq`・`awk`・`bash` |
| 権限 | docker を実行できるユーザー（docker グループは root と同等の権限のため扱いに注意）。`/sys/fs/cgroup` と `/proc/<pid>/cgroup` の読み取りに root は不要。rootless Docker は memory controller の delegation が必要 |
| 測定対象イメージ | ローカルに存在すること（pull しない）。`docker build -f docker/Dockerfile.release -t fandhe-browser:default .`（軽量は `--target lite`） |
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

## レポートテンプレート（実行と判定はオーナー。#306）

```markdown
- 実行日・マシン（CPU・メモリ・OS・カーネル・Docker のバージョン）:
- イメージ（ref・id・default / lite）:
- 引数（`args` を貼る）:
- idle: 最大 N（破綻なしで到達）= / 破綻の種類と件数 = / N=最大時の memory.peak 合計・平均 =
- loaded（URL）: 同上
- 判定（CTR-3・CTR-7）:
```
