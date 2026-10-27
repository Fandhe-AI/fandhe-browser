# Linux 実機での SSIM 測定 実行手順書（#55）

TASK-37（37.h1）/ MS-1 / ビヘイビア `RENDER-5`・`MEAS-3` 対応。Issue #55 の
実機作業（担当: 人間）の手順書である。**この文書は手順の草案であり、測定結果ではない**。
実機での撮影・測定・合否判定はオーナーが行い、結果は #55 に記録する。Issue #1281 で、
現行の Servo ホスト（servo 0.6.0）と隔離コンテナを前提に書き直した。

**撮影は「保存済みページを `docker run --network none` のコンテナで撮る」方式で行う**
（オーナー判断 2026-10-27・#55）。

- 現行の `fandhe-browser-servo-host` は proxy 系の Preferences を空に固定しており、
  かつての検証プロキシ経由（`--proxy` 対応の `servo-embed`）の手順は成り立たない。
  **接続下で実サイトを撮らない**。
- `docker run --network none` のコンテナは、旧 RUNBOOK 5.1 の再開条件（エンジンから外部への
  通信を事前に遮断する隔離）を満たすものとして認める。オフラインで保存したページで
  `RENDER-5` を測ってよい。
- cli・ハーネスに loopback を許す経路は作らない（#1003 とまとめて決める）。それまでは、
  コンテナ内の `/etc/hosts` の別名（`pages.test`）を使う運用とする（5.2）。
- Chromium は計測用コンテナに版を固定して入れる。ホストには入れない。

ハーネスの仕様（引数・スキーマ・セキュリティ上の注意）は [README.md](README.md) が
正本で、本書は「実機で何をどの順に実行するか」だけを扱う。

## 1. 目的と合否基準

`RENDER-5` の判定手順に従い、代表 5 サイト以上で Servo と Chromium の PNG を撮って比べる。

| 指標 | 合格基準 |
| ---- | -------- |
| SSIM | サイトごとに 0.90 以上 |
| 境界ボックス | 主要レイアウト要素の 80% 以上が、Chromium 側の位置から ±5%（viewport 寸法比）以内 |
| サイト数 | SSIM と境界ボックスの両方を満たすサイトが 5 以上（`--min-sites 5`） |

`measure_ssim.py` の `verdict` は閾値との機械的な比較に過ぎない。最終合否は、
本書の記録テンプレートを埋めたうえで #55 に記載する（README の「±5%・80% の解釈」も確認する）。

## 2. 前提

| 項目 | 内容 |
| ---- | ---- |
| OS | Linux x86_64（Docker が動くこと。共用機のホストには Chromium・Servo の依存を入れない） |
| ツール | Docker（BuildKit）、`python3`（標準ライブラリのみで動作。コンテナの外で `--dry-run` と `measure_ssim.py` を実行するため） |
| GPU | なし。コンテナ内の Mesa llvmpipe（ソフトウェア GL）で描画する。フォント差などで SSIM が下がり得る。コンテナは両エンジンで同じフォント（`fonts-dejavu-core`）を使う |
| ネットワーク | イメージのビルド時だけ必要（apt・crates.io・V8 の prebuilt 等）。**撮影は `--network none` で行い、実サイトへは接続しない** |
| ディスク | イメージ内で Servo ホストと cli を release ビルドする。`target/` 相当が数 GB 加わる（Servo ホストの release ビルドは長時間かかる） |
| 保存済みページ | 5 サイト以上を `<pages-root>/<site_id>/index.html`（とサブリソース）として用意する（5.1 の 1） |

撮影前に `make test-harness`（または
`python3 -B -m unittest discover -s harness/render-screenshot -p 'test_*.py' -v`）
を実行し、ハーネス自体が実機の Python で動くことを確認する（偽エンジンによるオフライン
テストであり、実機撮影の検証ではない）。

## 3. Chromium（コンテナ内に版を固定して導入）

Chromium は計測用コンテナの `runtime` ステージが導入する。Debian trixie の
`snapshot.debian.org` の固定時刻（`ARG DEBIAN_SNAPSHOT`）から `chromium=<exact version>`
（`ARG CHROMIUM_VERSION`）を `apt` で入れ、ビルド時に `chromium --version` と照合して
食い違えばビルドを失敗させる（apt の署名検証は外さない）。版の記録方法:

```bash
docker run --rm --entrypoint chromium fandhe-render-screenshot:local --version
```

保存済みページ用の既定テンプレート（`SAVED_PAGES_CHROMIUM_TEMPLATE`）は、検証プロキシ向けの
引数を持たず、`--no-sandbox` を明示で含む。コンテナ内では非 root かつユーザー名前空間が
使えずサンドボックスが起動できないためで、根拠は `--network none`・非 root・`--cap-drop ALL`・
ページを読み取り専用でマウントする隔離運用にある（既存の `DEFAULT_CHROMIUM_TEMPLATE` は変えない）。
UA の上書きや anti-bot 回避のフラグは付けない（SEC 系）。

## 4. Servo 側（servo-host + cli の release ビルド）

### 4.1 背景（#50 の経緯。過去の記録）

TASK-36（#50）の実機検証は PoC の `servo-embed`（Servo 0.3.0）で行った。改変なしでは
8 回すべて 60 秒でタイムアウトし、`paint()` を呼ぶ診断用コピーでは 7/7 回、1280x800 の PNG を
取得できた。**これは過去の経緯であり、現行の手順ではない**。現在は本体の `fandhe-browser`（cli）が
`fandhe-browser-servo-host`（servo 0.6.0。`servo-host/`）を子プロセスとして起動して撮る。

### 4.2 ビルドと配置

コンテナの `build` ステージが、ホストと同じ基盤（Debian trixie）で次を release ビルドする
（glibc を揃えるため、別ホストでビルドしたバイナリを持ち込まない）。

```bash
cargo build --release --locked --manifest-path servo-host/Cargo.toml
cargo build --release --locked -p fandhe-browser-cli
```

- ホストの解決順は、環境変数 `FANDHE_BROWSER_RENDER_HOST`、次に cli と同じディレクトリ。
  コンテナでは両バイナリを `/opt/fandhe/bin/` に並べて置く。
- ホスト単体の確認: `fandhe-browser-servo-host --selftest`。
- cli はホストの環境変数を `env_clear` して起動する。そのため `LIBGL_ALWAYS_SOFTWARE`・
  `GALLIUM_DRIVER`・`EGL_PLATFORM` や `xvfb-run` の指定は**届かない**ので使わない
  （旧手順の GL 環境変数は廃止）。GL の経路（llvmpipe かどうか）は、撮影中のホストの
  `/proc/<pid>/maps` で確認して 7 章に記録する。
- Servo 本体は改変しない（licensing.md）。イメージは配布しない。

### 4.3 cli `screenshot` の入出力仕様

```bash
fandhe-browser screenshot [--profile-dir D] <URL> <OUTPUT>
```

- URL は `http` / `https` のみ。IP リテラルと `localhost` の loopback は拒否する。
  DNS 名の解決先は検査しない（既知の制約。`servo-host-design.md` §2.5・§13）。
- 出力は 1280x800 固定。`sites.saved-pages.json` の viewport（1280x800）と一致する。
- 上書きは拒否する（`create_new`）。ハーネスは撮影前に出力先を消すので問題にならない。
- 両エンジンに**同じ URL**（`http://pages.test:<port>/<site_id>/index.html`）を渡す。旧方式の
  「Servo はスナップショット HTML・Chromium は URL 直接」という入力条件の不一致は解消され、
  結果 JSON の `captures[].input` は両エンジンとも `"url"` になる。
  ただし保存の欠け（5.1 の 1）、JS の挙動、フォント、取得時刻差は残るので 7 章に記録する。

## 5. 撮影

### 5.1 手順（保存済みページ・`--network none` コンテナ）

1. **ページを保存する（人間）**。保存の方法と基準は人間が決める（robots.txt・429 などで欠けた
   場合の扱いを含む）。`<pages-root>/<site_id>/index.html` と、そのサブリソースを同じ
   ディレクトリ配下に置く。サイト ID は `sites.saved-pages.json`（`sites.json` と同じ
   `id`・`category`・`catalog_id`）に合わせる。**欠けたリソース**（取得できなかった URL、JS が
   組み立てる URL）を記録しておく。保存ページはコミットしない。
2. **イメージをビルドする**（リポジトリのルートで）。

   ```bash
   docker build -f harness/render-screenshot/container/Dockerfile \
     -t fandhe-render-screenshot:local .
   ```

3. **展開結果を確認する**（`--dry-run`。コンテナの外でもよい。隔離確認とファイル作成はしない）。

   ```bash
   python3 harness/render-screenshot/capture_screenshots.py --saved-pages \
     --out-dir "$(mktemp -d)" \
     --servo-cmd '["fandhe-browser","screenshot","{url}","{out}"]' --dry-run
   ```

4. **隔離コンテナで実行する**。

   ```bash
   docker run --rm --network none \
     --add-host pages.test:127.0.0.1 \
     --read-only --tmpfs /tmp --tmpfs /home/measure:uid=1000,gid=1000,mode=0700 \
     --cap-drop ALL --security-opt no-new-privileges \
     --memory 4g --pids-limit 512 --user measure \
     -v "$PAGES_ROOT":/pages:ro -v "$OUT":/out \
     fandhe-render-screenshot:local
   ```

   `OUT` は `measure` ユーザー（uid 1000）が書き込める空のディレクトリ。`/home/measure` の tmpfs は
   既定で root 所有になり HOME に書けないため、`uid`・`gid` を付ける（#1289）。entrypoint
   （`container/run-in-container.sh`）は loopback の配信サーバー、隔離確認、撮影の順に実行する。
   `--network none` のもとで `--add-host` が `/etc/hosts` に反映されない環境では、隔離確認の
   `pages_host` 項目が fail-closed で検出して止まる。

5. **結果を確認する**。`$OUT/capture-result.json` の `isolation.isolated == true` と各
   `isolation.checks`（`interfaces`・`external_dns`・`external_ipv4`・`external_ipv6`・
   `pages_host`・`loopback_pages`）がすべて ok であること。`captures[].status` と `stderr_tail`
   で失敗要因を確認する。`$OUT/isolation-report.json` にも同じ確認の記録が残る。

`egress_guard` はこのモードでは使わない（検証プロキシを起動しないため `proxy` キーも付かない）。
隔離はコンテナのネットワーク名前空間の遮断を正とする。隔離確認に通らなければ、エンジンを
1 つも起動せず終了コード 2 になる（記録として `captures: []` の結果 JSON を残す）。保存済みページの欠落で `--min-sites` を満たせない場合も
同様に終了コード 2 と `captures: []` の結果 JSON で、再利用した `--out-dir` の古い結果は上書きされる。
異常があるサイトは失敗として記録し、接続下に切り替えて撮り直さない。

### 5.2 `/etc/hosts` の別名の運用と制約

`--add-host pages.test:127.0.0.1` で別名を置き、両エンジンに `http://pages.test:<port>/...` を
渡す。cli は IP リテラルと `localhost` の loopback を拒否するが、DNS 名の解決先は検査しない。
この運用が通るのは**その既知の制約に依存しているため**で、隔離したコンテナの中でのみ使う。
Issue #1003（出口の通信制御）の設計で cli が解決先を検査するようになれば、この運用は使えなくなる。
そのときは本節と `--saved-pages` の URL 規則を見直す。cli・ハーネスに loopback を許す経路は
作らない（#1003 とまとめて決める）。

### 5.3 主な引数

正本は README の「CLI 引数」。

| 引数 | 用途 | 既定 |
| ---- | ---- | ---- |
| `--saved-pages` | 保存済みページモード（`pages.test` の http のみ許可・検証プロキシなし・撮影前に隔離確認） | 無効 |
| `--allow-unproxied-engine` | `--saved-pages`（dry-run 以外）では必須。隔離が唯一の出口制御であることの利用者による明示 | 無効 |
| `--out-dir` | 出力先（必須）。`servo/` `chromium/` と `capture-result.json` を作る | なし |
| `--servo-cmd` | Servo 側コマンドテンプレート（JSON 配列文字列。`{url}` `{out}` 等。`{proxy}`・`{html_path}` は不可） | なし（必須） |
| `--chromium-bin` / `--chromium-cmd` | Chromium の実行ファイル / テンプレートの上書き | 自動検出 / 保存済みページ用テンプレート |
| `--timeout-sec` | 撮影 1 回あたりのタイムアウト（保存済みページモードでも有効） | 90 |
| `--settle-ms` | 描画の待ち時間（保存済みページモードでは両エンジンとも無効。Servo 側の cli `screenshot` には待ち時間の指定口がなく、load 完了と最初のフレームを待つため、Chromium も `--virtual-time-budget` を付けず同条件にしている） | 5000 |
| `--min-sites` | 両エンジンで共通して `ok` になるべき最低サイト数 | 5 |

終了コードは、両エンジンで共通して `ok` のサイト数が `--min-sites` 以上なら 0、未満なら 1、
引数不正・隔離確認の不合格は 2。

## 6. SSIM・境界ボックスの算出（`measure_ssim.py`）

```bash
python3 harness/render-screenshot/measure_ssim.py \
  --capture-dir "$OUT" \
  --min-sites 5
```

`$OUT/measure-result.json` にサイトごとの SSIM 値と境界ボックス一致率が出る。終了コードは
基準を満たすサイトが `--min-sites` 以上なら 0、未満なら 1、入力不正なら 2。

SSIM は Chromium を reference、Servo を target とした 7x7 一様窓の mean SSIM で、
他ツール（scikit-image 等）の値とは一致しない可能性がある（README「SSIM の算出パラメータ」）。

### 6.1 境界ボックスの入力

`measure_ssim.py` は境界ボックスの**抽出**を行わない。エンジンごとに
`$OUT/<engine>/<site_id>.bboxes.json`（`png_sha256`・`elements` を持つ。契約は README の
「境界ボックスの入力契約」）を用意する必要があり、無いサイトは `not_measured` で不合格になる。

- 1 サイトあたり主要要素 5〜10 件（Chromium 側が 5 件未満だと不合格）。要素の選定基準は未定。
- Chromium 側は `chromium_bbox_driver.py`（Issue #1288）が、`--remote-debugging-pipe` の CDP で
  PNG と `bboxes.json` を書く。使い方・信頼境界は README「`chromium_bbox_driver.py`」。座標は Servo と同じ
  JS 関数式（`querySelector` の最初の一致・`getBoundingClientRect`・ビューポート相対の CSS px）で取る。
  偽 Chromium でのオフライン検証のみで、**実機（実 Chromium・実コンテナ）では未検証**。
  オーナー判断（2026-10-27）: (1) 撮影時点は load（`Page.navigate` の `loaderId` に紐付く `Page.lifecycleEvent`）-> `document.fonts.ready` -> rAF 1 回の後、
  (2) `BBOX_SELECTORS` 指定時は Chromium の SSIM 基準 PNG も CDP `Page.captureScreenshot` で撮る
  （`png_sha256` の契約に合わせ bbox と同じセッションで撮る）、(3) 対象要素は #55 のコメントで採用したセレクタ。
- Servo 側は `servo_bbox_driver.py`（Issue #1282）が、ホストの `bounding_box` を stdio で呼んで
  PNG と `bboxes.json` を書く。使い方・セレクタ書式・信頼境界は README「`servo_bbox_driver.py`」。
  偽ホストでのオフライン検証のみで、**実機（実 Servo ホスト）では未検証**。
  - コンテナ: セレクタファイルを読み取り専用でマウントし、`BBOX_SELECTORS` に**コンテナ内のパス**を渡す。
    `SERVO_CMD` が cli `screenshot` から駆動スクリプトに切り替わり、Servo の PNG も駆動スクリプトが撮る
    （`png_sha256` で bbox と PNG を対応付けるため）。Chromium 側も `--chromium-cmd` が駆動スクリプトに
    切り替わる（`--screenshot` ではなく CDP の PNG になる）。例: `-v "$SELECTORS":/selectors.json:ro -e BBOX_SELECTORS=/selectors.json`
  - コンテナ外: `capture_screenshots.py` の `--servo-cmd` に駆動スクリプトのテンプレートを渡す（README の例）。
  - 対象要素（セレクタ）の選定は #55 でオーナーが決める。実際のリストはコミットしない。

`BBOX_SELECTORS` を使わない実行や、`site_id` がセレクタ入力に無いサイトは、境界ボックスを「未測定」として
記録し、SSIM のみを先に測定する（合格を装わない。REPAIR-3）。

## 7. 結果記録テンプレート

Issue #55 へ貼る。数値は `measure-result.json` から転記する。

```markdown
## 測定環境

| 項目 | 値 |
| ---- | -- |
| 測定日 | YYYY-MM-DD |
| OS / カーネル | |
| CPU / メモリ | |
| GL | llvmpipe（Mesa x.y.z。ホストの /proc/<pid>/maps で確認した経路） |
| Chromium | バージョン（`chromium --version`）/ DEBIAN_SNAPSHOT / CHROMIUM_VERSION |
| Servo | servo-host（servo 0.6.0）+ cli の release ビルド / リポジトリのコミット |
| イメージ | ダイジェスト（`docker image inspect --format '{{.Id}}'`） |
| ハーネス | コミット（git rev-parse --short HEAD） |
| 実行コマンド | docker run の引数一式 / EXTRA_ARGS / measure_ssim.py の引数 |
| viewport / settle | 1280x800 / 5000ms |

## 隔離

- `isolation.isolated`: true / false
- `isolation.checks`（interfaces・external_dns・external_ipv4・external_ipv6・pages_host・loopback_pages）: 結果

## 保存したページ

- 保存の方法と日付:
- 欠けたリソース（robots.txt・429・JS が組み立てる URL など）:

## 結果

| site_id | 撮影(servo) | 撮影(chromium) | SSIM | bbox 一致 (matched/total) | bbox 率 | 合格 |
| ------- | ----------- | -------------- | ---- | ------------------------- | ------- | ---- |
| a1-wikipedia | ok | ok | 0.00 | 0/0 | 0% | 否 |
| ... | | | | | | |

- 両基準を満たしたサイト数: N / 5（基準 5 以上）
- 入力条件の差（両エンジンとも同じ URL。保存の欠け・JS・フォント・取得時刻差の影響）: 記述
- 失敗・skipped のサイトと stderr_tail の要点: 記述
- 境界ボックスの取得方法と要素選定: 記述（未測定の場合はその旨）
- Servo PNG の取得経路: cli `screenshot` / `servo_bbox_driver.py`（bbox を取る場合は後者）
- Chromium PNG の取得経路: `--screenshot` / CDP `Page.captureScreenshot`（`chromium_bbox_driver.py`。bbox を取る場合は後者）
- セレクタファイルの SHA-256（`sha256sum`）: 記述（bbox を取った場合）

## 判定

RENDER-5: 達成 / 未達 / 一部のみ測定（理由）
```

## 8. 判定後の分岐

| 結果 | 次の対応 |
| ---- | -------- |
| 達成（5 サイト以上で両基準） | 結果を #55 に記録して受け入れ条件を満たす。TASK-40（#57）は「不要」と記録する方向（判断はオーナー） |
| 未達 | #55 に未達と原因（フォント差・入力条件差・描画差など）を記録し、#57（TASK-40: レンダリング層 MVP スコープの代替方針）へ進む。Blitz の追加評価（#56 の評価コメント参照）を含む |
| 一部のみ測定（境界ボックス未取得など） | 測定できた範囲と未測定の理由を記録し、残りの扱いをオーナーが決める |
