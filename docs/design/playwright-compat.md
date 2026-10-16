# Playwright 互換: newPage() 未到達原因の調査記録

- **対応ビヘイビア**: CDP-2（関連: CDP-6・SEC-2・REPAIR-3。`docs/spec/04-behavior/` が SSOT）
- **対応タスク**: TASK-43（サブ 43.2・Issue #247）。入力は TASK-43.1（Issue #246）のトレース
- **マイルストーン**: MS-4
- **基準コミット**: `fe3ac01`（`origin/main`）
- **調査日**: 2026-10-05
- **調査対象の版**: playwright-core 1.63.0（`scripts/versions.sh` の `PLAYWRIGHT_VERSION` と同一）・node v24.21.0

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
| `connectOverCDP(http://…)` | 成功（`noDefaults:true` 指定時。TASK-43.3g・#942 で到達）。`Browser.getVersion`・`Target.setAutoAttach`（`waitForDebuggerOnStart:true` は受理）・`Target.getTargetInfo` が成功する。指定なしの既定オプションでは接続直後の `Browser.setDownloadBehavior`（`allowAndName`）が `-32602` で拒否され通らない（後述「段階 2g」） | A（現行 head での再取得）: seq 1〜9 |
| `connectOverCDP(ws://…)` | 未実行（http 経由が成功するため、収集ハーネスの ws 直接の再試行は行われない） | A |
| `newContext()` | 成功（`acceptDownloads:false` 指定時。TASK-43.3d・#909 で到達）。`Target.createBrowserContext`（`disposeOnDetach:true`）と `browserContextId` 付き `Browser.setDownloadBehavior`（`deny` + `eventsEnabled:true`）が成功する（後述「段階 2b の実施結果」） | A: seq 10〜14 |
| `newPage()` | 失敗。`Target.createTarget` は成功し（TASK-43.3e・#910）、`Target.attachedToTarget` が応答の前に届く。セッション付きの初期化の最初の失敗 `Page.enable` の `-32601`（`method not implemented`）で止まる（後述「段階 2e の実施結果」） | A: seq 15〜47 |

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
| 11 | ページ初期化 | `FrameSession._initialize` | （メソッドではなくイベント待ち）初期の `about:blank` では `Page.frameNavigated` が非初回として届くまで `_firstNonInitialNavigationCommittedPromise` が解決しないとソース上読める | `Page.frameNavigated` イベント（`frame.id`・`url`・`loaderId`）、`Page.lifecycleEvent` | イベントを送るのは `Page.navigate` のみ。ターゲット生成時の送出なし | 新規ページ作成時の `Page.frameNavigated`（`about:blank`）の送出。未送出だと `newPage()` が初期化待ちでタイムアウトする可能性がある（実測していない。**訂正あり**: 「段階 2a の実施結果」の末尾参照） | B |

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
| 4 | 初期 `about:blank` | `Page.frameNavigated`（`about:blank`）の送出 | `page.rs` の `navigation_events`（`Page.navigate` 専用のため新規ページ作成時の送出経路を追加）、`navigation.rs` | 予測（#11。要否は段階 3 の実測で再判断。訂正は「段階 2a の実施結果」末尾） |

段階 2 の `Target.*` は、実測前の予測として Playwright 1.63.0 の初期化を満たす最小集合を挙げたもの。
どのメソッドを「実装」し、どのメソッドを「明示エラー」のままにするかは、段階 2・3 の実測後に決める（未決定）。

### 段階 1 の実施結果（TASK-43.3a・#804。実測 A）

- `/json/version/` を `router` へ明示登録し、`Browser.getVersion` を `playwright_compat.rs` に追加した。
  `product`・`userAgent` は `/json/version` と共通の定数（`discovery::PRODUCT`。core の `USER_AGENT` の別名。#814）で `fandhe-browser/<版>`、
  `protocolVersion` は `1.3`。`revision`・`jsVersion` は実値を参照できないため**省いたが、
  Playwright 1.63.0 は通過した**（フォールバックの空文字は使っていない。`SEC-2`・`REPAIR-3`）
- `make trace-playwright` の再取得結果: 両 `connectOverCDP` が `Browser.getVersion` を通過し、
  次に同時送信される `Target.setAutoAttach`（id 2）と `Browser.setDownloadBehavior`（id 3）が
  `-32601` となり `Protocol error (Target.setAutoAttach)` で失敗する（seq 5〜9・12〜16）。
  これは段階 2 の予測（#3・#4）と一致した
- 次の停止箇所は段階 2 の `Target.setAutoAttach` 以降。`Target.getTargetInfo` は上記失敗により未送信で、
  段階 2 の実測で確認する

### 段階 2a の実施結果（TASK-43.3b・#862）

- 実装したのは `Target.getTargetInfo` のみ（`playwright_compat.rs`。`sessionId` 付きは `-32602`）。
  引数なしは `type:"browser"`・`attached:true` の実値（`CdpState` のブラウザ ID）。`targetId` 指定は
  `TargetRegistry` の実値（`attached` は実セッションの有無）で、未知・不正は `-32602`
- `Target.setAutoAttach` と `Browser.setDownloadBehavior` は**登録を取り下げた**（`-32601` のまま。段階 2c〔TASK-43.3c・#908〕で実体に接続して登録し直した）。
  設定を保存して `{}` を返すだけでは、実セッションの attach・`Target.attachedToTarget` の送出や
  ダウンロード処理（`downloadPath` の保存・イベント通知）に接続されておらず、クライアントに有効化済みと
  誤認させる（`CDP-6`・`REPAIR-3`）。実体と接続できる段階（段階 2b・2c 以降）で改めて実装する
- このため `connectOverCDP` の到達点は TASK-43.3a 後と同じ（`Target.setAutoAttach` で停止。
  `Target.getTargetInfo` は送られない）。トレース（`newpage-trace.jsonl`）も TASK-43.3a 後の内容に戻した

### 段階 2c の実施結果（TASK-43.3c・#908。実測 A）

段階 2a で取り下げた 2 メソッドを、実体に接続して登録し直した（`playwright_compat.rs`・`conn_state.rs`）。

- `Target.setAutoAttach`（root セッションのみ）:
  - `autoAttach:true` は、この接続がまだ attach していない既存ターゲットすべてへ `TargetRegistry::attach` し、
    ターゲットごとに `Target.attachedToTarget`（`sessionId`・`targetInfo`・`waitingForDebugger:false`）を
    応答の後に送出する。重複呼び出しは冪等。途中失敗（`MAX_SESSIONS` 到達等）は巻き戻す（`CDP-7`）
  - 有効状態を接続ごとに保持し、有効化後に registry へ現れたターゲットも attach する。push 経路が無いため
    `Target.attachedToTarget` は当該接続の次のコマンド応答の後に送出する（`ConnectionScoped::reconcile_auto_attach`）
  - `autoAttach:false` はこの接続が attach したセッションを外し、`Target.detachedFromTarget` を送出する
  - セッションの所有は接続スコープで、切断（`ws.rs` の `on_close`）で解放する
  - 拒否（`-32602`）: `filter`（Puppeteer の除外フィルタ。黙って無視すると除外すべきターゲットまで attach するため）、
    `flatten` が `true` 以外（`sendMessageToTarget` 方式は未実装）、`sessionId` 付き（子ターゲットのモデルが無い）。
    `waitForDebuggerOnStart:true`（`autoAttach:true` 時）も当初は拒否していた（成功を装わないため。`REPAIR-3`・`CDP-6`）。**段階 2g（#942）で受理に変わった**（後述）
  - 接続なしの直接 dispatch は `-32000`（`no connection`）
- `Browser.setDownloadBehavior`:
  - ダウンロード実行系（保存・`Browser.downloadWillBegin`・`downloadProgress`）は未実装で、現状ダウンロードは発生しない。
    保証できる `deny`・`default`（イベントなし）だけを受理して接続スコープで保持し、`allow`・`allowAndName` と
    `eventsEnabled:true` は成功を装わず `-32602`（`unsupported params`）で拒否する（`CDP-6`・`REPAIR-3`）。
    このため Playwright の接続シーケンス（`allowAndName`・`eventsEnabled:true`）もこの段で拒否される（ただし現行サーバーでは、それより前の `Target.setAutoAttach` の `waitForDebuggerOnStart:true` が先に `-32602` で拒否されるため、接続は `Target.setAutoAttach` の段で止まる）
    （実行系の実装〔`CDP-2`〕まで許容。トレースは現行 head で再取得済み）
  - `behavior` は `deny`・`allow`・`allowAndName`・`default`（`allow*` は上記のとおり拒否）
  - `downloadPath` は絶対パス・`..` なし・NUL なし・4096 バイト以下。**サーバーはこのパスに一切ファイル操作をしない**
    （作成・書き込み・存在確認なし）。実行系で書き込む際はプロファイル境界（`PROF`）を踏まえた別途のセキュリティ設計を要する
  - `browserContextId` 指定は段階 2b（TASK-43.3d・#909）で context 単位の保持に対応した（不正・未知の ID は `-32602`）。`eventsEnabled` は保持するがイベントは送出しない
  - 寿命は接続スコープ（切断で破棄・プロファイルへ永続化しない）
- 段階 2c 時点の `make trace-playwright` の再取得結果: 両 `connectOverCDP` が `Target.setAutoAttach`（id 2）・`Browser.setDownloadBehavior`（id 3）の `-32602` で失敗し、`newContext()` には到達しなかった（当時の記録。次に止まるのは `waitForDebuggerOnStart:true` の拒否だった。**段階 2g で解消済み**）
- 既知の制約: `Target.attachedToTarget` の `targetInfo` に `browserContextId` が無い。トレースサーバーは
  ターゲットを事前作成しないため今回の到達には影響しないが、既存ターゲットがある状態の Playwright は
  `browserContextId` を要する見込み（B）。段階 2b（TASK-43.3d・#909）で追加した
- 送出順は応答 → イベント列のまま（`CDP-5`）。`Target.createTarget` だけは `Target.attachedToTarget` を応答の前に送る（段階 2e〔#910〕で実装）

### 段階 2g の実施結果（TASK-43.3g・#942。実測 A）

接続段階の 2 つの拒否を、オーナー判断（2026-10-12）に沿って次のとおり解消した（`playwright_compat.rs`・`conn_state.rs`）。

- `Target.setAutoAttach` の `waitForDebuggerOnStart:true` を受理して接続状態に保持する。CDP の定義（Target.pdl）で止める
  対象は「以後に attach される新しいターゲット」で、既存ターゲットは止めない（Chromium の `target_handler.cc` も既存を
  `AutoAttach(..., false)` で attach する）。既存ターゲットの `Target.attachedToTarget` は `waitingForDebugger:false`。
  新しいターゲットを実際に止める処理は `Target.createTarget`（段階 2e・#910）で実装した。
  `filter`・`flatten` が `true` 以外・`sessionId` 付き・型不正は従来どおり拒否する
- `Browser.setDownloadBehavior` は `deny`（`eventsEnabled` は省略・`true`・`false` のいずれも）と、`eventsEnabled` なしの `default` だけを
  受理する。`deny` ではダウンロードが起きないため、`eventsEnabled:true` を受理してもダウンロードのイベントは送出しない。
  `downloadPath` は検証のみで、`deny` では保持もしない。`allow`・`allowAndName` は保存の実行系が無いため `-32602` で拒否のまま
  （#924 の判断は変えない）。`default` + `eventsEnabled:true` も拒否のまま
- **制限（Playwright の既定のオプションでは通らない）**: Playwright は既定で接続直後に `Browser.setDownloadBehavior`
  （`allowAndName`）を送り、`newContext()` も `acceptDownloads` の既定で `allowAndName` を送るため、どちらも拒否される。
  通すには `chromium.connectOverCDP(url, { noDefaults: true })`（接続直後の送信を省く）と
  `browser.newContext({ acceptDownloads: false })`（`deny` + `eventsEnabled:true` を送る）の指定が要る
- 実測（`make trace-playwright`。`noDefaults:true` でも接続直後に `Browser.setDownloadBehavior` は送られないことを確認。
  2 回の実行で同一出力）: `Browser.getVersion`・`Target.setAutoAttach`・`Target.getTargetInfo` が成功して
  `connectOverCDP(http)` が成功し、`newContext()` が **`Target.createBrowserContext` の `-32601`（`method not implemented`）** で失敗する
  （`browser.newContext: Protocol error (Target.createBrowserContext): method not implemented`）。次に止まるのは段階 2b の
  `Target.createBrowserContext` で、`newPage()` には到達しない
- 注記: Playwright は接続の時点で既存ページの初期化（`Page.enable` 等）まで行い、`targetInfo.browserContextId` も接続時に確認する。
  トレースサーバーはターゲットを事前作成しないためこの実測には影響しないが、既存ページがある状態では段階 2b で
  `browserContextId` の付与が要る。上記の段階表（#9〜#11）の前提はこの点を含めて読むこと
- 一次情報: CDP `Target.setAutoAttach` の `waitForDebuggerOnStart`（Target.pdl）、Chromium
  `content/browser/devtools/protocol/target_handler.cc` の `AutoAttach(..., false)`、CDP `Browser.setDownloadBehavior` の
  `behavior`・`eventsEnabled`（Browser.pdl）、Playwright 1.63.0 の `crBrowser.ts`・`chromium.ts`（`noDefaults:true` なら接続直後に
  送らない。`newContext` は `acceptDownloads` の既定で `allowAndName`、`false` で `deny`、どちらも `eventsEnabled:true` 付き）

### 段階 2e の実施結果（TASK-43.3e・#910。実測 A）

実装範囲（`CDP-2`・`CDP-5`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`。`protocol.rs`・`playwright_compat.rs`・`conn_state.rs`・`target.rs`・`navigation.rs`）:

- 送出順: `HandlerOutput` と `DispatchOutcome` に opt-in の `pre_events` を足した。順序は pre_events → 応答 → events。
  既定（既存ハンドラ）は応答 → events のまま。応答の前に送るのは `Target.createTarget` の `Target.attachedToTarget` だけ
- `Target.createTarget` が受理する `params` は `url`（`"about:blank"` のみ。取得を伴わず SSRF の面を作らない）と `browserContextId`
  （省略時は既定 context。指定できるのは既定 context か自接続が作った context だけ）。他の `url`・他のキー
  （`newWindow`・`background`・`width` 等）は黙って無視せず `-32602`（`unsupported params`）、`url` の欠落・型不正と
  `browserContextId` の型・書式不正・未知・他接続の context は `-32602`（`invalid params`。未知と権限外は同じ文言で存在を漏らさない）。
  応答は `{"targetId"}` のみ。上限超過は `-32000`（`server error`）で、セッション枠が足りない場合は作成を巻き戻す
- 呼び出し接続の自動 attach が有効なら、作成と attach を 1 回のロックで行い、`Target.attachedToTarget` を応答の前に送る。
  attach は接続の所有記録に載るため、直後の reconcile で二重に attach しない。他接続は次のコマンドの応答に続けて
  `waitingForDebugger:false` で追従 attach する（push 経路が無いため）
- `waitForDebuggerOnStart:true` の接続が作ったターゲットは止める: `waitingForDebugger:true` で通知し、そのセッションへの
  `Runtime.runIfWaitingForDebugger` が届くまで初期 `about:blank` 文書を確定しない（確定前の `DOM.getDocument` は
  `-32000 document not available for target`、`Page.navigate` は `-32000 target is waiting for debugger`）。止めていない
  ターゲットは作成時に確定する。止まっていないターゲットへの `Runtime.runIfWaitingForDebugger` は何もせず `{}`（Chromium と同じ）。
  止めたセッションが外れた（切断・`autoAttach:false`）ときも再開して永久停止を残さない
- ブラウザ ID は別名前空間（最上位ビット）で払い出すため、ページの `targetId` と衝突しない
- Chromium との差異（`REPAIR-3`）: `waitingForDebugger:true` は作成した接続にだけ通知しクライアントごとの待ちは持たない。
  既定 context に作ったターゲットは作成接続の切断では閉じない（`Target.closeTarget` で閉じる。`MAX_TARGETS` が歯止め）
- 未実装: 任意 URL の読み込み、`Target.targetCreated`（`Target.setDiscoverTargets` が未実装）、window 系 params、
  初期読み込み時の `Page.frameNavigated` 等（`Page.enable` が未実装）

実測（`make trace-playwright`。`playwright-core` 1.63.0。`worldName` の乱数ハッシュを `<WORLD_HASH>` へ正規化した上で、2 回の実行で同一出力）:
予測どおり、`Target.attachedToTarget`（`waitingForDebugger:true`）が `Target.createTarget` の応答より前に届き、`newPage()` は
`Target.createTarget` を通過する。続けて Playwright が送るセッション付きの初期化メソッド
（`Browser.getWindowForTarget`・`Page.enable`・`Page.getFrameTree`・`Log.enable`・`Page.setLifecycleEventsEnabled`・`Runtime.enable`・
`Page.addScriptToEvaluateOnNewDocument`・`Network.enable`・`Emulation.setFocusEmulationEnabled`・`Emulation.setDeviceMetricsOverride`・
`Emulation.setEmulatedMedia`）は未実装で `-32601`、セッション付き `Target.setAutoAttach`（子ターゲットの自動 attach）は `-32602`
（`unsupported params`）、`Runtime.runIfWaitingForDebugger` は `{}` で成功する。`newPage()` は最初の失敗の
**`Page.enable` の `-32601`**（`browserContext.newPage: Protocol error (Page.enable): method not implemented`）で失敗し、
Playwright が `Target.closeTarget` を送って `Target.detachedFromTarget` が届く。次に実装すべきは、`newPage()` の初期化で
Playwright が応答を待つ `Page.enable`・`Page.getFrameTree` 以降のセッション付きメソッド群（段階 3）で、各メソッドの
必要最小限の応答は `Emulation.*`・`Network.enable` を含め個別に `SEC-2` の観点で判断する。

### 段階 2b の実施結果（TASK-43.3d・#909。実測 A）

実装範囲（`CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`）:

- browser context をモデルに入れた（`target.rs`）。既定 context の ID は固定で `00000000000000000000000000000001`、
  `Target.createBrowserContext` が払い出す ID は `…0002` から。全ページターゲットの `targetInfo`
  （`Target.getTargetInfo`・`Target.attachedToTarget`）に `browserContextId` を載せる（既存ページは既定 context）。
  ブラウザ自身（`type:"browser"`）の `targetInfo` には載せない
- `Target.createBrowserContext` は `disposeOnDetach:true` だけを受理する。省略・`false` は `-32602`（`unsupported params`）で拒否する。
  解放手段の `Target.disposeBrowserContext` が未実装で、永続 context を作ると枠が枯渇する（`CDP-7`）ため。受理した context は作成した
  接続が所有し、切断で所属ターゲット・セッションごと破棄する。`proxyServer`・`proxyBypassList`・
  `originsWithUniversalNetworkAccess` は未実装で、黙って無視すると proxy 経由と誤認させるため `-32602`（`unsupported params`）で拒否する
- `browserContextId` 付き `Browser.setDownloadBehavior` は、受理する組み合わせを接続単位の場合（#942）と同じにする（`deny`〔`eventsEnabled` 不問〕と
  `eventsEnabled` なしの `default`）。設定は context 単位で保持し（Chromium と同じ）、context の破棄で消える。`downloadPath` は検証のみで、
  `deny` では保持せずファイル操作もしない。不正・未知の `browserContextId` は `-32602`（`invalid params`）
- 上限: 新規 context は 64 件（`MAX_BROWSER_CONTEXTS`。既定 context を除く）。`MAX_TARGETS`・`MAX_SESSIONS` は全 context 共通で維持する
- 未実装のまま: `Target.disposeBrowserContext`・`Target.getBrowserContexts`（`Target.createTarget` は段階 2e〔#910〕で実装）

実測（`make trace-playwright`。2 回の実行で同一出力）: `Target.createBrowserContext` → `{"browserContextId":"…0002"}`、
`Browser.setDownloadBehavior`（`deny`・`browserContextId`・`downloadPath`・`eventsEnabled:true`）→ `{}` で `newContext()` が成功し、
`newPage()` が **`Target.createTarget` の `-32601`（`method not implemented`）** で失敗する
（`browserContext.newPage: Protocol error (Target.createTarget): method not implemented`）。次に止まるのは #910 の範囲で、
`newContext()`・`newPage()` 到達の結合確認は #911 で行う。

停止条件の判断: #942 後の停止点は `browserContextId` の assert ではなく `Target.createBrowserContext` の `-32601` だった。
トレースサーバーは既存ページを作らないため assert には到達しない。停止点は本 Issue の表題のメソッドそのものであるため、
スコープ内として実装を進めた。既存ページがある経路（`browserContextId` の assert）は実測ではなく Rust のテスト
（`cdp2_target_info_includes_browser_context_id`・WebSocket 経由の自動 attach テスト）で担保している。
ハーネスにページを事前作成させる変更は別 issue とする。

#### 予測 #11（`Page.frameNavigated`）の訂正（種別 B・未実測）

playwright-core 1.63.0 のソース読解では、`FrameSession._initialize` の `isInitialEmptyPage` は
`Page.getFrameTree` で得たメインフレーム URL が `":"` または空文字のときだけ真になる。
`about:blank` を返せば偽となり、同関数内で `_firstNonInitialNavigationCommittedFulfill()` が即座に呼ばれ、
`Page.frameNavigated` の非初回イベントを待たない。したがって元の予測（初期 `about:blank` の
`Page.frameNavigated` が無いと `newPage()` がタイムアウトし得る）は当たらない見込み。
段階 4 の要否は段階 3 の実測で再判断する（過去の記録は残し、本訂正で上書きしない）。

### 到達結果（TASK-43.3f・#911。実測 A）

`newPage()` 到達の結合確認の結果は **未到達**。基準は #1035 後の main（`08ab781c`）で再取得済みのトレース
（`playwright-core` 1.63.0・node v24.21.0。収集オプションは `connectOverCDP` の `noDefaults:true` と
`newContext` の `acceptDownloads:false`）。`connectOverCDP(http)` と `newContext` は成功し、`newPage` は最初の停止
**`Page.enable` の `-32601`**（`browserContext.newPage: Protocol error (Page.enable): method not implemented`）で失敗する。
空の成功やスタブで通さず、停止点を固定する（`CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`）。

停止したメソッドの一覧（トレース seq 19・32・34〜43。すべてセッション付き。`sessionId` は正規化済みの `…0001`）:

| id | メソッド | エラー（code / message） | Playwright 側の扱い |
| -- | -------- | ------------------------ | ------------------- |
| 7 | `Browser.getWindowForTarget` | `-32601` / `method not implemented` | 握りつぶす（不足箇所一覧 #10・種別 B） |
| 8 | `Page.enable` | `-32601` / `method not implemented` | 待つ（失敗で初期化が止まる。`newPage` の最初の停止） |
| 9 | `Page.getFrameTree` | `-32601` / `method not implemented` | 待つ（不足箇所一覧 #9・種別 B） |
| 10 | `Log.enable` | `-32601` / `method not implemented` | 待つ（同上） |
| 11 | `Page.setLifecycleEventsEnabled` | `-32601` / `method not implemented` | 待つ（同上） |
| 12 | `Runtime.enable` | `-32601` / `method not implemented` | 待つ（同上） |
| 13 | `Page.addScriptToEvaluateOnNewDocument` | `-32601` / `method not implemented` | 待つ（同上） |
| 14 | `Network.enable` | `-32601` / `method not implemented` | 待つ（同上） |
| 15 | `Target.setAutoAttach` | `-32602` / `unsupported params` | 待つ（同上） |
| 16 | `Emulation.setFocusEmulationEnabled` | `-32601` / `method not implemented` | 待つ（不足箇所一覧 #10） |
| 17 | `Emulation.setDeviceMetricsOverride` | `-32601` / `method not implemented` | 待つ（同上） |
| 18 | `Emulation.setEmulatedMedia` | `-32601` / `method not implemented` | 待つ（同上） |

結合テスト `cdp2_playwright_newpage_reach_report_lists_first_stop_and_failures`
（`crates/fandhe-browser-cdp/tests/playwright_trace.rs`。CI では unix の `rust-test-*` で実行）が、stage の成否・最初の停止・
上表の失敗一覧を具体値で固定する。

次の作業: ページ初期化メソッド群（上表の `Page.enable` 以降。`Browser.getWindowForTarget` は握りつぶされるため `-32601` のままでよい見込み）の
実装 issue を起票して #911 の依存に加える。実装後にトレースを再取得し、テストを到達確認（stage がすべて成功・停止なし）へ切り替える。
それまで #911 は open のまま残す。

### 段階 3a の実施結果（TASK-43.3i・#1037。実測 A）

**実装範囲**（`CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`・`CDP-7`）:

| メソッド | 受理条件 | エラー |
| -------- | -------- | ------ |
| `Page.enable` | 生存セッションの `sessionId` 付き・`params` なし（または `{}`）。`{}` を返し、セッションに有効フラグを立てる（冪等） | `sessionId` なし・`params` にキーあり（`enableFileChooserOpenedEvent` を含む）は `-32602 unsupported params`、未知・閉じたセッションは `-32602 invalid params` |
| `Page.getFrameTree` | 生存セッション付き。`frameTree.frame`（`id:"main"`・`loaderId`・`url`・`mimeType:"text/html"`）のみ返す。`Page.enable` は前提にしない | `Page.enable` と同じ分け方 |

- フラグは `target.rs` の `Tables.page_enabled` に置く（Issue 記載の `conn_state.rs` から外れる）。セッション削除（detach・`closeTarget`・
  `disposeBrowserContext`）と同じロックで消えて TOCTOU が起きず、自動 attach 由来でないセッションも扱える。件数は `MAX_SESSIONS` 以下。
- `Page.enable` の実効果は #1039 の `Page.lifecycleEvent` 送出ゲートだけで、現時点ではイベント送出を何も変えない。既存の
  `Page.frameNavigated`・`Page.loadEventFired` は `Page.enable` なしでも従来どおり送出する（Chromium との差異）。
- `getFrameTree` の `loaderId` は遷移状態の世代の hex。止めたターゲットは再開前後で同じ値（初期 `about:blank` 確定後の `"2"`）。
  `childFrames`・`securityOrigin`・`parentId` は返さない。遷移中・取得失敗後は「進行中または失敗した世代」と registry の URL を返し、
  Chromium（確定済みの直前文書）と異なる。

**実測**（`playwright-core` 1.63.0・node v24.21.0。トレースを再取得し 2 回の実行で同一出力）: id 8（`Page.enable`）は `{}`、
id 9（`Page.getFrameTree`）は `frameTree.frame = {id:"main", loaderId:"2", url:"about:blank", mimeType:"text/html"}` で成功する。
`newPage` の失敗は `Protocol error` から **`browserContext.newPage: Frame has been detached.`** に変わった。この失敗は
`Log.enable`（id 10）の `-32601` より前に起き、`Target.closeTarget` は id 9 の応答の直後に送られる（`Log.enable` 以降の `-32601` は
後から届く）。`failed` の一覧から id 8・9 が消え、`Page.createIsolatedWorld` は送られていない。

**`Page.frameNavigated` 待ちの有無**: 停止は `Frame has been detached.` で、`Page.frameNavigated` を待ったタイムアウトではない
（再取得トレースで `frameNavigated` 待ちの停止は観測していない）。

**停止原因（種別 B・ソース読み）**: playwright-core 1.63.0 の `CRPage._sessionForFrame` は `this._sessions.has(frame._id)`
（セッションを targetId で引く）を前提にし、無ければ親フレームを辿って `Frame has been detached.` を投げる。`frame.id` を
`"main"` で返しているため、targetId と一致せず止まる可能性が高い。どのコード経路で投げられたかは未確認。対処（frame id を targetId にする等）は
`Page.frameNavigated` の `frame.id` と併せた設計判断になるため本 issue では行っていない（TASK-44 の `page.goto()` にも影響し得る）。

次の作業: #1038〜#1042（ページ初期化メソッド群の残り）。frame id と targetId の関係は別途判断が要る。

### 段階 3b の実施結果（TASK-43.3j・#1038。実測 A）

**実装範囲**（`CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`・`CDP-7`）:

| メソッド | 受理条件 | エラー |
| -------- | -------- | ------ |
| `Log.enable`・`Runtime.enable`・`Network.enable` | 生存セッションの `sessionId` 付き・`params` なし（または `{}`）。`{}` を返し、セッションにドメインごとの有効フラグを立てる（冪等）。止めたターゲット（`Runtime.runIfWaitingForDebugger` より前）でも受理する | `sessionId` なし・`params` にキーあり（`Network.enable` の `maxTotalBufferSize`・`maxResourceBufferSize`・`maxPostDataSize`・`reportDirectSocketTraffic` を含む）は `-32602 unsupported params`（フラグは立てない）、未知・閉じたセッションは `-32602 invalid params` |

- フラグは `target.rs` の `Tables.domains_enabled`（`SessionDomain` ごとの bool）に置く（Issue 記載の `conn_state.rs` から外れる。理由は段階 3a と同じ。
  `page_enabled` は #1039 との衝突を避けるため変更せず別持ちにした）。セッション削除と同じロックで消え、件数は `MAX_SESSIONS` 以下。
- **イベントは送らない**（`REPAIR-3`）。`Runtime.executionContextCreated`・`consoleAPICalled` は `Runtime.evaluate` 等が未実装で、使えない
  `context.id` を見せると偽の応答になるため送らない（実行コンテキストのモデルは TASK-44）。`Network.*`（`requestWillBeSent`・`responseReceived`・
  `loadingFinished`・`loadingFailed`）と `Log.entryAdded` も送らない（後続 issue で追跡）。「イベントが来ない」ために生じる待ち（`page.evaluate` 等）は TASK-44 の課題。
- 3 メソッドのフラグは fetch・ヘッダ・UA・`navigator` を何も変えない（`SEC-2`）。`*.disable`・`Network.setUserAgentOverride`・
  `Emulation.setUserAgentOverride` は `-32601` のまま。

**実測**（`playwright-core` 1.63.0。トレースを再取得し 2 回の実行で同一出力）: id 10（`Log.enable`）・12（`Runtime.enable`）・14（`Network.enable`）は
`{}` で成功する。`newPage` の失敗は `browserContext.newPage: Frame has been detached.` のまま変わらない。`failed` の残りは
id 7・11・13・15・16・17・18。

次の作業: #1039〜#1042（ページ初期化メソッド群の残り）。frame id と targetId の関係は別途判断が要る。

### 段階 3c の実施結果（TASK-43.3k・#1039。実測 A と種別 B）

**実装範囲**（`CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`・`CDP-7`）:

| メソッド | 受理条件 | エラー |
| -------- | -------- | ------ |
| `Page.setLifecycleEventsEnabled` | 生存セッションの `sessionId` 付き・`params` が `{"enabled": bool}` のみ。`{}` を返し、セッションの lifecycle 有効フラグを設定する（true・false とも冪等）。止めたターゲットでも受理する | `sessionId` なし・`enabled` 以外のキーは `-32602 unsupported params`、`enabled` が無い・bool でない・未知／閉じたセッションは `-32602 invalid params`（いずれもフラグは変えない） |

- フラグは `target.rs` の `Tables.lifecycle_events_enabled` に置く（Issue 記載の `conn_state.rs` から外れる。理由は段階 3a と同じ）。
  セッション削除と同じロックで消え、件数は `MAX_SESSIONS` 以下。
- **送出**: `Page.enable` 済みかつ lifecycle 有効なセッションの `Page.navigate` が確定したとき、`Page.lifecycleEvent` を送る。
  順序は 応答 → `init` → `Page.frameNavigated` → `Page.loadEventFired` → `load`（`page.rs` の `navigation_events` が正本）。
  `frameId` は `main`、`loaderId` は応答と `frameNavigated` に一致し、`timestamp` は 3 イベントで同じ値。失敗・中断・追い越された遷移・
  閉じたターゲットでは `frameNavigated` と同じ条件ブロックの中で判定するため送らない。順序の根拠（`init` は `load` より前、`load` は
  `frameNavigated` より後）は Playwright / Puppeteer のソース読解による推定で、**種別 B（未実測）**。
- **送らない名前**（`REPAIR-3`）: `DOMContentLoaded`・`networkAlmostIdle`・`networkIdle`・`commit`・`firstPaint` 系・`InteractiveTime`。実体が無く、
  偽の値で埋めない。将来仕様は TASK-43・TASK-44（`CDP-2`）。
- **差異**: 有効化時に現在の文書の lifecycle を再送しない（Chromium は再送する）。送り先は `Page.navigate` を送ったセッションだけ。
  `Runtime.runIfWaitingForDebugger` による初期 `about:blank` の確定では送らない。
- フラグは fetch・ヘッダ・UA・`navigator` を何も変えない（`SEC-2`）。

**実測**（`playwright-core` 1.63.0。トレースを再取得し 2 回の実行で同一出力）: id 11（`Page.setLifecycleEventsEnabled`）は `{}` で成功する。
`newPage` の失敗は `browserContext.newPage: Frame has been detached.` のまま変わらない。`failed` の残りは id 7・13・15・16・17・18。
この trace は `Page.navigate` を含まないため、Playwright が `lifecycleEvent` を待つかは実測できていない（種別 B のまま）。

次の作業: #1041・#1042（ページ初期化メソッド群の残り。#1040 は段階 3d で実施）。

### 段階 3d の実施結果（TASK-43.3l・#1040。実測 A）

**実装範囲**（`CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`）:

| メソッド | 受理条件 | エラー |
| -------- | -------- | ------ |
| `Page.addScriptToEvaluateOnNewDocument` | 生存セッションの `sessionId` 付き・`params` が `source`（空文字列のみ）と任意の `worldName`（文字列）だけ。`{"identifier":"<10 進連番>"}` を返す。止めたターゲットでも受理する | `sessionId` なし・非空 `source`・`source`／`worldName` 以外のキー（`runImmediately`・`includeCommandLineAPI` 等）は `-32602 unsupported params`、`source` 欠落・型違い・`worldName` の型違い／NUL・未知セッションは `-32602 invalid params`、`worldName` が 256 バイト超は `-32000 world name too long`、セッションあたり 64 件超は `-32000 too many init scripts` |
| `Target.setAutoAttach`（`sessionId` 付き） | 生存セッション・`autoAttach` が bool。`autoAttach:true` は `flatten:true` 必須。`{}` を返し、`waitForDebuggerOnStart` とともに状態を保持する（`autoAttach:false` で解除）。イベントは 0 件 | `filter` 付き・`autoAttach:true` で `flatten` が `true` 以外は `-32602 unsupported params`、`autoAttach` 欠落・型違い・未知セッションは `-32602 invalid params` |

- 状態は `target.rs` の `Tables`（`init_scripts`・`next_init_script`・`child_auto_attach`）に置く（Issue 記載の `conn_state.rs` から外れる。理由は段階 3a と同じ）。
  セッション削除と同じロックで消え、切断・ターゲットの close・context の破棄で漏れない（付随状態の削除は `drop_session_state` に集約）。
- **identifier は registry 全体の連番**（Issue 記載の接続スコープより広い）。採番を 1 回のロックで原子的にでき、同一接続での再登録が別 identifier になる。
  セッション ID と同じ性質で新しい情報漏えいは無い。
- **非空 `source` を拒否する理由**: 実行経路（`JS-4`・TASK-112）が無く、効いていないのに成功を返すと stealth 用のスクリプト注入として働く（`SEC-2`・`CDP-6`）。
  空 `source` は実行しても何も起きないので no-op が事実と一致する。`worldName` は保持しない。
- **差異**（`REPAIR-3`）: 子ターゲット（iframe・worker・OOPIF）のモデルが無く、セッション付き `Target.setAutoAttach` の attach 対象は常に 0 件
  （事実と一致する応答で、一律の成功ではない）。`waitForDebuggerOnStart` は保持するだけ。`Page.removeScriptToEvaluateOnNewDocument` は未実装。
- UA・ヘッダ・`navigator`・画面・TLS・locale には触れない。`Emulation.setUserAgentOverride`・`Network.setUserAgentOverride` は `-32601` のまま。

**実測**（`playwright-core` 1.63.0。トレースを再取得し 2 回の実行で同一出力）: id 13 は `{"identifier":"1"}`、id 15 は `{}` で成功する。
`newPage` の失敗は `browserContext.newPage: Frame has been detached.` のまま変わらない。`failed` の残りは id 7・16・17・18。

次の作業: #1041・#1042（`Emulation.*` 等）。frame id と targetId の関係は別途判断が要る。

### 段階 3e の実施結果（TASK-43.3m・#1041。実測 A と種別 B）

画面に近い `Emulation.*` の受理を絞るオーナー方針（2026-10-15）に従い、先に収集ハーネスの `newContext` へ `viewport: null` を足して実測し、
実測後も送られるメソッドだけを受理した。

**実測 1 回目**（実装前。`playwright-core` 1.63.0。トレースを 2 回再取得し同一出力）: `viewport: null` で
`Browser.getWindowForTarget`（旧 id 7）と `Emulation.setDeviceMetricsOverride`（旧 id 17）が**送られなくなった**。送信が残る `Emulation.*` は
`setFocusEmulationEnabled` と `setEmulatedMedia` の 2 つ。送信メソッドは 20 件から 18 件になり、以降の id は繰り上がった。ソース読解の予測
（`noDefaultViewport` で `getWindowForTarget` を省く・`emulatedSize()` が無いと `setDeviceMetricsOverride` を省く・`skipDefaultOverrides` は
既定 context だけで真になるため focus と media は残る）と一致した。不足箇所一覧 #10 と到達結果の表にある旧 id（7・16〜18）は、
`viewport: null` 導入前の記録として残す（本節で訂正）。

**実装範囲**（`CDP-2`・`CDP-6`・`SEC-2`・`REPAIR-3`・`CDP-7`）:

| メソッド | 受理条件 | エラー |
| -------- | -------- | ------ |
| `Emulation.setFocusEmulationEnabled` | 生存セッションの `sessionId` 付き・`params` が `{"enabled": bool}` のみ。`{}` を返し、検証済みの値を保持する（true・false とも冪等）。止めたターゲットでも受理する | `sessionId` なし・`enabled` 以外のキーは `-32602 unsupported params`、`enabled` が無い・bool でない・未知／閉じたセッションは `-32602 invalid params` |
| `Emulation.setEmulatedMedia` | 生存セッションの `sessionId` 付き。`media` は `""` のみ、`features` は Playwright 既定の 4 件（`prefers-color-scheme:light`・`prefers-reduced-motion:no-preference`・`forced-colors:none`・`prefers-contrast:no-preference`）をちょうど 1 回ずつ（順不同）。`{}` を返し、既定値を受理した印を保持する | 既定値以外の値・未知の feature 名・重複・欠落・5 件以上・要素の余分なキー・`media` か `features` の欠落・未知のキーは `-32602 unsupported params`、型の不正・未知／閉じたセッションは `-32602 invalid params`（いずれも状態は変えない） |

- `Emulation.setDeviceMetricsOverride` は `viewport: null` で送られなくなったため**実装しない**（`-32601` のまま。`cdp6_builtin_other_methods_remain_unimplemented`
  と `cdp6_identity_and_screen_emulation_remain_method_not_implemented` で固定する）。`Emulation.setUserAgentOverride`・`setLocaleOverride`・`setTimezoneOverride`
  も `-32601` のまま（`SEC-2`・`CDP-6`）。
- 保持値は `target.rs` の `Tables.emulation`（セッションごとの小さな構造体）に置く（Issue 記載の `conn_state.rs` から外れる。理由は段階 3a と同じ。
  セッション削除と同じロックで消え、件数は `MAX_SESSIONS` 以下）。消える経路は `detach`・`close_target`・`dispose_browser_context` の 3 つ。
- **保持値はどの API からも読まれず、今は観測できる効果が無い**（`REPAIR-3`）。読み取り用メソッドは `#[cfg(test)]` だけに置く。
  `Browser.getVersion`・`Target.getTargetInfo`・`Page.getFrameTree`・`Page.navigate` のイベント列が受理の前後で変わらないことをテストで固定する（`SEC-2`）。
- **差異**: 実際の media・フォーカスのエミュレーションはしない。将来は render（TASK-38.5）のフォーカス管理・境界ボックスと CSSOM の media 評価に接続する（別 issue）。
- 既定のビューポートのまま Playwright を使う利用者の `setDeviceMetricsOverride` はオーナー方針により本 issue では扱わない。

**実測 2 回目**（実装後。2 回の実行で同一出力）: `Emulation.setFocusEmulationEnabled`（id 15）・`Emulation.setEmulatedMedia`（id 16）は `{}` で成功し、
`failed` から消えた。`failed` の残りは id 12（`Page.addScriptToEvaluateOnNewDocument`、`-32601`）と id 14（セッション付き `Target.setAutoAttach`、`-32602`）。
`newPage` の失敗は `browserContext.newPage: Frame has been detached.` のまま変わらない。

次の作業: #1040・#1042（ページ初期化メソッド群の残り）。

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
