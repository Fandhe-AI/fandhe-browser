# container-size

配布コンテナ 2 種（既定コンテナ = V8 同梱、軽量コンテナ = boa 同梱。`docker/Dockerfile.release` の
`default` / `lite` ターゲット）と比較対象（Playwright 公式イメージ）のイメージサイズを計測し、
`container-size:` 行で出力する（TASK-65.1・Issue #487・`CTR-1`・`CTR-5`・MS-5。親 Issue #300）。

本ハーネスは **計測のみでゲートではない**。閾値未達でも exit 0 で終わる。

| 後続 | 内容 |
| ---- | ---- |
| #488（TASK-65.2） | 測定レポート作成・机上概算との差分記録・`CTR-1`（70% 以上削減）の判定 |
| TASK-68 | `CTR-5`（95% 以上削減）の達成確認 |

## 使い方

```bash
make measure-container-size                                # 既定・軽量をビルドして計測
CONTAINER_SIZE_SKIP_BUILD=1 make measure-container-size    # ビルド済みイメージを計測
```

結果行は標準出力、ビルド・pull の進捗は標準エラーに出る。`make` 経由では自己テストの出力
（`ok: ...`）も標準出力に混ざるため、結果だけ取り出すときは `| grep '^container-size'` を使う。

### 環境変数

| 変数 | 既定 | 内容 |
| ---- | ---- | ---- |
| `CONTAINER_SIZE_REFERENCE_IMAGE` | `scripts/versions.sh` | 比較対象（tag 必須） |
| `CONTAINER_SIZE_REFERENCE_DIGEST` | `scripts/versions.sh` | 比較対象の manifest list digest |
| `CONTAINER_SIZE_DEFAULT_IMAGE` | `fandhe-browser:default` | 既定コンテナのタグ |
| `CONTAINER_SIZE_LITE_IMAGE` | `fandhe-browser:lite` | 軽量コンテナのタグ |
| `CONTAINER_SIZE_PLATFORM` | daemon のネイティブ | `linux/amd64` または `linux/arm64` |
| `CONTAINER_SIZE_SKIP_BUILD` | 空 | 非空でビルドを省略（イメージ不在は exit 2） |
| `CONTAINER_SIZE_NO_PULL` | 空 | 非空で比較対象を pull しない（不在は exit 2） |

## 比較対象のピン

`CTR-1` が指定する `mcr.microsoft.com/playwright:v1.49.0-noble` を tag と digest の両方で固定し、
`scripts/versions.sh` を正本とする。取得は digest 指定で行い、取得後に `RepoDigests` と照合する。
`--platform` 付き pull では platform 固有 digest が入るため、ピンした index digest に加え、
`docker buildx imagetools inspect --raw` で index から引いた platform 一致 digest も受理する
（いずれも無い・index 取得不能は fail-closed）。default と lite は参照の別名（`docker.io/library/` 等）を
正規化して衝突を拒否し、さらに計測前に両イメージ ID が異なることを検証する。

| 対象 | digest |
| ---- | ------ |
| manifest list（ピン対象） | `sha256:0fc07c73230cb7c376a528d7ffc83c4bdcdcd3fc7efbe54a2eed72b1ec118377` |
| linux/amd64 | `sha256:8d8af91e3f34a33c509022a20f62f53954f49011ad4104edd766138a2edfc567` |
| linux/arm64 | `sha256:43f30f10a45688023730cb6edcc6674bafae11c7bbc111c74c87bf8b2ba9bf15` |

更新手順: `docker buildx imagetools inspect <image>` の `Digest` を `scripts/versions.sh` へ転記する。

## 計測方法

- **主指標 `bytes`**: `docker history --human=false` のレイヤサイズの合計。image store に依存しない
- **参考値 `docker_size_bytes`**: `docker image inspect` の `.Size`。containerd image store
  （`container-size-env` の `image_store=containerd-snapshotter`）では圧縮 blob 分だけ大きくなる。
  graph driver の `docker images` の SIZE とは意味が異なるため、机上概算との比較には `bytes` を使う
- 圧縮サイズ（レジストリ転送サイズ）は計測しない
- 削減率 `reduction_pct` は同一実行で測った比較対象の `bytes` に対して、`docker_size_reduction_pct` は比較対象の `docker_size_bytes`（`.Size`）に対して算出し、小数第 2 位で **切り捨てる**
  （閾値を跨いで過大表示しないため）。`mb` は 10 進（10^6）

## 出力契約

```text
container-size-env: docker_server=<ver> storage_driver=<name> image_store=<containerd-snapshotter|graph-driver> platform=<os/arch> commit=<sha|unknown> dirty=<0|1|unknown> built=<yes|no>
container-size: role=reference image=<repo:tag> digest=<sha256:...> id=<sha256:...> platform=<os/arch> method=history-sum bytes=<N> mb=<X.XX> docker_size_bytes=<M> docker_size_mb=<Y.YY>
container-size: role=default image=<ref> digest=none id=<sha256:...> platform=<os/arch> method=history-sum bytes=<N> mb=<X.XX> docker_size_bytes=<M> docker_size_mb=<Y.YY> reference_bytes=<R> reduction_pct=<P.PP> docker_size_reduction_pct=<Q.QQ>
container-size: role=lite ...（default と同じ項目）
```

出力する値は正規表現で検証済みのものに限る。

## 終了コード

| exit | 意味 |
| ---- | ---- |
| 0 | 計測成功（閾値判定はしない） |
| 1 | 計測失敗（build・pull・inspect・history の失敗、digest 不一致、platform 不一致、サイズ値不正） |
| 2 | 使用エラー・前提不足（引数不正、docker・jq 不在、daemon 不達、`--skip-build` / `--no-pull` 指定時のイメージ不在） |

## 注意点

- 比較対象の取得は数 GB のディスクとネットワークを要する。`make ci` には含めない
- コンテナは起動しない（build・pull・inspect・history のみ）。イメージの削除・prune もしない
- 必須構成は `linux/amd64`。`linux/arm64` は arm64 ホストで実行する（`docker/release-build.sh` がクロスビルドを拒否するため）
- 自己テスト `self-test.sh` は偽 `docker` シムのみを使い、実 daemon・ネットワークへ出ない
- PoC-8 の 3.39GB は arm64・Docker Desktop での値で、image store 起因の水増しの有無は未確認。
  #488 で測定条件を確認する
- 机上概算（既定 約 45.18MB・軽量 約 12.31MB）との比較は #488 で行う
