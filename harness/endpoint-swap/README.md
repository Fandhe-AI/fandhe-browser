# endpoint-swap（Playwright / Puppeteer スクリプト成功率の再測定）

TASK-46（Issue #252・`CDP-4`・MS-4）の計測用スクリプト群。`compat-practical/tasks.json` の 28 タスクを
Playwright と Puppeteer の API で書いたスクリプトとして実行し、**接続先（エンドポイント）だけを差し替えて**
fandhe-browser と Chromium の両方へ流す。関連: `CDP-2`・`CDP-3`・`MEAS-2`・`SEC-2`・`SEC-3`。

本ディレクトリは**計測の実行・集計まで**を担う。成功率 60% 以上の達成**判定は人間**（TASK-46 の担当）で、
スクリプトは判定しない（率を出すだけ）。CI・`make ci` には組み込まない（手動専用）。

## 構成

| ファイル | 内容 |
| -------- | ---- |
| `swap_lib.mjs` | タスクの成否判定（`executeTask`）・全体期限（`runAll`）・集計（`summarize`）・導入検証（`verifyInstall`）。接続先もライブラリも知らない純粋ロジック |
| `adapters.mjs` | Playwright / Puppeteer のアダプタ。ライブラリ差だけを吸収する。接続先ごとの分岐は無い |
| `run_swap.mjs` | 1 ライブラリ × 1 接続先を実行して JSONL を書く。単体でも任意のループバックエンドポイントに対して使える |
| `summarize.mjs` | JSONL をライブラリ別・接続先別に集計（28 件と 22 件の両方）。入力ごとに meta 行を検証する |
| `verify_install.mjs` | npm の一時導入の版・integrity 検証 |
| `run.sh` | 実行入口。npm の一時導入 → 接続先の起動 → `run_swap.mjs` → 停止 → 集計 |
| `self-test.sh` / `self-test.mjs` | オフライン自己テスト（下記「自己テストの範囲」） |

## 「エンドポイント差し替えのみ」の意味

- 28 タスクの実行コード（`swap_lib.mjs` + `adapters.mjs`）は接続先に依存しない。`--endpoint` と記録用ラベル `--target` だけが異なる。
  fandhe-browser 用の分岐・待機・回避コードは入れない。UA・ヘッダ・フィンガープリントは変更せず、stealth 系プラグインも使わない（`SEC-2`。`self-test.sh` が語の混入を静的に検査する）
- 1 タスクは 1 本の独立したスクリプトとして実行する（接続 → ページ作成 → `goto` → 要素取得 → 切断を毎回やり直す）。1 つの接続が壊れても他タスクへ波及させず、接続段階の失敗をタスクごとに数える
- 両接続先に同じ接続オプションを渡す。これは測定条件であり、README に明記する:
  - Playwright: `connectOverCDP(endpoint, { noDefaults: true })`・`newContext({ acceptDownloads: false, viewport: null })`。`CDP-2`（2026-10-27 オーナー判断・根拠 #943）が適合判定に指定する組み合わせ
  - Puppeteer: `connect({ browserWSEndpoint, defaultViewport: null })`（Playwright の `viewport: null` に相当する公開オプション。既定の 800x600 が送る `Emulation.*` を避ける。`--puppeteer-viewport default` で既定動作に戻せる。`CDP-3` には指定の取り決めが無く、**この選択が「差し替えのみ」の範囲かはオーナー判断が要る**。両方の条件で測れる）
  - `browserWSEndpoint` は `/json/version` の `webSocketDebuggerUrl`（endpoint と同じホスト・ポートの `ws:` のみ受け付ける）から接続のたびに解決する。`--puppeteer-connect url` なら `browserURL` を渡す
- 待機条件は両接続先で同一: `goto(url, { waitUntil: "domcontentloaded" })` の後に `--settle-ms`（既定 2500）待つ（`chromium_reference.mjs` と同じ条件）

## 成功判定（`compat-practical/run_core.mjs` と同じ基準）

| kind | 成功の条件 | 実装 |
| ---- | ---------- | ---- |
| `text` | 先頭一致要素の `textContent` が空白除去後に非空 | `page.$(selector)` → `handle.evaluate(n => n.textContent)` |
| `texts` | 先頭一致 1 件の `textContent` が空白除去後に非空（件数は数えない。`method: "first_match"`・`match_count: null`） | 同上 |
| `form` | 先頭一致要素の子孫に、`name` 属性が非空の `input` / `textarea` / `select` が 1 件以上 | `handle.evaluate(...)` で数える（`match_count` に件数） |

- `reason` の語彙は `run_core.mjs` と同じ: `no_match`（要素なし）・`empty_result`・`timeout`・`fetch_error`（`goto` の例外）・`cdp_error`（それ以外の例外）・`blocked`（全体期限超過で未実行）。本ハーネス固有の追加が `http_error`（`goto` の応答が 4xx・5xx。次項）
- `failed_stage`: `connect` / `newContext`（Playwright のみ）/ `newPage` / `goto` / `selector`。成功は `null`。`reached_stage` は到達した最後の段階
- **HTTP ステータス条件**（両ライブラリ・両接続先に共通。接続先ごとの分岐は無い）: `goto` の応答ステータスを `http_status` に記録し、**4xx・5xx は要素取得の結果にかかわらず失敗**にする（`failed_stage: "goto"`・`reason: "http_error"`・`detail: "HTTP 403"` 等。要素取得には進まない）。403 のエラーページにも `a` などのセレクタと非空テキストがあり得るため、内容だけで成功にしない（a6 の 403・c1 の anti-bot を失敗として数える契約。`SEC-2`・`SEC-3`）
  - **ステータスが取れない場合**（`goto` の応答が `null`。fandhe-browser は現状 `http_status: null`、Playwright は応答を `null` で返すことがある）は「判定不能」として**ステータス条件だけをスキップ**し、要素取得の判定（上の表）は通常どおり行う。`null` を一律失敗にすると、ステータスを返さない fandhe-browser が構造的に不利になる。そこで、判定できない条件だけをスキップして要素判定に委ね、スキップした事実を結果に残して後から数え直せるようにする（成功を水増しせず、一方の接続先だけを不利にもしない）
  - 結果の `http_status_check` に判定を残す: `passed`（100〜599 のうち 400 未満）・`failed`（4xx・5xx）・`skipped_no_status`（取得不能）・`null`（`goto` に到達しなかった）。100〜599 の整数以外の値は取得不能として扱う
  - **残るリスク**: ステータスが取れない接続先では、エラーページ（403 等）が要素判定を通ると成功に数えられる。`summary.txt` / `summary.json` の `status_unchecked_success`（成功のうちステータス条件をスキップした id）が、その成功の中でエラーページ由来が混ざり得る範囲。人間の判定では、この id 群を別掲して確認する（Chromium 側が同じ id で `http_error` なら、fandhe-browser の同 id の成功は疑う）
- 各段階は `--stage-timeout`（既定 30 秒）で打ち切る。全体期限（`--total-timeout`、既定 1800 秒）を超えたら以降は実行せず `blocked`

## 分母と 60% の見方（判定は人間）

- **Playwright と Puppeteer を別々に**判定する。合算しない。接続先（`fandhe-browser` / `chromium`）ごとにも別々の集計になる
- 分母は 2 通りを同時に出す: `set=28`（`tasks.json` の全 28 件）と `set=22`（PoC-1 由来の 22 件 = 28 件から 28 サイト化で追加した c4・d5・d7・d8・e3・e6 を除いたもの。`compat-practical/README.md` の「tasks.json」）
- **a6（HTTP 403）・c1（anti-bot）などの失敗は分母に入れたまま失敗として数える**。回避しない（`SEC-2`）。取得できなかった結果を黙って分母から落とさないよう、`summarize.mjs` は id の欠落・重複・未知 id があると exit 2 で止まる。さらに入力ファイルごとに meta 行を検証し、次のいずれかで exit 2 になる（fail-closed）: meta 行がちょうど 1 行・先頭にない、meta の `tasks_sha256` が集計時に読んだタスクファイル（`--tasks` 省略時は `compat-practical/tasks.json`）のハッシュと不一致（id が同じでも URL・selector・kind を変えた旧定義の結果は受理しない）、`task_count` や結果行数が不一致、結果行の `library` / `target` が meta と不一致、meta / result 以外の行、同一ライブラリで接続先間の `library_version` または `options` が不一致、同一 library × target のファイルが複数
- 参考行 `of_chromium_success`: 同じライブラリで Chromium が成功した id のうち、fandhe-browser も成功した件数。サイト側の事情（時期・403 等）を切り分ける材料で、判定の分母ではない
- `summarize.mjs` の出力は 1 行 1 区分:

```text
library=playwright target=fandhe-browser set=28 success=17/28 rate=60.7%
library=playwright target=fandhe-browser set=22 success=14/22 rate=63.6%
library=playwright target=fandhe-browser failed_stage goto=8,selector=3
library=playwright target=fandhe-browser reason no_match=3,timeout=8
library=playwright target=fandhe-browser failed_ids a6,b5,...
library=playwright target=fandhe-browser of_chromium_success=17/26 rate=65.4% (reference only; not a denominator)
```

（上は書式の例で、実測値ではない）

## 一時導入の作法（承認範囲）

オーナー承認（2026-10-29）: npm の `playwright-core` と `puppeteer-core` を**測定のときだけ一時導入**してよい。
CI への組み込み・`package.json` と lockfile のコミットは承認されていない。

- `run.sh` が `mktemp -d` の作業ディレクトリへ `npm install --no-save --no-audit --no-fund --ignore-scripts <name>@<x.y.z>` で入れ、終了時に削除する。リポジトリには `package.json`・`package-lock.json`・`node_modules` を残さない（`self-test.sh` が不在を検査する）。ブラウザ本体は取得しない（`PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD`・`PUPPETEER_SKIP_DOWNLOAD`）
- 版は `scripts/versions.sh` の正本（`=x.y.z` の完全固定）:

| パッケージ | 版 | 備考 |
| ---------- | -- | ---- |
| `playwright-core` | `1.63.0`（`PLAYWRIGHT_VERSION`・`PLAYWRIGHT_INTEGRITY`） | 他のハーネス（`playwright-connect`・`playwright-trace`・`chromium_reference.mjs`）と共通。推移的依存なし（単独導入を検証） |
| `puppeteer-core` | `25.12.0`（`PUPPETEER_VERSION`・`PUPPETEER_INTEGRITY`） | `docs/design/puppeteer-compat.md` の調査版。推移的依存あり |

- `verify_install.mjs` が最上位パッケージの版と integrity を検証する（不一致は fail-closed）。`puppeteer-core` の推移的依存（`ws`・`chromium-bidi` 等。`^` 範囲）は導入時の解決結果で、`<out-dir>/puppeteer-npm-packages.json` に全パッケージの `name@version` を記録する。測定結果を報告するときはこのファイルを併記する
- 版を上げるときは `scripts/versions.sh` の版と integrity（`npm view <pkg>@<版> dist.integrity`）を一緒に更新する（依存の更新に当たるため承認事項）

## 使い方

前提: node 22 以降（組み込み WebSocket）・npm・ネットワーク。fandhe-browser 側は `cargo build --release -p fandhe-browser-cli` で作ったバイナリ、Chromium 側は手元の Chromium / Chrome の実行ファイルを渡す（取得はしない。`--chromium` には信頼できる配布元の実行ファイルを指定する）。Windows では fandhe-browser の起動を未対応にしている（`run_core.sh` と同じ理由）。

```bash
# 本測定（Playwright / Puppeteer × fandhe-browser / Chromium を一括実行して集計）
bash harness/endpoint-swap/run.sh --out-dir /abs/path/out \
  --bin target/release/fandhe-browser --page-js v8 --page-modules \
  --chromium "/path/to/chromium-or-chrome"

# 数タスクだけ（--only 指定時は完全な id 集合がないため集計は行わない）
bash harness/endpoint-swap/run.sh --out-dir /abs/path/out --only a1,d1 \
  --bin target/release/fandhe-browser --chromium "/path/to/chromium-or-chrome"

# Chromium だけ / Playwright だけ
bash harness/endpoint-swap/run.sh --out-dir /abs/path/out --targets chromium --libraries playwright --chromium "/path/to/chromium"

# 引数の検証のみ（起動・npm 導入なし）
bash harness/endpoint-swap/run.sh --out-dir /abs/path/out --validate-only --targets chromium --chromium "/path/to/chromium"

# 集計だけやり直す
# 引数のファイルがそのまま集計対象になる。出力ディレクトリに別の測定の *.jsonl があるときはグロブを使わず列挙する
node harness/endpoint-swap/summarize.mjs /abs/path/out/playwright-fandhe-browser.jsonl /abs/path/out/playwright-chromium.jsonl
```

- `--bin`: 省略時は `FANDHE_BROWSER_BIN`、なければ `target/release/fandhe-browser`（`compat-practical/lib.sh` の `resolve_bin`）
- `--page-js v8|boa [--page-modules]`: `run_core.sh` と同じ固定文面の設定（`[js] engine`・`execute_page_scripts = true`、`execute_modules = true`）で起動する。指定なしはページ JS オフ（`FANDHE_BROWSER_CONFIG` を外す）。直近の core の実測は `--page-js v8 --page-modules`
- fandhe-browser は固定ポート `127.0.0.1:9333` で起動する。起動前に応答があれば他プロセスを誤計測しないよう中止する。プロファイルは一時ディレクトリへ隔離する。Chromium は `--headless=new` で空きポートに一時プロファイルで起動する（UA は既定のまま）
- 任意の起動済みエンドポイントへ直接流す場合は `node harness/endpoint-swap/run_swap.mjs --library playwright|puppeteer --target <label> --endpoint http://127.0.0.1:<port> --module-dir <導入先> --out /abs/path.jsonl`。`--endpoint` はループバックの `http:` / `ws:`（ポート必須・userinfo なし）に限る
- **集計対象**: `run.sh` は今回のループで生成した結果ファイルだけを `summarize.mjs` に渡す（`--out-dir` を glob しない）。前回の測定の `*.jsonl` が `--out-dir` に残っていても集計には混ざらない（空でない出力ディレクトリは拒否しない）。同名の結果ファイルが残っている組は `run_swap.mjs` が上書きを拒否して exit 2 になる（`summary.json` も同様）。前回分と混ぜたくない場合は新しい `--out-dir` を使う
- 出力（`--out-dir`）: `<library>-<target>.jsonl`（メタ行 1 + 結果 28 行）・`<library>-npm-packages.json`・`summary.txt`・`summary.json`。結果行の `output_sample` はページ由来の非信頼テキスト（300 文字で切り詰め・制御文字除去）のため、**コミットしない**。進捗（stderr）には id・成否・理由コードしか出さない
- 終了コード: `0` 記録完了（失敗タスクがあっても 0）、`1` 実行失敗、`2` 入力・使用エラー

## 自己テストの範囲

```bash
bash harness/endpoint-swap/self-test.sh   # 要 node 22 以降。ネットワーク・npm 不要
```

- 対象: 判定（text / texts / form・各 `failed_stage` と `reason`・HTTP ステータス条件〔403・5xx は要素ありでも失敗、`null` は `skipped_no_status` で要素判定のみ〕・タイムアウト・全体期限）、期限後に完了した接続・コンテキスト・ページの解放（両ライブラリ）、集計（28 件 / 22 件の分母・欠落と重複の拒否）、集計入力の検証（meta 欠落・複数・`tasks_sha256` 不一致・件数・library / target・版 / オプション不一致の拒否。id が同じで定義だけ違う結果の拒否を含む）、`run.sh` が今回生成した分だけを集計すること（偽の `npm`・偽 Chromium で前回の古い JSONL が残るディレクトリを模擬）、導入検証の純関数、エンドポイント検証、`/json/version` からの WebSocket URL 解決（`compat-practical/fake_cdp_server.mjs` を再利用）、`run_swap.mjs` / `summarize.mjs` の CLI の配線（偽の `playwright-core` / `puppeteer-core` を一時ディレクトリに置いて実行）、`run.sh` の引数検証、`SEC-2` の静的検査
- ページ内で評価する関数（`PAGE_FN_*`）は偽 DOM に対して本物の関数のまま実行する
- **対象外（npm を入れないと回せない部分）**: 実 Playwright / Puppeteer の API 挙動（`page.$`・`evaluate`・`goto` の実際の戻り値や例外の形）、実 Chromium・実 fandhe-browser との接続、`run.sh` の npm 導入・接続先の起動・停止。これらは実走（上の使い方）でのみ確認できる
- 偽サーバーは `run_core.mjs` 用で、Playwright / Puppeteer のプロトコル全体は話せない。ライブラリ経由の実接続の検証には使っていない

## 注意

- 実サイトへ出る。対象は `tasks.json` の 28 サイトのみ（除外サイトは `compat-practical/README.md`）。anti-bot のブロックページ・403 は回避せず失敗として記録する（`SEC-2`・`SEC-3`）
- 測定のたびに結果は変わる（サイトの時期変化・Chromium の版・ネットワーク）。報告には実施日・OS・バイナリ（`bin_sha256`）・Chromium の版（メタ行 `browser`）・両ライブラリの版を添える
- 本ディレクトリは Rust のビルド・テストから読まれない（`include_str!` 等なし）
