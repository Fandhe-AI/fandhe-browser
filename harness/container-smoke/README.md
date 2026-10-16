# container-smoke

配布コンテナ（軽量 = boa / 既定 = V8）にローカルフィクスチャ 12 種をボリュームマウントし、`text` / `texts` / `attr` 相当の取得が追加 OS ライブラリなしで動くことを確認するハーネス（TASK-66.1・Issue #302・`CTR-2`・MS-5。親 Issue #301）。

**現状: 軽量コンテナで 12/12 成功**（`query --file`、#824 を利用。結果は「実行結果」参照）。一発実行を持たない旧バイナリに当たった場合は成功を装わず `result=blocked`（exit 2）を記録する。

| 後続 | 内容 |
| ---- | ---- |
| #303（TASK-66.2） | 既定コンテナで本スクリプトを流用（`--role default`） |
| #304（TASK-66.3） | JS スモーク |

## 使い方

```bash
make check-container-smoke                                  # 自己テスト → 軽量コンテナの確認（要 docker・jq）
CONTAINER_SMOKE_ROLE=default make check-container-smoke     # 既定コンテナ
CONTAINER_SMOKE_SKIP_BUILD=1 make check-container-smoke     # 既存イメージを使う（build しない）
bash harness/container-smoke/run.sh --role lite --image fandhe-browser:lite-smoke --task-timeout 20
```

| 環境変数 | 既定 | 対応オプション |
| -------- | ---- | -------------- |
| `CONTAINER_SMOKE_ROLE` | `lite` | `--role`（`lite` / `default`。`docker/Dockerfile.release` の target） |
| `CONTAINER_SMOKE_IMAGE` | `fandhe-browser:<role>` | `--image` |
| `CONTAINER_SMOKE_PLATFORM` | daemon の既定 | `--platform`（`linux/amd64` / `linux/arm64`） |
| `CONTAINER_SMOKE_SKIP_BUILD` | 空（build する） | `--skip-build`（非空で有効。イメージ不在は exit 2） |
| `CONTAINER_SMOKE_TASK_TIMEOUT` | `20` | `--task-timeout`（1〜120 秒） |

`make ci` には含めない（release ビルドと実コンテナ起動を伴う）。CI ジョブも追加しない。他エージェントと共有するタグ（`fandhe-browser:lite`）を上書きしたくない場合は `CONTAINER_SMOKE_IMAGE` で専用タグを指定する。

## 12 タスク（`tasks.json`）

`harness/compat_fixtures/`（本リポ自作のフィクスチャ）を `/fixtures:ro` にマウントして使う。`docs/spec` 配下の PoC-2 フィクスチャは参照・複製しない（内容は同一ではなく、類型の 1:1 対応のみ）。期待値は `crates/fandhe-browser-core/examples/compat_tasks/tasks.rs` の `LOCAL_TASKS` と同値。

| id | PoC-2 類型 | fixture | kind | selector | attr |
| -- | ---------- | ------- | ---- | -------- | ---- |
| C01 | 01 静的記事 | `static/01-article.html` | texts | `article h1, a[rel=author]` | — |
| C02 | 02 表 | `static/02-table.html` | texts | `#prices tbody td.name` | — |
| C03 | 03 ログインフォーム | `static/03-login-form.html` | attr | `form#login` | `action` |
| C04 | 04 商品一覧 | `static/04-product-list.html` | attr | `li.product` | `data-sku` |
| C05 | 05 ナビリンク | `static/05-nav-links.html` | attr | `nav a` | `href` |
| C06 | 06 SSR ブログ一覧 | `ssr_spa_static/01-ssr-blog-list.html` | texts | `main article h2` | — |
| C07 | 07 CSR シェル | `ssr_spa_static/06-csr-shell.html` | text | `title` | — |
| C08 | 08 ダッシュボード表 | `ssr_spa_static/02-dashboard-table.html` | texts | `td[data-col=revenue]` | — |
| C09 | 09 チェックボックス・ラジオ | `ssr_spa_static/03-checkbox-radio-form.html` | text | `form#prefs textarea[name=bio]` | — |
| C10 | 10 画像属性 | `static/06-images.html` | attr | `figure img` | `alt` |
| C11 | 11 Unicode | `static/07-unicode.html` | texts | `p.greeting` | — |
| C12 | 12 壊れた HTML | `static/08-malformed.html` | texts | `ul.items li` | — |

## 呼び出し契約と判定の限界

配布バイナリの `query` サブコマンド（TASK-47.4・#285・#824・`CLI-1`・`PERF-7`）を使う。構文は `run.sh` の `build_task_argv` 1 関数に隔離してある。

- 呼び出し: `query [--all | --attr NAME] --file <コンテナ内パス> -- <selector>`（`text` は無印、`texts` は `--all`、`attr` は `--attr NAME`）
- 出力: JSON 文字列リテラルの 1 値 1 行。`text` / `attr` は先頭一致要素の 1 件のみ、`texts` は全一致要素
- 比較: `jq -s` で配列へ正規化し、空白の連続を 1 個へ畳み前後を除いて比較する（CLI は `textContent` をそのまま返し、期待値は正規化済みのため）。`texts` は期待値全体と完全一致、`text` / `attr` は期待値の先頭 1 件と一致
- **判定の限界**: `attr` は先頭一致要素の属性だけを返すため、期待値が複数の C04・C05・C10 は先頭要素のみの検証になる（全要素の属性取得は CLI 側の機能追加が必要）

## 判定の流れ

1. 入力検証（role・image・platform・timeout・`tasks.json` のスキーマ）→ 不正は exit 2
2. `--skip-build` でなければ `docker build -f docker/Dockerfile.release --target <role>`
3. 構造確認: `docker create` + `docker export | tar -t` で `*.so` / `*.so.*` と `lib/`・`lib64/`・`usr/lib/` 配下の実体を数える（`shared_libs=`）。1 件でもあれば exit 1
4. preflight: 先頭タスクの argv で起動し、終了するか確認する。終了せず `DevTools listening on` で始まる起動行を出していれば（`query --file` 以前の旧バイナリ）`result=blocked`（exit 2）、バナーも無ければ `timeout`（exit 1）
5. 12 タスクを順に実行し 1 タスク 1 行を出力、最後にサマリ

コンテナ実行条件は固定で呼び出し側から変更できない: `--network none`、フィクスチャは `:ro` マウント、`--cap-drop ALL`、`no-new-privileges`、`--read-only` + `/data` を tmpfs（`uid=65532,gid=65532,mode=0700`。実機で起動を確認済み）、非 root（イメージ既定の UID 65532）。`--privileged`・`--init` は使わない。SSRF 防御（`file:` 拒否・private 帯拒否）を回避する経路は作らない。

## 出力契約

stdout は結果行のみ（進捗・ビルドログ・不一致時の実測値〔無害化・300 文字切り詰め〕は stderr）。

```text
container-smoke-env: role=<lite|default> image=<ref> id=<sha256:...> platform=<os/arch> docker_server=<ver> commit=<sha|unknown> dirty=<0|1|unknown> built=<yes|no> shared_libs=<N>
container-smoke: role=<role> id=<C01..C12> fixture=<path> kind=<text|texts|attr> status=<ok|mismatch|nonzero-exit|timeout|invalid-output> exit=<N|none>
container-smoke-summary: role=<role> total=12 executed=<N> success=<N> result=<pass|fail|blocked> reason=<label|none>
```

| exit | 意味 |
| ---- | ---- |
| 0 | 12/12 成功 |
| 1 | 実行したが失敗あり（不一致・非 0 終了・timeout・共有ライブラリ検出・build / inspect の失敗） |
| 2 | 使用エラー・前提不足（引数 / `tasks.json` 不正、docker・jq 不在、daemon 不達、`--skip-build` でイメージ不在、旧バイナリで一発実行サブコマンドが無い） |

## 要判断事項

1. **`attr` の複数要素検証**: 上記のとおり C04・C05・C10 は先頭要素のみ。全要素検証が必要なら CLI への `--all --attr` 相当の追加をオーナーが判断する
2. **ローカルファイル入力**: `query --file` はローカル操作者専用で、URL 経路の SSRF 防御（`file:` 拒否・private 帯拒否）は緩めていない。ハーネスはこの経路だけを使う

## 注意点

- `docker stop` は PID 1 が SIGTERM を無視する（graceful shutdown 無し）ため SIGKILL（exit 137）になる。後始末は `docker rm -f` で行い、137 は後始末の結果として失敗扱いにしない
- `--read-only` だけでは `/data` へ書けず `error: failed to open profile` で exit 1 になるため、`/data` を tmpfs にしている
- フィクスチャ由来のテキストは stdout へ出さない

## 実行結果

### 軽量コンテナ

2026-10-16、`docker/Dockerfile.release --target lite` を HEAD（#1077 取り込み後）から build して実行（amd64）:

```text
$ bash harness/container-smoke/run.sh --role lite --image fandhe-browser:lite-smoke302
container-smoke-env: role=lite image=fandhe-browser:lite-smoke302 id=sha256:c5c2b0d6... platform=linux/amd64 docker_server=29.8.1 commit=fbc392e998af dirty=1 built=yes shared_libs=0
container-smoke: role=lite id=C01 ... status=ok exit=0
  （C02〜C11 も status=ok）
container-smoke: role=lite id=C12 fixture=static/08-malformed.html kind=texts status=ok exit=0
container-smoke-summary: role=lite total=12 executed=12 success=12 result=pass reason=none
(exit 0)
```

- 追加 OS ライブラリ: イメージ内の共有ライブラリ類は 0 件（`shared_libs=0`。scratch）
- 12 タスク: **12/12 成功**（受入基準 1・2 を満たす。ただし attr の複数要素は先頭のみの検証）

### 既定コンテナ

Issue #303（TASK-66.2）で記入する。
