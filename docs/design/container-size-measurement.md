# コンテナイメージサイズ測定レポート

- **対応ビヘイビア**: CTR-1（70% 以上削減の確認）・CTR-5（95% 以上削減の判定。TASK-68・#489）。`docs/spec/04-behavior/container-cloud.md`「コンテナイメージの構成」
- **対応タスク**: TASK-65（サブ 65.1・65.2。軽量コンテナの再測定は #913）・TASK-68（サブ 68.1。CTR-5 の判定）
- **マイルストーン**: MS-5
- **基準コミット**: `411b1909b179104093babbeca39f564bfd7b52d7`（`origin/main`。#912 の補完文書を含む。`dirty=0`）
- **測定日**: 2026-10-16
- **測定環境**: Docker Engine 29.8.1（`linux/amd64`）/ storage driver `overlayfs`（image store は containerd snapshotter）/ ホスト Ubuntu 26.04.1 LTS（x86_64）。Docker Engine は 29.8.1 のまま
- **関連**: 親 Issue #300、計測ハーネス #487（PR #881）、本レポート #488、CTR-5 判定 #489、lite のライセンス補完文書 #912、再測定 #913

## 状態の要約

| 対象 | 状態 |
| ---- | ---- |
| 既定コンテナ（V8 同梱） | 基準コミットから本測定でビルドし、計測済み（正式値。#488 の値を置き換え） |
| 軽量コンテナ（boa 同梱） | 基準コミットから本測定でビルドし、計測済み（正式値。#488 の暫定値を置き換え） |

## 目的・前提

- 判定対象は CTR-1 の「配布コンテナが Playwright 公式イメージ比で 70% 以上小さいか」。CTR-5（95% 以上）の判定は本書「CTR-5（95% 以上削減）の判定」節で行う（TASK-68.1・#489）。測定値は TASK-65 の正式値を再利用し、再測定はしていない
- 計測対象のビルド構成は `docker/Dockerfile.release` の `default`（V8 152.2.0）と `lite`（`boa_engine` 0.22.0）。musl 静的バイナリを `scratch` に載せる。リリースプロファイルは `opt-level = "z"`・`lto`・`strip`・`panic = "abort"`
- イメージの中身はバイナリ・CA 証明書バンドル・`/licenses`・`/data`（空ディレクトリ）
- MB は 10 進（10^6 B）。比較対象の分母は同一実行で測った Playwright 公式イメージ

## 測定方法

```bash
# 基準コミットで default と lite を別々にビルド（Issue 専用タグ。共有タグを上書きしない）
docker build -f docker/Dockerfile.release --target lite -t fandhe-browser:issue913-lite .
docker build -f docker/Dockerfile.release --target default -t fandhe-browser:issue913-default .

# ビルドを省略し、取得済みの比較対象と合わせて計測
CONTAINER_SIZE_SKIP_BUILD=1 CONTAINER_SIZE_NO_PULL=1 \
CONTAINER_SIZE_DEFAULT_IMAGE=fandhe-browser:issue913-default \
CONTAINER_SIZE_LITE_IMAGE=fandhe-browser:issue913-lite \
make measure-container-size
```

- 主指標は `docker history` のレイヤサイズ合計（`method=history-sum`）。image store に依存しないため。詳細は `harness/container-size/README.md`
- 参考値として `docker image inspect` の `.Size` を併記する。`docker images` の SIZE に最も近い値
- 削減率は小数第 2 位で切り捨て。比較対象は tag と manifest list digest でピン済み（`scripts/versions.sh`）で、取得後に digest を照合する
- コンテナは起動していない（build・inspect・history のみ）

## 結果

ハーネスの出力をそのまま掲載する（比較対象は取得済みで、pull していない）。`built=no` は、ビルドと計測を別々のコマンドで実行したことを表す（ビルド元は上記の基準コミットで、`dirty=0`）。

```text
container-size-env: docker_server=29.8.1 storage_driver=overlayfs image_store=containerd-snapshotter platform=linux/amd64 commit=411b1909b179104093babbeca39f564bfd7b52d7 dirty=0 built=no
container-size: role=reference image=mcr.microsoft.com/playwright:v1.49.0-noble digest=sha256:0fc07c73230cb7c376a528d7ffc83c4bdcdcd3fc7efbe54a2eed72b1ec118377 id=sha256:0fc07c73230cb7c376a528d7ffc83c4bdcdcd3fc7efbe54a2eed72b1ec118377 platform=linux/amd64 method=history-sum bytes=2592825344 mb=2592.83 docker_size_bytes=3466071116 docker_size_mb=3466.07
container-size: role=default image=fandhe-browser:issue913-default digest=none id=sha256:4a041fd899b4ad75e59c31cd7c2191526daeb7ae9334fbdb1f0926a245f50c44 platform=linux/amd64 method=history-sum bytes=53747712 mb=53.75 docker_size_bytes=73393736 docker_size_mb=73.39 reference_bytes=2592825344 reduction_pct=97.92 docker_size_reduction_pct=97.88
container-size: role=lite image=fandhe-browser:issue913-lite digest=none id=sha256:d00857d897d78516a2b892e4a78155972e335c082b94ff52139a5e0936105226 platform=linux/amd64 method=history-sum bytes=13434880 mb=13.43 docker_size_bytes=17622622 docker_size_mb=17.62 reference_bytes=2592825344 reduction_pct=99.48 docker_size_reduction_pct=99.49
```

| 対象 | history-sum (B) | history-sum (MB) | `.Size` (MB) | 削減率（history-sum） | 削減率（`.Size`） | 状態 |
| ---- | --------------: | ---------------: | -----------: | --------------------: | ----------------: | ---- |
| reference（Playwright） | 2,592,825,344 | 2592.83 | 3466.07 | - | - | 正式 |
| default | 53,747,712 | 53.75 | 73.39 | 97.92% | 97.88% | 正式（基準コミットでビルド） |
| lite | 13,434,880 | 13.43 | 17.62 | 99.48% | 99.49% | 正式（基準コミットでビルド） |

レイヤ内訳（`docker history` の各レイヤ。B）:

| レイヤ | default | lite |
| ------ | ------: | ---: |
| CA 証明書バンドル | 200,704 | 200,704 |
| バイナリ | 52,006,912 | 11,087,872 |
| `/licenses` | 1,531,904 | 2,138,112 |
| `/data` | 8,192 | 8,192 |

イメージの由来:

| 対象 | イメージ ID | 作成時刻 | 由来 |
| ---- | ----------- | -------- | ---- |
| default | `4a041fd899b4…` | 2026-10-16T18:36:05Z | 基準コミットから本測定でビルド |
| lite | `d00857d897d7…` | 2026-10-16T18:35:25Z | 基準コミットから本測定でビルド |

## CTR-1（70% 以上削減）の確認

- default: history-sum 基準 97.92%、`.Size` 基準 97.88%。どちらの指標でも 70% 以上で、判定は計測方法に依存しない。**CTR-1 を満たす**
- lite: history-sum 基準 99.48%、`.Size` 基準 99.49%。どちらの指標でも 70% 以上で、基準コミットのビルド成果物による確定値。**CTR-1 を満たす**
- CTR-1 の操作は文面上 `docker images` での計測。本書の主指標は history-sum で、`.Size` を併記して両方で 70% 以上であることを確認した
- CTR-5（95% 以上）の判定は次節

## CTR-5（95% 以上削減）の判定

TASK-68.1（#489）。上記の正式値（基準コミット `411b1909…`・`linux/amd64`）をそのまま使い、再測定はしていない。95% 以上削減は「reference の 5% 以下」と同義で、上限は history-sum 基準 129,641,267B（約 129.64MB）、`.Size` 基準 173,303,555B（約 173.30MB）。

| 対象 | 削減率（history-sum） | 削減率（`.Size`） | 閾値 | 上限までの余裕（history-sum / `.Size`） | 判定 |
| ---- | --------------------: | ----------------: | ---: | --------------------------------------: | ---- |
| default | 97.92% | 97.88% | 95% | 約 75.89MB / 約 99.91MB | 達成 |
| lite | 99.48% | 99.49% | 95% | 約 116.21MB / 約 155.68MB | 達成 |

- default: どちらの指標でも 95% 以上で、**CTR-5 を満たす**
- lite: どちらの指標でも 95% 以上で、**CTR-5 を満たす**
- 操作の文面は `docker images` での計測だが、どちらの指標でも判定が変わらないため、指標のずれは判定に影響しない

### PoC-8 実測との比較

| 出典 | 削減率 | 備考 |
| ---- | -----: | ---- |
| PoC-8 `scratch` | 99.87% | 4.37MB / 3,390MB。`core-proto`（JS エンジンなし）。arm64・Docker Desktop |
| PoC-8 `alpine` | 99.42% | 同上 |
| default（本測定） | 97.92% | history-sum 基準。分母 3,390MB で割った参考値は 98.41% |
| lite（本測定） | 99.48% | history-sum 基準。同じく参考値は 99.60% |

- PoC-8 の 99.87% に対し、default は -1.95pt、lite は -0.39pt（history-sum 基準）。退行ではなく、PoC-8 の `core-proto` に JS エンジンが入っていないのに対し、配布コンテナは V8 または boa を同梱し、さらに `/licenses` と CA バンドルを含むため
- 分母も揃っていない（PoC-8 は arm64 の `docker images`、本測定は amd64 の history-sum）。各要因の寄与は分解していない。詳細は「机上概算との差分」「比較対象サイズの基準差」を参照

### 判定の条件・限界

- 判定は基準コミット `411b1909…`・`linux/amd64` の値による。`linux/arm64` は未測定（spec の決定 2 で aarch64 は任意）
- CTR-5 の前提にある `alpine` 構成は配布物ではない（配布は `scratch` の default・lite の 2 種）ため判定せず、判定対象は配布コンテナの 2 種とする
- 基準コミット以降に `fandhe-browser-cli` のコード変更が 2 件ある（#1073 setup コマンド・#1074 テスト）。`docker/`・`Cargo.toml`・`Cargo.lock` に差分はなく、default でも約 76MB の余裕があるため判定は変わらない見込みだが、再測定はしていない（未検証）。将来の機能追加でサイズが増えた場合は再測定が要る

### 未達要因の分析・追加最適化

該当なし（両構成・両指標とも達成）。TASK-68 の追加最適化パッチは不要。

## 机上概算との差分

机上概算は spec の約 45.18MB（default）・約 12.31MB（lite）。

| 対象 | 実測（history-sum） | 机上 | 差（実測 − 机上） | 比 |
| ---- | ------------------: | ---: | ----------------: | --: |
| default | 53.75MB | 約 45.18MB | 約 +8.57MB | 約 1.19 倍 |
| lite | 13.43MB | 約 12.31MB | 約 +1.12MB | 約 1.09 倍 |

- 合計が近くても、内訳が机上と一致しているとは限らない。合計だけで「概算どおり」とは結論しない。机上は PoC-8 の `scratch` 4.37MB（`core-proto` バイナリ 2.63MB と、差し引きの残り 1.74MB）にエンジン増分（V8 +40.81MB・boa +7.94MB）を足した構成で、実測のレイヤ（バイナリ・CA・`/licenses`・`/data`）とは粒度が異なる
- default と lite の実測差は、イメージ全体（history-sum）で 40.31MB（53.75 − 13.43。バイト差 40,312,832）。バイナリレイヤ単体の差は 40.92MB（52,006,912 − 11,087,872 = 40,919,040B）で、/licenses レイヤは lite の方が約 0.6MB 大きいため全体差より広がる。これは「V8 構成と boa 構成」の差で、机上の対応値は 40.81 − 7.94 = 32.87MB。机上の V8 単体増分（+40.81MB）とは直接比べない。エンジンなしの musl バイナリは測っていないため、エンジン単体の増分は本測定からは分離できない
- 机上の削減率（約 98.67%・約 99.64%）は分母が 3,390MB（PoC-8）。同じ分母で実測を割った参考値は default 98.41%・lite 99.60%で、同一実行の reference 基準（上表）とは別物
- 事実として言える条件差: 机上は macOS での strip 後の増分と arm64 の PoC-8 値を使っている、`v8` が 150.0.0 から 152.2.0 になった、`/licenses`（default 1.53MB・lite 2.14MB）は机上に無い、現行 cli と PoC-2 の `core-proto` で機能が異なる。各要因の寄与は分解していない

## 比較対象サイズの基準差

| 出典 | 値 | platform | 計測方法 |
| ---- | -: | -------- | -------- |
| PoC-8 | 3,390MB | arm64 | Docker Desktop の `docker images`（containerd image store 由来かは未確認） |
| 本測定 | 2592.83MB | amd64 | history-sum |
| 本測定 | 3466.07MB | amd64 | `.Size`（containerd snapshotter） |

`linux/arm64` は本ホスト（x86_64）では測っておらず、外挿もしない（未測定）。

## 限界・残課題

- lite のビルド不能（#488 時点。`boa` 系 9 crate のライセンス文書不足で `scripts/gen-cargo-licenses.sh` が fail-closed）は、#912 の補完文書（`docker/license-supplements/`）で解消した。本測定（#913）で `--target lite` のビルドに成功し、検査は迂回していない
- #488 時点の default の値（基準コミット `5a4b4d4913fd685e4f398a402b714204c5711569`・53,174,272B（53.17MB）・97.94%）は履歴。現行コードとは対応せず、本書の正式値は基準コミット `411b1909…` の測定値に置き換えた
- `linux/arm64` と、レジストリ転送サイズ（圧縮サイズ）は未計測
- 現行 CLI の機能追加でサイズは変わるため、再測定が要る（`js-binary-size-report.md` と同じ注意）
- history のレイヤ値は snapshotter が報告する値で、`.Size` との差（default で約 19.65MB）の内訳は調べていない

## spec への示唆

机上概算の更新、CTR-1・CTR-5 の記述の見直しは spec リポ側のユーザー判断事項。本書は実測値の提供にとどめる。

以下も spec リポ側のユーザー判断事項（本リポでは編集しない）。

- CTR-5 の前提文言「`scratch`/`alpine` 双方」と配布構成（`scratch` のみ）にずれがある。本書は配布コンテナ 2 種で判定した
- CTR-5 の根拠として、PoC-8 の値に加え配布コンテナの実測値（default 97.92%・lite 99.48%）を追記するかどうか
