# anti-bot 回避機能非提供の設計レビュー記録（TASK-86 草案）

**対象**: TASK-86 / SEC-2 / MS-7
**状態**: 草案（レビュー結果の記録。最終判断は人間担当）
**関連 Issue**: #350
**監査時点**: main `03b8617` と PR #808（TASK-43.3a・`Browser.getVersion`、OPEN。#808 由来の確認は個別に明記する）。行番号は監査時点のもの

## 目的

プロファイル分離（`PROF-1`〜`PROF-5`）と CDP 互換（`CDP-6`）の実装が、意図せずフィンガープリンティング偽装等の anti-bot 回避的機能を提供していないかを、設計とコードの両面で確認し記録する（SEC-2。SEC-1 の「認証突破・anti-bot 回避は実装しない」方針の実装側確認）。

## 結論

偽装機能・anti-bot 回避として作用する実装は検出されなかった（P0: 0 件、P1: 0 件）。指摘は P2 が 1 件、P3 が 3 件で、扱いを下表「指摘の扱い」に記す。

## 観点別の確認結果

| # | 観点 | 確認箇所 | 結果 |
| - | ---- | -------- | ---- |
| 1 | UA（core） | `crates/fandhe-browser-core/src/fetch.rs:205`（`USER_AGENT` は `fandhe-browser/<版>`）、`:627-633`（`ClientBuilder` は `.user_agent(USER_AGENT)` のみ。`default_headers`・`Accept-Language` の追加なし） | 偽装なし |
| 2 | ヘッダ注入口 | `fetch.rs:86-111`（`FetchOptions` は timeout・connect_timeout・max_redirects・max_body_bytes・allow_private_network_access・recorder のみ。ヘッダ・UA の注入口なし） | 偽装なし |
| 3 | JS shim の `navigator` | `crates/fandhe-browser-core/src/js_shim/window.js:52`・`:119-134`（`userAgent` は `op('navigatorUserAgent')` の getter のみで setter なし。`webdriver` は常に `true`）、`:26`（`language`・`platform`・`plugins` 等を追加しない方針のコメント）。`dom_bridge.rs:689`（`NavigatorUserAgent` は `fetch::USER_AGENT` を返す）。`tests/support/js_shim_cases.rs:360-428`（Chrome・Mozilla・Safari・Headless・Gecko を含まないこと、`webdriver` の代入不可、`navigator` 差し替えの影響を固定） | 偽装なし |
| 4 | fingerprint（TLS・ヘッダ順・画面） | `fetch.rs` に rustls 既定以外の設定なし（`:231` は ring provider の install のみ）。js_shim に `screen`・`innerWidth`・`languages`・`hardwareConcurrency`・`deviceMemory` の定義なし（grep 0 件）。crates 全体で stealth・spoof・fingerprint を実装する箇所なし（テスト・コメント上の禁止表明のみ） | 偽装なし |
| 5 | 設定項目 | `crates/fandhe-browser-cli/src/startup_config.rs:10-48`（環境変数 `FANDHE_BROWSER_CONFIG` のみ）。`crates/fandhe-browser-core/src/config.rs` の `Config` は `[profile]`・`[js]`・`[rendering]` のみで、UA・ヘッダ・locale・viewport の項目なし。ai・mcp の各 `src` にも UA・ヘッダ・emulate・stealth 設定なし（`ai/src/api.rs:105-130` は受信側の Host・Origin・Content-Type 検証） | 偽装なし |
| 6 | CDP-6 未実装メソッド | `crates/fandhe-browser-cdp/src/protocol.rs:87`（`METHOD_NOT_FOUND` = -32601 "method not implemented"）、`:20-45`（空 success フォールバックを採用しない理由に `Emulation.*`・`Network.setUserAgentOverride`・`Page.addScriptToEvaluateOnNewDocument` を明記）、`:373-` の `builtin_handlers` は `Page.navigate`・`DOM.getDocument`・`DOM.querySelector`・`DOM.requestChildNodes` のみ。`tests/playwright_trace.rs:288`・`tests/devtools_browser.rs:164,187` が -32601 を固定。`Emulation.*`・`Network.*`・`Fetch.*`・`Target.*` のハンドラなし | 偽装なし。未実装メソッドは「成功を一律に返す」のではなく -32601 を返すため、検出回避として作用しない |
| 7 | プロファイル分離 | `crates/fandhe-browser-profile/src/profile.rs:255-296`（`DataKind` は Cookies・Storage・Cache・History の 4 種固定。UA・ヘッダ・locale・viewport・fingerprint を保持する型・フィールドなし）、`config.rs:440-470`（`ProfileConfig` は `root` と `isolation` のみ）。分離の目的は保管領域の境界保護（`openat` + `NOFOLLOW`、`profile.lock`） | 偽装なし（用途は境界保護） |
| 7b | CSSOM 挙動プロファイル（PLUG-8） | `crates/fandhe-browser-core/src/cssom_profile.rs:13,47,386`（Chrome / Safari は CSS プロパティの対応可否のみ。`navigator.userAgent` 等の識別面は読み書きしない）、`tests/cssom_profile.rs:665-706`（UA は固定値で、gate 適用・名前解決で不変。ブラウザを装わないことを固定） | 偽装なし |
| 8 | `/json/version`・`Browser.getVersion` | main: `crates/fandhe-browser-cdp/src/discovery.rs:54`（`Browser`・`User-Agent` とも `fandhe-browser/<版>`。`V8-Version` なし）。PR #808（OPEN）: `Browser.getVersion` は `protocolVersion`・`product`・`userAgent` を定数のみで返し、`revision`・`jsVersion` は偽値を出さず省略。単体テスト `cdp2_browser_get_version_returns_real_values_only` が Chrome・Chromium・HeadlessChrome・Mozilla・AppleWebKit を含まないことを、`cdp2_browser_get_version_matches_json_version` が `/json/version` との一致を固定。`/json/version/` は明示ルートで他の `/json/*` には広げていない | 偽装なし。Chrome を装う余地は現状コード上なし |

## 指摘の扱い

| ID | 重大度 | 内容 | 扱い |
| -- | ------ | ---- | ---- |
| P2-1 | P2 | robots.txt（SEC-3）の実行時の扱いが設計書に明記されていない | 解消（下記「robots.txt の責任分界」） |
| P3-1 | P3 | 製品名定義が 2 箇所に重複（core の `fetch::USER_AGENT` と cdp の `discovery::PRODUCT`〔PR #808〕）。将来ずれる余地があった | 解消（#814）。cdp の `discovery::PRODUCT` を core の `fetch::USER_AGENT` の別名にし、一致を回帰テスト（`sec2_json_version_product_derives_from_core_user_agent`・`sec2_browser_get_version_product_derives_from_core_user_agent`）で固定 |
| P3-2 | P3 | 識別面の不変条件が未明文化 | 下記「不変条件」として本書に明文化し、`.claude/rules/security.md` の偽装禁止節にも 1 行追記した |
| P3-3 | P3 | CDP-6 方針の継続監視（`Target.setAutoAttach`・`Browser.setDownloadBehavior` は TASK-43.3c〔#908〕で実体に接続して個別登録した。UA 等の識別面を変更せず、`filter` 等の未対応入力は `-32602` で拒否、ダウンロード実行系は未実装で保持のみ。TASK-43.3g〔#942〕で `Browser.setDownloadBehavior` は `deny`〔`eventsEnabled:true` を含む。イベントは送出しない〕のみ受理に広げ、`allow*` は拒否のまま。`Target.setAutoAttach` の `waitForDebuggerOnStart:true` は保持のみで、いずれも識別面に影響しない。今後も同様に個別登録を監視する） | `AGENTS.md` の AI PR レビュー観点に 1 項目追記した（下記「CDP-6 の継続監視」） |

## robots.txt の責任分界（P2-1 の解消）

SEC-3 は「本項は実測対象の選定基準であり、ブラウザ本体の実行時に robots.txt を解釈する機能は定めない」と定める（`SEC-3`・TASK-70・TASK-85）。したがって次のとおり責任を分ける。

- 実測対象サイト群の選定（TASK-70）: robots.txt の `User-agent: *` グループを RFC 9309 に従い判定し、Disallow のサイトを対象から除外する（`docs/design/site-catalog-task70.md`、除外一覧は `docs/design/anti-bot-exclusion.md`）
- ブラウザ本体の実行時: `Fetcher` と `Page.navigate` は robots.txt を取得も判定もしない（実装なし。仕様どおり）
- 実行時のアクセス先の適否（robots.txt・利用規約の遵守）は利用者側の責任とする

この分界は仕様どおりであり、spec 側の変更は不要。

## 不変条件（P3-2）

プロファイル・設定・CSSOM プロファイル（PLUG-8 の `chrome.json`・`safari.json`）から、UA 等の識別面（UA・ヘッダ・`navigator`・画面・TLS・locale）を変更できない。

- `profile` の `DataKind`・`ProfileConfig`、core の `Config`、CSSOM 挙動プロファイルに、識別属性を保持するフィールドを追加しない
- UA の定義元は単一（`fetch::USER_AGENT`。cdp の `discovery::PRODUCT` はその別名）とし、`navigator.webdriver` は常に `true` とする
- 回帰テスト: `tests/support/js_shim_cases.rs:360-428`（`navigator` の識別面）、`tests/cssom_profile.rs:665-706`（CSSOM プロファイルは UA に影響しない）、PR #808 の `cdp2_browser_get_version_returns_real_values_only`

## CDP-6 の継続監視（P3-3）

新規 CDP メソッドを登録する際は、stealth 系のメソッド（`Emulation.*`・`Network.setUserAgentOverride`・`Page.addScriptToEvaluateOnNewDocument` 等）を SEC-2 の観点で個別にレビューする。これらは -32601 のまま残すか、ヘッダ・UA を実際に書き換えない旨を確認する。-32601 を固定する既存の回帰テスト（`tests/devtools_browser.rs`・`tests/playwright_trace.rs`）は維持する。

例外（2026-10-15 オーナー判断。TASK-43.3m・#1041。Playwright `newPage()` 到達のため）: `Emulation.setFocusEmulationEnabled` と `Emulation.setEmulatedMedia` は、次の条件をすべて満たす場合に限り、値の保持だけで `{}` を返してよい。

- 受理する値は Playwright が既定で送る値だけにする（`setFocusEmulationEnabled` は `enabled` の真偽、`setEmulatedMedia` は `media:""` と既定の media feature 値）。それ以外は `-32602`
- 保持値をどの API の応答にも反映しない。UA・ヘッダ・`navigator`・画面・TLS・locale・タイムゾーンに触れない
- 保持だけで実際のエミュレーションを行わないことを `///` と `docs/design/playwright-compat.md` に明記する（`REPAIR-3`）
- `Emulation.setDeviceMetricsOverride`・`setUserAgentOverride`・`setLocaleOverride`・`setTimezoneOverride` と `Network.setUserAgentOverride` は `-32601` のまま（Playwright 側は `newContext({ viewport: null })` で `setDeviceMetricsOverride` を送らない）

この例外は 2 メソッドの既定値に限る。保持値を実処理（CSSOM の media 評価・フォーカス管理など）に接続する際は、改めて SEC-2 の観点でレビューする。

補足（#1325・2026-10-30 オーナー判断）: `Page.addScriptToEvaluateOnNewDocument` は空 `source` に加え、`//# sourceURL=<ASCII 印字可能 1 文字以上>` の 1 行（256 バイト以下）だけの `source`（Puppeteer の `newPage()` が送る `//# sourceURL=pptr:internal`）も no-op で受理してよい。実行経路は無く値も保持しない。実行内容を 1 文字でも含む `source`（改行・LS/PS・空白・非 ASCII・旧形式 `//@`・長さ超過を含む）は `-32602` のまま。受理する形を広げる変更は改めて SEC-2 の観点でレビューする。

## 確認記録

| 日付 | 確認内容 | 結果 |
| ---- | -------- | ---- |
| 2026-10-10 | 観点 1〜8 の設計・コード確認（草案作成） | 偽装機能なし。P2 1 件（解消）・P3 3 件 |
| 2026-10-14 | TASK-43.3g（#942）: `Browser.setDownloadBehavior` の `deny` + `eventsEnabled:true` と `Target.setAutoAttach` の `waitForDebuggerOnStart:true` の受理を確認（UA・ヘッダ・`navigator`・画面・TLS・locale に触れない。`Emulation.*` は -32601 のまま） | 偽装機能なし |
| 2026-10-16 | TASK-43.3j（#1038）: セッション付き Log.enable・Runtime.enable・Network.enable の受理（フラグのみ・イベントなし）。UA・ヘッダ・navigator・画面・TLS・locale に触れない。Network.setUserAgentOverride・Emulation.setUserAgentOverride は -32601 のまま（tests/devtools_browser.rs・cdp6_builtin_other_methods_remain_unimplemented の固定を維持） | 偽装機能なし |
| 2026-10-16 | TASK-43.3k（#1039）: セッション付き Page.setLifecycleEventsEnabled の受理と Page.lifecycleEvent（init・load）の固定名での送出。UA・ヘッダ・navigator・画面・TLS・locale に触れない | 偽装機能なし |
| 2026-10-16 | TASK-43.3l（#1040）: Page.addScriptToEvaluateOnNewDocument は空 source だけを受理し（実行経路なし）、非空 source は -32602。セッション付き Target.setAutoAttach は状態を保持するだけで子 attach は 0 件。UA・ヘッダ・navigator・画面・TLS・locale に触れない。Emulation.setUserAgentOverride・Network.setUserAgentOverride は -32601 のまま（tests/devtools_browser.rs・cdp6_builtin_other_methods_remain_unimplemented の固定を維持） | 偽装機能なし |
| 2026-10-16 | TASK-43.3m（#1041）: セッション付き Emulation.setFocusEmulationEnabled と Emulation.setEmulatedMedia（Playwright 既定値のみ）の受理。保持値はどの API からも読まれず（読み取りは cfg(test) のみ）、UA・ヘッダ・navigator・画面・TLS・locale・タイムゾーンに触れない。Emulation.setUserAgentOverride・setLocaleOverride・setTimezoneOverride・setDeviceMetricsOverride と Network.setUserAgentOverride は -32601 のまま（tests/devtools_browser.rs・cdp6_* の固定を維持）。確認は実装担当による自己確認で、security-auditor のレビューは本セッションでは未実施 | 偽装機能なし（自己確認） |
| 2026-10-30 | #1322: Page.lifecycleEvent（DOMContentLoaded）と Page.domContentEventFired を固定名・固定形（数値 timestamp のみ）で送出。送出条件は core の発火記録（型付き列挙）に基づき、打ち切り・記録なし・未知の結果では送らない。UA・ヘッダ・navigator・画面・TLS・locale に触れない。確認は実装担当による自己確認 | 偽装機能なし（自己確認） |
| 2026-10-30 | #1325: Page.addScriptToEvaluateOnNewDocument が `//# sourceURL=<ASCII 印字可能 1 文字以上>` の 1 行（256 バイト以下）だけの source を空 source と同じ no-op で受理（Puppeteer の newPage 用）。実行経路なし・値は保持せず、改行・LS/PS・空白・非 ASCII・旧形式・長さ超過は -32602。UA・ヘッダ・navigator・画面・TLS・locale に触れない。security-auditor のレビュー済み（2026-10-30、迂回経路なし） | 偽装機能なし |
| 2026-10-31 | #1357: Runtime.evaluate が `userGesture`（bool のみ）を効果なしで受理（Puppeteer の puppeteerUtil 注入用）。値は保持も評価への受け渡しもせず、メソッドの成功を一律に返すものではない（評価・エラー応答は従来どおり）。非 bool は -32602、他の未知キー（timeout・uniqueContextId・objectGroup 等）も -32602 のまま。UA・ヘッダ・navigator・画面・TLS・locale・ユーザー活性化に触れない。security-auditor の事後レビュー済み（2026-10-31・633e0100。許可キーの追加は `userGesture` のみ、値は評価・状態に渡らず、指摘なし） | 偽装機能なし |
| 2026-10-31 | #1364: `Network.enable` 済みセッションに、`Page.navigate` が実際に取得した文書の Network.requestWillBeSent・responseReceived・loadingFinished を送出。request.headers は実際に送った User-Agent 定数（`fandhe-browser/<版>`）のみ、response.headers は観測した Content-Type のみ（長さ上限・制御文字検査つき）で、status・URL・本文長は core の取得結果そのもの。about:blank・取得失敗・ブラウザレベル遷移では送らず、リダイレクトは最終 URL と最終ステータスの組だけ。観測していない値（statusText・他ヘッダ・IP・タイミング）は出さない。UA・ヘッダ・navigator・画面・TLS・locale に触れず、fetch の挙動も変えない。確認は実装担当による自己確認で、security-auditor のレビューは本セッションでは未実施 | 偽装機能なし（自己確認） |
