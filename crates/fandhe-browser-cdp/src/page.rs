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
//! - 評価器（core の `ScriptContextHandle`）が有効で、要求セッションが `Runtime.enable` 済みなら、確定時に
//!   `Runtime.executionContextsCleared` を `frameNavigated` の前、`Runtime.executionContextCreated`
//!   （main と `worldName` の isolated world）を後ろに挟む（TASK-44.6・#1217。順序は [`navigation_events`]）。
//!   無効な構成・`Runtime` 無効のセッションでは従来どおり `frameNavigated` → `loadEventFired` のみ。
//! - HTML は `FetchResponse::body_text_lossy`（UTF-8 lossy）で文字列化する簡易実装。
//!   文字コード判定は `CORE-5`・TASK-25 で置き換える。
//! - メインフレームの frame ID は、ターゲットあり（`sessionId` 付き）なら targetId と同じ値
//!   （Chrome と同じ約束事。Playwright の `CRPage._sessionForFrame` が前提とする。TASK-43.3o・#1050・
//!   `CDP-2`）。ターゲットを持たないブラウザレベルの遷移は [`MAIN_FRAME_ID`] を使う。子フレームは持たない。
//! - `transitionType`・`referrer`・`frameId` 等の他パラメータは現時点では無視する。
//!
//! # ページ JS 実行の接続（TASK-112・`JS-4`・`JS-6`）
//!
//! `AppState::page_scripts()`（`[js] execute_page_scripts`）が有効なとき、[`navigate_gated`] は
//! HTML 取得後・確定（commit）前に core のページスクリプト実行アクターを呼び、実行後の HTML を
//! `NavigationResult` として確定する。`DOM.getDocument` などは確定済みの HTML を読むだけなので、
//! 有効時は実行後の DOM を返す。無効（既定）時は従来のコードパスだけを通り、応答・イベント順・
//! DOM は変わらない。
//!
//! - アクターの待機中に `gate`・`published` のロックを保持しない（他ターゲットを塞がない）。
//! - 上限超過・失敗・`Busy`・`Cancelled` でもナビゲーション自体は成功とし、実行前の HTML
//!   （打ち切り時はその時点の DOM）で確定する。診断は [`ScriptStage`] と stderr の固定 1 行のみ。
//! - `Page.loadEventFired` 等のイベントは [`navigate_gated`] の戻り後に組み立てるため、
//!   スクリプト実行（打ち切り含む）の完了後に送出される。追い越された遷移の分は送出しない。
//!
//! スタブ・差異（`REPAIR-3`）: 有効化は設定のみ（`Page.navigate` のオプションは未対応）。
//! ES module も同様で、`[js] execute_page_scripts` と `[js] execute_modules` が共に真のときだけ
//! 実行される（`JS-4`・Issue #1118。ゲートの適用は core の `PageScriptHandle::with_options`）。
//! `Page.frameStoppedLoading` は未送出（`CDP-2`・TASK-43）。`Page.lifecycleEvent` は `Page.enable` と
//! `Page.setLifecycleEventsEnabled` の両方が有効なセッションに `init`・`load` だけ送る（43.3k・#1039。
//! 送らない名前は [`lifecycle_event`] を参照）。
//! `Page.navigate` の応答自体もスクリプト完了後になる（実 Chromium との差異）。JS 段の全体期限は
//! cdp 側には無く、アクターの待ち行列上限とランナーの実時間上限に依存する。
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

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use fandhe_browser_core::{
    ActorFailureKind, Error, FetchOptions, Fetcher, NavigationGeneration, NavigationResult,
    NavigationState, PageRunRequest, PageScriptError, PageScriptHandle, PageScriptOutcome,
    PageScriptSkipReason, ScriptContextError, ScriptContextHandle, StateError,
};
use serde_json::{Value, json};

use crate::protocol::{
    BoxFuture, CdpError, CdpEvent, CommandContext, CommandHandler, HandlerOutput,
};
use crate::runtime::{context_created_event, contexts_cleared_event};
use crate::target::{CdpStateError, MAX_URL_LEN, SessionDomain, TargetId, TargetRegistry};

/// ターゲットを持たないブラウザレベルの `Page.navigate`（`sessionId` なし）が使うフォールバックの frame ID。
/// ターゲットありの場合のメインフレームの frame ID は targetId（[`main_frame_id`]。`CDP-2`・TASK-43.3o・#1050）。
const MAIN_FRAME_ID: &str = "main";

/// メインフレームの frame ID を返す。`Some` なら targetId（`Page.getFrameTree`・`Page.frameNavigated`・
/// `Page.lifecycleEvent`・`Page.navigate` 応答で同じ値）、`None`（ブラウザレベル）なら [`MAIN_FRAME_ID`]。
/// 子フレームのモデルは持たない（`REPAIR-3`）。
pub(crate) fn main_frame_id(target: Option<&TargetId>) -> &str {
    target.map_or(MAIN_FRAME_ID, TargetId::as_str)
}

/// メインフレームの `mimeType`（固定値。`Page.frameNavigated` と `Page.getFrameTree` で共有し食い違いを防ぐ）。
pub(crate) const MAIN_FRAME_MIME_TYPE: &str = "text/html";

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
    /// ページ JS 実行段の結果（どの経路を通ったか。`REPAIR-4`）。
    pub script: ScriptStage,
}

/// ページ JS 実行段の結果。テストと診断が通った経路を具体値で確認できるようにする（`REPAIR-4`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptStage {
    /// 設定無効・`about:blank`・取得失敗・検証で弾いた遷移（アクターを呼んでいない）。
    NotAttempted,
    /// 最終 URL が `validate_url` を通らないため実行しなかった。
    BaseUrlRejected,
    /// アクターが実行しなかった。
    Skipped(PageScriptSkipReason),
    /// 実行した。`abort` は打ち切り理由の安定表記（打ち切りなしなら `None`）。
    Executed { abort: Option<&'static str> },
    /// 実行できなかった。取得した HTML のまま確定する。
    Failed(ScriptFailure),
}

/// [`ScriptStage::Failed`] の理由（入力由来の文字列を持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptFailure {
    Busy,
    Cancelled,
    ActorUnavailable(ActorFailureKind),
    Run,
    Unknown,
}

/// アクターの結果を「確定する HTML」と [`ScriptStage`] へ畳み込む。`Executed` 以外はすべて
/// 取得したままの `original` で確定する（fail-closed。`#[non_exhaustive]` の未知の腕も同様）。
fn settle(
    original: &str,
    result: Result<PageScriptOutcome, PageScriptError>,
) -> (String, ScriptStage) {
    let fallback = |stage| (original.to_owned(), stage);
    match result {
        Ok(PageScriptOutcome::Executed(out)) => {
            let abort = out.abort().map(|a| a.kind().as_str());
            (out.into_html(), ScriptStage::Executed { abort })
        }
        Ok(PageScriptOutcome::Skipped(reason)) => fallback(ScriptStage::Skipped(reason)),
        Err(PageScriptError::Busy { .. }) => fallback(ScriptStage::Failed(ScriptFailure::Busy)),
        Err(PageScriptError::Cancelled) => fallback(ScriptStage::Failed(ScriptFailure::Cancelled)),
        Err(PageScriptError::ActorUnavailable { kind }) => {
            fallback(ScriptStage::Failed(ScriptFailure::ActorUnavailable(kind)))
        }
        Err(PageScriptError::Run(_)) => fallback(ScriptStage::Failed(ScriptFailure::Run)),
        Err(_) => fallback(ScriptStage::Failed(ScriptFailure::Unknown)),
        #[allow(unreachable_patterns)]
        Ok(_) => fallback(ScriptStage::Failed(ScriptFailure::Unknown)),
    }
}

/// 最終 URL に対してページスクリプトを実行してよいか（アクターへ渡す前の再検証。`SEC-2`）。
fn can_run_scripts(fetcher: &Fetcher, final_url: &str) -> bool {
    fetcher.validate_url(final_url).is_ok()
}

/// 通常でない結果だけ stderr へ出す固定 1 行を作る。URL・HTML・例外メッセージは含めない。
fn script_log_line(stage: &ScriptStage, generation: u64) -> Option<String> {
    let (status, abort) = match stage {
        ScriptStage::NotAttempted
        | ScriptStage::Skipped(_)
        | ScriptStage::Failed(ScriptFailure::Cancelled) => return None,
        ScriptStage::Executed { abort: None } => return None,
        ScriptStage::Executed { abort: Some(k) } => ("aborted", Some(*k)),
        ScriptStage::BaseUrlRejected => ("base_url_rejected", None),
        ScriptStage::Failed(ScriptFailure::Busy) => ("busy", None),
        ScriptStage::Failed(ScriptFailure::ActorUnavailable(_)) => ("actor_unavailable", None),
        ScriptStage::Failed(ScriptFailure::Run) => ("run_failed", None),
        ScriptStage::Failed(ScriptFailure::Unknown) => ("unknown", None),
    };
    Some(match abort {
        Some(k) => format!("cdp: page scripts: status={status} generation={generation} abort={k}"),
        None => format!("cdp: page scripts: status={status} generation={generation}"),
    })
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

pub(crate) fn lock_gate(gate: &CommitGate) -> std::sync::MutexGuard<'_, u64> {
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

/// 送る `Page.lifecycleEvent` の名前（`init`）。commit の時点でサーバーが確定できる。
const LIFECYCLE_INIT: &str = "init";
/// 送る `Page.lifecycleEvent` の名前（`load`）。`Page.loadEventFired` と同じ時点で確定できる。
const LIFECYCLE_LOAD: &str = "load";

/// `Page.lifecycleEvent` を 1 件作る（TASK-43.3k・#1039・`CDP-2`）。`name` は上の固定定数のみを渡す
/// （入力由来の文字列を載せない）。`session_id` は `Dispatcher::dispatch` が補完するため `None`。
///
/// # 送らない lifecycle 名（`REPAIR-3`）
///
/// `DOMContentLoaded`・`networkAlmostIdle`・`networkIdle`・`commit`・`firstPaint`・`firstContentfulPaint`・
/// `firstMeaningfulPaint`・`firstMeaningfulPaintCandidate`・`InteractiveTime` は送らない。対応する実体
/// （DOM 構築完了通知・ネットワーク静止の計測・描画計測）が無く、偽の値で埋めないため。将来仕様は
/// core・render の進捗に合わせた送出（TASK-43・TASK-44・`CDP-2`）。
fn lifecycle_event(
    frame_id: &str,
    loader_id: &str,
    name: &'static str,
    timestamp: f64,
) -> CdpEvent {
    CdpEvent {
        method: "Page.lifecycleEvent",
        params: json!({
            "frameId": frame_id,
            "loaderId": loader_id,
            "name": name,
            "timestamp": timestamp,
        }),
        session_id: None,
    }
}

/// 確定した遷移の CDP イベント列を作る（TASK-42.3・`CDP-1`）。`PageNavigate::handle` が、遷移が確定し
/// 結果が公開された場合のみ呼ぶ。`session_id` は `Dispatcher::dispatch` がリクエストの値で補完するため
/// `None` で返す。
///
/// 順序の正本はここ。`lifecycle` が偽なら `frameNavigated` → `loadEventFired`。真（`Page.enable` 済みかつ
/// lifecycle 有効のセッション。43.3k・#1039）なら `lifecycle{init}` → `frameNavigated` → `loadEventFired` →
/// `lifecycle{load}`。`runtime` が `Some`（評価器が有効で、要求セッションが `Runtime.enable` 済み。
/// TASK-44.6・#1217）なら、`frameNavigated` の前に `Runtime.executionContextsCleared`、後に渡された
/// `Runtime.executionContextCreated` 列（main → isolated）を挟み、`loadEventFired` はその後に置く
/// （Playwright が新文書のコンテキストを `load` 前に揃える前提のため）。`None` なら従来の列のまま。`init` は `load` より前、`load` は `frameNavigated` より後でなければならない
/// （Playwright は新文書の commit で lifecycle をクリアし、Puppeteer は `init` で集合をリセットするため。
/// ソース読解による推定で未実測）。`timestamp` は 3 イベントで同じ値。
///
/// 送出順は `DispatchOutcome` の「レスポンス → イベント列」で確定とする。実 Chromium のように
/// `frameNavigated` を応答より前へ出す並べ替えの要否は TASK-43（`CDP-2`）で判断する。
///
/// # スタブ（`REPAIR-3`）
///
/// - `frame.mimeType` は固定値 `text/html`（`NavigationResult` が Content-Type を保持しないため。
///   実値化は `CORE-5`・TASK-25 以降）。
/// - `securityOrigin` 等の他フィールドは出さない。
/// - `Page.enable` は実装済み（43.3i）だが、`Page.frameNavigated`・`Page.loadEventFired` は意図的に
///   購読ゲートしない（Chromium との差異。`Page.enable` を送らない既存の利用を壊さないため。`REPAIR-3`）。
///   `frameStartedLoading`・`domContentEventFired` 等は未実装（TASK-43・`CDP-2`）。
fn navigation_events(
    frame_id: &str,
    loader_id: &str,
    url: &str,
    timestamp: f64,
    lifecycle: bool,
    runtime: Option<&[CdpEvent]>,
) -> Vec<CdpEvent> {
    let mut events = Vec::with_capacity(5 + runtime.map_or(0, <[CdpEvent]>::len));
    if lifecycle {
        events.push(lifecycle_event(
            frame_id,
            loader_id,
            LIFECYCLE_INIT,
            timestamp,
        ));
    }
    if runtime.is_some() {
        events.push(contexts_cleared_event());
    }
    events.push(CdpEvent {
        method: "Page.frameNavigated",
        params: json!({
            "frame": {
                "id": frame_id,
                "loaderId": loader_id,
                "url": url,
                "mimeType": MAIN_FRAME_MIME_TYPE,
            },
            "type": "Navigation",
        }),
        session_id: None,
    });
    if let Some(created) = runtime {
        events.extend(created.iter().cloned());
    }
    events.push(CdpEvent {
        method: "Page.loadEventFired",
        params: json!({ "timestamp": timestamp }),
        session_id: None,
    });
    if lifecycle {
        events.push(lifecycle_event(
            frame_id,
            loader_id,
            LIFECYCLE_LOAD,
            timestamp,
        ));
    }
    events
}

fn state_error(_: StateError) -> CdpError {
    CdpError::SERVER_ERROR
}

/// 評価コンテキストへの世代通知の結果を、遷移の失敗文言へ畳む（`CDP-7`・TASK-44.6・#1217）。
///
/// 失敗は core の追跡 scope 数の上限（`TrackedScopes`）だけで、無視すると古い世代のコンテキストが
/// 残る（`CDP-2`）ため fail-closed で遷移を失敗として扱う。文言は `fetch_error_text` と同じく固定表の
/// 値で、入力を反映しない。`None` なら通知に成功。
fn script_contexts_failure(result: Result<(), ScriptContextError>) -> Option<&'static str> {
    result.err().map(|_| "net::ERR_INSUFFICIENT_RESOURCES")
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
    navigate_gated(
        fetcher,
        nav,
        url,
        &Mutex::new(0),
        &PageScriptHandle::disabled(),
        &ScriptContextHandle::disabled(),
    )
    .await
}

/// [`navigate`] の本体。結果の確定を `gate` のロック内で行い、完了順の通し番号を付ける。
pub(crate) async fn navigate_gated(
    fetcher: &Fetcher,
    nav: &NavigationState,
    url: &str,
    gate: &CommitGate,
    page_scripts: &PageScriptHandle,
    script_contexts: &ScriptContextHandle,
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
            script: ScriptStage::NotAttempted,
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
    // 新しい遷移の開始と同時に、同じターゲットの古い世代の JS 実行・ページ内 fetch を打ち切る。
    // `about:blank`・取得失敗・取得の遅延のどの経路でも古いページの JS を走らせ続けない（`JS-4`・`JS-6`）。
    page_scripts.begin_generation(nav.scope(), generation);
    let mut outcome = NavigateOutcome {
        loader_id: format!("{:x}", generation.get()),
        error_text: None,
        committed: None,
        began: Some(generation),
        script: ScriptStage::NotAttempted,
    };
    // 評価コンテキスト（`Runtime.evaluate` の世代別コンテキスト）も同じ世代で失効させる。失敗は取得の
    // 失敗と同じ後処理（直近結果の無効化・URL を試行 URL へ）に乗せる。
    if let Some(text) =
        script_contexts_failure(script_contexts.begin_generation(nav.scope(), generation))
    {
        outcome.error_text = Some(text);
        return Ok(outcome);
    }

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
            // 取得中に後続の遷移が始まっていれば、無効なページの JS を起動せず中断する。
            if nav.current_generation() != generation {
                outcome.error_text = Some("net::ERR_ABORTED");
                return Ok(outcome);
            }
            let fetched = resp.body_text_lossy();
            // ページ JS 実行段。アクターの待機中は `gate` を保持しない（他ターゲットを塞がない）。
            // 確定の審判は従来どおり `commit_navigation`（追い越された場合は `Superseded`）。
            let html = if !page_scripts.is_enabled() {
                fetched
            } else if !can_run_scripts(fetcher, &final_url) {
                outcome.script = ScriptStage::BaseUrlRejected;
                fetched
            } else {
                let original: Arc<str> = Arc::from(fetched);
                let request = PageRunRequest::new(
                    Arc::clone(&original),
                    final_url.clone(),
                    nav.scope(),
                    generation,
                );
                let run = page_scripts.run_page_scripts(request).await;
                let (html, stage) = settle(&original, run);
                outcome.script = stage;
                html
            };
            if let Some(line) = script_log_line(&outcome.script, generation.get()) {
                eprintln!("{line}");
            }
            let result = NavigationResult::new(&final_url, html);
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
    /// ターゲット別の遷移状態は `CdpState::navigations()`（`DOM.getDocument` も読む。`CDP-1`）に置く。
    /// `sessionId` なし（ブラウザレベル）は従来どおり `AppState::navigation()` を直接使う。
    /// 成功した遷移は `AppState::navigation()` へ「直近にナビゲートしたページ」としてミラーする。
    /// ai（`AISNAP-6`）と `sessionId` なしの `DOM.getDocument` が共有状態を読むため維持する。
    ///
    /// 以下の `gate`・`published` はハンドラ（= ディスパッチャ）単位で、遷移状態表は `CdpState` 単位。
    /// ディスパッチャは `browser_websocket_config` で 1 回だけ作る前提。
    /// 結果確定の直列化と完了順の採番。
    /// 共有状態へ最後にミラーした内容の記録。ターゲット表の更新・ミラーを 1 つの
    /// 同期境界（このロック）で行い、古い遷移が新しい結果を巻き戻さないようにする。
    published: Mutex<Published>,
    /// テスト専用: イベント送出直前で一時停止させ、確定後〜送出前の競合を再現する。
    #[cfg(test)]
    pre_emit_pause: PreEmitPause,
}

/// テスト専用の一時停止ゲート（`armed` の間、送出直前で `paused` を立てて待つ）。
#[cfg(test)]
#[derive(Default)]
struct PreEmitPause {
    armed: std::sync::atomic::AtomicBool,
    paused: std::sync::atomic::AtomicBool,
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
    /// ブラウザレベル遷移の進行中のためミラーを見送った、確定順（`seq`）が最新のターゲット確定。
    /// 進行中記録の解放時（最後のガードの `Drop`）に共有状態へ反映する（`CDP-1`）。
    pending: Option<PendingMirror>,
}

/// ミラーを見送ったターゲット確定結果（解放時の再反映用）。
struct PendingMirror {
    target: TargetId,
    /// 保留時点のターゲット遷移状態と世代。反映前に、同一ターゲットの後続遷移で無効化されて
    /// いないかを確認する。
    nav: Arc<NavigationState>,
    generation: NavigationGeneration,
    result: Arc<NavigationResult>,
}

/// ブラウザレベル遷移の進行中を [`Published::browser_in_flight`] へ記録するガード。
/// 完了・エラー・future の破棄のいずれでも減算する。
///
/// 最後のガードの解放時に、見送ったターゲット確定（`pending`）があれば共有状態へ反映する。
/// 反映するのは見送った中で確定順が最新のものだけで、ブラウザレベル遷移が後から確定していれば
/// `pending` は破棄済みのため古い結果で上書きしない（`CDP-1`）。
struct BrowserFlight<'a> {
    published: &'a Mutex<Published>,
    app_nav: &'a NavigationState,
    registry: &'a TargetRegistry,
    gate: &'a CommitGate,
}

impl<'a> BrowserFlight<'a> {
    fn enter(
        published: &'a Mutex<Published>,
        app_nav: &'a NavigationState,
        registry: &'a TargetRegistry,
        gate: &'a CommitGate,
    ) -> Self {
        let mut p = published.lock().unwrap_or_else(PoisonError::into_inner);
        p.browser_in_flight = p.browser_in_flight.saturating_add(1);
        drop(p);
        Self {
            published,
            app_nav,
            registry,
            gate,
        }
    }
}

impl Drop for BrowserFlight<'_> {
    fn drop(&mut self) {
        let mut p = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        p.browser_in_flight = p.browser_in_flight.saturating_sub(1);
        if p.browser_in_flight != 0 {
            return;
        }
        let Some(m) = p.pending.take() else { return };
        // 保留中にターゲットが閉じられた、または同一ターゲットの後続遷移で結果が無効化された
        // 場合は反映しない（直近結果の契約。`CDP-1`）。世代確認は `gate` を保持して行う。
        let _gate = lock_gate(self.gate);
        if self.registry.target(&m.target).is_some()
            && m.nav.current_generation() == m.generation
            && let Ok(g) = self.app_nav.begin_navigation()
            && self
                .app_nav
                .commit_navigation(g, NavigationResult::new(m.result.url(), m.result.html()))
                .is_ok()
        {
            p.mirrored = Some((m.target, g));
        }
    }
}

impl PageNavigate {
    /// `options` で `Fetcher` を構築する。本番は `FetchOptions::default()`
    /// （内部アドレス拒否の安全側既定）、テストのみループバックを許可する。
    pub fn new(options: FetchOptions) -> Result<Self, Error> {
        Ok(Self {
            fetcher: Fetcher::new(options)?,
            published: Mutex::new(Published {
                seq: 0,
                mirrored: None,
                browser_in_flight: 0,
                pending: None,
            }),
            #[cfg(test)]
            pre_emit_pause: PreEmitPause::default(),
        })
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
            // `waitForDebuggerOnStart` で止めたターゲットは再開前の遷移を拒否する（再開時の
            // 初期読み込みが実行中の遷移を中断する競合を防ぐ。#910・`CDP-2`）。
            if let Some(tid) = &target_id
                && ctx.state.registry().is_paused(tid)
            {
                return Err(CdpError::TARGET_WAITING_FOR_DEBUGGER);
            }
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
            // ターゲット遷移で確定した文書の世代（実行コンテキストの登録に使う。TASK-44.6）。
            let mut committed_generation: Option<NavigationGeneration> = None;
            // ブラウザレベル遷移の送出直前の世代再確認用（共有状態 `app_nav` の確定時の世代）。
            let mut recheck_shared: Option<NavigationGeneration> = None;
            // ブラウザレベル遷移の進行中記録。確定・公開後もイベント送出が済むまで保持する。
            // 送出前に手放すと、その隙にターゲット遷移のミラーが共有状態の世代を進め、送出直前の
            // 世代再確認が失敗して Page.frameNavigated / loadEventFired が落ち、待機中の
            // クライアントが止まる（`CDP-1`）。ミラーを見送る間も `seq` は記録される。
            let mut _flight: Option<BrowserFlight<'_>> = None;
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
                                "frameId": tid.as_str(),
                                "loaderId": format!(
                                    "{:x}",
                                    ctx.state.navigations().get_or_create(registry, tid).current_generation().get()
                                ),
                                "errorText": "net::ERR_INVALID_URL",
                            })));
                        }
                    };
                    let nav = ctx.state.navigations().get_or_create(registry, tid);
                    let out = navigate_gated(
                        &self.fetcher,
                        &nav,
                        url,
                        ctx.state.commit_gate(),
                        ctx.state.app_state().page_scripts(),
                        ctx.state.app_state().script_contexts(),
                    )
                    .await?;
                    // ターゲット表の更新と共有状態へのミラーを 1 つのロック内で行う。
                    let mut published = self
                        .published
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if registry.target(tid).is_none() {
                        // 取得中にターゲットが閉じられた。状態を破棄し、内容は公開しない。
                        ctx.state.navigations().forget_with(tid, &nav);
                    } else if let Some(c) = &out.committed {
                        // 後続の遷移に追い越されていない場合に限り、確定 URL を反映する（`CDP-1`）。
                        // 確定直後に別遷移が始まって結果を保持できなかった（`result` が `None`）場合は
                        // 追い越されたものとして扱い、URL もミラーも公開しない。URL だけ公開して
                        // 共有状態との食い違いを作らないため。
                        // 世代確認から URL 更新・ミラーまでは `gate` を保持し、同一ターゲットの後続遷移の
                        // 開始（`navigate_gated` の世代払い出し）と排他にする（`CDP-1`）。
                        let _gate = lock_gate(ctx.state.commit_gate());
                        if nav.current_generation() == c.generation
                            && let Some(r) = &c.result
                        {
                            match registry.set_target_url(tid, &c.url) {
                                Ok(()) => {
                                    navigated = Some(c.url.clone());
                                    recheck = Some((Arc::clone(&nav), c.generation));
                                    committed_generation = Some(c.generation);
                                    // 今回確定した結果だけを、完了順が新しい場合に限りミラーする。
                                    // ブラウザレベル遷移の進行中などでミラーを見送る場合も、確定順
                                    // （`seq`）は記録する。記録しないと、見送った新しい確定（B）より
                                    // 古い確定（A）が後から条件を通り、共有状態に古い HTML が残る。
                                    if c.seq > published.seq {
                                        published.seq = c.seq;
                                        if published.browser_in_flight != 0 {
                                            // 見送った最新の確定を、進行中記録の解放時に反映する。
                                            published.pending = Some(PendingMirror {
                                                target: tid.clone(),
                                                nav: Arc::clone(&nav),
                                                generation: c.generation,
                                                result: Arc::clone(r),
                                            });
                                        } else if let Ok(g) = app_nav.begin_navigation()
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
                                Err(CdpStateError::UnknownTarget) => {
                                    ctx.state.navigations().forget_with(tid, &nav)
                                }
                                Err(_) => return Err(CdpError::SERVER_ERROR),
                            }
                        }
                    } else if let Some(g) = out.began
                        && nav.current_generation() == g
                    {
                        // 上の確認と URL 更新の間に同一ターゲットの後続遷移が開始しないよう、
                        // `gate` を保持したうえで世代を再確認する（`CDP-1`）。
                        let _gate = lock_gate(ctx.state.commit_gate());
                        if nav.current_generation() != g {
                            return Ok(HandlerOutput::result(json!({
                                "frameId": tid.as_str(),
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
                            Err(CdpStateError::UnknownTarget) => {
                                ctx.state.navigations().forget_with(tid, &nav)
                            }
                            Err(_) => return Err(CdpError::SERVER_ERROR),
                        }
                        // 共有状態が当該ターゲット自身のミラーのままなら無効化し、直前に成功した
                        // ページを読めなくする。別ターゲットやブラウザレベル遷移の結果が載って
                        // いる間は消さない（`CDP-1`）。
                        // 見送り中の確定が当該ターゲットのものなら、失敗した遷移が無効化済みの
                        // ため解放時に反映しない。
                        if published.pending.as_ref().is_some_and(|m| &m.target == tid) {
                            published.pending = None;
                        }
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
                    _flight = Some(BrowserFlight::enter(
                        &self.published,
                        app_nav,
                        ctx.state.registry(),
                        ctx.state.commit_gate(),
                    ));
                    let out = navigate_gated(
                        &self.fetcher,
                        app_nav,
                        url,
                        ctx.state.commit_gate(),
                        ctx.state.app_state().page_scripts(),
                        ctx.state.app_state().script_contexts(),
                    )
                    .await?;
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
                            let _gate = lock_gate(ctx.state.commit_gate());
                            if app_nav.current_generation() == c.generation && c.result.is_some() {
                                navigated = Some(c.url.clone());
                                recheck_shared = Some(c.generation);
                            }
                        }
                        if c.seq > published.seq {
                            published.seq = c.seq;
                            // 見送り中のターゲット確定はこの確定より古いため破棄する。
                            published.pending = None;
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
                "frameId": main_frame_id(target_id.as_ref()),
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
            // ブラウザレベル遷移も、確定時の世代が共有状態に残っているかを同様に再確認する。
            // ターゲット付き遷移は、公開後にターゲットが閉じられていないか（ターゲット表に存在し、
            // セッションが同じターゲットを指したままか）も再確認する。保持中の `NavigationState`
            // の世代はターゲット削除では変わらないため、世代だけでは閉じたターゲットの
            // イベントを止められない（`CDP-1`・`CDP-2`）。
            // 送出対象の確定（ターゲット生存・世代の再確認）とイベント列の作成は、同じ `gate` の
            // ロック内で行う。確認後にロックを手放してから生成すると、その間に別の遷移が同一
            // ターゲットで開始・確定しても古いイベントが出てしまうため（`CDP-1`）。
            #[cfg(test)]
            {
                use std::sync::atomic::Ordering;
                // 最初に到達した 1 件だけ停止する（後続のターゲット遷移は素通りさせる）。
                if self.pre_emit_pause.armed.load(Ordering::SeqCst)
                    && !self.pre_emit_pause.paused.swap(true, Ordering::SeqCst)
                {
                    while self.pre_emit_pause.armed.load(Ordering::SeqCst) {
                        let mut yielded = false;
                        std::future::poll_fn(|cx| {
                            if yielded {
                                return std::task::Poll::Ready(());
                            }
                            yielded = true;
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        })
                        .await;
                    }
                }
            }
            if let Some(url) = navigated {
                let _gate = lock_gate(ctx.state.commit_gate());
                let target_alive = match (&target_id, ctx.session_id) {
                    (Some(tid), Some(sid)) => {
                        let registry = ctx.state.registry();
                        registry.target(tid).is_some()
                            && registry.session_target(sid).as_ref() == Some(tid)
                    }
                    _ => true,
                };
                if target_alive
                    && recheck
                        .as_ref()
                        .is_none_or(|(n, g)| n.current_generation() == *g)
                    && recheck_shared.is_none_or(|g| app_nav.current_generation() == g)
                {
                    let lifecycle = ctx
                        .session_id
                        .is_some_and(|sid| ctx.state.registry().lifecycle_events_active(sid));
                    // 評価器が有効なときだけ、要求セッションの実行コンテキストを新しい世代へ作り替える
                    // （古い世代は全セッション分消える）。`gate` を保持したままなので、確認済みの世代と
                    // 食い違わない。Runtime 無効のセッションには cleared も created も送らない。
                    let runtime_events = match (&target_id, ctx.session_id, committed_generation) {
                        (Some(tid), Some(sid), Some(g)) if ctx.state.evaluator_enabled() => {
                            let registry = ctx.state.registry();
                            let runtime_on =
                                registry.is_domain_enabled(sid, SessionDomain::Runtime);
                            let views = registry.announce_navigation(tid, g, Some(sid));
                            runtime_on.then(|| {
                                let origin = crate::runtime::origin_string(&url);
                                views
                                    .iter()
                                    .map(|v| context_created_event(v, &origin))
                                    .collect::<Vec<_>>()
                            })
                        }
                        _ => None,
                    };
                    for ev in navigation_events(
                        main_frame_id(target_id.as_ref()),
                        &out.loader_id,
                        &url,
                        monotonic_seconds(),
                        lifecycle,
                        runtime_events.as_deref(),
                    ) {
                        output = output.with_event(ev);
                    }
                }
            }
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 無効な評価コンテキストハンドル（future が借用を持ち越すテストのため `'static` で返す）。
    fn no_script_contexts() -> &'static ScriptContextHandle {
        static H: OnceLock<ScriptContextHandle> = OnceLock::new();
        H.get_or_init(ScriptContextHandle::disabled)
    }
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// リクエストヘッダー終端（`\r\n\r\n`）まで読み切り、読んだ内容を返す。
    /// 未読データを残したまま close すると Windows は RST を送り、クライアントが応答を
    /// 読む前に接続エラーになるため、長い URL（`MAX_URL_LEN` 超）でも読み切れる上限にする。
    fn drain_request_head(stream: &mut TcpStream) -> String {
        let mut buf = [0u8; 1024];
        let mut data = Vec::new();
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 65536 {
                break;
            }
        }
        String::from_utf8_lossy(&data).into_owned()
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
        let ev = navigation_events(
            "00000000000000000000000000000001",
            "1",
            "http://example.test/",
            1.5,
            false,
            None,
        );
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].method, "Page.frameNavigated");
        assert_eq!(
            serde_json::from_str::<Value>(&ev[0].to_json()).unwrap(),
            json!({"method": "Page.frameNavigated", "params": {
                "frame": {"id": "00000000000000000000000000000001", "loaderId": "1",
                    "url": "http://example.test/", "mimeType": "text/html"},
                "type": "Navigation"}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&ev[1].to_json()).unwrap(),
            json!({"method": "Page.loadEventFired", "params": {"timestamp": 1.5}})
        );
        assert!(ev[0].session_id.is_none() && ev[1].session_id.is_none());
    }

    /// `runtime` が `Some` なら `executionContextsCleared` → `frameNavigated` → created 列 → `loadEventFired`
    /// の順で、lifecycle 真なら `init` が先頭・`load` が末尾（TASK-44.6・#1217・`CDP-2`）。
    #[test]
    fn cdp2_navigation_events_with_runtime_order() {
        use crate::runtime::{ContextView, World};
        let st = NavigationState::new();
        let g = st.begin_navigation().unwrap();
        let tid = TargetId::parse("T1").unwrap();
        let created: Vec<CdpEvent> = [(5, World::Main), (6, World::Isolated("u".into()))]
            .into_iter()
            .map(|(id, world)| {
                context_created_event(
                    &ContextView {
                        id,
                        target: tid.clone(),
                        generation: g,
                        world,
                        helper_epoch: None,
                    },
                    "http://example.test",
                )
            })
            .collect();
        let methods = |lifecycle| -> Vec<&'static str> {
            navigation_events(
                "T1",
                "1",
                "http://example.test/",
                1.0,
                lifecycle,
                Some(&created),
            )
            .iter()
            .map(|e| e.method)
            .collect()
        };
        assert_eq!(
            methods(false),
            [
                "Runtime.executionContextsCleared",
                "Page.frameNavigated",
                "Runtime.executionContextCreated",
                "Runtime.executionContextCreated",
                "Page.loadEventFired",
            ]
        );
        assert_eq!(
            methods(true),
            [
                "Page.lifecycleEvent",
                "Runtime.executionContextsCleared",
                "Page.frameNavigated",
                "Runtime.executionContextCreated",
                "Runtime.executionContextCreated",
                "Page.loadEventFired",
                "Page.lifecycleEvent",
            ]
        );
        // Some(空) でも cleared は送る。
        let empty: Vec<&'static str> =
            navigation_events("T1", "1", "http://example.test/", 1.0, false, Some(&[]))
                .iter()
                .map(|e| e.method)
                .collect();
        assert_eq!(
            empty,
            [
                "Runtime.executionContextsCleared",
                "Page.frameNavigated",
                "Page.loadEventFired"
            ]
        );
    }

    /// `TrackedScopes` 上限の通知失敗は固定文言の遷移失敗になり、成功は `None`（`CDP-7`・TASK-44.6）。
    #[test]
    fn cdp7_script_contexts_failure_maps_to_fixed_navigation_error() {
        use fandhe_browser_core::ScriptContextLimit;
        assert_eq!(script_contexts_failure(Ok(())), None);
        assert_eq!(
            script_contexts_failure(Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::TrackedScopes { limit: 1024 },
            })),
            Some("net::ERR_INSUFFICIENT_RESOURCES")
        );
    }

    /// `lifecycle` 真のとき `init` → `frameNavigated` → `loadEventFired` → `load` の順で、
    /// lifecycle の params が具体値で一致する（TASK-43.3k・`CDP-2`）。
    #[test]
    fn cdp2_navigation_events_with_lifecycle_order_and_shape() {
        let ev = navigation_events(
            "00000000000000000000000000000001",
            "7",
            "http://example.test/",
            2.5,
            true,
            None,
        );
        let methods: Vec<&str> = ev.iter().map(|e| e.method).collect();
        assert_eq!(
            methods,
            [
                "Page.lifecycleEvent",
                "Page.frameNavigated",
                "Page.loadEventFired",
                "Page.lifecycleEvent"
            ]
        );
        assert_eq!(
            ev[0].params,
            json!({"frameId": "00000000000000000000000000000001", "loaderId": "7", "name": "init", "timestamp": 2.5})
        );
        assert_eq!(
            ev[3].params,
            json!({"frameId": "00000000000000000000000000000001", "loaderId": "7", "name": "load", "timestamp": 2.5})
        );
        assert!(ev.iter().all(|e| e.session_id.is_none()));
    }

    #[test]
    fn cdp1_monotonic_seconds_is_non_negative_and_non_decreasing() {
        let a = monotonic_seconds();
        let b = monotonic_seconds();
        assert!(a.is_finite() && a >= 0.0);
        assert!(b >= a);
    }

    // --- ページ JS 実行の接続（TASK-112・`JS-4`・`JS-6`・`CDP-1`） ---------------------------
    // 単体テストのバイナリは JS ワーカーとして再実行できないため、実エンジンは動かさない。
    // 有効なハンドルは「渡された future を poll しない」実行器だけで作る。

    use fandhe_browser_core::{PageScriptActorOptions, PageScriptExecutor};
    use std::pin::Pin;
    use std::sync::mpsc;

    /// `block_on` が直ちに `Err` を返す実行器（要求は `ExecutorFailed` で解決される）。
    struct FailingExecutor;

    impl PageScriptExecutor for FailingExecutor {
        fn block_on<'a>(
            &self,
            _future: Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
        ) -> std::io::Result<()> {
            Err(std::io::Error::other("test executor"))
        }
    }

    /// `Sender` の drop まで `block_on` で止まる実行器。止めている間、ジョブは待ち行列に残る。
    struct GatedExecutor(Mutex<mpsc::Receiver<()>>);

    impl PageScriptExecutor for GatedExecutor {
        fn block_on<'a>(
            &self,
            _future: Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
        ) -> std::io::Result<()> {
            let rx = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = rx.recv();
            Err(std::io::Error::other("test executor released"))
        }
    }

    /// `execute_page_scripts = true` の設定から有効ハンドルを作る。エンジンなしのビルドでは設定自体が
    /// 拒否されるため `None`（そのビルドでは「同梱エンジンが無い」ことを呼び出し側で assert する）。
    fn enabled_handle(
        executor: Arc<dyn PageScriptExecutor>,
        capacity: usize,
    ) -> Option<PageScriptHandle> {
        let config =
            fandhe_browser_core::Config::from_toml_str("[js]\nexecute_page_scripts = true\n")
                .ok()?;
        Some(PageScriptHandle::with_options(
            config.js(),
            executor,
            PageScriptActorOptions::default().with_queue_capacity(capacity),
        ))
    }

    fn gated_handle(capacity: usize) -> Option<(PageScriptHandle, mpsc::Sender<()>)> {
        let (tx, rx) = mpsc::channel();
        let h = enabled_handle(Arc::new(GatedExecutor(Mutex::new(rx))), capacity)?;
        Some((h, tx))
    }

    async fn until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "condition not reached");
            tokio::task::yield_now().await;
        }
    }

    const SCRIPT_PAGE: &str = "<p>a</p><script>1</script>";

    /// `JS-1`・`CDP-1`: 無効ハンドルでは取得した HTML のまま確定し、アクターを起動しない。
    #[tokio::test]
    async fn cdp1_navigate_with_disabled_page_scripts_keeps_fetched_html() {
        let port = serve("200 OK", SCRIPT_PAGE);
        let nav = NavigationState::new();
        let handle = PageScriptHandle::disabled();
        let url = format!("http://127.0.0.1:{port}/");
        let out = navigate_gated(
            &allowed_fetcher(),
            &nav,
            &url,
            &Mutex::new(0),
            &handle,
            no_script_contexts(),
        )
        .await
        .unwrap();
        assert_eq!(out.script, ScriptStage::NotAttempted);
        assert_eq!(out.error_text, None);
        assert_eq!(nav.latest().unwrap().html(), SCRIPT_PAGE);
        assert_eq!(handle.stats().worker_spawns(), 0);
    }

    /// `JS-4`: `Executed` 以外の結果はすべて取得した HTML で確定する。
    #[test]
    fn js4_settle_falls_back_to_original_html() {
        let cases = [
            (
                Ok(PageScriptOutcome::Skipped(
                    PageScriptSkipReason::NoScriptElement,
                )),
                ScriptStage::Skipped(PageScriptSkipReason::NoScriptElement),
            ),
            (
                Err(PageScriptError::Busy { capacity: 1 }),
                ScriptStage::Failed(ScriptFailure::Busy),
            ),
            (
                Err(PageScriptError::Cancelled),
                ScriptStage::Failed(ScriptFailure::Cancelled),
            ),
            (
                Err(PageScriptError::ActorUnavailable {
                    kind: ActorFailureKind::Terminated,
                }),
                ScriptStage::Failed(ScriptFailure::ActorUnavailable(
                    ActorFailureKind::Terminated,
                )),
            ),
            (
                Err(PageScriptError::Run(Error::InvalidInput {
                    message: "x".to_owned(),
                })),
                ScriptStage::Failed(ScriptFailure::Run),
            ),
        ];
        for (result, expected) in cases {
            let (html, stage) = settle("orig", result);
            assert_eq!(html, "orig");
            assert_eq!(stage, expected);
        }
    }

    /// `SEC-2`: 実行可否は `validate_url`。`file:` と内部アドレスは不可、公開ホストの https は可。
    #[test]
    fn sec2_can_run_scripts_follows_url_validation() {
        let f = Fetcher::new(FetchOptions::default()).unwrap();
        assert!(!can_run_scripts(&f, "file:///etc/passwd"));
        assert!(!can_run_scripts(&f, "http://127.0.0.1/"));
        assert!(can_run_scripts(&f, "https://example.com/"));
    }

    /// `JS-6`: ログ行は固定文言とラベル・数値のみ。通常の結果では出さない。
    #[test]
    fn js6_script_log_line_is_fixed_text() {
        let line = script_log_line(
            &ScriptStage::Executed {
                abort: Some("wall_time"),
            },
            3,
        );
        assert_eq!(
            line.as_deref(),
            Some("cdp: page scripts: status=aborted generation=3 abort=wall_time")
        );
        let line = script_log_line(&ScriptStage::Failed(ScriptFailure::Busy), 7);
        assert_eq!(
            line.as_deref(),
            Some("cdp: page scripts: status=busy generation=7")
        );
        assert_eq!(script_log_line(&ScriptStage::NotAttempted, 1), None);
        assert_eq!(
            script_log_line(&ScriptStage::Executed { abort: None }, 1),
            None
        );
        assert_eq!(
            script_log_line(&ScriptStage::Failed(ScriptFailure::Cancelled), 1),
            None
        );
    }

    /// `JS-4`: 実行器が失敗しても遷移は成功し、取得した HTML で確定する。
    #[tokio::test]
    async fn js4_executor_failure_commits_fetched_html() {
        let port = serve("200 OK", SCRIPT_PAGE);
        let nav = NavigationState::new();
        let Some(handle) = enabled_handle(Arc::new(FailingExecutor), 4) else {
            assert!(fandhe_browser_core::bundled_engines().is_empty());
            return;
        };
        let url = format!("http://127.0.0.1:{port}/");
        let out = navigate_gated(
            &allowed_fetcher(),
            &nav,
            &url,
            &Mutex::new(0),
            &handle,
            no_script_contexts(),
        )
        .await
        .unwrap();
        assert_eq!(out.error_text, None);
        assert_eq!(
            out.script,
            ScriptStage::Failed(ScriptFailure::ActorUnavailable(
                ActorFailureKind::ExecutorFailed
            ))
        );
        assert_eq!(nav.latest().unwrap().html(), SCRIPT_PAGE);
    }

    /// `JS-4`・`CDP-1`: `<script>` なしのページ・`about:blank`・`file:` ではアクターを起動しない。
    #[tokio::test]
    async fn js4_no_script_blank_and_rejected_urls_do_not_spawn_worker() {
        let Some(handle) = enabled_handle(Arc::new(FailingExecutor), 4) else {
            assert!(fandhe_browser_core::bundled_engines().is_empty());
            return;
        };
        let nav = NavigationState::new();
        let gate = Mutex::new(0);
        let port = serve("200 OK", "<p>plain</p>");
        let url = format!("http://127.0.0.1:{port}/");
        let out = navigate_gated(
            &allowed_fetcher(),
            &nav,
            &url,
            &gate,
            &handle,
            no_script_contexts(),
        )
        .await
        .unwrap();
        assert_eq!(
            out.script,
            ScriptStage::Skipped(PageScriptSkipReason::NoScriptElement)
        );
        assert_eq!(nav.latest().unwrap().html(), "<p>plain</p>");
        for u in ["about:blank", "file:///etc/passwd"] {
            let out = navigate_gated(
                &allowed_fetcher(),
                &nav,
                u,
                &gate,
                &handle,
                no_script_contexts(),
            )
            .await
            .unwrap();
            assert_eq!(out.script, ScriptStage::NotAttempted, "{u}");
        }
        assert_eq!(handle.stats().worker_spawns(), 0);
    }

    /// `JS-6`・`CDP-1`: 待ち行列が満杯なら待たずに `Busy` となり、取得した HTML で確定する。
    /// 待機中の別ターゲットの遷移（`<script>` なし）は塞がれずに完了する。
    #[tokio::test]
    async fn js6_busy_commits_original_and_waiting_run_does_not_block_others() {
        let Some((handle, tx)) = gated_handle(1) else {
            assert!(fandhe_browser_core::bundled_engines().is_empty());
            return;
        };
        let (p1, p2, p3) = (
            serve("200 OK", SCRIPT_PAGE),
            serve("200 OK", "<p>other</p>"),
            serve("200 OK", "<p>b</p><script>2</script>"),
        );
        let (nav1, nav2, nav3) = (
            NavigationState::new(),
            NavigationState::new(),
            NavigationState::new(),
        );
        let gate = Mutex::new(0);
        let fetcher = allowed_fetcher();
        let u = |p: u16| format!("http://127.0.0.1:{p}/");
        let url1 = u(p1);
        let first = navigate_gated(&fetcher, &nav1, &url1, &gate, &handle, no_script_contexts());
        let rest = async {
            until(|| handle.stats().queued() == 1).await;
            // 別ターゲット（`<script>` なし）は待機中でも完了する。
            let other = navigate_gated(
                &fetcher,
                &nav2,
                &u(p2),
                &gate,
                &handle,
                no_script_contexts(),
            )
            .await
            .unwrap();
            assert_eq!(other.error_text, None);
            assert_eq!(handle.stats().queued(), 1);
            // 満杯なので `<script>` ありの別遷移は待たされず Busy。
            let busy = navigate_gated(
                &fetcher,
                &nav3,
                &u(p3),
                &gate,
                &handle,
                no_script_contexts(),
            )
            .await
            .unwrap();
            assert_eq!(busy.script, ScriptStage::Failed(ScriptFailure::Busy));
            assert_eq!(nav3.latest().unwrap().html(), "<p>b</p><script>2</script>");
            drop(tx);
        };
        let (first, ()) = tokio::join!(first, rest);
        let first = first.unwrap();
        assert!(matches!(first.script, ScriptStage::Failed(_)));
        assert_eq!(nav1.latest().unwrap().html(), SCRIPT_PAGE);
    }

    /// `CDP-1`: JS 実行待ちの間に同一ターゲットの後続遷移が確定したら、古い遷移は
    /// `net::ERR_ABORTED` で何も確定しない。
    #[tokio::test]
    async fn cdp1_navigation_superseded_while_scripts_pending_is_aborted() {
        let Some((handle, tx)) = gated_handle(4) else {
            assert!(fandhe_browser_core::bundled_engines().is_empty());
            return;
        };
        let (p1, p2) = (serve("200 OK", SCRIPT_PAGE), serve("200 OK", "<p>new</p>"));
        let nav = NavigationState::new();
        let gate = Mutex::new(0);
        let fetcher = allowed_fetcher();
        let u = |p: u16| format!("http://127.0.0.1:{p}/");
        let url1 = u(p1);
        let first = navigate_gated(&fetcher, &nav, &url1, &gate, &handle, no_script_contexts());
        let rest = async {
            until(|| handle.stats().queued() == 1).await;
            let second =
                navigate_gated(&fetcher, &nav, &u(p2), &gate, &handle, no_script_contexts())
                    .await
                    .unwrap();
            assert!(second.committed.is_some());
            drop(tx);
        };
        let (first, ()) = tokio::join!(first, rest);
        let first = first.unwrap();
        assert_eq!(first.error_text, Some("net::ERR_ABORTED"));
        assert!(first.committed.is_none());
        assert_eq!(nav.latest().unwrap().html(), "<p>new</p>");
    }

    /// `JS-4`・`CDP-1`: 同一ターゲットが `about:blank` へ遷移した時点で、待機中の古い JS 要求は
    /// 打ち切られる（実行器を解放する前に待ち行列から外れ、古い遷移は `net::ERR_ABORTED`）。
    #[tokio::test]
    async fn js4_about_blank_cancels_pending_older_page_scripts() {
        let Some((handle, tx)) = gated_handle(4) else {
            assert!(fandhe_browser_core::bundled_engines().is_empty());
            return;
        };
        let port = serve("200 OK", SCRIPT_PAGE);
        let nav = NavigationState::new();
        let gate = Mutex::new(0);
        let fetcher = allowed_fetcher();
        let url = format!("http://127.0.0.1:{port}/");
        let first = navigate_gated(&fetcher, &nav, &url, &gate, &handle, no_script_contexts());
        let rest = async {
            until(|| handle.stats().queued() == 1).await;
            let blank = navigate_gated(
                &fetcher,
                &nav,
                "about:blank",
                &gate,
                &handle,
                no_script_contexts(),
            )
            .await
            .unwrap();
            assert!(blank.committed.is_some());
            // 実行器を解放する前に、古い要求が待ち行列から外れている。
            until(|| handle.stats().queued() == 0).await;
            drop(tx);
        };
        let (first, ()) = tokio::join!(first, rest);
        let first = first.unwrap();
        assert_eq!(first.error_text, Some("net::ERR_ABORTED"));
        assert!(first.committed.is_none());
        assert_eq!(nav.latest().unwrap().url(), "about:blank");
    }

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
                let base = fandhe_browser_profile::test_base_dir();
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
                        let head = drain_request_head(&mut s);
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

        fn nav_and_dom_dispatcher() -> Dispatcher {
            let h = PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                .unwrap();
            Dispatcher::from_handlers(vec![
                ("Page.navigate", Box::new(h)),
                ("DOM.getDocument", Box::new(crate::dom::DomGetDocument)),
            ])
            .unwrap()
        }

        async fn rpc(
            d: &Dispatcher,
            st: &Arc<CdpState>,
            method: &str,
            sid: &str,
            params: Value,
        ) -> Value {
            let req = json!({"id": 1, "method": method, "sessionId": sid, "params": params});
            frames(d.dispatch(st, &req.to_string()).await).remove(0)
        }

        /// 2 ターゲットを別 URL へ遷移させ、各セッションが自分の文書だけを返す（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_get_document_returns_each_targets_own_document() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let d = nav_and_dom_dispatcher();
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
            let u1 = format!("http://127.0.0.1:{}/", serve("200 OK", "<p>alpha-body</p>"));
            let u2 = format!("http://127.0.0.1:{}/", serve("200 OK", "<p>bravo-body</p>"));
            rpc(&d, &st, "Page.navigate", s1.as_str(), json!({"url": u1})).await;
            rpc(&d, &st, "Page.navigate", s2.as_str(), json!({"url": u2})).await;
            let g1 = rpc(
                &d,
                &st,
                "DOM.getDocument",
                s1.as_str(),
                json!({"depth": -1}),
            )
            .await;
            let g2 = rpc(
                &d,
                &st,
                "DOM.getDocument",
                s2.as_str(),
                json!({"depth": -1}),
            )
            .await;
            assert_eq!(g1["result"]["root"]["documentURL"], json!(u1));
            assert_eq!(g2["result"]["root"]["documentURL"], json!(u2));
            let (j1, j2) = (g1.to_string(), g2.to_string());
            assert!(j1.contains("alpha-body") && !j1.contains("bravo-body"));
            assert!(j2.contains("bravo-body") && !j2.contains("alpha-body"));
        }

        /// 取得失敗後は古い文書を返さず明示エラー。他ターゲットは影響を受けない（`REPAIR-3`・`SEC-2`）。
        #[tokio::test]
        async fn repair3_get_document_after_failed_navigation_is_unavailable() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let d = nav_and_dom_dispatcher();
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
            let u = format!("http://127.0.0.1:{}/", serve("200 OK", "<p>kept</p>"));
            rpc(&d, &st, "Page.navigate", s1.as_str(), json!({"url": u})).await;
            rpc(&d, &st, "Page.navigate", s2.as_str(), json!({"url": u})).await;
            rpc(
                &d,
                &st,
                "Page.navigate",
                s1.as_str(),
                json!({"url": dead_url()}),
            )
            .await;
            let g1 = rpc(&d, &st, "DOM.getDocument", s1.as_str(), json!({})).await;
            assert_eq!(
                g1["error"],
                json!({"code": -32000, "message": "document not available for target"})
            );
            assert!(g1.get("result").is_none());
            let g2 = rpc(
                &d,
                &st,
                "DOM.getDocument",
                s2.as_str(),
                json!({"depth": -1}),
            )
            .await;
            assert!(g2.to_string().contains("kept"));
        }

        /// `about:blank` へ遷移したセッションは空の html/head/body を持つ文書を返す（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_get_document_about_blank_returns_empty_document() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let d = nav_and_dom_dispatcher();
            let t = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let s = st.registry().attach(&t).unwrap();
            rpc(
                &d,
                &st,
                "Page.navigate",
                s.as_str(),
                json!({"url": "about:blank"}),
            )
            .await;
            let g = rpc(&d, &st, "DOM.getDocument", s.as_str(), json!({"depth": -1})).await;
            assert_eq!(g["result"]["root"]["documentURL"], json!("about:blank"));
            let names: Vec<String> = g["result"]["root"]["children"][0]["children"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["nodeName"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(names, vec!["HEAD", "BODY"]);
        }

        /// ターゲットを閉じた後の同セッションは `INVALID_PARAMS`（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_get_document_for_closed_target_is_invalid_params() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let d = nav_and_dom_dispatcher();
            let t = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let s = st.registry().attach(&t).unwrap();
            let u = format!("http://127.0.0.1:{}/", serve("200 OK", "x"));
            rpc(&d, &st, "Page.navigate", s.as_str(), json!({"url": u})).await;
            st.registry().close_target(&t).unwrap();
            let g = rpc(&d, &st, "DOM.getDocument", s.as_str(), json!({})).await;
            assert_eq!(
                g["error"],
                json!({"code": -32602, "message": "invalid params"})
            );
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
            let flight = BrowserFlight::enter(
                &h.published,
                st.app_state().navigation(),
                st.registry(),
                st.commit_gate(),
            );
            for (id, sid, body) in [(1, &sa, "x"), (2, &sb, "y")] {
                let u = format!("http://127.0.0.1:{}/", serve("200 OK", body));
                let req = json!({"id": id, "method": "Page.navigate",
                    "sessionId": sid.as_str(), "params": {"url": u}});
                d.dispatch(&st, &req.to_string()).await;
            }
            drop(flight);
            let p = h.published.lock().unwrap();
            assert_eq!(p.seq, 2);
            // 解放時に、見送った中で確定順が最新（seq 2）のターゲット結果だけが反映される。
            assert_eq!(p.mirrored.as_ref().map(|(t, _)| t), Some(&tb));
            assert!(p.pending.is_none());
            assert_eq!(st.app_state().navigation().latest().unwrap().html(), "y");
        }

        /// 保留中の確定は、解放時にターゲットが閉じられていれば、または同一ターゲットの後続遷移で
        /// 無効化されていれば共有状態へ反映しない（TASK-42.3・`CDP-1`。レビュー指摘
        /// 「保留した結果の反映前にターゲットの状態を再確認する」）。
        #[tokio::test]
        async fn cdp1_page_navigate_pending_mirror_skipped_when_target_closed_or_superseded() {
            use crate::target::TargetKind;
            for close in [true, false] {
                let dir = TempDir::new();
                let st = state(&dir);
                let ta = st
                    .registry()
                    .create_target(TargetKind::Page, "about:blank")
                    .unwrap();
                let sa = st.registry().attach(&ta).unwrap();
                let h = Arc::new(
                    PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                        .unwrap(),
                );
                let d = Dispatcher::from_handlers(vec![(
                    "Page.navigate",
                    Box::new(SharedHandler(Arc::clone(&h))),
                )])
                .unwrap();
                let flight = BrowserFlight::enter(
                    &h.published,
                    st.app_state().navigation(),
                    st.registry(),
                    st.commit_gate(),
                );
                let u = format!("http://127.0.0.1:{}/", serve("200 OK", "x"));
                let req = json!({"id": 1, "method": "Page.navigate",
                    "sessionId": sa.as_str(), "params": {"url": u}});
                d.dispatch(&st, &req.to_string()).await;
                assert!(h.published.lock().unwrap().pending.is_some());
                if close {
                    st.registry().close_target(&ta).unwrap();
                } else {
                    let nav = st.navigations().get_or_create(st.registry(), &ta);
                    nav.begin_navigation().unwrap();
                }
                drop(flight);
                assert!(st.app_state().navigation().latest().is_none());
                assert!(h.published.lock().unwrap().mirrored.is_none());
            }
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

        /// ブラウザレベル遷移の確定後〜イベント送出前にターゲット遷移が確定しても、共有状態への
        /// ミラーは進行中記録で見送られ、ブラウザレベル遷移のイベントは落ちない
        /// （TASK-42.3・`CDP-1`。レビュー指摘「Target mirror drops browser events」）。
        #[tokio::test]
        async fn cdp1_page_navigate_browser_events_survive_target_mirror_before_emit() {
            use crate::target::TargetKind;
            use std::sync::atomic::Ordering;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let h = Arc::new(
                PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                    .unwrap(),
            );
            let d = Dispatcher::from_handlers(vec![(
                "Page.navigate",
                Box::new(SharedHandler(Arc::clone(&h))),
            )])
            .unwrap();
            h.pre_emit_pause.armed.store(true, Ordering::SeqCst);
            let ub = format!("http://127.0.0.1:{}/", serve("200 OK", "browser"));
            let ut = format!("http://127.0.0.1:{}/", serve("200 OK", "target"));
            let browser_req = json!({"id": 1, "method": "Page.navigate", "params": {"url": ub}});
            let target_req = json!({"id": 2, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": ut}});
            let browser_msg = browser_req.to_string();
            let browser = d.dispatch(&st, &browser_msg);
            let driver = async {
                while !h.pre_emit_pause.paused.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                // ブラウザ遷移は確定済みで送出待ち。この間にターゲット遷移を完了させる。
                let tf = frames(d.dispatch(&st, &target_req.to_string()).await);
                h.pre_emit_pause.armed.store(false, Ordering::SeqCst);
                tf
            };
            let (bo, tf) = tokio::join!(browser, driver);
            let bf = frames(bo);
            assert_eq!(bf.len(), 3);
            assert_eq!(bf[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(bf[2]["method"], json!("Page.loadEventFired"));
            assert_eq!(tf.len(), 3);
            assert_eq!(h.published.lock().unwrap().browser_in_flight, 0);
            // ガード解放後、共有状態は確定順が最新の割り込んだターゲット遷移の結果になる
            // （レビュー指摘「イベント送出待ちの間に確定したターゲット遷移が反映されない」）。
            let latest = st.app_state().navigation().latest().unwrap();
            assert_eq!(latest.html(), "target");
            assert_eq!(latest.url(), ut);
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
                    conn: None,
                };
                h.handle(ctx, &params).await.unwrap();
            }
            assert_eq!(st.navigations().len(), 2);
            st.registry().close_target(&t1).unwrap();
            // 閉鎖後の次の遷移で、閉じたターゲットの状態が破棄される。
            let ctx = CommandContext {
                state: &st,
                session_id: Some(&s2),
                conn: None,
            };
            h.handle(ctx, &params).await.unwrap();
            assert_eq!(st.navigations().len(), 1);
            assert!(st.navigations().get(&t2).is_some());
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

        /// 遷移確定後の `Page.getFrameTree` が、確定した URL と `loaderId` を返す（43.3i・#1037・`CDP-2`）。
        #[tokio::test]
        async fn cdp2_page_get_frame_tree_reflects_committed_url() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let h = PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                .unwrap();
            let d = Dispatcher::from_handlers(vec![
                ("Page.navigate", Box::new(h)),
                (
                    "Page.getFrameTree",
                    Box::new(crate::playwright_compat::PageGetFrameTree),
                ),
            ])
            .unwrap();
            let port = serve("200 OK", "<p>tree</p>");
            let url = format!("http://127.0.0.1:{port}/");
            let nav = json!({"id": 1, "method": "Page.navigate", "sessionId": sid.as_str(),
                "params": {"url": url}});
            let f = frames(d.dispatch(&st, &nav.to_string()).await);
            assert_eq!(f[0]["result"]["loaderId"], json!("1"), "{f:?}");
            assert_eq!(f[1]["params"]["frame"]["loaderId"], json!("1"));
            // メインフレームの frame ID は targetId に揃う（TASK-43.3o・#1050・`CDP-2`）。
            assert_eq!(f[0]["result"]["frameId"], json!(tid.as_str()));
            assert_eq!(f[1]["params"]["frame"]["id"], json!(tid.as_str()));
            let req = json!({"id": 2, "method": "Page.getFrameTree", "sessionId": sid.as_str()});
            let g = frames(d.dispatch(&st, &req.to_string()).await);
            assert_eq!(
                g,
                vec![
                    json!({"id": 2, "sessionId": sid.as_str(), "result": {"frameTree": {"frame": {
                    "id": tid.as_str(), "loaderId": "1", "url": url, "mimeType": "text/html"}}}})
                ]
            );
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
