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
//! # 動的 `import()`（Issue #1117）
//!
//! イベントループ段が [`pump_one_dynamic_import`] を 1 件ずつ呼ぶ。specifier を referrer の URL に
//! 対して解決し、静的 import と同じ経路（`Fetcher`・MIME・同一オリジン・重複排除・上限）で対象を
//! 取得・compile・link して `finish_dynamic_import` で完了させる。完了にはページの残り時間を渡し、
//! 戻り値の `import()` の Promise の確定結果（resolve・reject・未確定）で件数を数える。失敗は `Err` で reject し、
//! メッセージは固定文字列 `dynamic import failed: <種別>` だけ（URL・specifier を含めない）。
//! 件数・総バイト・深さ・実時間の超過は reject したうえで `AbortKind` に写してページを打ち切る。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - 同一オリジンの module だけを取得する（設計 §6 の案 (a)）。別オリジンを許す案 (b)・(c) と
//!   `Origin` ヘッダの扱いは未対応。別オリジンは [`SrcFetchErrorKind::CrossOrigin`]。
//! - 応答の MIME は JavaScript MIME type の essence に限る（Content-Type が無い応答も拒否）。
//!   classic の `src` が Content-Type を検査しないこととの差異は意図的（module は MIME 検査が必須）。
//! - `compile_module` は期限を引数に取れない（エンジンの既定値）。compile の前後でページ時計を確認する。
//! - 動的 `import()`: `referrer=None`（classic スクリプトからの呼び出し）は外部スクリプトの URL では
//!   なく文書の base URL を基準に解決する近似。動的 import の対象は新しい root（深さ 0）として扱い、
//!   連鎖は `max_modules` と [`super::MAX_PAGE_DYNAMIC_IMPORTS`] で抑える。V8 の
//!   `finish_dynamic_import` は内部で microtask checkpoint を回すため、上限で打ち切る場合も
//!   `catch` ハンドラが 1 回だけ走り得る（実行時間は残り時間で打ち切る）。評価済み entry の `ScriptOutcome::ModulePending` は
//!   動的 import の完了後に遡って更新しない。
//! - module の評価中は `document.currentScript` を設定しない（WHATWG では module は null）。
//!   実行順は classic と同じ文書順（`async` / `defer` の順序近似は行わない）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use fandhe_browser_js::{
    DynamicImportRequest, EvaluateOptions, JsEngineError, ModuleEvaluation, ModuleId,
    ModuleResolution,
};
use reqwest::Url;

use super::{
    AbortKind, DynamicImportFailureKind, DynamicImportReport, MAX_MODULE_GRAPH_DEPTH,
    MAX_PAGE_DYNAMIC_IMPORTS, PageAbort, RunContext, ScriptBytesScope, ScriptOutcome, SrcFailure,
    SrcFetchErrorKind, WallTimeScope, check_src_response, classify_fetch_error, engine_error_abort,
    lossy_len_exceeds, microtask_checkpoint, page_wall_abort, script_budget, tripped_abort_kind,
};
use crate::error::Error;
use crate::fetch::Fetcher;
use crate::js_stub::JsRuntime;
use crate::page_script::ScriptSource;

/// 深さ超過の打ち切りに付ける固定メッセージ（URL を含めない）。
const DEPTH_EXCEEDED_MESSAGE: &str = "module graph depth limit exceeded";

/// module の構文・リンク・評価の失敗に付ける固定メッセージ（`JS-8`・`SEC-2`）。
///
/// エンジンの生メッセージは specifier・URL・クエリを含み得る（V8 の instantiate エラー等）ため
/// 診断には保存せず、失敗の種別だけを残す。
const MODULE_COMPILE_FAILED_MESSAGE: &str = "module compile failed";
const MODULE_EVALUATION_FAILED_MESSAGE: &str = "module evaluation failed";

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
    /// compile 済みモジュールの referrer URL（動的 `import()` の解決基準。件数は `max_modules` で頭打ち）。
    referrers: HashMap<ModuleId, Url>,
    /// これまでに処理を始めた動的 `import()` の要求数（[`MAX_PAGE_DYNAMIC_IMPORTS`] で頭打ち）。
    dynamic_requests: usize,
}

/// evaluate 済み root の結果（再参照時に同じ結果を返すために保持する）。
#[derive(Debug, Clone)]
enum EvalState {
    Succeeded,
    /// 保留（top-level await 等）。再参照でも成功とは見なさない。
    Pending,
    /// 評価の失敗。内容は固定メッセージで返す（生メッセージは保持しない）。
    Rejected,
}

impl ModuleGraph {
    pub(super) fn new() -> Self {
        Self {
            map: HashMap::new(),
            attempted: 0,
            classic_bytes: 0,
            module_bytes: 0,
            evaluated: HashMap::new(),
            referrers: HashMap::new(),
            dynamic_requests: 0,
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
        Err(Error::JsEvaluation(JsEngineError::EvaluationFailed(_))) => {
            if page_expired {
                return Err(page_clock_abort(ctx));
            }
            return Err(ModuleFailure::Exception {
                message: MODULE_COMPILE_FAILED_MESSAGE.to_string(),
                truncated: false,
            }
            .into());
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
    graph.referrers.insert(id, final_url);
    Ok(ReadyModule {
        id,
        deps,
        linked: false,
    })
}

/// グラフの root。
enum GraphRoot<'a> {
    /// inline module の本文（マップに登録しない）。
    Inline(&'a str),
    /// URL で指す module（`src`・動的 `import()` の対象）。
    Url { specifier: &'a str, url: Url },
}

/// entry のグラフを取得・compile・link し、root の id を返す。
async fn load_graph(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    source: &ScriptSource,
) -> Result<ModuleId, GraphError> {
    let root = match source {
        ScriptSource::Inline(text) => GraphRoot::Inline(text),
        ScriptSource::External(src) => GraphRoot::Url {
            specifier: src,
            url: ctx
                .base
                .join(src)
                .map_err(|_| ModuleFailure::Fetch(SrcFetchErrorKind::InvalidUrl))?,
        },
    };
    load_graph_root(rt, ctx, graph, root).await
}

/// `root` のグラフを取得・compile・link し、root の id を返す（静的 entry と動的 `import()` 共通）。
async fn load_graph_root(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    root: GraphRoot<'_>,
) -> Result<ModuleId, GraphError> {
    let mut queue: VecDeque<(Dep, usize)> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut to_link: Vec<String> = Vec::new();
    // inline の root はマップに登録しない（同じ base URL の inline module が複数あり得るため）。
    let mut inline_root: Option<(ModuleId, Vec<Dep>)> = None;
    let mut root_key: Option<String> = None;

    match root {
        GraphRoot::Inline(text) => {
            reserve_module_slot(ctx, graph)?;
            add_module_bytes(ctx, graph, text.len())?;
            let (id, deps) = compile_source(rt, ctx, ctx.base, text)?;
            graph.referrers.insert(id, ctx.base.clone());
            for dep in &deps {
                queue.push_back((dep.clone(), 1));
            }
            inline_root = Some((id, deps));
        }
        GraphRoot::Url { specifier, url } => {
            let (key, url) = module_key(url);
            root_key = Some(key.clone());
            queue.push_back((
                Dep {
                    specifier: specifier.to_string(),
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

    // 取得の重複排除（最短距離での判定）とは別に、依存経路の最長の深さで上限を検査する。
    let starts: Vec<(String, usize)> = match (&inline_root, &root_key) {
        (Some((_, deps)), _) => deps.iter().map(|d| (d.key.clone(), 1)).collect(),
        (None, Some(k)) => vec![(k.clone(), 0)],
        _ => Vec::new(),
    };
    if longest_depth(graph, &starts) > MAX_MODULE_GRAPH_DEPTH {
        return Err(GraphError::Abort(
            AbortKind::ResourceLimit,
            Some(DEPTH_EXCEEDED_MESSAGE.to_string()),
        ));
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

/// `starts`（キーと基準の深さ）から辿れる依存経路の最長の深さを返す。
///
/// 循環（強連結成分）は 1 つの塊に縮約し、塊の深さへの寄与は所属 module 数とする
/// （循環内の単純経路の長さの上界。過小評価しない側＝fail-closed）。縮約後は DAG なので、
/// 反復版 Tarjan の 1 回の走査で成分ごとの高さを O(V+E) で求める。単純経路の列挙は
/// 完全相互 import で階乗規模になり、ホストスレッドを占有できるため行わない（`JS-6`）。
fn longest_depth(graph: &ModuleGraph, starts: &[(String, usize)]) -> usize {
    struct Frame<'a> {
        key: &'a str,
        idx: usize,
    }
    fn ready_deps<'a>(graph: &'a ModuleGraph, key: &str) -> &'a [Dep] {
        match graph.map.get(key) {
            Some(ModuleSlot::Ready(r)) => &r.deps,
            _ => &[],
        }
    }
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    let mut low: HashMap<&str, usize> = HashMap::new();
    // 成分に確定したノードの成分の高さ（module 数で数える）。
    let mut comp_height: HashMap<&str, usize> = HashMap::new();
    let mut scc_stack: Vec<&str> = Vec::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut next_index = 0usize;
    let mut deepest = 0usize;
    for (start, base) in starts {
        if !matches!(graph.map.get(start.as_str()), Some(ModuleSlot::Ready(_))) {
            continue;
        }
        if !index_of.contains_key(start.as_str()) {
            let mut calls: Vec<Frame<'_>> = vec![Frame {
                key: start.as_str(),
                idx: 0,
            }];
            index_of.insert(start.as_str(), next_index);
            low.insert(start.as_str(), next_index);
            next_index += 1;
            scc_stack.push(start.as_str());
            on_stack.insert(start.as_str());
            while let Some(top) = calls.last_mut() {
                let key = top.key;
                let idx = top.idx;
                top.idx += 1;
                if let Some(dep) = ready_deps(graph, key).get(idx) {
                    let child = dep.key.as_str();
                    if !matches!(graph.map.get(child), Some(ModuleSlot::Ready(_))) {
                        continue;
                    }
                    if let Some(&ci) = index_of.get(child) {
                        if on_stack.contains(child) {
                            let l = low.get(key).copied().unwrap_or(usize::MAX).min(ci);
                            low.insert(key, l);
                        }
                    } else {
                        index_of.insert(child, next_index);
                        low.insert(child, next_index);
                        next_index += 1;
                        scc_stack.push(child);
                        on_stack.insert(child);
                        calls.push(Frame { key: child, idx: 0 });
                    }
                    continue;
                }
                // 全ての辺を見終えた。
                calls.pop();
                let key_low = low.get(key).copied().unwrap_or(usize::MAX);
                if let Some(parent) = calls.last() {
                    let l = low
                        .get(parent.key)
                        .copied()
                        .unwrap_or(usize::MAX)
                        .min(key_low);
                    low.insert(parent.key, l);
                }
                if key_low == index_of.get(key).copied().unwrap_or(usize::MAX) {
                    let mut members: Vec<&str> = Vec::new();
                    while let Some(m) = scc_stack.pop() {
                        on_stack.remove(m);
                        members.push(m);
                        if m == key {
                            break;
                        }
                    }
                    let mut child_max = 0usize;
                    for m in &members {
                        for d in ready_deps(graph, m) {
                            if let Some(&h) = comp_height.get(d.key.as_str())
                                && !members.contains(&d.key.as_str())
                            {
                                child_max = child_max.max(h);
                            }
                        }
                    }
                    let height = members.len().saturating_add(child_max);
                    for m in members {
                        comp_height.insert(m, height);
                    }
                }
            }
        }
        // 高さは module 数。深さは辺の数なので 1 引く。
        let h = comp_height.get(start.as_str()).copied().unwrap_or(1);
        deepest = deepest.max(base.saturating_add(h.saturating_sub(1)));
    }
    deepest
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

/// 評価失敗の結果（URL を含まない固定メッセージ）。
fn rejected_outcome() -> ScriptOutcome {
    ScriptOutcome::Exception {
        message: MODULE_EVALUATION_FAILED_MESSAGE.to_string(),
        truncated: false,
    }
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
        Ok(ModuleEvaluation::Rejected { .. }) => {
            graph.evaluated.insert(root, EvalState::Rejected);
            rejected_outcome()
        }
        // 未知の結果は成功と見なさない（fail-closed）。
        Ok(_) => aborted(AbortKind::EngineError, None, index, abort),
        Err(Error::JsEvaluation(JsEngineError::NotYetImplemented(_))) => {
            ScriptOutcome::ModuleUnsupported
        }
        // 拒否メッセージの上限超過等。classic の `evaluate_with_budget` と同様に
        // 固定メッセージの例外として記録し、後続を続ける（ページ時計が尽きていれば打ち切り）。
        Err(Error::JsEvaluation(JsEngineError::EvaluationFailed(_))) => {
            if page_expired {
                aborted(page_wall_abort(options, None).kind, None, index, abort)
            } else {
                graph.evaluated.insert(root, EvalState::Rejected);
                rejected_outcome()
            }
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
        // 既に evaluate 済みの module（同じ `src` の再指定等）は二度実行せず、確定した結果を返す
        // （例外は再提示する）。保留は確定結果ではない（後続の classic が TLA の待ちを解く等）ため、
        // 再参照ではエンジン側の現在の評価状態を取り直し、確定したらキャッシュを更新する
        // （`JS-4`。エンジンは evaluate 済みの root に対し現在の Promise の状態を返す）。
        match state {
            EvalState::Succeeded => return ScriptOutcome::Succeeded,
            EvalState::Rejected => return rejected_outcome(),
            EvalState::Pending => {}
        }
    }
    evaluate_root(rt, ctx, graph, root, index, abort)
}

/// 動的 `import()` の失敗を `finish_dynamic_import` に渡す固定メッセージにする（URL を含めない）。
fn failure_message(kind: &DynamicImportFailureKind) -> String {
    format!("dynamic import failed: {}", kind.as_str())
}

/// 要求 1 件の対象を解決・取得・compile・link し、対象の id を返す（`JS-4`・`JS-8`）。
async fn load_dynamic_target(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    request: &DynamicImportRequest,
) -> Result<ModuleId, DynamicImportError> {
    // `referrer=None` は classic スクリプト。文書の base URL で近似する（REPAIR-3）。
    let referrer = match request.referrer() {
        None => ctx.base.clone(),
        Some(id) => graph
            .referrers
            .get(&id)
            .cloned()
            .ok_or(DynamicImportError::UnknownReferrer)?,
    };
    let url = resolve_specifier(&referrer, request.specifier())
        .map_err(|kind| DynamicImportError::Graph(ModuleFailure::Fetch(kind).into()))?;
    load_graph_root(
        rt,
        ctx,
        graph,
        GraphRoot::Url {
            specifier: request.specifier(),
            url,
        },
    )
    .await
    .map_err(DynamicImportError::Graph)
}

/// [`load_dynamic_target`] の失敗。
enum DynamicImportError {
    Graph(GraphError),
    UnknownReferrer,
}

/// `import()` の Promise の確定結果と、対象をエンジンが受け付けたか（[`call_finish`] の戻り値）。
struct Finished {
    /// microtask checkpoint 後の `import()` の Promise の状態。
    settlement: ModuleEvaluation,
    /// `Ok` の対象をエンジンが受け付けたか（受け付けなかったときは `Err` で完了し直している）。
    delivered: bool,
}

/// `finish_dynamic_import` を、ページの残り時間を課して呼ぶ（`JS-6`・Issue #1117）。
///
/// 残り時間（`script_budget`）を `EvaluateOptions` の timeout としてエンジンへ渡し、動的 module の
/// 評価と `then` / `catch` の checkpoint 全体をその予算で停止させる。`Ok` の対象を engine が
/// 受け付けない（`ModuleResolutionFailed`）ときは同じ予算で 1 回だけ `Err` で完了し直し、
/// `delivered = false` を返す（呼び出し側は reject として数える）。戻り値の `settlement` は
/// `import()` の Promise の実際の確定結果で、通知を受理しただけでは成功と見なさない。
fn call_finish(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    id: fandhe_browser_js::DynamicImportId,
    result: Result<ModuleId, String>,
) -> Result<Finished, GraphError> {
    let Some((timeout, scope)) = script_budget(
        ctx.options.script_wall_time,
        ctx.options.page_wall_time,
        ctx.page_start.elapsed(),
    ) else {
        return Err(page_clock_abort(ctx));
    };
    let eval_options = EvaluateOptions::default().with_timeout(timeout);
    let was_ok = result.is_ok();
    let mut delivered = true;
    let mut outcome = rt.finish_dynamic_import(id, result, &eval_options);
    if was_ok
        && matches!(
            outcome,
            Err(Error::JsEvaluation(JsEngineError::ModuleResolutionFailed(
                _
            )))
        )
    {
        delivered = false;
        let kind = DynamicImportFailureKind::Fetch(SrcFetchErrorKind::Other);
        // 1 回目で予算を使った可能性があるため、残りを取り直す（尽きていれば打ち切り）。
        let Some((timeout, _)) = script_budget(
            ctx.options.script_wall_time,
            ctx.options.page_wall_time,
            ctx.page_start.elapsed(),
        ) else {
            return Err(page_clock_abort(ctx));
        };
        outcome = rt.finish_dynamic_import(
            id,
            Err(failure_message(&kind)),
            &EvaluateOptions::default().with_timeout(timeout),
        );
    }
    let page_expired = ctx.page_start.elapsed() > ctx.options.page_wall_time;
    match outcome {
        Ok(_) if page_expired => Err(page_clock_abort(ctx)),
        Ok(settlement) => Ok(Finished {
            settlement,
            delivered,
        }),
        Err(Error::JsEvaluation(JsEngineError::NotYetImplemented(_))) => {
            Err(GraphError::Unsupported)
        }
        Err(e) => {
            let (kind, message) = engine_error_abort(&e, scope, ctx.options);
            Err(GraphError::Abort(kind, message))
        }
    }
}

/// 動的 `import()` を 1 件取得して完了させ、microtask checkpoint を回す（`JS-4`・`JS-6`・`JS-8`・`SEC-2`）。
///
/// 呼び出し元: [`super::run_event_loop`]（gate 有効のページだけ）。継続してよければ `true`、
/// 打ち切った（`abort` を設定した）なら `false`。上限・実時間の超過は reject してから打ち切る
/// （ページの実時間が尽きた場合は JS を走らせる予算が無いため reject せずに打ち切る）。
/// 評価する文字列は固定の関数名だけで、specifier・URL は埋め込まない。
pub(super) async fn pump_one_dynamic_import(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    graph: &mut ModuleGraph,
    request: &DynamicImportRequest,
    report: &mut DynamicImportReport,
    abort: &mut Option<PageAbort>,
) -> crate::Result<bool> {
    report.requested = report.requested.saturating_add(1);
    let id = request.id();
    // 失敗の種別と、打ち切りの理由（打ち切るときだけ）。
    let verdict: Result<ModuleId, (DynamicImportFailureKind, Option<AbortKind>)> =
        if graph.dynamic_requests >= MAX_PAGE_DYNAMIC_IMPORTS {
            Err((
                DynamicImportFailureKind::Limit(AbortKind::ResourceLimit),
                Some(AbortKind::ResourceLimit),
            ))
        } else {
            graph.dynamic_requests = graph.dynamic_requests.saturating_add(1);
            match load_dynamic_target(rt, ctx, graph, request).await {
                Ok(target) => Ok(target),
                Err(DynamicImportError::UnknownReferrer) => {
                    Err((DynamicImportFailureKind::UnknownReferrer, None))
                }
                Err(DynamicImportError::Graph(GraphError::Failed(f))) => Err((
                    match f {
                        ModuleFailure::Fetch(kind) => DynamicImportFailureKind::Fetch(kind),
                        ModuleFailure::Exception { .. } => DynamicImportFailureKind::CompileFailed,
                    },
                    None,
                )),
                Err(DynamicImportError::Graph(GraphError::Unsupported)) => {
                    Err((DynamicImportFailureKind::Unsupported, None))
                }
                Err(DynamicImportError::Graph(GraphError::Abort(kind, _))) => {
                    Err((DynamicImportFailureKind::Limit(kind.clone()), Some(kind)))
                }
            }
        };

    let result = match &verdict {
        Ok(target) => Ok(*target),
        Err((kind, _)) => Err(failure_message(kind)),
    };
    let page_wall = matches!(
        &verdict,
        Err((
            _,
            Some(AbortKind::WallTime {
                scope: WallTimeScope::Page,
                ..
            })
        ))
    );
    // ページの実時間が尽きたときは JS を走らせない。それ以外は reject してから打ち切る。
    let finished = if page_wall {
        Err(page_clock_abort(ctx))
    } else {
        call_finish(rt, ctx, id, result)
    };
    match (&verdict, finished) {
        (Err((kind, Some(abort_kind))), _) => {
            report.rejected = report.rejected.saturating_add(1);
            report.failures.push(kind.clone());
            // finish が失敗しても、打ち切りの理由は元の超過を優先する。
            *abort = Some(PageAbort {
                kind: abort_kind.clone(),
                index: None,
            });
            return Ok(false);
        }
        (_, Err(GraphError::Abort(kind, _))) => {
            // 評価中の予算超過など、エンジンが reject して打ち切ったものは reject として数える。
            // ページの実時間切れは JS を走らせず reject もしないため数えない。
            if !matches!(
                &kind,
                AbortKind::WallTime {
                    scope: WallTimeScope::Page,
                    ..
                }
            ) {
                report.rejected = report.rejected.saturating_add(1);
                report
                    .failures
                    .push(DynamicImportFailureKind::Limit(kind.clone()));
            }
            *abort = Some(PageAbort { kind, index: None });
            return Ok(false);
        }
        (_, Err(_)) => {
            *abort = Some(PageAbort {
                kind: AbortKind::EngineError,
                index: None,
            });
            return Ok(false);
        }
        (Ok(_), Ok(finished)) if !finished.delivered => {
            report.rejected = report.rejected.saturating_add(1);
            report
                .failures
                .push(DynamicImportFailureKind::Fetch(SrcFetchErrorKind::Other));
        }
        // 実際の Promise の確定結果で数える（通知を受理しただけでは成功にしない）。
        (Ok(_), Ok(finished)) => match finished.settlement {
            ModuleEvaluation::Fulfilled => report.fulfilled = report.fulfilled.saturating_add(1),
            ModuleEvaluation::Pending => report.pending = report.pending.saturating_add(1),
            // 評価の例外（Rejected）と未知の確定結果は fail-closed で reject として数える。
            _ => {
                report.rejected = report.rejected.saturating_add(1);
                report
                    .failures
                    .push(DynamicImportFailureKind::EvaluationFailed);
            }
        },
        (Err((kind, None)), Ok(_)) => {
            report.rejected = report.rejected.saturating_add(1);
            report.failures.push(kind.clone());
        }
    }
    // shim は op のエラー（操作予算切れ）を握りつぶすため、先に検出して打ち切る（`JS-6`）。
    if let Some(kind) = tripped_abort_kind(ctx)? {
        *abort = Some(PageAbort { kind, index: None });
        return Ok(false);
    }
    // `then` / `catch` の反応内の `queueMicrotask` を実行する。
    if let Some((kind, _)) = microtask_checkpoint(rt, ctx) {
        *abort = Some(PageAbort { kind, index: None });
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
