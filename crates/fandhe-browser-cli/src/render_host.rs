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
//!
//! `load` の上限（`HostRendererConfig::with_load_timeout_ms`・設計 `servo-host-design.md` §2.4）は
//! `screenshot --timeout`（TASK-47.5・#1301・`CLI-1`）だけが [`renderer_from_process_with_load_timeout`]
//! 経由で渡す。`launch` など他の入口は `None`（ホスト既定の 30 秒）のまま。

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

/// [`renderer_from_process`] に `load` の上限（ミリ秒）を渡す版。`screenshot --timeout` が呼ぶ。
///
/// `load_timeout_ms` は 1〜`MAX_LOAD_TIMEOUT_MS` で、範囲の検証は呼び出し側の引数解析が済ませている
/// （範囲外は `load` 時に render 側が拒否する）。`None` はホスト既定。
pub(crate) fn renderer_from_process_with_load_timeout(
    configured: Option<&Path>,
    load_timeout_ms: Option<u32>,
) -> Result<Arc<dyn Renderer>, StartupError> {
    select_renderer_with(
        resolve_host_binary_from_process(configured),
        HostProcessConfig::new,
        load_timeout_ms,
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
    select_renderer_with(resolved, make_config, None)
}

/// [`select_renderer`] に `load` の上限（ミリ秒。`None` はホスト既定）を足した版。
pub(crate) fn select_renderer_with(
    resolved: Result<HostBinary, HostProcessError>,
    make_config: impl FnOnce(HostBinary) -> HostProcessConfig,
    load_timeout_ms: Option<u32>,
) -> Result<Arc<dyn Renderer>, StartupError> {
    match resolved {
        Ok(binary) => Ok(Arc::new(HostRenderer::new(host_renderer_config(
            make_config(binary),
            load_timeout_ms,
        )))),
        Err(HostProcessError::Disabled) => Ok(Arc::new(DisabledRenderer::new())),
        Err(e) => Err(StartupError::RenderHost(e)),
    }
}

/// 子プロセス設定と `load` の上限から `HostRenderer` の設定を作る（テストが設定値を直接確かめられるよう分離）。
fn host_renderer_config(
    process: HostProcessConfig,
    load_timeout_ms: Option<u32>,
) -> HostRendererConfig {
    HostRendererConfig::new(process).with_load_timeout_ms(load_timeout_ms)
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

    /// `CLI-1`・`RENDER-4`: `load` の上限は `HostRendererConfig` まで届く。未指定は `None`（ホスト既定）。
    /// `HostRendererConfig` に getter が無いため、`Debug` 出力で設定値を確かめる。
    #[test]
    fn cli1_load_timeout_reaches_host_renderer_config() {
        let binary = || {
            resolve_host_binary(Some(&std::env::current_exe().unwrap()), None, None)
                .expect("absolute existing path resolves")
        };
        for (given, shown) in [
            (None, "load_timeout_ms: None"),
            (Some(1_000), "load_timeout_ms: Some(1000)"),
            (Some(30_000), "load_timeout_ms: Some(30000)"),
            (Some(60_000), "load_timeout_ms: Some(60000)"),
        ] {
            let config = host_renderer_config(HostProcessConfig::new(binary()), given);
            let text = format!("{config:?}");
            assert!(text.contains(shown), "{text}");
        }
    }

    /// `CLI-1`: 上限つきの選択でも、未設定は無効レンダラー・不正パスは起動エラーのまま（fail-closed）。
    #[test]
    fn cli1_select_renderer_with_keeps_resolution_semantics() {
        let disabled = select_renderer_with(
            Err(HostProcessError::Disabled),
            HostProcessConfig::new,
            Some(60_000),
        )
        .expect("disabled is not an error");
        let page = fandhe_browser_core::render::PageRef::new("https://a.example/");
        assert!(matches!(
            disabled.load(&page),
            Err(RenderError::RenderingDisabled)
        ));
        let relative = resolve_host_binary(Some(Path::new("relative-host")), None, None);
        assert!(matches!(
            select_renderer_with(relative, HostProcessConfig::new, Some(60_000)),
            Err(StartupError::RenderHost(
                HostProcessError::InvalidHostBinary(InvalidBinaryReason::NotAbsolute)
            ))
        ));
    }
}
