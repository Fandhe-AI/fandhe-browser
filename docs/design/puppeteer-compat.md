# Puppeteer 互換: 接続試験の結果レポート

- **対応ビヘイビア**: CDP-3（関連: CDP-2・CDP-6・SEC-2・REPAIR-3。`docs/spec/04-behavior/` が SSOT）
- **対応タスク**: TASK-45（サブ 45.3・Issue #482）。入力は TASK-45.1（Issue #480）・TASK-45.2（Issue #481）の試験基盤
- **マイルストーン**: MS-4
- **基準コミット**: `42ce7c0`（`origin/main`）
- **調査日**: 2026-10-06
- **調査対象の版**: puppeteer-core 25.12.0（npm の最新版。integrity `sha512-z6LQUt5SH7jwGtGA+nMtJuEjZIsqPiwQ+EFHCYuVDQrJpvifNmmGuyDvTzAKPyFzihxgHc5f3aMzu2kn8Ps37A==`）・node v24.21.0。
  これは読解した公開物の記録であり、依存としての採用・固定ではない（導入はユーザー承認待ち）

## 目的・前提

`puppeteer.connect({browserWSEndpoint})` → `browser.newPage()` → `page.goto("about:blank")` → `page.$("body")` →
`browser.disconnect()` の各段階が fandhe-browser の CDP サーバーでどこまで到達するかを記録する。
本書は記録までで、方針の決定（Playwright と Puppeteer のどちらを主軸にするか）と追加実装は含めない。

**重要: 実 Puppeteer での実測は 1 件も存在しない。** `puppeteer-core` の導入・バージョン固定は承認待ちで、
`package.json`・実試験ターゲット・CI ジョブは未追加（`harness/puppeteer-connect/README.md`「導入状況（承認待ち）」）。
段階別 status は「未実測」であり、「到達 0 段階」という実測結果ではない。以下の停止点はすべてソース読解に基づく予測である（`REPAIR-3`）。

| 種別 | 内容 | 確度 |
| ---- | ---- | ---- |
| A（実測） | 実 Puppeteer による段階別 status・CDP トレース | **該当なし（未実行）** |
| B（ソース読解） | npm 公開物 puppeteer-core 25.12.0 の `src/cdp/` 配下（`BrowserConnector.ts`・`Browser.ts`・`TargetManager.ts`・`FrameManager.ts`・`Page.ts`・`Frame.ts` 等）の該当箇所を読んだ結果 | 実測していない。ソース上そう読める、という記録 |
| C（二次情報） | spec の PoC-5（`docs/spec/03-poc/cdp-compat/`）の「Puppeteer は Playwright より CDP 依存が薄いとされる」（推測・未検証） | 本調査環境では `docs/spec` を再照合していない |

Puppeteer のソースは長く引用せず、クラス・関数名と要約に留める。

## 到達段階のまとめ

試験基盤の段階は `harness/puppeteer-connect/stages.mjs` の `connect` → `newPage` → `goto` → `selector` → `disconnect`。

| 段階 | 実測 | 予測（B） | 予測の根拠 |
| ---- | ---- | --------- | ---------- |
| `connect` | 未実測 | 失敗する見込み | 接続直後の最初の送信が `Target.getBrowserContexts`（root session）で、組込みハンドラが無く `-32601` になる（#1） |
| `newPage` | 未実測 | 未到達の見込み | `connect` が先に失敗するため |
| `goto` | 未実測 | 未到達の見込み | 同上 |
| `selector` | 未実測 | 未到達の見込み | 同上 |
| `disconnect` | 未実測 | 先行段階の失敗後は後始末のみ | `stages.mjs` の仕様（先行失敗時は結果を上書きしない） |

`-32601` は未実装メソッドへ成功を捏造しない `CDP-6`・`SEC-2` の方針どおりの挙動で、バグではない。
Playwright と異なり、`browserWSEndpoint` 指定の接続では HTTP の `/json/version` を使わない
（読むのは `browserURL` 指定時のみ。`common/BrowserConnector.ts` の `getWSEndpoint`）。
本試験基盤は `browserWSEndpoint` を使うため、Playwright 側の停止点 #1（`/json/version/` の 404）は Puppeteer の経路には現れない（B）。

## 不足箇所一覧

「現状の応答」は `crates/fandhe-browser-cdp/src/` の現実装（組込みハンドラは `Page.navigate`・`DOM.getDocument`・
`DOM.querySelector`・`DOM.requestChildNodes` のみ。`protocol.rs` の `builtin_handlers`）に基づく。

| # | 段階 | Puppeteer の箇所 | 送る CDP メソッド | 読む応答・待つイベント | 現状の応答 | 不足 | 種別 |
| - | ---- | ---------------- | ----------------- | ---------------------- | ---------- | ---- | ---- |
| 1 | connect | `_connectToCdpBrowser` | `Target.getBrowserContexts`（root。接続後の最初の送信） | `browserContextIds`（配列。既存 context の把握） | `-32601` | ハンドラと browser context の概念（`TargetInfo` は `browserContextId` を持たない） | B |
| 2 | connect | `TargetManager.initialize` | `Target.setDiscoverTargets` `{discover:true, filter}` | 応答は読み捨て。以後 `Target.targetCreated`・`targetInfoChanged`・`targetDestroyed` を購読 | `-32601` | ハンドラと `Target.targetCreated` 等の送出 | B |
| 3 | connect | `TargetManager.initialize` | `Target.setAutoAttach` `{autoAttach:true, waitForDebuggerOnStart:true, flatten:true, filter}`（root。`page` を除外するフィルタ付き） | 応答は読み捨て。以後 `Target.attachedToTarget` を購読し、既存 `tab` ターゲットの初期化完了まで `connect` が待つ | `-32601` | ハンドラ、自動 attach の実体（`TargetRegistry::attach` は CDP から未接続）、filter の解釈 | B |
| 4 | newPage | `CdpBrowser._createPageInContext` | `Target.createTarget` `{url:"about:blank", browserContextId?}` | 応答の `targetId`。その後 `waitForTarget` で、同 `targetId` のターゲットが `targetCreated`／attach 経由で現れるのを待つ | `-32601` | ハンドラ（`TargetRegistry::create_target` は存在するが未接続。上限 `MAX_TARGETS`=64） | B |
| 5 | newPage | `TargetManager.#onAttachedToTarget` | イベント `Target.attachedToTarget`（`sessionId`・`targetInfo`）。受信後に `Target.setAutoAttach`（子 session）・`Runtime.runIfWaitingForDebugger` を送る | `targetInfo.targetId`・`type`・`url`・`browserContextId`。`waitForDebugger` 中のターゲットは `runIfWaitingForDebugger` で解放される | 送出経路なし。現実装のイベントはコマンド応答に同伴する形のみ（`HandlerOutput.events`） | `Target.attachedToTarget` の生成、子 session 向けの 2 メソッド | B |
| 6 | newPage | `CdpPage.#initialize`・`FrameManager.initialize` | page session で `Page.enable`・`Page.getFrameTree`・`Page.setLifecycleEventsEnabled`・`Runtime.enable`・`Network.enable`（`NetworkManager.addClient`）・`Performance.enable`・`Log.enable`・`Audits.enable`（既定で有効）を並行送信 | `Page.getFrameTree` の `frameTree.frame` で frame を構築。いずれかが失敗するとページ初期化が失敗する（ターゲットが閉じた場合のエラーのみ握りつぶし） | 全て `-32601` | 上記メソッド群のハンドラとフレームツリーのモデル | B |
| 7 | newPage | `FrameManager.#createIsolatedWorld` | `Page.createIsolatedWorld` `{frameId, worldName, grantUniveralAccess:true}`（`Runtime.enable` の後。失敗はログのみで握りつぶし） | `Runtime.executionContextCreated` を待って実行コンテキストを構築 | `-32601`（握りつぶされる） | 未実装でも初期化は進むとソース上読めるが、実行コンテキストが作られず #11 に影響 | B |
| 8 | newPage | `EmulationManager`（`defaultViewport` 既定 800x600） | `Emulation.setDeviceMetricsOverride`・`Emulation.setTouchEmulationEnabled` 等 | 結果は使わない（失敗でページ作成が失敗する経路。握りつぶしの有無は個別には未確認） | `-32601` | ハンドラ。`defaultViewport: null` で回避できる可能性があるが、試験基盤は既定値を使用 | B |
| 9 | goto | `Frame.goto`（`about:blank`） | `Page.navigate` `{url, referrer, frameId, referrerPolicy}` | 応答の `loaderId`・`errorText`。完了は `Page.lifecycleEvent`（`load` 等）・`Page.frameNavigated` を `LifecycleWatcher` が待つ | `Page.navigate` は実装済み（`Page.frameNavigated`・`Page.loadEventFired` を同伴）。ただし `Page.lifecycleEvent` は出さない | `Page.lifecycleEvent` の送出（`about:blank` の完了判定に必要とソース上読める） | B |
| 10 | selector | `Page.$("body")` | `Runtime.callFunctionOn` 等で isolated world の実行コンテキスト越しに評価（`ExecutionContext` の送信は確認。`page.$` の呼び出し連鎖は完全には追跡していない） | `Runtime.executionContextCreated`・`RemoteObject` | `-32601`（`Runtime.*` なし）。`DOM.querySelector` だけでは足りない見込み | `Runtime` ドメインの実行コンテキストとリモートオブジェクトのモデル | B |
| 11 | disconnect | `CdpBrowser.disconnect` | CDP メッセージ送信なしで transport を閉じると読める（未精査） | — | — | なし（見込み） | B |

### 実測で確認できた停止点と予測の境界

実測で確認できた停止点は 0 件。#1〜#11 はすべて B の予測で、`puppeteer-core` 導入承認後に実行して確認する。

## イベント順序の前提

- `newPage` は `Target.createTarget` の応答後に `waitForTarget` でターゲットの出現を待つ。Playwright が応答直後に同期参照する構造と異なり、
  応答とイベントの到着順への依存は緩いとソース上読める（B）。ただしイベントが一度も届かなければ待ち続け、`stages.mjs` の期限で `StageTimeout` になる
- 現実装は応答を先に送る規約（`protocol.rs` の `DispatchOutcome`。`CDP-5`）。Puppeteer では順序入れ替えが必須かは未確定
- 接続直後は `Target.setAutoAttach` に `waitForDebuggerOnStart:true` を渡す。attach 後に `Runtime.runIfWaitingForDebugger` を送る前提のため、
  サーバー側が「デバッガ待ち」状態を表現しない場合は、属性を偽らず実装方針を決める必要がある（未確定）
- `Page.getFrameTree` の応答処理前にイベントが届いた場合は Puppeteer 側でバッファされる（`frameTreeHandled`。B）

## Playwright との比較・追加実装コストの見込み

比較対象は `docs/design/playwright-compat.md` の不足箇所 #1〜#11。すべて B（実測なし）に基づく相対評価で、人日の見積りは出さない。

| 区分 | 項目 | 影響する実装箇所 |
| ---- | ---- | ---------------- |
| 両方が必要 | `Target.setAutoAttach`・`Target.createTarget`・`Target.attachedToTarget` 送出・子 session の `runIfWaitingForDebugger`・`Page.enable`・`Page.getFrameTree`・`Page.setLifecycleEventsEnabled`・`Runtime.enable`・`Network.enable`・`Log.enable`・`Page.createIsolatedWorld`（握りつぶし）・`Emulation.*` | `target.rs`（ターゲット・attach・上限）、イベント送出経路（`DispatchOutcome`）、page 状態モデル（frame tree・実行コンテキスト） |
| Puppeteer のみ | `Target.getBrowserContexts`・`Target.setDiscoverTargets`（＋ `Target.targetCreated` 系イベント）・`Performance.enable`・`Audits.enable`・`Target` の `tab` 型階層・`Runtime.callFunctionOn` 等の評価系（`page.$`） | `target.rs`（discover とイベント）、`Runtime` ドメイン |
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

## 対応候補（列挙のみ。優先度・採否は未決定）

| 候補 | 影響範囲 |
| ---- | -------- |
| `Target.getBrowserContexts`・`Target.setDiscoverTargets`・`Target.setAutoAttach`・`Target.createTarget` と `Target.targetCreated`／`attachedToTarget` の実装 | `target.rs`・イベント送出経路 |
| ページ session 初期化メソッド群（#6〜#8）の最小実装 | page 状態モデル。メソッドごとに「実装」か「明示エラー」かを選ぶ |
| `Page.lifecycleEvent` の送出 | `navigation.rs`・`page.rs` |
| `Runtime` の実行コンテキストと評価系の最小実装 | 新規モジュール（JS エンジン抽象との接続を含む。設計判断が必要） |

## 方針

（未記入。人間が決定し、ここへ記録する。本書では方針を決めていない。）

## 未確定事項・再現手順

- 実測が未実施。`puppeteer-core` の導入承認後に次を実測値で更新する: 段階別 status・失敗段階のエラー名とメッセージ・
  `CdpState::received_methods()`・本書の B の予測との差分
- #8 のうち握りつぶしの有無、#10 の `page.$` の呼び出し連鎖、#11 の切断時の送信有無は精査が不完全
- `Target.setAutoAttach` の `filter` 引数と `waitForDebuggerOnStart` の扱いの実装方針は未確定
- spec（`docs/spec`）は本調査環境で再照合しておらず、C の記述は spec 本文と未照合
- 再現（ソース確認）: `npm pack puppeteer-core@25.12.0` で得た tarball を一時ディレクトリへ展開し、`src/cdp/` を閲覧のみする（展開物内でスクリプトは実行しない）
- 再現（実測。承認後）: `harness/puppeteer-connect/README.md` の導入手順に従う
