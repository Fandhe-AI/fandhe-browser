//! boa の ES module 評価を支える部品（`JS-4`・`JS-6`・`REPAIR-3`・Issue #1114。
//! 設計 `docs/design/es-module-design.md` §5）。
//!
//! 呼び出し元: 親モジュール [`super`]（`BoaEngine` の `compile_module`・
//! `link_module`・`evaluate_module`）だけ。boa の具象型（`Module`・`Context`）は
//! `boa_engine` モジュールの外へ出さない（coding-rust.md）。
//!
//! # 構成
//!
//! - [`analyze_module_source`]: ソースを別途パースし、依存の specifier を集め、
//!   動的 `import()`・top-level await（TLA）を compile 時に拒否する
//! - [`BoaModuleLoader`]: ファイル・ネットワークに触れない map ローダー。親が
//!   `LINK_MODULE` で渡した compile 済みモジュールしか返さない（boa の既定ローダー
//!   `SimpleModuleLoader::new(".")` は cwd 起点のファイル読み込みをするため、
//!   `Context` の生成時に必ずこれへ置き換える。`JS-8`・SSRF/任意ファイル読み込みの防止）
//! - [`ModuleAwareJobExecutor`]: モジュールのグラフロードが積む `NativeAsyncJob` だけを
//!   捕まえて走らせる job 実行器。ページ全体の job drain は行わない
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - 動的 `import()`（関数の内側を含む）と TLA を含むモジュールは compile が
//!   `NotYetImplemented`（`TAKE_DYNAMIC_IMPORTS`・`FINISH_DYNAMIC_IMPORT` も未対応）。
//!   将来仕様は設計 §8 の動的 import と TLA の対応（boa 側は未着手）
//! - job はモジュールロード用の `NativeAsyncJob` だけを走らせる。モジュール本体が
//!   積んだ `PromiseJob`（`Promise.resolve().then(...)` 等）は classic 評価と同じく
//!   走らない（ページ全体の job drain は TASK-30 の残課題）
//! - `import.meta.url` は設定しない（`import.meta` は空オブジェクト。V8 側は #1112）
//! - import attributes（`with { type: "json" }`）を付けた import は拒否する
//! - ソースは検出用と本体用の 2 回パースする（入力は 1 MiB 以下）

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};

use boa_engine::ast::expression::ImportCall;
use boa_engine::ast::expression::literal::ObjectMethodDefinition;
use boa_engine::ast::function::{
    ArrowFunction, AsyncArrowFunction, AsyncFunctionDeclaration, AsyncFunctionExpression,
    AsyncGeneratorDeclaration, AsyncGeneratorExpression, ClassDeclaration, ClassExpression,
    FunctionDeclaration, FunctionExpression, GeneratorDeclaration, GeneratorExpression,
};
use boa_engine::ast::operations::{ContainsSymbol, contains};
use boa_engine::ast::scope::Scope;
use boa_engine::ast::statement::iteration::ForOfLoop;
use boa_engine::ast::visitor::{VisitWith, Visitor};
use boa_engine::builtins::promise::PromiseState;
use boa_engine::interner::Interner;
use boa_engine::job::{Job, JobExecutor, NativeAsyncJob, SimpleJobExecutor};
use boa_engine::module::{Module, ModuleLoader, ModuleRequest as BoaModuleRequest, Referrer};
use boa_engine::parser::{Parser, Source};
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsValue as BoaValue};

use crate::engine_trait::{JsEngineError, ModuleId, ModuleResolution};
use crate::worker_protocol::module_frames::{
    MAX_MODULE_REQUESTS, MAX_MODULE_SPECIFIER_BYTES, MAX_MODULES_PER_WORKER,
};

/// グラフロード用 job を走らせる最大回数。1 モジュールが `MAX_MODULE_REQUESTS`
/// 件の job を積み、モジュール数は `MAX_MODULES_PER_WORKER` で抑えられるため、
/// 正常系では超えない（超えたら fail-closed で空回りさせない。OWASP A04）。
const MAX_LOAD_JOB_ITERATIONS: usize = MAX_MODULES_PER_WORKER * MAX_MODULE_REQUESTS;

// ---- ソースの検査 ----

/// 動的 `import()` を見つけたら走査を打ち切る visitor。関数の内側も走査する
/// （boa には対応する `ContainsSymbol` が無い）。
struct ImportCallFinder;

impl<'ast> Visitor<'ast> for ImportCallFinder {
    type BreakTy = ();

    fn visit_import_call(&mut self, _node: &'ast ImportCall) -> ControlFlow<()> {
        ControlFlow::Break(())
    }
}

/// トップレベルの `for await (... of ...)` を見つけたら打ち切る visitor。
///
/// boa 自身の TLA 判定（`ContainsSymbol::AwaitExpression`）は `await` 式しか数えず、
/// トップレベルの `for await` を見落とす（`has_tla` が偽になる）ため補う。関数・
/// クラス・オブジェクトメソッドの内側は数えない（それらは自前の async 境界を持つ）。
struct TopLevelForAwaitFinder;

macro_rules! skip_function_boundary {
    ($($method:ident: $node:ty),* $(,)?) => {
        $(
            fn $method(&mut self, _node: &'ast $node) -> ControlFlow<()> {
                ControlFlow::Continue(())
            }
        )*
    };
}

impl<'ast> Visitor<'ast> for TopLevelForAwaitFinder {
    type BreakTy = ();

    fn visit_for_of_loop(&mut self, node: &'ast ForOfLoop) -> ControlFlow<()> {
        if node.r#await() {
            return ControlFlow::Break(());
        }
        node.visit_with(self)
    }

    skip_function_boundary! {
        visit_function_expression: FunctionExpression,
        visit_function_declaration: FunctionDeclaration,
        visit_generator_expression: GeneratorExpression,
        visit_generator_declaration: GeneratorDeclaration,
        visit_async_function_expression: AsyncFunctionExpression,
        visit_async_function_declaration: AsyncFunctionDeclaration,
        visit_async_generator_expression: AsyncGeneratorExpression,
        visit_async_generator_declaration: AsyncGeneratorDeclaration,
        visit_arrow_function: ArrowFunction,
        visit_async_arrow_function: AsyncArrowFunction,
        visit_class_expression: ClassExpression,
        visit_class_declaration: ClassDeclaration,
        visit_object_method_definition: ObjectMethodDefinition,
    }
}

/// モジュールのソースを検出用にパースし、依存の specifier（出現順・重複なし）を返す。
///
/// 次の場合は `Err`（モジュールは登録しない）:
///
/// - 構文エラー → `EvaluationFailed`
/// - TLA を含む → `NotYetImplemented`（boa 自身の `has_tla` と同じ判定）
/// - 動的 `import()` を含む（関数の内側を含む）→ `NotYetImplemented`
/// - 依存が `MAX_MODULE_REQUESTS` 件超、または specifier が
///   `MAX_MODULE_SPECIFIER_BYTES` 超 → `ResourceLimitExceeded`（文言に specifier を
///   入れない）
/// - specifier が整形式の UTF-16 でない（lone surrogate）→ `EvaluationFailed`
pub(super) fn analyze_module_source(source: &str) -> Result<Vec<String>, JsEngineError> {
    let mut interner = Interner::default();
    let scope = Scope::new_global();
    let module = Parser::new(Source::from_bytes(source))
        .parse_module(&scope, &mut interner)
        .map_err(|err| {
            JsEngineError::EvaluationFailed(super::truncate_message(&format!(
                "module syntax error: {err}"
            )))
        })?;

    if contains(module.items(), ContainsSymbol::AwaitExpression)
        || TopLevelForAwaitFinder.visit_module(&module).is_break()
    {
        return Err(JsEngineError::NotYetImplemented(
            "compile_module: top-level await is not supported by the boa engine yet".to_string(),
        ));
    }
    if ImportCallFinder.visit_module(&module).is_break() {
        return Err(JsEngineError::NotYetImplemented(
            "compile_module: dynamic import() is not supported by the boa engine yet".to_string(),
        ));
    }

    let symbols = module.items().requests();
    if symbols.len() > MAX_MODULE_REQUESTS {
        return Err(JsEngineError::ResourceLimitExceeded(format!(
            "module has {} dependencies, exceeding the maximum of {MAX_MODULE_REQUESTS}",
            symbols.len()
        )));
    }
    let mut requests = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let resolved = interner.resolve(symbol).and_then(|text| {
            text.join(
                |utf8| Some(utf8.to_string()),
                |utf16| String::from_utf16(utf16).ok(),
                true,
            )
        });
        let specifier = resolved.ok_or_else(|| {
            JsEngineError::EvaluationFailed(
                "module specifier is not a well-formed string".to_string(),
            )
        })?;
        if specifier.len() > MAX_MODULE_SPECIFIER_BYTES {
            return Err(JsEngineError::ResourceLimitExceeded(format!(
                "module specifier exceeds the maximum of {MAX_MODULE_SPECIFIER_BYTES} bytes"
            )));
        }
        requests.push(specifier);
    }
    Ok(requests)
}

// ---- ローダー ----

/// compile 済みモジュール 1 件の記録。
struct ModuleEntry {
    module: Module,
    /// ソースに書かれた依存（出現順）。
    requests: Vec<String>,
    /// link 済みの specifier → 対象。
    links: HashMap<String, ModuleId>,
}

/// ローダーが保持するモジュール表。
#[derive(Default)]
struct LoaderState {
    entries: HashMap<ModuleId, ModuleEntry>,
    ids: HashMap<Module, ModuleId>,
}

/// ローダーが import を拒否する理由（判定の純粋関数 [`resolve_request`] の結果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadRejection {
    /// 静的グラフのロード中でない（評価中の動的 `import()` など）。
    OutsideStaticPhase,
    /// referrer がこのローダーに登録されたモジュールでない。
    UnknownReferrer,
    /// import attributes 付き（JSON・CSS module は範囲外）。
    ImportAttributes,
    /// specifier が link されていない（または文字列に変換できない）。
    Unresolved,
}

impl LoadRejection {
    /// JS 側へ投げる固定文言。specifier・URL を含めない。
    fn message(self) -> &'static str {
        match self {
            Self::OutsideStaticPhase => "dynamic import() is not supported by the boa engine yet",
            Self::UnknownReferrer => "module referrer is not registered",
            Self::ImportAttributes => "import attributes are not supported by the boa engine yet",
            Self::Unresolved => "module specifier is not linked",
        }
    }
}

/// import 1 件を解決できるかの判定（純粋関数。ローダー本体と単体テストが共有する）。
fn resolve_request(
    state: &LoaderState,
    static_phase: bool,
    referrer: Option<ModuleId>,
    has_attributes: bool,
    specifier: Option<&str>,
) -> Result<Module, LoadRejection> {
    if !static_phase {
        return Err(LoadRejection::OutsideStaticPhase);
    }
    let entry = referrer
        .and_then(|id| state.entries.get(&id))
        .ok_or(LoadRejection::UnknownReferrer)?;
    if has_attributes {
        return Err(LoadRejection::ImportAttributes);
    }
    let target = specifier
        .and_then(|text| entry.links.get(text))
        .and_then(|id| state.entries.get(id))
        .ok_or(LoadRejection::Unresolved)?;
    Ok(target.module.clone())
}

/// ファイルシステム・ネットワークに触れない map ローダー。
///
/// 親が `LINK_MODULE` で渡した対応表（referrer の specifier → compile 済み
/// モジュール）だけで解決する。状態は内部可変性（`RefCell` / `Cell`）で持ち、
/// `Rc` を [`super::BoaEngine`] と boa の `Context` が共有する。
pub(super) struct BoaModuleLoader {
    state: RefCell<LoaderState>,
    /// 静的グラフのロード中だけ真。評価中の動的 `import()` を実行時にも止める
    /// 多層防御（compile 時の拒否に加えて）。
    static_phase: Cell<bool>,
    /// 直近のロードで「未 link の specifier」による拒否が起きたか。
    unresolved_hit: Cell<bool>,
}

impl BoaModuleLoader {
    pub(super) fn new() -> Self {
        Self {
            state: RefCell::new(LoaderState::default()),
            static_phase: Cell::new(false),
            unresolved_hit: Cell::new(false),
        }
    }

    /// 登録済みのモジュール数。
    pub(super) fn len(&self) -> usize {
        self.state.borrow().entries.len()
    }

    /// compile 済みモジュールを登録する。
    pub(super) fn insert(&self, id: ModuleId, module: Module, requests: Vec<String>) {
        let mut state = self.state.borrow_mut();
        state.ids.insert(module.clone(), id);
        state.entries.insert(
            id,
            ModuleEntry {
                module,
                requests,
                links: HashMap::new(),
            },
        );
    }

    /// `referrer` の依存を対応表どおりに link する。いずれの不整合も
    /// `ModuleResolutionFailed`（固定文言）で、検査が全件通った後にだけ表を書き換える。
    ///
    /// boa は同じ (referrer, specifier) に別の `Module` を返すと `assert_eq!` で
    /// panic する（release は abort）ため、link 済みの specifier を別の対象へ
    /// 張り替える要求は拒否する。同じ対象での再 link は冪等に受け付ける。
    pub(super) fn link(
        &self,
        referrer: ModuleId,
        resolutions: &[ModuleResolution],
    ) -> Result<(), JsEngineError> {
        let mut state = self.state.borrow_mut();
        let entry = state.entries.get(&referrer).ok_or_else(|| {
            JsEngineError::ModuleResolutionFailed("referrer module is not registered".to_string())
        })?;
        for resolution in resolutions {
            if !state.entries.contains_key(&resolution.target()) {
                return Err(JsEngineError::ModuleResolutionFailed(
                    "link target module is not registered".to_string(),
                ));
            }
            if !entry
                .requests
                .iter()
                .any(|request| request == resolution.specifier())
            {
                return Err(JsEngineError::ModuleResolutionFailed(
                    "specifier is not a dependency of the module".to_string(),
                ));
            }
            if let Some(existing) = entry.links.get(resolution.specifier())
                && *existing != resolution.target()
            {
                return Err(JsEngineError::ModuleResolutionFailed(
                    "specifier is already linked to a different module".to_string(),
                ));
            }
        }
        if let Some(entry) = state.entries.get_mut(&referrer) {
            for resolution in resolutions {
                entry
                    .links
                    .insert(resolution.specifier().to_string(), resolution.target());
            }
        }
        Ok(())
    }

    /// `root` から到達できる全モジュールの全依存が link 済みかを確認し、`root` の
    /// `Module` を返す。未知の `root`・未解決の依存は `ModuleResolutionFailed`。
    pub(super) fn root_module_if_fully_linked(
        &self,
        root: ModuleId,
    ) -> Result<Module, JsEngineError> {
        let state = self.state.borrow();
        let root_entry = state.entries.get(&root).ok_or_else(|| {
            JsEngineError::ModuleResolutionFailed("module is not registered".to_string())
        })?;
        let mut visited: HashSet<ModuleId> = HashSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let entry = state.entries.get(&id).ok_or_else(|| {
                JsEngineError::ModuleResolutionFailed("module is not registered".to_string())
            })?;
            for request in &entry.requests {
                match entry.links.get(request) {
                    Some(target) => pending.push(*target),
                    None => {
                        return Err(JsEngineError::ModuleResolutionFailed(
                            "module graph has an unresolved specifier".to_string(),
                        ));
                    }
                }
            }
        }
        Ok(root_entry.module.clone())
    }

    /// 静的グラフのロード区間を開始する。返す guard が drop されると終了する。
    pub(super) fn static_load_scope<'a>(
        self: &'a Rc<Self>,
        executor: &'a Rc<ModuleAwareJobExecutor>,
    ) -> StaticLoadScope<'a> {
        self.static_phase.set(true);
        self.unresolved_hit.set(false);
        executor.capturing.set(true);
        StaticLoadScope {
            loader: self,
            executor,
        }
    }
}

/// [`BoaModuleLoader::static_load_scope`] の guard。どの経路で抜けても
/// 静的フェーズと job の捕捉を終了する。
pub(super) struct StaticLoadScope<'a> {
    loader: &'a Rc<BoaModuleLoader>,
    executor: &'a Rc<ModuleAwareJobExecutor>,
}

impl StaticLoadScope<'_> {
    /// このスコープで「未 link の specifier」による拒否が起きたか。
    pub(super) fn unresolved_hit(&self) -> bool {
        self.loader.unresolved_hit.get()
    }
}

impl Drop for StaticLoadScope<'_> {
    fn drop(&mut self) {
        self.loader.static_phase.set(false);
        self.executor.capturing.set(false);
        // 走らせなかった job が残っていれば捨てる（次の評価へ持ち越さない）。
        self.executor.module_loads.borrow_mut().clear();
    }
}

impl ModuleLoader for BoaModuleLoader {
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: BoaModuleRequest,
        _context: &RefCell<&mut Context>,
    ) -> impl Future<Output = JsResult<Module>> {
        let result = {
            let state = self.state.borrow();
            let referrer_id = match &referrer {
                Referrer::Module(module) => state.ids.get(module).copied(),
                _ => None,
            };
            // 厳格な変換（lone surrogate は拒否）。specifier は文言に含めない。
            let specifier = request.specifier().to_std_string().ok();
            resolve_request(
                &state,
                self.static_phase.get(),
                referrer_id,
                !request.attributes().is_empty(),
                specifier.as_deref(),
            )
        };
        let result = result.map_err(|rejection| {
            if rejection == LoadRejection::Unresolved {
                self.unresolved_hit.set(true);
            }
            JsError::from(JsNativeError::typ().with_message(rejection.message()))
        });
        async move { result }
    }
}

// ---- job 実行器 ----

/// モジュールのグラフロードが積む `NativeAsyncJob` だけを捕まえる job 実行器。
///
/// `Module::load` は未ロードの import ごとに `NativeAsyncJob` を積み、それを走らせ
/// ないとロードが完了しない。一方 `Context::run_jobs()` は classic 評価が残した
/// `PromiseJob` なども走らせるため、ページ全体の drain は本 issue の範囲外
/// （TASK-30 の残課題）。そこで捕捉中（[`BoaModuleLoader::static_load_scope`]）の
/// `AsyncJob` だけを別キューへ取り、[`Self::drain_module_load_jobs`] が走らせる。
/// それ以外の job は `SimpleJobExecutor` へ委譲して積んだままにする（従来どおり
/// 走らない）。`run_jobs` は `SimpleJobExecutor` へ委譲するが、本 crate は呼ばない。
pub(super) struct ModuleAwareJobExecutor {
    inner: Rc<SimpleJobExecutor>,
    capturing: Cell<bool>,
    module_loads: RefCell<VecDeque<NativeAsyncJob>>,
}

impl ModuleAwareJobExecutor {
    pub(super) fn new() -> Self {
        Self {
            inner: Rc::new(SimpleJobExecutor::new()),
            capturing: Cell::new(false),
            module_loads: RefCell::new(VecDeque::new()),
        }
    }

    /// 捕まえたロード用 job を、キューが空になるまで 1 回ずつ poll する。
    ///
    /// ローダーは同期で結果を返すため各 job は 1 回の poll で完了する。`Pending`・
    /// job の失敗・反復回数の上限超過は `EvaluationFailed`（fail-closed）。
    pub(super) fn drain_module_load_jobs(
        &self,
        context: &mut Context,
    ) -> Result<(), JsEngineError> {
        let cell = RefCell::new(context);
        let mut task_context = std::task::Context::from_waker(Waker::noop());
        let mut iterations: usize = 0;
        loop {
            // job の poll 中に新しい job が積まれるため、借用は pop の間だけにする。
            let next = self.module_loads.borrow_mut().pop_front();
            let Some(job) = next else {
                return Ok(());
            };
            iterations += 1;
            if iterations > MAX_LOAD_JOB_ITERATIONS {
                return Err(JsEngineError::EvaluationFailed(
                    "module graph loading exceeded the job iteration limit".to_string(),
                ));
            }
            let mut future = job.call(&cell);
            match Pin::new(&mut future).poll(&mut task_context) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(_)) => {
                    return Err(JsEngineError::EvaluationFailed(
                        "module graph loading failed".to_string(),
                    ));
                }
                Poll::Pending => {
                    return Err(JsEngineError::EvaluationFailed(
                        "module graph loading did not complete".to_string(),
                    ));
                }
            }
        }
    }
}

impl JobExecutor for ModuleAwareJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context) {
        match job {
            Job::AsyncJob(async_job) if self.capturing.get() => {
                self.module_loads.borrow_mut().push_back(async_job);
            }
            other => Rc::clone(&self.inner).enqueue_job(other, context),
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        Rc::clone(&self.inner).run_jobs(context)
    }
}

// ---- ロード → link → 評価 ----

/// `root` を静的グラフごとロード・link・評価し、top-level の promise の状態を返す。
///
/// 呼び出し元: `BoaEngine::evaluate_module`（事前に未解決の依存がないことを確認済み）。
///
/// - ロードの拒否は `ModuleResolutionFailed`、`Pending`・job 失敗は `EvaluationFailed`
/// - link の失敗（存在しない export の import 等の SyntaxError）は JS が 1 行も
///   走っていないため `EvaluationFailed`（`Rejected` にしない）
/// - boa は link 前の `evaluate` で panic する（release は abort）ため、link が
///   `Ok` のときだけ `evaluate` を呼ぶ
/// - 評価後に job は drain しない。非 TLA の静的グラフでは top-level の promise は
///   `evaluate` の中で確定する
pub(super) fn load_link_evaluate(
    context: &mut Context,
    loader: &Rc<BoaModuleLoader>,
    executor: &Rc<ModuleAwareJobExecutor>,
    root: &Module,
) -> Result<GraphOutcome, JsEngineError> {
    {
        let scope = loader.static_load_scope(executor);
        let load_promise = root.load(context);
        executor.drain_module_load_jobs(context)?;
        match load_promise.state() {
            PromiseState::Fulfilled(_) => {}
            PromiseState::Rejected(_) => {
                return Err(JsEngineError::ModuleResolutionFailed(
                    if scope.unresolved_hit() {
                        "module graph has an unresolved specifier"
                    } else {
                        "module graph could not be loaded"
                    }
                    .to_string(),
                ));
            }
            PromiseState::Pending => {
                return Err(JsEngineError::EvaluationFailed(
                    "module graph loading did not complete".to_string(),
                ));
            }
        }
    }

    root.link(context).map_err(|err| {
        JsEngineError::EvaluationFailed(super::truncate_message(&format!(
            "module link failed: {}",
            super::error_message(&err, context)
        )))
    })?;

    let promise = root
        .evaluate(context)
        .map_err(|err| super::convert_eval_error(&err, context))?;
    Ok(match promise.state() {
        PromiseState::Fulfilled(_) => GraphOutcome::Fulfilled,
        PromiseState::Rejected(reason) => GraphOutcome::Rejected(rejection_error(reason, context)),
        PromiseState::Pending => GraphOutcome::Pending,
    })
}

/// [`load_link_evaluate`] の結果。
pub(super) enum GraphOutcome {
    Fulfilled,
    /// 評価が例外で終わった。
    Rejected(JsEngineError),
    Pending,
}

/// 拒否理由を `JsEngineError` へ変換する（ランタイム上限は `ResourceLimitExceeded`、
/// それ以外は `EvaluationFailed`。メッセージは切り詰め済み）。
fn rejection_error(reason: BoaValue, context: &mut Context) -> JsEngineError {
    super::convert_eval_error(&JsError::from_opaque(reason), context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(context: &mut Context, source: &str) -> Module {
        Module::parse(Source::from_bytes(source), None, context).expect("module parses")
    }

    /// JS-4・Issue #1114: 依存は出現順・重複なしで集まり、関数内の `await` は
    /// TLA と誤検出しない。
    #[test]
    fn js_4_analyze_collects_requests_in_order_without_duplicates() {
        let requests = analyze_module_source(
            "import a from './a.js'; export { b } from './b.js'; import './a.js'; \
             async function f() { await 1; for await (const x of []) {} }",
        )
        .expect("analyzes");
        assert_eq!(requests, vec!["./a.js".to_string(), "./b.js".to_string()]);
    }

    /// JS-4・REPAIR-3・Issue #1114: TLA と動的 import は具体的な未対応エラーになる。
    #[test]
    fn js_4_analyze_rejects_tla_and_dynamic_import_with_specific_messages() {
        for source in [
            "await 1;",
            "for await (const x of []) {}",
            "export const v = await Promise.resolve(1);",
        ] {
            match analyze_module_source(source) {
                Err(JsEngineError::NotYetImplemented(message)) => {
                    assert!(message.contains("top-level await"), "{source}: {message}");
                }
                other => panic!("{source}: unexpected {other:?}"),
            }
        }
        for source in [
            "import('./a.js');",
            "function f() { return import('./a.js'); }",
            "const g = () => import('x');",
        ] {
            match analyze_module_source(source) {
                Err(JsEngineError::NotYetImplemented(message)) => {
                    assert!(message.contains("dynamic import()"), "{source}: {message}");
                }
                other => panic!("{source}: unexpected {other:?}"),
            }
        }
    }

    /// JS-6・Issue #1114: 依存数・specifier 長の上限は ResourceLimitExceeded。
    #[test]
    fn js_6_analyze_enforces_request_count_and_specifier_length() {
        let many: String = (0..=MAX_MODULE_REQUESTS)
            .map(|i| format!("import './m{i}.js';\n"))
            .collect();
        match analyze_module_source(&many) {
            Err(JsEngineError::ResourceLimitExceeded(message)) => {
                assert!(message.contains("257"), "{message}");
                assert!(message.contains("256"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        let long = format!("import '{}';", "a".repeat(MAX_MODULE_SPECIFIER_BYTES + 1));
        assert!(matches!(
            analyze_module_source(&long),
            Err(JsEngineError::ResourceLimitExceeded(_))
        ));
        assert!(matches!(
            analyze_module_source("import '\\uD800';"),
            Err(JsEngineError::EvaluationFailed(_))
        ));
        assert!(matches!(
            analyze_module_source("export const"),
            Err(JsEngineError::EvaluationFailed(_))
        ));
    }

    /// JS-4・Issue #1114: ローダーの判定。phase の外・未知の referrer・attributes 付き・
    /// 未 link の specifier はそれぞれ別の理由で拒否し、link 済みなら対象を返す。
    #[test]
    fn js_4_resolve_request_rejects_each_invalid_case_and_resolves_linked() {
        let mut context = Context::default();
        let a = parse(&mut context, "import './b.js';");
        let b = parse(&mut context, "export const x = 1;");
        let (id_a, id_b) = (ModuleId::from_raw(1), ModuleId::from_raw(2));
        let loader = BoaModuleLoader::new();
        loader.insert(id_a, a, vec!["./b.js".to_string()]);
        loader.insert(id_b, b.clone(), Vec::new());
        loader
            .link(id_a, &[ModuleResolution::new("./b.js".to_string(), id_b)])
            .expect("links");
        let state = loader.state.borrow();

        assert_eq!(
            resolve_request(&state, false, Some(id_a), false, Some("./b.js")).unwrap_err(),
            LoadRejection::OutsideStaticPhase
        );
        assert_eq!(
            resolve_request(
                &state,
                true,
                Some(ModuleId::from_raw(9)),
                false,
                Some("./b.js")
            )
            .unwrap_err(),
            LoadRejection::UnknownReferrer
        );
        assert_eq!(
            resolve_request(&state, true, None, false, Some("./b.js")).unwrap_err(),
            LoadRejection::UnknownReferrer
        );
        assert_eq!(
            resolve_request(&state, true, Some(id_a), true, Some("./b.js")).unwrap_err(),
            LoadRejection::ImportAttributes
        );
        assert_eq!(
            resolve_request(&state, true, Some(id_a), false, Some("./c.js")).unwrap_err(),
            LoadRejection::Unresolved
        );
        assert_eq!(
            resolve_request(&state, true, Some(id_a), false, None).unwrap_err(),
            LoadRejection::Unresolved
        );
        let resolved = resolve_request(&state, true, Some(id_a), false, Some("./b.js"))
            .expect("linked specifier resolves");
        assert_eq!(resolved, b);
    }

    /// JS-4・Issue #1114: 固定文言に specifier・URL を含めない。
    #[test]
    fn js_4_rejection_messages_are_fixed_strings() {
        for rejection in [
            LoadRejection::OutsideStaticPhase,
            LoadRejection::UnknownReferrer,
            LoadRejection::ImportAttributes,
            LoadRejection::Unresolved,
        ] {
            assert!(!rejection.message().contains("://"));
            assert!(!rejection.message().is_empty());
        }
    }
}
