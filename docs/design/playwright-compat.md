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
| `newPage()` | 成功（`connectOverCDP(noDefaults:true)`・`newContext({acceptDownloads:false, viewport:null})` 指定時。TASK-43.3n・#1042 で到達を確認。後述「到達結果（TASK-43.3n）」）。`Browser.getWindowForTarget` は `-32601` のまま残るが `newPage()` は失敗しない（`Page.createIsolatedWorld` は TASK-44.6・#1217 で実装済み。評価器が無効な構成ではエラー。cli 既定〔V8〕では TASK-44.9・#1220 で有効） | A: seq 15〜41 |
| `goto`（ローカル fixture） | 成功（TASK-44.3・TASK-44.4 で確認。後述「テスト結果レポート（TASK-44.4）」） | A |
| `selector`（`#target`） | 成功（#1245 で `Element.attributes` を補い、5 段階すべてに到達。経過: TASK-44.9・#1220 で `StageTimeout` を解消、#1237 で `*` に対応、#1242 で `CustomEvent` を追加。後述「再実測（#1245）」） | A |

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
  - 拒否（`-32602`）: 受理表にない `filter`（`filter` 省略は従来どおりで Playwright は送らない。`[{type:page,exclude:true},{}]` は Puppeteer 経路の tab 擬似ターゲットとして #1334 で受理に変わった。受理表は `puppeteer-compat.md`「tab 擬似ターゲットと `filter`」。黙って無視すると除外すべきターゲットまで attach するため）、
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
  受理表にない `filter`・`flatten` が `true` 以外・型不正は従来どおり拒否する（`filter` 省略時の挙動は #1334 の後も変わらない）
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
- discover 有効な接続（`Target.setDiscoverTargets`。#1324・`CDP-3`）では `Target.targetCreated` を `attachedToTarget` より前に送る。
  Playwright は `setDiscoverTargets` を送らないため、Playwright の経路は変わらない
- 未実装: 任意 URL の読み込み、window 系 params、
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
- 未実装のまま: `Target.disposeBrowserContext`（`Target.createTarget` は段階 2e〔#910〕、`Target.getBrowserContexts` は #1323 で実装済み。後者は自接続所有の非既定 context のみ返す）

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
- **`Runtime.executionContextCreated` は評価器が有効なときだけ送る**（TASK-44.6・#1217。`REPAIR-3`）。core の評価器（`ScriptContextHandle`）が
  無効な構成では 0 件のまま（使えない `context.id` を見せると偽の応答になるため）。有効な構成では、`Runtime.enable` の初回（確定文書があるとき）・
  `Runtime.runIfWaitingForDebugger` の再開時・`Page.navigate` の確定時に、main と `Page.addScriptToEvaluateOnNewDocument` の `worldName`
  の isolated world を告知する（遷移時は `Runtime.executionContextsCleared` → `Page.frameNavigated` → created → `Page.loadEventFired`）。
  main world の評価（`Runtime.evaluate`）は未実装で CDP エラーになる（#1218）。`Page.createIsolatedWorld` は有効な構成で contextId を返し、
  無効な構成では `-32000 execution contexts unavailable`。`consoleAPICalled` は送らない。`Network.*`（`requestWillBeSent`・`responseReceived`・
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
- **送らない名前**（`REPAIR-3`。`DOMContentLoaded` は #1322 で送出に変更。下の「DOMContentLoaded の lifecycleEvent 送出」節）: `DOMContentLoaded`・`networkAlmostIdle`・`networkIdle`・`commit`・`firstPaint` 系・`InteractiveTime`。実体が無く、
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
| `Target.setAutoAttach`（`sessionId` 付き） | 生存セッション・`autoAttach` が bool。`autoAttach:true` は `flatten:true` 必須。`{}` を返し、`waitForDebuggerOnStart` とともに状態を保持する（`autoAttach:false` で解除）。イベントは 0 件 | 受理表にない `filter` 付き（ページのセッションでは `[{type:page,exclude:true},{}]` も）・`autoAttach:true` で `flatten` が `true` 以外は `-32602 unsupported params`、`autoAttach` 欠落・型違い・未知セッションは `-32602 invalid params` |

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

次の作業: #1041・#1042（`Emulation.*` 等）。frame id と targetId の関係は段階 3f（#1050）で解決した。

### 段階 3f の実施結果（TASK-43.3o・#1050。実測 A）

**実装範囲**（`CDP-2`・`REPAIR-3`・`SEC-2`）: メインフレームの frame ID を、ターゲットあり（`sessionId` 付き）なら targetId と同じ値にした
（Chrome と同じ約束事。playwright-core 1.63.0 の `CRPage._sessionForFrame` が前提とする）。

| 箇所 | frame ID |
| ---- | -------- |
| `Page.getFrameTree` の `frame.id` | targetId |
| `Page.frameNavigated` の `frame.id` | targetId |
| `Page.lifecycleEvent` の `frameId` | targetId |
| `Page.navigate` 応答の `frameId`（成功・失敗・追い越しの全経路） | targetId |
| `sessionId` なしのブラウザレベル `Page.navigate` | 従来どおり `"main"`（targetId が無いためのフォールバック。既存の期待値を変えない） |

- `Page.loadEventFired` は `frameId` を持たない（CDP 仕様どおり）ため変更なし。`Page.navigate` の `frameId` パラメータは従来どおり無視する。
  `frameId` を入力に取る既存メソッドは無い。子フレームのモデルは持たない（`REPAIR-3`）。
- 識別面（UA・ヘッダ・`navigator`・画面・TLS・locale）には触れない（`SEC-2`）。
- 期待値を変えたテストは、セッション付きの frame ID が `"main"` から targetId になった仕様変更によるもので、弱体化ではない。

**実測**（`playwright-core` 1.63.0。トレースを再取得し 2 回の実行で同一出力）: `Page.getFrameTree` の `frame.id` は targetId になり、
`browserContext.newPage: Frame has been detached.` は解消した。次の送信 `Page.createIsolatedWorld`（id 20）は `-32601`、`newPage` は
`Emulation.setFocusEmulationEnabled` の Protocol error（`method not implemented`）で失敗する。`failed` は id 7・16・17・18・20。

次の作業: #1041・#1042（`Emulation.*`・`Page.createIsolatedWorld` 等）。

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
| `Emulation.setFocusEmulationEnabled` | 生存セッションの `sessionId` 付き・`params` が `{"enabled": true}`（Playwright 既定値）のみ。`{}` を返し、受理した印を保持する（冪等）。止めたターゲットでも受理する | `sessionId` なし・`enabled` 以外のキー・`enabled:false` は `-32602 unsupported params`、`enabled` が無い・bool でない・未知／閉じたセッションは `-32602 invalid params` |
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
`failed` から消えた。同梱の `newpage-trace.jsonl` では id 12（空 source の `Page.addScriptToEvaluateOnNewDocument`）は `{"identifier":"1"}`、
id 14（セッション付き `Target.setAutoAttach`）は `{}` で成功し（#1040 で対応済み）、`failed` は空で CDP エラーは無い。
TASK-43.3o（#1050）の取り込み後に再取得した同梱トレース（3 回の実行で同一出力）では `Frame has been detached.` が解消し、`newPage` は成功する
（stage の `ok:true`）。直後に `Page.createIsolatedWorld`（id 18）が送られるが、`newPage` は応答を待たないためトレースに応答（実際は未実装の `-32601`）は
記録されず、`Target.closeTarget` も送られない。`failed` は空のまま。

次の作業: 後述「到達結果（TASK-43.3n）」で到達を確認した（#1042）。

### 到達結果（TASK-43.3n・#1042。実測 A）

`CDP-2`・`CDP-6`・MS-4。基準は main の `03fda9d0`、playwright-core 1.63.0、node v24.21.0、収集オプションは `connectOverCDP(noDefaults:true)`・`newContext({acceptDownloads:false, viewport:null})`。

- 結果: 3 stage（`connectOverCDP(http)`・`newContext`・`newPage`）がすべて成功。`first_stop` なし、`failed` は空。
  失敗時のクリーンアップ（`Target.closeTarget` の送信・`Target.detachedFromTarget` の受信）は無い。
- 段階 3 の結果の要約:

| 段階 | issue | 内容 |
| ---- | ----- | ---- |
| 3a | #1037 | `Page.enable`・`Page.getFrameTree` |
| 3b | #1038 | `Log.enable`・`Runtime.enable`・`Network.enable` |
| 3c | #1039 | `Page.setLifecycleEventsEnabled` |
| 3d | #1040 | 空 `source` の `Page.addScriptToEvaluateOnNewDocument`・セッション付き `Target.setAutoAttach` |
| 3f | #1050 | frame ID = targetId |
| 3e | #1041 | `Emulation.setFocusEmulationEnabled`・`setEmulatedMedia`（`viewport:null`） |

- 段階 4 の要否の最終判断: **不要**。「予測 #11 の訂正」（種別 B）を実測 A で裏づけた。初期 `about:blank` の `Page.frameNavigated` を送らずに
  `newPage()` が成功し、トレースに `Page.frameNavigated`・`Page.lifecycleEvent` は 0 件。過去の訂正節は書き換えない。
- 残る `-32601`: `Browser.getWindowForTarget`（`viewport:null` のため送られない。ウィンドウ境界の実体が無く値を作ると偽になるため、オーナー方針 2026-10-15 どおり `-32601` のまま）と、
  `Page.createIsolatedWorld`（`newPage()` が応答を待たない。段階 3f の「次の作業」にある実装は #1042 では行わない）。
- 収集の非決定性: `Page.createIsolatedWorld`（id 18）の `-32601` 応答が `newPage` stage の前後どちらに届くかは競合で、再取得 4 回中 1 回は応答が記録され
  （seq 41、stage は seq 42 へずれる）、`failed` が 1 件になる。stage の結果は毎回成功。同梱トレースは応答なしの版を固定している（手で編集していない）。
- 範囲外: `goto()`・セレクタ取得は TASK-44（`CDP-2`）。
- 固定するテスト: `cdp2_playwright_newpage_is_reached_without_failures_or_cleanup`（`tests/playwright_trace.rs`）。

### 段階 TASK-44.3 の実施結果（#478。実測 A）

`CDP-2`・MS-4。基準は main の `fd7bbb72`、playwright-core 1.63.0、`connectOverCDP(noDefaults:true)`・`newContext({acceptDownloads:false, viewport:null})`。

- ループバック許可: 本番の `endpoints()` の `Page.navigate` は内部アドレス拒否（SSRF ガード）のまま。実テストは feature `test-support` の
  `test_support::endpoints_allowing_loopback` で、`FetchOptions` を差し替えた組込み表のサーバーを起動する（既定ビルドには含まれない）。
- 5 段階: `connect` → `newContext` → `newPage` → `goto`（ローカル fixture）→ `selector`（`#target` のテキスト `Hello fixture`・属性 `data-role="greeting"` を具体値で照合）。
- 実測結果（`make test-playwright-connect`）: `connect`・`newContext`・`newPage`・`goto` は成功。`goto` 段階の成功を確認した（fixture への到達 assert は全段階成功後のため未実行）。
  `selector` は `StageTimeout`（10 秒）で失敗した。サーバー側ログに `Page.createIsolatedWorld` の未実装受信があり、`Runtime.callFunctionOn`・`Runtime.evaluate` も未実装（`-32601`）。
- 原因の見立て（B）: locator は utility world に注入スクリプトを載せて評価する。`Runtime.executionContextCreated` を送らず実行コンテキストが成立しないため、応答待ちで止まる。
- 扱い: 未実装を成功に見せるフォールバックは入れない（`CDP-6`・`SEC-2`）。期待値の弱体化・skip もしない。実テストは `#[ignore]` のまま失敗が正で、
  Runtime ドメインの実装 Issue の完了後に通る見込み。受入基準のうちセレクタ取得は未達。

### テスト結果レポート（TASK-44.4・#479。実測 A）

`CDP-2`（関連: `CDP-6`・`SEC-2`・`REPAIR-3`）・TASK-44（44.4）・MS-4。Playwright の一連フロー（TASK-44.1〜44.3 で整備）の到達可否を記録する。

#### 実測条件

| 項目 | 値 |
| ---- | -- |
| 基準コミット | main の `61ffb1c4` |
| 実測日 | 2026-10-16 |
| playwright-core | 1.63.0（出典: `scripts/versions.sh` の `PLAYWRIGHT_VERSION`。integrity は `verify-install.mjs` で検証。`--ignore-scripts`・ブラウザ本体は取得しない） |
| node | v24.21.0 |
| 接続オプション | `connectOverCDP(noDefaults:true)`・`newContext({acceptDownloads:false, viewport:null})` |
| ループバック | feature `test-support` の `endpoints_allowing_loopback` のみ。本番の `endpoints()` の内部アドレス拒否（SSRF ガード）は変えていない |
| fixture | `127.0.0.1` だけに配信。期待値は `#target` のテキスト `Hello fixture`・属性 `data-role="greeting"` |

#### 5 段階の到達結果

| 段階 | 結果 | 失敗コード・内容 | 根拠 |
| ---- | ---- | ---------------- | ---- |
| `connect` | 成功 | なし | A（`make test-playwright-connect`） |
| `newContext` | 成功 | なし | A |
| `newPage` | 成功 | なし | A |
| `goto` | 成功 | なし（`Page.navigate` が `loaderId` を返し、`Page.frameNavigated`・`Page.loadEventFired` が届く） | A |
| `selector` | 失敗 | `StageTimeout`: `stage timed out after 10000 ms` | A |

`make test-playwright-connect` は `cdp2_real_playwright_goto_and_selector` が `selector` 段階の失敗で終了コード 2 になる。これは現状の正しい結果であり、skip・期待値の弱体化・成功フォールバックは入れていない（`CDP-6`・`SEC-2`）。

#### 失敗段階（selector）の CDP トレース

実測 A。`DEBUG=pw:protocol` を付けて `harness/playwright-connect/connect.mjs` を直接実行して採取した。
採取にはサーバー起動用の `examples/trace_server.rs` へ、feature `test-support` のときだけ `endpoints_allowing_loopback` を使う一時変更を入れた（採取後に元へ戻し、コミットしていない）。
ポートは `<PORT>`、一時パスは `<ARTIFACTS_DIR>` と読み替え、時刻・ANSI は除いて要約した。`goto` 以降の区間は次のとおり。

| 順 | 方向 | メッセージ | 結果 |
| -- | ---- | ---------- | ---- |
| 1 | send | `Page.createIsolatedWorld`（id 18。`worldName` は `__playwright_utility_world_page@…`） | `-32601` `method not implemented`（サーバー側ログに `unimplemented method received: Page.createIsolatedWorld`） |
| 2 | send | `Page.navigate`（id 19。`http://127.0.0.1:<PORT2>/index.html`） | 成功（`frameId`・`loaderId`） |
| 3 | recv | `Page.lifecycleEvent`（`init`）・`Page.frameNavigated`・`Page.loadEventFired`・`Page.lifecycleEvent`（`load`） | 届く |
| 4 | - | （以降、クライアントは何も送らず、サーバーも何も送らない） | 10 秒後に `StageTimeout` |

- 応答の無い要求: なし（送った 19 件すべてに応答がある）
- `-32601` を返したメソッド: `Page.createIsolatedWorld` の 1 件のみ
- 重要な事実: `Runtime.enable` は空の成功（`{}`）を返すが、`Runtime.executionContextCreated` は 1 件も送られない。Playwright は `Runtime.callFunctionOn`・`Runtime.evaluate` を一度も送らずに実行コンテキストの成立を待ち続ける。TASK-44.3 の記述にあった「`Runtime.callFunctionOn`・`Runtime.evaluate` も `-32601`」は、実測では送信自体されていない（実行コンテキストが無い段階で止まるため）。

#### 原因の見立て（B）

実測ではなくソース読解（playwright-core 1.63.0 の `lib/coreBundle.js`）に基づく。locator は frame の utility world の実行コンテキストへ注入スクリプトを載せて評価する。
サーバーが `Runtime.executionContextCreated` を送らず、`Page.createIsolatedWorld` も `-32601` のため、コンテキストが成立せず `StageTimeout` になる。

#### 扱い・未達の明示

- #250 の受入基準（セレクタで要素を取得すること）は未達。解消の前提は Runtime ドメインの最小実装（`Runtime.executionContextCreated` の送出・`Page.createIsolatedWorld`・`Runtime.callFunctionOn`・`Runtime.evaluate`）で、後続 Issue は #1062 で追跡
- 実装済みを装わない（`REPAIR-3`）。Runtime 実装後に `selector` が通ることを再実測して本節を更新する（TASK-44.9・#1220 で再実測済み。次節。本節の記録は当時のまま残す）

#### 再現手順

1. `make test-playwright-connect`（要 node/npm/ネットワーク。CI 対象外）で 5 段階の結果を得る
2. トレースは、上記の一時変更を入れた `trace_server` を `cargo run -p fandhe-browser-cdp --features test-support --example trace_server` で起動し、`DEBUG=pw:protocol`・`FANDHE_CDP_WS_ENDPOINT`・`FANDHE_PLAYWRIGHT_MODULE_DIR`・`FANDHE_FIXTURE_URL` を渡して `node harness/playwright-connect/connect.mjs` を実行する。ハーネスとしての恒久化は今後の課題

### 再実測（TASK-44.9・#1220。実測 A）

`CDP-2`（関連: `CDP-6`・`SEC-2`・`REPAIR-3`・`PERF-7`）・TASK-44（44.9）・MS-4。#1216〜#1219 で揃った Runtime ドメインの最小実装を、cli 本番と接続試験のテストサーバーの両方で有効にして `selector` 段階を再実測した。

#### 実測条件

| 項目 | 値 |
| ---- | -- |
| 基準コミット | main の `fc619994` |
| 実測日 | 2026-10-22 |
| playwright-core | 1.63.0（出典: `scripts/versions.sh`。`verify-install.mjs` で integrity 検証済み） |
| node | v24.21.0 |
| エンジン / feature | V8（cli の既定と同じ）/ `test-support,js-v8` |
| 評価コンテキストの配線 | cli は `commands::launch::start_server` が `[js] engine` の解決結果から `ScriptContextHandle::new` を作り `AppState` へ渡す（エンジンなしのビルドでは無効のまま。新しい設定キーは無い）。テストサーバーは同じ `ScriptContextHandle::new(&JsConfig::default())` を `start_server_allowing_loopback_with_scripts` で渡す |
| 接続オプション・ループバック・fixture | TASK-44.4 節と同じ（ループバック許可は `test-support` 限定） |

#### 5 段階の到達結果

| 段階 | 結果 | 失敗内容 |
| ---- | ---- | -------- |
| `connect` | 成功 | なし |
| `newContext` | 成功 | なし |
| `newPage` | 成功 | なし |
| `goto` | 成功 | なし |
| `selector` | 失敗 | `locator.count: Error: script evaluation failed: DOM operation failed: unsupported: unsupported selector syntax (universal selector) at byte offset 0` |

`StageTimeout`（実行コンテキストが成立しない）は解消し、評価器が注入スクリプトを実行して core の query に到達するようになった。失敗の原因は core のセレクタパーサー（`selector.rs`）が `*` を未対応として拒否することと推定される（プロトコルトレースでの確定は未実施）。skip・期待値の弱体化・成功フォールバックは入れていない。

#### 扱い・未達の明示

- #250 の受入基準（セレクタで要素を取得すること）は未達。universal selector の対応（#1237）後の再実測は次節
- 残る差異（`REPAIR-3`）: main world の評価は未対応（固定文言で拒否）、`Runtime.getProperties` は未実装、評価中の DOM 変更は `NavigationResult` へ書き戻さない、イベントループ・タイマーは回さない。#1237 の対応後に他の未対応 API が出た場合は同じ手順で追跡する

#### 再現手順

`make test-playwright-connect`（要 node/npm/ネットワーク。CI 対象外）。`run.sh` が `--features test-support,js-v8` で `playwright_flow`（`harness = false`）をケース名の引数付きで実行し、完了マーカー行が 1 件のときだけ成功とする。

### 再実測（#1237。実測 A）

`CDP-2`（関連: `CORE-1`・`CORE-5`・`REPAIR-3`）・TASK-44・MS-4。core のセレクタサブセットに全称セレクタ `*` を加えた（`CompoundSelector::is_universal`。詳細度は (0,0,0)。`*|a` などの名前空間と `div*` のような不正な並びは従来どおり拒否）うえで、`selector` 段階を再実測した。

#### 実測条件

| 項目 | 値 |
| ---- | -- |
| 基準コミット | main の `4f7ef849` に #1237 の変更を載せたもの |
| 実測日 | 2026-10-22 |
| playwright-core | 1.63.0（出典: `scripts/versions.sh`） |
| node | v24.21.0 |
| エンジン / feature | V8 / `test-support,js-v8` |

#### 5 段階の到達結果

| 段階 | 結果 | 失敗内容 |
| ---- | ---- | -------- |
| `connect` | 成功 | なし |
| `newContext` | 成功 | なし |
| `newPage` | 成功 | なし |
| `goto` | 成功 | なし |
| `selector` | 失敗 | `locator.textContent: ReferenceError: CustomEvent is not defined`（call log: `waiting for locator('#target').first()`） |

`stages.mjs` は `locator.count()` を先に呼んでから `locator.first().textContent()` を呼ぶ。前回失敗した `locator.count` は通過し、`*` の未対応は解消した。失敗は次の未対応 API である `CustomEvent`（js_shim にグローバルが無い）に移った。プロトコルトレースでの確定は未実施で、`CustomEvent` を Playwright の注入スクリプトが `textContent` の待機中に参照すると推定している。

#### 扱い・未達の明示

- #250 の受入基準（セレクタで要素を取得すること）は未達（5/5 には届いていない）。`CustomEvent` の提供（js_shim）を後続の課題として追跡する。skip・期待値の弱体化・成功フォールバックは入れていない（`CDP-6`・`SEC-2`）
- さらに先で別の未対応 API が出る可能性がある。出た時点で同じ手順で記録する

#### 再現手順

`make test-playwright-connect`（前節と同じ）。

### 再実測（#1242。実測 A）

`CDP-2`（関連: `JS-5`・`REPAIR-3`・`SEC-2`）・TASK-44・MS-4。js_shim（`events.js`）に `CustomEvent`（コンストラクタ・`detail`・`initCustomEvent`。`Event` を継承）と、要素（`Node.prototype`）の `dispatchEvent` を加えて `selector` 段階を再実測した。要素の `dispatchEvent` は対象要素の上の capture → 非 capture だけに配り、`bubbles` が true でも祖先・document・window へは伝播しない（木に沿った伝播は未実装。`REPAIR-3`）。`isTrusted` は常に false。

要素の `dispatchEvent` を足した理由は、playwright-core 1.63.0 の `markTargetElements`（`locator.textContent` が `markTargets: "first"` で呼ぶ）が `new CustomEvent(...)` を作って `element.dispatchEvent(...)` を呼ぶことをソースの読解で確認したため（推定。プロトコルトレースでの確定は未実施）。`CustomEvent` のみを足した中間状態は単独では実測していない（2 件を同時に補った）。

#### 実測条件

| 項目 | 値 |
| ---- | -- |
| 基準コミット | main の `e673e41a` に #1242 の変更を載せたもの |
| 実測日 | 2026-10-22 |
| playwright-core | 1.63.0（出典: `scripts/versions.sh`） |
| node | v24.21.0 |
| エンジン / feature | V8 / `test-support,js-v8` |

#### 5 段階の到達結果

| 段階 | 結果 | 失敗内容 |
| ---- | ---- | -------- |
| `connect` | 成功 | なし |
| `newContext` | 成功 | なし |
| `newPage` | 成功 | なし |
| `goto` | 成功 | なし |
| `selector` | 失敗 | `locator.textContent: TypeError: Cannot read properties of undefined (reading 'length')`（call log: `waiting for locator('#target').first()`） |

`CustomEvent is not defined` は解消した。失敗は別の `TypeError` に移ったが、どの式で `undefined` の `length` を読んだかは未特定だった（後続の「再実測（#1245）」で `Element.attributes` と判明。当時はページ内の注入スクリプトか Playwright 側かも未確定）。参考として、注入スクリプトの `_refreshListeners` は `document` 上の `__playwright_mark_target__` リスナーで `event.composedPath()[0]` を読むが、`Event.composedPath` は未実装で、要素上の `dispatchEvent` は document へ伝播しない。これらが原因かどうかは未検証。

#### 扱い・未達の明示

- #250・#1062 の受入基準（セレクタで要素を取得すること）は未達（5/5 には届いていない）。close の判断はオーナーに残す。次の阻害要因（上記 `TypeError` の原因特定。`composedPath`・木に沿った伝播・nth セレクタ周辺が候補）は別 Issue の候補で、本 Issue では追わない（計画の「1〜2 件まで」の範囲を超えるため）。skip・期待値の弱体化・成功フォールバックは入れていない（`CDP-6`・`SEC-2`）

#### 再現手順

`make test-playwright-connect`（前節と同じ）。

### 再実測（#1245。実測 A）

`CDP-2`（関連: `JS-5`・`REPAIR-3`・`SEC-2`）・TASK-44・MS-4。#1242 の記録で原因未特定だった `locator.textContent` の `TypeError: Cannot read properties of undefined (reading 'length')` を解消し、`selector` 段階を再実測した。

#### 原因

- 未定義だったのは `Element.prototype.attributes`。呼び出し元は playwright-core 1.63.0 の `Frame.textContent` → `_waitForFunctionOnSelector` → InjectedScript の `previewNode(element)` で、`for (let i = 0; i < element.attributes.length; i++)` が `undefined.length` を読んでいた（fixture の `#target` は `data-role` 属性を持つためループに入る）。`locator.count()` は `previewNode` を呼ばないため通り、`textContent()` だけが落ちた
- 根拠は `playwright-core@1.63.0` の `lib/coreBundle.js` のソース読解（推定）。修正後の再実測で例外が消え 5 段階が通ったことをもって確認とした（プロトコルトレースでの確定は未実施）
- #1242 の記録で候補に挙げた `composedPath`・木に沿った伝播・nth セレクタは、この経路の原因ではなかった（要素の `dispatchEvent` が document へ伝播しないため `composedPath` を読むリスナーまで到達しない）

#### 対応

- core の bridge に読み取り専用 op を追加した: `attributeCount`・`attributeNameAt`・`attributeValueAt`（`dom_bridge.rs`。index は有限・非負・整数・2^53 未満を検証し、範囲外は null、名前・値は応答上限 `JS-6` に従う）
- `dom.js` に `Element.prototype.attributes` を追加した。簡略版（`REPAIR-3`）: live でない静的な Array（`item`・`getNamedItem` つき）で、呼ぶたびに作り直す。`Attr` は読み取り専用で `Node` を継承せず、prefix 付き属性の `localName` は qualified name のまま・`prefix`・`namespaceURI` は null。名前付きプロパティアクセス・`setNamedItem` 等・グローバルの `NamedNodeMap` / `Attr` は未実装

#### 実測条件

| 項目 | 値 |
| ---- | -- |
| 基準コミット | main の `bd1c8be9` に #1245 の変更を載せたもの |
| 実測日 | 2026-10-22 |
| playwright-core | 1.63.0（出典: `scripts/versions.sh`） |
| node | v24.21.0 |
| エンジン / feature | V8 / `test-support,js-v8` |

#### 5 段階の到達結果

| 段階 | 修正前（#1242 の記録） | 修正後 |
| ---- | ---------------------- | ------ |
| `connect` | 成功 | 成功 |
| `newContext` | 成功 | 成功 |
| `newPage` | 成功 | 成功 |
| `goto` | 成功 | 成功 |
| `selector` | 失敗（`TypeError ... reading 'length'`） | 成功 |

成功判定は完了マーカー行 `playwright_flow: real playwright case passed (1 case)` が 1 件であること（`harness/playwright-connect/README.md`）。

#### 扱い・未達の明示

- 5/5 に到達した。#250・#1062 を close するかの判断はオーナーに残す
- 範囲外として記録のみ: 要素の `dispatchEvent` の木に沿った伝播（capture・bubble が document・window へ届くこと）と `Event.composedPath`、live な `NamedNodeMap`。skip・期待値の弱体化・成功フォールバックは入れていない（`CDP-6`・`SEC-2`）

#### 再現手順

`make test-playwright-connect`（前節と同じ）。

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

### ダウンロード実行系の判断材料（#943。オーナー判断待ち）

本節は判断材料の整理であり、決定を含まない。実装するかどうかと範囲は人間（オーナー）が決める（`CDP-2`・`CDP-6`・TASK-43・MS-4）。

#### 前提の充足状況

| 項目 | 事実 | 出典 |
| ---- | ---- | ---- |
| `newPage()` 到達 | 到達済み。`noDefaults:true`・`acceptDownloads:false`・`viewport:null` の指定が条件 | 上記「到達結果（TASK-43.3n・#1042）」 |
| TASK-71 の実測 | TASK-71.3（#312）は完了。`harness/compat-practical/tasks.json` の 28 タスクにダウンロードを要するものは 0 件 | `tasks.json` |
| spec の要件 | `04-behavior/` にダウンロードの要件は無い。`CDP-2` の操作（`newContext()` → `newPage()` → `goto()` → セレクタ取得）にも含まれない | `CDP-2` |

現状の制限（既定のオプションでは `allowAndName` が `-32602` で拒否される）は「段階 2g の実施結果」を参照。
`allow*` を受けて何もせず `{}` を返す案は、#924・#942 の判断と「原則」（空の成功で通さない）に反し、検出回避として作用しうる（`SEC-2`・`CDP-6`）ため選択肢に含めない。

#### 選択肢 A: 実装する

決めること:

- 保存先: プロファイル保管場所の配下に限るか、`downloadPath` の指定を受けるか。受ける場合はルート配下であることの検証と symlink の扱い（`PROF` 系・「段階 2c の実施結果」の記載どおり、現状は検証のみでファイル操作はしない）
- 上限: 1 件あたりのサイズ上限と同時ダウンロード数の上限（`CDP-7` の上限の考え方との整合）。上限なしの保存はディスク枯渇（DoS）になる
- イベント: `Browser.downloadWillBegin`・`Browser.downloadProgress` を送出する範囲と、`allowAndName` の名前付け（GUID）。`Content-Disposition` 由来のファイル名は untrusted で、保管場所外へ出さない
- 単位: context 単位の設定（「段階 2b の実施結果」）との関係と、context・プロファイル間で保存先を共有しないこと

範囲が決まったら、実装 Issue を別途起票する（起票は人間の判断を経る）。

#### 選択肢 B: 実装しない

決めること:

- `CDP-2` の適合判定を「`noDefaults:true`・`acceptDownloads:false` の指定つき」で行うと明記するかどうか。明記する場合は spec リポ側の変更になる（本リポから `docs/spec` は編集しない）
- 本リポでは設計書の制限の記述（段階 2g）を残す

#### Claude の提示（推奨。決定ではない）

TASK-71 の対象にダウンロードが無く、spec にも要件が無いため、現時点では B（オプション指定つきで判定し、制限を明記したまま）を提示する。ダウンロードを要する対象タスクや利用者の要望が出た時点で A を見直す。

## DOMContentLoaded の lifecycleEvent 送出（#1322・`CDP-2`・`CDP-4`）

TASK-46（#252）の測定で `page.goto(url, { waitUntil: "domcontentloaded" })` が 28 件すべて時間切れになった。
Playwright は `Page.lifecycleEvent` の `name == "DOMContentLoaded"` を受けたときだけ domcontentloaded を満たす。
Puppeteer の `LifecycleWatcher` も同じ。これを受けて `Page.navigate` の確定時に次を送る（`page.rs` の `navigation_events` が正本）。

- **順序**: `[lifecycle{init}]` → `[Runtime.executionContextsCleared]` → `Page.frameNavigated` → `[Runtime.executionContextCreated…]` →
  `[Page.domContentEventFired]` → `[lifecycle{DOMContentLoaded}]` → `Page.loadEventFired` → `[lifecycle{load}]`。
- **ゲート**: `lifecycle{DOMContentLoaded}` は `init`・`load` と同じ（`Page.enable` 済みかつ lifecycle 有効）。
  `Page.domContentEventFired` は `Page.enable` 済みのセッションに限る（Chromium の規則。新しいイベントなので、
  既存の `frameNavigated`・`loadEventFired` のような互換上のゲート免除は持たない）。ブラウザレベル（`sessionId` なし）の遷移では出ない。
  ゲートは `target.rs` の `event_gates` が 1 回のロックで返す。
- **送出条件**: core の `page_runner` が記録した `DOMContentLoaded` の発火結果（`PageRunOutput::lifecycle()`）で決める。

  | 経路 | 送るか | 理由 |
  | ---- | ------ | ---- |
  | JS を実行していない（既定設定・`about:blank`・`Skipped`・`BaseUrlRejected`・`Failed`） | 送る | 取得した HTML は解析済みで、リスナーも存在し得ない |
  | `Executed` で記録が `Dispatched` | 送る | 実際に発火した |
  | `Executed` で記録が `Aborted`・`NotFired`・無し・未知の結果 | 送らない | 発火したと言えない（fail-closed。偽の値で埋めない。`SEC-2`） |

- **timestamp**: 送出時刻で、`init` ≦ `DOMContentLoaded` 系 ≦ `load` 系の非減少。同種のペア（`domContentEventFired` と
  `lifecycle{DOMContentLoaded}`、`loadEventFired` と `lifecycle{load}`）は同じ値（Chromium も同様）。実際の発火時刻ではない
  （スクリプトは commit 前に走る）。厳密増加に見せかける人為的なオフセットは付けない。
- **差異（`REPAIR-3`）**: load が打ち切りで発火しなかった文書でも `loadEventFired`・`lifecycle{load}` は従来どおり送る
  （DOMContentLoaded との非対称。変えると `waitUntil: "load"` の待機が止まるため別途判断）。
- **検証**: 単体テスト・ディスパッチテストで順序・params・timestamp・打ち切り時の非送出を具体値で確認（種別 A）。
  実 Playwright は `make test-playwright-connect` の goto 段階が `waitUntil: "domcontentloaded"` に続き `waitForLoadState("load")` で通ることを確認した（種別 A、
  ループバック fixture）。TASK-46 の再測定（成功率の判定）は未実施で、人間担当。
- フラグ・イベントは fetch・ヘッダ・UA・`navigator`・画面・TLS・locale に触れない（`SEC-2`）。

## 未確定事項・再現手順

- #3 以降は B（ソース読解）で、1.63.0 に対する実測は未実施。#1・#2 の解消後にトレースを再取得して検証する
- #1 の 404 が、ルータ登録（`server.rs`）と依存先ルーティングの完全一致仕様のどちらに由来するかは未切り分け
  （トレースは 404 の事実のみ）
- spec（`docs/spec`）は本調査環境で未取得のため、C の記述は spec 本文と未照合
- 再現: `make trace-playwright`（`harness/playwright-trace/README.md`。ネットワークと node/npm が必要）。
  ソース確認は `npm pack playwright-core@1.63.0` で得た `lib/coreBundle.js` を一時ディレクトリで読む
