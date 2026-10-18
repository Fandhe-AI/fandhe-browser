//! ページスクリプト実行アクターの構築（TASK-112・MS-6・Issue #784・ビヘイビア `JS-1`・`JS-4`）。
//!
//! `main::run_main` が設定 `[js] execute_page_scripts`（module は `[js] execute_modules` も真のとき。
//! `JS-4`・Issue #1118。ゲートは core が `JsConfig` から適用するため cli に追加の配線はない）から [`PageScriptHandle`] を作り、
//! `server::run` が `AppState`（→ `CdpState::app_state()`）へ渡す。アクター本体は core の
//! `page_actor` で、core は tokio を通常依存に持たないため、専用スレッド上で `!Send` な future を
//! 駆動する実行器（[`TokioPageScriptExecutor`]）だけを cli が供給する。
//!
//! 設定が無効（既定）なら無効ハンドルを返し、スレッドも JS 子プロセスも作らない。有効でも
//! 構築だけでは何も起動せず、最初の実行要求（TASK-112・#785 の cdp `navigate_gated`）で初めて
//! ワーカースレッドが起動する（遅延起動。`PERF-7`）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fandhe_browser_core::config::JsConfig;
use fandhe_browser_core::{PageScriptExecutor, PageScriptHandle};

/// ワーカースレッド上で current_thread の tokio ランタイムを作り、future を完了まで駆動する実行器。
///
/// `Fetcher`（reqwest）が tokio のリアクタを要求するため、ランタイムはワーカーの寿命に
/// 合わせて都度構築する（ワーカーは仕事が尽きると終了する。core の `page_actor` のモジュール doc）。
/// 構築に失敗したら panic せず `Err` を返す（release は `panic = "abort"`）。
#[derive(Debug, Default)]
pub(crate) struct TokioPageScriptExecutor;

impl PageScriptExecutor for TokioPageScriptExecutor {
    fn block_on<'a>(&self, future: Pin<Box<dyn Future<Output = ()> + 'a>>) -> std::io::Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(future);
        Ok(())
    }
}

/// 設定からハンドルを構築する。`execute_page_scripts` が偽（またはエンジンなし）なら無効ハンドル。
pub(crate) fn handle_from_config(js: &JsConfig) -> PageScriptHandle {
    PageScriptHandle::new(js, Arc::new(TokioPageScriptExecutor))
}

/// テスト用: `execute_page_scripts = true` の設定から作ったハンドル。エンジンなしのビルドでは
/// 設定自体が拒否されるため無効ハンドルを返す。
#[cfg(test)]
pub(crate) fn test_enabled_handle() -> PageScriptHandle {
    match fandhe_browser_core::Config::from_toml_str("[js]\nexecute_page_scripts = true\n") {
        Ok(config) => handle_from_config(config.js()),
        Err(_) => PageScriptHandle::disabled(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// `JS-4`: 実行器は渡された future を完了まで駆動する（`!Send` な `Rc` 相当の `Cell` 借用を含む）。
    #[test]
    fn js4_executor_drives_future_to_completion() {
        let done = Cell::new(0u32);
        let result = TokioPageScriptExecutor.block_on(Box::pin(async {
            tokio::task::yield_now().await;
            done.set(42);
        }));
        assert!(result.is_ok());
        assert_eq!(done.get(), 42);
    }

    /// `JS-1`: 既定の設定ではハンドルは無効で、スレッドを起動していない。
    #[test]
    fn js1_default_config_builds_disabled_handle() {
        let handle = handle_from_config(&JsConfig::default());
        assert!(!handle.is_enabled());
        assert_eq!(handle.stats().worker_spawns(), 0);
        assert!(!handle.stats().worker_running());
    }

    /// `JS-1`: 有効な設定でも構築だけではスレッドを起動しない（遅延起動）。エンジンなしのビルドでは
    /// `true` の設定自体が拒否されるので無効ハンドルになる。
    #[test]
    fn js1_enabled_config_builds_handle_without_spawning() {
        let handle = test_enabled_handle();
        assert_eq!(
            handle.is_enabled(),
            !fandhe_browser_core::bundled_engines().is_empty()
        );
        assert_eq!(handle.stats().worker_spawns(), 0);
    }
}
