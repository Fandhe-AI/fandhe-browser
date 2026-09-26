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

保存先（`snapshots/<site_id>.html`・`capture-result.json`）は書き込み前に
`resolve()` して `--out-dir` 配下であることを確認し、既存の symlink があっても
`O_NOFOLLOW`（Windows では `is_symlink()` での事前拒否）でリンクをたどらずに
書き込む。`--out-dir` を使い回す再実行で保存先が外部ファイルへの symlink に
差し替えられていても、そのファイルを取得データで上書きしない。

### `{url}` 直接ナビゲーションの許可リスト

`{url}` をエンジンへ渡すテンプレート（既定 Chromium テンプレート等）では、上記の
SSRF 検証はエンジン起動前の一時点の名前解決に基づくベストエフォートに過ぎず、
別プロセスのブラウザ自身がその後たどるリダイレクト先までは検証できない。そのため
`{url}` を渡すテンプレートは `capture_screenshots.py` の
`DIRECT_NAVIGATION_ALLOWED_URLS`（`sites.json` の既定サイト URL をそのまま複製した
固定リスト）に完全一致する URL のみを許可し、それ以外（`--sites` に差し替えた
任意の URL 等）は `skipped` として拒否する。**この検証は `{url}` を含むテンプレート
であれば必ず適用する**。同じテンプレートが `{html_path}` も併用している場合でも
（例: エンジンへスナップショットのパスと元 URL の両方を渡すテンプレート）省略され
ない（旧実装は `{html_path}` の有無で分岐していたため、両方の placeholder を持つ
テンプレートでは許可リスト検査を素通りできた。codex P0 の再指摘）。任意 URL を
撮影したい場合は `{url}` を使わず `{html_path}` のみのテンプレートを使うこと
（取得後にリダイレクト先も検証する `_PublicOnlyRedirectHandler` を通る）。
`sites.json` を更新した場合はこの固定リストも合わせて更新する必要があり、乖離は
`test_default_sites_urls_are_all_allowlisted` で検出される。Chromium の
`--host-resolver-rules` はホスト名の解決先を固定できるだけで、応答先 IP を
グローバルユニキャストの範囲に強制する機能ではないため、確実な多層防御としては
採用していない。

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
  "--window-size={width},{height}", "--force-device-scale-factor=1",
  "--virtual-time-budget={settle_ms}", "--screenshot={out}", "{url}"
]
```

`--user-data-dir` は撮影ごとに使い捨てのディレクトリを作り、終わったら削除する
（実際のユーザープロファイル・Cookie・キャッシュは使わず、残さない。PROF 系規約）。
コンテナで root 実行する場合に必要な `--no-sandbox` は既定では付けない。必要なら
`--chromium-cmd` で明示的に追加する。UA の上書きや anti-bot 回避のフラグは付けない
（SEC 系: 偽装・回避機能の禁止）。`--force-device-scale-factor=1` は HiDPI ホスト
（既定のデバイススケールが 1 でない環境）で PNG が `--window-size` の DPR 倍の
寸法になり、`capture_one` の viewport 寸法一致検証で全サイトが `failed` になる
問題への対策（Cursor Bugbot）。`--chromium-cmd` で独自テンプレートを使う場合は
このフラグを自分で含める必要がある。

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
| 指定した全エンジンで **同じ site_id が共通して** `ok` になった件数が `--min-sites` 以上 | 0 |
| 上記の共通 `ok` 件数が `--min-sites` 未満 | 1 |
| 引数・サイト一覧・テンプレートの不正 | 2 |

RENDER-5 が求めるのは「同じサイトを両エンジンで撮って比較する」ことなので、
終了コードの判定はエンジンごとに独立した `ok` 件数ではなく、**全エンジンで
共通して `ok` だった site_id の件数**（`_count_common_ok_sites`）を
`--min-sites` と比較する。例えば Servo がサイト 1〜5・Chromium がサイト 2〜6 で
成功した場合、エンジンごとの独立集計ではどちらも 5 件だが、両エンジンで比較
できるのはサイト 2〜5 の 4 件のみであり、`--min-sites 5` は満たさない
（codex P1）。`--engines chromium` のように 1 エンジンだけを指定した実行では、
共通集合はそのエンジン単体の `ok` 件数と一致する（後方互換）。実行のたびに
`per-engine ok counts` と共通件数を標準エラー出力へ表示する。

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
  `--allow-file-url` を明示したときのみ、テスト用途。サイト一覧の検証・
  `{html_path}` 用スナップショット取得の両方で同じフラグを共有する）で、かつ
  IP リテラル・名前解決結果がグローバルユニキャストアドレスであることを検証する
  （ループバック・プライベート・リンクローカル・クラウドメタデータアドレス等への
  SSRF を防ぐ）。この検証は `{html_path}` 経由（`fetch_snapshot`。リダイレクト先にも
  同じ検証を適用）だけでなく、`{url}` を直接エンジンへ渡す直接ナビゲーション
  テンプレート（既定 Chromium テンプレート等）にも適用する。ただし直接ナビゲーション側は
  起動前の一時点の名前解決に基づくベストエフォートで、エンジン（別プロセスの
  ブラウザ）自身がその後たどるリダイレクト先までは検証できない。サイト一覧は
  `docs/design/site-catalog-task70.md`（TASK-70 / COMPAT-3）で 403 や
  robots.txt Disallow により除外されたものを使わない（SSRF・偽装回避の禁止）
- `{url}` を直接エンジンへ渡す直接ナビゲーションは、上記の SSRF 検証に加えて
  `DIRECT_NAVIGATION_ALLOWED_URLS`（`sites.json` の既定サイト URL の固定リスト）
  への完全一致を要求する。任意 URL は `{html_path}` 経由でのみ撮影できる
- 保存先ファイル（`snapshots/<site_id>.html`・`capture-result.json`）は
  `resolve()` 後に `--out-dir` 配下であることを確認し、既存の symlink があっても
  `O_NOFOLLOW`（`_write_bytes_nofollow`。Windows では `is_symlink()` 事前チェック）
  でリンク先ではなく新規ファイルとして書き込む（再実行時の symlink 経由の
  外部ファイル上書き対策）
- 撮影ごとのタイムアウト・スナップショットのサイズ上限・サイト数上限（50）・
  `stderr` 末尾 2000 文字までの保持（子プロセスの標準出力・標準エラーは
  無制限にメモリへは保持せず、`stdout` は破棄し `stderr` はテンポラリファイル
  経由で末尾のみ読む）により、無制限のリソース確保を防ぐ。撮影済み PNG の
  検証も `stat` でのサイズ上限（`MAX_PNG_BYTES`＝64 MiB）確認を先に行い、
  超過分は `read_bytes()` で読まずに `failed` として扱う
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
