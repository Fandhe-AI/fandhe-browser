//! ES module のグラフ解決: 静的 `import` の依存を [`Fetcher`] 経由で取得し、上限と MIME を検査して
//! compile → link → evaluate する（`JS-4`・`JS-6`・`JS-8`・`SEC-2`・`COMPAT-1`・MS-6・Issue #1116。
//! 設計 `docs/design/es-module-design.md` §6・§7・§8）。
//!
//! 呼び出し元は [`super::execute`] の実行ループ。`type=module` のエントリ
//! （[`crate::page_script::ScriptCollectionOptions::with_execute_modules`] が有効なときだけ収集される）を
//! [`run_module_entry`] に渡す。gate が無効なら module のエントリは存在せず、この経路は通らない。
//!
//! # 処理の流れ
//!
//! 1. entry を compile する（inline は本文、`src` は取得した本文）。`ModuleRecord` の依存
//!    （specifier）を referrer の URL に対して解決する（[`resolve_specifier`]）。
//! 2. 未取得の依存を BFS で取得して compile する。取得は `Fetcher::get_for_module` のみ
//!    （`validate_url` → 同一オリジン確認 → 取得 → `final_url` の再確認 → ステータス → MIME →
//!    バイト数）。解決後の URL をキーとするモジュールマップで重複を除き、循環は同じ id を返して許容する。
//!    失敗した URL もキャッシュし、再取得しない。
//! 3. 取得したグラフの未 link のモジュールを link する（全員が compile 済みなので循環も解決できる）。
//! 4. root を evaluate し、結果を [`ScriptOutcome`] に写す。
//!
//! # 上限と打ち切り（`JS-6`）
//!
//! 件数（`PageRunOptions::max_modules`）・深さ（[`super::MAX_MODULE_GRAPH_DEPTH`]）・
//! バイト数（`src` 1 件と総量。classic と総量を共有）・実時間（取得待ちはページの残り時間、評価は
//! スクリプト単位とページ全体）はすべて fail-closed で、取得・確保の前に判定する。超過は既存の
//! [`AbortKind`] に写し、原因の entry を `Aborted`、以降を `NotExecuted` にする（classic と同じ単位）。
//! 診断に URL・クエリは残さない。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - 同一オリジンの module だけを取得する（設計 §6 の案 (a)）。別オリジンを許す案 (b)・(c) と
//!   `Origin` ヘッダの扱いは未対応。別オリジンは [`SrcFetchErrorKind::CrossOrigin`]。
//! - 応答の MIME は JavaScript MIME type の essence に限る（Content-Type が無い応答も拒否）。
//!   classic の `src` が Content-Type を検査しないこととの差異は意図的（module は MIME 検査が必須）。
//! - `compile_module` は期限を引数に取れない（エンジンの既定値）。compile の前後でページ時計を確認する。
//! - 動的 `import()` の取得は未対応（#1117）。件数の計上枠（`attempted`）だけ用意してある。
//! - module の評価中は `document.currentScript` を設定しない（WHATWG では module は null）。
//!   実行順は classic と同じ文書順（`async` / `defer` の順序近似は行わない）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use fandhe_browser_js::{
    EvaluateOptions, JsEngineError, ModuleEvaluation, ModuleId, ModuleResolution,
};
use reqwest::Url;

use super::{
    AbortKind, MAX_MODULE_GRAPH_DEPTH, PageAbort, RunContext, ScriptBytesScope, ScriptOutcome,
    SrcFailure, SrcFetchErrorKind, WallTimeScope, check_src_response, classify_fetch_error,
    engine_error_abort, lossy_len_exceeds, page_wall_abort, script_budget, truncated_message,
};
use crate::error::Error;
use crate::fetch::Fetcher;
use crate::js_stub::JsRuntime;
use crate::page_script::ScriptSource;

/// 深さ超過の打ち切りに付ける固定メッセージ（URL を含めない）。
const DEPTH_EXCEEDED_MESSAGE: &str = "module graph depth limit exceeded";

/// WHATWG MIME Sniffing の JavaScript MIME type（essence の完全一致で比べる）。
const JAVASCRIPT_MIME_ESSENCES: &[&str] = &[
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
];

/// Content-Type が JavaScript MIME type かを判定する。`None`（ヘッダ無し）は偽（fail-closed）。
///
/// `;` より前を trim して ASCII 小文字にした essence を、一覧と完全一致で比べる
/// （パラメータは見ない）。
fn is_javascript_mime(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        return false;
    };
    let essence = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    JAVASCRIPT_MIME_ESSENCES.contains(&essence.as_str())
}

/// specifier を referrer の URL に対して解決する（設計 §6）。
///
/// `/`・`./`・`../` で始まるものは相対として `join` し、それ以外は絶対 URL として解析する。
/// 解析できなければ bare specifier として失敗させる（import map は未対応）。
fn resolve_specifier(referrer: &Url, specifier: &str) -> Result<Url, SrcFetchErrorKind> {
    if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
        referrer
            .join(specifier)
            .map_err(|_| SrcFetchErrorKind::InvalidUrl)
    } else {
        Url::parse(specifier).map_err(|_| SrcFetchErrorKind::BareSpecifier)
    }
}

/// URL とそのキー（モジュールマップのキー）を返す。
///
/// フラグメントは残す: ES module では `./m.js#a` と `./m.js#b` は別モジュールで、
/// `import.meta.url` にもフラグメントが残る（`JS-4`）。取得時だけ [`fetch_module`] が除く。
fn module_key(url: Url) -> (String, Url) {
    (url.as_str().to_string(), url)
}

/// 依存 1 件（解決済み）。
#[derive(Debug, Clone)]
struct Dep {
    /// ソースに書かれたままの specifier（link の対応表のキー）。
    specifier: String,
    /// 解決後の URL（フラグメント付き）のキー。
    key: String,
    url: Url,
}

/// compile 済みのモジュール。
#[derive(Debug)]
struct ReadyModule {
    id: ModuleId,
    deps: Vec<Dep>,
    /// 依存を含めて link 済みか。
    linked: bool,
}

/// モジュールの失敗（キャッシュする。再取得しない）。
#[derive(Debug, Clone)]
enum ModuleFailure {
    Fetch(SrcFetchErrorKind),
    Exception { message: String, truncated: bool },
}

#[derive(Debug)]
enum ModuleSlot {
    Ready(ReadyModule),
    Failed(ModuleFailure),
}

/// グラフ構築の失敗。
#[derive(Debug)]
enum GraphError {
    /// entry の結果として記録し、後続の実行は続ける。
    Failed(ModuleFailure),
    /// エンジンが module に未対応。
    Unsupported,
    /// ページの実行を打ち切る。
    Abort(AbortKind, Option<String>),
}

impl From<ModuleFailure> for GraphError {
    fn from(f: ModuleFailure) -> Self {
        Self::Failed(f)
    }
}

/// ページで 1 つの module グラフ（モジュールマップと上限の計上）。
///
/// [`super::execute`] が実行ループの前に作り、ページの終わりまで保持する。動的 `import()` の取得
/// （#1117）が同じ件数枠・マップを再利用できるようにするため、entry 単位ではなくページ単位。
#[derive(Debug)]
pub(super) struct ModuleGraph {
    /// 解決後の request URL をキーとするモジュールマップ。
    map: HashMap<String, ModuleSlot>,
    /// 取得を試みた URL 数と compile した inline module の数（`max_modules` の計上）。
    attempted: usize,
    /// 文書順でここまでに実行した classic のスクリプト本文のバイト数（総量の上限を共有する）。
    classic_bytes: usize,
    /// 取得・compile した module 本文の累計バイト数。
    module_bytes: usize,
    /// evaluate 済みの root と、その結果（同じ module を二度 evaluate せず、再参照には同じ結果を返す）。
    evaluated: HashMap<ModuleId, EvalState>,
}

/// evaluate 済み root の結果（再参照時に同じ結果を返すために保持する）。
#[derive(Debug, Clone)]
enum EvalState {
    Succeeded,
    /// 保留（top-level await 等）。再参照でも成功とは見なさない。
    Pending,
    Rejected {
        message: String,
        truncated: bool,
    },
}

impl ModuleGraph {
    pub(super) fn new() -> Self {
        Self {
            map: HashMap::new(),
            attempted: 0,
            classic_bytes: 0,
            module_bytes: 0,
            evaluated: HashMap::new(),
        }
    }

    /// classic 1 件の実行時点でその本文のバイト数を総量へ加算する（文書順の会計。`JS-6`）。
    /// 後続の classic の分は、その classic の実行時点まで module の枠を消費しない。
    /// 総量を超えるなら `false`（呼び出し側が打ち切る）。
    pub(super) fn account_classic(&mut self, len: usize, max_total: usize) -> bool {
        if self
            .total_bytes()
            .checked_add(len)
            .is_none_or(|t| t > max_total)
        {
            return false;
        }
        self.classic_bytes = self.classic_bytes.saturating_add(len);
        true
    }

    /// これまでに計上した本文の総バイト数。
    fn total_bytes(&self) -> usize {
        self.classic_bytes.saturating_add(self.module_bytes)
    }
}

fn page_clock_abort(ctx: &RunContext<'_>) -> GraphError {
    GraphError::Abort(page_wall_abort(ctx.options, None).kind, None)
}

/// ページの残り実時間（0 なら取得しない）。
fn remaining_page_time(ctx: &RunContext<'_>) -> Duration {
    ctx.options
        .page_wall_time
        .saturating_sub(ctx.page_start.elapsed())
}

/// ページ時計が尽きているか。
fn page_clock_exhausted(ctx: &RunContext<'_>) -> bool {
    script_budget(
        ctx.options.script_wall_time,
        ctx.options.page_wall_time,
        ctx.page_start.elapsed(),
    )
    .is_none()
}

/// 件数の枠を 1 つ使う。ページ時計が尽きていれば打ち切る。
fn reserve_module_slot(ctx: &RunContext<'_>, graph: &mut ModuleGraph) -> Result<(), GraphError> {
    if page_clock_exhausted(ctx) {
        return Err(page_clock_abort(ctx));
    }
    if graph.attempted >= ctx.options.max_modules {
        return Err(GraphError::Abort(
            AbortKind::ScriptCount {
                limit: ctx.options.max_modules,
            },
            None,
        ));
    }
    graph.attempted = graph.attempted.saturating_add(1);
    Ok(())
}

/// module 本文のバイト数を総量へ加算する。上限を超えるなら総量超過の打ち切り。
fn add_module_bytes(
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    len: usize,
) -> Result<(), GraphError> {
    if graph
        .total_bytes()
        .checked_add(len)
        .is_none_or(|t| t > ctx.options.max_total_script_bytes)
    {
        return Err(total_bytes_abort(ctx));
    }
    graph.module_bytes = graph.module_bytes.saturating_add(len);
    Ok(())
}

fn total_bytes_abort(ctx: &RunContext<'_>) -> GraphError {
    GraphError::Abort(
        AbortKind::ScriptBytes {
            scope: ScriptBytesScope::Total,
            limit: ctx.options.max_total_script_bytes,
        },
        None,
    )
}

/// `url` の module を 1 件取得する。取得は [`Fetcher`] 経由のみ（`JS-8`）。
///
/// 検査の順序: `validate_url`（scheme・IP リテラル。`data:`・`file:`・`blob:` はここで拒否）→
/// 同一オリジン確認 → 取得（同一オリジンのリダイレクトのみ・本文は受信中に `limit` で強制）→
/// `final_url` のオリジン再確認 → ステータス → MIME → 非可逆変換後の長さ。
async fn fetch_module(
    fetcher: &Fetcher,
    base: &Url,
    url: &Url,
    limit: usize,
    deadline: Duration,
) -> Result<(Url, String), SrcFailure> {
    // フラグメントは取得に関係しない（モジュールの同一性だけに使う）。
    let mut request_url = url.clone();
    request_url.set_fragment(None);
    let url = &request_url;
    fetcher
        .validate_url(url.as_str())
        .map_err(|e| classify_fetch_error(&e))?;
    if url.origin() != base.origin() {
        return Err(SrcFailure::Fetch(SrcFetchErrorKind::CrossOrigin));
    }
    let response = fetcher
        .get_for_module(
            url.as_str(),
            u64::try_from(limit).unwrap_or(u64::MAX),
            deadline,
        )
        .await
        .map_err(|e| classify_fetch_error(&e))?;
    let final_url = Url::parse(response.final_url())
        .map_err(|_| SrcFailure::Fetch(SrcFetchErrorKind::Other))?;
    if final_url.origin() != base.origin() {
        return Err(SrcFailure::Fetch(SrcFetchErrorKind::CrossOrigin));
    }
    check_src_response(response.status(), response.body().len(), limit)?;
    if !is_javascript_mime(response.content_type()) {
        return Err(SrcFailure::Fetch(SrcFetchErrorKind::UnsupportedMimeType));
    }
    // 非可逆変換で膨らむ分を、確保前に判定する。
    if lossy_len_exceeds(response.body(), limit) {
        return Err(SrcFailure::TooLarge);
    }
    Ok((final_url, response.body_text_lossy()))
}

/// ソースを compile し、依存を `referrer` に対して解決する。
///
/// 構文エラーは [`ModuleFailure::Exception`]、解決できない specifier は
/// [`ModuleFailure::Fetch`]、期限・資源・エンジンの失敗は打ち切りとして返す。
fn compile_source(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    referrer: &Url,
    source: &str,
) -> Result<(ModuleId, Vec<Dep>), GraphError> {
    let Some((_, scope)) = script_budget(
        ctx.options.script_wall_time,
        ctx.options.page_wall_time,
        ctx.page_start.elapsed(),
    ) else {
        return Err(page_clock_abort(ctx));
    };
    let result = rt.compile_module(referrer.as_str(), source);
    // compile は期限を引数に取れないため、後でページ時計を確認する（REPAIR-3）。
    let page_expired = ctx.page_start.elapsed() > ctx.options.page_wall_time;
    let record = match result {
        Ok(_) if page_expired => return Err(page_clock_abort(ctx)),
        Ok(record) => record,
        Err(Error::JsEvaluation(JsEngineError::EvaluationFailed(m))) => {
            if page_expired {
                return Err(page_clock_abort(ctx));
            }
            let (message, truncated) = truncated_message(&m);
            return Err(ModuleFailure::Exception { message, truncated }.into());
        }
        Err(Error::JsEvaluation(JsEngineError::NotYetImplemented(_))) => {
            return Err(GraphError::Unsupported);
        }
        Err(e) => {
            let (kind, message) = engine_error_abort(&e, scope, ctx.options);
            return Err(GraphError::Abort(kind, message));
        }
    };
    let mut deps = Vec::with_capacity(record.requests().len());
    for request in record.requests() {
        let resolved = resolve_specifier(referrer, request.specifier())
            .map_err(|kind| GraphError::Failed(ModuleFailure::Fetch(kind)))?;
        let (key, url) = module_key(resolved);
        deps.push(Dep {
            specifier: request.specifier().to_string(),
            key,
            url,
        });
    }
    Ok((record.id(), deps))
}

/// 未取得の module を取得して compile する。件数の枠は呼び出し側が確保済み。
async fn fetch_and_compile(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    url: &Url,
) -> Result<ReadyModule, GraphError> {
    let per = ctx.options.max_src_script_bytes;
    let remaining = ctx
        .options
        .max_total_script_bytes
        .saturating_sub(graph.total_bytes());
    // 総量の残りも受信上限に含める（超過分を確保しない）。
    let limit = per.min(remaining);
    let fetched = fetch_module(ctx.fetcher, ctx.base, url, limit, remaining_page_time(ctx)).await;
    let (mut final_url, text) = match fetched {
        Ok(v) => v,
        // ページ時計が尽きたことによるタイムアウトだけを実時間超過として扱う。
        Err(SrcFailure::Fetch(SrcFetchErrorKind::Timeout)) if page_clock_exhausted(ctx) => {
            return Err(page_clock_abort(ctx));
        }
        Err(SrcFailure::Fetch(kind)) => return Err(ModuleFailure::Fetch(kind).into()),
        Err(SrcFailure::TooLarge) if remaining < per => return Err(total_bytes_abort(ctx)),
        Err(SrcFailure::TooLarge) => {
            return Err(GraphError::Abort(
                AbortKind::ScriptBytes {
                    scope: ScriptBytesScope::PerScript,
                    limit: per,
                },
                None,
            ));
        }
    };
    add_module_bytes(ctx, graph, text.len())?;
    // `import.meta.url` とモジュール同一性のため、要求のフラグメントを戻す。
    final_url.set_fragment(url.fragment());
    let (id, deps) = compile_source(rt, ctx, &final_url, &text)?;
    Ok(ReadyModule {
        id,
        deps,
        linked: false,
    })
}

/// entry のグラフを取得・compile・link し、root の id を返す。
async fn load_graph(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    source: &ScriptSource,
) -> Result<ModuleId, GraphError> {
    let mut queue: VecDeque<(Dep, usize)> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut to_link: Vec<String> = Vec::new();
    // inline の root はマップに登録しない（同じ base URL の inline module が複数あり得るため）。
    let mut inline_root: Option<(ModuleId, Vec<Dep>)> = None;
    let mut root_key: Option<String> = None;

    match source {
        ScriptSource::Inline(text) => {
            reserve_module_slot(ctx, graph)?;
            add_module_bytes(ctx, graph, text.len())?;
            let (id, deps) = compile_source(rt, ctx, ctx.base, text)?;
            for dep in &deps {
                queue.push_back((dep.clone(), 1));
            }
            inline_root = Some((id, deps));
        }
        ScriptSource::External(src) => {
            let resolved = ctx
                .base
                .join(src)
                .map_err(|_| ModuleFailure::Fetch(SrcFetchErrorKind::InvalidUrl))?;
            let (key, url) = module_key(resolved);
            root_key = Some(key.clone());
            queue.push_back((
                Dep {
                    specifier: src.clone(),
                    key,
                    url,
                },
                0,
            ));
        }
    }

    while let Some((dep, depth)) = queue.pop_front() {
        if !seen.insert(dep.key.clone()) {
            continue;
        }
        let already_linked = matches!(
            graph.map.get(&dep.key),
            Some(ModuleSlot::Ready(ReadyModule { linked: true, .. }))
        );
        if already_linked {
            continue;
        }
        if depth > MAX_MODULE_GRAPH_DEPTH {
            return Err(GraphError::Abort(
                AbortKind::ResourceLimit,
                Some(DEPTH_EXCEEDED_MESSAGE.to_string()),
            ));
        }
        if !graph.map.contains_key(&dep.key) {
            reserve_module_slot(ctx, graph)?;
            match fetch_and_compile(rt, ctx, graph, &dep.url).await {
                Ok(ready) => {
                    graph.map.insert(dep.key.clone(), ModuleSlot::Ready(ready));
                }
                Err(GraphError::Failed(f)) => {
                    graph
                        .map
                        .insert(dep.key.clone(), ModuleSlot::Failed(f.clone()));
                    return Err(GraphError::Failed(f));
                }
                Err(e) => return Err(e),
            }
        }
        match graph.map.get(&dep.key) {
            Some(ModuleSlot::Failed(f)) => return Err(GraphError::Failed(f.clone())),
            Some(ModuleSlot::Ready(ready)) => {
                to_link.push(dep.key.clone());
                for d in &ready.deps {
                    queue.push_back((d.clone(), depth.saturating_add(1)));
                }
            }
            // 直前に挿入済みなので到達しない（fail-closed）。
            None => return Err(GraphError::Abort(AbortKind::EngineError, None)),
        }
    }

    // 全員が compile 済みなので、循環していても link できる。
    for key in &to_link {
        let Some(ModuleSlot::Ready(ready)) = graph.map.get(key) else {
            continue;
        };
        let (id, deps) = (ready.id, ready.deps.clone());
        link_one(rt, ctx, graph, id, &deps)?;
        if let Some(ModuleSlot::Ready(ready)) = graph.map.get_mut(key) {
            ready.linked = true;
        }
    }
    if let Some((id, deps)) = inline_root {
        link_one(rt, ctx, graph, id, &deps)?;
        return Ok(id);
    }
    match root_key.and_then(|k| graph.map.get(&k)) {
        Some(ModuleSlot::Ready(ready)) => Ok(ready.id),
        Some(ModuleSlot::Failed(f)) => Err(GraphError::Failed(f.clone())),
        None => Err(GraphError::Abort(AbortKind::EngineError, None)),
    }
}

/// `id` の依存を対応表（specifier → 解決先の id）で link する。
fn link_one(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &ModuleGraph,
    id: ModuleId,
    deps: &[Dep],
) -> Result<(), GraphError> {
    let mut specifiers: HashSet<&str> = HashSet::new();
    let mut resolutions = Vec::with_capacity(deps.len());
    for dep in deps {
        if !specifiers.insert(dep.specifier.as_str()) {
            continue;
        }
        let Some(ModuleSlot::Ready(target)) = graph.map.get(&dep.key) else {
            return Err(GraphError::Abort(AbortKind::EngineError, None));
        };
        resolutions.push(ModuleResolution::new(dep.specifier.clone(), target.id));
    }
    match rt.link_module(id, &resolutions) {
        Ok(()) => Ok(()),
        Err(Error::JsEvaluation(JsEngineError::NotYetImplemented(_))) => {
            Err(GraphError::Unsupported)
        }
        Err(e) => {
            let (kind, message) = engine_error_abort(&e, WallTimeScope::Page, ctx.options);
            Err(GraphError::Abort(kind, message))
        }
    }
}

/// 打ち切りを記録して `Aborted` の結果を作る。
fn aborted(
    kind: AbortKind,
    message: Option<String>,
    index: usize,
    abort: &mut Option<PageAbort>,
) -> ScriptOutcome {
    *abort = Some(PageAbort {
        kind: kind.clone(),
        index: Some(index),
    });
    ScriptOutcome::Aborted { kind, message }
}

/// link 済みの root を evaluate して結果を写す。
fn evaluate_root(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    root: ModuleId,
    index: usize,
    abort: &mut Option<PageAbort>,
) -> ScriptOutcome {
    let options = ctx.options;
    let Some((timeout, scope)) = script_budget(
        options.script_wall_time,
        options.page_wall_time,
        ctx.page_start.elapsed(),
    ) else {
        return aborted(page_wall_abort(options, None).kind, None, index, abort);
    };
    let result = rt.evaluate_module(root, &EvaluateOptions::default().with_timeout(timeout));
    // 子プロセス経路の猶予で、残り時間を超えて応答が届くことがある（`JS-6`）。
    let page_expired = ctx.page_start.elapsed() > options.page_wall_time;
    match result {
        Ok(_) if page_expired => aborted(page_wall_abort(options, None).kind, None, index, abort),
        Ok(ModuleEvaluation::Fulfilled) => {
            graph.evaluated.insert(root, EvalState::Succeeded);
            ScriptOutcome::Succeeded
        }
        Ok(ModuleEvaluation::Pending) => {
            graph.evaluated.insert(root, EvalState::Pending);
            ScriptOutcome::ModulePending
        }
        Ok(ModuleEvaluation::Rejected { message, .. }) => {
            let (message, truncated) = truncated_message(&message);
            graph.evaluated.insert(
                root,
                EvalState::Rejected {
                    message: message.clone(),
                    truncated,
                },
            );
            ScriptOutcome::Exception { message, truncated }
        }
        // 未知の結果は成功と見なさない（fail-closed）。
        Ok(_) => aborted(AbortKind::EngineError, None, index, abort),
        Err(Error::JsEvaluation(JsEngineError::NotYetImplemented(_))) => {
            ScriptOutcome::ModuleUnsupported
        }
        Err(e) => {
            let (kind, message) = engine_error_abort(&e, scope, options);
            aborted(kind, message, index, abort)
        }
    }
}

/// `type=module` のエントリ 1 件を、グラフの解決から evaluate まで実行する。
///
/// 呼び出し元: [`super::execute`] の実行ループ。取得・検査・上限は module 上部の doc のとおり。
/// 取得に失敗しても（`SrcFetchFailed`）例外でも後続の実行は続け、上限・実時間・エンジン不能は
/// `abort` に理由を入れて打ち切る。
pub(super) async fn run_module_entry(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    source: &ScriptSource,
    index: usize,
    abort: &mut Option<PageAbort>,
) -> ScriptOutcome {
    let root = match load_graph(rt, ctx, graph, source).await {
        Ok(id) => id,
        Err(GraphError::Failed(ModuleFailure::Fetch(kind))) => {
            return ScriptOutcome::SrcFetchFailed { kind };
        }
        Err(GraphError::Failed(ModuleFailure::Exception { message, truncated })) => {
            return ScriptOutcome::Exception { message, truncated };
        }
        Err(GraphError::Unsupported) => return ScriptOutcome::ModuleUnsupported,
        Err(GraphError::Abort(kind, message)) => return aborted(kind, message, index, abort),
    };
    if let Some(state) = graph.evaluated.get(&root) {
        // 既に evaluate 済みの module（同じ `src` の再指定等）は二度実行せず、最初の結果を返す
        // （例外は再提示し、保留は成功と見なさない）。
        return match state {
            EvalState::Succeeded => ScriptOutcome::Succeeded,
            EvalState::Pending => ScriptOutcome::ModulePending,
            EvalState::Rejected { message, truncated } => ScriptOutcome::Exception {
                message: message.clone(),
                truncated: *truncated,
            },
        };
    }
    evaluate_root(rt, ctx, graph, root, index, abort)
}

#[cfg(test)]
mod tests;
