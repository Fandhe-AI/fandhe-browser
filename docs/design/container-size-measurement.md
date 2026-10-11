# コンテナイメージサイズ測定レポート

- **対応ビヘイビア**: CTR-1（70% 以上削減の確認）・CTR-5（95% 以上削減。判定は対象外、数値の提示のみ）。`docs/spec/04-behavior/container-cloud.md`「コンテナイメージの構成」
- **対応タスク**: TASK-65（サブ 65.1・65.2）
- **マイルストーン**: MS-5
- **基準コミット**: `5a4b4d4913fd685e4f398a402b714204c5711569`（`origin/main`。`dirty=0`）
- **測定日**: 2026-10-11
- **測定環境**: Docker Engine 29.8.1（`linux/amd64`）/ storage driver `overlayfs`（image store は containerd snapshotter）/ ホスト Ubuntu 26.04.1 LTS（x86_64）
- **関連**: 親 Issue #300、計測ハーネス #487（PR #881）、本レポート #488

## 状態の要約

| 対象 | 状態 |
| ---- | ---- |
| 既定コンテナ（V8 同梱） | 基準コミットから本測定でビルドし、計測済み（正式値） |
| 軽量コンテナ（boa 同梱） | **基準コミットではビルドできない**。値は由来不明の既存イメージによる暫定値で、確定の合否には使えない |

## 目的・前提

- 判定対象は CTR-1 の「配布コンテナが Playwright 公式イメージ比で 70% 以上小さいか」。CTR-5（95% 以上）の判定は TASK-68（#489）で行い、本書は数値の提示にとどめる
- 計測対象のビルド構成は `docker/Dockerfile.release` の `default`（V8 152.2.0）と `lite`（`boa_engine` 0.22.0）。musl 静的バイナリを `scratch` に載せる。リリースプロファイルは `opt-level = "z"`・`lto`・`strip`・`panic = "abort"`
- イメージの中身はバイナリ・CA 証明書バンドル・`/licenses`・`/data`（空ディレクトリ）
- MB は 10 進（10^6 B）。比較対象の分母は同一実行で測った Playwright 公式イメージ

## 測定方法

```bash
# 基準コミットでの通し計測（Issue 専用タグ。共有タグを上書きしない）
CONTAINER_SIZE_DEFAULT_IMAGE=fandhe-browser:issue488-default \
CONTAINER_SIZE_LITE_IMAGE=fandhe-browser:issue488-lite \
make measure-container-size

# default のビルド成功後、既存の lite イメージと合わせて計測
CONTAINER_SIZE_SKIP_BUILD=1 CONTAINER_SIZE_NO_PULL=1 \
CONTAINER_SIZE_DEFAULT_IMAGE=fandhe-browser:issue488-default \
make measure-container-size
```

- 主指標は `docker history` のレイヤサイズ合計（`method=history-sum`）。image store に依存しないため。詳細は `harness/container-size/README.md`
- 参考値として `docker image inspect` の `.Size` を併記する。`docker images` の SIZE に最も近い値
- 削減率は小数第 2 位で切り捨て。比較対象は tag と manifest list digest でピン済み（`scripts/versions.sh`）で、取得後に digest を照合する
- コンテナは起動していない（build・inspect・history のみ）

## 結果

ハーネスの出力をそのまま掲載する（2 回目の実行。比較対象は取得済み）。

```text
container-size-env: docker_server=29.8.1 storage_driver=overlayfs image_store=containerd-snapshotter platform=linux/amd64 commit=5a4b4d4913fd685e4f398a402b714204c5711569 dirty=0 built=no
container-size: role=reference image=mcr.microsoft.com/playwright:v1.49.0-noble digest=sha256:0fc07c73230cb7c376a528d7ffc83c4bdcdcd3fc7efbe54a2eed72b1ec118377 id=sha256:0fc07c73230cb7c376a528d7ffc83c4bdcdcd3fc7efbe54a2eed72b1ec118377 platform=linux/amd64 method=history-sum bytes=2592825344 mb=2592.83 docker_size_bytes=3466071116 docker_size_mb=3466.07
container-size: role=default image=fandhe-browser:issue488-default digest=none id=sha256:f2300d27fd1e2944e54c952bef2938f25c56a946a497b82074a937df66e79270 platform=linux/amd64 method=history-sum bytes=53174272 mb=53.17 docker_size_bytes=72566092 docker_size_mb=72.57 reference_bytes=2592825344 reduction_pct=97.94 docker_size_reduction_pct=97.90
container-size: role=lite image=fandhe-browser:lite digest=none id=sha256:fb23446d21df7aa7dea2f40527ef1310455a917598717d62c4d9dd0e93d31b4b platform=linux/amd64 method=history-sum bytes=12271616 mb=12.27 docker_size_bytes=16178674 docker_size_mb=16.18 reference_bytes=2592825344 reduction_pct=99.52 docker_size_reduction_pct=99.53
```

| 対象 | history-sum (B) | history-sum (MB) | `.Size` (MB) | 削減率（history-sum） | 削減率（`.Size`） | 状態 |
| ---- | --------------: | ---------------: | -----------: | --------------------: | ----------------: | ---- |
| reference（Playwright） | 2,592,825,344 | 2592.83 | 3466.07 | - | - | 正式 |
| default | 53,174,272 | 53.17 | 72.57 | 97.94% | 97.90% | 正式（基準コミットでビルド） |
| lite | 12,271,616 | 12.27 | 16.18 | 99.52% | 99.53% | **暫定**（由来不明の既存イメージ） |

レイヤ内訳（`docker history` の各レイヤ。B）:

| レイヤ | default | lite |
| ------ | ------: | ---: |
| CA 証明書バンドル | 200,704 | 200,704 |
| バイナリ | 51,470,336 | 10,530,816 |
| `/licenses` | 1,495,040 | 1,531,904 |
| `/data` | 8,192 | 8,192 |

イメージの由来:

| 対象 | イメージ ID | 作成時刻 | 由来 |
| ---- | ----------- | -------- | ---- |
| default | `f2300d27fd1e…` | 2026-10-11T15:21:04Z | 基準コミットから本測定でビルド |
| lite | `fb23446d21df…` | 2026-10-11T10:24:01Z | ビルド元コミット・手順とも不明の既存イメージ |

lite のハーネス出力にある `commit=…` と `built=no` は、計測時のリポジトリ HEAD を示すだけで、lite イメージのビルド元を示さない。
このイメージの作成時刻は PR #871 のマージより前である。

## CTR-1（70% 以上削減）の確認

- default: history-sum 基準 97.94%、`.Size` 基準 97.90%。どちらの指標でも 70% 以上で、判定は計測方法に依存しない。**CTR-1 を満たす**
- lite: 暫定値では history-sum 基準 99.52%、`.Size` 基準 99.53%。ただし基準コミットのビルド成果物ではないため、**確定の合否としては扱わない**。lite がビルド可能になった後の再測定が必要
- CTR-1 の操作は文面上 `docker images` での計測。本書の主指標は history-sum で、`.Size` を併記して両方で 70% 以上であることを確認した
- CTR-5（95% 以上）は判定しない。数値は上表のとおりで、判定は #489（TASK-68）が行う

## 机上概算との差分

机上概算は spec の約 45.18MB（default）・約 12.31MB（lite）。

| 対象 | 実測（history-sum） | 机上 | 差（実測 − 机上） | 比 |
| ---- | ------------------: | ---: | ----------------: | --: |
| default | 53.17MB | 約 45.18MB | 約 +7.99MB | 約 1.18 倍 |
| lite（暫定） | 12.27MB | 約 12.31MB | 約 −0.04MB | 約 1.00 倍 |

- 合計が近くても、内訳が机上と一致しているとは限らない。合計だけで「概算どおり」とは結論しない。机上は PoC-8 の `scratch` 4.37MB（`core-proto` バイナリ 2.63MB と、差し引きの残り 1.74MB）にエンジン増分（V8 +40.81MB・boa +7.94MB）を足した構成で、実測のレイヤ（バイナリ・CA・`/licenses`・`/data`）とは粒度が異なる
- default と lite の実測差は 40.90MB（53.17 − 12.27）。これは「V8 構成と boa 構成」のバイナリ差で、机上の対応値は 40.81 − 7.94 = 32.87MB。机上の V8 単体増分（+40.81MB）とは直接比べない。エンジンなしの musl バイナリは測っていないため、エンジン単体の増分は本測定からは分離できない
- 机上の削減率（約 98.67%・約 99.64%）は分母が 3,390MB（PoC-8）。同じ分母で実測を割った参考値は default 98.43%・lite 99.64%（暫定）で、同一実行の reference 基準（上表）とは別物
- 事実として言える条件差: 机上は macOS での strip 後の増分と arm64 の PoC-8 値を使っている、`v8` が 150.0.0 から 152.2.0 になった、`/licenses`（default 1.50MB・lite 1.53MB）は机上に無い、現行 cli と PoC-2 の `core-proto` で機能が異なる。各要因の寄与は分解していない

## 比較対象サイズの基準差

| 出典 | 値 | platform | 計測方法 |
| ---- | -: | -------- | -------- |
| PoC-8 | 3,390MB | arm64 | Docker Desktop の `docker images`（containerd image store 由来かは未確認） |
| 本測定 | 2592.83MB | amd64 | history-sum |
| 本測定 | 3466.07MB | amd64 | `.Size`（containerd snapshotter） |

`linux/arm64` は本ホスト（x86_64）では測っておらず、外挿もしない（未測定）。

## 限界・残課題

- **lite のビルド不能**: 基準コミットで `--target lite` のビルドが失敗した。`scripts/gen-cargo-licenses.sh` が fail-closed で、`boa_ast`・`boa_engine`・`boa_gc`・`boa_interner`・`boa_macros`・`boa_parser`・`boa_string`（各 0.22.0）・`small_btree` 0.1.0・`tag_ptr` 0.1.0 の 9 crate について「no license file in package and no supplement in `docker/license-supplements/`」となり、`error: 9 crate(s) without license documents` で終了する。`docker/license-supplements/` にあるのは `fandhe-backend-0.4.2` と `v8-152.2.0` のみ。補完文書の追加は帰属表示・ライセンス判断を伴うため本作業では行わず、検査の迂回もしていない。追加後に lite を再測定し、本書の該当箇所を更新する必要がある
- `linux/arm64` と、レジストリ転送サイズ（圧縮サイズ）は未計測
- 現行 CLI の機能追加でサイズは変わるため、再測定が要る（`js-binary-size-report.md` と同じ注意）
- history のレイヤ値は snapshotter が報告する値で、`.Size` との差（default で約 19.4MB）の内訳は調べていない

## spec への示唆

机上概算の更新、CTR-1・CTR-5 の記述の見直しは spec リポ側のユーザー判断事項。本書は実測値の提供にとどめる。
