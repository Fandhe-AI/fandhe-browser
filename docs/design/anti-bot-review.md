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
| 5 | 設定項目 | `crates/fandhe-browser-cli/src/startup_config.rs:10-47`（環境変数 `FANDHE_BROWSER_CONFIG` のみ）。`crates/fandhe-browser-core/src/config.rs` の `Config` は `[profile]`・`[js]`・`[rendering]` のみで、UA・ヘッダ・locale・viewport の項目なし。ai・mcp の各 `src` にも UA・ヘッダ・emulate・stealth 設定なし（`ai/src/api.rs:105-130` は受信側の Host・Origin・Content-Type 検証） | 偽装なし |
| 6 | CDP-6 未実装メソッド | `crates/fandhe-browser-cdp/src/protocol.rs:87`（`METHOD_NOT_FOUND` = -32601 "method not implemented"）、`:20-45`（空 success フォールバックを採用しない理由に `Emulation.*`・`Network.setUserAgentOverride`・`Page.addScriptToEvaluateOnNewDocument` を明記）、`:373-` の `builtin_handlers` は `Page.navigate`・`DOM.getDocument`・`DOM.querySelector`・`DOM.requestChildNodes` のみ。`tests/playwright_trace.rs:288`・`tests/devtools_browser.rs:164,187` が -32601 を固定。`Emulation.*`・`Network.*`・`Fetch.*`・`Target.*` のハンドラなし | 偽装なし。未実装メソッドは「成功を一律に返す」のではなく -32601 を返すため、検出回避として作用しない |
| 7 | プロファイル分離 | `crates/fandhe-browser-profile/src/profile.rs:255-296`（`DataKind` は Cookies・Storage・Cache・History の 4 種固定。UA・ヘッダ・locale・viewport・fingerprint を保持する型・フィールドなし）、`config.rs:440-470`（`ProfileConfig` は `root` と `isolation` のみ）。分離の目的は保管領域の境界保護（`openat` + `NOFOLLOW`、`profile.lock`） | 偽装なし（用途は境界保護） |
| 7b | CSSOM 挙動プロファイル（PLUG-8） | `crates/fandhe-browser-core/src/cssom_profile.rs:13,47,386`（Chrome / Safari は CSS プロパティの対応可否のみ。`navigator.userAgent` 等の識別面は読み書きしない）、`tests/cssom_profile.rs:665-706`（UA は固定値で、gate 適用・名前解決で不変。ブラウザを装わないことを固定） | 偽装なし |
| 8 | `/json/version`・`Browser.getVersion` | main: `crates/fandhe-browser-cdp/src/discovery.rs:54`（`Browser`・`User-Agent` とも `fandhe-browser/<版>`。`V8-Version` なし）。PR #808（OPEN）: `Browser.getVersion` は `protocolVersion`・`product`・`userAgent` を定数のみで返し、`revision`・`jsVersion` は偽値を出さず省略。単体テスト `cdp2_browser_get_version_returns_real_values_only` が Chrome・Chromium・HeadlessChrome・Mozilla・AppleWebKit を含まないことを、`cdp2_browser_get_version_matches_json_version` が `/json/version` との一致を固定。`/json/version/` は明示ルートで他の `/json/*` には広げていない | 偽装なし。Chrome を装う余地は現状コード上なし |

## 指摘の扱い

| ID | 重大度 | 内容 | 扱い |
| -- | ------ | ---- | ---- |
| P2-1 | P2 | robots.txt（SEC-3）の実行時の扱いが設計書に明記されていない | 解消（下記「robots.txt の責任分界」） |
| P3-1 | P3 | 製品名定義が 2 箇所に重複（core の `fetch::USER_AGENT` と cdp の `discovery::PRODUCT`〔PR #808〕）。将来ずれる余地がある | #814 で対応する |
| P3-2 | P3 | 識別面の不変条件が未明文化 | 下記「不変条件」として本書に明文化し、`.claude/rules/security.md` の偽装禁止節にも 1 行追記した |
| P3-3 | P3 | CDP-6 方針の継続監視（今後 `Target.setAutoAttach`・`Browser.setDownloadBehavior` 等が個別登録される見込み） | `AGENTS.md` の AI PR レビュー観点に 1 項目追記した（下記「CDP-6 の継続監視」） |

## robots.txt の責任分界（P2-1 の解消）

SEC-3 は「本項は実測対象の選定基準であり、ブラウザ本体の実行時に robots.txt を解釈する機能は定めない」と定める（`SEC-3`・TASK-70・TASK-85）。したがって次のとおり責任を分ける。

- 実測対象サイト群の選定（TASK-70）: robots.txt の `User-agent: *` グループを RFC 9309 に従い判定し、Disallow のサイトを対象から除外する（`docs/design/site-catalog-task70.md`、除外一覧は `docs/design/anti-bot-exclusion.md`）
- ブラウザ本体の実行時: `Fetcher` と `Page.navigate` は robots.txt を取得も判定もしない（実装なし。仕様どおり）
- 実行時のアクセス先の適否（robots.txt・利用規約の遵守）は利用者側の責任とする

この分界は仕様どおりであり、spec 側の変更は不要。

## 不変条件（P3-2）

プロファイル・設定・CSSOM プロファイル（PLUG-8 の `chrome.json`・`safari.json`）から、UA 等の識別面（UA・ヘッダ・`navigator`・画面・TLS・locale）を変更できない。

- `profile` の `DataKind`・`ProfileConfig`、core の `Config`、CSSOM 挙動プロファイルに、識別属性を保持するフィールドを追加しない
- UA の定義元は単一（`fetch::USER_AGENT`）とし、`navigator.webdriver` は常に `true` とする
- 回帰テスト: `tests/support/js_shim_cases.rs:360-428`（`navigator` の識別面）、`tests/cssom_profile.rs:665-706`（CSSOM プロファイルは UA に影響しない）、PR #808 の `cdp2_browser_get_version_returns_real_values_only`

## CDP-6 の継続監視（P3-3）

新規 CDP メソッドを登録する際は、stealth 系のメソッド（`Emulation.*`・`Network.setUserAgentOverride`・`Page.addScriptToEvaluateOnNewDocument` 等）を SEC-2 の観点で個別にレビューする。これらは -32601 のまま残すか、ヘッダ・UA を実際に書き換えない旨を確認する。-32601 を固定する既存の回帰テスト（`tests/devtools_browser.rs`・`tests/playwright_trace.rs`）は維持する。

## 確認記録

| 日付 | 確認内容 | 結果 |
| ---- | -------- | ---- |
| 2026-10-10 | 観点 1〜8 の設計・コード確認（草案作成） | 偽装機能なし。P2 1 件（解消）・P3 3 件 |
