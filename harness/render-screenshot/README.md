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
  --servo-cmd '[
    "servoshell", "--headless", "--window-size", "{width}x{height}",
    "--output", "{out}",
    "--pref", "network_http_proxy_uri={proxy}",
    "--pref", "network_https_proxy_uri={proxy}",
    "--pref", "network_http_no_proxy=",
    "{url}"
  ]'
```

`--pref network_http_proxy_uri={proxy}` / `network_https_proxy_uri={proxy}` は
servoshell（TASK-36 で確定するオプション名に合わせて読み替える想定）に
ローカル転送プロキシを設定する preference で、`--servo-cmd` のテンプレートに
`{proxy}` を含めない限り撮影自体が拒否される（`--allow-unproxied-engine` を
付けない限り。下記「ローカル転送プロキシによる撮影プロセスの全通信フィルタ」
参照）ため、このように必ず含める必要がある。`network_http_no_proxy=` は
loopback 等をプロキシ除外にしないための明示（servoshell の既定値は空文字列
で無条件にプロキシを使うが、preference ファイルで上書きされていないことを
はっきりさせるため明示的に空を渡す）。**これらの preference を実機の
servoshell バイナリが実際に強制することまでは本ハーネスから検証していない**
（TASK-36／#50・#55 の人間による実機検証の範囲。詳細は下記「Servo
（servoshell）のプロキシ対応について」）。

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
| `--allow-unproxied-engine` | コマンドテンプレートが `{proxy}` を使わなくても撮影を許可する（ネットワークを完全に遮断したテスト環境専用。下記「ローカル転送プロキシ」参照） | ― |

`--engines` にカンマ区切りで同じエンジン名を重複指定した場合（例: `servo,servo`）は
`ok` サイト数の集計と `partial` 判定が食い違うため、引数エラー（終了コード 2）として
拒否する。

### テンプレートの placeholder

`{url}` `{out}` `{width}` `{height}` `{settle_ms}` `{html_path}` `{user_data_dir}`
`{chromium_bin}` `{proxy}` が使える。argv の各要素の中で個別に置換するため
（`shell=True` は使わない）、URL 中の空白やシェルのメタ文字があっても 1 つの
argv 要素のまま残る。テンプレートに定義されていない placeholder（未知の
`{...}`）が含まれる場合は実行前にエラーで止まる。

`--dry-run` を付けない実行では、コマンドテンプレートに `{proxy}` を含めない
限り撮影を拒否する（`--allow-unproxied-engine` で明示的に無効化できる。下記
「ローカル転送プロキシによる撮影プロセスの全通信フィルタ」参照）。

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

### ローカル転送プロキシによる撮影プロセスの全通信フィルタ

`_check_public_host`（上記）が検証するのは撮影対象の**最初の URL** だけである。
`{html_path}` で保存したスナップショットが `<base href>` 経由で読み込む外部の
script/img/css・`{url}` 直接ナビゲーションのリダイレクト先・撮影プロセス
（Chromium 等）自身がそれ以降たどる遷移には、この検証は一切効かない。固定 URL
リスト（後述）もページの内容や遷移先までは制限しない（codex P0 再指摘）。

そこで「最初の URL の検証を積み増す」のではなく、**撮影プロセスの通信経路
そのもの**を Python 標準ライブラリだけで書いたローカル転送プロキシ
（`start_filtering_proxy`。HTTP の forward と `CONNECT` に対応）へ強制する。

- `--dry-run` を付けない実行では、`main` がキャプチャループの前に
  `127.0.0.1` の空きポートでプロキシを起動し、`{proxy}` プレースホルダーへ
  `http://127.0.0.1:<port>` を渡す。既定 Chromium テンプレートは
  `--proxy-server={proxy}` と `--proxy-bypass-list=<-loopback>`
  （Chromium が暗黙に持つ loopback 宛のプロキシ除外を無効化し、`127.0.0.1` 等
  への接続もプロキシ経由にする）を含む
- プロキシは宛先ホストを `_resolve_public_addresses`（`_check_public_host` と
  同じグローバルユニキャスト判定）で検証し、**検証で得た IP へ直接接続する**
  （ホスト名を再解決しない。DNS リバインディング対策）。非公開アドレス・
  許可外ポート（80/443 のみ許可）は `CONNECT`・HTTP フォワードのいずれも
  403 で拒否する
- コマンドテンプレートに `{proxy}` を含めない場合、`capture_one` は既定で
  撮影自体を `skipped` にする（fail-closed）。ネットワークを完全に遮断した
  環境やオフラインのテストでのみ `--allow-unproxied-engine` で無効化する
- `{html_path}` 用スナップショット取得（`fetch_snapshot`）自体もこのプロキシを
  必ず経由する（codex P0 再指摘）。従来は `_check_public_host` が事前に
  DNS 解決結果を検証するだけで、実際の接続は `urllib` が改めて名前解決して
  直接行っていたため、検証後に DNS 応答が内部アドレスへ変わる（DNS
  リバインディング）と内部サービスへ到達できてしまっていた。`urllib.request.
  ProxyHandler` で http/https の両方をこのプロキシへ強制することで、検証
  （`_resolve_public_addresses`）と実際の接続を同じ場所（プロキシ）に
  一本化する。https は `CONNECT` になるため、TLS の SNI・`Host` ヘッダは
  元のホスト名のまま維持され、実際に接続する IP だけがプロキシ側の検証済み
  のものに固定される。`fetch_snapshot` のリダイレクト追跡も、ホップごとに
  プロキシが宛先を検証する（`_PublicOnlyRedirectHandler` による事前検査も
  多層防御として残す）。`file:` はプロキシの対象外（`ProxyHandler` は
  http/https のみを差し替える）のため、`--allow-file-url` 時は通常どおり
  ローカルファイルとして読む
- CONNECT トンネルの中継（`_relay`）は両ソケットをブロッキングのまま
  `select` で読み取り可能を待ち、`sendall` もブロッキングで行う（書き込み側
  にもアイドルタイムアウトを設定）。両ソケットを non-blocking にして
  `sendall` していた旧実装は、相手の送信バッファが埋まると
  `BlockingIOError` でトンネルを閉じてしまい、大きな転送が途中で欠落する
  問題があった（Cursor Medium 再指摘）
- 撮影エンジンのプロセスがタイムアウトした場合、直接の子プロセスだけでなく
  子孫（Chromium のレンダラー・GPU プロセス等）もまとめて終了する
  （POSIX: `start_new_session=True` で起動し `os.killpg` + `SIGKILL`。
  Windows: `CREATE_NEW_PROCESS_GROUP` で起動し `taskkill /T /F` でプロセス
  ツリーごと終了）。`subprocess.run(..., timeout=)` は直接の子しか kill
  しないため、子孫が残って以後の撮影のプロキシ接続枠・CPU を奪い合ったり、
  `user_data_dir` 削除と競合したりする問題があった（Cursor Medium 再指摘）。
  子孫の終了後に `user_data_dir` を削除する

**このプロキシで防げないこと（正直に書く。REPAIR-3）**:

- UDP（WebRTC/STUN・QUIC 等）はプロキシを経由しないため制限できない
- `file:` 等プロキシの対象外のスキームは制限できない
- エンジンがプロキシ設定を無視・迂回する実装だった場合は防げない（`{proxy}`
  を含むテンプレートで起動していることは確認できるが、エンジンが実際に
  全通信をそこへ流しているかまでは検証できない）
- プロキシは認証なしで `127.0.0.1` にバインドするため、実行中は同じホスト上の
  他プロセスからも到達できる（開発機でのローカル実行を前提とし、認証・TLS
  終端は実装しない。将来必要になれば追加のユーザー承認事項とする）
- `CONNECT` トンネルの中身（TLS で暗号化された実際の HTTP リクエスト）までは
  検査せず、宛先ホスト・ポートのみをフィルタする
- 同時接続数（既定 64）・アイドルタイムアウト（既定 30 秒）・1 接続あたりの
  転送量上限（既定 256 MiB）は無制限のリソース確保を防ぐための上限であり、
  DoS を完全に防ぐものではない

#### Servo（servoshell）のプロキシ対応について

Servo 本体は `network_http_proxy_uri` / `network_https_proxy_uri` /
`network_http_no_proxy` という preference を持ち（`components/config/prefs.rs`）、
servoshell の `--pref` 引数から設定できる（例:
`--pref network_http_proxy_uri=http://127.0.0.1:PORT --pref
network_https_proxy_uri=http://127.0.0.1:PORT`）。ソースコードの調査では
この preference の存在までは確認できたが、**実機の servoshell バイナリで
実際にこのプロキシ経由の通信が強制されることまでは本ハーネスから検証していない**
（TASK-36／#50・#55 の人間による実機検証の範囲）。そのため `--servo-cmd` の
テンプレートにも他エンジンと同様 `{proxy}` を必須にし（`--allow-unproxied-engine`
無しでは `{proxy}` を含まない `--servo-cmd` は撮影を拒否する）、実際に上記の
`--pref` を使うかはユーザー・実機検証側の判断とする。

### `{url}` 直接ナビゲーションの許可リスト（多層防御）

上記のローカル転送プロキシが撮影プロセスの全通信を検証する一方、`{url}` を
エンジンへ渡すテンプレート（既定 Chromium テンプレート等）には従来からの
追加の防御層として、`capture_screenshots.py` の `DIRECT_NAVIGATION_ALLOWED_URLS`
（`sites.json` の既定サイト URL をそのまま複製した固定リスト）に完全一致する
URL のみへの直接ナビゲーションを許可し、それ以外（`--sites` に差し替えた
任意の URL 等）は `skipped` として拒否する仕組みも残している。**この検証は
`{url}` を含むテンプレートであれば必ず適用する**。同じテンプレートが
`{html_path}` も併用している場合でも（例: エンジンへスナップショットのパスと
元 URL の両方を渡すテンプレート）省略されない（旧実装は `{html_path}` の
有無で分岐していたため、両方の placeholder を持つテンプレートでは許可リスト
検査を素通りできた。codex P0 の再指摘）。任意 URL を撮影したい場合は `{url}`
を使わず `{html_path}` のみのテンプレートを使うこと（取得後にリダイレクト先も
検証する `_PublicOnlyRedirectHandler` を通る）。`sites.json` を更新した場合は
この固定リストも合わせて更新する必要があり、乖離は
`test_default_sites_urls_are_all_allowlisted` で検出される。

保存した HTML はそのままだと `file://` の保存先パス基準で相対 URL が解決され、
元の URL を直接開く Chromium と条件が食い違う。取得したスナップショットには
`<head>` 直後（無ければ先頭）に `<base href="...">` を注入し、相対リンク・
相対リソースの解決基準を揃える（JS が動的に発行するリクエストの起点までは
揃わないため完全な条件一致ではない）。埋め込む URL は `sites.json` の元の
URL ではなく、取得時にリダイレクトを辿った後の最終 URL（`response.geturl()`）
である（Cursor Medium 再指摘: 最終的な本文を保存するのに base href がリダイレクト
前の URL のままだと、相対 URL のリソースが誤った場所を基準に解決される）。
この撮影が「元 URL を直接開いた」ものか「スナップショット経由」だったかは
結果 JSON の `captures[].input`（`"url"` / `"snapshot"`）で判別できる。

### Servo のコマンド例

`--allow-unproxied-engine` を使わない限り、`--servo-cmd` のテンプレートにも
`{proxy}` を含める必要がある（上記「Servo（servoshell）のプロキシ対応に
ついて」参照。servoshell が実際にこれを強制することまでは未検証）。

PoC-6 の `servo-embed`（`{html_path}` を渡す想定。`servo-embed` 自体がプロキシ
設定に対応するかは PoC-6 側の実装次第で、本ハーネスからは確認していない）:

```json
["servo_embed_poc", "{html_path}", "{out}", "--proxy", "{proxy}"]
```

servoshell 系（TASK-36 で確定したオプション名に合わせて読み替える想定。
`--pref` は servoshell が実際に持つ preference 名）:

```json
[
  "servoshell", "--headless", "--window-size", "{width}x{height}",
  "--output", "{out}",
  "--pref", "network_http_proxy_uri={proxy}",
  "--pref", "network_https_proxy_uri={proxy}",
  "{url}"
]
```

### Chromium の既定テンプレート

```json
[
  "{chromium_bin}", "--headless=new", "--disable-gpu", "--hide-scrollbars",
  "--no-first-run", "--user-data-dir={user_data_dir}",
  "--window-size={width},{height}", "--force-device-scale-factor=1",
  "--proxy-server={proxy}", "--proxy-bypass-list=<-loopback>",
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
問題への対策（Cursor Bugbot）。`--proxy-server={proxy}` / `--proxy-bypass-list=<-loopback>`
は撮影プロセスの全通信をローカル転送プロキシへ強制する（上記「ローカル転送
プロキシによる撮影プロセスの全通信フィルタ」参照）。`--chromium-cmd` で独自
テンプレートを使う場合はこれらのフラグを自分で含める必要がある（含めない
場合は `--allow-unproxied-engine` を明示しない限り撮影自体が拒否される）。

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
`input` は `"url"`（テンプレートが `{url}` を直接使う）・`"snapshot"`
（`{html_path}` 経由でスナップショットを使う）・`"snapshot+url"`
（`{html_path}` と `{url}` を両方使うテンプレートで、エンジンへスナップショットの
パスと元 URL の両方が渡っている場合。codex P2: 併用時に `"snapshot"` のまま
にすると実態と食い違い、撮影条件の判別を誤る）のいずれかで、`--dry-run` の
結果では `null`。
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
- 上記はいずれも撮影対象の最初の URL しか見ておらず、撮影プロセス自身が
  以後たどるサブリソース・リダイレクトは防げない。そのため撮影プロセスの
  全通信を、Python 標準ライブラリだけで書いたローカル転送プロキシ
  （`start_filtering_proxy`）へ強制する。宛先を都度 `_resolve_public_addresses`
  で検証し、検証で得た IP へ直接接続する（ホスト名の再解決をしない。DNS
  リバインディング対策）。`{proxy}` を使わないテンプレートでの撮影は既定で
  拒否する（`--allow-unproxied-engine` で明示的にオプトアウト）。`fetch_snapshot`
  自体（`{html_path}` 用スナップショット取得）もこのプロキシを必ず経由し、
  直接 `socket` 接続する経路を残さない（`_ForcedProxyHandler`。https は
  `CONNECT` になるため TLS の SNI・`Host` は元のホスト名のまま維持される）。
  素の `urllib.request.ProxyHandler` は `no_proxy`/`NO_PROXY` 環境変数を見て
  明示的な辞書を渡していてもバイパス（直接接続）してしまうため、`_ForcedProxyHandler`
  でこのバイパスを無視する（CI・開発機の環境変数だけでプロキシ強制が
  無効化される迂回経路を防ぐ）。詳細・限界は上記「ローカル転送プロキシによる
  撮影プロセスの全通信フィルタ」を参照
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
- 撮影済み PNG は IHDR・IEND の CRC 検証に加え、IDAT チャンクが 1 つ以上
  存在し zlib として展開できること・展開後のバイト数が IHDR の幅・高さ・
  色タイプ・ビット深度から計算した期待値と一致することまで確認する
  （`zlib.decompressobj` の Adler-32 検証を含む）。IHDR だけを偽装した
  解凍爆弾を防ぐため、展開前に期待サイズが `MAX_PNG_RAW_BYTES`（256 MiB）を
  超えないことを確認し、展開中も 64 KiB 刻みで期待値超過を検知した時点で
  打ち切る（画素データが存在しない・破損しているファイルを "ok" と誤判定
  しない。codex P1）
- 撮影済み PNG のチャンク構造検証は上記に加え、シグネチャ直後の最初の
  チャンクが IHDR で長さがちょうど 13 バイトであること・IHDR と IEND が
  1 回だけであること・IEND の長さが 0 で最後のチャンクであること・
  パレット画像（色タイプ 3）に必須の PLTE が IDAT より前に正しい長さ・
  エントリ数で存在すること（色タイプ 0・4 では逆に禁止）・IDAT が連続して
  現れること・先頭文字が大文字の未知の critical チャンクを拒否すること
  （ancillary チャンクは無視してよい）・チャンク数が `MAX_PNG_CHUNKS`
  （10000）を超えないこと・展開後データの各走査行のフィルタタイプバイトが
  0〜4 であることまで網羅的に確認する（codex P1）
- `sites.json`（`--sites`）の読み込みは、パース後のサイト件数上限
  （`MAX_SITES`）とは別に、ファイル自体のサイズも `stat` で事前確認し
  （`MAX_SITES_FILE_BYTES`＝1 MiB）、実際の読み込みもその上限 + 1 バイトまでに
  制限する（`stat` と読み込みの間にファイルが差し替えられて大きくなる
  TOCTOU にも対応するため。codex P1。巨大な `--sites` ファイルで
  `json.loads` 前のメモリ消費が無制限になるのを防ぐ）
- `sites.json` の `viewport.width`/`height` は `[1, 10000]`（`MIN_VIEWPORT`〜
  `MAX_VIEWPORT`）の範囲だけでなく、`load_sites` の時点でこの viewport が
  生成しうる PNG の最悪ケース（RGBA・16bit。PNG が許す最大のチャンネル数・
  ビット深度）の展開後サイズが `MAX_PNG_RAW_BYTES`（256 MiB）を超えないことも
  確認する。範囲内でも、例えば 10000x10000 は非圧縮で約 400 MiB になり
  `read_png_size` が必ず `failed` にしてしまうため、load 時点で拒否する
  （正方形の viewport ではおおよそ 5790 角までが実質的な上限になる。既定の
  1280x800 は最悪ケースでも約 7.8 MiB で上限に遠く及ばない。codex P2）
- `--timeout-sec` / `--settle-ms` / `--min-sites` は有限かつ範囲内の値のみを
  受け付け、`nan` / `inf` / 0 以下 / 極端に大きい値は起動時に拒否する
- 撮影用の一時 `--user-data-dir`（Chromium）はテンプレート展開・スナップショット
  取得の失敗を含むあらゆる終了経路で必ず削除する（`try`/`finally`）。撮影が
  タイムアウトした場合は、子孫プロセス（レンダラー・GPU プロセス等）を含めて
  終了させてからこの削除を行う（下記参照）
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
