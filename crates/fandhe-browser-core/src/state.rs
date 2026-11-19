//! CDP と AI API が共有する最小の共通状態 [`AppState`]（TASK-41（41.1）・MS-3・
//! ビヘイビア `CDP-1`・`AISNAP-6`）。
//!
//! `fandhe-browser-cli`（TASK-41.5・#616 で追加済み。`server` モジュールが生成する。ai ルータへも同じ `Arc` を共有済み。TASK-19.3）が `Profile::open` の結果と描画ハンドルから
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

use crate::interactive::{HostDomSnapshot, InteractiveDocument, InteractiveSync};
use crate::page_actor::PageScriptHandle;
use crate::render::{
    BoundingBox, DisabledRenderer, ElementRef, ElementVisibility, PageRef, RenderError, Renderer,
    Screenshot, ScreenshotOptions,
};
use crate::script_context::ScriptContextHandle;

/// 直近のナビゲート結果（最終 URL と HTML）。
///
/// HTML と URL を 1 つの値にまとめ、[`NavigationState`] のロック内で丸ごと置き換える
/// ことで、別ページの組み合わせが観測されないようにする（`CDP-1`・`AISNAP-6`）。
/// URL には `FetchResponse::final_url()`（userinfo 除去済み）を渡すこと。HTML は呼び出し側が
/// 文字コード判定（`CORE-5`・本モジュールの範囲外）を終えた文字列を渡す。
/// 将来のフィールド追加（ステータスコード等。`REPAIR-4`）に備え `#[non_exhaustive]`
/// とし、フィールドは非公開とする。
#[derive(Clone)]
#[non_exhaustive]
pub struct NavigationResult {
    url: String,
    html: String,
    /// 操作モードの中身（静的モードでは `None`。TASK-124・`AISNAP-20`）。
    interactive: Option<Arc<InteractiveDocument>>,
}

impl PartialEq for NavigationResult {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.html == other.html
            && match (&self.interactive, &other.interactive) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

impl Eq for NavigationResult {}

/// 確定結果のモード（`AISNAP-16`・`AISNAP-20`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PageMode {
    /// 本体が取得し、自前の DOM を正とする（従来の動作）。
    Static,
    /// Servo ホストのページを正とし、その DOM を同期した結果。
    Interactive,
}

impl fmt::Debug for NavigationResult {
    /// URL（クエリ・フラグメントにトークン等の秘密情報を含み得る）と HTML をログへ漏らさないよう、
    /// どちらも長さだけを出力する（`AppState` の方針と揃える。security.md の秘密情報 P0）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NavigationResult")
            .field("url_len", &self.url.len())
            .field("html_len", &self.html.len())
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl NavigationResult {
    /// 最終 URL と HTML から結果を構築する。
    pub fn new(url: impl Into<String>, html: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            html: html.into(),
            interactive: None,
        }
    }

    fn from_sync(sync: InteractiveSync, revision: u64) -> Self {
        Self {
            url: sync.url,
            html: sync.html,
            interactive: Some(Arc::new(sync.document.with_revision(revision))),
        }
    }

    /// モード（静的・操作）。
    pub fn mode(&self) -> PageMode {
        if self.interactive.is_some() {
            PageMode::Interactive
        } else {
            PageMode::Static
        }
    }

    /// 操作モードの中身（ページ識別子・版・対応表・番号を外した文書）。静的モードでは `None`。
    ///
    /// 操作モードの利用側（ai・cdp）は [`Self::html`] を再パースせず、ここで保持した文書を使う
    /// （再パースすると要素の並びが変わりうるため。§14.3）。
    pub fn interactive(&self) -> Option<&Arc<InteractiveDocument>> {
        self.interactive.as_ref()
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

/// 遷移系列（[`NavigationState`] 1 つ）の識別子（TASK-112・`JS-4`）。
///
/// [`NavigationGeneration`] は [`NavigationState`] ごとの連番で、cdp はターゲットごとに別の
/// [`NavigationState`] を持つため、世代の大小だけではターゲットをまたいだ比較ができない。
/// ページスクリプトのアクター（`page_actor`）は「同じ scope で世代が新しい要求」だけを
/// 古い要求の打ち切り対象にし、別ターゲットの実行を巻き込まない。
/// 値はプロセス内で一意に採番する（比較・ハッシュ以外の意味は持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NavigationScope(u64);

/// [`NavigationScope`] の採番元（プロセス内で単調増加）。
static NEXT_NAVIGATION_SCOPE: AtomicU64 = AtomicU64::new(1);

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
    /// [`NavigationState::begin_navigation`] が払い出していない世代（初期値 0）が渡された。
    /// begin 無しでの保存を許さないため、状態は変更しない。
    NotStarted,
    /// 操作モードの確定結果が無いのに差し替えようとした（静的モードのページ・結果なし。TASK-124）。
    NotInteractive,
    /// 差し替えの `session` が保存済みの値と違う（ホストが再起動した。黙って差し替えない。`AISNAP-21`）。
    SessionMismatch,
    /// 操作モードが許されていない（設定キーは TASK-131 まで受理しない。テストの入口だけが許す）。
    InteractiveNotAllowed,
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
            Self::NotStarted => f.write_str("navigation generation was never started"),
            Self::NotInteractive => f.write_str("current navigation result is not interactive"),
            Self::SessionMismatch => f.write_str("interactive session does not match"),
            Self::InteractiveNotAllowed => f.write_str("interactive mode is not allowed"),
        }
    }
}

impl std::error::Error for StateError {}

/// OS 非依存のナビゲート状態（世代番号と直近結果）。
///
/// [`AppState`] が内包する。`Profile::open` が使えない OS でも単体で生成・テストできるよう
/// 別型に切り出している。cdp の `Page.navigate` ハンドラが [`Self::begin_navigation`] →
/// 取得 → [`Self::commit_navigation`] の順で呼び、ai は [`Self::latest`] で読む想定。
pub struct NavigationState {
    latest: Mutex<Option<Arc<NavigationResult>>>,
    generation: AtomicU64,
    /// 操作モードの確定結果を同期の失敗で無効にした世代（0 は無し）。[`Self::interactive_lost`] が使う。
    lost_generation: AtomicU64,
    scope: NavigationScope,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self::with_initial_generation(0)
    }
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

    fn with_initial_generation(generation: u64) -> Self {
        Self {
            latest: Mutex::new(None),
            generation: AtomicU64::new(generation),
            lost_generation: AtomicU64::new(0),
            // wrap は実用上起こらない（u64）。起きても scope が重なるだけで安全側（打ち切り過多）。
            scope: NavigationScope(NEXT_NAVIGATION_SCOPE.fetch_add(1, Ordering::Relaxed)),
        }
    }

    #[cfg(test)]
    fn with_generation(generation: u64) -> Self {
        Self::with_initial_generation(generation)
    }

    /// この遷移系列の識別子を返す（インスタンスの生存中は不変。TASK-112・`JS-4`）。
    pub fn scope(&self) -> NavigationScope {
        self.scope
    }

    /// 世代番号をインクリメントし、新しい世代を返す。
    ///
    /// `u64` を使い切った場合は wrap せず [`StateError::GenerationExhausted`] を返す。
    ///
    /// 開始時に直近結果（前ページの URL と HTML）も無効化するため、commit されるまでの間
    /// [`Self::latest`] は `None` を返す。
    ///
    /// 世代の更新は結果保存（[`Self::commit_navigation`]）と同じロック内で行う。
    /// ロック無しだと、保存側の世代確認の直後に新しい navigate が始まり、古い結果が
    /// 保存されたうえで `Superseded` も返らない競合が生じる（`CDP-1`）。
    pub fn begin_navigation(&self) -> Result<NavigationGeneration, StateError> {
        let mut guard = self.lock();
        let next = self
            .generation
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_add(1))
            .map(|prev| NavigationGeneration(prev.saturating_add(1)))
            .map_err(|_| StateError::GenerationExhausted)?;
        // 前ページの結果を同じロック内で無効化する。残すと、取得中や取得失敗後に
        // `latest()` が古いページを現在の結果として返してしまう（`CDP-1`・`AISNAP-6`）。
        // 世代の払い出しに失敗した場合は状態を変更しない。
        *guard = None;
        Ok(next)
    }

    /// 現在の（最後に開始された）世代を返す。
    pub fn current_generation(&self) -> NavigationGeneration {
        NavigationGeneration(self.generation.load(Ordering::SeqCst))
    }

    /// `generation` が最新のときに限り、結果を HTML・URL 同時に原子的に保存する。
    ///
    /// 後から新しい navigate が始まっていれば [`StateError::Superseded`] を、
    /// 未開始の世代 0 なら [`StateError::NotStarted`] を返し、状態は書き換えない。
    pub fn commit_navigation(
        &self,
        generation: NavigationGeneration,
        result: NavigationResult,
    ) -> Result<(), StateError> {
        self.replace_if_current(generation, Arc::new(result), false)
    }

    /// `about:blank`（URL は `about:blank`・HTML は空）へ遷移済みの結果を同時に保存する。世代の
    /// 判定は [`Self::commit_navigation`] と同じ。
    ///
    /// 成功時は世代も 1 つ進める。同じ世代で進行中の取得が後から
    /// [`Self::commit_navigation`] しても `Superseded` となり、クリア前のページが復活しない
    /// （`CDP-1`・`AISNAP-6`）。世代を使い切っている場合は [`StateError::GenerationExhausted`]
    /// を返し状態を変更しない。
    ///
    /// `None` を保存しないため、[`Self::latest`] の利用者は「`about:blank` へ遷移済み」
    /// （`Some`・`url() == "about:blank"`）と「取得中・取得失敗で結果なし」（`None`）を区別できる
    /// （`CDP-1`・`AISNAP-6`）。
    pub fn clear_navigation(&self, generation: NavigationGeneration) -> Result<(), StateError> {
        self.replace_if_current(
            generation,
            Arc::new(NavigationResult::new("about:blank", "")),
            true,
        )
    }

    /// 直近の結果を返す。ロック内では `Arc` の clone だけを行い、HTML をコピーしない。
    pub fn latest(&self) -> Option<Arc<NavigationResult>> {
        self.lock().clone()
    }

    /// 直近の結果と、それを保存した時点の世代の組を返す（結果が無ければ `None`）。
    ///
    /// [`Self::latest`] と [`Self::current_generation`] を別々に読むと、その間に新しい navigate が
    /// 始まって食い違った組になりうる。ai の `/ai/snapshot` が応答の `generation` と操作モードの
    /// 差し替え（[`AppState::replace_interactive`]）に使う（`AISNAP-20`・TASK-125）。
    pub fn latest_with_generation(&self) -> Option<(NavigationGeneration, Arc<NavigationResult>)> {
        let (generation, latest) = self.snapshot();
        latest.map(|r| (generation, r))
    }

    /// 操作モードの最初の確定結果を保存する（版 1。世代の判定は [`Self::commit_navigation`] と同じ）。
    /// 呼び出し元は [`AppState::commit_interactive`]（操作モードが許されているかを確かめる）。
    fn commit_interactive(
        &self,
        generation: NavigationGeneration,
        sync: InteractiveSync,
    ) -> Result<(), StateError> {
        self.replace_if_current(
            generation,
            Arc::new(NavigationResult::from_sync(sync, 1)),
            false,
        )
    }

    /// 操作モードの確定結果を、同期の結果で差し替える（スクリプトを実行しない専用の入口。TASK-124）。
    ///
    /// 世代が最新で、現在の結果が操作モードで、`session` が同じときだけ差し替える。`doc_seq` と URL は
    /// 変わってよい（ページ自身の遷移）。版は 1 上げ、世代は進めない。パースと保存だけで、ページの
    /// スクリプト・`meta refresh` を動かす経路を通らない（試作 (12)）。
    fn replace_interactive(
        &self,
        generation: NavigationGeneration,
        sync: InteractiveSync,
    ) -> Result<(), StateError> {
        let mut guard = self.lock();
        let current = self.current_generation();
        if generation.get() == 0 {
            return Err(StateError::NotStarted);
        }
        if generation != current {
            return Err(StateError::Superseded {
                attempted: generation,
                current,
            });
        }
        let Some(prev) = guard.as_ref().and_then(|r| r.interactive.clone()) else {
            return Err(StateError::NotInteractive);
        };
        if prev.page().session() != sync.page().session() {
            return Err(StateError::SessionMismatch);
        }
        let revision = prev
            .revision()
            .checked_add(1)
            .ok_or(StateError::GenerationExhausted)?;
        *guard = Some(Arc::new(NavigationResult::from_sync(sync, revision)));
        Ok(())
    }

    /// 操作モードの確定結果を、同期の失敗で無効にする（`AISNAP-20`・`AISNAP-21`・TASK-125）。
    ///
    /// 2026-11-19 のオーナー判断。無効にした後は新しい navigate まで、[`Self::latest`] が `None` を返し
    /// （CDP は文書なし）、[`Self::interactive_lost`] が `true` を返す（ai の `/ai/snapshot` は `session_lost`）。
    /// 世代が最新で現在の結果が操作モードのときだけ無効にする（新しい navigate を壊さない）。呼び出し元は
    /// ai の同期（同期の途中で新しい navigate が始まった場合は呼ばない）。
    pub fn invalidate_interactive(
        &self,
        generation: NavigationGeneration,
    ) -> Result<(), StateError> {
        let mut guard = self.lock();
        let current = self.current_generation();
        if generation != current {
            return Err(StateError::Superseded {
                attempted: generation,
                current,
            });
        }
        if guard
            .as_ref()
            .is_none_or(|r| r.mode() != PageMode::Interactive)
        {
            return Err(StateError::NotInteractive);
        }
        *guard = None;
        self.lost_generation
            .store(generation.get(), Ordering::SeqCst);
        Ok(())
    }

    /// 現在の世代の操作モードの確定結果を [`Self::invalidate_interactive`] で無効にしたか。新しい navigate で
    /// `false` に戻る（世代が進むため）。
    pub fn interactive_lost(&self) -> bool {
        let _guard = self.lock();
        let lost = self.lost_generation.load(Ordering::SeqCst);
        lost != 0 && lost == self.current_generation().get()
    }

    fn replace_if_current(
        &self,
        generation: NavigationGeneration,
        value: Arc<NavigationResult>,
        advance: bool,
    ) -> Result<(), StateError> {
        // 世代の判定と置き換えを同じロック内で行い、判定後の割り込みを防ぐ。
        let mut guard = self.lock();
        let current = self.current_generation();
        if generation.get() == 0 {
            return Err(StateError::NotStarted);
        }
        if generation != current {
            return Err(StateError::Superseded {
                attempted: generation,
                current,
            });
        }
        if advance {
            // ロックを保持しているため、判定から加算までに他の begin は割り込めない。
            self.generation
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_add(1))
                .map_err(|_| StateError::GenerationExhausted)?;
        }
        *guard = Some(value);
        Ok(())
    }

    /// 世代と直近結果の組を同じロック内で取得する（`RENDER-3`・TASK-38）。
    ///
    /// `begin_navigation` / `replace_if_current` は世代と結果を同じロック内で更新するため、
    /// 食い違った組（新しい世代に古い結果）は観測されない。[`RenderGate`] が使う。
    fn snapshot(&self) -> (NavigationGeneration, Option<Arc<NavigationResult>>) {
        let guard = self.lock();
        (self.current_generation(), guard.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Arc<NavigationResult>>> {
        // 書き込みは値全体の置き換えのみで途中状態が無いため、poison は回復して使う。
        self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// ナビゲーション世代を見て [`Renderer::load`] を挟む内部ゲート（`RENDER-1`・`RENDER-3`・
/// TASK-38。`servo-host-design.md` §3.3）。
///
/// [`AppState`] が内包し、描画系メソッドの呼び出しごとに [`NavigationState`] の世代を
/// 比べる。最後に `load` が成功した世代と異なるときだけ `load` を呼ぶ。`Profile` に依存しない
/// 型として切り出し、3 OS で単体テストできるようにしている。
///
/// `load` から描画までを 1 つのロックで直列化する（設計 §2.4「同時に処理中の要求は 1」）。
/// ロック取得順は「gate → navigation（短時間）」に固定する。`NavigationState` 側は gate を
/// 取らず、`Renderer` は `AppState` を参照しないためデッドロックも再入も起きない。
/// 残るリスク: snapshot 後に `begin_navigation` が走ると、その 1 回の描画は直前のページに
/// 対して行われうる。次の呼び出しで世代の違いが検出され再ロードされる。ホストの起動世代
/// （設計 §2.7）の管理は `HostRenderer`（#235）側の責務。
struct RenderGate {
    renderer: Arc<dyn Renderer>,
    /// 最後に `load` が成功した世代。
    loaded: Mutex<Option<NavigationGeneration>>,
}

impl RenderGate {
    fn new(renderer: Arc<dyn Renderer>) -> Self {
        Self {
            renderer,
            loaded: Mutex::new(None),
        }
    }

    /// 必要なら `load` を呼んでから `f` を実行する。
    ///
    /// ページが無い（結果が `None`）ときは [`RenderError::PageNotLoaded`] を返し、前のページの
    /// 描画を返さない（fail-closed）。`load` が失敗したら世代を記録せず、そのエラーを返す
    /// （次の呼び出しで再試行する）。
    fn with_page<T>(
        &self,
        nav: &NavigationState,
        f: impl FnOnce(&dyn Renderer) -> Result<T, RenderError>,
    ) -> Result<T, RenderError> {
        // 書き込みは値全体の置き換えのみで途中状態が無いため、poison は回復して使う。
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        let (generation, latest) = nav.snapshot();
        let Some(latest) = latest else {
            return Err(RenderError::PageNotLoaded);
        };
        // 操作モードのページはホストが保っている。静的な `load` で読み直すと入力・遷移の状態が
        // 失われるため呼ばない（ホストが居なくなっていれば描画実装が `SessionLost` を返す。TASK-124）。
        if latest.mode() == PageMode::Interactive {
            return f(self.renderer.as_ref());
        }
        if *loaded != Some(generation) {
            self.renderer.load(&PageRef::new(latest.url()))?;
            *loaded = Some(generation);
        }
        f(self.renderer.as_ref())
    }
}

/// CDP ルータと AI API ルータが共有する共通状態（`CDP-1`・`AISNAP-6`）。
///
/// `Clone` は実装しない。共有は常に `Arc<AppState>` で行い、単一インスタンスであることを
/// 型の上で保つ（`AISNAP-6`）。1 つの `AppState` は 1 つの [`Profile`] に対応し、
/// プロファイル間でデータを共有する経路は持たない（`PROF-1`）。
pub struct AppState {
    navigation: NavigationState,
    /// 操作モードを許すか（既定 false。設定キー `[rendering] interactive` は TASK-131 まで受理せず、
    /// 今はテストの入口 [`AppState::allow_interactive_for_tests`] だけが立てる。TASK-124）。
    interactive_allowed: bool,
    profile: Arc<Profile>,
    render: RenderGate,
    page_scripts: PageScriptHandle,
    script_contexts: ScriptContextHandle,
}

impl AppState {
    /// プロファイルと描画ハンドルから構築する。cli が feature `rendering` 有効時に
    /// TASK-38 で cli が具象実装を注入する予定の経路（`RENDER-1`。現状 cli は `with_disabled_renderer` を使う）。
    pub fn new(profile: Arc<Profile>, renderer: Arc<dyn Renderer>) -> Self {
        Self {
            navigation: NavigationState::new(),
            interactive_allowed: false,
            profile,
            render: RenderGate::new(renderer),
            // 既定は無効（スレッドも子プロセスも作らない。`JS-1`）。有効化は `with_page_scripts`。
            page_scripts: PageScriptHandle::disabled(),
            // 既定は無効（スレッドも子プロセスも作らない。`PERF-7`）。有効化は `with_script_contexts`。
            script_contexts: ScriptContextHandle::disabled(),
        }
    }

    /// 操作モードを許す（テストからだけ立てられる入口。TASK-124）。
    ///
    /// `cfg(test)` か feature `test-support`（結合テスト専用。ai 等の dev-dependencies だけが有効にする）のときだけ
    /// コンパイルされる。本番の入口は設定キー `[rendering] interactive`（TASK-131）。
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    #[must_use]
    pub fn allow_interactive_for_tests(mut self) -> Self {
        self.interactive_allowed = true;
        self
    }

    /// 操作モードの最初の確定結果を保存する（版 1。`AISNAP-16`・`AISNAP-20`）。操作モードが許されていなければ
    /// [`StateError::InteractiveNotAllowed`]。世代の判定は [`NavigationState::commit_navigation`] と同じ。
    pub fn commit_interactive(
        &self,
        generation: NavigationGeneration,
        sync: InteractiveSync,
    ) -> Result<(), StateError> {
        if !self.interactive_allowed {
            return Err(StateError::InteractiveNotAllowed);
        }
        self.navigation.commit_interactive(generation, sync)
    }

    /// 操作モードの確定結果を同期の結果で差し替える（スクリプトを実行しない専用の入口。`AISNAP-20`）。
    /// 条件は [`NavigationState`] の差し替え（世代・モード・`session`）と同じ。
    pub fn replace_interactive(
        &self,
        generation: NavigationGeneration,
        sync: InteractiveSync,
    ) -> Result<(), StateError> {
        if !self.interactive_allowed {
            return Err(StateError::InteractiveNotAllowed);
        }
        self.navigation.replace_interactive(generation, sync)
    }

    /// ページスクリプトのアクターのハンドルを差し替える（TASK-112・`JS-4`）。cli が設定
    /// `[js] execute_page_scripts` から構築したハンドルを起動時に 1 度だけ渡す。
    #[must_use]
    pub fn with_page_scripts(mut self, handle: PageScriptHandle) -> Self {
        self.page_scripts = handle;
        self
    }

    /// ページスクリプトのアクターのハンドルを返す。cdp の `Page.navigate` 経路
    /// （`CdpState::app_state()` 経由。TASK-112・#785）が使う。
    pub fn page_scripts(&self) -> &PageScriptHandle {
        &self.page_scripts
    }

    /// 確定文書の上で保持する評価コンテキストのハンドルを差し替える（TASK-44.5・`CDP-2`・`JS-1`）。
    /// cli が `[js] engine` から構築したハンドルを起動時に 1 度だけ渡す（配線は cli の `commands::launch::start_server`、#1220）。
    #[must_use]
    pub fn with_script_contexts(mut self, handle: ScriptContextHandle) -> Self {
        self.script_contexts = handle;
        self
    }

    /// 評価コンテキストのハンドルを返す。cdp の Runtime ドメイン（#1217〜#1219）が
    /// `CdpState::app_state()` 経由で使う。
    pub fn script_contexts(&self) -> &ScriptContextHandle {
        &self.script_contexts
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
    ///
    /// 世代の判定（`load` の挿入）を経ない生のハンドル。ページを描画するときは、世代を
    /// 判定する [`Self::capture_screenshot`]・[`Self::element_visibility`]・
    /// [`Self::bounding_box`] を使うこと（`RENDER-3`・TASK-38）。
    pub fn renderer(&self) -> &Arc<dyn Renderer> {
        &self.render.renderer
    }

    /// 現在のページのスクリーンショットを取得する（`RENDER-1`・`RENDER-3`・TASK-38）。
    ///
    /// ナビゲーション世代が前回の `load` 成功時から変わっていれば、先に [`Renderer::load`] を
    /// 呼ぶ。ページが無ければ [`RenderError::PageNotLoaded`]。同期でブロックしうるため、
    /// async の呼び出し元（cdp・ai。結線は別 issue）は `spawn_blocking` 等で呼ぶこと。
    pub fn capture_screenshot(
        &self,
        options: &ScreenshotOptions,
    ) -> Result<Screenshot, RenderError> {
        self.render
            .with_page(&self.navigation, |r| r.capture_screenshot(options))
    }

    /// 現在のページ上の要素の可視性を判定する（`RENDER-1`・`RENDER-3`・TASK-38）。
    /// 世代の扱いとブロッキングの注意は [`Self::capture_screenshot`] と同じ。
    pub fn element_visibility(
        &self,
        element: &ElementRef,
    ) -> Result<ElementVisibility, RenderError> {
        self.render
            .with_page(&self.navigation, |r| r.element_visibility(element))
    }

    /// 現在のページ上の要素の境界ボックスを取得する（`RENDER-1`・`RENDER-3`・TASK-38）。
    /// 世代の扱いとブロッキングの注意は [`Self::capture_screenshot`] と同じ。
    pub fn bounding_box(&self, element: &ElementRef) -> Result<BoundingBox, RenderError> {
        self.render
            .with_page(&self.navigation, |r| r.bounding_box(element))
    }

    /// 操作モードのページの DOM をホストから取得する（`RENDER-11`・`AISNAP-20`・TASK-125）。
    ///
    /// 描画系メソッドと同じゲートを通し、撮影などと直列にする（操作モードのページは読み直さない）。
    /// ページが無ければ [`RenderError::PageNotLoaded`]。結果は untrusted で、呼び出し元が
    /// [`crate::interactive::build_interactive_document`] で検査してから [`Self::replace_interactive`] へ渡す。
    /// ブロッキングの注意は [`Self::capture_screenshot`] と同じ。
    pub fn dom_snapshot(&self) -> Result<HostDomSnapshot, RenderError> {
        self.render
            .with_page(&self.navigation, |r| r.dom_snapshot())
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
            .field("page_scripts_enabled", &self.page_scripts.is_enabled())
            .field(
                "script_contexts_enabled",
                &self.script_contexts.is_enabled(),
            )
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
        assert!(d.contains("url_len: 20"), "{d}");
        assert!(!d.contains("secret-body"), "{d}");
        let t = NavigationResult::new("https://example.com/?token=secret-token", "");
        let d = format!("{t:?}");
        assert!(!d.contains("secret-token"), "{d}");
        assert!(!d.contains("example.com"), "{d}");
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
        assert_eq!(s.latest().unwrap().url(), "https://a/");
        // begin_navigation を挟まず、保存済み結果を clear_navigation 自体が消すことを検証する。
        s.clear_navigation(g).unwrap();
        // about:blank へ遷移済みであることが None（結果なし）と区別できる。
        let blank = s.latest().unwrap();
        assert_eq!(blank.url(), "about:blank");
        assert_eq!(blank.html(), "");
        // clear は世代を進めるため、同じ世代の遅れた commit は復活させられない。
        assert_eq!(s.current_generation().get(), g.get() + 1);
        assert!(matches!(
            s.commit_navigation(g, NavigationResult::new("https://a/", "a")),
            Err(StateError::Superseded { .. })
        ));
        assert_eq!(s.latest().unwrap().url(), "about:blank");
        // 古い世代での clear は拒否され、新しい結果を消さない。
        let g2 = s.begin_navigation().unwrap();
        s.commit_navigation(g2, NavigationResult::new("https://b/", "b"))
            .unwrap();
        let g3 = s.begin_navigation().unwrap();
        assert!(s.clear_navigation(g2).is_err());
        assert_eq!(s.current_generation(), g3);
    }

    #[test]
    fn cdp1_generation_zero_is_rejected_without_begin() {
        let s = NavigationState::new();
        let zero = s.current_generation();
        assert_eq!(
            s.commit_navigation(zero, NavigationResult::new("https://a/", "a")),
            Err(StateError::NotStarted)
        );
        assert_eq!(s.clear_navigation(zero), Err(StateError::NotStarted));
        assert!(s.latest().is_none());
        assert_eq!(s.current_generation().get(), 0);
        assert_eq!(
            StateError::NotStarted.to_string(),
            "navigation generation was never started"
        );
    }

    #[test]
    fn cdp1_clear_at_max_generation_is_rejected_unchanged() {
        let s = NavigationState::with_generation(u64::MAX);
        let g = s.current_generation();
        assert_eq!(s.clear_navigation(g), Err(StateError::GenerationExhausted));
        assert!(s.latest().is_none());
    }

    #[test]
    fn cdp1_begin_navigation_invalidates_previous_result() {
        // 取得中・取得失敗後に前ページが現在結果として見えないこと（コミットされない場合を含む）。
        let s = NavigationState::new();
        let g1 = s.begin_navigation().unwrap();
        s.commit_navigation(g1, NavigationResult::new("https://a/", "a"))
            .unwrap();
        assert_eq!(s.latest().unwrap().url(), "https://a/");
        let g2 = s.begin_navigation().unwrap();
        assert!(s.latest().is_none());
        assert_eq!(s.current_generation(), g2);
    }

    #[test]
    fn cdp1_exhausted_begin_keeps_previous_result() {
        let s = NavigationState::with_generation(u64::MAX - 1);
        let g = s.begin_navigation().unwrap();
        s.commit_navigation(g, NavigationResult::new("https://a/", "a"))
            .unwrap();
        assert_eq!(s.begin_navigation(), Err(StateError::GenerationExhausted));
        assert_eq!(s.latest().unwrap().url(), "https://a/");
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
        // 保存と begin が競合しても、begin 完了後に古い結果は残らない。
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
                // 保存が先に直列化された場合でも、後続の begin_navigation が結果を無効化する。
                Ok(()) => assert!(s.latest().is_none()),
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

    /// `load` と描画の呼び出し回数・渡された URL を記録するテスト用レンダラー。
    struct CountingRenderer {
        loads: std::sync::atomic::AtomicUsize,
        renders: std::sync::atomic::AtomicUsize,
        urls: Mutex<Vec<String>>,
        load_error: Mutex<Option<RenderError>>,
    }

    impl CountingRenderer {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                loads: Default::default(),
                renders: Default::default(),
                urls: Mutex::new(Vec::new()),
                load_error: Mutex::new(None),
            })
        }
        fn loads(&self) -> usize {
            self.loads.load(Ordering::SeqCst)
        }
        fn renders(&self) -> usize {
            self.renders.load(Ordering::SeqCst)
        }
        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
        fn fail_load_with(&self, e: Option<RenderError>) {
            *self.load_error.lock().unwrap() = e;
        }
    }

    impl Renderer for CountingRenderer {
        fn load(&self, page: &PageRef) -> Result<crate::render::PageLoad, RenderError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.urls.lock().unwrap().push(page.url().to_string());
            match self.load_error.lock().unwrap().take() {
                Some(e) => Err(e),
                None => Ok(crate::render::PageLoad::new(page.url())),
            }
        }
        fn capture_screenshot(&self, _: &ScreenshotOptions) -> Result<Screenshot, RenderError> {
            self.renders.fetch_add(1, Ordering::SeqCst);
            Ok(Screenshot::new(
                crate::render::ImageFormat::Png,
                1,
                1,
                vec![0],
            ))
        }
        fn element_visibility(&self, _: &ElementRef) -> Result<ElementVisibility, RenderError> {
            self.renders.fetch_add(1, Ordering::SeqCst);
            Ok(ElementVisibility::new(true))
        }
        fn bounding_box(&self, _: &ElementRef) -> Result<BoundingBox, RenderError> {
            self.renders.fetch_add(1, Ordering::SeqCst);
            Ok(BoundingBox::new(1.0, 2.0, 3.0, 4.0))
        }
    }

    fn navigate(nav: &NavigationState, url: &str) {
        let g = nav.begin_navigation().unwrap();
        nav.commit_navigation(g, NavigationResult::new(url, "<p></p>"))
            .unwrap();
    }

    fn shot(gate: &RenderGate, nav: &NavigationState) -> Result<Screenshot, RenderError> {
        gate.with_page(nav, |r| r.capture_screenshot(&ScreenshotOptions::new()))
    }

    fn sync(session: &str, doc_seq: u64, html: &str) -> crate::interactive::InteractiveSync {
        use crate::interactive::{HostDomSnapshot, InteractivePageId, build_interactive_document};
        build_interactive_document(
            HostDomSnapshot::new(
                InteractivePageId::new(session, doc_seq).unwrap(),
                "https://a.example/",
                html.to_owned(),
                10,
                "s1",
            )
            .unwrap(),
        )
        .unwrap()
    }

    /// TASK-124・AISNAP-20: 操作モードの確定結果は版 1 で始まり、差し替えのたびに版が上がる。
    /// `session` が違う差し替え・静的モードのページ・古い世代は拒否する（黙って差し替えない。AISNAP-21）。
    #[test]
    fn aisnap20_interactive_commit_and_replace() {
        let nav = NavigationState::new();
        let g = nav.begin_navigation().unwrap();
        nav.commit_interactive(g, sync("s", 1, r#"<p data-fandhe-n="0">a</p>"#))
            .unwrap();
        let latest = nav.latest().unwrap();
        assert_eq!(latest.mode(), PageMode::Interactive);
        assert_eq!(latest.interactive().unwrap().revision(), 1);
        assert!(!latest.html().contains("data-fandhe-n"));
        nav.replace_interactive(g, sync("s", 2, r#"<p data-fandhe-n="1">b</p>"#))
            .unwrap();
        let latest = nav.latest().unwrap();
        let doc = latest.interactive().unwrap();
        assert_eq!((doc.revision(), doc.page().doc_seq()), (2, 2));
        assert_eq!(
            nav.replace_interactive(g, sync("other", 3, "<p></p>")),
            Err(StateError::SessionMismatch)
        );
        assert_eq!(nav.latest().unwrap().interactive().unwrap().revision(), 2);
        // 新しい navigate の後は古い世代で差し替えられない。静的モードのページは差し替えない。
        let g2 = nav.begin_navigation().unwrap();
        assert!(matches!(
            nav.replace_interactive(g, sync("s", 4, "<p></p>")),
            Err(StateError::Superseded { .. })
        ));
        nav.commit_navigation(g2, NavigationResult::new("https://b.example/", "<p></p>"))
            .unwrap();
        assert_eq!(nav.latest().unwrap().mode(), PageMode::Static);
        assert_eq!(
            nav.replace_interactive(g2, sync("s", 5, "<p></p>")),
            Err(StateError::NotInteractive)
        );
    }

    /// AISNAP-21・TASK-125: 同期の失敗で無効にすると、新しい navigate まで結果は無く `interactive_lost` が立つ。
    /// 古い世代・静的モードの結果は無効にしない。
    #[test]
    fn aisnap21_invalidate_interactive_until_next_navigation() {
        let nav = NavigationState::new();
        let g = nav.begin_navigation().unwrap();
        assert_eq!(
            nav.invalidate_interactive(g),
            Err(StateError::NotInteractive)
        );
        nav.commit_interactive(g, sync("s", 1, "<p></p>")).unwrap();
        assert!(!nav.interactive_lost());
        nav.invalidate_interactive(g).unwrap();
        assert!(nav.latest().is_none());
        assert!(nav.interactive_lost());
        assert_eq!(
            nav.replace_interactive(g, sync("s", 2, "<p></p>")),
            Err(StateError::NotInteractive)
        );
        let g2 = nav.begin_navigation().unwrap();
        assert!(!nav.interactive_lost());
        assert!(matches!(
            nav.invalidate_interactive(g),
            Err(StateError::Superseded { .. })
        ));
        nav.commit_navigation(g2, NavigationResult::new("https://b.example/", "<p></p>"))
            .unwrap();
        assert_eq!(
            nav.invalidate_interactive(g2),
            Err(StateError::NotInteractive)
        );
        assert!(nav.latest().is_some());
    }

    /// TASK-124: 操作モードは許可（テストの入口。本番は TASK-131 の設定キー）が無ければ保存できない。
    #[test]
    fn aisnap16_app_state_requires_interactive_permission() {
        let dir = fandhe_browser_profile::test_base_dir()
            .join(format!("fandhe-core-interactive-{}", std::process::id()));
        let profile = Arc::new(Profile::open(&dir).expect("profile"));
        let state = AppState::with_disabled_renderer(Arc::clone(&profile));
        let g = state.navigation().begin_navigation().unwrap();
        assert_eq!(
            state.commit_interactive(g, sync("s", 1, "<p></p>")),
            Err(StateError::InteractiveNotAllowed)
        );
        assert_eq!(
            state.replace_interactive(g, sync("s", 1, "<p></p>")),
            Err(StateError::InteractiveNotAllowed)
        );
        drop(state);
        let state = AppState::with_disabled_renderer(profile).allow_interactive_for_tests();
        let g = state.navigation().begin_navigation().unwrap();
        state
            .commit_interactive(g, sync("s", 1, "<p></p>"))
            .unwrap();
        assert_eq!(
            state.navigation().latest().unwrap().mode(),
            PageMode::Interactive
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TASK-124: 操作モードのページは描画ゲートから静的な `load` で読み直さない（ホストの状態を保つ）。
    #[test]
    fn aisnap20_gate_does_not_reload_interactive_page() {
        let r = CountingRenderer::new();
        let gate = RenderGate::new(r.clone());
        let nav = NavigationState::new();
        let g = nav.begin_navigation().unwrap();
        nav.commit_interactive(g, sync("s", 1, "<p></p>")).unwrap();
        shot(&gate, &nav).unwrap();
        shot(&gate, &nav).unwrap();
        assert_eq!((r.loads(), r.renders()), (0, 2));
    }

    #[test]
    fn render3_gate_returns_page_not_loaded_without_navigation() {
        let r = CountingRenderer::new();
        let gate = RenderGate::new(r.clone());
        let nav = NavigationState::new();
        assert!(matches!(shot(&gate, &nav), Err(RenderError::PageNotLoaded)));
        assert_eq!((r.loads(), r.renders()), (0, 0));
    }

    #[test]
    fn render3_gate_loads_once_then_reuses_same_generation() {
        let r = CountingRenderer::new();
        let gate = RenderGate::new(r.clone());
        let nav = NavigationState::new();
        navigate(&nav, "https://a.example/");
        shot(&gate, &nav).unwrap();
        shot(&gate, &nav).unwrap();
        assert_eq!((r.loads(), r.renders()), (1, 2));
        assert_eq!(r.urls(), vec!["https://a.example/".to_string()]);
    }

    #[test]
    fn render3_gate_reloads_after_generation_change() {
        let r = CountingRenderer::new();
        let gate = RenderGate::new(r.clone());
        let nav = NavigationState::new();
        navigate(&nav, "https://a.example/");
        shot(&gate, &nav).unwrap();
        navigate(&nav, "https://b.example/");
        shot(&gate, &nav).unwrap();
        assert_eq!((r.loads(), r.renders()), (2, 2));
        assert_eq!(
            r.urls(),
            vec![
                "https://a.example/".to_string(),
                "https://b.example/".to_string()
            ]
        );
    }

    #[test]
    fn render3_gate_does_not_record_failed_load() {
        let r = CountingRenderer::new();
        let gate = RenderGate::new(r.clone());
        let nav = NavigationState::new();
        navigate(&nav, "https://a.example/");
        r.fail_load_with(Some(RenderError::HostUnavailable));
        assert!(matches!(
            shot(&gate, &nav),
            Err(RenderError::HostUnavailable)
        ));
        assert_eq!((r.loads(), r.renders()), (1, 0));
        shot(&gate, &nav).unwrap();
        assert_eq!((r.loads(), r.renders()), (2, 1));
    }

    #[test]
    fn render3_gate_page_not_loaded_while_navigating() {
        let r = CountingRenderer::new();
        let gate = RenderGate::new(r.clone());
        let nav = NavigationState::new();
        navigate(&nav, "https://a.example/");
        shot(&gate, &nav).unwrap();
        nav.begin_navigation().unwrap();
        assert!(matches!(shot(&gate, &nav), Err(RenderError::PageNotLoaded)));
        assert_eq!((r.loads(), r.renders()), (1, 1));
    }

    #[test]
    fn render3_gate_disabled_renderer_propagates_rendering_disabled() {
        let gate = RenderGate::new(Arc::new(DisabledRenderer::new()));
        let nav = NavigationState::new();
        navigate(&nav, "https://a.example/");
        assert!(matches!(
            shot(&gate, &nav),
            Err(RenderError::RenderingDisabled)
        ));
    }

    #[test]
    fn state_types_are_send_sync() {
        assert_send_sync::<AppState>();
        assert_send_sync::<ScriptContextHandle>();
        assert_send_sync::<NavigationState>();
        assert_send_sync::<NavigationResult>();
        assert_send_sync::<StateError>();
    }
}
