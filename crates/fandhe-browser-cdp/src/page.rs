//! CDP `Page` ドメインのハンドラ（TASK-42（42.2）・#240、ビヘイビア `CDP-1`・`SEC-2`・MS-4）。
//!
//! `crate::protocol::builtin_handlers` が [`PageNavigate`] を `Page.navigate` として登録し、
//! ws.rs 経由のディスパッチャから呼ばれる。本モジュールは core の
//! [`Fetcher`]（HTTP 取得・SSRF 防御・サイズ／時間上限）で URL を取得し、
//! core の共有状態 [`NavigationState`]（`AppState::navigation()`）の直近結果
//! （最終 URL と HTML）を世代付きで更新する。後続の `DOM.getDocument`（42.4）などが
//! 同じ状態を読む。
//!
//! # スタブ・範囲外（`REPAIR-3`）
//!
//! - イベントは確定した遷移（結果が公開されたもの）に限り `Page.frameNavigated` →
//!   `Page.loadEventFired` を送出する（TASK-42.3・#241）。失敗・中断・追い越された遷移では
//!   送出しない。残るスタブは [`navigation_events`] を参照。
//! - HTML は `FetchResponse::body_text_lossy`（UTF-8 lossy）で文字列化する簡易実装。
//!   文字コード判定は `CORE-5`・TASK-25 で置き換える。
//! - `frameId` は [`MAIN_FRAME_ID`] の固定値。セッション／ターゲットに基づく決定は
//!   TASK-43（`CDP-2`）で行う。
//! - `transitionType`・`referrer`・`frameId` 等の他パラメータは現時点では無視する。
//!
//! # 失敗時の応答方針（`SEC-2`）
//!
//! パラメータ不正は JSON-RPC エラー（`-32602`）。取得失敗は CDP 仕様どおり `result` に
//! `errorText` を付けて返す。これは仕様上の失敗通知であり、Puppeteer / Playwright は
//! 失敗として扱う（一律 success を返して検出を回避する挙動ではない）。`errorText` は
//! [`fetch_error_text`] の閉じた固定文言表のみで、URL・アドレス等の入力由来文字列を含めない。
//!
//! 検証段階で弾いた遷移（不正 URL・許可外 scheme）は状態を変えず、ターゲット表の URL も維持する。
//! 世代を払い出した後の取得失敗は直近結果を無効化するため、ターゲット表の URL も試行した URL
//! へ更新し、文書を持たない共有状態と食い違わせない（`CDP-1`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use fandhe_browser_core::{
    Error, FetchOptions, Fetcher, NavigationGeneration, NavigationResult, NavigationState,
    StateError,
};
use serde_json::{Value, json};

use crate::protocol::{
    BoxFuture, CdpError, CdpEvent, CommandContext, CommandHandler, HandlerOutput,
};
use crate::target::{CdpStateError, MAX_URL_LEN, TargetId, TargetRegistry};

/// メインフレームの `frameId`（スタブ。将来はセッション → ターゲット ID。`CDP-2`・TASK-43）。
pub(crate) const MAIN_FRAME_ID: &str = "main";

/// [`navigate`] の結果。将来の拡張（イベント用情報など）に備えて構造体にする（`REPAIR-4`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavigateOutcome {
    /// 払い出した世代番号から決定的に作る `loaderId`。
    pub loader_id: String,
    /// 取得失敗・中断時の CDP `errorText`（固定文言）。成功時は `None`。
    pub error_text: Option<&'static str>,
    /// 今回の遷移が commit された場合の確定 URL と世代。失敗・中断（`Superseded`）時は `None`。
    /// ターゲット表の更新は `nav.latest()` の再読込ではなく、この値だけを根拠に行う
    /// （並行する別の `Page.navigate` の URL を書き込まないため。`CDP-1`）。
    pub committed: Option<CommittedNavigation>,
    /// 今回の遷移で払い出した世代（検証失敗など世代を払い出す前に返した場合は `None`）。
    /// 取得失敗時に共有状態の古い結果を無効化するかの判定に使う（`CDP-1`）。
    pub began: Option<NavigationGeneration>,
}

/// commit に成功した遷移の確定 URL と、commit 直後の世代。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommittedNavigation {
    pub url: String,
    pub generation: NavigationGeneration,
    /// 確定の完了順を表す通し番号（[`CommitGate`] のロック内で採番）。共有状態へのミラーを
    /// 遷移の完了順に一致させるために使う（`CDP-1`）。
    pub seq: u64,
    /// 今回確定した結果そのもの（`nav.latest()` の再読込で別遷移の結果を拾わないため）。
    /// 確定直後に別の遷移が始まって無効化された場合は `None`。
    pub result: Option<Arc<NavigationResult>>,
}

/// 結果確定の直列化と完了順の採番を行うゲート。確定は同期処理のため `await` を跨がない。
pub(crate) type CommitGate = Mutex<u64>;

fn lock_gate(gate: &CommitGate) -> std::sync::MutexGuard<'_, u64> {
    gate.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 取得エラーを CDP の `errorText`（固定文言）へ写像する閉じた表。
///
/// `Error` は `#[non_exhaustive]` のためワイルドカード腕を持つ。入力由来の文字列は返さない。
pub(crate) fn fetch_error_text(err: &Error) -> &'static str {
    match err {
        Error::Timeout { .. } => "net::ERR_TIMED_OUT",
        Error::TooManyRedirects { .. } => "net::ERR_TOO_MANY_REDIRECTS",
        Error::ResponseTooLarge { .. } => "net::ERR_FILE_TOO_BIG",
        Error::DisallowedScheme { .. } | Error::DisallowedAddress { .. } => {
            "net::ERR_ACCESS_DENIED"
        }
        Error::InvalidInput { .. } => "net::ERR_INVALID_URL",
        _ => "net::ERR_FAILED",
    }
}

/// プロセス内の単調時計（秒）。`Page.loadEventFired` の `timestamp`（CDP `MonotonicTime`）に使う。
/// 基点は初回呼び出し時で、システム時刻の変更に影響されない。
fn monotonic_seconds() -> f64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// 確定した遷移の CDP イベント列（`Page.frameNavigated` → `Page.loadEventFired`）を作る
/// （TASK-42.3・`CDP-1`）。`PageNavigate::handle` が、遷移が確定し結果が公開された場合のみ呼ぶ。
/// `session_id` は `Dispatcher::dispatch` がリクエストの値で補完するため `None` で返す。
///
/// 送出順は `DispatchOutcome` の「レスポンス → イベント列」で確定とする。実 Chromium のように
/// `frameNavigated` を応答より前へ出す並べ替えの要否は TASK-43（`CDP-2`）で判断する。
///
/// # スタブ（`REPAIR-3`）
///
/// - `frame.mimeType` は固定値 `text/html`（`NavigationResult` が Content-Type を保持しないため。
///   実値化は `CORE-5`・TASK-25 以降）。
/// - `securityOrigin` 等の他フィールドは出さない。
/// - `Page.enable` による購読ゲート・`frameStartedLoading`・`domContentEventFired` 等は未実装
///   （TASK-43・`CDP-2`）。
fn navigation_events(loader_id: &str, url: &str, timestamp: f64) -> [CdpEvent; 2] {
    [
        CdpEvent {
            method: "Page.frameNavigated",
            params: json!({
                "frame": {
                    "id": MAIN_FRAME_ID,
                    "loaderId": loader_id,
                    "url": url,
                    "mimeType": "text/html",
                },
                "type": "Navigation",
            }),
            session_id: None,
        },
        CdpEvent {
            method: "Page.loadEventFired",
            params: json!({ "timestamp": timestamp }),
            session_id: None,
        },
    ]
}

fn state_error(_: StateError) -> CdpError {
    CdpError::SERVER_ERROR
}

/// `url` へ遷移し、`nav` の直近結果を更新する。ハンドラ本体（`Page.navigate`）の中核で、
/// `CdpState` に依存しないため 3 OS で単体テストできる。
///
/// 4xx/5xx も完了したナビゲーションとして commit する（core の `Fetcher::get` は判断を
/// 呼び出し側へ委ねる）。取得失敗時は直近結果が `None` のまま（begin で無効化済み）。
#[cfg(test)]
pub(crate) async fn navigate(
    fetcher: &Fetcher,
    nav: &NavigationState,
    url: &str,
) -> Result<NavigateOutcome, CdpError> {
    navigate_gated(fetcher, nav, url, &Mutex::new(0)).await
}

/// [`navigate`] の本体。結果の確定を `gate` のロック内で行い、完了順の通し番号を付ける。
pub(crate) async fn navigate_gated(
    fetcher: &Fetcher,
    nav: &NavigationState,
    url: &str,
    gate: &CommitGate,
) -> Result<NavigateOutcome, CdpError> {
    // 共有状態を変更する前に URL の形式・scheme を検証する。不正 URL や `file:` で
    // 保存済みの URL / HTML を失わせない（`about:blank` は下で個別に扱う）。
    if url != "about:blank"
        && let Err(e) = fetcher.validate_url(url)
    {
        return Ok(NavigateOutcome {
            loader_id: format!("{:x}", nav.current_generation().get()),
            error_text: Some(fetch_error_text(&e)),
            committed: None,
            began: None,
        });
    }
    // 世代の払い出しは `gate` のロック内で行う。呼び出し側（`PageNavigate::handle`）は世代確認から
    // ターゲット表の更新・公開までを同じ `gate` のロック内で行うため、同一ターゲットの後続遷移の
    // 開始と排他になり、確認後に追い越されて URL と HTML が食い違うことがない（`CDP-1`）。
    let generation = {
        let _guard = lock_gate(gate);
        nav.begin_navigation()
    }
    .map_err(state_error)?;
    let mut outcome = NavigateOutcome {
        loader_id: format!("{:x}", generation.get()),
        error_text: None,
        committed: None,
        began: Some(generation),
    };

    if url == "about:blank" {
        // clear_navigation は成功時に自身で世代を 1 つ進める。返す `loaderId` は進めた後の
        // 世代（`current_generation`）に合わせ、次の遷移で番号が飛ばないようにする。
        let mut seq = lock_gate(gate);
        return match nav.clear_navigation(generation) {
            Ok(()) => {
                let advanced = nav.current_generation();
                outcome.loader_id = format!("{:x}", generation.get().saturating_add(1));
                *seq = seq.saturating_add(1);
                outcome.committed = Some(CommittedNavigation {
                    url: "about:blank".to_owned(),
                    generation: advanced,
                    seq: *seq,
                    result: nav.latest(),
                });
                Ok(outcome)
            }
            Err(StateError::Superseded { .. }) => {
                outcome.error_text = Some("net::ERR_ABORTED");
                Ok(outcome)
            }
            Err(e) => Err(state_error(e)),
        };
    }

    match fetcher.get(url).await {
        Ok(resp) => {
            // 共有状態・ターゲット表を更新する前に確定 URL の長さを検証する。超過分を commit
            // したうえでターゲット表だけ古いまま成功を返す不整合を作らない（`CDP-1`・`SEC-2`）。
            if resp.final_url().len() > MAX_URL_LEN {
                outcome.error_text = Some("net::ERR_INVALID_URL");
                return Ok(outcome);
            }
            let final_url = resp.final_url().to_owned();
            let result = NavigationResult::new(&final_url, resp.body_text_lossy());
            let mut seq = lock_gate(gate);
            match nav.commit_navigation(generation, result) {
                Ok(()) => {
                    *seq = seq.saturating_add(1);
                    outcome.committed = Some(CommittedNavigation {
                        url: final_url,
                        generation,
                        seq: *seq,
                        result: nav.latest(),
                    });
                }
                Err(StateError::Superseded { .. }) => {
                    outcome.error_text = Some("net::ERR_ABORTED");
                }
                Err(e) => return Err(state_error(e)),
            }
        }
        Err(e) => outcome.error_text = Some(fetch_error_text(&e)),
    }
    Ok(outcome)
}

/// `Page.navigate` ハンドラ。`Fetcher`（接続プール）を所有し、起動時に 1 回だけ構築する。
pub(crate) struct PageNavigate {
    fetcher: Fetcher,
    /// ターゲットごとの遷移状態（`CDP-1` のターゲット境界）。別ターゲットへの
    /// `Page.navigate` が他ターゲットの HTML・URL を上書きしないよう分離する。
    /// `sessionId` なし（ブラウザレベル）は従来どおり `AppState::navigation()` を直接使う。
    /// TASK-42.4（`DOM.getDocument`）がターゲット別に読めるようになるまでは、成功した遷移を
    /// `AppState::navigation()` へ「直近にナビゲートしたページ」としてミラーする（スタブ。`REPAIR-3`）。
    targets: Mutex<HashMap<TargetId, Arc<NavigationState>>>,
    /// 結果確定の直列化と完了順の採番。
    gate: CommitGate,
    /// 共有状態へ最後にミラーした内容の記録。ターゲット表の更新・ミラーを 1 つの
    /// 同期境界（このロック）で行い、古い遷移が新しい結果を巻き戻さないようにする。
    published: Mutex<Published>,
}

/// 共有状態（`AppState::navigation()`）へ最後にミラーした内容の記録（`CDP-1`）。
///
/// 取得失敗時に共有状態を無効化してよいのは、共有状態が「その失敗したターゲット自身の
/// ミラー」のままである場合に限る。別ターゲットのページを消さないための根拠として使う。
struct Published {
    /// 最後にミラーした確定の通し番号。
    seq: u64,
    /// ミラー元ターゲットと、ミラー直後の共有状態の世代。
    /// ミラーが無い・無効化済み・ブラウザレベル遷移で上書き済みなら `None`。
    mirrored: Option<(TargetId, NavigationGeneration)>,
    /// 進行中のブラウザレベル（`sessionId` なし）遷移の数。0 でない間は、ターゲット遷移の
    /// ミラー（共有状態の世代払い出し）を行わない。ミラーが進行中の遷移の世代を進めて
    /// `Superseded` で中断させないため（`CDP-1`）。
    browser_in_flight: usize,
}

/// ブラウザレベル遷移の進行中を [`Published::browser_in_flight`] へ記録するガード。
/// 完了・エラー・future の破棄のいずれでも減算する。
struct BrowserFlight<'a>(&'a Mutex<Published>);

impl<'a> BrowserFlight<'a> {
    fn enter(published: &'a Mutex<Published>) -> Self {
        let mut p = published.lock().unwrap_or_else(PoisonError::into_inner);
        p.browser_in_flight = p.browser_in_flight.saturating_add(1);
        drop(p);
        Self(published)
    }
}

impl Drop for BrowserFlight<'_> {
    fn drop(&mut self) {
        let mut p = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        p.browser_in_flight = p.browser_in_flight.saturating_sub(1);
    }
}

impl PageNavigate {
    /// `options` で `Fetcher` を構築する。本番は `FetchOptions::default()`
    /// （内部アドレス拒否の安全側既定）、テストのみループバックを許可する。
    pub fn new(options: FetchOptions) -> Result<Self, Error> {
        Ok(Self {
            fetcher: Fetcher::new(options)?,
            targets: Mutex::new(HashMap::new()),
            gate: Mutex::new(0),
            published: Mutex::new(Published {
                seq: 0,
                mirrored: None,
                browser_in_flight: 0,
            }),
        })
    }

    /// `tid` 用の遷移状態を返す（なければ作る）。
    ///
    /// 呼び出しのたびにレジストリから消えた（閉じられた）ターゲットの状態を破棄する。
    /// 閉鎖経路（`Target.closeTarget`）に依存せず、作成・遷移・閉鎖の繰り返しで取得済み HTML が
    /// 無制限に蓄積しないようにする（保持数は `MAX_TARGETS` 以下に収まる）。
    fn target_state(&self, registry: &TargetRegistry, tid: &TargetId) -> Arc<NavigationState> {
        let mut m = self.targets.lock().unwrap_or_else(PoisonError::into_inner);
        m.retain(|id, _| registry.target(id).is_some());
        Arc::clone(
            m.entry(tid.clone())
                .or_insert_with(|| Arc::new(NavigationState::new())),
        )
    }

    /// 閉じられたターゲットの状態を破棄する。
    fn forget_target(&self, tid: &TargetId) {
        let mut m = self.targets.lock().unwrap_or_else(PoisonError::into_inner);
        m.remove(tid);
    }
}

impl CommandHandler for PageNavigate {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            // 状態変更（世代の払い出し）より前に検証する。`sessionId` が付いている場合は
            // レジストリに登録済みであることを要求する（未登録 ID で共有状態を書き換えさせない。
            // `CDP-1`・`SEC-2`）。`sessionId` なし（ブラウザレベル）はセッション／ターゲット対応
            // （TASK-43・`CDP-2`）が入るまで従来どおり許容する（スタブ）。
            let target_id = match ctx.session_id {
                Some(sid) => Some(
                    ctx.state
                        .registry()
                        .session_target(sid)
                        .ok_or(CdpError::INVALID_PARAMS)?,
                ),
                None => None,
            };
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty() && u.len() <= MAX_URL_LEN)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let app_nav = ctx.state.app_state().navigation();
            // 遷移が確定し結果が公開された場合のみ `Some(確定 URL)`。イベント送出の根拠になる。
            let mut navigated: Option<String> = None;
            // ターゲット遷移の送出直前の世代再確認用（遷移状態と確定時の世代）。
            let mut recheck: Option<(Arc<NavigationState>, NavigationGeneration)> = None;
            let out = match &target_id {
                Some(tid) => {
                    let registry = ctx.state.registry();
                    // 失敗時に保存する URL（userinfo 除去後）を状態変更の前に確定・検証する。
                    // 再文字列化（Unicode パスのパーセントエンコード等）で `MAX_URL_LEN` を超える
                    // URL は、世代の払い出し・ドキュメントの無効化の前に固定文言の失敗として返し、
                    // ターゲット表と共有状態を食い違わせない（`CDP-1`・`SEC-2`）。
                    let stored = match self.fetcher.sanitize_url(url) {
                        Ok(u) if u.len() <= MAX_URL_LEN => u,
                        _ => {
                            return Ok(HandlerOutput::result(json!({
                                "frameId": MAIN_FRAME_ID,
                                "loaderId": format!(
                                    "{:x}",
                                    self.target_state(registry, tid).current_generation().get()
                                ),
                                "errorText": "net::ERR_INVALID_URL",
                            })));
                        }
                    };
                    let nav = self.target_state(registry, tid);
                    let out = navigate_gated(&self.fetcher, &nav, url, &self.gate).await?;
                    // ターゲット表の更新と共有状態へのミラーを 1 つのロック内で行う。
                    let mut published = self
                        .published
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if registry.target(tid).is_none() {
                        // 取得中にターゲットが閉じられた。状態を破棄し、内容は公開しない。
                        self.forget_target(tid);
                    } else if let Some(c) = &out.committed {
                        // 後続の遷移に追い越されていない場合に限り、確定 URL を反映する（`CDP-1`）。
                        // 確定直後に別遷移が始まって結果を保持できなかった（`result` が `None`）場合は
                        // 追い越されたものとして扱い、URL もミラーも公開しない。URL だけ公開して
                        // 共有状態との食い違いを作らないため。
                        // 世代確認から URL 更新・ミラーまでは `gate` を保持し、同一ターゲットの後続遷移の
                        // 開始（`navigate_gated` の世代払い出し）と排他にする（`CDP-1`）。
                        let _gate = lock_gate(&self.gate);
                        if nav.current_generation() == c.generation
                            && let Some(r) = &c.result
                        {
                            match registry.set_target_url(tid, &c.url) {
                                Ok(()) => {
                                    navigated = Some(c.url.clone());
                                    recheck = Some((Arc::clone(&nav), c.generation));
                                    // 今回確定した結果だけを、完了順が新しい場合に限りミラーする。
                                    // ブラウザレベル遷移の進行中などでミラーを見送る場合も、確定順
                                    // （`seq`）は記録する。記録しないと、見送った新しい確定（B）より
                                    // 古い確定（A）が後から条件を通り、共有状態に古い HTML が残る。
                                    if c.seq > published.seq {
                                        published.seq = c.seq;
                                        if published.browser_in_flight == 0
                                            && let Ok(g) = app_nav.begin_navigation()
                                            && app_nav
                                                .commit_navigation(
                                                    g,
                                                    NavigationResult::new(r.url(), r.html()),
                                                )
                                                .is_ok()
                                        {
                                            published.mirrored = Some((tid.clone(), g));
                                        }
                                    }
                                }
                                Err(CdpStateError::UnknownTarget) => self.forget_target(tid),
                                Err(_) => return Err(CdpError::SERVER_ERROR),
                            }
                        }
                    } else if let Some(g) = out.began
                        && nav.current_generation() == g
                    {
                        // 上の確認と URL 更新の間に同一ターゲットの後続遷移が開始しないよう、
                        // `gate` を保持したうえで世代を再確認する（`CDP-1`）。
                        let _gate = lock_gate(&self.gate);
                        if nav.current_generation() != g {
                            return Ok(HandlerOutput::result(json!({
                                "frameId": MAIN_FRAME_ID,
                                "loaderId": out.loader_id,
                                "errorText": out.error_text.unwrap_or("net::ERR_FAILED"),
                            })));
                        }
                        // 取得失敗（後続の遷移に追い越されていない）。ターゲットの直近結果は
                        // begin で無効化済みのため、ターゲット表の URL も試行した URL へ揃える
                        // （失敗時の URL 契約: Chromium がエラーページに試行 URL を保つのと同じ。
                        // 検証で弾いた遷移は状態を変えないので URL も変えない。`CDP-1`）。
                        // `url` は長さ検証済みで、scheme・形式は `validate_url` 通過済み。
                        // 保存する URL は userinfo（認証情報）を除去した形にする（成功時の `final_url`
                        // と同じ扱い。`/json/list` の url・title へ漏らさない。`SEC-2`）。
                        // `stored` は状態変更前に検証済み（長さ・形式）。
                        match registry.set_target_url(tid, &stored) {
                            Ok(()) => {}
                            Err(CdpStateError::UnknownTarget) => self.forget_target(tid),
                            Err(_) => return Err(CdpError::SERVER_ERROR),
                        }
                        // 共有状態が当該ターゲット自身のミラーのままなら無効化し、直前に成功した
                        // ページを読めなくする。別ターゲットやブラウザレベル遷移の結果が載って
                        // いる間は消さない（`CDP-1`）。
                        if let Some((mid, mg)) = &published.mirrored
                            && mid == tid
                            && published.browser_in_flight == 0
                            && app_nav.current_generation() == *mg
                        {
                            let _ = app_nav.begin_navigation();
                            published.mirrored = None;
                        }
                    }
                    out
                }
                None => {
                    // 世代払い出しより前に進行中を記録し、ターゲット遷移のミラーと調停する。
                    // 記録は結果の公開（下の `published` 更新）が済むまで保持する。
                    let _flight = BrowserFlight::enter(&self.published);
                    let out = navigate_gated(&self.fetcher, app_nav, url, &self.gate).await?;
                    // ブラウザレベル遷移も確定順（`seq`）の管理に含める。実際に確定し、かつ公開済みより
                    // 新しい場合に限り、共有状態をどのターゲットのミラーでもないものとして記録する。
                    // 失敗・中断・より新しい確定が公開済みの場合は記録を変えない（`CDP-1`）。
                    if let Some(c) = &out.committed {
                        let mut published = self
                            .published
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        // イベント送出の可否は `seq` ではなく共有状態の世代で決める。`seq` は
                        // `app_nav` を置き換えないターゲット遷移のコミットでも進むため、`seq` で
                        // 判定すると並行するセッションスコープの遷移が大きい `seq` を記録しただけで、
                        // 現在の遷移（世代一致・結果保持）のイベントが落ちて待機中のクライアントが
                        // 止まる。追い越された遷移（世代不一致）は `gate` を保持して確認し、送出しない。
                        {
                            let _gate = lock_gate(&self.gate);
                            if app_nav.current_generation() == c.generation && c.result.is_some() {
                                navigated = Some(c.url.clone());
                            }
                        }
                        if c.seq > published.seq {
                            published.seq = c.seq;
                            // 共有状態に古いターゲットのミラーが載ったままなら、今回の確定結果で
                            // 戻す。別の進行中遷移を巻き込まないよう、載っているのがそのミラー
                            // 自身（世代一致）の場合に限る。確定結果を戻せなかった（`result` が
                            // `None`・begin 失敗）場合はミラー記録を残し、そのターゲットの後の
                            // 失敗で共有状態を無効化できるようにする。
                            let on_shared = published
                                .mirrored
                                .as_ref()
                                .is_some_and(|(_, mg)| app_nav.current_generation() == *mg);
                            if !on_shared {
                                published.mirrored = None;
                            } else if let Some(r) = &c.result
                                && let Ok(g) = app_nav.begin_navigation()
                            {
                                published.mirrored = None;
                                let _ = app_nav
                                    .commit_navigation(g, NavigationResult::new(r.url(), r.html()));
                            }
                        }
                    }
                    out
                }
            };
            let mut result = json!({
                "frameId": MAIN_FRAME_ID,
                "loaderId": out.loader_id,
            });
            if let (Some(text), Some(obj)) = (out.error_text, result.as_object_mut()) {
                obj.insert("errorText".into(), json!(text));
            }
            let mut output = HandlerOutput::result(result);
            // 公開処理の後に別の `Page.navigate` が同一ターゲットで確定していないかを、送出の
            // 直前に `gate` を保持して再確認する（`CDP-1`）。追い越された遷移のイベントを出さない。
            // 応答フレームの組み立て後〜ソケット書き込みの区間は、イベントが要求元の応答に同梱される
            // 設計上このハンドラでは直列化できない（セッション単位の配送順は TASK-43・`CDP-2` で扱う）。
            if let Some(url) = navigated
                && recheck.as_ref().is_none_or(|(n, g)| {
                    let _gate = lock_gate(&self.gate);
                    n.current_generation() == *g
                })
            {
                for ev in navigation_events(&out.loader_id, &url, monotonic_seconds()) {
                    output = output.with_event(ev);
                }
            }
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn drain_request_head(stream: &mut TcpStream) {
        let mut buf = [0u8; 1024];
        let mut data = Vec::new();
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 8192 {
                break;
            }
        }
    }

    /// 接続ごとに `status` と `body` を返すループバックサーバー。ポート番号を返す。
    fn serve(status: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                thread::spawn(move || {
                    drain_request_head(&mut s);
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        port
    }

    fn allowed_fetcher() -> Fetcher {
        Fetcher::new(FetchOptions::new().with_allow_private_network_access(true)).unwrap()
    }

    #[tokio::test]
    async fn cdp1_navigate_commits_fetched_html_and_final_url() {
        let port = serve("200 OK", "<h1>hello</h1>");
        let nav = NavigationState::new();
        let url = format!("http://127.0.0.1:{port}/");
        let out = navigate(&allowed_fetcher(), &nav, &url).await.unwrap();
        assert_eq!(out.error_text, None);
        assert_eq!(out.loader_id, "1");
        let latest = nav.latest().unwrap();
        assert_eq!(latest.url(), url);
        assert_eq!(latest.html(), "<h1>hello</h1>");
        assert_eq!(nav.current_generation().get(), 1);
    }

    #[tokio::test]
    async fn cdp1_navigate_twice_replaces_result_and_advances_generation() {
        let p1 = serve("200 OK", "first");
        let p2 = serve("200 OK", "second");
        let nav = NavigationState::new();
        let f = allowed_fetcher();
        navigate(&f, &nav, &format!("http://127.0.0.1:{p1}/"))
            .await
            .unwrap();
        let out = navigate(&f, &nav, &format!("http://127.0.0.1:{p2}/"))
            .await
            .unwrap();
        assert_eq!(out.loader_id, "2");
        assert_eq!(nav.latest().unwrap().html(), "second");
        assert_eq!(nav.current_generation().get(), 2);
    }

    #[tokio::test]
    async fn cdp1_navigate_commits_4xx_page() {
        let port = serve("404 Not Found", "missing");
        let nav = NavigationState::new();
        let out = navigate(
            &allowed_fetcher(),
            &nav,
            &format!("http://127.0.0.1:{port}/"),
        )
        .await
        .unwrap();
        assert_eq!(out.error_text, None);
        assert_eq!(nav.latest().unwrap().html(), "missing");
    }

    #[tokio::test]
    async fn cdp1_navigate_about_blank_clears_without_fetch() {
        let nav = NavigationState::new();
        let out = navigate(&allowed_fetcher(), &nav, "about:blank")
            .await
            .unwrap();
        assert_eq!(out.error_text, None);
        let latest = nav.latest().unwrap();
        assert_eq!(latest.url(), "about:blank");
        assert_eq!(latest.html(), "");
        assert_eq!(out.loader_id, "2");
        assert_eq!(
            out.loader_id,
            format!("{:x}", nav.current_generation().get())
        );
    }

    #[tokio::test]
    async fn cdp1_navigate_fetch_failure_sets_error_text_and_no_result() {
        let port = serve("200 OK", "secret");
        let strict = Fetcher::new(FetchOptions::default()).unwrap();
        let nav = NavigationState::new();
        let out = navigate(&strict, &nav, &format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        assert_eq!(out.error_text, Some("net::ERR_ACCESS_DENIED"));
        assert!(nav.latest().is_none());

        let out = navigate(&strict, &nav, "file:///etc/passwd").await.unwrap();
        assert_eq!(out.error_text, Some("net::ERR_ACCESS_DENIED"));
        assert!(nav.latest().is_none());
    }

    #[tokio::test]
    async fn cdp1_navigate_invalid_url_keeps_previous_result() {
        let port = serve("200 OK", "keep");
        let nav = NavigationState::new();
        let f = allowed_fetcher();
        let url = format!("http://127.0.0.1:{port}/");
        navigate(&f, &nav, &url).await.unwrap();
        let before = nav.current_generation().get();

        let out = navigate(&f, &nav, "file:///etc/passwd").await.unwrap();
        assert_eq!(out.error_text, Some("net::ERR_ACCESS_DENIED"));
        let out = navigate(&f, &nav, "not a url").await.unwrap();
        assert_eq!(out.error_text, Some("net::ERR_INVALID_URL"));

        let latest = nav.latest().unwrap();
        assert_eq!(latest.url(), url);
        assert_eq!(latest.html(), "keep");
        assert_eq!(nav.current_generation().get(), before);
    }

    #[test]
    fn cdp1_fetch_error_text_is_fixed_table() {
        use std::time::Duration;
        let cases: Vec<(Error, &str)> = vec![
            (
                Error::Timeout {
                    limit: Duration::from_secs(1),
                },
                "net::ERR_TIMED_OUT",
            ),
            (
                Error::TooManyRedirects { limit: 3 },
                "net::ERR_TOO_MANY_REDIRECTS",
            ),
            (
                Error::ResponseTooLarge { limit: 1 },
                "net::ERR_FILE_TOO_BIG",
            ),
            (
                Error::DisallowedScheme {
                    scheme: "file".into(),
                },
                "net::ERR_ACCESS_DENIED",
            ),
            (
                Error::DisallowedAddress {
                    address: "127.0.0.1".into(),
                },
                "net::ERR_ACCESS_DENIED",
            ),
            (
                Error::InvalidInput {
                    message: "x".into(),
                },
                "net::ERR_INVALID_URL",
            ),
            (
                Error::Unsupported {
                    message: "x".into(),
                },
                "net::ERR_FAILED",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(fetch_error_text(&err), expected);
        }
    }

    /// `navigation_events` は `frameNavigated` → `loadEventFired` を具体値で返す（TASK-42.3・`CDP-1`）。
    #[test]
    fn cdp1_navigation_events_shape() {
        let ev = navigation_events("1", "http://example.test/", 1.5);
        assert_eq!(ev[0].method, "Page.frameNavigated");
        assert_eq!(
            serde_json::from_str::<Value>(&ev[0].to_json()).unwrap(),
            json!({"method": "Page.frameNavigated", "params": {
                "frame": {"id": "main", "loaderId": "1",
                    "url": "http://example.test/", "mimeType": "text/html"},
                "type": "Navigation"}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&ev[1].to_json()).unwrap(),
            json!({"method": "Page.loadEventFired", "params": {"timestamp": 1.5}})
        );
        assert!(ev[0].session_id.is_none() && ev[1].session_id.is_none());
    }

    #[test]
    fn cdp1_monotonic_seconds_is_non_negative_and_non_decreasing() {
        let a = monotonic_seconds();
        let b = monotonic_seconds();
        assert!(a.is_finite() && a >= 0.0);
        assert!(b >= a);
    }

    // CdpState は AppState（Profile::open）が必要で非 unix では構築手段が無いため unix 限定
    // （protocol.rs の `mod dispatch` と同じ理由）。
    #[cfg(unix)]
    mod dispatch {
        use super::*;
        use crate::protocol::{DispatchOutcome, Dispatcher};
        use crate::server::CdpState;
        use fandhe_browser_core::AppState;
        use fandhe_browser_profile::Profile;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                Self(base.join(format!("fandhe-cdp-page-test-{}-{n}", std::process::id())))
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn state(dir: &TempDir) -> Arc<CdpState> {
            let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
            Arc::new(CdpState::new(Arc::new(AppState::with_disabled_renderer(
                profile,
            ))))
        }

        fn allowed_dispatcher() -> Dispatcher {
            let h = PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                .unwrap();
            Dispatcher::from_handlers(vec![("Page.navigate", Box::new(h))]).unwrap()
        }

        fn frames(o: DispatchOutcome) -> Vec<Value> {
            o.into_frames()
                .iter()
                .map(|f| serde_json::from_str(f).unwrap())
                .collect()
        }

        #[tokio::test]
        async fn cdp1_page_navigate_via_dispatcher_updates_app_state() {
            let dir = TempDir::new();
            let st = state(&dir);
            let port = serve("200 OK", "<p>via cdp</p>");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            // 応答 → frameNavigated → loadEventFired の順（TASK-42.3）。
            assert_eq!(f.len(), 3);
            assert_eq!(
                f[0],
                json!({"id": 1, "result": {"frameId": "main", "loaderId": "1"}})
            );
            assert_eq!(f[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(f[2]["method"], json!("Page.loadEventFired"));
            let latest = st.app_state().navigation().latest().unwrap();
            assert_eq!(latest.url(), url);
            assert_eq!(latest.html(), "<p>via cdp</p>");
        }

        /// 成功遷移は `frameNavigated` → `loadEventFired` を具体値で送出する
        /// （TASK-42.3・`CDP-1`）。
        #[tokio::test]
        async fn cdp1_page_navigate_emits_frame_navigated_then_load_event_fired() {
            let dir = TempDir::new();
            let st = state(&dir);
            let port = serve("200 OK", "<p>ev</p>");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f.len(), 3);
            assert_eq!(f[0]["result"]["loaderId"], json!("1"));
            assert_eq!(
                f[1],
                json!({"method": "Page.frameNavigated", "params": {
                    "frame": {"id": "main", "loaderId": "1", "url": url, "mimeType": "text/html"},
                    "type": "Navigation"}})
            );
            assert_eq!(f[2]["method"], json!("Page.loadEventFired"));
            let ts = f[2]["params"]["timestamp"].as_f64().unwrap();
            assert!(ts.is_finite() && ts >= 0.0);
            assert_eq!(f[2]["params"].as_object().unwrap().len(), 1);
            assert!(f[1].get("sessionId").is_none());
        }

        /// 登録済み `sessionId` 付きの遷移では、全フレームへ同じ `sessionId` が付く。
        #[tokio::test]
        async fn cdp1_page_navigate_events_carry_session_id() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let port = serve("200 OK", "ok");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f.len(), 3);
            assert_eq!(f[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(f[2]["method"], json!("Page.loadEventFired"));
            for fr in &f {
                assert_eq!(fr["sessionId"], json!(sid.as_str()));
            }
        }

        /// `about:blank` も確定遷移としてイベントを送出し、`loaderId` は応答と一致する。
        #[tokio::test]
        async fn cdp1_page_navigate_about_blank_emits_events() {
            let dir = TempDir::new();
            let st = state(&dir);
            let req = json!({"id": 1, "method": "Page.navigate", "params": {"url": "about:blank"}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f.len(), 3);
            assert_eq!(f[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(f[1]["params"]["frame"]["url"], json!("about:blank"));
            assert_eq!(
                f[1]["params"]["frame"]["loaderId"],
                f[0]["result"]["loaderId"]
            );
            assert_eq!(f[2]["method"], json!("Page.loadEventFired"));
        }

        /// 4xx 応答も完了した遷移として commit されるためイベントを送出する。
        #[tokio::test]
        async fn cdp1_page_navigate_http_error_status_still_emits_events() {
            let dir = TempDir::new();
            let st = state(&dir);
            let port = serve("404 Not Found", "nope");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f.len(), 3);
            assert_eq!(f[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(f[2]["method"], json!("Page.loadEventFired"));
        }

        /// 取得失敗・不正 URL・許可外 scheme・未登録 session・`url` 欠落ではイベントを送出しない。
        #[tokio::test]
        async fn cdp1_page_navigate_failures_emit_no_events() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = allowed_dispatcher();
            let cases = [
                json!({"url": dead_url()}),
                json!({"url": "not a url"}),
                json!({"url": "file:///etc/passwd"}),
                json!({}),
            ];
            for (i, params) in cases.iter().enumerate() {
                let req = json!({"id": i, "method": "Page.navigate", "params": params});
                let f = frames(d.dispatch(&st, &req.to_string()).await);
                assert_eq!(f.len(), 1, "params={params}");
            }
            let req = json!({"id": 9, "method": "Page.navigate",
                "sessionId": "NOSUCH", "params": {"url": "about:blank"}});
            let f = frames(d.dispatch(&st, &req.to_string()).await);
            assert_eq!(f.len(), 1);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_unknown_session_is_rejected_without_state_change() {
            let dir = TempDir::new();
            let st = state(&dir);
            let port = serve("200 OK", "x");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": "NOSUCH", "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(
                f[0]["error"],
                json!({"code": -32602, "message": "invalid params"})
            );
            let nav = st.app_state().navigation();
            assert_eq!(nav.current_generation().get(), 0);
            assert!(nav.latest().is_none());
        }

        #[tokio::test]
        async fn cdp1_page_navigate_registered_session_succeeds() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let port = serve("200 OK", "ok");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f[0]["result"]["loaderId"], json!("1"));
            assert_eq!(st.app_state().navigation().latest().unwrap().html(), "ok");
            // 成功後はターゲット表の URL も遷移先へ更新される（`CDP-1`）。
            assert_eq!(st.registry().target(&tid).unwrap().url(), url);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_keeps_per_target_state_and_urls() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let t1 = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let t2 = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let s1 = st.registry().attach(&t1).unwrap();
            let s2 = st.registry().attach(&t2).unwrap();
            let p1 = serve("200 OK", "one");
            let p2 = serve("200 OK", "two");
            let u1 = format!("http://127.0.0.1:{p1}/");
            let u2 = format!("http://127.0.0.1:{p2}/");
            let h = PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                .unwrap();
            let d = Dispatcher::from_handlers(vec![("Page.navigate", Box::new(h))]).unwrap();
            for (id, sid, u) in [(1, &s1, &u1), (2, &s2, &u2)] {
                let req = json!({"id": id, "method": "Page.navigate",
                    "sessionId": sid.as_str(), "params": {"url": u}});
                let f = frames(d.dispatch(&st, &req.to_string()).await);
                assert_eq!(f[0]["result"]["loaderId"], json!("1"));
            }
            assert_eq!(st.registry().target(&t1).unwrap().url(), u1);
            assert_eq!(st.registry().target(&t2).unwrap().url(), u2);
        }

        /// 正規化（パーセントエンコード）後に `MAX_URL_LEN` を超える URL は、状態変更前に
        /// 固定文言の失敗として返り、ターゲット URL・共有状態は変わらない（`CDP-1`・`SEC-2`）。
        #[tokio::test]
        async fn cdp1_page_navigate_url_overlong_after_normalization_fails_without_state_change() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let port = serve("200 OK", "prev");
            let ok_url = format!("http://127.0.0.1:{port}/");
            let ok = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": ok_url}});
            let d = allowed_dispatcher();
            let _ = d.dispatch(&st, &ok.to_string()).await;
            // 生では 4000 バイト（上限内）、エンコード後は 12000 バイトを超える。
            let url = format!("http://127.0.0.1:{port}/{}", "é".repeat(2000));
            assert!(url.len() <= MAX_URL_LEN);
            let req = json!({"id": 2, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": url}});
            let f = frames(d.dispatch(&st, &req.to_string()).await);
            assert_eq!(f[0]["result"]["errorText"], json!("net::ERR_INVALID_URL"));
            assert!(f[0].get("error").is_none());
            assert_eq!(st.registry().target(&tid).unwrap().url(), ok_url);
            assert_eq!(st.app_state().navigation().latest().unwrap().html(), "prev");
        }

        #[tokio::test]
        async fn cdp1_page_navigate_overlong_final_url_fails_and_target_url_is_attempted_url() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let long = format!("/{}", "a".repeat(MAX_URL_LEN));
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            thread::spawn(move || {
                for mut s in listener.incoming().flatten() {
                    let long = long.clone();
                    thread::spawn(move || {
                        let mut buf = [0u8; 1024];
                        let n = s.read(&mut buf).unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let resp = if head.starts_with("GET / ") {
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: {long}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                                .to_owned()
                        };
                        let _ = s.write_all(resp.as_bytes());
                    });
                }
            });
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f[0]["result"]["errorText"], json!("net::ERR_INVALID_URL"));
            // 取得失敗後のターゲット URL は試行した URL（失敗時の URL 契約。`CDP-1`）。
            assert_eq!(st.registry().target(&tid).unwrap().url(), url);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_failure_invalidates_shared_state() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let d = allowed_dispatcher();
            let port = serve("200 OK", "first");
            let ok = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": format!("http://127.0.0.1:{port}/")}});
            d.dispatch(&st, &ok.to_string()).await;
            assert_eq!(
                st.app_state().navigation().latest().unwrap().html(),
                "first"
            );
            // 閉じたポートへの取得は失敗し、直前のページを共有状態から読めなくする。
            let dead = TcpListener::bind("127.0.0.1:0").unwrap();
            let dead_port = dead.local_addr().unwrap().port();
            drop(dead);
            let dead = format!("http://127.0.0.1:{dead_port}/");
            let ng = json!({"id": 2, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": dead}});
            let f = frames(d.dispatch(&st, &ng.to_string()).await);
            assert!(f[0]["result"]["errorText"].is_string());
            assert!(st.app_state().navigation().latest().is_none());
            // ターゲット表の URL も、文書を持たない状態と食い違わないよう試行 URL へ揃う。
            assert_eq!(st.registry().target(&tid).unwrap().url(), dead);
        }

        /// 閉じたポートの URL を返す。
        fn dead_url() -> String {
            let dead = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = dead.local_addr().unwrap().port();
            drop(dead);
            format!("http://127.0.0.1:{p}/")
        }

        /// 取得失敗時にターゲット表へ保存する URL から userinfo が除去される（`CDP-1`・`SEC-2`）。
        #[tokio::test]
        async fn cdp1_page_navigate_failure_strips_userinfo_from_target_url() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let base = dead_url();
            let dead = base.replace("http://", "http://user:secret@");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": dead}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert!(f[0]["result"]["errorText"].is_string());
            let stored = st.registry().target(&tid).unwrap().url().to_owned();
            assert_eq!(stored, base);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_failure_on_other_target_keeps_shared_state() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let ta = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let tb = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sa = st.registry().attach(&ta).unwrap();
            let sb = st.registry().attach(&tb).unwrap();
            let d = allowed_dispatcher();
            let ua = format!("http://127.0.0.1:{}/", serve("200 OK", "a-page"));
            let ub = format!("http://127.0.0.1:{}/", serve("200 OK", "b-page"));
            for (id, sid, u) in [(1, &sa, &ua), (2, &sb, &ub)] {
                let req = json!({"id": id, "method": "Page.navigate",
                    "sessionId": sid.as_str(), "params": {"url": u}});
                d.dispatch(&st, &req.to_string()).await;
            }
            let dead = dead_url();
            let ng = json!({"id": 3, "method": "Page.navigate",
                "sessionId": sa.as_str(), "params": {"url": dead}});
            let f = frames(d.dispatch(&st, &ng.to_string()).await);
            assert!(f[0]["result"]["errorText"].is_string());
            let latest = st.app_state().navigation().latest().unwrap();
            assert_eq!(latest.html(), "b-page");
            assert_eq!(latest.url(), ub);
            assert_eq!(st.registry().target(&tb).unwrap().url(), ub);
            // 失敗したターゲット自身の URL は試行 URL（他ターゲットには影響しない）。
            assert_eq!(st.registry().target(&ta).unwrap().url(), dead);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_target_failure_keeps_browser_level_result() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let ta = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sa = st.registry().attach(&ta).unwrap();
            let d = allowed_dispatcher();
            let ua = format!("http://127.0.0.1:{}/", serve("200 OK", "a-page"));
            let ub = format!("http://127.0.0.1:{}/", serve("200 OK", "browser-level"));
            let ok = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sa.as_str(), "params": {"url": ua}});
            d.dispatch(&st, &ok.to_string()).await;
            let bl = json!({"id": 2, "method": "Page.navigate", "params": {"url": ub}});
            d.dispatch(&st, &bl.to_string()).await;
            let ng = json!({"id": 3, "method": "Page.navigate",
                "sessionId": sa.as_str(), "params": {"url": dead_url()}});
            let f = frames(d.dispatch(&st, &ng.to_string()).await);
            assert!(f[0]["result"]["errorText"].is_string());
            let latest = st.app_state().navigation().latest().unwrap();
            assert_eq!(latest.html(), "browser-level");
            assert_eq!(latest.url(), ub);
        }

        struct SharedHandler(Arc<PageNavigate>);

        impl CommandHandler for SharedHandler {
            fn handle<'a>(
                &'a self,
                ctx: CommandContext<'a>,
                params: &'a Value,
            ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
                self.0.handle(ctx, params)
            }
        }

        /// ブラウザレベル遷移の進行中にミラーを見送った確定も、確定順（`seq`）を記録する
        /// （古い確定が後から共有状態へ公開されないようにする。`CDP-1`）。
        #[tokio::test]
        async fn cdp1_page_navigate_skipped_mirror_still_records_commit_order() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let ta = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let tb = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sa = st.registry().attach(&ta).unwrap();
            let sb = st.registry().attach(&tb).unwrap();
            let h = Arc::new(
                PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                    .unwrap(),
            );
            let d = Dispatcher::from_handlers(vec![(
                "Page.navigate",
                Box::new(SharedHandler(Arc::clone(&h))),
            )])
            .unwrap();
            let flight = BrowserFlight::enter(&h.published);
            for (id, sid) in [(1, &sa), (2, &sb)] {
                let u = format!("http://127.0.0.1:{}/", serve("200 OK", "x"));
                let req = json!({"id": id, "method": "Page.navigate",
                    "sessionId": sid.as_str(), "params": {"url": u}});
                d.dispatch(&st, &req.to_string()).await;
            }
            drop(flight);
            let p = h.published.lock().unwrap();
            assert_eq!(p.seq, 2);
            assert!(p.mirrored.is_none());
            assert!(st.app_state().navigation().latest().is_none());
        }

        /// ターゲットレベルのコミットで `seq` だけが進んでいても、共有状態の世代が一致する
        /// ブラウザレベル遷移はイベントを送出する（待機中のクライアントを止めない。
        /// TASK-42.3・`CDP-1`）。
        #[tokio::test]
        async fn cdp1_page_navigate_browser_level_emits_events_despite_advanced_seq() {
            let dir = TempDir::new();
            let st = state(&dir);
            let h = Arc::new(
                PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                    .unwrap(),
            );
            h.published.lock().unwrap().seq = u64::MAX;
            let d = Dispatcher::from_handlers(vec![(
                "Page.navigate",
                Box::new(SharedHandler(Arc::clone(&h))),
            )])
            .unwrap();
            let u = format!("http://127.0.0.1:{}/", serve("200 OK", "x"));
            let req = json!({"id": 1, "method": "Page.navigate", "params": {"url": u}});
            let f = frames(d.dispatch(&st, &req.to_string()).await);
            assert_eq!(f.len(), 3);
            assert_eq!(f[0]["id"], json!(1));
            assert_eq!(f[0]["result"]["frameId"], json!("main"));
            assert_eq!(f[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(f[2]["method"], json!("Page.loadEventFired"));
        }

        #[tokio::test]
        async fn cdp1_page_navigate_prunes_state_of_closed_targets() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let h = PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                .unwrap();
            let t1 = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let t2 = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let s1 = st.registry().attach(&t1).unwrap();
            let s2 = st.registry().attach(&t2).unwrap();
            let port = serve("200 OK", "x");
            let params = json!({"url": format!("http://127.0.0.1:{port}/")});
            for sid in [&s1, &s2] {
                let ctx = CommandContext {
                    state: &st,
                    session_id: Some(sid),
                };
                h.handle(ctx, &params).await.unwrap();
            }
            assert_eq!(h.targets.lock().unwrap().len(), 2);
            st.registry().close_target(&t1).unwrap();
            // 閉鎖後の次の遷移で、閉じたターゲットの状態が破棄される。
            let ctx = CommandContext {
                state: &st,
                session_id: Some(&s2),
            };
            h.handle(ctx, &params).await.unwrap();
            let m = h.targets.lock().unwrap();
            assert_eq!(m.len(), 1);
            assert!(m.contains_key(&t2));
        }
        #[tokio::test]
        async fn cdp1_page_navigate_failure_keeps_target_url() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": "file:///etc/passwd"}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f[0]["result"]["errorText"], json!("net::ERR_ACCESS_DENIED"));
            assert_eq!(st.registry().target(&tid).unwrap().url(), "about:blank");
        }

        #[tokio::test]
        async fn cdp1_page_navigate_invalid_params_keep_generation() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = allowed_dispatcher();
            let too_long = format!("http://example.invalid/{}", "a".repeat(MAX_URL_LEN));
            let bodies = [
                json!({"id": 1, "method": "Page.navigate"}),
                json!({"id": 2, "method": "Page.navigate", "params": {"url": 5}}),
                json!({"id": 3, "method": "Page.navigate", "params": {"url": ""}}),
                json!({"id": 4, "method": "Page.navigate", "params": {"url": too_long}}),
            ];
            for (i, b) in bodies.iter().enumerate() {
                let f = frames(d.dispatch(&st, &b.to_string()).await);
                assert_eq!(
                    f,
                    vec![
                        json!({"id": i + 1, "error": {"code": -32602, "message": "invalid params"}})
                    ]
                );
            }
            assert_eq!(st.app_state().navigation().current_generation().get(), 0);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_error_frame_does_not_echo_url() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = Dispatcher::builtin().unwrap();
            let req = json!({"id": 1, "method": "Page.navigate",
                "params": {"url": "http://127.0.0.1:9/path?token=dummy-secret"}});
            let raw = d
                .dispatch(&st, &req.to_string())
                .await
                .into_frames()
                .remove(0);
            assert!(!raw.contains("dummy-secret") && !raw.contains("127.0.0.1"));
        }

        #[tokio::test]
        async fn cdp1_builtin_dispatcher_registers_page_navigate() {
            let dir = TempDir::new();
            let st = state(&dir);
            let req = json!({"id": 1, "method": "Page.navigate",
                "params": {"url": "http://127.0.0.1:9/"}});
            let f = frames(
                Dispatcher::builtin()
                    .unwrap()
                    .dispatch(&st, &req.to_string())
                    .await,
            );
            assert_eq!(
                f,
                vec![json!({"id": 1, "result": {
                    "frameId": "main", "loaderId": "0", "errorText": "net::ERR_ACCESS_DENIED"}})]
            );
        }
    }
}
