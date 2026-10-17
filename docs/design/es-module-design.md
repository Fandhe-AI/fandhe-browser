# ES module（`type=module`）スクリプト実行対応の設計

対応 `JS-4`・`JS-6`・`JS-8`・`COMPAT-1` / TASK-71 の残課題 / `MS-6`（spec に個別 TASK なし）/ Issue #918・#964・#309（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 位置づけ

- 本書は設計案であり、**実装は §9 の分割案で別 issue として進める**。本書の時点でコードは変更していない
- `JS-4` の現行挙動（`type=module` は実行せず `skipped_module` 診断を出す）は、実装が入るまで変わらない
- オーナー判断（2026-10-12）により ES module 対応を進める。spec `04-behavior/js-engine.md`「ページ内 JS 実行の設計制約」節の「ES module の扱い」の方針は次の 3 点
  - 実装の前に設計する（本書）
  - import の取得は既存 `Fetcher` 経由に限る（決定 4・`JS-8`）
  - 件数・サイズの上限は `JS-6` を適用する
- b5・d2・d3 が module により解消する見込みは**推測**であり、実装後の再測定（§9 の 11）まで未検証である（REPAIR-3）
- 「オーナー判断待ち」と書いた箇所は、推奨案を置いたうえで決定を保留している

## 1. 対象ページの調査結果と最小対応範囲

`harness/compat-practical/REPORT.md` で JS 必須として残った b5（Trello）・d2（Saucedemo）・d3（DemoQA）の
HTML と entry を、2026-10-17 に curl で取得して確認した（`<script>` タグと、`import`・`import(`・`import.meta`・`export` の出現数・サイズのみを見た。取得物はコミットしていない。実装者は再取得して確認する）。

| id | module entry | オリジン | サイズ | 静的 import | 動的 `import()` | `import.meta` | 備考 |
| -- | ------------ | -------- | ------ | ----------- | --------------- | ------------- | ---- |
| d2 | `/assets/index-*.js`（`type=module crossorigin`）＋ inline classic 1 件 | 同一 | 約 554 KB | なし（単一 bundle。末尾に `export {...}`） | 3 箇所（Vite の遅延 chunk） | 3 箇所 | Vite ビルド |
| d3 | `/assets/index-*.js`（`type=module crossorigin`）のみ | 同一 | 約 921 KB | なし | 11 箇所（jQuery 系などの遅延 chunk） | 0 | Vite ビルド |
| b5 | runtime（約 2 KB）＋ main の 2 件 | **別オリジン**（Atlassian の CDN） | main は 3,051,573 バイト | なし（Parcel の bundle） | main に 1 箇所 | runtime に 1 箇所 | 応答は `access-control-allow-origin` がページのオリジンに限定・`nosniff`・`text/javascript` |

- top-level await（TLA）の有無は、minify 済みのソースから判定できなかった。評価結果が Promise になる場合を前提にする
- import map と `modulepreload` は 3 件とも見当たらない
- 末尾に `export` 文があるため、classic として評価すると SyntaxError になる。最小対応でも「モジュールとして評価する API」が要る

### 最小対応範囲

1. モジュールとしての評価（strict mode・`export` 構文・トップレベルの `this === undefined`）
2. 静的 import のグラフ解決（3 件は深さ 0 だが、一般のサイトで必要）
3. `import.meta.url`
4. 動的 `import()`（d2・d3 の遅延 chunk。form の描画に必須かは未確認）
5. 評価結果が Promise の場合（TLA を含む）の扱い

範囲外（初版）: import map、`modulepreload`、JSON / CSS module（import attributes）、Worker module、`data:` / `blob:` からの import、bare specifier の解決（失敗させる）。

## 2. 対応しても解消しない場合の見込み

- **d2・d3**: module を実行できても、React 系 bundle が必要とする DOM / Web API が shim に不足して失敗する可能性がある（`JS-5` の範囲。REPORT のスパイクは素のエンジンでの値）。解消は §9 の 11 の再測定まで未検証
- **b5** は次の 2 点で阻まれる。どちらもオーナーまたは spec の判断事項で、判断が出るまで b5 は解消しない見込みとする
  - (a) 別オリジンからの module 取得の方針（§6）
  - (b) main が 3.05 MB で、`src` 1 件の上限 `DEFAULT_MAX_SRC_SCRIPT_BYTES`（1 MiB）と js crate の `MAX_SCRIPT_SOURCE_BYTES`（1 MiB）を超え、合計上限 `DEFAULT_MAX_TOTAL_SCRIPT_BYTES`（4 MiB）にも迫る。上限を上げる場合は、V8 の `Script::compile` が watchdog で中断されない点（実測は 1 MiB まで。`v8_engine.rs` の doc）を再評価する必要がある

## 3. `JsEngine` トレイトの拡張

現状の `JsEngine`（`crates/fandhe-browser-js/src/engine_trait.rs`）は `evaluate_script`・`inject_global_function`・`bind_dom_like_object` の 3 メソッドで、object-safe・`Send`/`Sync` 境界なし。V8・boa は子プロセスのワーカーで動く。

**推奨案: 段階的に呼ぶ同期 API で、core 側の async な取得を呼び出しの間に挟む。**

- 子プロセスは取得できず、`NativeFn`（同期）から async の `Fetcher` を待つこともできない。既存の NATIVE_CALL の逆方向 RPC で取得を挟む案は、親の async と子の同期待ちが噛み合わずデッドロックの余地が増えるため採らない
- 追加メソッド案（js crate の型だけで表し、object-safe を保つ）

| メソッド | 役割 |
| -------- | ---- |
| `compile_module(url, source) -> Result<ModuleRecord, JsEngineError>` | 子側のモジュールマップに登録し、`ModuleId` と生の specifier 列（`ModuleRequest`）を返す |
| `link_module(referrer, resolutions)` | 親が解決した specifier から `ModuleId` への対応表を渡す。未解決が残る状態の評価は失敗させる |
| `evaluate_module(root, options) -> Result<ModuleEvaluation, JsEngineError>` | instantiate と evaluate を行い、microtask checkpoint の後に `Fulfilled` / `Pending` / `Rejected(message)` を返す |
| `take_dynamic_import_requests()` | 保留中の動的 import（`id`・referrer・specifier）を取り出す |
| `finish_dynamic_import(id, Result<ModuleId, String>)` | 親が取得・compile した結果で保留 Promise を解決する |

- 戻り値の型は `#[non_exhaustive]` にする（REPAIR-4）。エラーは既存の `JsEngineError` に `ModuleResolutionFailed` などを足す
- 既定実装は **`Err`（未対応）を返す**。fake エンジンなど既存実装を壊さず、未対応のエンジンが成功を装わない（REPAIR-3）
- 子の再起動でモジュールマップは失われる（"context was discarded"）。再登録はせず、スクリプトが作った状態と同じ扱いにする

## 4. ワーカープロトコルの変更

- `PROTOCOL_VERSION`（現在 5。`worker_protocol.rs`）を 6 に上げ、世代差はハンドシェイクで検出する
- 新しい tag の案（親から子）: `COMPILE_MODULE`（→ `MODULE_COMPILED`: id と requests）・`LINK_MODULE`・`EVALUATE_MODULE`（timeout・`max_result_bytes` のヘッダ付き → `MODULE_RESULT` か ERROR）・`TAKE_DYNAMIC_IMPORTS`・`FINISH_DYNAMIC_IMPORT`
- 検証（アロケーション前に実施）: 方向ごとのフレーム長上限、1 モジュールの request 数上限（例 256）、specifier の長さ上限、子側のモジュール数上限、未知・重複 id、不正な UTF-8。違反はプロトコル違反として扱い子を終了する（既存方針）
- compile と evaluate に、既存の watchdog と親の応答待ち期限をそれぞれ適用する

## 5. エンジン別の対応

以下の API 名は事前の想定であり、**実装 issue で v8 `=152.2.0`・boa_engine `=0.22.0` の docs で確認してから使う**（未確認の API を確定扱いにしない）。

- **V8**: `script_compiler` によるモジュール compile → モジュール request の取得 → 子側のマップで同期に解決する resolve callback を使った instantiate → evaluate（Promise）→ microtask checkpoint。動的 import は host import callback で保留キューに積み、`import.meta` は host の import.meta 初期化 callback で `url` を設定する。現状 V8 では microtask checkpoint を行っていない（REPAIR-3）ため、導入が必要
- **boa**: `Module::parse` と、子側のマップを返す独自の `ModuleLoader` で load / link / evaluate し、Promise は `run_jobs` で drain する。boa は現状 job を drain していない（`page_runner.rs` の残課題・TASK-30）。動的 import と TLA は、drain 導入が先か、当面 `NotYetImplemented` を返すかを選ぶ。**推奨は後者**（成功を装わない。REPAIR-3）。macOS で boa を無効にしている挙動は変えない
- 新規依存は不要の見込み。必要になった場合は [dependency-policy](../../.claude/rules/dependency-policy.md) に従い承認 issue を先行させる

## 6. import 解決と取得（`JS-8`）

- 解決は core 側で行う。specifier を referrer の URL（inline の場合は文書の base URL）に対して `Url::join` で解決する。相対・絶対 URL のみ受け付け、bare specifier は失敗させる
- 取得は `Fetcher::validate_url` の後に、ランナーが所有する `Fetcher` の `get_with_max_body` で行う。別経路の HTTP クライアントは作らない。scheme の検査・内部アドレスの拒否・リダイレクト先の検査は `Fetcher` に任せる（SSRF 対策）。`file:`・`data:`・`blob:` は受け付けない
- Cookie・認証情報は送らない（既存と同じ）
- MIME: module の応答は JavaScript MIME type essence に限る（fail-closed。ブラウザと同じ厳格さ）。classic の `src` は現状未検査なので、差異として記録する
- 重複排除: 解決後の URL をキーにモジュールマップを持つ。循環 import は同じ id を返して許容する
- URL・クエリ（トークンを含み得る）は診断やログに残さず、失敗の種別だけを残す

### 別オリジンの方針（オーナー判断待ち）

現状、classic の `src` は別オリジンでも取得している（オリジン検査なし）一方、ページ内 fetch / XHR は `NetRejectKind::CrossOrigin` で reject する（`page_net.rs`。CORS 実装までの暫定。`SEC-2`）。

| 案 | 内容 | 評価 |
| -- | ---- | ---- |
| (a) | 同一オリジンのみ | ページ内 fetch と揃い最も安全。b5 は解消しない |
| (b) | 応答の `Access-Control-Allow-Origin` がページのオリジンまたは `*` のときだけ許可。資格情報なし・preflight なし | b5 に必要。`Origin` ヘッダの扱いを決める必要がある（偽装しない。SEC-2） |
| (c) | classic の `src` と同様に無条件で許可 | 既存と一貫するが module の仕様（CORS 必須）から外れる |

**推奨: 初版は (a)**。b5 の解消には (b) が要る旨と `Origin` ヘッダの扱いを spec 側の課題とする（§10）。

## 7. 上限（`JS-6`）

- **件数**: ページ単位の総数に静的・動的の両方を含める。classic と同じ `max_scripts`（`DEFAULT_MAX_PAGE_SCRIPTS = 64`）の枠で数えるか、新設の `max_modules` を設けるかは実装 issue で決める（推奨は後者で既定 64 程度）
- **バイト数**: 1 件は `DEFAULT_MAX_SRC_SCRIPT_BYTES` を受信中に強制、合計は `DEFAULT_MAX_TOTAL_SCRIPT_BYTES` に module も含める。判定はアロケーション前
- **グラフ**: 深さ上限（例 32）と 1 モジュールの request 数上限（§4）
- **時間**: 取得待ちはページの残り実時間で打ち切り、評価にはスクリプト単位とページ全体の実時間上限を課す
- **超過時**: fail-closed。既存の `AbortKind`（`ScriptCount`・`WallTime`・`ResourceLimit` 等）に対応づけ、グラフ単位で失敗させるか以降を打ち切るかを実装 issue で決める。診断には URL を残さない

## 8. core 側の挙動

- `page_script.rs`: gate 有効時は module を `ScriptKind::Module` として収集し、`nomodule` 付き classic は skip する（新診断種別、例 `skipped_nomodule`）。無効時は現状の skip と診断を維持する（現状は `nomodule` 付き classic を収集対象に含めている）
- gate: 新設定（例 `[js] execute_modules`、既定 off）を設け、既存挙動と AISNAP / トークン削減の測定（`AISNAP-1`・`PLUG-4`）を変えない。`Page.navigate` のオプションと設定への配線は §9 の 10
- 実行順: module は defer 相当。現行の「async / defer を文書順で近似」に合わせ、初版は「文書順のまま」か「classic の後に文書順」かを実装 issue で選び、差異を REPAIR-3 として記載する
- 動的 import はイベントループ段で、ページ内 fetch と同様に 1 件ずつ取得してから `finish_dynamic_import` を呼び、その後に microtask checkpoint を行う
- module の評価は完了値を持たないため、classic のような `non_scalar_result` の問題は起きない。結果は `Fulfilled` / `Pending` / `Rejected` を `ScriptOutcome` に対応づける

## 9. 実装 issue の分割案（各 2h 以下・依存順）

| # | タイトル案 | 依存 |
| - | ---------- | ---- |
| 1 | `feat(js): JsEngine にモジュール評価の API と型を追加（既定は Err）` | なし |
| 2 | `feat(js): ワーカープロトコル v6 にモジュール用フレームのコーデックを追加` | 1 |
| 3 | `feat(js): V8 ワーカーで静的 import のグラフをコンパイル・リンク・評価` | 1・2 |
| 4 | `feat(js): V8 ワーカーで import.meta.url と Promise 状態の取得を実装` | 3 |
| 5 | `feat(js): V8 ワーカーで動的 import の保留キューと完了を実装` | 3 |
| 6 | `feat(js): boa ワーカーのモジュール評価（動的 import・TLA は未対応を返す）` | 1・2 |
| 7 | `feat(core): page_script で module を収集し nomodule を skip（gate 付き）` | 1 |
| 8 | `feat(core): page_runner で module グラフを Fetcher 経由で取得し上限と MIME を検査` | 3・7 |
| 9 | `feat(core): イベントループで動的 import を取得して完了させる` | 5・8 |
| 10 | `feat(cli): ES module 実行の設定 gate を設定ファイルと Page.navigate に配線` | 8 |
| 11 | `test(harness): b5・d2・d3 を module 有効で再測定し REPORT に記録` | 9・10 |

実装 issue の起票は本書のレビュー後に行う。依存の追加はいずれも不要の見込み。

## 10. spec 側の課題（spec リポ側で管理。本リポでは編集しない）

- ES module の対応範囲を定める新ビヘイビア（例 `JS-9`）と、TASK・MS への割り当てが無い
- `JS-4` の「`type=module` はスキップ」の文言を、gate 有効時の挙動に合わせて改める必要がある
- `JS-6` の具体値: module の件数とグラフ深さの上限、b5 のための `src` 1 件上限（1 MiB）見直しの要否
- `JS-8`: 別オリジン module の方針（§6 の a / b / c）と `Origin` ヘッダの扱い
- `JS-2` の期待（b5・d2・d3 の解消確認）と、本書の見込み（§2）の関係

## セキュリティ観点（OWASP Top 10）

| 観点 | 対応 |
| ---- | ---- |
| SSRF（A10） | specifier は untrusted。解決後に必ず `Fetcher` で scheme・内部アドレス・リダイレクト先を検査する |
| アクセス制御（A01） | 別オリジンは既定で拒否。Cookie・認証情報は送らない |
| 不安全な設計（A04） | グラフの件数・深さ・合計バイト数・1 件のバイト数・request 数・specifier 長・フレーム長をアロケーション前に検査。取得と評価に実時間上限。違反は fail-closed |
| インジェクション（A03） | specifier や URL を評価文字列やシェルに連結しない。ソースは compile API に直接渡す |
| 脆弱な依存（A06） | 新規依存なしの見込み。必要なら承認制 |
| 偽装の禁止（SEC 系） | `Origin` 等を偽装しない。未実装は未対応エラーと診断で正直に示す |
| 情報漏えい | 診断・ログに URL を残さず、失敗の種別のみ |
