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
//! `dom.js` / `window.js` 冒頭の「未実装」節を参照（DOMException 写像・NodeList 互換・
//! イベント系・`document.location`・追加 API）。

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
    let mut evaluated = Vec::with_capacity(JS_SHIM_SOURCES.len());
    for shim in JS_SHIM_SOURCES {
        engine.evaluate_script(shim.source, options)?;
        evaluated.push(shim.name);
    }
    Ok(JsShimInstallation { evaluated })
}
