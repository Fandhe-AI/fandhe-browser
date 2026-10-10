# Playwright 互換: newPage() 未到達原因の調査記録

- **対応ビヘイビア**: CDP-2（関連: CDP-6・SEC-2・REPAIR-3。`docs/spec/04-behavior/` が SSOT）
- **対応タスク**: TASK-43（サブ 43.2・Issue #247）。入力は TASK-43.1（Issue #246）のトレース
- **マイルストーン**: MS-4
- **基準コミット**: `fe3ac01`（`origin/main`）
- **調査日**: 2026-10-05
- **調査対象の版**: playwright-core 1.63.0（`Makefile` の `PLAYWRIGHT_VERSION` と同一）・node v24.21.0

## 目的・前提

`chromium.connectOverCDP()` → `newContext()` → `newPage()` が fandhe-browser の CDP サーバーで
到達しない原因を、CDP メソッド・応答フィールド・イベント順序の単位で記録する。
本書は原因の記録までで、方針の決定（TASK-43.h1・#248）と追加実装（TASK-43.3・#249）は含めない。

根拠は確度の異なる 3 種類があり、不足箇所ごとに種別を付ける。

| 種別 | 内容 | 確度 |
| ---- | ---- | ---- |
| A（実測） | `harness/playwright-trace/results/newpage-trace.jsonl` の `seq` を引用 | 実測済み |
| B（ソース読解） | npm 公開物 playwright-core 1.63.0 の `lib/coreBundle.js`（バンドル済み）の該当関数を読んだ結果 | 実測していない（`REPAIR-3`）。ソース上そう読める、という記録 |
| C（二次情報） | spec の PoC-5（`docs/spec/03-poc/cdp-compat/`）に基づくとされる記述 | 本調査環境では `docs/spec` を取得できず再確認していない |

Playwright のソースは長く引用せず、関数名と要約に留める。B の関数名は `lib/coreBundle.js` 内のもの
（`urlToWSEndpoint`・`CRBrowser.connect`・`CRBrowser.doCreateNewContext`・`CRBrowserContext.doCreateNewPage`・
`CRBrowser._onAttachedToTarget`・`FrameSession._initialize` 等）。

## 到達段階のまとめ

| 段階 | 現状 | 根拠 |
| ---- | ---- | ---- |
| `connectOverCDP(http://…)` | 失敗。`/json/version/` は 200（TASK-43.3a で解消）。`Browser.getVersion` は成功し、次の `Target.setAutoAttach`・`Browser.setDownloadBehavior` が `-32601` | A（再取得後）: seq 1〜9 |
| `connectOverCDP(ws://…)` | 失敗。`Browser.getVersion` は成功し（TASK-43.3a で解消）、次の `Target.setAutoAttach`・`Browser.setDownloadBehavior` が `-32601` | A（再取得後）: seq 10〜16 |
| `newContext()` | 未到達（トレースに該当メッセージなし） | A + B |
| `newPage()` | 未到達（同上） | A + B |

`-32601` は未実装メソッドへ成功を捏造しない `CDP-6`・`SEC-2` の方針どおりの挙動で、バグではない。

## 不足箇所一覧

Playwright が接続から `newPage()` 完了までに必要とする項目を、到達順に並べる。
「現状の応答」は `crates/fandhe-browser-cdp/src/` の現実装（組込みハンドラは `Page.navigate`・`DOM.getDocument`・
`DOM.querySelector`・`DOM.requestChildNodes` のみ。`protocol.rs` の `builtin_handlers`）に基づく。

| # | 段階 | Playwright の箇所 | 送る CDP メソッド / HTTP | 読む応答フィールド・待つイベント | 現状の応答 | 不足 | 種別 |
| - | ---- | ----------------- | ------------------------ | -------------------------------- | ---------- | ---- | ---- |
| 1 | http 接続 | `urlToWSEndpoint` | `GET <base>/json/version/`（パス末尾に必ず `/` を付与） | `webSocketDebuggerUrl`（JSON 本体） | `/json/version` のみルータ登録（`server.rs` の `router`）。末尾スラッシュ付きは 404 | `/json/version/` の受理。応答本体は `webSocketDebuggerUrl` を既に含む | A（seq 2）・B |
| 2 | ws 接続直後 | `CRBrowser.connect` | `Browser.getVersion`（root session） | `product`（`/` より後を版として解析）・`userAgent`（`Headless` を含むかで headful 判定）・`revision` | `-32601` | ハンドラ自体が無い。応答フィールドの値の方針は下記「制約との交差」 | A（seq 4〜5）・B |
| 3 | ws 接続直後 | `CRBrowser.connect` | `Target.setAutoAttach` `{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}`（root session） | 応答は空オブジェクトで足りるとソース上読める。以後の `Target.attachedToTarget` イベントを購読 | `-32601`（未到達） | ハンドラと、自動 attach の実体（`TargetRegistry::attach` は CDP から未接続） | B |
| 4 | ws 接続直後 | `CRBrowser.connect`（`connectOverCDP` は persistent 扱いの options で呼ぶ） | `Target.getTargetInfo`（引数なし）→ `Browser.setDownloadBehavior`（`browserContextId` なしの既定 context 向け。`downloadPath`・`eventsEnabled`） | `Target.getTargetInfo` の応答は読み捨て。既存ページは `Target.attachedToTarget` で受け、全ページの初期化完了を待つ | `-32601`（未到達） | 2 メソッドのハンドラ。既存ターゲットの attach イベント（`TargetRegistry` はターゲットを持つがイベント送出と未接続） | B |
| 5 | `newContext()` | `CRBrowser.doCreateNewContext` | `Target.createBrowserContext` `{disposeOnDetach:true, proxyServer?, proxyBypassList?}` | `browserContextId`（文字列。以降の全ターゲットの `targetInfo.browserContextId` と一致させる） | `-32601` | ハンドラと browser context の概念（`TargetInfo` は `target_id`・`kind`・`url` のみで `browserContextId` を持たない） | B |
| 6 | `newContext()` | `CRBrowserContext.initialize` | `Browser.setDownloadBehavior` `{behavior, browserContextId, downloadPath, eventsEnabled:true}`（`behavior` は acceptDownloads の値により `allowAndName` または `deny`） | 結果は使わない（失敗すると `newContext()` が失敗） | `-32601` | #4 と同じハンドラ | B |
| 7 | `newPage()` | `CRBrowserContext.doCreateNewPage` | `Target.createTarget` `{url:"about:blank", browserContextId}` | 応答の `targetId`。応答直後に、同じ `targetId` の `CRPage` が既に存在している必要がある（下記「イベント順序」） | `-32601` | ハンドラ（`TargetRegistry::create_target` は存在するが未接続・上限は `MAX_TARGETS`=64） | B |
| 8 | `newPage()` | `CRBrowser._onAttachedToTarget` | イベント `Target.attachedToTarget`（root session 宛て） | `sessionId`・`targetInfo.targetId`・`targetInfo.type`（`page`）・`targetInfo.browserContextId`（必須。欠けると assert 失敗）・`targetInfo.url`・`waitingForDebugger` | 送出経路なし。現実装のイベントはコマンド応答に同伴する形のみ（`HandlerOutput.events`）で、`Target.*` イベントは未定義 | `Target.attachedToTarget` の生成と、`createTarget` の応答より前に届く順序の保証 | B・C |
| 9 | ページ初期化 | `FrameSession._initialize` | `sessionId` 付きで `Page.enable`・`Page.getFrameTree`・`Log.enable`・`Page.setLifecycleEventsEnabled` `{enabled:true}`・`Runtime.enable`・`Page.addScriptToEvaluateOnNewDocument`・`Network.enable`・`Target.setAutoAttach`・`Runtime.runIfWaitingForDebugger` を並行送信 | `Page.getFrameTree` の `frameTree.frame`（`id`・`url`・`loaderId` 等）で frame を構築。`Runtime.executionContextCreated` の `context.auxData.frameId`・`auxData.isDefault`・`context.id` で実行コンテキストを構築 | 全て `-32601`。`Page.navigate` 側が出すのは `Page.frameNavigated`・`Page.loadEventFired` のみ | 上記メソッド群のハンドラとフレームツリー・実行コンテキストのモデル。いずれかが失敗するとページ初期化が失敗する | B |
| 10 | ページ初期化 | `FrameSession._initialize` | 条件付き: `Emulation.setFocusEmulationEnabled`・`Emulation.setDeviceMetricsOverride`（`newContext()` が既定ビューポートを持つ場合）・`Emulation.setEmulatedMedia`・`Page.setInterceptFileChooserDialog`（失敗は握りつぶし）・`Browser.getWindowForTarget`（失敗は握りつぶし）・`Page.createIsolatedWorld`（失敗無視送信）。headless 判定時のみ `Page.setFontFamilies` | 結果は使わない（失敗で初期化が止まるのは握りつぶしでないもの） | 全て `-32601` | 握りつぶされないメソッドのハンドラ。握りつぶされるものは未実装でも初期化は進むとソース上読める | B |
| 11 | ページ初期化 | `FrameSession._initialize` | （メソッドではなくイベント待ち）初期の `about:blank` では `Page.frameNavigated` が非初回として届くまで `_firstNonInitialNavigationCommittedPromise` が解決しないとソース上読める | `Page.frameNavigated` イベント（`frame.id`・`url`・`loaderId`）、`Page.lifecycleEvent` | イベントを送るのは `Page.navigate` のみ。ターゲット生成時の送出なし | 新規ページ作成時の `Page.frameNavigated`（`about:blank`）の送出。未送出だと `newPage()` が初期化待ちでタイムアウトする可能性がある（実測していない） | B |

### 実測で確認できた停止点と予測の境界

トレースが示す停止点は #1 と #2 の 2 つだけである。#3 以降は `connectOverCDP` が成功しないと実行されず、
現トレースでは到達していない。#3〜#11 は B（ソース読解）の予測で、#1・#2 を解消した後に
`make trace-playwright` を再実行して実測で確認する必要がある。

## イベント順序の前提

- `Target.createTarget` の応答を受けた直後に `CRBrowserContext.doCreateNewPage` が `_crPages.get(targetId)._page` を参照する。
  `Target.attachedToTarget` が先に処理されていないと `undefined` の参照になる（B）。
  Chromium は `attachedToTarget` を応答より先に送る。現実装は応答を先に送る規約（`protocol.rs` の `DispatchOutcome`。
  「Chromium 流の並べ替えの要否は TASK-43・CDP-2 で判断」と明記済み。`CDP-5`）
- PoC-5 の記録（C）では空 success フォールバック下で `newPage()` が内部エラー（`_page` が undefined 相当）で失敗したとされ、
  上記の参照と整合する。ただし旧版 Playwright・フォールバック下の結果で、1.63.0 での再現は未実施
- 同一メッセージ列内で応答とイベントが連続して届いた場合に Playwright 側の処理順がどうなるかは、ソース読解だけでは確定しない（未確定）
- `Runtime.enable` を受けた後に `Runtime.executionContextCreated` が届くこと、`Page.getFrameTree` の応答より前の
  `Target.attachedToTarget`（子ターゲット）は Playwright 側でバッファされること（B）

## PoC-5 記録との差分

- CDP-2 の前提は「HTTP ハンドシェイクと WS 接続は成功済み」だが、現状は #1 で止まる。Playwright 1.63.0 は
  `/json/version/`（末尾スラッシュ付き）を要求する（B・A）。PoC-5 当時のサーバーが末尾スラッシュを許容していたのか、
  旧版 Playwright が末尾スラッシュなしで要求していたのかは確認できていない（未確定）
- 空 success フォールバックで到達した 15 メソッド（C）は、本書の #2〜#10 のメソッドと重なると推測されるが、
  メソッド名の突き合わせは spec 未取得のため未実施

## 制約との交差

| 制約 | 本件との関係 |
| ---- | ------------ |
| SEC-2（偽装禁止） | `Browser.getVersion` の `product` は `/` の後ろを版として解析するだけで、`Chrome` を要求する箇所は確認していない。`userAgent` に `Headless` を含まなければ headful 扱いとなり `Page.setFontFamilies` を省くだけ（B）。Chrome を装う値を返さなくても通る可能性があるが、実測前 |
| CDP-6（一律成功の禁止） | #2〜#11 を空 success で通す案は到達性を上げるが、未実装を実装済みに見せる。検出回避として作用しないかのレビューが必要 |
| SEC-4・CDP-1（Host / Origin 検証） | #1 の対応で Host 検証（localhost・IP リテラル限定）・Origin 拒否・loopback 限定バインドを緩めない |
| 上限（`MAX_TARGETS`・`MAX_SESSIONS`） | #5・#7 の対応でターゲット数・セッション数の上限を維持する |
| SSRF | `Target.createTarget` の `url` が取得を伴う場合は既存 `FetchOptions` の内部アドレス拒否を通す |

## 対応候補（列挙のみ。優先度・採否は未決定）

| 候補 | 影響範囲 |
| ---- | -------- |
| `/json/version/` をルータへ明示登録 | `server.rs` の `router` のみ（パス正規化は文字列加工でなく明示登録） |
| `Browser.getVersion`・`Browser.setDownloadBehavior` の実装 | 新ハンドラ（`protocol.rs` の `builtin_handlers`）・`discovery.rs` の版表記との整合 |
| `Target.setAutoAttach`・`Target.getTargetInfo`・`Target.createBrowserContext`・`Target.createTarget` と `Target.attachedToTarget` の実装 | `target.rs`（browser context と `browserContextId` の追加）・イベント送出経路と順序（`DispatchOutcome`） |
| ページセッション初期化メソッド群（#9・#10）の最小実装 | page 状態モデル。メソッドごとに「実装」「明示エラー」のどちらかを選ぶ |
| 新規ページの `Page.frameNavigated` 送出 | `navigation.rs`・`page.rs` |

## 方針

TASK-43.h1（Issue #248）のオーナー判断（2026-10-10）を記録する。根拠ビヘイビアは `CDP-2`（主）・`CDP-6`・`SEC-2`
（`docs/spec/04-behavior/api-cdp.md`・`security-policy.md` が SSOT）。

### 範囲

- `newPage()` 到達までの最小実装とする。`goto()`・セレクタ取得は TASK-44（`CDP-2`）で扱う
- 追加実装は TASK-43.3（Issue #249）。成果物は `crates/fandhe-browser-cdp/src/playwright_compat.rs`（現時点で未作成）

### 進め方（4 段階・段階ごとに実測）

各段階の実装後に `make trace-playwright`（TASK-43.1）で実測し、結果を見て次段階の内容を見直してから進む。

- 実測で停止を確認済みなのは段階 1 の 2 か所のみ（上記 #1・#2。TASK-43.3a で両方解消済み。`newpage-trace.jsonl` の seq 2〜3 と seq 4〜6）
  - 停止 1: `GET /json/version/`（末尾スラッシュ付き）が 404（`connectOverCDP(http://…)`）
  - 停止 2: `Browser.getVersion` が `-32601`（`connectOverCDP(ws://…)`）
- 段階 2 以降は**予測**（B: ソース読解）であり、各段階の実測で見直す。下表の「触る箇所」も予測を含む

| 段階 | 内容 | 触る CDP メソッド / エンドポイント | cdp crate 内の該当箇所 | 根拠 |
| ---- | ---- | ---------------------------------- | ---------------------- | ---- |
| 1 | 接続の確立 | `GET /json/version/`、`Browser.getVersion` | `server.rs` の `router`（ルート明示登録）、`discovery.rs` の `version_body`（版表記の整合）、`protocol.rs` の `builtin_handlers`（ハンドラ登録） | 実測（#1・#2） |
| 2 | ターゲットと順序制御 | `Target.setAutoAttach`・`Target.getTargetInfo`・`Browser.setDownloadBehavior`・`Target.createBrowserContext`・`Target.createTarget`、イベント `Target.attachedToTarget`（`browserContextId` を含める） | `target.rs`（`TargetInfo` へ `browserContextId` 追加、`TargetRegistry::create_target`・`attach`）、`protocol.rs` の `DispatchOutcome`（イベントを応答より先に送る順序制御。現状は応答→イベント列で固定） | 予測（#3〜#8） |
| 3 | ページ初期化系 | `Page.enable`・`Page.getFrameTree`・`Log.enable`・`Page.setLifecycleEventsEnabled`・`Runtime.enable`（`Runtime.executionContextCreated`）・`Page.addScriptToEvaluateOnNewDocument`・`Network.enable`・`Runtime.runIfWaitingForDebugger` ほか（握りつぶされないものから） | `protocol.rs` の `builtin_handlers`、`page.rs`（フレームツリー・実行コンテキストのモデル。`MAIN_FRAME_ID`） | 予測（#9・#10） |
| 4 | 初期 `about:blank` | `Page.frameNavigated`（`about:blank`）の送出 | `page.rs` の `navigation_events`（`Page.navigate` 専用のため新規ページ作成時の送出経路を追加）、`navigation.rs` | 予測（#11） |

段階 2 の `Target.*` は、実測前の予測として Playwright 1.63.0 の初期化を満たす最小集合を挙げたもの。
どのメソッドを「実装」し、どのメソッドを「明示エラー」のままにするかは、段階 2・3 の実測後に決める（未決定）。

### 段階 1 の実施結果（TASK-43.3a・#804。実測 A）

- `/json/version/` を `router` へ明示登録し、`Browser.getVersion` を `playwright_compat.rs` に追加した。
  `product`・`userAgent` は `/json/version` と共通の定数（`discovery::PRODUCT`）で `fandhe-browser/<版>`、
  `protocolVersion` は `1.3`。`revision`・`jsVersion` は実値を参照できないため**省いたが、
  Playwright 1.63.0 は通過した**（フォールバックの空文字は使っていない。`SEC-2`・`REPAIR-3`）
- `make trace-playwright` の再取得結果: 両 `connectOverCDP` が `Browser.getVersion` を通過し、
  次に同時送信される `Target.setAutoAttach`（id 2）と `Browser.setDownloadBehavior`（id 3）が
  `-32601` となり `Protocol error (Target.setAutoAttach)` で失敗する（seq 5〜9・12〜16）。
  これは段階 2 の予測（#3・#4）と一致した
- 次の停止箇所は段階 2 の `Target.setAutoAttach` 以降。`Target.getTargetInfo` は上記失敗により未送信で、
  段階 2 の実測で確認する

### 原則

| 原則 | 内容 | ビヘイビア |
| ---- | ---- | ---------- |
| 未実装は `-32601` | 未実装メソッドは `-32601`（`method not implemented`）を返し続ける。必要なメソッドだけを個別に実装し、一律の成功フォールバックを入れない | `CDP-6`・`SEC-2` |
| 空の成功で通さない | 到達性を上げるためだけに、何もしないメソッドへ空の `success` を返さない。実装済みを装わない（スタブは `///` に将来仕様と ID を明記） | `CDP-6`・`REPAIR-3` |
| Chrome を装わない | `Browser.getVersion` の `userAgent`・`product` 等に Chrome / Chromium を装う値を入れない。`/json/version` と同様に `fandhe-browser/<版>` の実値を返す | `SEC-2` |
| Host / Origin 検証を緩めない | `/json/version/` の受理は明示ルート追加で行い、Host 検証・Origin 拒否・loopback 限定バインドを緩めない | `SEC-4`・`CDP-1` |
| 上限の維持 | browser context・ターゲット・セッション追加でも `MAX_TARGETS`・`MAX_SESSIONS` を維持する | `CDP-7` |

`Browser.getVersion` の値の方針（上記「制約との交差」）は、上記原則どおり実値を返し、Playwright 側が通るかを段階 1 の実測で確認する。
通らなかった場合の扱い（偽装に当たらない範囲での代替）は未決定（オーナー判断待ち）。

### 次の作業

- 草案のマージ後、オーナーが #248 を close する
- #249 の段階別分割は、オーナー承認後に Issue を起票する（本書では決めない）

## 未確定事項・再現手順

- #3 以降は B（ソース読解）で、1.63.0 に対する実測は未実施。#1・#2 の解消後にトレースを再取得して検証する
- #1 の 404 が、ルータ登録（`server.rs`）と依存先ルーティングの完全一致仕様のどちらに由来するかは未切り分け
  （トレースは 404 の事実のみ）
- spec（`docs/spec`）は本調査環境で未取得のため、C の記述は spec 本文と未照合
- 再現: `make trace-playwright`（`harness/playwright-trace/README.md`。ネットワークと node/npm が必要）。
  ソース確認は `npm pack playwright-core@1.63.0` で得た `lib/coreBundle.js` を一時ディレクトリで読む
