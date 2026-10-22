//! cli の描画実装の選択（TASK-38（38.7）・#236・`RENDER-3`・`PLUG-1`・設計 §2.6・§3.5）。
//!
//! `main::run_main` が起動時に 1 度だけ呼び、core の `AppState::new` へ渡す `Renderer` を決める。
//! ホストバイナリが解決できたときだけ render の `HostRenderer` を返し、未設定なら core の
//! `DisabledRenderer`（常に `RenderingDisabled`）を返す。`HostRenderer` は構築時にホストを起動せず、
//! 初回の描画要求で遅延起動する（起動時の自動起動はしない）。
//!
//! # スタブについて
//!
//! 設定ファイルからのホストパス指定は未配線（REPAIR-3）。設定キーが core の `Config` に未定義のため、
//! 呼び出し側（`main`）は `configured` に `None` を渡し、ホストパスは環境変数
//! `FANDHE_BROWSER_RENDER_HOST`・本体の隣の `fandhe-browser-servo-host` の順で解決される。
//! 設定キーを追加したら `main` がその値をここへ渡す。

use std::path::Path;
use std::sync::Arc;

use fandhe_browser_core::render::{DisabledRenderer, Renderer};
use fandhe_browser_render::host_process::{
    HostBinary, HostProcessConfig, HostProcessError, resolve_host_binary_from_process,
};
use fandhe_browser_render::host_renderer::{HostRenderer, HostRendererConfig};

use crate::server::StartupError;

/// プロセス環境（環境変数・実行ファイルの隣）からホストを解決して描画実装を選ぶ。
///
/// `configured` は設定ファイル由来のホストパス（絶対パスのみ許可。PATH 探索はしない）。
pub(crate) fn renderer_from_process(
    configured: Option<&Path>,
) -> Result<Arc<dyn Renderer>, StartupError> {
    select_renderer(
        resolve_host_binary_from_process(configured),
        HostProcessConfig::new,
    )
}

/// 解決結果から描画実装を選ぶ（環境に触れない純粋な部分。テストから直接呼ぶ）。
///
/// - 解決成功: `HostRenderer`（ホストは起動しない。プライベートネットワーク拒否の既定のまま）
/// - 未設定（`Disabled`）: 無効レンダラー
/// - 不正なパスなどその他の失敗: 起動エラー。下位の候補や無効レンダラーへ黙って落とさない（fail-closed）
///
/// `make_config` はテストが一時ディレクトリや短いタイムアウトを差し込むための注入点で、
/// 本番は `HostProcessConfig::new`（各種既定値のまま。cli では緩めない）。
pub(crate) fn select_renderer(
    resolved: Result<HostBinary, HostProcessError>,
    make_config: impl FnOnce(HostBinary) -> HostProcessConfig,
) -> Result<Arc<dyn Renderer>, StartupError> {
    match resolved {
        Ok(binary) => Ok(Arc::new(HostRenderer::new(HostRendererConfig::new(
            make_config(binary),
        )))),
        Err(HostProcessError::Disabled) => Ok(Arc::new(DisabledRenderer::new())),
        Err(e) => Err(StartupError::RenderHost(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::support::{FixedStore, TempDir};
    use crate::server::{RENDER_HOST_INVALID_MESSAGE, open_app_state_with};
    use fandhe_browser_core::NavigationResult;
    use fandhe_browser_core::PageScriptHandle;
    use fandhe_browser_core::render::{RenderError, ScreenshotOptions};
    use fandhe_browser_render::host_process::{InvalidBinaryReason, resolve_host_binary};
    use std::ffi::OsStr;

    fn commit_page(app: &fandhe_browser_core::AppState) {
        let g = app.navigation().begin_navigation().unwrap();
        app.navigation()
            .commit_navigation(g, NavigationResult::new("https://example.com/", "<p>x</p>"))
            .unwrap();
    }

    /// `RENDER-3`: 未設定ならホストを起動せず、描画は `RenderingDisabled` になる。
    #[test]
    fn render3_unset_host_yields_rendering_disabled() {
        let renderer = select_renderer(Err(HostProcessError::Disabled), HostProcessConfig::new)
            .expect("disabled is not an error");
        let dir = TempDir::new();
        let app = open_app_state_with(
            &FixedStore(dir.path.clone()),
            PageScriptHandle::disabled(),
            fandhe_browser_core::ScriptContextHandle::disabled(),
            renderer,
        )
        .unwrap();
        commit_page(&app);
        let err = app
            .capture_screenshot(&ScreenshotOptions::default())
            .unwrap_err();
        assert!(matches!(err, RenderError::RenderingDisabled), "{err:?}");
    }

    /// `RENDER-3`・`PLUG-1`: 相対パスは設定・環境変数のどちらでも起動エラーで、PATH 探索はしない。
    /// 表示は固定文言で入力値を含まない。
    #[test]
    fn render3_relative_host_path_is_startup_error_without_path() {
        let cases = [
            resolve_host_binary(Some(Path::new("fandhe-browser-servo-host")), None, None),
            resolve_host_binary(None, Some(OsStr::new("relative/host")), None),
        ];
        for resolved in cases {
            let err = select_renderer(resolved, HostProcessConfig::new)
                .err()
                .expect("relative path must be rejected");
            assert!(matches!(
                err,
                StartupError::RenderHost(HostProcessError::InvalidHostBinary(
                    InvalidBinaryReason::NotAbsolute
                ))
            ));
            let shown = err.to_string();
            assert_eq!(shown, RENDER_HOST_INVALID_MESSAGE);
            assert!(!shown.contains("relative"));
        }
    }

    /// `RENDER-3`: ホストパスが与えられると `AppState` 経由の描画呼び出しがホストの起動に届く。
    /// 構築時には起動しない（遅延起動）。偽ホストは hello を返さず終了するため `HostUnavailable`。
    /// unix 限定なのは偽実行ファイル（シェルスクリプト）の仕組みが unix 依存のため
    /// （3 OS での完全な往復は #237・TASK-38.8a）。
    #[cfg(unix)]
    #[test]
    fn render3_host_is_spawned_lazily_and_reached_via_app_state() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let root = tmp.owned.join("host");
        std::fs::create_dir(&root).unwrap();
        let script = root.join("fake-host");
        // HostProcess の cwd は temp_root 配下の専用ディレクトリ。その親に到達印を残す。
        std::fs::write(&script, "#!/bin/sh\n: > ../reached\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let temp_root = tmp.owned.join("work");
        std::fs::create_dir(&temp_root).unwrap();

        let resolved = resolve_host_binary(Some(&script), None, None);
        let renderer = select_renderer(resolved, |b| {
            HostProcessConfig::new(b).with_temp_root(temp_root.clone())
        })
        .expect("host resolves");
        let app = open_app_state_with(
            &FixedStore(tmp.path.clone()),
            PageScriptHandle::disabled(),
            fandhe_browser_core::ScriptContextHandle::disabled(),
            renderer,
        )
        .unwrap();
        commit_page(&app);
        let reached = temp_root.join("reached");
        assert!(
            !reached.exists(),
            "host must not start before a render call"
        );

        let err = app
            .capture_screenshot(&ScreenshotOptions::default())
            .unwrap_err();
        assert!(matches!(err, RenderError::HostUnavailable), "{err:?}");
        assert!(reached.exists(), "host must have been spawned");
    }
}
