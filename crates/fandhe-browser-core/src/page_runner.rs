//! ページスクリプトランナー: inline `<script>` の順次実行と失敗の集約
//! （TASK-109・MS-6・Issue #780・ビヘイビア `JS-4`・`JS-6`）。
//!
//! 「HTML を受け取り、inline の `<script>` を文書順に実行し、変更後の HTML と実行結果の
//! 診断を返す」入口。部品は次のとおり組み合わせる。
//!
//! - [`crate::page_script::collect_page_scripts`]（#774）: 実行対象の収集
//! - [`crate::dom_bridge::DomBridge`]（#777）と `JsRuntime::install_dom_shim_with_options`
//!   （#778・#779）: ページ内 JS から DOM を操作する経路
//! - [`fandhe_browser_js::EvaluateOptions`]（#776）: スクリプトごとの実時間上限の強制
//!
//! 呼び出し元は、#781（`src` の取得・DOMContentLoaded / load の発火。本モジュールを拡張する）と
//! TASK-112（#784）の専用スレッド上のアクター（`Page.navigate` への配線）を想定する。
//!
//! # 契約
//!
//! - ページごとに新しい [`JsRuntime`] と [`DomBridge`] を作り、ランナーが所有して返す前に
//!   解放する。グローバル状態はページ間で漏れない。
//! - 解放の順序は「診断の取得、`DomBridge::detach`、`JsRuntime` の drop、シリアライズ」。
//!   打ち切り後に放棄されたネイティブ呼び出しが遅れて DOM を変更しても、detach 済みのため
//!   結果には影響せず、子プロセス版エンジンは drop で kill / wait される（`JS-6`）。
//! - 例外は集約して後続を続ける。実時間超過・リソース上限・エンジン不能は fail-closed で
//!   以降の実行を打ち切り、残りを [`ScriptOutcome::NotExecuted`] にする。
//! - inline が 1 件も無いページではランタイムを作らない（子プロセスを起動しない。`PERF-7`）。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - `src` スクリプトは取得も実行もせず [`ScriptOutcome::DeferredToSrcLoader`] にする
//!   （#781 が実行結果に置き換える。`JS-8`）。総バイト上限・DOMContentLoaded / load の発火・
//!   `readyState` の遷移（`loading` のまま）も #781 の担当。
//! - 上限値の設定ファイルからの読み込みと専用スレッド化は TASK-112（#784）の担当。
//! - 可観測性（対応する `OperationKind` が無い）は未対応。
//! - パースは既定の `scripting_enabled = false` のため `<noscript>` の扱いが実ブラウザと異なる。
//! - スクリプトの completion 値が結果サイズ上限を超えると例外として扱われる
//!   （ソースの書き換えによる回避はしない）。

use std::cell::Cell;
use std::time::{Duration, Instant};

use fandhe_browser_js::{EvaluateOptions, JsEngineError};

use crate::config::JsConfig;
use crate::dom_bridge::{BridgeDiagnostics, DomBridge, DomBridgeError, DomBridgeLimits};
use crate::error::Error;
use crate::js_stub::JsRuntime;
use crate::page_script::{
    CollectedScripts, ScriptCollectionOptions, ScriptDiagnosticKind, ScriptSource,
    collect_page_scripts,
};
use crate::parse::{ParseOptions, parse_document};
use crate::{NodeId, dom_bridge::truncate_utf8};

/// スクリプト 1 件あたりの実時間上限の既定（`EvaluateOptions` の既定と同じ 2 秒）。
pub const DEFAULT_SCRIPT_WALL_TIME: Duration = EvaluateOptions::DEFAULT_TIMEOUT;
/// ページ全体（shim の注入を含む全スクリプト）の実時間上限の既定（5 秒）。
pub const DEFAULT_PAGE_WALL_TIME: Duration = Duration::from_secs(5);
/// 例外メッセージを保持する最大バイト数（ページ由来の文字列のため上限を設ける）。
pub const MAX_SCRIPT_ERROR_MESSAGE_BYTES: usize = 4096;

/// ランナーへの入力（HTML とベース URL）。
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PageRunInput<'a> {
    html: &'a str,
    base_url: &'a str,
}

impl<'a> PageRunInput<'a> {
    /// `html` と、`window.location` に設定するベース URL（http / https のみ）から作る。
    /// `base_url` は #781 で `src` の解決にも使う。
    pub fn new(html: &'a str, base_url: &'a str) -> Self {
        Self { html, base_url }
    }

    /// 入力 HTML。
    pub fn html(&self) -> &'a str {
        self.html
    }

    /// ベース URL。
    pub fn base_url(&self) -> &'a str {
        self.base_url
    }
}

/// ランナーの設定。上限値の設定ファイルからの読み込みは TASK-112 の担当。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PageRunOptions {
    parse: ParseOptions,
    collection: ScriptCollectionOptions,
    bridge_limits: DomBridgeLimits,
    script_wall_time: Duration,
    page_wall_time: Duration,
}

impl Default for PageRunOptions {
    fn default() -> Self {
        Self {
            parse: ParseOptions::default(),
            collection: ScriptCollectionOptions::default(),
            bridge_limits: DomBridgeLimits::default(),
            script_wall_time: DEFAULT_SCRIPT_WALL_TIME,
            page_wall_time: DEFAULT_PAGE_WALL_TIME,
        }
    }
}

impl PageRunOptions {
    /// パース設定を差し替える。
    #[must_use]
    pub fn with_parse(mut self, parse: ParseOptions) -> Self {
        self.parse = parse;
        self
    }

    /// スクリプト収集の上限を差し替える。
    #[must_use]
    pub fn with_collection(mut self, collection: ScriptCollectionOptions) -> Self {
        self.collection = collection;
        self
    }

    /// DOM ブリッジの上限を差し替える。
    #[must_use]
    pub fn with_bridge_limits(mut self, limits: DomBridgeLimits) -> Self {
        self.bridge_limits = limits;
        self
    }

    /// スクリプト 1 件あたりの実時間上限を設定する。
    #[must_use]
    pub fn with_script_wall_time(mut self, limit: Duration) -> Self {
        self.script_wall_time = limit;
        self
    }

    /// ページ全体の実時間上限を設定する。
    #[must_use]
    pub fn with_page_wall_time(mut self, limit: Duration) -> Self {
        self.page_wall_time = limit;
        self
    }

    /// パース設定。
    pub fn parse(&self) -> &ParseOptions {
        &self.parse
    }

    /// スクリプト収集の上限。
    pub fn collection(&self) -> &ScriptCollectionOptions {
        &self.collection
    }

    /// DOM ブリッジの上限。
    pub fn bridge_limits(&self) -> &DomBridgeLimits {
        &self.bridge_limits
    }

    /// スクリプト 1 件あたりの実時間上限。
    pub fn script_wall_time(&self) -> Duration {
        self.script_wall_time
    }

    /// ページ全体の実時間上限。
    pub fn page_wall_time(&self) -> Duration {
        self.page_wall_time
    }
}

/// 実時間上限の適用範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WallTimeScope {
    /// スクリプト 1 件あたりの上限。
    Script,
    /// ページ全体の上限。
    Page,
}

impl WallTimeScope {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Page => "page",
        }
    }
}

/// ページの実行を打ち切った理由（超過した上限の種別と値を残す。`JS-6`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AbortKind {
    /// 実時間上限を超えた。
    WallTime {
        /// どの上限か。
        scope: WallTimeScope,
        /// 超過した上限の値。
        limit: Duration,
    },
    /// エンジンのリソース上限（ヒープ等）に達した。
    ResourceLimit,
    /// エンジンが使えなくなった（子プロセスの終了等）。shim が失われるため継続しない。
    EngineUnavailable,
    /// 上記以外のエンジン側の失敗（未知の種別を含む。fail-closed）。
    EngineError,
}

impl AbortKind {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WallTime { .. } => "wall_time",
            Self::ResourceLimit => "resource_limit",
            Self::EngineUnavailable => "engine_unavailable",
            Self::EngineError => "engine_error",
        }
    }
}

/// スクリプト 1 件の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptOutcome {
    /// 正常終了（completion 値は保持しない）。
    Succeeded,
    /// 例外で終了した。後続のスクリプトは続行する。
    Exception {
        /// 例外メッセージ（[`MAX_SCRIPT_ERROR_MESSAGE_BYTES`] で切り詰め済み）。
        message: String,
        /// 切り詰めたか。
        truncated: bool,
    },
    /// 収集時に実行対象から外れた（`type=module` 等。#774 の診断）。
    Skipped {
        /// 外れた理由。
        kind: ScriptDiagnosticKind,
        /// 件数を伴う診断（上限超過）の件数。
        count: Option<usize>,
    },
    /// `src` スクリプト。本 issue では取得も実行もしない（#781 が置き換える。`JS-8`）。
    DeferredToSrcLoader,
    /// このスクリプトが原因でページの実行を打ち切った。
    Aborted {
        /// 打ち切りの理由。
        kind: AbortKind,
        /// エンジンのメッセージ（切り詰め済み）。
        message: Option<String>,
    },
    /// 先行スクリプトの打ち切りにより実行しなかった。
    NotExecuted {
        /// 打ち切りの理由。
        kind: AbortKind,
    },
}

impl ScriptOutcome {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Exception { .. } => "exception",
            Self::Skipped { .. } => "skipped",
            Self::DeferredToSrcLoader => "deferred_to_src_loader",
            Self::Aborted { .. } => "aborted",
            Self::NotExecuted { .. } => "not_executed",
        }
    }
}

/// スクリプト 1 件分の記録。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptRunRecord {
    index: Option<usize>,
    node: Option<NodeId>,
    outcome: ScriptOutcome,
}

impl ScriptRunRecord {
    /// 文書順の通し番号（件数上限の診断など特定できないものは `None`）。
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// 対応する `<script>` ノード。
    pub fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// 結果。
    pub fn outcome(&self) -> &ScriptOutcome {
        &self.outcome
    }
}

/// ページの実行を打ち切った記録。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageAbort {
    kind: AbortKind,
    index: Option<usize>,
}

impl PageAbort {
    /// 打ち切りの理由。
    pub fn kind(&self) -> &AbortKind {
        &self.kind
    }

    /// 原因になったスクリプトの通し番号。
    pub fn index(&self) -> Option<usize> {
        self.index
    }
}

/// ランナーの結果。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PageRunOutput {
    html: String,
    scripts: Vec<ScriptRunRecord>,
    dropped_collection_diagnostics: usize,
    abort: Option<PageAbort>,
    bridge_diagnostics: BridgeDiagnostics,
    elapsed: Duration,
}

impl PageRunOutput {
    /// 実行後の DOM をシリアライズした HTML。
    pub fn html(&self) -> &str {
        &self.html
    }

    /// 実行後の HTML を取り出す。
    pub fn into_html(self) -> String {
        self.html
    }

    /// スクリプトごとの記録（通し番号の昇順）。
    pub fn scripts(&self) -> &[ScriptRunRecord] {
        &self.scripts
    }

    /// 収集時の診断のうち、上限で記録しなかった件数。
    pub fn dropped_collection_diagnostics(&self) -> usize {
        self.dropped_collection_diagnostics
    }

    /// ページを打ち切った理由。打ち切っていなければ `None`。
    pub fn abort(&self) -> Option<&PageAbort> {
        self.abort.as_ref()
    }

    /// ブリッジが集めた診断（`console`・無視した `location` 操作）。
    pub fn bridge_diagnostics(&self) -> &BridgeDiagnostics {
        &self.bridge_diagnostics
    }

    /// ページ時計で測った経過時間。
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// 次のスクリプトに課す実時間上限を決める。ページの残りが 0 なら `None`（評価せず打ち切る）。
///
/// スクリプト単位の上限がページの残りに収まれば `Script`、収まらなければ残りで丸めて `Page`。
fn script_budget(
    per_script: Duration,
    page_limit: Duration,
    elapsed: Duration,
) -> Option<(Duration, WallTimeScope)> {
    let remaining = page_limit.saturating_sub(elapsed);
    if remaining.is_zero() {
        return None;
    }
    if per_script <= remaining {
        Some((per_script, WallTimeScope::Script))
    } else {
        Some((remaining, WallTimeScope::Page))
    }
}

/// ブリッジのエラーを `crate::Error` へ写す（URL 等の入力は反響しない）。
fn bridge_error(e: DomBridgeError) -> Error {
    match e {
        DomBridgeError::InvalidLocation { .. } => Error::InvalidInput {
            message: format!("invalid base URL: {e}"),
        },
        other => Error::Unsupported {
            message: format!("DOM bridge failure: {other}"),
        },
    }
}

fn truncated_message(message: &str) -> (String, bool) {
    let (kept, truncated) = truncate_utf8(message, MAX_SCRIPT_ERROR_MESSAGE_BYTES);
    (kept.to_string(), truncated)
}

/// ページの inline スクリプトを文書順に実行し、変更後の HTML と診断を返す
/// （`JS-4`・`JS-6`）。
///
/// 入力エラー（不正な base URL・パース失敗）、JS 無効のビルド、エンジン生成・shim 注入の失敗、
/// シリアライズ上限超過は `Err`（成功を装わない）。スクリプト側の失敗は [`PageRunOutput`] に
/// 集約する。
pub fn run_page_scripts(
    input: &PageRunInput<'_>,
    js: &JsConfig,
    options: &PageRunOptions,
) -> crate::Result<PageRunOutput> {
    run_with_runtime_factory(input, options, || JsRuntime::from_config(js))
}

fn run_with_runtime_factory(
    input: &PageRunInput<'_>,
    options: &PageRunOptions,
    make_runtime: impl FnOnce() -> crate::Result<JsRuntime>,
) -> crate::Result<PageRunOutput> {
    let parsed = parse_document(input.html, &options.parse)?;
    let doc = parsed.document;
    let collected = collect_page_scripts(&doc, &options.collection);

    let bridge = DomBridge::new(options.bridge_limits.clone());
    bridge.attach(doc).map_err(bridge_error)?;
    // 不正な base URL はここで Err にする（黙って about:blank で続行しない）。
    bridge.set_location(input.base_url).map_err(bridge_error)?;

    let has_inline = collected
        .entries()
        .iter()
        .any(|e| matches!(e.source(), ScriptSource::Inline(_)));
    let page_start = Instant::now();

    let mut records: Vec<ScriptRunRecord> = Vec::new();
    let mut abort: Option<PageAbort> = None;
    let mut runtime: Option<JsRuntime> = None;

    if has_inline {
        // shim 注入の各評価の直前に、ページ時計から残り予算を再計算する（先行 shim や
        // 初期化の消費時間を差し引く。期限切れならそこで打ち切る。`JS-6`）。
        let last_scope = Cell::new(WallTimeScope::Page);
        let mut next_options = || match script_budget(
            options.script_wall_time,
            options.page_wall_time,
            page_start.elapsed(),
        ) {
            Some((budget, scope)) => {
                last_scope.set(scope);
                Ok(EvaluateOptions::default().with_timeout(budget))
            }
            None => {
                last_scope.set(WallTimeScope::Page);
                Err(JsEngineError::Timeout(
                    "page wall time exhausted before shim installation".to_string(),
                ))
            }
        };
        // 評価前に予算が尽きていれば runtime を作らず打ち切る。
        if script_budget(
            options.script_wall_time,
            options.page_wall_time,
            page_start.elapsed(),
        )
        .is_none()
        {
            abort = Some(page_wall_abort(options, None));
        } else {
            let mut rt = make_runtime()?;
            match rt.install_dom_shim_with(&bridge, &mut next_options) {
                Ok(_) => runtime = Some(rt),
                // shim 評価がタイムアウトした場合はページの打ち切りとして集約する
                // （shim が無い runtime では継続できないため runtime は捨てる）。
                Err(Error::JsEvaluation(JsEngineError::Timeout(_))) => {
                    let scope = last_scope.get();
                    let limit = match scope {
                        WallTimeScope::Script => options.script_wall_time,
                        WallTimeScope::Page => options.page_wall_time,
                    };
                    abort = Some(PageAbort {
                        kind: AbortKind::WallTime { scope, limit },
                        index: None,
                    });
                }
                Err(e) => return Err(e),
            }
        }
    }

    for entry in collected.entries() {
        let outcome = match entry.source() {
            ScriptSource::External(_) => ScriptOutcome::DeferredToSrcLoader,
            ScriptSource::Inline(src) => {
                if let Some(a) = &abort {
                    ScriptOutcome::NotExecuted {
                        kind: a.kind.clone(),
                    }
                } else if let Some(rt) = runtime.as_mut() {
                    run_one(
                        rt,
                        &RunContext {
                            bridge: &bridge,
                            options,
                            page_start,
                        },
                        entry.node(),
                        entry.index(),
                        src,
                        &mut abort,
                    )?
                } else {
                    // runtime が無いのは abort 済みのときだけ（上の分岐で処理済み）。
                    ScriptOutcome::NotExecuted {
                        kind: AbortKind::EngineError,
                    }
                }
            }
        };
        records.push(ScriptRunRecord {
            index: Some(entry.index()),
            node: Some(entry.node()),
            outcome,
        });
    }

    let elapsed = page_start.elapsed();
    let bridge_diagnostics = bridge.take_diagnostics().map_err(bridge_error)?;
    // detach で DOM を確定してから runtime を解放する（遅れた変更を結果へ混ぜない）。
    let document = bridge
        .detach()
        .map_err(bridge_error)?
        .ok_or_else(|| Error::Unsupported {
            message: "DOM bridge has no attached document".to_string(),
        })?;
    drop(runtime);
    let html = document.serialize_html()?.into_html();

    Ok(finish(
        html,
        records,
        &collected,
        abort,
        bridge_diagnostics,
        elapsed,
    ))
}

/// ページの実時間上限超過による打ち切り記録を作る。
fn page_wall_abort(options: &PageRunOptions, index: Option<usize>) -> PageAbort {
    PageAbort {
        kind: AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit: options.page_wall_time,
        },
        index,
    }
}

/// [`run_one`] が参照するページ共通の実行コンテキスト。
struct RunContext<'a> {
    bridge: &'a DomBridge,
    options: &'a PageRunOptions,
    /// ページ実行の開始時刻（ページ時計。`JS-6`）。
    page_start: Instant,
}

/// inline スクリプト 1 件を評価し、結果を記録用の [`ScriptOutcome`] に写す。
/// 打ち切りになる場合は `abort` に理由を入れる。
fn run_one(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    node: NodeId,
    index: usize,
    src: &str,
    abort: &mut Option<PageAbort>,
) -> crate::Result<ScriptOutcome> {
    let RunContext {
        bridge,
        options,
        page_start,
    } = ctx;
    let Some((timeout, scope)) = script_budget(
        options.script_wall_time,
        options.page_wall_time,
        page_start.elapsed(),
    ) else {
        // このスクリプトは実行していない（原因ではない）ので index は付けない。
        let page_abort = page_wall_abort(options, None);
        let kind = page_abort.kind.clone();
        *abort = Some(page_abort);
        return Ok(ScriptOutcome::NotExecuted { kind });
    };

    bridge
        .set_current_script(Some(node))
        .map_err(bridge_error)?;
    let result = rt.execute_with_options(src, &EvaluateOptions::default().with_timeout(timeout));
    bridge.set_current_script(None).map_err(bridge_error)?;

    // 評価後にもページ時計を確認する。子プロセス経路の切り上げ・猶予で、残り時間を
    // 超えて正常応答が届くことがある（`JS-6`）。
    let page_expired = page_start.elapsed() > options.page_wall_time;

    let (kind, message) = match result {
        Ok(_) if page_expired => (page_wall_abort(options, None).kind, None),
        Ok(_) => return Ok(ScriptOutcome::Succeeded),
        Err(Error::JsEvaluation(JsEngineError::EvaluationFailed(m))) => {
            let (message, truncated) = truncated_message(&m);
            if page_expired {
                (page_wall_abort(options, None).kind, Some(message))
            } else {
                return Ok(ScriptOutcome::Exception { message, truncated });
            }
        }
        Err(Error::JsEvaluation(JsEngineError::Timeout(m))) => {
            let limit = match scope {
                WallTimeScope::Script => options.script_wall_time,
                WallTimeScope::Page => options.page_wall_time,
            };
            (
                AbortKind::WallTime { scope, limit },
                Some(truncated_message(&m).0),
            )
        }
        Err(Error::JsEvaluation(JsEngineError::ResourceLimitExceeded(m))) => {
            (AbortKind::ResourceLimit, Some(truncated_message(&m).0))
        }
        Err(Error::JsEvaluation(JsEngineError::EngineUnavailable(m))) => {
            (AbortKind::EngineUnavailable, Some(truncated_message(&m).0))
        }
        Err(Error::JsEvaluation(_)) | Err(_) => (AbortKind::EngineError, None),
    };
    *abort = Some(PageAbort {
        kind: kind.clone(),
        index: Some(index),
    });
    Ok(ScriptOutcome::Aborted { kind, message })
}

/// 実行結果と収集時の診断を通し番号で統合して出力にまとめる。
fn finish(
    html: String,
    mut scripts: Vec<ScriptRunRecord>,
    collected: &CollectedScripts,
    abort: Option<PageAbort>,
    bridge_diagnostics: BridgeDiagnostics,
    elapsed: Duration,
) -> PageRunOutput {
    for d in collected.diagnostics() {
        scripts.push(ScriptRunRecord {
            index: d.index(),
            node: d.node(),
            outcome: ScriptOutcome::Skipped {
                kind: d.kind(),
                count: d.count(),
            },
        });
    }
    // 安定ソート。通し番号の無い診断（件数上限）は末尾に置く。
    scripts.sort_by_key(|r| r.index.unwrap_or(usize::MAX));
    PageRunOutput {
        html,
        scripts,
        dropped_collection_diagnostics: collected.dropped_diagnostics(),
        abort,
        bridge_diagnostics,
        elapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js_shim::JS_SHIM_SOURCES;
    use fandhe_browser_js::{EngineKind, JsEngine, JsValue, NativeFn};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(String, Duration)>>>;

    /// テスト用エンジン。shim は成功させ、それ以外は script 文字列で振る舞いを分岐する。
    struct FakeEngine {
        calls: Calls,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl JsEngine for FakeEngine {
        fn evaluate_script(
            &mut self,
            script: &str,
            options: &EvaluateOptions,
        ) -> Result<JsValue, JsEngineError> {
            if JS_SHIM_SOURCES.iter().any(|s| s.source == script) {
                return Ok(JsValue::Undefined);
            }
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((script.to_string(), options.timeout()));
            }
            if script.starts_with("throw") {
                // 4 KiB 境界の検証用: "throw:<n>" で n バイトのメッセージ。
                let n = script
                    .split_once(':')
                    .and_then(|(_, n)| n.parse::<usize>().ok())
                    .unwrap_or(1);
                return Err(JsEngineError::EvaluationFailed("x".repeat(n)));
            }
            if let Some(ms) = script.strip_prefix("sleep:") {
                // 評価が予算を超えて正常応答する経路の再現用。
                let ms = ms.parse::<u64>().unwrap_or(0);
                std::thread::sleep(Duration::from_millis(ms));
                return Ok(JsValue::Undefined);
            }
            match script {
                "timeout" => Err(JsEngineError::Timeout("t".to_string())),
                "oom" => Err(JsEngineError::ResourceLimitExceeded("r".to_string())),
                "unavailable" => Err(JsEngineError::EngineUnavailable("u".to_string())),
                _ => Ok(JsValue::Undefined),
            }
        }

        fn inject_global_function(
            &mut self,
            _name: &str,
            _func: NativeFn,
        ) -> Result<(), JsEngineError> {
            Ok(())
        }

        fn bind_dom_like_object(
            &mut self,
            _name: &str,
            _methods: Vec<(String, NativeFn)>,
        ) -> Result<(), JsEngineError> {
            Ok(())
        }
    }

    struct Harness {
        calls: Calls,
        dropped: Arc<AtomicBool>,
        factory_called: Arc<AtomicBool>,
    }

    fn run(html: &str, options: &PageRunOptions) -> (crate::Result<PageRunOutput>, Harness) {
        let h = Harness {
            calls: Arc::new(Mutex::new(Vec::new())),
            dropped: Arc::new(AtomicBool::new(false)),
            factory_called: Arc::new(AtomicBool::new(false)),
        };
        let (calls, dropped, called) =
            (h.calls.clone(), h.dropped.clone(), h.factory_called.clone());
        let input = PageRunInput::new(html, "https://example.test/");
        let out = run_with_runtime_factory(&input, options, move || {
            called.store(true, Ordering::SeqCst);
            Ok(JsRuntime::from_engine_for_test(
                EngineKind::V8,
                Box::new(FakeEngine { calls, dropped }),
            ))
        });
        (out, h)
    }

    fn scripts_seen(h: &Harness) -> Vec<String> {
        h.calls
            .lock()
            .map(|c| c.iter().map(|(s, _)| s.clone()).collect())
            .unwrap_or_default()
    }

    fn outcomes(out: &PageRunOutput) -> Vec<&'static str> {
        out.scripts().iter().map(|r| r.outcome().as_str()).collect()
    }

    /// JS-4: inline は文書順に評価される。
    #[test]
    fn js_4_inline_scripts_run_in_document_order() {
        let (out, h) = run(
            "<script>1</script><script>2</script><script>3</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "2", "3"]);
        assert_eq!(outcomes(&out), vec!["succeeded"; 3]);
        assert!(out.abort().is_none());
    }

    /// JS-4: module・未対応 type は評価されず、通し番号順に Skipped として並ぶ。
    #[test]
    fn js_4_module_and_unsupported_are_skipped_not_executed() {
        let (out, h) = run(
            "<script>1</script><script type=module>2</script><script type=application/json>3</script><script>4</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "4"]);
        let kinds: Vec<_> = out
            .scripts()
            .iter()
            .map(|r| (r.index(), r.outcome().as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (Some(0), "succeeded"),
                (Some(1), "skipped"),
                (Some(2), "skipped"),
                (Some(3), "succeeded"),
            ]
        );
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::Skipped {
                kind: ScriptDiagnosticKind::SkippedModule,
                count: None
            }
        );
    }

    /// JS-4: src スクリプトは評価せず deferred。src だけのページではランタイムを作らない。
    #[test]
    fn js_4_src_scripts_are_deferred_to_src_loader() {
        let (out, h) = run("<script src=\"a.js\"></script>", &PageRunOptions::default());
        let out = out.expect("run");
        assert_eq!(outcomes(&out), vec!["deferred_to_src_loader"]);
        assert!(!h.factory_called.load(Ordering::SeqCst));
        assert!(scripts_seen(&h).is_empty());

        let (out, h) = run(
            "<script>1</script><script src=\"a.js\"></script>",
            &PageRunOptions::default(),
        );
        assert_eq!(
            outcomes(&out.expect("run")),
            vec!["succeeded", "deferred_to_src_loader"]
        );
        assert_eq!(scripts_seen(&h), vec!["1"]);
    }

    /// PERF-7 / JS-4: スクリプトの無いページでは factory が呼ばれず html が返る。
    #[test]
    fn js_4_no_inline_scripts_does_not_create_runtime() {
        let (out, h) = run("<p>hi</p>", &PageRunOptions::default());
        let out = out.expect("run");
        assert!(!h.factory_called.load(Ordering::SeqCst));
        assert!(out.html().contains("<p>hi</p>"));
        assert!(out.scripts().is_empty());
    }

    /// JS-4: 例外は集約し後続を続ける。
    #[test]
    fn js_4_exception_does_not_stop_following_scripts() {
        let (out, h) = run(
            "<script>1</script><script>throw:5</script><script>3</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "throw:5", "3"]);
        assert_eq!(outcomes(&out), vec!["succeeded", "exception", "succeeded"]);
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::Exception {
                message: "xxxxx".to_string(),
                truncated: false
            }
        );
        assert!(out.abort().is_none());
    }

    /// JS-4: 例外メッセージは 4096 バイトちょうどまで保持し、超えたら切り詰める。
    #[test]
    fn js_4_exception_message_truncated_at_4kib() {
        let (out, _) = run(
            "<script>throw:4096</script><script>throw:4097</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        match out.scripts()[0].outcome() {
            ScriptOutcome::Exception { message, truncated } => {
                assert_eq!(message.len(), 4096);
                assert!(!truncated);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match out.scripts()[1].outcome() {
            ScriptOutcome::Exception { message, truncated } => {
                assert_eq!(message.len(), 4096);
                assert!(*truncated);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // 多バイト文字の境界で切れる（文字境界を割らない）。
        let (kept, truncated) = truncated_message(&format!("{}あ", "x".repeat(4095)));
        assert_eq!(kept.len(), 4095);
        assert!(truncated);
    }

    /// JS-6: タイムアウトでその後を打ち切り、原因の上限（scope と値）を残す。
    #[test]
    fn js_6_timeout_aborts_remaining_with_wall_time() {
        let (out, h) = run(
            "<script>1</script><script>timeout</script><script>3</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        let expected = AbortKind::WallTime {
            scope: WallTimeScope::Script,
            limit: Duration::from_secs(2),
        };
        assert_eq!(scripts_seen(&h), vec!["1", "timeout"]);
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::Aborted {
                kind: expected.clone(),
                message: Some("t".to_string())
            }
        );
        assert_eq!(
            out.scripts()[2].outcome(),
            &ScriptOutcome::NotExecuted {
                kind: expected.clone()
            }
        );
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind(), &expected);
        assert_eq!(abort.index(), Some(1));
    }

    /// JS-6: 予算計算の境界値。
    #[test]
    fn js_6_script_budget_boundaries() {
        let ms = Duration::from_millis;
        let s2 = Duration::from_secs(2);
        let page = Duration::from_secs(5);
        assert_eq!(script_budget(s2, page, page), None);
        assert_eq!(script_budget(s2, page, page + ms(1)), None);
        assert_eq!(
            script_budget(s2, page, page - ms(1)),
            Some((ms(1), WallTimeScope::Page))
        );
        assert_eq!(
            script_budget(s2, page, page - s2),
            Some((s2, WallTimeScope::Script))
        );
        assert_eq!(
            script_budget(Duration::ZERO, page, Duration::ZERO),
            Some((Duration::ZERO, WallTimeScope::Script))
        );
    }

    /// JS-6: 既定設定ではエンジンへスクリプトごとに 2 秒が渡る。
    #[test]
    fn js_6_default_budget_passes_2s_to_engine() {
        let (out, h) = run(
            "<script>1</script><script>2</script>",
            &PageRunOptions::default(),
        );
        out.expect("run");
        let calls = h.calls.lock().expect("lock");
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|(_, t)| *t == Duration::from_secs(2)));
    }

    /// JS-6: ページ上限 0 では何も実行せず、ランタイムも作らない。
    #[test]
    fn js_6_page_wall_time_zero_runs_nothing() {
        let opts = PageRunOptions::default().with_page_wall_time(Duration::ZERO);
        let (out, h) = run("<script>1</script><script>2</script>", &opts);
        let out = out.expect("run");
        let kind = AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit: Duration::ZERO,
        };
        assert!(!h.factory_called.load(Ordering::SeqCst));
        assert!(scripts_seen(&h).is_empty());
        assert_eq!(
            out.scripts()
                .iter()
                .map(|r| r.outcome().clone())
                .collect::<Vec<_>>(),
            vec![
                ScriptOutcome::NotExecuted { kind: kind.clone() },
                ScriptOutcome::NotExecuted { kind: kind.clone() }
            ]
        );
        assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind));
    }

    /// JS-6: 最終スクリプトが予算を超えて正常応答してもページ期限超過として記録する。
    #[test]
    fn js_6_final_script_over_page_deadline_is_aborted() {
        let limit = Duration::from_millis(20);
        let opts = PageRunOptions::default().with_page_wall_time(limit);
        let (out, _h) = run("<script>sleep:60</script>", &opts);
        let out = out.expect("run");
        let kind = AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit,
        };
        assert_eq!(
            out.scripts()[0].outcome(),
            &ScriptOutcome::Aborted {
                kind: kind.clone(),
                message: None
            }
        );
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind(), &kind);
        assert_eq!(abort.index(), Some(0));
    }

    /// JS-6: ページの残りが小さいとスクリプトのタイムアウトが丸められ、scope は Page。
    #[test]
    fn js_6_page_budget_clamps_script_timeout() {
        let opts = PageRunOptions::default().with_page_wall_time(Duration::from_millis(500));
        let (out, h) = run("<script>timeout</script>", &opts);
        let out = out.expect("run");
        let t = h.calls.lock().expect("lock")[0].1;
        assert!(
            t > Duration::ZERO && t <= Duration::from_millis(500),
            "{t:?}"
        );
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::WallTime {
                scope: WallTimeScope::Page,
                limit: Duration::from_millis(500)
            })
        );
    }

    /// JS-6: リソース上限・エンジン不能は打ち切り（残りは NotExecuted）。
    #[test]
    fn js_6_resource_limit_and_engine_unavailable_abort() {
        for (src, kind) in [
            ("oom", AbortKind::ResourceLimit),
            ("unavailable", AbortKind::EngineUnavailable),
        ] {
            let html = format!("<script>{src}</script><script>2</script>");
            let (out, h) = run(&html, &PageRunOptions::default());
            let out = out.expect("run");
            assert_eq!(scripts_seen(&h), vec![src.to_string()]);
            assert_eq!(out.scripts()[0].outcome().as_str(), "aborted");
            assert_eq!(
                out.scripts()[1].outcome(),
                &ScriptOutcome::NotExecuted { kind: kind.clone() }
            );
            assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind));
        }
    }

    /// JS-6: 打ち切り後もエンジンは解放される。
    #[test]
    fn js_6_runtime_is_dropped_after_abort() {
        let (out, h) = run("<script>timeout</script>", &PageRunOptions::default());
        out.expect("run");
        assert!(h.dropped.load(Ordering::SeqCst));
    }

    /// JS-2 / JS-4: JS 無効のランタイムで inline があると Err（成功を装わない）。
    #[test]
    fn js_4_disabled_runtime_returns_err() {
        let input = PageRunInput::new("<script>1</script>", "https://example.test/");
        let err = run_with_runtime_factory(&input, &PageRunOptions::default(), || {
            Ok(JsRuntime::disabled())
        })
        .expect_err("disabled");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    }

    /// SEC-2: http / https 以外の base URL は拒否され、メッセージに URL を含まない。
    #[test]
    fn js_4_invalid_base_url_is_rejected() {
        for url in ["file:///etc/passwd", "javascript:alert(1)"] {
            let input = PageRunInput::new("<script>1</script>", url);
            let err = run_with_runtime_factory(&input, &PageRunOptions::default(), || {
                Ok(JsRuntime::disabled())
            })
            .expect_err("invalid");
            assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
            assert!(!err.to_string().contains("passwd"));
            assert!(!err.to_string().contains("alert"));
        }
    }
}
