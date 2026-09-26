# render-screenshot: 代表サイトのスクリーンショット取得ハーネス

TASK-37.1（ビヘイビア `RENDER-5`。関連: `MEAS-3`）/ MS-1 対応。

## これは何をするものか

RENDER-5 の判定手順は、代表 5 サイト以上で Servo と Chromium の PNG を撮り、
SSIM 0.90 以上・主要レイアウト要素の境界ボックスの 80% 以上が位置ずれ ±5% 以内かを
比べるというものである。本ディレクトリのスクリプトが受け持つのはそのうち
「両エンジンで同じ条件（viewport・待ち時間）の PNG を揃えて取得する」部分のみで、
比較そのもの（SSIM・境界ボックス算出）と実機での合否判定は別 Issue の範囲になる。

- SSIM・境界ボックスの算出: #54（TASK-37.2）。`measure_ssim.py` が本ディレクトリに
  追加され、`capture-result.json`（下記スキーマ）を入力として読む想定
- Linux 実機での測定と合否判定: #55（TASK-37.h1。人間が担当）

### spec パスからの読み替え

spec（`docs/spec` submodule）のタスク定義は本スクリプトの想定パスとして
`03-poc/rendering-layer-servo/capture_screenshots.py` を挙げているが、そのパスは
private な spec リポ側のものであり、spec-reference 規約により本リポからは
`docs/spec` 配下を編集しない。そのため本リポでは CLAUDE.md が計画するハーネス用
ディレクトリ `harness/` 配下の `harness/render-screenshot/` に配置している。

### TASK-36（#50）との関係

Servo 側のヘッドレス撮影コマンド自体（PoC-6 の `servo-embed` バイナリや
servoshell 等）は TASK-36（#50。Linux 実機での検証、人間が担当）でまだ確定して
いない。そのため本スクリプトは Servo の撮影コマンドを `--servo-cmd` で外から
差し込む形にしてあり、TASK-36 の成果物が決まっていなくても使える。

**実機で Servo・Chromium から実際に PNG が撮れることの確認はこのスクリプトの
範囲外である**（#55 が担当）。このリポジトリのテスト（`test_capture_screenshots.py`）は
`fixtures/fake_engine.py`（偽エンジン）を使ったオフライン結合テストであり、
実機での撮影を検証したものではない。

## 使い方

```bash
python3 harness/render-screenshot/capture_screenshots.py \
  --out-dir /path/to/out \
  --engines servo,chromium \
  --servo-cmd '["servo_embed_poc", "{html_path}", "{out}"]'
```

Chromium 側はローカルに `chromium` / `chromium-browser` / `google-chrome` の
いずれかがあれば自動検出される（`--chromium-bin` で明示指定も可能）。

### CLI 引数

| 引数 | 意味 | 既定 |
| --- | --- | --- |
| `--sites` | サイト一覧 JSON のパス | 同じディレクトリの `sites.json` |
| `--out-dir` | 出力先（必須） | なし |
| `--engines` | 撮影するエンジン（`servo`・`chromium`。カンマ区切り） | `servo,chromium` |
| `--servo-cmd` | Servo 側のコマンドテンプレート（JSON 配列文字列） | なし（servo を撮るなら必須） |
| `--chromium-cmd` | Chromium 側のコマンドテンプレート | 下記の既定テンプレート |
| `--chromium-bin` | Chromium 実行ファイルのパス | `chromium` → `chromium-browser` → `google-chrome` の順に自動検出 |
| `--timeout-sec` | 1 回の撮影のタイムアウト（秒。有限数かつ `(0, 3600]` の範囲のみ許可） | 90 |
| `--settle-ms` | 描画の待ち時間（ミリ秒。`[0, 600000]` の範囲のみ許可） | 5000 |
| `--min-sites` | 最低サイト数（`[1, 50]` の範囲のみ許可） | 5 |
| `--dry-run` | 展開後のコマンドを表示するだけで、実行もファイル作成もしない | ― |
| `--allow-file-url` | `{html_path}` 用スナップショット取得で `file:` を許可する（テスト専用） | ― |

`--engines` にカンマ区切りで同じエンジン名を重複指定した場合（例: `servo,servo`）は
`ok` サイト数の集計と `partial` 判定が食い違うため、引数エラー（終了コード 2）として
拒否する。

### テンプレートの placeholder

`{url}` `{out}` `{width}` `{height}` `{settle_ms}` `{html_path}` `{user_data_dir}`
`{chromium_bin}` が使える。argv の各要素の中で個別に置換するため（`shell=True` は
使わない）、URL 中の空白やシェルのメタ文字があっても 1 つの argv 要素のまま残る。
テンプレートに定義されていない placeholder（未知の `{...}`）が含まれる場合は
実行前にエラーで止まる。

`{html_path}` を使うテンプレートを指定した場合だけ、撮影の直前にその URL の HTML を
1 回取得してスナップショットとして保存する（PoC-6 の `servo-embed` は HTML ファイルしか
受け付けないため、Chromium の `{url}` 直接指定と条件を揃える目的）。取得は https のみ・
`urllib`・タイムアウト 30 秒・上限 5 MiB（超えたら打ち切ってそのサイトを `skipped` にする）・
偽装しない UA（`fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)`）
で行う。`--dry-run` 時はこの取得を行わない（表示用のパスを組み立てるだけ）。

取得先ホストは scheme（https のみ）に加え、IP リテラル・`getaddrinfo` で解決した
すべてのアドレスがグローバルユニキャストであること（ループバック・プライベート・
リンクローカル・`169.254.169.254` 等のクラウドメタデータアドレスでないこと）を
毎回検証する（SSRF 対策）。リダイレクト先にも同じ検証を適用し、内部アドレスや
非 https への転送は追跡しない。DNS 応答は検証後に変わりうる（DNS リバインディング）
ため、これは接続前チェックのベストエフォートであり完全な対策ではない。

保存した HTML はそのままだと `file://` の保存先パス基準で相対 URL が解決され、
元の URL を直接開く Chromium と条件が食い違う。取得したスナップショットには
`<head>` 直後（無ければ先頭）に `<base href="{元の URL}">` を注入し、相対リンク・
相対リソースの解決基準を揃える（JS が動的に発行するリクエストの起点までは
揃わないため完全な条件一致ではない）。この撮影が「元 URL を直接開いた」ものか
「スナップショット経由」だったかは結果 JSON の `captures[].input`（`"url"` /
`"snapshot"`）で判別できる。

### Servo のコマンド例

PoC-6 の `servo-embed`（`{html_path}` を渡す想定）:

```json
["servo_embed_poc", "{html_path}", "{out}"]
```

servoshell 系（TASK-36 で確定したオプション名に合わせて読み替える想定）:

```json
["servoshell", "--headless", "--window-size", "{width}x{height}", "--output", "{out}", "{url}"]
```

### Chromium の既定テンプレート

```json
[
  "{chromium_bin}", "--headless=new", "--disable-gpu", "--hide-scrollbars",
  "--no-first-run", "--user-data-dir={user_data_dir}",
  "--window-size={width},{height}", "--virtual-time-budget={settle_ms}",
  "--screenshot={out}", "{url}"
]
```

`--user-data-dir` は撮影ごとに使い捨てのディレクトリを作り、終わったら削除する
（実際のユーザープロファイル・Cookie・キャッシュは使わず、残さない。PROF 系規約）。
コンテナで root 実行する場合に必要な `--no-sandbox` は既定では付けない。必要なら
`--chromium-cmd` で明示的に追加する。UA の上書きや anti-bot 回避のフラグは付けない
（SEC 系: 偽装・回避機能の禁止）。

## 出力構造

```text
<out-dir>/
  servo/<site_id>.png
  chromium/<site_id>.png
  snapshots/<site_id>.html   # {html_path} を使うエンジンのみ
  capture-result.json
```

## 結果 JSON（`capture-result.json`）のスキーマ

`schema_version: 1`。#54（TASK-37.2）の `measure_ssim.py` が読む入力契約。

```json
{
  "schema_version": 1,
  "generated_at": "2026-09-26T00:00:00Z",
  "viewport": { "width": 1280, "height": 800 },
  "sites": [
    { "id": "a1-wikipedia", "url": "https://...", "category": "static", "catalog_id": "a1" }
  ],
  "captures": [
    {
      "site_id": "a1-wikipedia",
      "engine": "servo",
      "status": "ok",
      "png": "servo/a1-wikipedia.png",
      "width": 1280,
      "height": 800,
      "duration_ms": 1234,
      "exit_code": 0,
      "stderr_tail": "",
      "command": ["servo_embed_poc", "..."],
      "input": "snapshot"
    }
  ],
  "partial": true
}
```

`status` は `ok` / `failed`（終了コード非 0・PNG なし・PNG 不正・PNG 寸法が
`viewport` と不一致・エンジンバイナリ不在等でプロセスを起動できなかった場合を含む）/
`timeout` / `skipped`（`{html_path}` のスナップショット取得に失敗した場合）のいずれか。
`partial` キーは `--engines` で 1 エンジンのみを指定した実行にのみ付く。
`input` は `"url"`（テンプレートが `{url}` を直接使う）または `"snapshot"`
（`{html_path}` 経由でスナップショットを使う）で、`--dry-run` の結果では `null`。
`--out-dir` を使い回す再実行では、実行前に前回の PNG を必ず削除してから撮影する
（プロセスが PNG を出力しなくても前回分の残置ファイルで `ok` 誤判定にならない）。

## 終了コード

| 条件 | 終了コード |
| --- | --- |
| `--engines` に指定した各エンジンで、`ok` になったサイト数が `--min-sites` 以上 | 0 |
| いずれかのエンジンで `ok` のサイト数が `--min-sites` 未満 | 1 |
| 引数・サイト一覧・テンプレートの不正 | 2 |

**受入基準（Issue #53）そのものは「両エンジンでの実行」を前提にしている**。
`--engines chromium` のように 1 エンジンだけを指定した実行が終了コード 0 を返しても、
それは「そのエンジン単体では基準を満たした」という意味であり、結果 JSON には
`"partial": true` が付く。RENDER-5 の判定には両エンジン分の実行が必要である。

## 検証（このリポジトリでの動作確認）

```bash
python3 -m unittest discover -s harness/render-screenshot -p 'test_*.py' -v
```

ネットワークにも実際の Servo・Chromium バイナリにも依存せず、
`fixtures/fake_engine.py`（テスト専用の偽エンジン。最小の PNG を書き出すだけ）を使う。

## セキュリティ上の注意

- `subprocess` には常に argv のリストを渡し `shell=True` は使わない（インジェクション対策）
- `site_id` は `^[a-z0-9][a-z0-9_-]{0,63}$` に限定し、出力パスは `resolve()` 後に
  `--out-dir` 配下であることを確認する（パストラバーサル対策）
- サイト URL・スナップショット取得先はいずれも既定で https のみ（`file:` は
  `--allow-file-url` を明示したときのみ、テスト用途）で、かつ IP リテラル・
  名前解決結果がグローバルユニキャストアドレスであることを検証する（ループバック・
  プライベート・リンクローカル・クラウドメタデータアドレス等への SSRF を防ぐ）。
  リダイレクト先にも同じ検証を適用する。サイト一覧は
  `docs/design/site-catalog-task70.md`（TASK-70 / COMPAT-3）で 403 や
  robots.txt Disallow により除外されたものを使わない（SSRF・偽装回避の禁止）
- 撮影ごとのタイムアウト・スナップショットのサイズ上限・サイト数上限（50）・
  `stderr` 末尾 2000 文字までの保持（子プロセスの標準出力・標準エラーは
  無制限にメモリへは保持せず、`stdout` は破棄し `stderr` はテンポラリファイル
  経由で末尾のみ読む）により、無制限のリソース確保を防ぐ
- `--timeout-sec` / `--settle-ms` / `--min-sites` は有限かつ範囲内の値のみを
  受け付け、`nan` / `inf` / 0 以下 / 極端に大きい値は起動時に拒否する
- 撮影用の一時 `--user-data-dir`（Chromium）はテンプレート展開・スナップショット
  取得の失敗を含むあらゆる終了経路で必ず削除する（`try`/`finally`）
- 撮影した PNG の寸法は要求した `viewport` と一致することを確認し、不一致は
  `failed` として扱う（エンジンが異なるサイズで撮ってしまう取り違えの検出）
- エンジンバイナリが存在しない・実行権限がない等でプロセス自体を起動できない
  場合も例外を伝播させず `failed` として記録し、他サイト・他エンジンの撮影と
  `capture-result.json` の書き出しを継続する

## スコープ外・申し送り

- 実機（Linux）での Servo・Chromium の撮影と合否判定: #55（人間が担当）
- SSIM・境界ボックスの算出: #54（TASK-37.2）
- `make ci` / `.github/workflows/ci.yml` への本ハーネスのテスト組み込み: 未実施
  （後続候補として PR に記載）
