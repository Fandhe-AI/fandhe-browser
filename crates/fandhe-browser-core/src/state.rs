//! CDP と AI API が共有する最小の共通状態 [`AppState`]（TASK-41（41.1）・MS-3・
//! ビヘイビア `CDP-1`・`AISNAP-6`）。
//!
//! `fandhe-browser-cli`（TASK-41.5・#174）が `Profile::open` の結果と描画ハンドルから
//! [`AppState`] を 1 つ生成し、`Arc<AppState>` として `fandhe-browser-cdp`
//! （TASK-41.2・#170 の状態型が内包）と `fandhe-browser-ai` のルータへ同じ
//! インスタンスを渡す。共有型を下位の core に置くことで、ai と cdp の間に依存を作らずに
//! 同一ブラウザインスタンスの状態を参照できる（`self-repair-design.md`
//! 「crate 間の依存方向と `AppState` の配置」決定 1・4）。
//!
//! ## 含めないもの
//!
//! - HTTP フレームワーク・WebSocket・非同期ランタイムの型（承認 issue #171・#419 の範囲）
//! - CDP セッション表・ターゲット表（cdp 側の状態型。#170）
//! - プラグインレジストリ（ai 側。`PLUG-2`）
//! - 文字コード判定（`CORE-5`）と HTML の上限サイズ検証。これらは取得層と、`Page.navigate`
//!   等の呼び出し側の責務であり、本モジュールは渡された文字列を解釈せずそのまま保持する
//!
//! ## スレッド安全性と poison 方針
//!
//! 同期には `std::sync::Mutex` と `AtomicU64` だけを使い、全メソッドは同期でロックを
//! メソッド内で解放する（ガードを返さない）。このため await をまたいだロック保持は
//! 起こらない。ロックの書き込みは常に `Option<Arc<NavigationResult>>` 全体の置き換えで
//! あり、途中まで書かれた状態が存在しないため、poison されても内部値をそのまま回復して
//! 使う（ライブラリコードで panic しない。coding-rust.md）。
//!
//! ## 関連ビヘイビア
//!
//! `RENDER-1`（描画ハンドルの保持と既定の [`DisabledRenderer`]）・`PROF-1`
//! （[`Profile`] の共有）。

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fandhe_browser_profile::Profile;

use crate::render::{DisabledRenderer, Renderer};

/// 直近のナビゲート結果（最終 URL と HTML）。
///
/// HTML と URL を 1 つの値にまとめ、[`NavigationState`] のロック内で丸ごと置き換える
/// ことで、別ページの組み合わせが観測されないようにする（`CDP-1`・`AISNAP-6`）。
/// URL には `FetchResponse::final_url()`（userinfo 除去済み）を渡すこと。HTML は呼び出し側が
/// 文字コード判定（`CORE-5`・本モジュールの範囲外）を終えた文字列を渡す。
/// 将来のフィールド追加（ステータスコード等。`REPAIR-4`）に備え `#[non_exhaustive]`
/// とし、フィールドは非公開とする。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NavigationResult {
    url: String,
    html: String,
}

impl fmt::Debug for NavigationResult {
    /// ページ HTML をログへ漏らさないよう、HTML は長さだけを出力する（`AppState` の方針と揃える）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NavigationResult")
            .field("url", &self.url)
            .field("html_len", &self.html.len())
            .finish_non_exhaustive()
    }
}

impl NavigationResult {
    /// 最終 URL と HTML から結果を構築する。
    pub fn new(url: impl Into<String>, html: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            html: html.into(),
        }
    }

    /// 最終 URL を返す。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// HTML を返す。
    pub fn html(&self) -> &str {
        &self.html
    }
}

/// navigate の世代番号（`u64` の newtype。`CDP-1`）。
///
/// [`NavigationState::begin_navigation`] が単調増加で払い出す。後から新しい navigate が
/// 始まった場合に、古い navigate の結果が保存されないようにするために使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NavigationGeneration(u64);

impl NavigationGeneration {
    /// 世代番号の数値を返す。
    pub fn get(&self) -> u64 {
        self.0
    }
}

/// [`NavigationState`] の操作が失敗した理由（`CDP-1`）。
///
/// 本モジュール内に閉じた契約であり、crate 全体の [`crate::error::Error`] には統合しない。
/// 将来の理由追加に備え `#[non_exhaustive]` とする（`REPAIR-4`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StateError {
    /// 後から新しい navigate が始まったため、古い世代の結果を破棄した。状態は変更していない。
    Superseded {
        /// 操作しようとした世代。
        attempted: NavigationGeneration,
        /// 操作時点の最新世代。
        current: NavigationGeneration,
    },
    /// 世代番号が `u64` の上限に達した。wrap による世代の取り違えを避けるため失敗させる。
    GenerationExhausted,
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Superseded { attempted, current } => write!(
                f,
                "navigation superseded by a newer navigation (attempted {}, current {})",
                attempted.get(),
                current.get()
            ),
            Self::GenerationExhausted => f.write_str("navigation generation counter exhausted"),
        }
    }
}

impl std::error::Error for StateError {}

/// OS 非依存のナビゲート状態（世代番号と直近結果）。
///
/// [`AppState`] が内包する。`Profile::open` が使えない OS でも単体で生成・テストできるよう
/// 別型に切り出している。cdp の `Page.navigate` ハンドラが [`Self::begin_navigation`] →
/// 取得 → [`Self::commit_navigation`] の順で呼び、ai は [`Self::latest`] で読む想定。
#[derive(Default)]
pub struct NavigationState {
    latest: Mutex<Option<Arc<NavigationResult>>>,
    generation: AtomicU64,
}

impl fmt::Debug for NavigationState {
    /// 直近結果の HTML を出さないため、世代と結果の有無だけを出力する。
    /// ロック競合・poison 時に待たないよう `try_lock` で判定する。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let has_result = match self.latest.try_lock() {
            Ok(g) => Some(g.is_some()),
            Err(_) => None,
        };
        f.debug_struct("NavigationState")
            .field("generation", &self.generation.load(Ordering::SeqCst))
            .field("has_result", &has_result)
            .finish_non_exhaustive()
    }
}

impl NavigationState {
    /// 世代 0・結果なしの状態を構築する。
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_generation(generation: u64) -> Self {
        Self {
            latest: Mutex::new(None),
            generation: AtomicU64::new(generation),
        }
    }

    /// 世代番号をインクリメントし、新しい世代を返す。
    ///
    /// `u64` を使い切った場合は wrap せず [`StateError::GenerationExhausted`] を返す。
    ///
    /// 世代の更新は結果保存（[`Self::commit_navigation`]）と同じロック内で行う。
    /// ロック無しだと、保存側の世代確認の直後に新しい navigate が始まり、古い結果が
    /// 保存されたうえで `Superseded` も返らない競合が生じる（`CDP-1`）。
    pub fn begin_navigation(&self) -> Result<NavigationGeneration, StateError> {
        let _guard = self.lock();
        self.generation
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_add(1))
            .map(|prev| NavigationGeneration(prev.saturating_add(1)))
            .map_err(|_| StateError::GenerationExhausted)
    }

    /// 現在の（最後に開始された）世代を返す。
    pub fn current_generation(&self) -> NavigationGeneration {
        NavigationGeneration(self.generation.load(Ordering::SeqCst))
    }

    /// `generation` が最新のときに限り、結果を HTML・URL 同時に原子的に保存する。
    ///
    /// 後から新しい navigate が始まっていれば [`StateError::Superseded`] を返し、
    /// 状態は書き換えない。
    pub fn commit_navigation(
        &self,
        generation: NavigationGeneration,
        result: NavigationResult,
    ) -> Result<(), StateError> {
        self.replace_if_current(generation, Some(Arc::new(result)))
    }

    /// `about:blank` 相当として HTML と URL を同時に空にする。世代の判定は
    /// [`Self::commit_navigation`] と同じ。
    pub fn clear_navigation(&self, generation: NavigationGeneration) -> Result<(), StateError> {
        self.replace_if_current(generation, None)
    }

    /// 直近の結果を返す。ロック内では `Arc` の clone だけを行い、HTML をコピーしない。
    pub fn latest(&self) -> Option<Arc<NavigationResult>> {
        self.lock().clone()
    }

    fn replace_if_current(
        &self,
        generation: NavigationGeneration,
        value: Option<Arc<NavigationResult>>,
    ) -> Result<(), StateError> {
        // 世代の判定と置き換えを同じロック内で行い、判定後の割り込みを防ぐ。
        let mut guard = self.lock();
        let current = self.current_generation();
        if generation != current {
            return Err(StateError::Superseded {
                attempted: generation,
                current,
            });
        }
        *guard = value;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Arc<NavigationResult>>> {
        // 書き込みは値全体の置き換えのみで途中状態が無いため、poison は回復して使う。
        self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// CDP ルータと AI API ルータが共有する共通状態（`CDP-1`・`AISNAP-6`）。
///
/// `Clone` は実装しない。共有は常に `Arc<AppState>` で行い、単一インスタンスであることを
/// 型の上で保つ（`AISNAP-6`）。1 つの `AppState` は 1 つの [`Profile`] に対応し、
/// プロファイル間でデータを共有する経路は持たない（`PROF-1`）。
pub struct AppState {
    navigation: NavigationState,
    profile: Arc<Profile>,
    renderer: Arc<dyn Renderer>,
}

impl AppState {
    /// プロファイルと描画ハンドルから構築する。cli が feature `rendering` 有効時に
    /// 具象実装を注入する経路（TASK-41.5・#174。`RENDER-1`）。
    pub fn new(profile: Arc<Profile>, renderer: Arc<dyn Renderer>) -> Self {
        Self {
            navigation: NavigationState::new(),
            profile,
            renderer,
        }
    }

    /// 描画ハンドルに [`DisabledRenderer`]（常に `RenderingDisabled` を返す）を使って構築する。
    /// feature `rendering` 無効時の既定（`RENDER-1`）。
    pub fn with_disabled_renderer(profile: Arc<Profile>) -> Self {
        Self::new(profile, Arc::new(DisabledRenderer::new()))
    }

    /// 共有プロファイルを返す。
    pub fn profile(&self) -> &Arc<Profile> {
        &self.profile
    }

    /// 描画ハンドルを返す。
    pub fn renderer(&self) -> &Arc<dyn Renderer> {
        &self.renderer
    }

    /// ナビゲート状態を返す。
    pub fn navigation(&self) -> &NavigationState {
        &self.navigation
    }
}

impl fmt::Debug for AppState {
    /// `dyn Renderer` は `Debug` ではなく、HTML の中身も出したくないため、
    /// プロファイルのルートと現在の世代だけを出力する。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("profile_root", &self.profile.root())
            .field("generation", &self.navigation.current_generation().get())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn aisnap6_debug_output_hides_html() {
        let r = NavigationResult::new("https://example.com/", "<p>secret-body</p>");
        let d = format!("{r:?}");
        assert!(d.contains("html_len: 18"), "{d}");
        assert!(!d.contains("secret-body"), "{d}");
        let s = NavigationState::new();
        let g = s.begin_navigation().unwrap();
        s.commit_navigation(g, r).unwrap();
        let d = format!("{s:?}");
        assert!(d.contains("has_result: Some(true)"), "{d}");
        assert!(!d.contains("secret-body"), "{d}");
    }

    #[test]
    fn aisnap6_navigation_state_starts_empty() {
        let s = NavigationState::new();
        assert!(s.latest().is_none());
        assert_eq!(s.current_generation().get(), 0);
    }

    #[test]
    fn cdp1_begin_navigation_increments_generation() {
        let s = NavigationState::new();
        let gens: Vec<u64> = (0..3)
            .map(|_| s.begin_navigation().map(|g| g.get()))
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(gens, vec![1, 2, 3]);
    }

    #[test]
    fn cdp1_commit_with_current_generation_stores_result() {
        let s = NavigationState::new();
        let g = s.begin_navigation().unwrap();
        s.commit_navigation(g, NavigationResult::new("https://a.example/", "<p>a</p>"))
            .unwrap();
        let r = s.latest().unwrap();
        assert_eq!(r.url(), "https://a.example/");
        assert_eq!(r.html(), "<p>a</p>");
    }

    #[test]
    fn cdp1_stale_commit_is_superseded_and_not_stored() {
        let s = NavigationState::new();
        let g1 = s.begin_navigation().unwrap();
        let g2 = s.begin_navigation().unwrap();
        let err = s
            .commit_navigation(g1, NavigationResult::new("https://old/", "old"))
            .unwrap_err();
        assert_eq!(
            err,
            StateError::Superseded {
                attempted: g1,
                current: g2
            }
        );
        assert!(s.latest().is_none());
        assert_eq!(
            err.to_string(),
            "navigation superseded by a newer navigation (attempted 1, current 2)"
        );
    }

    #[test]
    fn cdp1_clear_navigation_resets_url_and_html_together() {
        let s = NavigationState::new();
        let g = s.begin_navigation().unwrap();
        s.commit_navigation(g, NavigationResult::new("https://a/", "a"))
            .unwrap();
        let g2 = s.begin_navigation().unwrap();
        assert!(s.clear_navigation(g).is_err());
        assert!(s.latest().is_some());
        s.clear_navigation(g2).unwrap();
        assert!(s.latest().is_none());
    }

    #[test]
    fn cdp1_generation_exhaustion_is_rejected() {
        let s = NavigationState::with_generation(u64::MAX);
        assert_eq!(s.begin_navigation(), Err(StateError::GenerationExhausted));
        assert_eq!(s.current_generation().get(), u64::MAX);
        assert_eq!(
            StateError::GenerationExhausted.to_string(),
            "navigation generation counter exhausted"
        );
    }

    #[test]
    fn aisnap6_latest_returns_shared_arc() {
        let s = NavigationState::new();
        let g = s.begin_navigation().unwrap();
        s.commit_navigation(g, NavigationResult::new("https://a/", "a"))
            .unwrap();
        assert!(Arc::ptr_eq(&s.latest().unwrap(), &s.latest().unwrap()));
    }

    #[test]
    fn cdp1_concurrent_commits_keep_url_and_html_paired() {
        let s = NavigationState::new();
        std::thread::scope(|scope| {
            for t in 0..4 {
                let s = &s;
                scope.spawn(move || {
                    for i in 0..200 {
                        let n = format!("{t}-{i}");
                        if let Ok(g) = s.begin_navigation() {
                            let _ = s.commit_navigation(
                                g,
                                NavigationResult::new(format!("https://x/{n}"), format!("<{n}>")),
                            );
                        }
                        if let Some(r) = s.latest() {
                            assert_eq!(
                                r.url().trim_start_matches("https://x/"),
                                r.html().trim_matches(|c| c == '<' || c == '>')
                            );
                        }
                    }
                });
            }
        });
        assert_eq!(s.current_generation().get(), 800);
    }

    #[test]
    fn cdp1_begin_navigation_waits_for_commit_lock() {
        // 結果保存側がロックを保持している間は世代が進まないこと（確認直後の割り込みの防止）。
        let s = NavigationState::new();
        let g1 = s.begin_navigation().unwrap();
        let guard = s.lock();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let s = &s;
            scope.spawn(move || {
                let g = s.begin_navigation().unwrap();
                tx.send(g.get()).unwrap();
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(200))
                    .is_err()
            );
            assert_eq!(s.current_generation(), g1);
            drop(guard);
            assert_eq!(rx.recv().unwrap(), 2);
        });
        assert_eq!(s.current_generation().get(), 2);
    }

    #[test]
    fn cdp1_stale_result_never_survives_newer_begin() {
        // 保存と begin が競合しても、Ok で保存された結果は必ず最後に払い出された世代のもの。
        for _ in 0..200 {
            let s = NavigationState::new();
            let g1 = s.begin_navigation().unwrap();
            let committed = std::thread::scope(|scope| {
                let h = scope.spawn(|| {
                    s.commit_navigation(g1, NavigationResult::new("https://old/", "old"))
                });
                let g2 = s.begin_navigation().unwrap();
                (h.join().unwrap(), g2)
            });
            let (res, g2) = committed;
            match res {
                // 保存が先に直列化された場合。後続の世代で上書き・破棄できる。
                Ok(()) => assert_eq!(s.latest().unwrap().url(), "https://old/"),
                Err(e) => {
                    assert_eq!(
                        e,
                        StateError::Superseded {
                            attempted: g1,
                            current: g2
                        }
                    );
                    assert!(s.latest().is_none());
                }
            }
        }
    }

    #[test]
    fn state_types_are_send_sync() {
        assert_send_sync::<AppState>();
        assert_send_sync::<NavigationState>();
        assert_send_sync::<NavigationResult>();
        assert_send_sync::<StateError>();
    }
}
