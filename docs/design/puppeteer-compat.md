# Puppeteer 互換: 接続試験の結果レポート

- **対応ビヘイビア**: CDP-3（関連: CDP-2・CDP-6・SEC-2・REPAIR-3。`docs/spec/04-behavior/` が SSOT）
- **対応タスク**: TASK-45（サブ 45.3・Issue #482）。入力は TASK-45.1（Issue #480）・TASK-45.2（Issue #481）の試験基盤
- **マイルストーン**: MS-4
- **基準コミット**: 読解（B）は `42ce7c0`（2026-10-06）。実測（A）は `683ca65a`（`origin/main`。2026-10-30・Issue #1349。`selector` 前半まで）と `633e0100`（`origin/main`。2026-10-31・Issue #1358。`selector` 後半と `disconnect` を含む全段階）
- **調査日**: 2026-10-06（読解）・2026-10-30（実測）・2026-10-31（再実測）
- **調査対象の版**: puppeteer-core 25.12.0（npm の最新版。integrity `sha512-z6LQUt5SH7jwGtGA+nMtJuEjZIsqPiwQ+EFHCYuVDQrJpvifNmmGuyDvTzAKPyFzihxgHc5f3aMzu2kn8Ps37A==`）・node v24.21.0。
  これは読解した公開物の記録であり、依存としての採用・固定ではない（測定時の一時導入のみ承認済み〔2026-10-29・#252〕。CI への組み込み・lockfile のコミットは未承認。実測は一時導入で行い、削除済み。2026-10-31 の再実測も同じ承認の範囲で、新たな承認は得ていない）

## 目的・前提

`puppeteer.connect({browserWSEndpoint})` → `browser.newPage()` → `page.goto("about:blank")` → `page.$("body")` →
`browser.disconnect()` の各段階が fandhe-browser の CDP サーバーでどこまで到達するかを記録する。
本書は記録までで、主軸の決定（Playwright と Puppeteer のどちらを主軸にするか）と追加実装は含めない。`Target.setAutoAttach` の `filter` の扱いのみ「方針」節で決定している。

**重要: 2026-10-31 の再実測（A。Issue #1358・`633e0100`）で、`connect`・`newPage`・`goto`・`selector`・`disconnect` の全 5 段階が ok だった（3 回実行して 3 回とも同じ）。**
2026-10-30 の実測（Issue #1349）では `selector` が `Runtime.evaluate` の `-32602 unsupported params`（`userGesture`）で failed、`disconnect` が未到達だった。#1357（PR #1359）で `userGesture` を効果なしで受理した後は、
`puppeteerUtil` の注入・`queryOne`・isolated → main のハンドル移送・`Runtime.releaseObject` まで通り、新しい停止点は出なかった。
ただし範囲は限られる: `about:blank` のみ・`page.$("body")` の 1 経路のみ・debug ビルド・Linux のみで、成功を保証する範囲は実行した経路に限る。
`puppeteer-core` は測定時の一時導入のみ承認済み（2026-10-29・#252。`harness/endpoint-swap/`）で、
`package.json`・CI ジョブは追加していない（`harness/puppeteer-connect/README.md`「導入状況」）。実測していない行（不足箇所一覧の B の行）は
ソース読解に基づく予測のままで、実測済みとは書かない（`REPAIR-3`）。

| 種別 | 内容 | 確度 |
| ---- | ---- | ---- |
| A（実測） | 実 Puppeteer（puppeteer-core 25.12.0）による段階別 status・CDP トレース（`NODE_DEBUG='puppeteer:*'`）。2026-10-30 に 1 回、2026-10-31 に 3 回実行（再現手順は末尾の節） | 実測。ただし 1 回・`about:blank` のみ・debug ビルド・Linux のみ。成功を保証する範囲は実行した経路に限る |
| B（ソース読解） | npm 公開物 puppeteer-core 25.12.0 の `src/cdp/` 配下（`BrowserConnector.ts`・`Browser.ts`・`TargetManager.ts`・`FrameManager.ts`・`Page.ts`・`Frame.ts` 等）の該当箇所を読んだ結果 | 実測していない。ソース上そう読める、という記録 |
| C（二次情報） | spec の PoC-5（`docs/spec/03-poc/cdp-compat/`）の「Puppeteer は Playwright より CDP 依存が薄いとされる」（推測・未検証） | 本調査環境では `docs/spec` を再照合していない |

Puppeteer のソースは長く引用せず、クラス・関数名と要約に留める。

## 到達段階のまとめ

試験基盤の段階は `harness/puppeteer-connect/stages.mjs` の `connect` → `newPage` → `goto` → `selector` → `disconnect`。

| 段階 | 実測（A。2026-10-31・633e0100） | 予測（B。2026-10-06 時点） | 予測の根拠と実測との差 |
| ---- | ------------------------------ | ------------------------- | ---------------------- |
| `connect` | ok | 到達する見込み | 的中（再実測でも変わらず）。`Target.getBrowserContexts`（#1323）→ `Target.setDiscoverTargets`（#1324）→ root の `Target.setAutoAttach`（#1334）が通り、`puppeteer.connect` が完了した（`defaultViewport: null`） |
| `newPage` | ok（再実測でも変わらず） | 未到達の見込み（`Page.addScriptToEvaluateOnNewDocument` で停止すると予測） | 外れ（#1340・#1334 の後は到達する）。`Target.createTarget` → tab → ページの attach、`Page.enable`・`Page.getFrameTree`・`Runtime.enable` 等の初期化、`Page.addScriptToEvaluateOnNewDocument`（sourceURL のみの source）、`Page.createIsolatedWorld` が成功した。`WebMCP.enable` は `-32601` だが Puppeteer は握りつぶして進んだ |
| `goto` | ok（再実測でも変わらず） | 未到達の見込み | 外れ。`Page.navigate` の応答後に `Page.lifecycleEvent`（`init`・`DOMContentLoaded`・`load`）・`Page.frameNavigated`・`Runtime.executionContextsCleared` と新しい contextId の `executionContextCreated`（main・isolated 各 1）が届き、完了した |
| `selector` | ok（3 回中 3 回）。前回（2026-10-30・683ca65a）は failed: `Runtime.evaluate` の `-32602 unsupported params`（`userGesture`） | 未到達の見込み（`Runtime` なしで停止すると予測） | 外れ。main の `Runtime.callFunctionOn`（#1345・#1347）→ `DOM.describeNode` → `DOM.resolveNode`（#1348）に続き、`userGesture` 受理後（#1357）は isolated の `puppeteerUtil` 注入、`queryOne` の `callFunctionOn`（引数 3 件）、isolated → main の `describeNode`／`resolveNode`、`Runtime.releaseObject` がすべて成功し、`page.$("body")` が要素ハンドル（`description:"body"`）を返した（下記「`page.$` の呼び出し連鎖」） |
| `disconnect` | ok（3 回中 3 回）。前回は未到達（`not_reached`） | 先行段階の失敗後は後始末のみ | `browser.disconnect()` は成功し、トレース上は切断時の CDP メッセージ送信が無かった（最後の送信は `Runtime.releaseObject` で、その後に送受信なし）。サーバー側にも異常ログは無い |

実測の出力（`FANDHE_SCRIPT_RESULT` 行の要旨）: 2026-10-31 の 3 回とも `ok:true`・`step:"disconnect"`・`stages` の 5 段階すべて `ok`（`error` は null）。
2026-10-30 の実測は `ok:false`・`step:"selector"`・disconnect が `not_reached` だった（履歴）。
段階の期限（既定 10 秒）には当たっていない。サーバーの stderr に出た「unimplemented method」は `WebMCP.enable` と `Runtime.addBinding` の 2 種だけ（3 回の実行で同じ）。

`-32601` は未実装メソッドへ成功を捏造しない `CDP-6`・`SEC-2` の方針どおりの挙動で、バグではない。
Playwright と異なり、`browserWSEndpoint` 指定の接続では HTTP の `/json/version` を使わない
（読むのは `browserURL` 指定時のみ。`common/BrowserConnector.ts` の `getWSEndpoint`）。
本試験基盤は `browserWSEndpoint` を使うため、Playwright 側の停止点 #1（`/json/version/` の 404）は Puppeteer の経路には現れない（B）。

## 不足箇所一覧

「現状の応答」は 2026-10-31（`633e0100`）時点の `crates/fandhe-browser-cdp/src/` の現実装に基づく。種別 A の行は実測で確認した応答、B の行は実測していない（コード上の記述）。#10・#11 は 2026-10-31 の再実測（Issue #1358）で更新した。
2026-10-06 時点の初版にあった `-32601` の記述は、その後の実装（#1323〜#1354）で陳腐化したため、実測またはコードに合わせて訂正した。

| # | 段階 | Puppeteer の箇所 | 送る CDP メソッド | 読む応答・待つイベント | 現状の応答 | 不足 | 種別 |
| - | ---- | ---------------- | ----------------- | ---------------------- | ---------- | ---- | ---- |
| 1 | connect | `_connectToCdpBrowser` | `Target.getBrowserContexts`（root。接続後の最初の送信） | `browserContextIds`（配列。既存 context の把握） | 実装済み（#1323）。自接続が所有する非既定 context の `browserContextIds` を作成順に返す（context 未作成なら空配列。既定 context は含めず、他接続の context も返さない） | なし（`TargetInfo` は `browserContextId` を持たない点は別途） | A（応答はトレースで確認。`connect` の完了） |
| 2 | connect | `TargetManager.initialize` | `Target.setDiscoverTargets` `{discover:true, filter}` | 応答は読み捨て。以後 `Target.targetCreated`・`targetInfoChanged`・`targetDestroyed` を購読 | #1324 で実装済み（`-32601` ではない）。`filter` は省略（ページのみ通知）か `[{}]`（ページと tab を通知。#1334）。既存ターゲットの通知は応答の後（Chromium は前。tab → 子ページの attach で初期化待ちが空になるため `initialize()` は完了する。ソース読解で、実測の範囲は `connect` の完了まで）。`targetInfoChanged` と browser 型の通知は未実装 | ハンドラと `Target.targetCreated`・`targetDestroyed` の送出（実装済み。`targetInfoChanged` は未実装） | A（応答はトレースで確認。`connect` の完了） |
| 3 | connect | `TargetManager.initialize` | `Target.setAutoAttach` `{autoAttach:true, waitForDebuggerOnStart:true, flatten:true, filter}`（root。`page` を除外するフィルタ付き） | 応答は読み捨て。以後 `Target.attachedToTarget` を購読し、既存 `tab` ターゲットの初期化完了まで `connect` が待つ | #1334 で実装済み: `[{type:page,exclude:true},{}]` を受理して tab 擬似ターゲットを attach し、tab のセッションの `Target.setAutoAttach` で子のページを attach する。受理しない `filter` は `-32602` | 実装済み（一般の TargetFilter の評価は未実装） | A（応答はトレースで確認。`connect` の完了） |
| 4 | newPage | `CdpBrowser._createPageInContext` | `Target.createTarget` `{url:"about:blank"}` | 応答の `targetId`。その後 `waitForTarget` で、同 `targetId` のターゲットが `targetCreated`／attach 経由で現れるのを待つ | 実測で成功。応答の前に `targetCreated`（tab → ページ）と `attachedToTarget`（tab）を送り、応答の `targetId` はページの ID（「tab 擬似ターゲットと `filter`」節。#1334） | なし | A |
| 5 | newPage | `TargetManager.#onAttachedToTarget` | イベント `Target.attachedToTarget`（`sessionId`・`targetInfo`）。受信後に `Target.setAutoAttach`（子 session）・`Runtime.runIfWaitingForDebugger` を送る | `targetInfo.targetId`・`type`・`url`・`browserContextId`。`waitForDebugger` 中のターゲットは `runIfWaitingForDebugger` で解放される | 実測で成功。tab（root のイベント）→ ページ（tab のセッションのイベント）の順に attach し、いずれの `setAutoAttach`・`runIfWaitingForDebugger` も `{}` で応答した（#1334） | なし | A |
| 6 | newPage | `CdpPage.#initialize`・`FrameManager.initialize` | page session で `Network.enable`・`Page.enable`・`Page.getFrameTree`・`Page.setLifecycleEventsEnabled`・`Runtime.enable`・`Audits.enable`・`Performance.enable`・`Log.enable`・`WebMCP.enable` を並行送信（実測の送信順） | `Page.getFrameTree` の `frameTree.frame` で frame を構築。いずれかが失敗するとページ初期化が失敗する（ターゲットが閉じた場合のエラーのみ握りつぶし） | 実測で、`WebMCP.enable` を除く 8 メソッドが成功（`Performance.enable`・`Audits.enable` は #1326。フラグ保持のみ・イベントなし）。`WebMCP.enable` は `-32601`（Puppeteer 25.12.0 が送る。実装していないため成功を装わない）で、Puppeteer は握りつぶして進んだ。`Runtime.enable` の応答後に main の `executionContextCreated` を送る | `WebMCP.enable` は未実装のまま（ページ初期化は止まらない） | A |
| 6b | newPage | `CdpPage.#initialize`・`FrameManager` 系 | page session で `Page.addScriptToEvaluateOnNewDocument` `{source:"//# sourceURL=pptr:internal", worldName:"__puppeteer_utility_world__25.12.0"}`（実行内容なし） | 応答の `identifier` を保持。失敗するとページ初期化が失敗する | 実測で成功（`identifier:"1"`）。#1325・#1340 で sourceURL コメントだけの `source` を no-op 登録で受理し、実行内容を持つ `source` は `-32602`（`SEC-2`・`CDP-6`） | 実行経路（`JS-4`・TASK-112）は未実装 | A |
| 7 | newPage | `FrameManager.#createIsolatedWorld` | `Page.createIsolatedWorld` `{frameId, worldName, grantUniveralAccess:true}` | `Runtime.executionContextCreated` を待って実行コンテキストを構築 | 実測で成功（`executionContextId:2`・直前に isolated の `executionContextCreated`）。遷移後は main・isolated の contextId が作り直される（3・4） | なし | A |
| 8 | newPage | `EmulationManager`（`defaultViewport` 既定 800x600） | `Emulation.setDeviceMetricsOverride`・`Emulation.setTouchEmulationEnabled` 等 | 結果は使わない（失敗でページ作成が失敗する経路） | 実測では `defaultViewport: null` のため送信されなかった（未実測）。`Emulation.*` は実装しない（画面の識別面。`SEC-2`） | `defaultViewport: null` を前提とする（「測定条件」） | B |
| 9 | goto | `Frame.goto`（`about:blank`） | `Page.navigate` `{url, frameId}`（実測では `referrer` 等は送られなかった） | 応答の `loaderId`・`errorText`。完了は `Page.lifecycleEvent`（`load` 等）・`Page.frameNavigated` を `LifecycleWatcher` が待つ | 実測で成功。応答の後に `Page.lifecycleEvent`（`init`）・`Runtime.executionContextsCleared`・`Page.frameNavigated`・main／isolated の `executionContextCreated`・`Page.domContentEventFired`・`lifecycleEvent`（`DOMContentLoaded`）・`Page.loadEventFired`・`lifecycleEvent`（`load`）の順に届いた（#1322 ほか） | なし | A |
| 10 | selector | `Page.$("body")` | `Runtime.callFunctionOn`（main）→ `DOM.describeNode` → `DOM.resolveNode`（isolated）→ `Runtime.addBinding` ×2 → `Runtime.evaluate`（`puppeteerUtil`）→ `Runtime.callFunctionOn`（`queryOne`）→ `DOM.describeNode` → `DOM.resolveNode`（main）→ `Runtime.releaseObject`（実測の送信順） | `RemoteObject`・`DOM.describeNode` の `node`・`resolveNode` の `object` | 実測で全件成功。`Runtime.addBinding` だけ `-32601` で握りつぶされた。`puppeteerUtil` の `Runtime.evaluate` は `userGesture:true` 付きで成功（`objectId:"4.2"`）。`queryOne` の `callFunctionOn` は `arguments` 3 件（要素の `objectId`・selector の `value`・`puppeteerUtil` の `objectId`。同じ isolated 内のハンドル）を受理し、`description:"body"` の要素を返した（下記「`page.$` の呼び出し連鎖」） | `Runtime.evaluate` の `userGesture` は #1357 で受理（効果なし。`REPAIR-3`）。`about:blank` の `body` 以外のセレクタ・ページスクリプトのある実ページは未実測（B） | A |
| 11 | disconnect | `CdpBrowser.disconnect` | 実測では CDP メッセージを送らず transport を閉じた（トレースに切断時の送信なし） | — | 実測で `browser.disconnect()` が成功（`stages` の disconnect が `ok`）。切断後にサーバー側へ異常ログは出なかった | なし（接続数の解放・ターゲットの扱いは未確認〔B〕） | A |

### 実測で確認できた停止点と予測の境界

2026-10-30 の実測（Issue #1349）で確認した停止点は 1 件で、2026-10-31 の再実測（Issue #1358）では解消していた。2026-10-31 の実測では新しい停止点は出なかった。

- **`Runtime.evaluate` の `userGesture`（解消済み。履歴）**: Puppeteer 25.12.0 は `puppeteerUtil`（約 10.5KB の式）の注入で `{expression, contextId:<isolated>, returnByValue:false, awaitPromise:true, userGesture:true}` を送る。
  2026-10-30 当時の `Runtime.evaluate` の許可キーは `expression`・`contextId`・`returnByValue`・`awaitPromise`・`silent` で、`userGesture` が未知キーとして `-32602 unsupported params` になった。#1357 で `userGesture` を許可キーに加えた（`crates/fandhe-browser-cdp/src/runtime/evaluate.rs`）。
  式の長さ（10513 文字）は上限（`MAX_SCRIPT_CONTEXT_SOURCE_BYTES`=1MiB）内で、上限は原因ではない（読解）。
  `Runtime.callFunctionOn` は `userGesture` を受理して効果なしで流す（ユーザー活性化の概念を持たないため。`REPAIR-3`）。#1357 で `callFunctionOn` と同じ扱い（bool の型検査だけ・効果なし）にそろえた。他の未知キーは `-32602` のまま。再実測（2026-10-31）では `userGesture:true` 付きの `Runtime.evaluate` と `callFunctionOn` が成功した

読解（B）の予測との差:

- 予測した停止点（#1〜#11 のうち `newPage` の `addScriptToEvaluateOnNewDocument`、`goto` の `lifecycleEvent`、`selector` の `Runtime` 全般）は、#1340・#1322・#1345・#1347・#1348・#1357 等で解消済みで、実測では停止点にならなかった
- `Runtime.addBinding`・`WebMCP.enable` の `-32601` は握りつぶされて進行を止めない（読解と一致）
- 2026-10-30 に「受理後に新たな停止点が出うる」と予測した 3 点（`puppeteerUtil` の注入式が評価できるか、`queryOne` の `callFunctionOn` が引数 3 つを受理するか、isolated → main の移送）は、再実測で停止点にならなかった。`disconnect`（#11）も成功した
- 残る限界: `about:blank` の `body` だけで、ページスクリプトの無い DOM である。複雑なセレクタ・実ページ・`page.$$`・`page.evaluate` 等は未実測（B）

### `page.$` の呼び出し連鎖（実測。A）

セッション種別は root／tab／ページ（ページ session）、world は main／isolated。順序は 2026-10-31 のトレース（3 回とも同じ列）による。`main-world-evaluation.md` §2 の読解との照合結果もそちらに記す。

| 順 | セッション | メソッド | world | 結果 |
| -- | ---------- | -------- | ----- | ---- |
| 1 | ページ | `Runtime.callFunctionOn` `{functionDeclaration:"() => { return document; }", executionContextId:<main>, arguments:[], returnByValue:false, awaitPromise:true, userGesture:true}` | main | 成功。`RemoteObject{type:object, subtype:node, className:Document, description:#document, objectId:"3.1"}` |
| 2 | ページ | `DOM.describeNode {objectId:"3.1"}` | main | 成功。`backendNodeId:1` 等 |
| 3 | ページ | `DOM.resolveNode {backendNodeId:1, executionContextId:<isolated>}` | isolated | 成功。`objectId:"4.1"` |
| 4 | ページ | `Runtime.addBinding {name:"puppeteer___ariaQuerySelector" と "puppeteer___ariaQuerySelectorAll", executionContextName:"__puppeteer_utility_world__25.12.0"}` ×2 | — | `-32601`。Puppeteer は握りつぶして進んだ |
| 5 | ページ | `Runtime.evaluate {expression:<puppeteerUtil 約 10.5KB>, contextId:<isolated>, returnByValue:false, awaitPromise:true, userGesture:true}` | isolated | 成功。`RemoteObject{type:object, className:Object, objectId:"4.2"}`（2026-10-30 は `-32602` で停止） |
| 6 | ページ | `Runtime.callFunctionOn {functionDeclaration:"(element, selector, { cssQuerySelector }) => { return cssQuerySelector(element, selector); }", executionContextId:<isolated>, arguments:[{objectId:"4.1"},{value:"body"},{objectId:"4.2"}], returnByValue:false, awaitPromise:true, userGesture:true}` | isolated | 成功。`RemoteObject{subtype:node, className:Element, description:"body", objectId:"4.3"}` |
| 7 | ページ | `DOM.describeNode {objectId:"4.3"}` | isolated | 成功。`backendNodeId:4`・`nodeName:BODY`・`parentId:2` |
| 8 | ページ | `DOM.resolveNode {backendNodeId:4, executionContextId:<main>}` | main | 成功。`objectId:"3.2"`（Puppeteer が `page.$` の戻り値のハンドルを main へ移す操作。関数の送信ではなく DOM 操作） |
| 9 | ページ | `Runtime.releaseObject {objectId:"4.3"}` | isolated | 成功（`{}`） |

main の `executionContextId` に送られた `functionDeclaration` は、この実行では順 1 の `() => { return document; }` の 1 件だけで、ページ定義のグローバル変数を参照しない（`document` は DOM の入口）。
順 6 の `queryOne` は isolated に留まり、main には送られない。ソース上のコメント（`//# sourceURL=pptr:evaluateHandle;…`）が末尾に付く。
`arguments` の 3 件は、いずれも同じ isolated world のハンドルまたは値だった（world をまたぐ `objectId` の受理可否は、この経路では確かめていない）。

## イベント順序の前提

- `newPage` は `Target.createTarget` の応答後に `waitForTarget` でターゲットの出現を待つ。Playwright が応答直後に同期参照する構造と異なり、
  応答とイベントの到着順への依存は緩いとソース上読める（B）。ただしイベントが一度も届かなければ待ち続け、`stages.mjs` の期限で `StageTimeout` になる
- 現実装は応答を先に送る規約（`protocol.rs` の `DispatchOutcome`。`CDP-5`）。Puppeteer では順序入れ替えが必須かは未確定
- 接続直後は `Target.setAutoAttach` に `waitForDebuggerOnStart:true` を渡す。attach 後に `Runtime.runIfWaitingForDebugger` を送る前提のため、
  サーバー側が「デバッガ待ち」状態を表現しない場合は、属性を偽らず実装方針を決める必要がある（未確定。
  `waitForDebuggerOnStart:true` の受理自体は TASK-43.3g〔#942〕で行い、新ターゲットの停止は #910 で実装する）
- `Page.getFrameTree` の応答処理前にイベントが届いた場合は Puppeteer 側でバッファされる（`frameTreeHandled`。B）

## Playwright との比較・追加実装コストの見込み

比較対象は `docs/design/playwright-compat.md` の不足箇所 #1〜#11。すべて B（実測なし）に基づく相対評価で、人日の見積りは出さない。

| 区分 | 項目 | 影響する実装箇所 |
| ---- | ---- | ---------------- |
| 両方が必要 | `Target.setAutoAttach`・`Target.createTarget`・`Target.attachedToTarget` 送出・子 session の `runIfWaitingForDebugger`・`Page.enable`・`Page.getFrameTree`・`Page.setLifecycleEventsEnabled`・`Runtime.enable`・`Network.enable`・`Log.enable`・`Page.createIsolatedWorld`（握りつぶし）・`Emulation.*` | `target.rs`（ターゲット・attach・上限）、イベント送出経路（`DispatchOutcome`）、page 状態モデル（frame tree・実行コンテキスト） |
| Puppeteer のみ | `Target.getBrowserContexts`（#1323 で実装済み）・`Target.setDiscoverTargets`（#1324 で実装済み。`Target.targetCreated`・`targetDestroyed` を送出。`targetInfoChanged` は未実装）・`Performance.enable`・`Audits.enable`（#1326 で実装済み。フラグ保持のみ。イベント・計測なし）・`Target` の `tab` 型階層・`Runtime.callFunctionOn` 等の評価系（`page.$`） | `target.rs`（discover とイベント）、`Runtime` ドメイン |
| Playwright のみ | `/json/version/` の末尾スラッシュ受理・`Browser.getVersion`・`Browser.setDownloadBehavior`・`Target.createBrowserContext`・`Target.getTargetInfo`・`Page.frameNavigated`（初期 `about:blank`）の送出 | `server.rs`・`protocol.rs`・browser context の追加 |

所見（B に基づく）:

- 両者が共通して必要とする部分（自動 attach・ターゲット作成・attach イベント・ページ初期化メソッド群・フレームツリー・実行コンテキスト）が不足の大半を占める。
  この共通部分は一度実装すれば両クライアントに効く
- 差分は、Puppeteer では browser context 作成を要求せず（既定 context のみなら `Target.getBrowserContexts` の応答で足りる）、
  HTTP 発見（`/json/version`）経路を使わない点で軽い。一方で `Target.setDiscoverTargets` 系のイベントと `Runtime` 評価系（`page.$`）が追加で必要になる
- したがって PoC-5 の「Puppeteer は CDP 依存が薄い」（C・推測）は、`newPage` までについては部分的に整合する（browser context・HTTP 発見が不要）が、
  `page.$` を含む基本操作まで見ると `Runtime` 依存が加わり、一概に薄いとは言えない。確度は B で、実測で再確認が必要
- どちらを主軸にするかの推奨・決定は本書では行わない

## 制約との交差

| 制約 | 本件との関係 |
| ---- | ------------ |
| SEC-2（偽装禁止） | 本調査の範囲では、Puppeteer が `Browser.getVersion` を `allowlist` 指定時にのみ読み Chrome 149 以上を要求する（B）。通常経路では版の偽装を要求しない。UA・版を装う値を返す案は採らない |
| CDP-6（一律成功の禁止） | #1〜#8 を空 success で通す案は到達性を上げるが未実装を実装済みに見せる。メソッド単位で個別にレビューする |
| SEC-4・CDP-1（Host / Origin 検証） | loopback 限定バインド・Host 検証・Origin 拒否を緩めない |
| 上限（`MAX_TARGETS`・`MAX_SESSIONS`） | `Target.createTarget`・auto-attach の対応でターゲット数・セッション数の上限を維持する |
| SSRF | 試験の `goto` は `about:blank` 固定のまま。`FetchOptions` の内部アドレス拒否を緩める案は採らない |

## 対応候補（列挙のみ。優先度・採否は未決定。ただし `Target.setAutoAttach` の `filter` の扱いは「方針」節で決定済み）

| 候補 | 影響範囲 |
| ---- | -------- |
| `Target.setDiscoverTargets`（#1324 で実装済み。`Target.getBrowserContexts` は #1323 で実装済み）・`Target.setAutoAttach`・`Target.createTarget` と `Target.targetCreated`／`attachedToTarget` の実装 | `target.rs`・イベント送出経路 |
| ページ session 初期化メソッド群（#6〜#8）の最小実装 | page 状態モデル。メソッドごとに「実装」か「明示エラー」かを選ぶ |
| `Page.lifecycleEvent` の送出 | `navigation.rs`・`page.rs` |
| `Runtime` の実行コンテキストと評価系の最小実装 | 新規モジュール（JS エンジン抽象との接続を含む。設計判断が必要） |

## 方針

Puppeteer が接続段階で送る `Target.setAutoAttach` の `filter` の扱いについて、オーナーが次のとおり判断した（Issue #1327・2026-10-30。対応ビヘイビア: `CDP-2`・`CDP-3`・`CDP-4`・`CDP-6`・`SEC-2`。対応タスク: TASK-45・TASK-46。マイルストーン: MS-4）。

- **論点**: Puppeteer は root セッションに `Target.setAutoAttach` を `filter: [{type: 'page', exclude: true}, {}]` 付きで送る（Puppeteer `TargetManager.ts:154`）。fandhe-browser は `filter` を `-32602` で拒否しており（`crates/fandhe-browser-cdp/src/playwright_compat.rs:141`、main 165eda0 時点）、Puppeteer は接続段階で止まる
- **採用**: **案 (a) tab 擬似ターゲットを持ち、Chrome の階層（tab → page）に合わせる**
- **不採用**:
  - 案 (b)「`filter` を受けるが page 除外は適用しない」: 除外すべき `page` まで attach してしまい、#908 の「黙って無視しない」方針に反する。また未実装の指定を成功扱いで受け流すフォールバックに当たる。これはセキュリティ規約が検出回避として作用しないか確認を求めるもので、`CDP-6`（未実装メソッドへの応答方針。検討中）の論点とも重なる
  - 案 (c)「非対応のまま」: Puppeteer で `CDP-4` を満たせない
- **着手順**: #1321 配下の Playwright 側の不足（例: DOMContentLoaded の lifecycleEvent〔#1322。`Page.navigate` が確定時に送出するようになった。`playwright-compat.md` の該当節〕）を先に直し、その後に案 (a) に着手する。Playwright で `CDP-4` の成功率を上げる方が効果が大きく、手間も小さいため
- **案 (a) の実装時の確認事項**: tab ターゲットを `Target.getTargets`・`Target.targetCreated` 等に出すことで、Playwright の接続・`newPage` が後退しないことを `harness/playwright-connect`・`harness/endpoint-swap` で確かめる
- **測定条件の前提**: Puppeteer は `defaultViewport: null` 以外では `Emulation.setDeviceMetricsOverride` で止まる。これは画面の識別面にあたり `SEC-2` で実装しないため、Puppeteer の測定は `defaultViewport: null` を前提とする
- **実装状況**: 実装済み（#1334）。tab 擬似ターゲット・root と tab のセッションの `Target.setAutoAttach` の `filter`・`Target.setDiscoverTargets` の `filter:[{}]` への tab 通知を実装した。#1334 の時点では、`puppeteer-core@25.12.0` を一時導入して 1 回だけ手動実測し、`connect` は ok、`newPage` は `Page.addScriptToEvaluateOnNewDocument` で失敗した（履歴。その後 #1340 ほかで解消）。`userGesture` は #1357 で受理済み。最新の実測は Issue #1358 の再実測（2026-10-31・`633e0100`。5 段階すべて ok。3 回実行）で、「到達段階のまとめ」を参照（Issue #1349 の実測では `selector` が `userGesture` で停止していた）。Playwright 側は Rust の結合テスト（`playwright_flow` 等）が後退していないことを確認した範囲で、実 Playwright での再測定は未実施。詳細は次節

## tab 擬似ターゲットと `filter`

実装: #1334（対応ビヘイビア: `CDP-2`・`CDP-3`・`CDP-4`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・`REPAIR-4`。TASK-45・TASK-46。MS-4）。

### モデル

- tab はページと 1:1 の**派生ターゲット**で、registry の `targets` には入れない。ID はページの連番に `1 << 62` を足した値
  （例: 先頭ページ `00000000000000000000000000000001` の tab は `00000000000000004000000000000001`）。ブラウザ ID（bit 63）とも衝突しない
- tab の `targetInfo`: `{targetId: <tab ID>, type: "tab", title/url: ページの実 URL, attached, canAccessOpener: false, browserContextId: ページの context}`。
  ページの実値だけで組み立て、Chrome を装う値は載せない（`SEC-2`）
- tab のセッションは `sessions` とは別表で、ページ系コマンド（`Page.*`・`DOM.*`・`Runtime.enable` 等）は未知のセッションとして
  `-32602` で拒否する（fail-closed）。tab のセッションで受けるのは `Target.setAutoAttach` と `Runtime.runIfWaitingForDebugger` だけ。
  どちらも**その tab のセッションを払い出した接続**だけが使える（他接続・接続なしは `-32602`）
- Puppeteer の利用では 1 ページが 2 セッション（tab とページ）を消費する。上限 `MAX_SESSIONS=256` は tab のセッションと共有する（`CDP-7`）

### 受理表（`filter`）

照合は固定形の完全一致で、入力の大きさで処理量は増えない。表にない形は黙って無視せず `-32602 unsupported params`（`CDP-6`・`REPAIR-3`）。
実装は `playwright_compat::parse_target_filter`（結果は enum。一般の first-match 評価へ広げられる形にしている。`REPAIR-4`）。

| 呼び出し | 省略 | `[{}]` | `[{"type":"page","exclude":true},{}]` |
| -------- | ---- | ------ | ------------------------------------- |
| root の `Target.setAutoAttach` | ページを root で attach（Playwright 経路。従来と同一） | 拒否（ページと tab の同時 attach は未実装） | tab を root で attach（Puppeteer 経路） |
| tab のセッションの `Target.setAutoAttach` | 子のページを attach | 子のページを attach | 拒否 |
| ページのセッションの `Target.setAutoAttach` | 状態の保持のみ（子 0 件） | 同左 | 拒否 |
| `Target.setDiscoverTargets` | ページのみ通知 | ページと tab を通知 | 拒否 |

上記以外（`[]`・`{}`・`null`・`[{},{}]`・`[{"type":"page"}]`・未知キー・他の型名など）はすべて拒否する。
root で自動 attach が有効な間に別種別（ページ ↔ tab）で指定し直した場合も `-32602`（先に `autoAttach:false`）。拒否は状態を変えない。

### Puppeteer のフロー（`newPage` まで）

1. `Target.setDiscoverTargets {discover:true, filter:[{}]}` → 応答の後に既存ターゲットの `Target.targetCreated`（tab → ページ）
2. root の `Target.setAutoAttach {waitForDebuggerOnStart:true, flatten:true, filter:[{type:page,exclude:true},{}]}` → 既存ページの tab へ attach し、root で `Target.attachedToTarget`（`type:"tab"`）
3. `Target.createTarget {url:"about:blank"}`: 応答の前に `targetCreated`（tab）→ `targetCreated`（ページ）→ `attachedToTarget`（tab・`waitingForDebugger:true`）。応答の `targetId` は**ページの ID**（Puppeteer の `waitForTarget` が照合する）
4. tab のセッションの `Target.setAutoAttach {waitForDebuggerOnStart:true, flatten:true, filter:[{}]}` → 子のページを attach し、`Target.attachedToTarget` を**エンベロープの `sessionId` = tab のセッション**で送る（Puppeteer の `Connection` が親セッションを決める）。tab が止めていたページの停止は子へ移り `waitingForDebugger:true`
5. tab のセッションの `Runtime.runIfWaitingForDebugger`（停止を子へ移した後は実際の no-op）。ページのセッションの `Target.setAutoAttach`（`[{}]`）と `Runtime.runIfWaitingForDebugger`（初期 `about:blank` を確定）

閉鎖・消失の通知順は、子のページの `detachedFromTarget`（エンベロープは tab のセッション）→ tab の `detachedFromTarget`（root）→ `targetDestroyed`（ページ）→ `targetDestroyed`（tab。通知済みのときだけ）。
切断・`Target.closeTarget`・context の破棄・`autoAttach:false` のすべてで、子のページと tab のセッションを解放する（枠の枯渇を防ぐ。`CDP-7`）。

### tab の露出範囲

- `Target.getTargetInfo {targetId: <tab ID>}` は実値を返す（読み取りのみ）。`Target.setDiscoverTargets` は `filter:[{}]` のときだけ tab の `targetCreated`・`targetDestroyed` を送る
- 未実装: `Target.getTargets`・`Target.attachToTarget`・`Target.closeTarget`（tab の ID は `-32602`）・`Target.targetInfoChanged`・browser 型の通知。`/json/list` はページのみのまま
- Chromium との差異（`REPAIR-3`）: Chromium の `[{}]` は browser 型も含むが本実装は含めない。root の `[{}]` は拒否する。既存ターゲットの `targetCreated` は応答の後
- 既存の `Target.setAutoAttach`（ページ経路）が他接続の context のページも attach する挙動は変えていない（tab 経路は既定 context と自接続所有 context のページに限る）

### 測定条件

`harness/puppeteer-connect/stages.mjs` は `puppeteer.connect` に `defaultViewport: null` を渡す（既定だと `newPage` が `Emulation.setDeviceMetricsOverride` を送るが、
画面の識別面を変える実装はしない。`SEC-2`）。実機の測定は `puppeteer-core` の一時導入（`mktemp -d`・`--no-save --ignore-scripts`・版は `scripts/versions.sh`）による手動で、
リポジトリには `package.json`・lockfile・`node_modules` を残さない。

## main world 評価と段階 2（#1350）の判断材料

実測（Issue #1349）の結果を、段階 2（ページランタイムの保持。owner:human の #1350 で採否を判断）への材料として整理する。判断そのものは本書では行わない。
設計は [main-world-evaluation.md](./main-world-evaluation.md)（段階 1 は確定 DOM の再パースで評価するため、ページ JS のヒープは無い。§3）。

- **(a) 実測で main に送られた関数（A）**: `() => { return document; }` の 1 件のみで、ページのグローバルを参照しない。確定 DOM の再パースで足りた（段階 1 で `page.$("body")` の前半が成功）。再実測（2026-10-31・#1358）で `selector` の後半の `queryOne` の `callFunctionOn` は isolated world に送られ、main には送られなかった（A）。main への送信は `DOM.resolveNode`（ハンドルの移送）だけだった
- **(b) 試験基盤の `page.$` 以外の関数（B）**: `harness/endpoint-swap/adapters.mjs` の `PAGE_FN_*`（`textContent` の取得、`name` 属性の数え上げ）は DOM の読み取りで、読解上はページの JS ヒープ（グローバル変数・クロージャ）を要しない。未実測
- **(c) ページのグローバルを要しうる API（B）**: 利用者が `page.evaluate` 等で `window.<ページ定義の変数>` を参照する場合は、段階 1 では `exceptionDetails`（`ReferenceError` 等）になる。偽の値は返さない（`SEC-2`・`CDP-6`）。確定 HTML に残る DOM 状態の参照は動く
- **(d) 限界（A）**: 実測（2026-10-30・2026-10-31）は `about:blank` のみで、ページスクリプトが無い。段階 1 のヒープ欠落（JS が作る変数・イベントリスナーの状態）は、この実測では表に出ない。実サイトでの影響は `harness/endpoint-swap` の実サイト計測（`tasks.json` の対象）でも未確認
- 実測の範囲では「main world でページのグローバルを必要とする操作」は見つかっていない。`page.$("body")` は全段階（後半と `disconnect` を含む）を通ったが、`about:blank` の 1 経路だけで、ページスクリプトのある実ページや `page.$$`・`page.evaluate` は未実測。段階 2 の採否はこの事実を材料に #1350 で判断する（本書では判断しない）

## 未確定事項・再現手順

- 実測は 2026-10-30（Issue #1349。1 回）と 2026-10-31（Issue #1358。3 回で同じ結果）で、`about:blank`・debug ビルド・Linux のみ。他 OS は未確認。残る未確定事項:
  - `about:blank` 以外のページ（ページスクリプトを持つ実ページ）での `page.$`、`page.$$`・`page.evaluate`・`waitForSelector` 等の別経路。切断後のサーバー側の資源解放（接続数・ターゲット）
  - #8（`defaultViewport` 既定時の `Emulation.*`。`SEC-2` で実装しない）
  - `CdpState::received_methods()` の値は取得していない（トレースはクライアント側の `NODE_DEBUG` の送受信列）
  - Chromium 実機との照合（`objectId` と `executionContextId` の排他エラー等。`main-world-evaluation.md` §8）。実機が手元に無く未確認
- `Target.setAutoAttach` の `filter` 引数は `-32602`（`unsupported params`）で拒否すると確定（TASK-43.3c・#908。黙って無視すると除外すべき `page` まで attach するため）。`autoAttach:true` と `waitForDebuggerOnStart:true` の組み合わせは、当初拒否していたが TASK-43.3g（#942）で受理に変わった（保持のみ。新しいターゲットの停止は #910）。Puppeteer の `TargetManager.initialize` の `filter` 付き呼び出しは、#1334 で案 (a)（tab 擬似ターゲット）により受理に変わった（受理表は「tab 擬似ターゲットと `filter`」節。表にない形の拒否は残る）
- spec（`docs/spec`）は本調査環境で再照合しておらず、C の記述は spec 本文と未照合
- 再現（ソース確認）: `npm pack puppeteer-core@25.12.0` で得た tarball を一時ディレクトリへ展開し、`src/cdp/` を閲覧のみする（展開物内でスクリプトは実行しない）
- 再現（実測。2026-10-30・2026-10-31 に実施した手順）: すべて隔離ディレクトリ（`mktemp -d`）で行い、リポジトリには何も残さない。
  1. `cargo build -p fandhe-browser-cli`（debug）
  2. npm 導入用ディレクトリで `PUPPETEER_SKIP_DOWNLOAD=1 npm install --no-save --no-audit --no-fund --ignore-scripts puppeteer-core@25.12.0`（版は `scripts/versions.sh`）。
     `node harness/endpoint-swap/verify_install.mjs <導入先> puppeteer-core 25.12.0 <PUPPETEER_INTEGRITY> --allow-transitive` で版と integrity を検証（推移的依存 25 件。`ws@8.22.0`・`chromium-bidi@17.0.2`・`zod@3.25.76` 等）
  3. プロファイルを `mktemp -d` に向け（`XDG_DATA_HOME`・`HOME`。`--profile-dir` は存在しない子ディレクトリを指す）、`FANDHE_BROWSER_CONFIG` を外して `target/debug/fandhe-browser launch --port 0` を起動する（固定ポートは並列作業と衝突しうるため使わない）。stderr の `DevTools listening on ws://...` の URL を `FANDHE_CDP_WS_ENDPOINT` に使う（`/json/version` の `webSocketDebuggerUrl` でも可）
  4. 3 回繰り返す。`NODE_PATH=<導入先>/node_modules FANDHE_CDP_WS_ENDPOINT=<ws URL> NODE_DEBUG='puppeteer:*' node harness/puppeteer-connect/connect.mjs`（stdout の `FANDHE_SCRIPT_RESULT` 行が段階別 status、stderr がトレース。`puppeteer-core` 25.12.0 のトレースは `DEBUG` ではなく `NODE_DEBUG` で有効になる）
  5. サーバーを停止し、作った一時ディレクトリをすべて削除する。生のトレースはコミットしない
  測定条件は `defaultViewport: null` のみで、UA・ヘッダ・`Emulation.*` は変えない（`SEC-2`）。サーバー側の stderr には `WebMCP.enable` と `Runtime.addBinding` の「unimplemented method」だけが出た
