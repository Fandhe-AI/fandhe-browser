//! JS エンジン種別の列挙と、同梱エンジン一覧を返す入口（TASK-28（28.2）・
//! ビヘイビア `JS-1`・Issue #148）。
//!
//! `docs/spec/04-behavior/js-engine.md`「JS エンジンの切替方式」決定 5 は、
//! js crate が feature の有無に関係なく常に定義するエンジン種別の列挙型
//! （V8・Boa）と、同梱エンジンの一覧を返す関数を公開する契約を定める。
//! 本モジュールはその契約の入口のみを提供する。
//!
//! 呼び出し元（将来）: `fandhe-browser-core` は TASK-30（Issue #143・
//! `JS-2`）で `js_stub::execute_js_stub` を実エンジン呼び出しに置換する際、
//! また TASK-91（設定ファイルの `[js] engine` 解析）は「同梱されている
//! エンジンは何か」を問い合わせる際に、それぞれ [`bundled_engines`] を
//! 経由する想定。
//!
//! 本モジュールに今後追加される予定（本 Issue のスコープ外）:
//! - エンジン抽象トレイト本体・[`EngineKind`] からトレイトオブジェクトを
//!   生成する関数（未同梱の種別への `Err` 返却を含む） → TASK-28.3
//!   （Issue #149）
//! - 両エンジン共通のコンフォーマンステスト → TASK-28.4（Issue #150）
//!
//! [`EngineKind`] の文字列表現（設定ファイルの `"v8"`/`"boa"` との相互
//! 変換・未同梱時のエラーメッセージ整形）は TASK-91.2（Issue #215）で
//! 確定するため、本 Issue では先取りしない。

/// 本 crate が抽象化対象とする JS エンジンの種別。
///
/// `docs/spec/04-behavior/js-engine.md` 決定 5 は V8・Boa の 2 種に固定した
/// 設計であるため、`fandhe-browser-core::Error`（他モジュールが今後バリアント
/// を追加する前提）とは異なり、`#[non_exhaustive]` は付けない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// V8（`rusty_v8`）エンジン。既定エンジン（README「実装方針（要点）」）。
    V8,
    /// boa（`boa_engine`）エンジン。切替先エンジン。
    Boa,
}

/// この crate に同梱されている（有効化された feature に対応する）エンジンの
/// 固定配列（[`bundled_engines`] の実体）。
const BUNDLED: &[EngineKind] = &[
    #[cfg(feature = "js-v8")]
    EngineKind::V8,
    #[cfg(feature = "js-boa")]
    EngineKind::Boa,
];

/// 同梱エンジンの一覧を、V8 → Boa の固定順で返す。
///
/// 順序は `js-engine.md`「(1) 省略」の「同梱エンジンから V8 → boa の優先順で
/// 選ぶ」という後続契約（TASK-91 が依存）に対応する。
///
/// `js-v8`/`js-boa` は現時点で `dep:` 接頭辞なしのプレースホルダ feature
/// （TASK-28.1・#147）であり、有効化しても実際の V8/boa 実装が使えるわけでは
/// ない（実装済みを装わない。REPAIR-3）。feature が無効なバリアントは
/// コンパイル自体から除外されるため（`#[cfg(...)]` 付きの配列要素）、
/// 「同梱されていないのに一覧に載る」幽霊エントリが実行時分岐の書き間違いで
/// 混入する余地がない。
pub fn bundled_engines() -> &'static [EngineKind] {
    BUNDLED
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JS-1: feature 無し（既定ビルド）では同梱エンジンが 0 件であること。
    #[test]
    #[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
    fn js_1_bundled_engines_returns_empty_by_default() {
        assert_eq!(bundled_engines(), &[] as &[EngineKind]);
    }

    /// JS-1: `js-v8` のみ有効な場合、V8 のみを含む一覧を返すこと。
    #[test]
    #[cfg(all(feature = "js-v8", not(feature = "js-boa")))]
    fn js_1_bundled_engines_returns_v8_only() {
        assert_eq!(bundled_engines(), &[EngineKind::V8]);
    }

    /// JS-1: `js-boa` のみ有効な場合、Boa のみを含む一覧を返すこと。
    #[test]
    #[cfg(all(feature = "js-boa", not(feature = "js-v8")))]
    fn js_1_bundled_engines_returns_boa_only() {
        assert_eq!(bundled_engines(), &[EngineKind::Boa]);
    }

    /// JS-1: 両 feature が有効な場合、V8 → Boa の固定順で一覧を返すこと。
    #[test]
    #[cfg(all(feature = "js-v8", feature = "js-boa"))]
    fn js_1_bundled_engines_returns_v8_then_boa() {
        assert_eq!(bundled_engines(), &[EngineKind::V8, EngineKind::Boa]);
    }
}
