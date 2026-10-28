//! Servo ホストの Servo インスタンスと `SoftwareRenderingContext` のライフサイクル管理。
//!
//! 役割: 固定の許可リスト構成で Servo とソフトウェア描画コンテキストを 1 組生成し、
//! `Drop` で確実に破棄する層（RENDER-3・TASK-38.1・#230。設計 §1・§2.3・§2.6）。
//! 呼び出し元は `main` の `--selftest`。将来は stdio サーバー（#993）と WebView を扱う
//! `page`（#231）がこの層の上に載る。
//!
//! 制約と不変条件:
//! - Servo は 1 プロセスにつき 1 回しか初期化できない（`servo-config` の `initialize_options`
//!   が二重初期化で panic する）。プロセス全体のガードで 2 回目以降の生成は
//!   [`EngineError::AlreadyCreated`] にする。破棄後も再生成はできず、作り直すときはホストの
//!   プロセスごと作り直す（設計 §2.7）。
//! - 起動構成は [`fixed_opts`]・[`fixed_preferences`] のコード内固定値だけで作る。
//!   argv・環境変数・ファイルから Servo の設定へ流す経路を持たない（SEC-2）。
//!   ビューポートも定数 [`VIEWPORT`] で、API にしない。
//! - 標準出力へは書かない（将来 stdout はフレーム専用になる。設計 §2.1）。

use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};

use servo::{Opts, Preferences, RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext};

/// 描画ビューポート（幅, 高さ。px）。設計 §2.3 の固定値で、引数や設定にしない（SEC-2）。
pub(crate) const VIEWPORT: (u32, u32) = (1280, 800);

/// プロセス内で `Engine` を生成済みかどうか。破棄してもリセットしない（Servo の制約）。
static ENGINE_CREATED: AtomicBool = AtomicBool::new(false);

/// `Engine` 生成の失敗。メッセージは英語（ログ・連携向け）。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum EngineError {
    /// このプロセスで既に生成済み（Servo は 1 プロセス 1 回のみ）。
    AlreadyCreated,
    /// レンダリングコンテキストの作成・有効化に失敗した。
    RenderingContext(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyCreated => {
                write!(f, "servo engine was already created in this process")
            }
            Self::RenderingContext(detail) => {
                write!(f, "cannot set up software rendering context: {detail}")
            }
        }
    }
}

impl std::error::Error for EngineError {}

/// 固定の Servo `Opts`。引数を取らず、環境変数・argv・ファイルを読まない（SEC-2）。
///
/// `hard_fail: true`（既定）はスレッド失敗時に終了する fail-closed 側なので変えない。
/// `temporary_storage` でホストがディスクへデータを残さないようにする（設計 §2.6）。
/// `Opts::default()` の `debug` は `Default::default()` を使い、環境変数（`SERVO_DIAGNOSTICS`）を
/// 読む `DiagnosticsLogging::new` を経由しない。`HOST_FILE` は Servo 側が読むため、親プロセスの `env_clear`（#994）で防ぐ。
fn fixed_opts() -> Opts {
    Opts {
        temporary_storage: true,
        config_dir: None,
        certificate_path: None,
        host_file: None,
        ignore_certificate_errors: false,
        multiprocess: false,
        sandbox: false,
        time_profiling: None,
        ..Opts::default()
    }
}

/// 固定の Servo `Preferences`。
///
/// `Preferences::default()` は `http_proxy` 等の環境変数から proxy を設定するため、
/// 3 フィールドを空に戻して環境由来の値を持ち込ませない（proxy は識別面・SSRF 経路）。
/// あわせて IntersectionObserver は無効のままにする（既定どおり。理由は本体のコメント。#1291）。
/// ほかの pref は既定のまま。UA 等の識別面には触れない（SEC-2）。
fn fixed_preferences() -> Preferences {
    Preferences {
        network_http_proxy_uri: String::new(),
        network_https_proxy_uri: String::new(),
        network_http_no_proxy: String::new(),
        // servo 0.6.0 では IntersectionObserver を有効にすると、ボックスを持たない祖先で包含
        // ブロックの探索が無限ループし、Script スレッドがハングする（上流 servo/servo#45915・
        // 修正 servo/servo#47693 は 0.7.0 から）。実測で MDN の保存ページが毎回ハングした
        // （#1291）ため、無効に戻した。既定でも false だが、意図を残すために明示する。
        // 代償として、IntersectionObserver を前提にする Next.js 系のページ（react.dev。#1273）
        // は `ReferenceError` で JS が止まり、白紙になる。servo 0.7.0 への更新で再び有効にする
        // （#1297・TASK-38・`RENDER-3`・`RENDER-5`）。UA・ヘッダ・`navigator`・画面・TLS・
        // locale 等の識別面は変えない（SEC-2）。Servo のソースは改変せず設定値だけを変える。
        dom_intersection_observer_enabled: false,
        ..Preferences::default()
    }
}

/// Servo とソフトウェア描画コンテキストの組。`Drop` で Servo を停止してから解放する。
pub(crate) struct Engine {
    // フィールドの宣言順が drop 順になる。Servo の `Drop` はイベントループを回して
    // constellation の終了を待つ間もレンダリングコンテキストを必要とするため、
    // servo を context より先に宣言する。順序を入れ替えないこと。
    servo: Servo,
    context: Rc<SoftwareRenderingContext>,
}

impl Engine {
    /// 固定構成で Servo と描画コンテキストを生成する。引数は取らない（SEC-2）。
    ///
    /// 2 回目以降の呼び出しは [`EngineError::AlreadyCreated`]。コンテキスト作成に
    /// 失敗した場合もガードは戻さない（Servo 側の状態を確定できないため fail-closed）。
    pub(crate) fn new() -> Result<Self, EngineError> {
        if ENGINE_CREATED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(EngineError::AlreadyCreated);
        }
        let context = SoftwareRenderingContext::new(VIEWPORT.into())
            .map_err(|e| EngineError::RenderingContext(format!("{e:?}")))?;
        context
            .make_current()
            .map_err(|e| EngineError::RenderingContext(format!("make_current: {e:?}")))?;
        let servo = ServoBuilder::default()
            .opts(fixed_opts())
            .preferences(fixed_preferences())
            .build();
        Ok(Self {
            servo,
            context: Rc::new(context),
        })
    }

    /// WebView 生成・イベントループ駆動用の Servo 参照（#231 が使う）。
    pub(crate) fn servo(&self) -> &Servo {
        &self.servo
    }

    /// WebView 生成用の描画コンテキスト。複製した `Rc` は WebView の寿命内だけで持つ契約。
    pub(crate) fn rendering_context(&self) -> Rc<dyn RenderingContext> {
        self.context.clone()
    }

    /// 破棄後の解放確認用の弱参照（selftest が `Engine` の drop 後に None を確かめる）。
    pub(crate) fn context_weak(&self) -> Weak<SoftwareRenderingContext> {
        Rc::downgrade(&self.context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SEC-2: 固定構成は proxy を持ち込まず、一時ストレージ・単一プロセスに固定される。
    #[test]
    fn fixed_config_is_closed() {
        let opts = fixed_opts();
        assert!(opts.temporary_storage);
        assert!(!opts.multiprocess);
        assert!(!opts.ignore_certificate_errors);
        assert_eq!(opts.host_file, None);
        let prefs = fixed_preferences();
        assert_eq!(prefs.network_http_proxy_uri, "");
        assert_eq!(prefs.network_https_proxy_uri, "");
        assert_eq!(prefs.network_http_no_proxy, "");
        // #1291・#1297: servo 0.6.0 ではハングするため IntersectionObserver は無効のまま。
        // 0.7.0 への更新で有効に戻すときにこの assert を元の形へ戻す。
        assert!(!prefs.dom_intersection_observer_enabled);
    }

    #[test]
    fn viewport_is_fixed() {
        assert_eq!(VIEWPORT, (1280, 800));
    }

    #[test]
    fn already_created_message_is_english() {
        assert_eq!(
            EngineError::AlreadyCreated.to_string(),
            "servo engine was already created in this process"
        );
    }
}
