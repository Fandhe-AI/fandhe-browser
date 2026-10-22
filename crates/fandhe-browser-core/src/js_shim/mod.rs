//! js_shim: ページ内 JS に `document` / Node 系を見せる JS shim の埋め込みと注入
//! （TASK-108・Issue #778・ビヘイビア `JS-5`。SSOT: `js-engine.md`「ページ内 JS 実行の
//! 設計制約」決定 3）。
//!
//! shim 本体は JS ソース（`dom.js`）で、`include_str!` によりバイナリへ埋め込む。実行時に
//! 外部ファイル・ネットワークから読まない。shim はノード ID を隠したラッパーを作り、操作を
//! すべて [`crate::dom_bridge`] のネイティブ関数 `__dom.op` へ転送する（DOM の実体・
//! HTML パース・上限検証は core 側）。
//!
//! 呼び出し元は [`crate::js_stub::JsRuntime::install_dom_shim`]（ページ実行ランナー
//! （TASK-109）がページごとに呼ぶ）。エンジンは `dyn JsEngine` 越しに使い、V8 / boa の
//! 具象型には依存しない。同一ソースが両エンジンで動く（ES2015 の範囲）。
//!
//! # 拡張
//!
//! `window` / `self` / `location` / `navigator` / `console`（#779）は `window.js` に置き、
//! [`JS_SHIM_SOURCES`] の dom.js の後ろに足してある（評価は配列の順）。`navigator` は
//! 自動化ブラウザであることを正直に示し（`SEC-2`）、`location` はランナーが
//! [`crate::dom_bridge::DomBridge::set_location`] で設定した URL を読む。
//!
//! # 未実装（REPAIR-3）
//!
//! `dom.js` / `window.js` / `events.js` 冒頭の「未実装」節を参照（DOMException 写像・
//! NodeList 互換・要素レベルのイベント系・`document.location`・追加 API）。`events.js`（#781）は
//! `document` / `window` の最小 `addEventListener` とランナー用ディスパッチャー
//! `__fandheLifecycle` を提供する。`timers.js`（#782・`JS-7`）は `setTimeout` / `setInterval` /
//! `queueMicrotask` とランナー用ディスパッチャー `__fandheRunTimer` / `__fandheRunMicrotasks` を
//! 提供する（期限・順序・上限の実体は core の `event_loop`）。`net.js`（#783・`JS-8`）は GET だけの
//! 最小の `fetch` / `XMLHttpRequest` とランナー用ディスパッチャー `__fandheNetDeliver` を提供する
//! （取得・上限・SSRF 検査の実体は core の `page_net` と Fetcher）。`observers.js`（#1138・`JS-5`・
//! `JS-7`・`JS-6`）は `MutationObserver` / `MutationRecord` を提供し、dom.js の変更系ラッパーが呼ぶ
//! フックで検出した変更を microtask で配信する（oldValue・transient registered observer・
//! innerHTML / textContent 設定時の追加・削除ノード等は未実装。診断は `mutationOptionsIgnored` op）。
//! `style.js`（#1141・`JS-5`・`JS-6`・`SEC-2`）は要素の `style`（最小の `CSSStyleDeclaration` 相当）を
//! 提供し、`style` 属性へ既存の `setAttribute` op で同期する（`getComputedStyle`・レイアウト・値の構文検証・
//! HTML 由来の `style` 属性の読み取りは未実装。ベンダー接頭辞付きのプロパティは意図して定義しない）。
//! `stylesheet.js`（#1202・`JS-5`・`JS-6`・`REPAIR-3`）は `<style>` 要素の `sheet` と `document.styleSheets` の
//! 最小の面（`insertRule` / `deleteRule` / 持続する `cssRules`）を提供する。emotion の speedy 経路を通すためのもので、
//! `<style>` の内容の反映・Rust 側 CSSOM との連携・グローバルの `CSSStyleSheet` は未実装。dom.js の窓口
//! `__fandheDomStyleSheetHandoff` を install 時に捕捉し、abort.js の `DOMException` を使うため style.js の次に評価する。
//! `url.js`（#1144・`JS-5`・`JS-6`・`REPAIR-3`）は `URL` / `URLSearchParams` を提供し、URL の解析は
//! bridge op `urlParse`（core の `reqwest::Url` に委譲）、form-urlencoded は JS で実装する
//! （`protocol`・`host`・`port`・`pathname` 等の setter・`createObjectURL`・`URLPattern`・
//! `URLSearchParams.sort` 等は未実装）。
//! `abort.js`（#1150・`JS-5`・`JS-6`・`JS-7`・`JS-8`・`REPAIR-3`）は `DOMException` / `AbortSignal` /
//! `AbortController` を提供し、timers.js の後・net.js の前に評価する（`AbortSignal.timeout` が
//! `setTimeout` を、net.js の `fetch` が abort.js の窓口 `__fandheAbortFollow` を install 時に捕捉するため）。
//! `storage.js`（#1167・`JS-5`・`JS-6`・`SEC-2`・`REPAIR-3`）は `Storage` / `localStorage` /
//! `sessionStorage` を in-memory で提供し（ページ実行ごとに空から始まる。永続化・オリジン分離・`storage`
//! イベント・プロパティ形式のアクセスは未実装）、abort.js の `DOMException` を install 時に捕捉するため
//! abort.js の後に評価する。
//! `history.js`（#1171・`JS-5`・`JS-6`・`SEC-2`・`REPAIR-3`）は `History` / `history` を提供する。
//! `pushState` / `replaceState` は同一オリジンの URL だけを bridge op `historySetUrl` で受け付け
//! （検査は core 側）、state は予算付きの再帰複製（ノード数・深さ・累計サイズの上限・undefined を保持）で
//! 保持する。#1177 でページ内の履歴エントリ（最大 50 件・state の合計 8 MiB まで）を持ち、`back` / `forward` /
//! `go` が履歴の範囲で URL と state を動かして `popstate`（`PopStateEvent`・`window.onpopstate`）を配る
//! （取得・遷移はせず、microtask で近似。`go(0)` は実行せず診断に記録。hashchange は未実装）。
//! abort.js の `DOMException` と events.js の `Event` を install 時に捕捉するため、abort.js・events.js の後
//! （storage.js の次）に評価する。
//! 汎用の `EventTarget` / `Event` / `dispatchEvent`（#1177・`JS-5`）は `events.js` にあり、リスナー例外の報告に
//! abort.js の `DOMException` と timers.js の `queueMicrotask` を使うため、timers.js・abort.js の後に評価する。
//! `window` / `document` / `AbortSignal` は `EventTarget` を継承しない。`CustomEvent`・要素の
//! `dispatchEvent`（対象要素の上だけに配る。#1242）は実装済みで、木に沿った伝播は未実装（`REPAIR-3`）。
//! フォーカス追跡（#1185・`JS-5`・`JS-6`・`REPAIR-3`）は `dom.js` が持ち（`Element.prototype.focus` / `blur` と
//! `document.activeElement`。フォーカス可否は `isFocusable` op）、focus / blur は install 中だけの一時
//! グローバル `__fandheDomFocusHandoff` 経由で `events.js` が対象要素にだけ配る。focusin / focusout・
//! FocusEvent・`document.hasFocus()` は未実装。

use fandhe_browser_js::{EvaluateOptions, JsEngine, JsEngineError};

/// 埋め込み済みの shim ソース 1 本。
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct JsShimSource {
    /// ソース名（ログ・[`JsShimInstallation`] 用。パスではない）。
    pub name: &'static str,
    /// JS ソース本体。
    pub source: &'static str,
}

/// 評価順に並べた shim ソース（`JS-5`）。
pub const JS_SHIM_SOURCES: &[JsShimSource] = &[
    JsShimSource {
        name: "dom.js",
        source: include_str!("dom.js"),
    },
    JsShimSource {
        name: "window.js",
        source: include_str!("window.js"),
    },
    JsShimSource {
        name: "timers.js",
        source: include_str!("timers.js"),
    },
    JsShimSource {
        name: "abort.js",
        source: include_str!("abort.js"),
    },
    JsShimSource {
        name: "events.js",
        source: include_str!("events.js"),
    },
    JsShimSource {
        name: "net.js",
        source: include_str!("net.js"),
    },
    JsShimSource {
        name: "observers.js",
        source: include_str!("observers.js"),
    },
    JsShimSource {
        name: "style.js",
        source: include_str!("style.js"),
    },
    JsShimSource {
        name: "stylesheet.js",
        source: include_str!("stylesheet.js"),
    },
    JsShimSource {
        name: "url.js",
        source: include_str!("url.js"),
    },
    JsShimSource {
        name: "storage.js",
        source: include_str!("storage.js"),
    },
    JsShimSource {
        name: "history.js",
        source: include_str!("history.js"),
    },
];

/// [`install`] の結果（REPAIR-4: 将来の拡張に備え構造体で返す）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JsShimInstallation {
    /// 評価したソース名（評価順）。
    pub evaluated: Vec<&'static str>,
}

/// shim ソースを順にエンジンで評価する。
///
/// 前提: エンジンに `__dom.op` が bind 済みであること
/// （[`crate::dom_bridge::DomBridge::register`]。`JsRuntime::install_dom_shim` が行う）。
/// 失敗した時点で打ち切り、以降のソースは評価しない。
pub fn install(
    engine: &mut dyn JsEngine,
    options: &EvaluateOptions,
) -> Result<JsShimInstallation, JsEngineError> {
    install_with(engine, &mut || Ok(options.clone()))
}

/// [`install`] の評価オプションを shim ごとに決め直す版。
///
/// `next_options` は各 shim の評価直前に呼ばれ、その shim に課す [`EvaluateOptions`] を返す。
/// `Err` を返すとその時点で打ち切る。ページ実行ランナー（`page_runner`）が、先行 shim の
/// 消費時間を差し引いたページの残り予算を課すために使う（`JS-6`）。
pub fn install_with(
    engine: &mut dyn JsEngine,
    next_options: &mut dyn FnMut() -> Result<EvaluateOptions, JsEngineError>,
) -> Result<JsShimInstallation, JsEngineError> {
    let mut evaluated = Vec::with_capacity(JS_SHIM_SOURCES.len());
    for shim in JS_SHIM_SOURCES {
        let options = next_options()?;
        engine.evaluate_script(shim.source, &options)?;
        evaluated.push(shim.name);
    }
    Ok(JsShimInstallation { evaluated })
}
