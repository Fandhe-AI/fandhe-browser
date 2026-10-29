# compat-practical（実測ハーネス）

PoC-9（実用互換性レベルの実測）の 22 サイトに 6 サイトを加えた 28 サイト分のタスク定義と、到達可否の確認手順を
V8 統合後（TASK-30）の fandhe-browser 向けに移植したハーネス。
spec 対応: TASK-71.1・TASK-71.2 / MS-6 / `MEAS-4`（関連 `COMPAT-4`・`JS-2`）。

| 後続 | 内容 |
| ---- | ---- |
| #311（TASK-71.2） | 22 タスク（現在は 28）の実行スクリプト `run_core.sh`（本 README「run_core.sh」節） |
| #312（TASK-71.3） | Chromium 実測との突合・`results/matrix.json` 生成（本 README「make_matrix.sh」節。生成スクリプトに加え、実測から生成した `results/matrix.json` をコミット済みで、回帰ゲートは有効。#920） |
| #315（TASK-72.1） | 類型別動作率集計 `aggregate_by_type.sh`（本 README「aggregate_by_type.sh」節） |
| #316（TASK-72.2） | 類型別の達成判定レポート `type-report.md`（static・spa・form の 70% 達成確認） |
| #313（TASK-71.4） | 測定レポート `REPORT.md`（JS 有効の再計測と b5・d2・d3 の解消確認） |

## 共通実測環境（TASK-89）

TASK-89 / MS-1 / `MEAS-2`（#68）。本ハーネスを、対象サイト群を使う再測定タスクの共通実測環境とする。

- **対象サイト群（確定した除外後リスト）**: `tasks.json` の 28 タスク。PoC-1 の元カタログ 36 サイトから次の 8 件を除いたもの。各サイトの判定は `docs/design/site-catalog-task70.md`、anti-bot 除外の扱いは `docs/design/anti-bot-exclusion.md`
  - 403 ブロック（`SEC-1`）: a8 Medium・b6 Figma
  - robots.txt の Disallow: b7 Airbnb の検索結果・b8 Twitter・c3 Pinterest・c5 Instagram
  - 認証必須による到達不能（401）: c2 Unsplash
  - 採用保留（動的テーブルの公開ビューが未確保）: e4 Airtable。見つかれば追加する
- **到達可否**: `access_check.sh` → `results/access_check.jsonl`（「計測結果」）
- **core の実測と動作率マトリクス**: `run_core.sh` → `results/core_results.jsonl` → `make_matrix.sh` → `results/matrix.json`。Chromium 参照は `reference/chromium_results.json`

| 参照するタスク | 使うもの |
| -------------- | -------- |
| TASK-71（#311・#312・#313） | `run_core.sh`・`make_matrix.sh`・`REPORT.md` |
| TASK-72（#315・#316） | `results/matrix.json` を入力にした `aggregate_by_type.sh`・`type-report.md` |
| TASK-74（#318） | `results/matrix.json` の全体動作率（`harness/compat-regression/check-matrix.sh` が閾値を判定） |
| TASK-46（#252） | `tasks.json` の 28 タスクを対象サイト群とし、Playwright / Puppeteer のスクリプトはエンドポイントの差し替えだけで同じタスクを実行する（接続の基盤は `harness/playwright-connect/`・`harness/puppeteer-connect/`） |

## ファイル

| ファイル | 役割 |
| -------- | ---- |
| `tasks.json` | 28 タスク定義（PoC-9 の 22 件 + 28 サイト化で追加した 6 件） |
| `lib.sh` | 共通関数 `resolve_bin`（実行対象バイナリの解決と検証）・`sha256_of` |
| `make_matrix.sh` | core 実測と Chromium 参照データを `id` で突合し動作率マトリクスを生成（TASK-71.3） |
| `aggregate_by_type.sh` | `results/matrix.json` を類型 (a)〜(e) 別に集計し成功数・全体数・動作率を出力（TASK-72.1。判定はしない） |
| `type-report.md` | 類型別動作率の達成判定と失敗の原因分類（TASK-72.2） |
| `reference/chromium_results.json` | Chromium 参照（PoC-9 の実測の転記 22 件 + `chromium_reference.mjs` で取得した 6 件。出所は「make_matrix.sh」節） |
| `chromium_reference.mjs` | tasks.json の判定を Chromium（playwright-core）で実行し、Chromium 参照データを作る手動専用スクリプト（#68・TASK-89。引数検証のみ `self-test.sh` が確認） |
| `access_check.sh` | tasks.json の検証と、28 サイトへの到達可否の記録（curl） |
| `run_core.sh` | 28 タスクを `fandhe-browser` の CDP サーバーで実行し結果を JSONL へ記録（TASK-71.2） |
| `run_core.mjs` | `run_core.sh` から呼ばれる依存ゼロの CDP クライアント（Node 22 以降） |
| `repeat_core.sh` | `run_core.sh` を 1〜5 回繰り返し、各回の JSONL と集計 `summary.md` を出す（TASK-59。`xos-measure.yml` の入口） |
| `summarize_repeats.sh` | 複数回の結果から 28 件・22 件の成功数と、1 回でも失敗したタスクを Markdown で集計する（TASK-59。判定はしない） |
| `fake_cdp_server.mjs` | 自己テスト用の偽 CDP サーバー（loopback のみ） |
| `self-test.sh` | 上記のオフライン自己テスト（curl はスタブ・CDP は偽サーバー。CI で実行） |
| `js_api_spike/` | ページ内 JS が要求する Web API の計測スパイク（TASK-106。core の example `compat_js_api_spike` としてビルド） |
| `page_script_probe/` | 本番と同じ経路でページ内 script の結果を script 単位に観測する診断プローブ（#964。core の example `compat_page_script_probe` としてビルド） |
| `results/access_check.jsonl` | 到達可否の実測結果（下記「計測結果」） |
| `results/core_results.jsonl` | `run_core.sh --page-js v8 --page-modules` の実測結果（ページ JS・ES module 有効。2026-10-22・#920。`make_matrix.sh` の入力） |
| `results/matrix.json` | `make_matrix.sh` が `core_results.jsonl` から生成したマトリクス（コミット済み・手で編集しない。回帰ゲートの入力。#920） |
| `results/core_results_modules.jsonl` | `run_core.sh --page-js v8 --page-modules` の実測結果（2026-10-19 の ES module 有効の記録。#1119。`make_matrix.sh` の入力ではない） |
| `results/core_results_js_off.jsonl` | 2026-10-11 の対照（ページ JS 無効。`core_results.jsonl` とは測定日・バイナリが異なる） |
| `REPORT.md` | 測定レポート（TASK-71.4） |

## tasks.json

- PoC-9 の `harness/tasks.json` から 22 件をそのまま移植し、28 サイト化（#68・TASK-89・`MEAS-2`。対象の確定は `docs/design/site-catalog-task70.md`）で c4・d5・d7・d8・e3・e6 の 6 件を追加した。
  #312 が PoC-9 の Chromium 実測結果と `id` で突き合わせるため、`id` は変更しない
- スキーマ: JSON 配列。各要素は `{id, cat, url, selector, kind}`
  - `id`・`cat`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`（`cat` は `check-matrix.sh` と同じ規則）
  - `kind`: `text` / `texts` / `form`
  - `url`: `https://` のみ（空白・制御文字不可）
  - `selector`: 空でない文字列
- 除外した 8 サイト（m1・m2: 403、m3・m4・m5・m7: robots.txt、c2〔Unsplash〕: 401、m12〔Airtable〕: 公開ビューなし）は含めない（`MEAS-2`・`SEC-1`・`SEC-3`）
- 追加 6 件の selector は Chromium で描画後の DOM に対して確認した（d7 は core のセレクタサブセットに合わせて `^=` を使用、d8 は `[data-qa="renderer"]`、e3 は iframe 内の表を直接指す URL）。根拠は `REPORT.md`「28 サイト化の実測」
- JS 必須で失敗していた 3 件は b5（Trello）・d2（Saucedemo）・d3（DemoQA）。V8 統合後に解消するかが測定の焦点

## access_check.sh

```bash
bash harness/compat-practical/access_check.sh [--tasks P] [--out P] [--timeout SEC] [--total-timeout SEC] [--bin P] [--validate-only]
```

- 依存は bash・jq・curl のみ。実ネットワークへ出るため CI では実行しない（CI は `self-test.sh` と `--validate-only`）
- `--timeout` は 1〜60 の整数（既定 8。curl 1 回あたりの上限）。`--total-timeout` は 1〜3600 の整数（既定 600）で全タスク合計の期限。超過後のタスクは取得せず `blocked: "total time limit exceeded"`・`curl_exit: 28` で記録を確定する（各 curl の `--max-time` も残り時間で切り詰める）。リダイレクトは自前で 5 回まで追う（`curl -L` は使わない）、https のみ（`--proto '=https' --proto-redir '=https'`）
- UA は偽装しない識別子 `fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)`。anti-bot 回避はしない
- SSRF 対策（SEC 系）: 各ホップで `lib.sh` の `url_check`（https のみ・userinfo なし・ポート 443 のみ・IPv6 リテラル不可・localhost 系/内部ドメイン不可・IP リテラルは公開 IPv4 の厳密な 10 進表記のみ）と、ホスト名の正規形検証（小文字化後 ASCII の DNS ラベルのみ。末尾ドット・連続ドット・パーセントエンコード・IDN・バックスラッシュは拒否し、検証したホスト名と curl の接続先ホスト名を常に一致させて `--resolve` の固定を迂回させない）と、DNS 解決後アドレスの公開判定（DNS 解決にも `--timeout` と全体期限の残り時間の小さい方を上限とし、超過時は `total time limit exceeded` で記録して以降の取得を止める。getent / dscacheutil / Windows は powershell の `[System.Net.Dns]`（ホスト名は環境変数で渡す）で解決できるとき。検証済み IPv4/IPv6 を 1 本のカンマ区切り `--resolve`（IPv4 先頭。curl は同一 `host:port` への複数指定を後勝ちで置換するため単一指定にまとめ、IPv6 に届かない環境でも IPv4 で接続を試みる。curl 7.59 以降が必要。#1276・`MEAS-4`）で固定。解決できない・固定できないホストは取得前に拒否）を行い、curl は `-q --noproxy '*'`（.curlrc・プロキシ無効）で起動し、`remote_ip` も事後検証する。tasks.json は JSON 文書がちょうど 1 個であることを検証する（空ファイル・複数文書は拒否）拒否時は curl を呼ばず（事後検証を除く）`blocked` に理由を記録し `reachable:false`
- Windows の jq が出す CRLF は `lib.sh` の `jq` ラッパーが除去する
- 出力は JSONL。1 行目がメタ行、以降は 1 件 1 行

```json
{"type":"meta","measured_at":"2026-10-03T00:00:00Z","user_agent":"...","timeout_sec":8,"total_timeout_sec":600,"tasks_sha256":"...","bin":null}
{"type":"result","id":"a1","cat":"static","url":"https://...","http_status":200,"reachable":true,"curl_exit":0,"elapsed_ms":1234,"blocked":null}
```

- `elapsed_ms`: curl の `time_total`（リダイレクト追従を含む総所要時間）をミリ秒へ変換した値。取得できない場合は 0
- `blocked`: SSRF 対策で取得を拒否した理由（英語）。拒否していなければ null
- `reachable`: リダイレクトを追った最終応答が 200〜399 かつ `curl_exit == 0`。DNS 失敗・タイムアウト等は `http_status: 0`
- 終了コード: `0` 記録完了（到達不能サイトがあっても 0。到達可否は計測結果でありゲートではない）、`1` 記録の書き込み失敗、`2` 入力・使用エラー（スキーマ違反・引数不正・jq/curl 無し・バイナリ検証失敗）

## 実行対象バイナリの指定

実行対象は cli crate の `fandhe-browser`（`fandhe-browser-core` はライブラリで単体バイナリを持たない）。
`lib.sh` の `resolve_bin` が次の優先順位で解決・検証する。

1. `--bin <path>`
2. 環境変数 `FANDHE_BROWSER_BIN`
3. `<repo>/target/release/fandhe-browser`（Windows は `.exe` も探す）

`resolve_bin` は存在・通常ファイル・実行可能であることだけを確認し、バイナリは起動しない（起動するのは `run_core.sh`）。

## run_core.sh

```bash
bash harness/compat-practical/run_core.sh [--tasks P] [--out P] [--bin P] [--endpoint URL] [--no-spawn] \
  [--page-js v8|boa [--page-modules]] [--log ABS_PATH] \
  [--task-timeout SEC] [--total-timeout SEC] [--startup-timeout SEC] [--windows-localappdata-profile] [--validate-only]
```

- 実行経路は **CDP クライアント方式**。CLI サブコマンド（TASK-47）は未実装で、`fandhe-browser` は引数なしで `127.0.0.1:9333` に CDP サーバーを立てるだけのため、`run_core.mjs` が `Page.navigate` → `DOM.getDocument` → `DOM.querySelector` → `DOM.requestChildNodes` を送って判定する（新規依存なし。Node 22 以降の組み込み WebSocket / fetch のみ）
- 既定はバイナリ（`resolve_bin` の優先順位）を起動して終了時に停止する。固定ポートのため、起動前に endpoint が応答していたら中断する（他プロセスの誤計測防止）。計測用プロファイルは一時ディレクトリへ隔離する（`XDG_DATA_HOME`・`HOME`）。`--no-spawn` は起動済みサーバーへ接続する。Windows のバイナリは `XDG_DATA_HOME`・`HOME` を見ずに `%LOCALAPPDATA%` 配下をプロファイルに使い、上の隔離が効かず実プロファイルに触れる恐れがあるため（起動も未確認）、起動経路は exit 2（`--no-spawn`・`--validate-only` は動く）
- `--endpoint` は `http://127.0.0.1:<port>` のみ（loopback 限定）。バイナリは 9333 固定で待ち受けるため、起動モードでは 9333 以外を拒否する（別ポートは `--no-spawn` のみ）。discovery の `webSocketDebuggerUrl` が同一ホスト・ポートでなければ接続しない
- 期限: `--task-timeout`（既定 45・1〜120。タスク開始時に期限と区分を単調時計で確定し、各 CDP 操作へは残り時間を渡す。全体期限の残りが `--task-timeout` 以下のタスクが期限に達したら `blocked`、それ以外は `timeout`。同値は全体期限を優先）・`--total-timeout`（既定 1200・1〜3600。一度全体期限で `blocked` になったら以降のタスクは実行せず `reason: "blocked"`。タイムアウト以外のエラーは期限と無関係にそのエラーで記録）・`--startup-timeout`（既定 15・1〜60。起動モードでは起動待ち、`--no-spawn` では接続確認の予算。期限切れだけ有界リトライし、接続拒否・2xx 以外は即失敗する。失敗時は `attempts` `elapsed` `last` を出力。#956）
- 判定（PoC-9 の成功基準に対応）: `text` は一致要素の部分木の text が空白除去後に非空、`texts` は **先頭一致 1 件**のテキスト非空（`DOM.querySelectorAll` が未実装のため。`method: "first_match"`・`match_count: null` とし件数は捏造しない）、`form` は部分木に `name` 属性付きの `input`/`textarea`/`select` が 1 件以上（`method: "form_fields"`・`match_count` に件数）
- `reason` の語彙: `fetch_error` / `cdp_error` / `selector_unsupported` / `document_too_large` / `no_match` / `empty_result` / `timeout` / `blocked`。HTTP ステータスは CDP から取れないため記録しない（`access_check.jsonl` と `id` で突合する）。403 のページは本文が確定するため `fetch_error` でなく `no_match` 側に出る
- 出力は JSONL（メタ行 1 行 + タスクごとに 1 行）。`output_sample` はページ由来の非信頼テキストで、300 文字で切り詰め・制御文字を空白へ置換する（進捗出力にはページ由来テキストを出さない）

```json
{"type":"meta","schema_version":1,"measured_at":"...","tasks_sha256":"...","bin":"target/release/fandhe-browser","endpoint":"http://127.0.0.1:9333","cdp_browser":"fandhe-browser/0.1.0","driver":"cdp","page_js_executed":false,"page_js_engine":null,"page_modules_executed":false,"task_timeout_sec":45,"total_timeout_sec":1200,"os":"Linux"}
{"type":"result","id":"b5","cat":"spa","url":"https://trello.com","kind":"text","selector":"h1","success":false,"reason":"no_match","detail":null,"method":"first_match","match_count":null,"elapsed_ms":1234,"output_sample":""}
```

- b5・d2・d3 の個別確認: `jq -c 'select(.type=="result" and (.id=="b5" or .id=="d2" or .id=="d3")) | {id,success,reason}' results/core_results.jsonl`
- 終了コード: `0` 記録完了（失敗タスクがあっても 0。成否は計測結果でありゲートではない）、`1` 書き込み失敗・結果件数/id 集合の不一致、`2` 入力・使用エラー（スキーマ違反・引数不正・jq/node 無し・起動失敗）
- `--windows-localappdata-profile`（Windows の起動モード専用の明示オプトイン。手動 workflow `xos-measure.yml` の `compat-practical-windows` 用）: Windows のバイナリは `XDG_DATA_HOME`・`HOME` を見ないため既定では起動不可（上記）だが、このオプションを付けると `launch --profile-dir %LOCALAPPDATA%\fandhe-ct-<pid>-<乱数>\p`（`XOS-7`・`XOS-10`）で隔離して起動し、終了時にその一時領域だけを消す。設定ファイルのパスは `cygpath -m` で Windows 形式にして渡す。Windows 以外・`--no-spawn` との併用は exit 2。オプションの分岐（`launch --profile-dir` の組み立てと掃除）は偽 `uname`・`cygpath` で `self-test.sh` が模擬確認しているが、実 Windows での起動は手動 workflow の初回実行で確かめる
- 起動したバイナリの停止（`lib.sh` の `stop_spawned`）: Linux・macOS は `kill` → `wait`。Windows（Git Bash）は signal がネイティブ PE プロセスに効かず `wait` が詰まって次の回で 9333 が使用中になるため、Windows の PID（`/proc/<pid>/winpid`、無ければ `ps -p` の WINPID 列）を取って `taskkill /F /T /PID` でツリーごと止め、終了確認は約 10 秒の有界ポーリングにする（止まらなければ警告して諦め、`wait` しない）。この分岐は偽 `uname`・`ps`・`taskkill` を PATH に置いた `self-test.sh` で論理のみ確認しており、**実際の Windows での動作は CI の `workflow_dispatch`（`xos-measure.yml`）でしか確かめられない**
- 結果ファイルは一時ファイルへ書いてから置換する（途中失敗で既存結果を壊さない）。`bin` はリポジトリ配下ならリポ相対で記録する

### 制約（REPAIR-3・JS-2）

ページ内 JS（`JS-4`〜`JS-6`）の実行は**既定オフ**で、`run_core.sh --page-js <v8|boa>` が固定文面の設定（`[js] engine`・`execute_page_scripts = true`）を `FANDHE_BROWSER_CONFIG` で渡して有効にする。
`--page-js` なしの起動は呼び出し元の `FANDHE_BROWSER_CONFIG` を外し、メタ行を `page_js_executed:false`・`page_js_engine:null` にする。有効時は `true`・`"v8"` 等。`--no-spawn` は接続先の設定を確認できないため併用不可（exit 2）。
`page_js_executed` は「実行を有効にして起動した」意味で、ページごとの実行成否ではない。実行上限（1 script 2 秒・ページ 5 秒・`src` 1 MiB / 件・合計 4 MiB）は固定で、設定から変えられない。
`--log <絶対パス>` は起動したバイナリの stdout / stderr を保存する（固定診断行 `cdp: page scripts: ...` の回収用。ページ由来の非信頼データを含みうるためコミットしない）。
b5・d2・d3 の結果は実測のまま記録し、解消を装わない（結果は `REPORT.md`）。
属性演算子 `^=` は TASK-71.6（#760）で core のセレクタサブセットに入った。e1 の再計測と `matrix.json` の更新は #920 で行った（2026-10-22。e1 は依然 `no_match`）。

## repeat_core.sh・summarize_repeats.sh

```bash
bash harness/compat-practical/repeat_core.sh --repeat N --bin P --out-dir DIR [run_core.sh のオプション...]
bash harness/compat-practical/summarize_repeats.sh [--tasks P] [--out-md P] [--total N] [--run N] RESULT.jsonl [[--run N] RESULT.jsonl ...]
```

spec 対応: TASK-59（PoC-1 代表タスクの 3 OS 動作率差）/ `MEAS-4`（関連 `COMPAT-4`）。3 OS の実行は手動 workflow `.github/workflows/xos-measure.yml`（`suite=compat-practical`）が GitHub ホステッドランナーで行う（オーナー承認 2026-10-29）。**成功数の集計までで、OS 間の差の判定は人間が行う**。

- `repeat_core.sh`: `--repeat`（1〜5）回 `run_core.sh --out DIR/core_results_<n>.jsonl` を実行し、`DIR/summary.md` を出す。`--bin`・`--out` 以外の引数は `run_core.sh` へそのまま渡す（`--page-js v8 --page-modules`、Windows は `--windows-localappdata-profile`）。**`--tasks` は拒否する**（exit 2。集計 `summarize_repeats.sh` が `tasks.json` の 28 件・22 件の定義に固定のため、渡して両者がずれるのを避ける。22/28 件固定の入口）。**出力先に前回の成果物（`core_results_*.jsonl`・`summary.md`）が既にあれば拒否する**（exit 2。失敗した回の位置に前回のファイルが残って今回のものと誤認するのを防ぐ。新しい `--out-dir` か空のディレクトリを指定する）。実サイトの応答揺れ・runner のネットワーク差による成否のぶれを見るための繰り返し。いずれかの回が失敗したら終了コード 1（完了した回だけで集計は出し、失敗した回は元の回番号のまま「欠測」行として表に残す）。自己テスト用の `FC_REPEAT_RUN_CORE` は実測では設定しない
- `summarize_repeats.sh`: 回ごとに 28 件（`tasks.json` 全体）と **22 件**（PoC-1 の定義 = 28 件から 28 サイト化で追加した c4・d5・d7・d8・e3・e6 を除いたもの。上記「tasks.json」節）の成功数・動作率を表にし、1 回でも失敗したタスクを cat・22 件に含むか・成功回数・失敗理由つきで並べる。`--run N` で直後のファイルの元の実行番号を明示し（省略時は直前 + 1）、`--total N` で実行した回数を渡すと、結果ファイルの無い回を「欠測」行にする（`repeat_core.sh` が常に指定する。タスク別の成功回数は記録された回数に対する値）。入力検証は fail-closed（メタ行が 1 行、結果の id 集合が `tasks.json` と完全一致、追加 6 件が `tasks.json` にあり差し引いて 22 件になること。違反は終了コード 1）
- 自己テスト: `self-test.sh` が合成データ（28 件分の結果 JSONL）で集計値・失敗時の挙動・引数エラーを確認する（`make check-compat-practical`）

## make_matrix.sh

```bash
bash harness/compat-practical/make_matrix.sh [--tasks P] [--core P] [--chromium P] [--out P] [--validate-only]
```

spec 対応: TASK-71.3 / MS-6 / `COMPAT-4`（関連 `COMPAT-1`・`MEAS-4`・`REPAIR-3`・`REPAIR-8`）。

- 入力: `tasks.json`・`results/core_results.jsonl`（`run_core.sh` の実測）・`reference/chromium_results.json`。依存は bash・jq のみ（ネットワーク・バイナリ起動なし）
- 出力: `results/matrix.json`（既定）。`[{id, cat, fandhe_browser_core, chromium}]` を `tasks.json` の順に 1 要素 1 行で書く。`harness/compat-regression/README.md` のスキーマ契約に従い、そのまま `check-matrix.sh` へ渡せる。ページ由来テキスト・絶対パス・ホスト名は含めない。実測の来歴（実施日・OS・バイナリ）は `core_results.jsonl` のメタ行と本 README に残す
- 入力検証（fail-closed・終了コード 2）: ファイルサイズ 1 MiB 以下、`tasks.json` は `access_check.sh --validate-only` に委譲、core のメタ行がちょうど 1 行で `schema_version` が 1、メタの `tasks_sha256` が現行 `tasks.json` と一致（別のタスク集合に対する実測を混ぜない）、`id` 重複なし、`success` が boolean、Chromium 参照データが単一の JSON 配列、`id` 集合と `cat` が 3 入力で完全一致
- `--validate-only`: 検証と突合までで、ファイルは書かない（CI で実行）
- 終了コード: `0` 生成・検証完了（動作率が低くても 0。判定はゲートの `check-matrix.sh` の役割）、`1` 書き込み失敗、`2` 入力・使用エラー。書き込みは一時ファイル経由の置換で、失敗時に既存の `--out` を壊さない
- 類型の対応（PoC-9 の a〜e）: a=`static`・b=`spa`・c=`lazy`・d=`form`・e=`table`。`id` の頭文字と一致する

## aggregate_by_type.sh

```bash
bash harness/compat-practical/aggregate_by_type.sh [--matrix P] [--key NAME] [--out P]
```

spec 対応: TASK-72.1 / MS-6 / `COMPAT-1`（関連 `COMPAT-4`・`MEAS-4`・`REPAIR-3`）。

- 入力: `results/matrix.json`（既定。`--matrix` で変更）。`--key` は集計する boolean フィールド名（既定 `fandhe_browser_core`。`^[A-Za-z0-9_]+$`）
- 類型の対応: a=`static`・b=`spa`・d=`form`（目標類型）、c=`lazy`・e=`table`（参考値）。表に無い `cat` は `type=-`・`role=unmapped` として末尾に参考出力する。並びは a・b・d・c・e・unmapped・overall
- 出力（stdout・1 行 1 区分）: `type=a cat=static role=target core=<成功>/<全体> rate=<率>% chromium=<成功>/<全体> rate=<率>%`。chromium は `chromium` フィールドを持つ要素だけで数え、1 件も無ければ `n/a`。全体 0 件の類型は `rate=n/a`
- 動作率は整数演算で小数 1 桁に四捨五入した文字列（例 17/22 → `77.3`）
- `--out P`: 指定時のみ集計 JSON（`schema_version`・`key`・`matrix_sha256`・`types`・`overall`）を一時ファイル経由で書く。絶対パス・ホスト名は含めない
- 入力検証（fail-closed・終了コード 2）: サイズ 1 MiB 以下・要素数 10000 以下・単一 JSON 配列・`id` / `cat` の文字種と `id` 重複なし・`--key` と `chromium` が boolean・目標類型 (a)(b)(d) の `cat` が各 1 件以上
- 終了コード: `0` 集計完了（率の高低によらない）、`1` `--out` の書き込み失敗、`2` 入力・使用エラー
- 70% の達成判定はしない（ゲートは `check-matrix.sh`、考察は TASK-72.2 の `type-report.md`）。ハーネスの他スクリプトと同じく bash + jq とし、CI の `self-test.sh` 経路（3 OS）に収めた

### Chromium 参照データの出所

- `reference/chromium_results.json` は PoC-9（`docs/spec/03-poc/practical-compat-level/harness/results/chromium_results.json`、`COMPAT-4`）の生データを次の変換で機械的に写したもの。値は手で書き換えていない。`sample`（ページ由来の第三者テキスト）と `url`（`tasks.json` と重複）は載せない

```bash
jq 'map({id, cat, kind, success, error})' <PoC-9 の chromium_results.json>
```

- 28 サイト化で追加した 6 件（c4・d5・d7・d8・e3・e6）は PoC-9 に生データが無いため、`chromium_reference.mjs` で `tasks.json` と同じ判定（url・selector・kind）を実行して取得した（2026-10-26）。判定は PoC-9 の `run_chromium.mjs` と同一（`domcontentloaded` 後に 2500ms 待機・timeout 15000。`text` は innerText 非空、`texts` は一致件数 1 以上、`form` は配下の `input`/`select`/`textarea` が 1 件以上）。実行環境は playwright-core 1.63.0（`scripts/versions.sh` の固定版。`harness/playwright-connect/run.sh` と同じく `mktemp -d` へ一時導入して `verify-install.mjs` で検証。リポの依存には加えない）と Chrome for Testing 153 の Chromium（ブラウザ本体は取得せず `FANDHE_CHROMIUM_EXECUTABLE` で指定）。UA は変更しない（Chromium 既定。PoC-9 は別 UA を指定していた）。出力は `{id, cat, kind, success, error}` だけで、既存 22 件の値は変えていない
  - 実行例（手動専用・要ネットワーク）: `FANDHE_PLAYWRIGHT_MODULE_DIR=<導入先> FANDHE_CHROMIUM_EXECUTABLE=<chromium> node harness/compat-practical/chromium_reference.mjs --only c4,d5,d7,d8,e3,e6 --out <絶対パス>`。`--probe` は一致件数と先頭一致要素の要約を stderr だけへ出す（ページ由来テキストを含みうるため転記しない）
  - 前提（セキュリティ）: `FANDHE_CHROMIUM_EXECUTABLE`（`executablePath`）には信頼できる配布元から入手した Chromium を指定する。`--module-dir`（`FANDHE_PLAYWRIGHT_MODULE_DIR`）は playwright-core を `require` するため、導入先に対し事前に `harness/playwright-trace/verify-install.mjs`（版・integrity・単独導入の検証。`harness/playwright-connect/run.sh` と同じ手順）を実行してから渡す
  - c4 は Chromium が Cloudflare のチャレンジページで止まり `success:false`（回避しない。`SEC-2`）
- 計測条件の差: Chromium 側は PoC-9 当時の別時期・別 UA の Playwright 実測（`domcontentloaded` 後に待機）で、`texts` は全件一致の件数判定。本リポ側は `run_core.sh` の先頭一致 1 件判定（`method: "first_match"`）。条件が揃った比較ではない
- PoC-9 の表との食い違い（生データをそのまま採用し、判断を混ぜない）:
  - e1: 生データは `success: false`（一致 0 件）。PoC-9 の `matrix.json`・README の表は成功扱い
  - c1: 生データは `success: true`（anti-bot のブロックページ内のリンクに一致した機械判定）。PoC-9 README は実質失敗と注記している
  - spec 側の不整合はオーナー経由で spec リポ側の課題として扱う

### 追加 6 件の出所と robots.txt 確認

- d7・d8・e3 は第三者が公開している form・sheet の ID を使う（所有者の同意の有無は未記録）。出所は `docs/design/site-catalog-task70.md` の記録に従う:
  - d7: Google Forms API 公式ガイドの JSON 例に載るフォーム
  - d8: Typeform 公式 GitHub の embed-demo
  - e3: Google Sheets API 公式クイックスタートの「Class Data」シート（実測では `htmlview` 内の iframe URL を使う）
  - 各ページの具体的な URL（辿ったページ）は未記録
- robots.txt の確認（2026-10-26。`curl` で取得しただけで、UA は偽装していない）:

| id | サイト | 結果 |
| -- | ------ | ---- |
| c4 | Product Hunt | 取得できた。`User-agent: *` の Disallow に `/auth/*`・`/search*`・`/my/*` 等があり、対象 URL（トップ `/`）は該当しない |
| e6 | Kaggle | `https://www.kaggle.com/robots.txt` は HTTP 404（HTML）で、robots.txt は取得できなかった（規則の有無は未確認） |

- c4・e6 以外の追加サイトの robots.txt は未確認

### 実測の突合結果と回帰ゲートの状況（2026-10-07 実測）

| 区分 | fandhe-browser-core | Chromium（PoC-9 生データ） |
| ---- | ------------------- | -------------------------- |
| 全体 | 15/22（68.2%） | 20/22 |
| static（a） | 6/7 | 7/7 |
| spa（b） | 4/5 | 5/5 |
| lazy（c） | 1/2 | 2/2 |
| form（d） | 3/5 | 5/5 |
| table（e） | 1/3 | 1/3 |

- この値を `check-matrix.sh --threshold 70 --categories static,spa,form` に通すと、全体・form が閾値未満で exit 1 になる（static・spa は通過）。以下は履歴（2026-10-07 時点）
- 値は 2026-10-07 の実測（ページ JS 未実行）のもの。2026-10-11 の JS 有効の再計測（`REPORT.md`）でも 15/22 で、成否は変わらなかった。閾値・対象類型を下げてゲートを通すことはしない
- 当時は 15/22（68.2%）が閾値未満のため `matrix.json` をコミットできなかった。2026-10-22 の再計測（下記）で閾値に到達し、コミット済み
- 失敗 7 件の内訳と追跡先:
  - b5・d2・d3（`no_match`）: 2026-10-11 の JS 有効の再計測でも未解消（原因は `REPORT.md`）。扱いは #758（TASK-71.h1・人間判断）で決める
  - e1（`selector_unsupported`）: 属性演算子 `^=` が core のセレクタサブセット外だった（計測時点）。#760（TASK-71.6）で対応済み。再計測は #312
  - a6（`no_match`）: HTTP 403 でコンテンツを取得できない
  - c1（`no_match`）・e5（`no_match`）: 本 PR では原因を未調査（実測のまま記録。解消を装わない。REPAIR-3）

#### 2026-10-22 の再計測と回帰ゲートの有効化（#920）

| 区分 | fandhe-browser-core | Chromium（PoC-9 生データ） |
| ---- | ------------------- | -------------------------- |
| 全体 | 17/22（77.3%） | 20/22 |
| static（a） | 6/7 | 7/7 |
| spa（b） | 4/5 | 5/5 |
| lazy（c）（判定外） | 1/2 | 2/2 |
| form（d） | 5/5 | 5/5 |
| table（e）（判定外） | 1/3 | 1/3 |

- 判定は `check-matrix.sh --threshold 70 --categories static,spa,form`（lazy・table は実測が 70% 未満のため判定対象に含めない。閾値は緩めていない）。全体・static・spa・form がすべて PASS で exit 0
- 実測値をそのまま `results/core_results.jsonl`・`results/matrix.json` にコミットした（手で補正していない。値を整えるための再実行もしていない）。`make_matrix.sh` の再生成結果との一致は `self-test.sh` が検査する
- 失敗 5 件（すべて `no_match`）: a6（HTTP 403）・b5（#964）・c1（anti-bot）・e1・e5
- 再生成: `bash harness/compat-practical/run_core.sh --page-js v8 --page-modules` で再計測してから `bash harness/compat-practical/make_matrix.sh`

## 計測結果

`results/access_check.jsonl` は `access_check.sh` の実測（手書きしていない）。

- 実施日: 2026-10-26（メタ行の `measured_at`。UTC）/ OS: Linux（IPv6 に届かない環境）/ スクリプト: main `8ca44ba`（#1276 の修正後）
- 結果: 28 件中 26 件が到達可（a6 StackOverflow・d4 W3Schools は HTTP 403。2026-10-03 の 22 件の計測と同じ 2 件）。`curl_exit` は全件 0
- 出力形式: メタ行に `total_timeout_sec` を含む現行の形式。`tasks_sha256` は 28 件の `tasks.json` と一致する
- 前回（2026-10-03・22 件・`total_timeout_sec` 追加前の形式）の結果はこの計測で置き換えた
- 到達可否はネットワーク環境と時期で変わる。再計測したら実施日と OS をここへ追記する

### core_results.jsonl（run_core.sh の実測）

`results/core_results.jsonl` は `run_core.sh` の実測（手書きしていない）。

- 実施日: 2026-10-07 / OS: Linux / バイナリ: `cargo build --release -p fandhe-browser-cli`（既定 feature・V8 同梱）
- 結果: 22 件中 15 件が成功
- b5（Trello）・d2（Saucedemo）・d3（DemoQA）はいずれも失敗（`no_match`）。ページ JS が実行されないため V8 による解消は未確認（上記「制約」）
- そのほかの失敗: a6（`no_match`・HTTP 403）・c1（`no_match`）・e1（`selector_unsupported`）・e5（`no_match`）
- `--page-modules`（真偽フラグ。`--page-js` が必須）は設定へ `execute_modules = true` を足し、メタ行に `page_modules_executed` を記録する（ES module 実行。`JS-2`・#1119）。`--page-js` だけの設定はバイト単位で変わらない
- ネットワーク環境と時期で変わる。再計測したら実施日と OS をここへ追記する

### 2026-10-11 の再計測（TASK-71.4）

- 実施日: 2026-10-11 / OS: Linux / バイナリ: 同上（main + `--page-js` 拡張）
- `results/core_results.jsonl`: `--page-js v8`（ページ JS 有効）で 15/22。`results/core_results_js_off.jsonl`: 同日・同一バイナリの対照（無効）で 15/22。成否は全 id で一致
- b5・d2・d3 は 3 件とも未解消。数値・原因・残存失敗の扱いは `REPORT.md` を参照（二重管理しない）
- `results/access_check.jsonl` は今回の環境で一部ホストが `curl_exit 7` となったため更新していない（2026-10-03 のまま）

### 2026-10-22 の再計測（#920）

- 実施日: 2026-10-22 / OS: Linux / バイナリ: `cargo build --release -p fandhe-browser-cli`（main `bd1c8be9`）/ フラグ: `--page-js v8 --page-modules`
- `results/core_results.jsonl` をこの実測で置き換え、17/22。`results/matrix.json` を生成してコミットした。数値と判定は上の「2026-10-22 の再計測と回帰ゲートの有効化（#920）」、詳細は `REPORT.md` を参照

### 2026-10-26 の再計測（28 サイト化。#68）

- 実施日: 2026-10-26 / OS: Linux / バイナリ: `cargo build --release -p fandhe-browser-cli`（main `7d602f6`）/ フラグ: `--page-js v8 --page-modules`
- `results/core_results.jsonl` を 28 件の実測で置き換え、21/28。`results/matrix.json` を `make_matrix.sh` で再生成した。内訳は `REPORT.md`「28 サイト化の実測」、類型別は `type-report.md`
- この時点では `results/access_check.jsonl` を更新しなかった: 実測環境が IPv6 に届かず、`access_check.sh` が dual-stack ホストで `curl_exit 7` となり 28 件中 16 件が到達不能と記録されて実態（`run_core.sh`・Chromium は到達できた）を表さなかったため。原因は `--resolve` の複数指定が後勝ちで IPv4 候補が捨てられていたことで、#1276 で単一のカンマ区切り指定へ修正し、同じ環境で 28 件を取り直した（上の「計測結果」。26/28）

### 2026-10-19 の再計測（#1119）

- 実施日: 2026-10-19 / OS: Linux / ES module 有効（`--page-js v8 --page-modules`）で 15/22。b5・d2・d3 は 3 件とも未解消。数値・原因・次の対応候補は `REPORT.md` の「ES module 有効での再測定（#1119）」を参照（二重管理しない）

## js_api_spike（TASK-106）

b5（Trello）・d2（Saucedemo）・d3（DemoQA）の bundle が要求する Web API を実測し、`JS-5`
（最小 DOM バインディング）の対象範囲を確定する材料を作る計測スパイク。
spec 対応: TASK-106 / MS-6 / `JS-5`（関連 `JS-2`・`JS-6`）。

- 対象ページの HTML と `<script>` を `Fetcher` 経由だけで取得し、**何も注入しない素の JS エンジン**で文書順に評価する。script ごとの最初の未定義 API 名と例外分類を JSON Lines で出す
- ソースは `js_api_spike/`。`crates/fandhe-browser-core/Cargo.toml` の `[[example]] compat_js_api_spike` としてビルドする（新規依存なし）
- 自己テストはオフライン・決定的（`cargo test -p fandhe-browser-core --example compat_js_api_spike`。`cargo test --workspace` にも含まれ、3 OS の CI で走る）。`file:`・内部アドレスの拒否と、独自 HTTP クライアントを持たないことを検査する

### 実行手順（実サイトへ出るため手動実行専用）

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_js_api_spike
url() { jq -r --arg id "$1" '.[] | select(.id==$id) | .url' harness/compat-practical/tasks.json; }
target/release/examples/compat_js_api_spike \
  --target b5="$(url b5)" --target d2="$(url d2)" --target d3="$(url d3)" \
  --engine v8 --out /tmp/js_api_spike.jsonl
```

- 引数: `--target <id>=<https url>`（複数可・id は tasks.json と同じ規則）/ `--engine v8|boa` / `--out <path>` / `--max-scripts 1..=256`（既定 64）/ `--fetch-timeout 1..=60`（既定 15）/ `--total-timeout 1..=3600`（既定 600）/ `--debug-messages`（切り詰めた生メッセージを stderr のみへ）
- 終了コード: 0 = 記録完了（評価失敗はデータ）/ 1 = 書き込み失敗 / 2 = 入力・使用エラー（エンジン未同梱を含む）
- 出力（`schema_version: 1`）: `meta` 1 行、script ごとの `script` 行（`outcome`・`error_kind`・`message_class`・`missing_api`・`fetch_error` など）、サイトごとの `site` 行（`first_missing_api`・`missing_apis`・件数・バイト数）。`site` 行の `scripts_over_limit` / `truncated`（`--max-scripts` 超過で未計測の件数と理由）、非 2xx のページは `page_error=http_status` と数値の `page_status` を併記。相対 script URL は最初の有効な `<base href>` 基準で解決し、`--total-timeout` は取得中・評価前にも適用（超過は `skip_reason=total_time_limit`）
- script の URL・本文・ページテキスト・Cookie・ヘッダ・生の例外メッセージは出力しない（external は同一オリジンかの真偽値のみ）

### 制約（REPAIR-3）

- 評価 1 回あたり 2 秒の上限は変更できない（`JsRuntime::execute` が評価オプションを取らない。#775・#776 が未完了）。setTimeout・イベントループは無い
- 文書の文字コードは UTF-8 lossy 固定。`type=module`・`nomodule`・データブロックは評価せず skip として数える
- DOM 無しの素のエンジンのため、最初の未定義 API で script が止まる。1 script につき最初の 1 件のみ分かる（反復プローブは範囲外）

### 人間が行う残作業

実サイトでの実測と記録（b5・d2・d3 の不足 API 一覧）、`JS-5` の API 範囲確定と #777・#778・#779 との差分整理・追加 issue の要否判断。Trello（b5）は #964 で原因を特定済み（anti-bot・認証ではなく ES module。`REPORT.md`）。結果をコミットするときは API 名と件数だけにし、本文・URL・Cookie は載せない。

## page_script_probe（#964）

b5（Trello）で `h1` が得られない原因を切り分ける診断プローブ。本番の `run_page_scripts` を既定の上限・
`page_script_fetch_options()` で呼び、script 単位の結果を出す。`js_api_spike` と違い shim 導入後の実行結果で、
22 タスクの通し実行では見えない「実行して成功したか・実行対象が無かったか」を区別できる。
spec 対応: TASK-71 / MS-6 / `MEAS-4`・`JS-4`（関連 `JS-6`・`SEC-2`・`REPAIR-3`）。結果は `REPORT.md`。

- ソースは `page_script_probe/`。`crates/fandhe-browser-core/Cargo.toml` の `[[example]] compat_page_script_probe`（新規依存なし）。自己テストはオフライン・決定的（`cargo test -p fandhe-browser-core --example compat_page_script_probe`）
- 実行（実サイトへ出るため手動専用）: `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` のあと `target/release/examples/compat_page_script_probe --target b5=<https url> --engine v8 --out <path>`
- 引数: `--target <id>=<https url>`（1 件）/ `--engine v8|boa`（既定 v8）/ `--out <path>`（既存ファイルは上書きせずエラー）/ `--debug-raw-exception`（別名 `--debug-messages`。切り詰めた生メッセージを stderr のみへ。classic・module 評価失敗・動的 import の評価失敗・タイマー callback・ライフサイクルリスナー・fetch / XHR callback の例外が対象。#1152・#1156・#1172）/ `--modules`（ES module 実行を有効にして収集・実行。#1119）/ `--selector <css>`（件数を数えるセレクタ。既定 `h1`。出力の `selector_initial`・`selector_after` に反映）。実行上限・UA は引数から変えられない
- 終了コード: 0 = 記録完了 / 1 = 書き込み失敗 / 2 = 入力・使用エラー（エンジン未同梱を含む）
- 出力（JSONL）: `meta`・`page`（status・本文バイト数・リダイレクト有無・初期 HTML の `h1` 件数・script の内訳）・`script`（index ごとの outcome と分類）・`run`（abort・ライフサイクル・イベントループ・ページ内 fetch / XHR・ブリッジ診断の件数・実行後の `h1` 件数・動的 import の件数と reject 種別名）
- script の URL・本文・ページテキスト・Cookie・生の例外メッセージは出力しない（src は同一オリジンかの真偽値のみ）
- module 評価失敗（`outcome` が `exception` で `exception_source` が `module_evaluation`。#1133）は、core の `ExceptionSummary` から `error_kind`（`ReferenceError` 等。語彙外は `null`）・`message_class`（`not_defined`・`not_a_function`・`not_a_constructor`・`read_property`・`missing_export`・`cannot_use_in_operator`・`cannot_set_properties`・`instanceof_not_object`・`instanceof_not_callable`・`minified_react_error`・`other`）・`missing_api`（文字種・段数・長さを検証済みの識別子。通らなければ `null`）・`property`（読み取り・設定・`in` 検索の対象になったプロパティ名。同じ検証を通ったものだけで、通らなければ `null`。不足 API 名ではなく、`<kind>:<class>` ラベルには入れない。#1140）・`error_code`（`minified_react_error` のときの `Minified React error #N` の番号 `N`。ASCII 数字 1〜6 桁で、直後が終端・`;`・空白のものだけ。通らなければ `null`。案内 URL・引数は出さず、ラベルにも入れない。classic の例外にも同じ規則で出す。#1145）を出す。動的 import の評価失敗は `run` 行の `dynamic_imports.evaluation_errors` に `<kind>:<class>[:<identifier>]` のラベルで出す。classic の例外は従来どおり生メッセージを分類して出す
- `--debug-raw-exception`（別名 `--debug-messages`）は既定 off の opt-in で、生の例外メッセージを 300 文字に切り詰め、制御文字を除いて **stderr にだけ**出す（行頭に `<id>#<index> classic` / `<id>#<index> module_evaluation` / `<id> dynamic_import#<n>` / `<id> timer_error#<n> <source>` / `<id> listener_error#<n> <event>` / `<id> net_callback_error#<n> <request>` の出どころを付け、各例外は先頭フレームも `... frame` 行で出す。`<id> console_error#<n>` / `console_warn#<n>` の本文行も、Error が渡されたときだけ `... frame` 行を足す。#1183。JSONL の `listener_errors`・`net_callback_errors`・`timer_errors` は `<source>:<kind>:<class>[:<api>]` 形式の要約ラベル）。module 評価失敗の `outcome` は固定文（`module evaluation failed`）のままで、生メッセージは core の `PageRunOptions` の opt-in（`#[doc(hidden)]`・既定 off。`Config`・CLI・CDP からは有効にできない）が有効なときだけ保持され、JSONL・`--out` には出ない。URL・トークンを含み得るため、stderr を `--out` と同じファイルや追跡対象のファイルへリダイレクトしない・コミットしない・REPORT や fixture に転記しない（端末で読むだけにする。`SEC-2`・`REPAIR-4`・#1152）
  - 例: `target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --debug-raw-exception --out /abs/path/d3.jsonl`
