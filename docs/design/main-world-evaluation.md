# main world への評価と `Runtime.callFunctionOn` の `executionContextId` 対応の設計

対応 `CDP-2`・`CDP-3`・`CDP-6`・`SEC-2`・`JS-4`（関連 `CDP-7`・`JS-6`・`PERF-7`・`REPAIR-3`）/ TASK-45・TASK-46 / `MS-4` / Issue #1328・#1327・#1334（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 位置づけ

- 本書は設計案である。実装は §9 の分割案で進める。分割案 1（`executionContextId` 指定の `Runtime.callFunctionOn`）は #1345、分割案 2（段階 1: main world を確定 DOM の再パースで評価する）は #1347、分割案 3（core の初期の文書順番号）は #1346、分割案 4（`DOM.describeNode`・`DOM.resolveNode`）は #1348、分割案 5（Puppeteer `page.$` の実測と `puppeteer-compat.md` の更新）は #1349 で実施済み
- 段階 2（ページランタイムの保持）は**未実装**で、保留している（`REPAIR-3`）。これについて以下に書く挙動は設計案である
- 「オーナー判断待ち」と書いた箇所は、推奨案を置いたうえで決定を保留している。段階 1 の判断は §3 に記録した
- 本書の §1 と §2 は #1347 の時点で一部更新したが、基準コミットの記述（`MAIN_WORLD_EVALUATION_UNSUPPORTED` など）は設計時点の履歴として残している。定数は #1347 で削除した
- 確度の区分は [puppeteer-compat.md](./puppeteer-compat.md) と同じ。A は実測、B はソースの読解（未実測）。§2 の連鎖は #1349 で実測（A）と照合した（2026-10-30・`683ca65a`）。実測結果は [puppeteer-compat.md](./puppeteer-compat.md)「`page.$` の呼び出し連鎖」にあり、**停止点は `Runtime.evaluate` の `userGesture`**（§2 の段 3）
- 基準コミットは origin/main `04a9d4e2`。シンボル名は基準コミットのもので、実装時に再確認する

## 1. 現状と停止点

### 1.1 cdp crate

- `runtime.rs` の `ExecutionContextTable` は、main world にも contextId を払い出して告知している（`World::Main`）。`resolve` はセッションの所有と世代を照合する（他セッションの contextId は `INVALID_PARAMS`、世代違いは `EXECUTION_CONTEXT_STALE`）
- （設計時点の記述。#1347 で変更済み）`runtime/evaluate.rs` の `RuntimeEvaluate::handle` は、main world への評価に固定エラー `MAIN_WORLD_EVALUATION_UNSUPPORTED` を返していた。理由は、main world のページ JS 状態が別の実行系にあること（`runtime.rs` の「差異・スタブ」）
- ハンドル表は JS 側のヘルパー（`helper.js` の `__fandheCdpHelper`）が持つ。`objectId` は `<contextId>.<n>` の書式。ヘルパーは注入時点の組込み（`JSON.parse`・`Reflect.apply` 等）を退避する。ヘルパー未注入の `objectId` は `REMOTE_OBJECT_NOT_FOUND` になる（設計時点では main world の `objectId` も同じだった。#1347 で main world もヘルパーを注入する）
- `runtime/evaluate/call_function_on.rs`
  - `objectId` か `executionContextId` のどちらか一方を必須とする（排他。#1345）。`executionContextId` は #1345 の時点では isolated world だけ受理し（main world は `MAIN_WORLD_EVALUATION_UNSUPPORTED`。#1347 で main も受理）、`this` は undefined
  - 引数の `objectId` の contextId が `this` と違えば、Rust 側で `ARGUMENT_CONTEXT_MISMATCH`
  - `uniqueContextId`・`serializationOptions` 等も `UNSUPPORTED_PARAMS`
- `dom.rs` の `nodeId` は、払い出した文書ごとの `base` に文書順の番号を足して振る（`IssuedDocuments`）。`backendNodeId` は `nodeId` と同じ値。`DOM.describeNode`・`DOM.resolveNode` は設計時点では未実装（#1348 で実装。§5）

### 1.2 core crate

- `core::script_context`（`ScriptContextHandle`。#1216）は、キー（`scope`・`generation`・`world` = contextId）ごとに `JsRuntime` と `DomBridge` を 1 つずつ持つ
  - 確定済みの `NavigationResult` の HTML を**再パース**するだけで、`<script>` は再実行しない
  - world ごとに別の `Document` を持ち、互いの変更は見えない
  - DOM の変更は `NavigationResult` に書き戻さない。イベントループも回さない
  - 上限は scope あたり 4 コンテキスト・全体 16
- `core::page_actor` / `page_runner`（`JS-4`）は、遷移中に `<script>` を 1 回実行し、ランタイムを破棄する。確定する HTML はスクリプト実行**後**の DOM をシリアライズしたもの（`execute_page_scripts = true` のとき）
- 帰結: 確定 HTML から作るコンテキストは、スクリプト実行後の **DOM** を見られる。ページ JS の**ヒープ**（グローバル変数・クロージャ・リスナー）は失われている

## 2. Puppeteer `page.$` の CDP 連鎖（読解 B を実測 A と照合）

puppeteer-core 25.12.0 の `src/` を一時ディレクトリに展開して読んだ（閲覧のみ。スクリプトは実行していない。展開物は削除済み）。#1349 で実機（puppeteer-core 25.12.0・debug ビルド・`about:blank`）を実測して照合した。
下表の「実測の順」は実際の送信順で、読解の段の番号とは一致しない。

| 段 | 送るメソッド | 必須の引数 | 実測（A）と fandhe の現状 |
| -- | ------------ | ---------- | ------------------------- |
| 1 | `Runtime.callFunctionOn`（`Frame.#document()` の `evaluateHandle(() => document)`） | main の `executionContextId`、`functionDeclaration`、`returnByValue: false`、`awaitPromise`、`userGesture`。`objectId` は使わない | 実測の順 1。読解どおりの引数で、成功（`objectId:"3.1"`・`Document`）。#1345・#1347 |
| 2 | `Runtime.addBinding`（`executionContextName` 指定） | 失敗はログだけで握りつぶされる | 実測の順 4。`-32601` が握りつぶされて進行を止めなかった。成功を装わない（§5） |
| 3 | `Runtime.evaluate`（`puppeteerUtil` の注入） | isolated の `contextId` | 実測の順 5。**`userGesture:true` も送られ、`-32602 unsupported params` で停止**（読解の食い違い。§2 の要点参照）。他の引数は `expression`（約 10.5KB）・`contextId`・`returnByValue:false`・`awaitPromise:true` |
| 4 | `DOM.describeNode {objectId}` → `DOM.resolveNode {backendNodeId, executionContextId: isolated}`（`@bindIsolatedHandle` の `adoptHandle`） | | 実測の順 2・3。成功（#1348）。**段 3 より前、段 2 より前に送られる**（読解の順と異なる） |
| 5 | `Runtime.callFunctionOn`（`QueryHandler.queryOne`） | isolated の `executionContextId`。引数は `[{objectId: 要素}, {value: selector}, {objectId: puppeteerUtil}]` | 未到達（段 3 で停止）。#1345 後の現状は B のまま。旧記述の「`UNSUPPORTED_PARAMS` で拒否」は #1345 で変わっている可能性が高いが未実測 |
| 6 | `DOM.describeNode` → `DOM.resolveNode {backendNodeId, executionContextId: main}` → 元のハンドルを `Runtime.releaseObject`（`transferHandle`） | | 未到達。#1348 でメソッドは実装済み（B） |

読解と実測の食い違い:

- 段 3 の `Runtime.evaluate` は `userGesture` も送る（読解では `contextId` 中心と記録し、`userGesture` を見落としていた）。fandhe の `Runtime.evaluate` は `userGesture` を未知キーとして拒否する（`Runtime.callFunctionOn` は受理して効果なし）
- 送信順は 段 1 → 段 4 → 段 2（`addBinding`）→ 段 3 → 段 5 → 段 6。段 4 は `puppeteerUtil` を注入する前の isolated ハンドルの用意として先に行われる

要点は次のとおり。

- 文字列の評価（`Runtime.evaluate`）は `contextId` を常に明示して送る。`uniqueContextId` と `serializationOptions` は送らない
- 「ハンドル表の統合」の本題は、**world をまたぐノードの受け渡し**（`backendNodeId` 経由）にある。fandhe-browser は world ごとに `Document` が別でノードの同一性が無いため、world 共通のノード ID 空間（または対応表）が要る
- `executionContextId` の受理は main だけでなく isolated world でも要る
- Playwright の main world の評価（`page.evaluate`）も同じ制限に当たると読めるが、**要確認**（#1349 では未確認。§8）

## 3. 論点 1: main world のページ JS 状態

問題を 2 つに分ける。

- **DOM 状態**: 確定 HTML に残っている。isolated world と同じ作り方のコンテキスト（キーの `world` = main の contextId）で、スクリプト実行後の DOM を評価できる。`page.$` や `document.querySelector` 系にはこれで足りる
- **JS ヒープ状態**: 確定の時点で失われている

| 案 | 内容 | 評価 |
| -- | ---- | ---- |
| 段階 1（#1347 で実装） | main world も確定 DOM の再パースで評価する。ページ JS のヒープは無い | `REPAIR-3` の差異として明記し、「main world と同等」とは表現しない。ページのグローバルを参照すると `ReferenceError` 等が `exceptionDetails` に正直に出る。偽の値は返さない |
| 段階 2（保留。Puppeteer の実測後に別 issue〔owner:human〕で判断） | `page_actor` のランタイムを確定後も保持し、main world に昇格させる | 下の注意点を参照 |
| 却下 | スクリプトの再実行 | 冪等でない。再取得による SSRF 面と負荷が増える。確定 DOM とずれる |

段階 2 の注意点:

- `JsRuntime` は `!Send` で、別スレッドの 2 つのアクターをまたいで渡せない。アクターの統合が要る
- ターゲットごとに V8 子プロセスを常駐させることになる（`JS-6`・`PERF-7`）
- 確定後のイベントループの扱いを決める必要がある
- ヘルパーの組込み退避は、ページ JS より前に注入しないと改竄済みの組込みを掴む（§7）

オーナー判断（2026-10-30・#1347 に記録）:

- 段階 1 は既定で有効にし、設定キーは足さない
- 差異は `executionContextCreated` の `auxData` などの非標準フィールドで示さず、本書と doc コメントで示す
- 段階 2（ページランタイムの保持）は保留とする。Puppeteer の実測後に、別 issue（owner:human）で採否を判断する

`JS-4` との関係:

- `execute_page_scripts = false` のときは、確定 HTML が元の HTML のままになり、main world もそれを見る。JS 無効時の意味と一致する
- core の scope あたり 4 コンテキストの上限は、1 セッションで main と utility の 2 つを使う。2 セッションで上限に達する。既知の制限として扱う

## 4. 論点 2: `executionContextId` の受理（`Runtime.callFunctionOn`）

- `objectId` と `executionContextId` は排他にする。両方ある、またはどちらも無い場合は `INVALID_PARAMS`。Chromium の挙動との照合は未了（§8）
- `executionContextId` は既存の `ExecutionContextTable::resolve` で解決する
  - 他セッションの contextId は `INVALID_PARAMS` で、存在を区別しない
  - 世代違いは `EXECUTION_CONTEXT_STALE`
  - main world も受理する（#1347 で実装。`MAIN_WORLD_EVALUATION_UNSUPPORTED` は削除した）
- この経路では `this` を `undefined` にする（ヘルパーのペイロードの `thisHandle` を `null` にして表す。キーを省くとプロトタイプ汚染を拾い得るため、own property の null にする）
- 引数の `objectId` の contextId は、解決した contextId と一致すること（既存の `ARGUMENT_CONTEXT_MISMATCH` の経路を流用する）
- 検証順は #1345 で確定した。排他の検査は `functionDeclaration` の型の後、各キーの型の前に置く。`executionContextId` の解決は `objectId` 経路と同じ位置で行い、エラーの写像だけを経路で分ける。順序の全文は `runtime/evaluate.rs` のモジュール doc「検証順」を参照
- `uniqueContextId`・`serializationOptions` は Puppeteer 25.12.0 が送らないため、`UNSUPPORTED_PARAMS` のまま残す
- `Runtime.evaluate` の `contextId` 省略は、Chromium の意味に合わせ「セッションの現在の文書の main world」にする（#1347 で実装。Puppeteer と Playwright はどちらも明示するため互換性への影響は小さい）

## 5. 論点 3: ハンドル表と world をまたぐノード同一性

- `objectId` の書式 `<contextId>.<n>` とハンドル表は JS 側のままにする（main world にも同じヘルパーを注入する。#1347 で実装）
- `releaseObject`・`callFunctionOn` の解決で `World::Main` を許可する（#1347 で `REMOTE_OBJECT_NOT_FOUND` から変えた）

ノード同一性の案:

| 案 | 内容 | 評価 |
| -- | ---- | ---- |
| (i)（推奨） | core の `DomBridge` が、コンテキスト作成時（再パース直後）に「初期の文書順番号 → ノード」の表を Rust 側で持つ。`backendNodeId` を `dom.rs` の採番（`IssuedDocuments` の `base` + 文書順番号）と一致させる | DOM ドメインと Runtime で ID 空間が 1 つになる。JS 側の走査は不要で DOM 操作回数の上限を消費しない。初期ノードは移動しても同一性が保たれる。確定後に作られたノードには `backendNodeId` が無く、固定文言のエラーで返す。別のノードへは解決しない |
| (ii) | ヘルパーが JS 側で文書を走査し、WeakMap で番号を振る | `max_ops_per_page` を消費する。注入前に変更されていれば番号がずれる |

- (i) は core の公開 API（`ScriptContextHandle` / `DomBridge`）の変更を伴う。crate 境界の設計にあたるため、main（opus / fable）が設計してから builder に委譲する
- 新設するメソッドの最小範囲
  - `DOM.describeNode`: `objectId` から `backendNodeId`・`nodeId`・`nodeName` 等の最小の `Node`
  - `DOM.resolveNode`: `backendNodeId` と `executionContextId` から `RemoteObject`
- その他のキー（`depth`・`pierce` 等）は既存の DOM ドメインの方針に合わせる（`UNSUPPORTED_PARAMS` または明示エラー）
- world 間で DOM の変更は共有されない（既存の差異）。受け渡したノードは移動先 world の `Document` 上の同じ初期ノードであり、移動元での変更は反映されない
- 実装（#1348。`runtime/evaluate/dom_node.rs`）: `describeNode` の `Node` は確定 HTML の初期状態から組み（world 内の変更は反映しない）、評価文書に ID 範囲が未払い出しなら払い出す（Puppeteer は `DOM.getDocument` を呼ばない）。`resolveNode` は払い出さず、要求元セッションの現在の世代の文書に払い出した範囲だけを受理し、他セッション・旧世代・未払い出し・範囲外は `node not found`。確定後に作られたノードは `node has no backend node id`、ノードでない値は `object is not a node`。ID は JS の安全整数（2^53−1）以下に収め、数値のまま検査する
- `Runtime.addBinding` は Puppeteer が失敗を握りつぶす。`-32601` のままでよいと読めるため、`CDP-6` の観点で**実装しない**（成功を装わない）

## 6. 上限（`CDP-7`・`JS-6`）

既存の上限をそのまま使い、新しい上限値は足さない。

- ハンドル 4096 / コンテキスト
- 引数 64 件
- isolated world 8 個
- core のコンテキスト数 4 / scope
- `resolveNode` で 1 回に作るハンドルは 1 つ

## 7. セキュリティ（OWASP・`SEC-2`）

- **インジェクション**: `executionContextId` 経路でも、`functionDeclaration`・引数を JS ソースへ連結しない。JSON 文字列リテラル 1 個に入れてヘルパーの `JSON.parse` 経由で渡す既存方式を維持する。`backendNodeId` は数値として上限を検査し（安全整数・checked 演算）、文字列化してソースへ埋め込まない
- **アクセス制御**: contextId の解決は常にセッションの所有を照合し、他セッションの contextId・objectId の存在を区別せずに拒否する。`resolveNode` は、要求元セッションの現在の世代の文書に限る
- **不安全な設計（DoS）**: ハンドル数・引数件数・コンテキスト数・ソース長・タイムアウトは既存の上限を流用する。ノード対応表の大きさは DOM のノード数の上限（`DomLimits`）に縛られる
- **SSRF**: 段階 1 はページスクリプトを再実行しないため、新たな取得は発生しない。再実行案を却下する理由の 1 つ
- **偽装（`SEC-2`・`CDP-6`）**: main world の評価は UA・`navigator`・画面などの識別面に触れない。ページ JS のヒープが無いことを偽の値で補わず、`exceptionDetails` で正直に返す。`Runtime.addBinding` 等の未実装メソッドに一律で成功を返さない
- **段階 2 を採る場合の追加リスク**: ページ JS の実行後にヘルパーを注入すると、退避する組込みが改竄済みになり得る。ページスクリプトより前に注入するか、そのリスクを受容して明記する。ヘルパーのグローバルがページ JS から見えること（改竄耐性・検出され得る点）も判断事項になる

## 8. 未確定事項と確認手順

- Puppeteer の実測: **実施済み**（#1349。2026-10-30）。`page.$("body")` は `Runtime.evaluate` の `userGesture` で停止した。結果は [puppeteer-compat.md](./puppeteer-compat.md)。§2 の連鎖の読解は、送信順と `userGesture` の点で実測と食い違った（§2）。`selector` の後半は未到達で未確認
- Playwright の main world 評価の連鎖の確認: **未確認**（#1349 では実施していない）
- Chromium での `objectId` と `executionContextId` の排他エラーの照合: **未確認**（#1349 では Chromium 実機を使っていない）

## 9. 実装 issue 分割案（起票はオーナー承認後。本 PR では起票しない）

| # | 対象 | 内容 | 依存 |
| - | ---- | ---- | ---- |
| 1 | cdp | `Runtime.callFunctionOn` の `executionContextId` 受理（isolated world のみ。排他・`this` undefined・引数の contextId 一致・検証順・テスト）。**#1345 で実装済み** | なし |
| 2 | cdp | 段階 1: main world の評価（`Runtime.evaluate` の main の contextId と `contextId` 省略・`releaseObject`・`callFunctionOn` の `World::Main` 許可・既存テスト `tests/support/runtime_evaluate_cases.rs` の main world エラー期待の更新・`MAIN_WORLD_EVALUATION_UNSUPPORTED` の扱い）。**#1347 で実装済み**（定数は削除。オーナー判断は §3） | 段階 1 の既定有効・opt-in のオーナー判断（済） |
| 3 | core（設計は main） | 初期の文書順番号によるノード同一性の API（`DomBridge` / `ScriptContextHandle`）。**#1346 で実装済み** | なし（2 と並行可） |
| 4 | cdp | `DOM.describeNode`（`objectId`）・`DOM.resolveNode`（`backendNodeId` + `executionContextId`）。DOM ドメインの採番と統一する。**#1348 で実装済み** | 3、main world を対象にするなら 2 |
| 5 | docs / harness | Puppeteer `page.$` の段階の実測と `puppeteer-compat.md` の該当行の更新（本書へのポインタ追記を含む）。**#1349 で実施済み**（停止点は `Runtime.evaluate` の `userGesture`） | 1・2・4 と #1334 |
| 6 | オーナー判断 | 段階 2（ページランタイムの保持）を採るか。判断用の Issue は実装ツリー（#10 配下）に入れない | 5 の実測結果 |
