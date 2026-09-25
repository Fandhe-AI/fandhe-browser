//! 描画機能への境界を定義するモジュール（RENDER-1・TASK-33（サブタスク 33.2）・MS-1）。
//!
//! `fandhe-browser-cdp`・`fandhe-browser-ai` は Servo 実装（`fandhe-browser-render`）へ
//! 直接依存せず、本モジュールが公開する [`Renderer`] トレイト経由でのみスクリーンショット
//! 取得・要素可視性判定・境界ボックス取得へアクセスする
//! （`self-repair-design.md`「crate 間の依存方向と `AppState` の配置」決定 4）。
//!
//! 本モジュールはトレイトと入出力型・エラー型の定義のみを提供する。以下は別 issue の担当であり、
//! 本モジュールには含まれない。
//! - feature `rendering` の Cargo 定義（TASK-33.1）
//! - feature `rendering` 無効時に用いる既定実装（`RenderError::RenderingDisabled` を返す実装。TASK-33.3）
//! - `AppState` へのレンダリングハンドル格納（TASK-41 系）
//! - `fandhe-browser-render`（Servo）側の本実装（TASK-33 本体・TASK-38 系）

/// 描画機能（スクリーンショット・可視性判定・境界ボックス取得）への唯一の境界となるトレイト
/// （RENDER-1）。
///
/// `cdp`・`ai` はこのトレイトのオブジェクト（`dyn Renderer`）を介してのみ描画機能へアクセスする
/// 想定のため、object-safe（`dyn` 互換）である必要がある。`AppState` から複数の呼び出し元
/// （CDP ハンドラ・AI API）へ共有される前提のため `Send + Sync` を要求する。
///
/// 非同期ランタイムへの依存を本 crate に持ち込まないため、メソッドは `async fn` にせず
/// 同期シグネチャとする（実装側で必要であれば内部的にブロッキング呼び出しへ変換する）。
pub trait Renderer: Send + Sync {
    /// 現在の描画内容のスクリーンショットを取得する（RENDER-1）。
    fn capture_screenshot(&self, options: &ScreenshotOptions) -> Result<Screenshot, RenderError>;

    /// 指定した要素の可視性を判定する（RENDER-1）。
    fn element_visibility(&self, element: &ElementRef) -> Result<ElementVisibility, RenderError>;

    /// 指定した要素の境界ボックスを取得する（RENDER-1）。
    fn bounding_box(&self, element: &ElementRef) -> Result<BoundingBox, RenderError>;
}

/// スクリーンショット取得時のオプション（RENDER-1）。
///
/// 将来的にクリップ領域・スケールなどのフィールドを追加できるよう、フィールド追加のみで
/// 済むようにするため `#[non_exhaustive]` を付与する。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScreenshotOptions {
    /// ビューポート外を含むページ全体を取得するかどうか。
    pub full_page: bool,
}

/// 取得したスクリーンショットの画像形式（RENDER-1）。
///
/// 将来的な形式追加に備え `#[non_exhaustive]` を付与する。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// PNG 形式。
    Png,
}

/// スクリーンショット取得結果（RENDER-1）。
///
/// `data` は画像バイト列。本モジュールはトレイト定義のみを提供するため実データ生成は行わず、
/// サイズ上限の検証は実装側（`fandhe-browser-render` 等。TASK-33 本体）の責務とする
/// （coding-rust.md「長さ・件数を上限検証してからアロケーションに使う」）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Screenshot {
    /// 画像形式。
    pub format: ImageFormat,
    /// 画像の幅（ピクセル）。
    pub width: u32,
    /// 画像の高さ（ピクセル）。
    pub height: u32,
    /// 画像バイト列。
    pub data: Vec<u8>,
}

impl Screenshot {
    /// [`Screenshot`] を構築する（RENDER-1）。
    ///
    /// `#[non_exhaustive]` により定義元 crate の外から構造体リテラルで生成できないため、
    /// `fandhe-browser-render`（TASK-33 本体）が `Renderer::capture_screenshot` の成功結果を
    /// 構築するための唯一の経路として提供する。将来のフィールド追加時も本コンストラクタへの
    /// 引数追加のみで済むよう、フィールドを直接公開しない設計を維持する。
    pub fn new(format: ImageFormat, width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            format,
            width,
            height,
            data,
        }
    }
}

/// 描画対象の要素を指す暫定的な参照型（RENDER-1）。
///
/// DOM モジュール（TASK-24・CORE-1）が未実装のため、`NodeId` 等の DOM 由来の識別子は
/// まだ core に存在しない。本 variant はそれまでの暫定的な表現であり、TASK-24 完了後は
/// DOM の識別子型と統合する想定である（REPAIR-3: 実装済みを装わない）。
///
/// `Selector` は CSS セレクタ等、cdp・ai からの外部入力をそのまま保持しうる。本モジュールは
/// 型定義のみを提供し検証・評価ロジックを持たないため、セレクタの検証は将来の実装
/// （render 側・DOM 統合時）の責務となる（OWASP A03: インジェクション対策は呼び出し先で行う）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementRef {
    /// CSS セレクタによる暫定的な要素参照。
    Selector(String),
}

/// 要素の可視性判定結果（RENDER-1）。
///
/// 真偽値のみで済ませず、将来「非表示の理由」等のフィールドを拡張できるよう構造体にする
/// （coding-rust.md「公開 API」・REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElementVisibility {
    /// 要素が可視かどうか。
    pub visible: bool,
}

impl ElementVisibility {
    /// [`ElementVisibility`] を構築する（RENDER-1）。
    ///
    /// `#[non_exhaustive]` のため定義元 crate の外から構造体リテラルで生成できず、
    /// `fandhe-browser-render`（TASK-33 本体）が `Renderer::element_visibility` の成功結果を
    /// 構築するための唯一の経路として提供する。
    pub fn new(visible: bool) -> Self {
        Self { visible }
    }
}

/// 要素の境界ボックス（RENDER-1）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    /// 左上 X 座標。
    pub x: f64,
    /// 左上 Y 座標。
    pub y: f64,
    /// 幅。
    pub width: f64,
    /// 高さ。
    pub height: f64,
}

impl BoundingBox {
    /// [`BoundingBox`] を構築する（RENDER-1）。
    ///
    /// `#[non_exhaustive]` のため定義元 crate の外から構造体リテラルで生成できず、
    /// `fandhe-browser-render`（TASK-33 本体）が `Renderer::bounding_box` の成功結果を
    /// 構築するための唯一の経路として提供する。
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// [`Renderer`] の各メソッドが返すエラー（RENDER-1）。
///
/// 将来の variant 追加に備え `#[non_exhaustive]` を付与する。新規依存（`thiserror` 等）を
/// 追加しない方針（dependency-policy.md）のため手書きで `Display` / `Error` を実装する。
#[non_exhaustive]
#[derive(Debug)]
pub enum RenderError {
    /// feature `rendering` が無効、または描画実装が未接続の状態を表す。
    ///
    /// この variant を返す既定実装は TASK-33.3（issue #47）で追加する。呼び出し失敗時に
    /// 「成功を一律に返す」フォールバックにしないための土台であり、偽装的な検出回避に
    /// ならないよう常に `Err` を返せる設計とする（security.md「偽装・回避機能の禁止」）。
    RenderingDisabled,
    /// 指定した要素が見つからない場合。
    ElementNotFound,
    /// 上記に分類できない内部エラー。
    ///
    /// メッセージには内部実装の詳細（ファイルパス等）を含めすぎないこと
    /// （OWASP A02: 機密情報の露出対策。将来の実装者への申し送り）。
    Internal(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RenderingDisabled => write!(f, "rendering layer is disabled"),
            Self::ElementNotFound => write!(f, "element not found"),
            Self::Internal(message) => write!(f, "internal render error: {message}"),
        }
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Renderer` が object-safe（`dyn` 互換）であることをコンパイル時に検証する（RENDER-1）。
    fn _assert_object_safe(_: &dyn Renderer) {}

    /// `Renderer` トレイトオブジェクトが `Send + Sync` であることをコンパイル時に検証する
    /// （RENDER-1）。`AppState` から複数スレッド間で共有される前提のため。
    fn _assert_send_sync() {
        fn assert<T: Send + Sync>() {}
        assert::<Box<dyn Renderer>>();
    }

    /// テスト専用のモック実装。本番の既定実装（`RenderingDisabled` を返す実装）は
    /// TASK-33.3（issue #47）の担当であり、本モジュールには含めない。
    struct MockRenderer {
        screenshot_ok: bool,
        visible: bool,
        bounding_box_ok: bool,
    }

    impl Renderer for MockRenderer {
        fn capture_screenshot(
            &self,
            _options: &ScreenshotOptions,
        ) -> Result<Screenshot, RenderError> {
            if self.screenshot_ok {
                Ok(Screenshot {
                    format: ImageFormat::Png,
                    width: 800,
                    height: 600,
                    data: vec![0u8; 16],
                })
            } else {
                Err(RenderError::Internal("capture failed".to_string()))
            }
        }

        fn element_visibility(
            &self,
            element: &ElementRef,
        ) -> Result<ElementVisibility, RenderError> {
            match element {
                ElementRef::Selector(selector) if selector.is_empty() => {
                    Err(RenderError::ElementNotFound)
                }
                ElementRef::Selector(_) => Ok(ElementVisibility {
                    visible: self.visible,
                }),
            }
        }

        fn bounding_box(&self, element: &ElementRef) -> Result<BoundingBox, RenderError> {
            match element {
                ElementRef::Selector(selector) if selector.is_empty() => {
                    Err(RenderError::ElementNotFound)
                }
                ElementRef::Selector(_) if self.bounding_box_ok => Ok(BoundingBox {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 50.0,
                }),
                ElementRef::Selector(_) => Err(RenderError::ElementNotFound),
            }
        }
    }

    /// RENDER-1: `capture_screenshot` が成功時に具体的な値を返すことを確認する。
    #[test]
    fn capture_screenshot_returns_ok_with_expected_values() {
        let renderer = MockRenderer {
            screenshot_ok: true,
            visible: true,
            bounding_box_ok: true,
        };
        let screenshot = renderer
            .capture_screenshot(&ScreenshotOptions::default())
            .expect("screenshot should succeed");
        assert_eq!(screenshot.format, ImageFormat::Png);
        assert_eq!(screenshot.width, 800);
        assert_eq!(screenshot.height, 600);
        assert_eq!(screenshot.data.len(), 16);
    }

    /// RENDER-1: `capture_screenshot` が失敗時に `RenderError` を返すことを確認する。
    #[test]
    fn capture_screenshot_returns_err_on_failure() {
        let renderer = MockRenderer {
            screenshot_ok: false,
            visible: true,
            bounding_box_ok: true,
        };
        let err = renderer
            .capture_screenshot(&ScreenshotOptions::default())
            .expect_err("screenshot should fail");
        assert!(matches!(err, RenderError::Internal(ref message) if message == "capture failed"));
    }

    /// RENDER-1: `element_visibility` が成功時に具体的な可視性を返すことを確認する。
    #[test]
    fn element_visibility_returns_ok_with_expected_value() {
        let renderer = MockRenderer {
            screenshot_ok: true,
            visible: true,
            bounding_box_ok: true,
        };
        let visibility = renderer
            .element_visibility(&ElementRef::Selector("#app".to_string()))
            .expect("visibility should succeed");
        assert_eq!(visibility, ElementVisibility { visible: true });
    }

    /// RENDER-1: `element_visibility` が要素未検出時に `ElementNotFound` を返すことを確認する。
    #[test]
    fn element_visibility_returns_element_not_found() {
        let renderer = MockRenderer {
            screenshot_ok: true,
            visible: true,
            bounding_box_ok: true,
        };
        let err = renderer
            .element_visibility(&ElementRef::Selector(String::new()))
            .expect_err("visibility should fail");
        assert!(matches!(err, RenderError::ElementNotFound));
    }

    /// RENDER-1: `bounding_box` が成功時に具体的な座標・サイズを返すことを確認する。
    #[test]
    fn bounding_box_returns_ok_with_expected_values() {
        let renderer = MockRenderer {
            screenshot_ok: true,
            visible: true,
            bounding_box_ok: true,
        };
        let bbox = renderer
            .bounding_box(&ElementRef::Selector("#app".to_string()))
            .expect("bounding box should succeed");
        assert_eq!(bbox.x, 10.0);
        assert_eq!(bbox.y, 20.0);
        assert_eq!(bbox.width, 100.0);
        assert_eq!(bbox.height, 50.0);
    }

    /// RENDER-1: `bounding_box` が要素未検出時に `ElementNotFound` を返すことを確認する。
    #[test]
    fn bounding_box_returns_element_not_found() {
        let renderer = MockRenderer {
            screenshot_ok: true,
            visible: true,
            bounding_box_ok: false,
        };
        let err = renderer
            .bounding_box(&ElementRef::Selector("#app".to_string()))
            .expect_err("bounding box should fail");
        assert!(matches!(err, RenderError::ElementNotFound));
    }

    /// japanese-style.md: プログラム出力文字列（エラーメッセージ）は英語であることの回帰防止。
    #[test]
    fn render_error_display_is_english() {
        assert_eq!(
            RenderError::RenderingDisabled.to_string(),
            "rendering layer is disabled"
        );
        assert_eq!(
            RenderError::ElementNotFound.to_string(),
            "element not found"
        );
        assert_eq!(
            RenderError::Internal("boom".to_string()).to_string(),
            "internal render error: boom"
        );
    }
}
