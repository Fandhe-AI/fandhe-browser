//! core が再エクスポートする `run_js_worker_if_requested`（TASK-30・ビヘイビア `JS-2`・
//! Issue #513）の結合テスト。
//!
//! cli が js へ直接依存せずワーカー入口を呼べること（シグネチャ固定）と、`js-v8` 無効の
//! ビルドでワーカー起動要求（マーカー環境変数）を受けたとき成功を装わず fail-closed になる
//! ことを、自身の再実行（`std::env::current_exe()`）で検証する。`js-v8` 有効ビルドでは
//! 任意のマーカー値で V8 ワーカーに入ってしまうため再実行はせず、V8 経路は
//! `tests/js_stub_v8.rs`（再エクスポート経由）が担う。独自 `main` のため `harness = false`。

use std::process::ExitCode;

/// js crate の `worker_protocol::MARKER_ENV_VAR` と同じ契約値（js 側では `pub(crate)` で
/// 参照できないため、テスト内に契約文字列として持つ）。
#[cfg(not(feature = "js-v8"))]
const MARKER_ENV_VAR: &str = "FANDHE_BROWSER_JS_WORKER";

fn main() -> ExitCode {
    // 子モード: ワーカー起動要求があれば再エクスポート経由で処理して終了する。
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    eprintln!("case: JS-2 reexport signature");
    let _: fn() -> Option<ExitCode> = fandhe_browser_core::run_js_worker_if_requested;

    #[cfg(not(feature = "js-v8"))]
    {
        eprintln!("case: JS-2 worker request without js-v8 fails closed");
        let exe = std::env::current_exe().expect("current exe");
        let out = std::process::Command::new(exe)
            .env(MARKER_ENV_VAR, "1")
            .output()
            .expect("re-run self as worker");
        assert!(!out.status.success(), "must not succeed: {:?}", out.status);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not built with the js-v8 or js-boa feature"),
            "unexpected stderr: {stderr}"
        );
    }

    ExitCode::SUCCESS
}
