//! 実行コンテキストの表と `Runtime.executionContextCreated`・`Page.createIsolatedWorld`
//! （TASK-44.6・#1217、ビヘイビア `CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・MS-4）。
//!
//! Playwright は `Runtime.enable` 後に届く `Runtime.executionContextCreated` で main world と utility
//! world（`Page.createIsolatedWorld` / `Page.addScriptToEvaluateOnNewDocument` の `worldName`）を
//! 把握し、揃うまで `selector` 段階を待つ。本モジュールは、その contextId の表
//! （[`ExecutionContextTable`]）・イベントの組み立て（[`context_created_event`]・
//! [`contexts_cleared_event`]）・`Page.createIsolatedWorld` ハンドラを持つ。評価系の
//! `Runtime.evaluate`・`Runtime.releaseObject`（TASK-44.7・#1218）は子モジュール `evaluate` が持ち、
//! contextId の照合（[`ExecutionContextTable::resolve`]）・評価ヘルパーの注入記録（epoch）・失効を本表が提供する。
//! `Runtime.callFunctionOn`・`awaitPromise` は #1219 の範囲。
//!
//! # 構成と呼び出し元
//!
//! - 表は [`crate::target::TargetRegistry`] の内部状態（`Tables`）に 1 つ埋め込む（単一 Mutex で保護）。
//!   エントリは必ず「あるセッション」に属するため、セッションの削除（detach・close・dispose・接続解放）と
//!   同じ 1 か所（`drop_session_state`）で消える（`CDP-7`）。本モジュールの表自体はロックを持たない。
//! - `Page.navigate`（[`crate::page`]）の確定、`Runtime.enable`・`Runtime.runIfWaitingForDebugger`
//!   （[`crate::playwright_compat`]）が表へ登録し、イベントを同伴させる。
//! - 評価器（core の `ScriptContextHandle`）が無効なときは、どの経路もエントリを作らず
//!   イベントも送らない（偽の `context.id` を見せない。`SEC-2`・43.3j の方針）。
//!
//! # ID と上限（`CDP-7`）
//!
//! - contextId は registry 全体で 1 から増やし、再利用しない。JS の安全整数 2^53−1
//!   （[`MAX_CONTEXT_ID`]）を超える前に checked 演算で検査し、枯渇したら `SERVER_ERROR`（wrap しない）。
//! - 文書 × セッションあたりの isolated world は [`MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION`] まで。
//!   同じ名前の `Page.createIsolatedWorld` は既存の contextId を返し、イベントは再送しない。
//! - `uniqueId` は contextId から作る不透明な文字列（`ctx-<id>`）で、Chrome の書式を模さない（`SEC-2`）。
//! - `origin` は確定 URL から導いた実値（`about:blank` 等の不透明・解析失敗は空文字。core の `url_origin`）。
//!
//! # 差異・スタブ（`REPAIR-3`）
//!
//! - main world は告知するが、そこへの評価（`Runtime.evaluate` の `contextId` 省略・main の contextId）は
//!   仕様として固定文言の CDP エラー（`MAIN_WORLD_EVALUATION_UNSUPPORTED`）にする（#1218）。評価できるのは
//!   isolated world（utility world 等）だけで、main world のページ JS 状態は別の実行系にある。
//! - 評価の再起動（boa のタイムアウト・子プロセスの異常終了等で core が `EngineRestarted` を返したとき）は
//!   隠さず、その contextId を表から失効させて固定文言のエラーを返す（黙って再注入しない。`objectId` は
//!   `<contextId>.<n>` でハンドル番号が 1 から振り直されるため、同じ contextId で作り直すと古い `objectId`
//!   が別の値を指してしまう）。回復は次の遷移または `Page.createIsolatedWorld`（新しい id）。
//!   `Runtime.executionContextDestroyed` は送らない。失効させた contextId の core 側コンテキストは、
//!   世代が替わるまで scope あたり 4 枠の 1 つを占め得る（上限到達時は `EVALUATION_RESOURCE_LIMIT`）。
//! - `Runtime.executionContextDestroyed` は送らず、遷移時は `Runtime.executionContextsCleared` で代える。
//! - イベントは要求したセッションにだけ送る（`Page.frameNavigated` と同じ）。同じターゲットの他セッションには
//!   届かず、そのセッションの古い世代のエントリは遷移の確定時に表から消える。
//! - `Page.createIsolatedWorld` の `grantUniveralAccess`（Chrome の綴りのまま）は受け入れるが効果は無い。

use std::collections::BTreeMap;

use fandhe_browser_core::NavigationGeneration;
use serde_json::{Value, json};

mod evaluate;

pub(crate) use evaluate::{EvalLimits, RuntimeEvaluate, RuntimeReleaseObject};

use crate::page::main_frame_id;
use crate::protocol::{
    BoxFuture, CdpError, CdpEvent, CommandContext, CommandHandler, HandlerOutput,
};
use crate::target::{MAX_WORLD_NAME_LEN, SessionId, TargetId};

/// 1 文書 × 1 セッションあたりの isolated world 数の上限（`CDP-7`。無制限確保の防止）。
/// `Page.addScriptToEvaluateOnNewDocument` の `worldName`（遷移時に自動で作る world）も含めて数える。
pub(crate) const MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION: usize = 8;

/// contextId の上限（JS の安全整数 2^53−1。クライアントが数値として正確に扱える範囲）。
pub(crate) const MAX_CONTEXT_ID: u64 = (1 << 53) - 1;

/// 実行コンテキストの world 種別。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum World {
    /// 既定（main）world。`isDefault: true`・`type: "default"`。
    Main,
    /// 名前付き isolated world（utility world 等）。名前は `worldName`（空文字もあり得る）。
    Isolated(String),
}

/// 表のエントリ 1 件。
#[derive(Debug, Clone)]
struct ContextEntry {
    target: TargetId,
    generation: NavigationGeneration,
    world: World,
    session: SessionId,
    /// 評価ヘルパー（`evaluate::helper.js`）を注入済みなら、その epoch（#1218）。
    helper_epoch: Option<u64>,
}

/// 表から取り出した読み取り用の値（将来の拡張に備え構造体にする。`REPAIR-4`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextView {
    /// contextId。
    pub(crate) id: u64,
    /// 属するターゲット（main フレームの frame ID と同じ値）。
    pub(crate) target: TargetId,
    /// 属する文書の世代。
    pub(crate) generation: NavigationGeneration,
    /// world 種別。
    pub(crate) world: World,
    /// 評価ヘルパーを注入済みなら、その epoch（未注入は `None`。#1218）。
    pub(crate) helper_epoch: Option<u64>,
}

/// 表の操作エラー。表は失敗時に変更されない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextError {
    /// 未知・閉じたセッション。
    UnknownSession,
    /// contextId が枯渇した（wrap しない）。
    IdExhausted,
    /// 文書 × セッションあたりの isolated world が上限に達している。
    TooManyWorlds,
    /// 指定した世代より新しい世代のエントリが既にある（古い文書への登録を拒否する）。
    StaleDocument,
}

impl ContextError {
    /// CDP エラーへの写像（固定文言。入力値を含めない）。
    pub(crate) fn to_cdp(self) -> CdpError {
        match self {
            Self::UnknownSession => CdpError::INVALID_PARAMS,
            Self::IdExhausted => CdpError::SERVER_ERROR,
            Self::TooManyWorlds => CdpError::TOO_MANY_WORLDS,
            Self::StaleDocument => CdpError::TARGET_DOCUMENT_UNAVAILABLE,
        }
    }
}

/// [`ExecutionContextTable::resolve`] の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextLookupError {
    /// 未知、または他セッションの contextId（存在を区別して漏らさない）。
    UnknownContext,
    /// 古い世代の contextId（エントリは削除済み）。
    Stale,
}

impl ContextLookupError {
    /// CDP エラーへの写像（評価系 `Runtime.evaluate`〔#1218〕が使う）。
    pub(crate) fn to_cdp(self) -> CdpError {
        match self {
            Self::UnknownContext => CdpError::INVALID_PARAMS,
            Self::Stale => CdpError::EXECUTION_CONTEXT_STALE,
        }
    }
}

/// contextId の表（ロックを持たない純粋なデータ構造。保護は registry の Mutex が担う）。
#[derive(Debug)]
pub(crate) struct ExecutionContextTable {
    next_id: u64,
    /// 評価ヘルパーの注入ごとに払い出す epoch（1 から単調増加・再利用しない。#1218）。
    next_epoch: u64,
    /// id 昇順で走査できるよう `BTreeMap`（イベント順を決定的にする）。
    entries: BTreeMap<u64, ContextEntry>,
}

impl Default for ExecutionContextTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionContextTable {
    /// 空の表（contextId は 1 から）。
    pub(crate) fn new() -> Self {
        Self::with_next_id(1)
    }

    /// 次に払い出す contextId を指定して作る（枯渇のテスト用）。
    pub(crate) fn with_next_id(next_id: u64) -> Self {
        Self {
            next_id,
            next_epoch: 1,
            entries: BTreeMap::new(),
        }
    }

    /// 保持しているエントリ数（テスト用）。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// 残りの払い出し可能数。
    fn remaining_ids(&self) -> u64 {
        MAX_CONTEXT_ID
            .saturating_add(1)
            .saturating_sub(self.next_id)
    }

    fn allocate(&mut self) -> Result<u64, ContextError> {
        if self.remaining_ids() == 0 {
            return Err(ContextError::IdExhausted);
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(ContextError::IdExhausted)?;
        Ok(id)
    }

    /// `tid` の古い世代（`generation` 未満）のエントリを全セッション分消す。`generation` より新しい
    /// エントリが既にあれば何も消さず `StaleDocument`（古い文書の登録で新しい文書の表を壊さない）。
    pub(crate) fn prune_target(
        &mut self,
        tid: &TargetId,
        generation: NavigationGeneration,
    ) -> Result<(), ContextError> {
        if self
            .entries
            .values()
            .any(|e| &e.target == tid && e.generation > generation)
        {
            return Err(ContextError::StaleDocument);
        }
        self.entries
            .retain(|_, e| !(&e.target == tid && e.generation < generation));
        Ok(())
    }

    fn find(
        &self,
        sid: &SessionId,
        tid: &TargetId,
        generation: NavigationGeneration,
        world: &World,
    ) -> Option<u64> {
        self.entries
            .iter()
            .find(|(_, e)| {
                &e.session == sid
                    && &e.target == tid
                    && e.generation == generation
                    && &e.world == world
            })
            .map(|(id, _)| *id)
    }

    fn isolated_count(
        &self,
        sid: &SessionId,
        tid: &TargetId,
        generation: NavigationGeneration,
    ) -> usize {
        self.entries
            .values()
            .filter(|e| {
                &e.session == sid
                    && &e.target == tid
                    && e.generation == generation
                    && matches!(e.world, World::Isolated(_))
            })
            .count()
    }

    fn insert(
        &mut self,
        sid: &SessionId,
        tid: &TargetId,
        generation: NavigationGeneration,
        world: World,
    ) -> Result<u64, ContextError> {
        let id = self.allocate()?;
        self.entries.insert(
            id,
            ContextEntry {
                target: tid.clone(),
                generation,
                world,
                session: sid.clone(),
                helper_epoch: None,
            },
        );
        Ok(id)
    }

    /// `(sid, tid, generation)` の文書の main と `isolated_names` の world を揃え（無ければ作る）、
    /// その文書の全コンテキストを main → isolated（id 昇順）で返す。
    ///
    /// 先に `tid` の古い世代を全セッション分消す。上限・枯渇・古い文書で失敗するときは、古い世代の
    /// 削除を除き表を変えない。`isolated_names` は重複を除き、空文字は無視する（空は main の意味）。
    pub(crate) fn ensure_document(
        &mut self,
        sid: &SessionId,
        tid: &TargetId,
        generation: NavigationGeneration,
        isolated_names: &[String],
    ) -> Result<Vec<ContextView>, ContextError> {
        self.prune_target(tid, generation)?;
        let mut wanted: Vec<&String> = Vec::new();
        for n in isolated_names {
            if !n.is_empty() && !wanted.contains(&n) {
                wanted.push(n);
            }
        }
        let missing_main = self.find(sid, tid, generation, &World::Main).is_none();
        let missing_names: Vec<&String> = wanted
            .iter()
            .copied()
            .filter(|n| {
                self.find(sid, tid, generation, &World::Isolated((*n).clone()))
                    .is_none()
            })
            .collect();
        if self
            .isolated_count(sid, tid, generation)
            .saturating_add(missing_names.len())
            > MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION
        {
            return Err(ContextError::TooManyWorlds);
        }
        let needed = u64::try_from(missing_names.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::from(missing_main));
        if needed > self.remaining_ids() {
            return Err(ContextError::IdExhausted);
        }
        if missing_main {
            self.insert(sid, tid, generation, World::Main)?;
        }
        for n in missing_names {
            self.insert(sid, tid, generation, World::Isolated(n.clone()))?;
        }
        Ok(self.views_for(sid, tid, generation))
    }

    /// 名前付き isolated world を 1 つ作る。同じ名前が既にあればその contextId を返す（2 つ目の値は
    /// 新規作成なら `true`。再利用ではイベントを再送しない）。
    pub(crate) fn create_isolated(
        &mut self,
        sid: &SessionId,
        tid: &TargetId,
        generation: NavigationGeneration,
        name: &str,
    ) -> Result<(ContextView, bool), ContextError> {
        self.prune_target(tid, generation)?;
        let world = World::Isolated(name.to_owned());
        if let Some(id) = self.find(sid, tid, generation, &world) {
            return Ok((
                ContextView {
                    id,
                    target: tid.clone(),
                    generation,
                    world,
                    helper_epoch: None,
                },
                false,
            ));
        }
        if self.isolated_count(sid, tid, generation) >= MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION {
            return Err(ContextError::TooManyWorlds);
        }
        let id = self.insert(sid, tid, generation, world.clone())?;
        Ok((
            ContextView {
                id,
                target: tid.clone(),
                generation,
                world,
                helper_epoch: None,
            },
            true,
        ))
    }

    /// `(sid, tid, generation)` の全コンテキスト（main → isolated の id 昇順）。
    pub(crate) fn views_for(
        &self,
        sid: &SessionId,
        tid: &TargetId,
        generation: NavigationGeneration,
    ) -> Vec<ContextView> {
        let mut views: Vec<ContextView> = self
            .entries
            .iter()
            .filter(|(_, e)| &e.session == sid && &e.target == tid && e.generation == generation)
            .map(|(id, e)| ContextView {
                id: *id,
                target: e.target.clone(),
                generation: e.generation,
                world: e.world.clone(),
                helper_epoch: e.helper_epoch,
            })
            .collect();
        // BTreeMap は id 昇順。main を先頭へ寄せる（安定ソート）。
        views.sort_by_key(|v| !matches!(v.world, World::Main));
        views
    }

    /// セッションのエントリをすべて消す（`drop_session_state` から。`CDP-7`）。
    pub(crate) fn remove_session(&mut self, sid: &SessionId) {
        self.entries.retain(|_, e| &e.session != sid);
    }

    /// contextId を現在の世代に照らして解決する（評価系 #1218 が使う）。他セッションの contextId は
    /// `UnknownContext`。世代が違えばエントリを消して `Stale`。
    pub(crate) fn resolve(
        &mut self,
        sid: &SessionId,
        context_id: u64,
        current_generation: NavigationGeneration,
    ) -> Result<ContextView, ContextLookupError> {
        let Some(e) = self.entries.get(&context_id) else {
            return Err(ContextLookupError::UnknownContext);
        };
        if &e.session != sid {
            return Err(ContextLookupError::UnknownContext);
        }
        if e.generation != current_generation {
            self.entries.remove(&context_id);
            return Err(ContextLookupError::Stale);
        }
        Ok(ContextView {
            id: context_id,
            target: e.target.clone(),
            generation: e.generation,
            world: e.world.clone(),
            helper_epoch: e.helper_epoch,
        })
    }

    /// 評価ヘルパーの epoch を 1 つ払い出す（1 から。JS の安全整数 2^53−1 を超える前に checked 演算で
    /// 検査し、枯渇したら `IdExhausted`。wrap しない）。
    pub(crate) fn allocate_helper_epoch(&mut self) -> Result<u64, ContextError> {
        if self.next_epoch > MAX_CONTEXT_ID {
            return Err(ContextError::IdExhausted);
        }
        let e = self.next_epoch;
        self.next_epoch = e.checked_add(1).ok_or(ContextError::IdExhausted)?;
        Ok(e)
    }

    /// 評価ヘルパーの注入結果（epoch）を記録する。未記録なら `epoch` を書き、記録済みなら既存の値を
    /// 変えずに返す（並行した初回評価の冪等性）。照合は [`Self::resolve`] と同じ（他セッションは
    /// `UnknownContext`、世代違いはエントリを消して `Stale`）。
    pub(crate) fn record_helper_epoch(
        &mut self,
        sid: &SessionId,
        context_id: u64,
        current_generation: NavigationGeneration,
        epoch: u64,
    ) -> Result<u64, ContextLookupError> {
        self.resolve(sid, context_id, current_generation)?;
        let Some(e) = self.entries.get_mut(&context_id) else {
            return Err(ContextLookupError::UnknownContext);
        };
        Ok(*e.helper_epoch.get_or_insert(epoch))
    }

    /// contextId を失効させる（評価コンテキストが作り直された・ヘルパーが失われたとき。他セッションの
    /// contextId は消さない）。消したら `true`。
    pub(crate) fn invalidate(&mut self, sid: &SessionId, context_id: u64) -> bool {
        if self
            .entries
            .get(&context_id)
            .is_some_and(|e| &e.session == sid)
        {
            self.entries.remove(&context_id);
            return true;
        }
        false
    }
}

/// core の [`fandhe_browser_core::UrlOrigin`] を CDP の `origin` 文字列へ変換する
/// （tuple はそのまま、不透明・解析失敗は空文字）。
pub(crate) fn origin_string(url: &str) -> String {
    match fandhe_browser_core::url_origin(url) {
        fandhe_browser_core::UrlOrigin::Tuple(s) => s,
        _ => String::new(),
    }
}

/// `Runtime.executionContextCreated` を作る。`origin` は確定 URL から導いた値（`about:blank` は空）。
/// `session_id` は呼び出し側（`Dispatcher`）が補完するため `None`。
pub(crate) fn context_created_event(view: &ContextView, origin: &str) -> CdpEvent {
    let (name, is_default, kind) = match &view.world {
        World::Main => ("", true, "default"),
        World::Isolated(n) => (n.as_str(), false, "isolated"),
    };
    CdpEvent {
        method: "Runtime.executionContextCreated",
        params: json!({
            "context": {
                "id": view.id,
                "origin": origin,
                "name": name,
                "uniqueId": format!("ctx-{}", view.id),
                "auxData": {
                    "frameId": main_frame_id(Some(&view.target)),
                    "isDefault": is_default,
                    "type": kind,
                },
            }
        }),
        session_id: None,
    }
}

/// `Runtime.executionContextsCleared`（遷移で古い文書のコンテキストをすべて失効させる）。
pub(crate) fn contexts_cleared_event() -> CdpEvent {
    CdpEvent {
        method: "Runtime.executionContextsCleared",
        params: json!({}),
        session_id: None,
    }
}

/// セッションの現在の確定文書について main（と `include_init_worlds` なら保持中の `worldName` の
/// isolated world）を表へ揃え、`Runtime.executionContextCreated` の列にして返す（`Runtime.enable` の
/// 初回・`Runtime.runIfWaitingForDebugger` の再開から呼ばれる。TASK-44.6・#1217）。
///
/// 評価器が無効・ターゲットが止まっている・確定文書が無い（遷移中・取得失敗後）・世代が読み取り中に
/// 変わった場合は空（エントリも作らない）。呼び出し側が Runtime 有効を確認する。
pub(crate) fn current_document_events(
    state: &crate::server::CdpState,
    sid: &SessionId,
    include_init_worlds: bool,
) -> Vec<CdpEvent> {
    if !state.evaluator_enabled() {
        return Vec::new();
    }
    let registry = state.registry();
    let Some(tid) = registry.session_target(sid) else {
        return Vec::new();
    };
    if registry.is_paused(&tid) {
        return Vec::new();
    }
    let Some(nav) = state.navigations().get(&tid) else {
        return Vec::new();
    };
    // 世代確認から登録・イベント構築までを遷移の開始・確定と同じゲートで直列化する
    // （確認後に別接続の `Page.navigate` が始まっても旧世代の contextId を作らない。`CDP-2`）。
    let _gate = crate::page::lock_gate(state.commit_gate());
    let before = nav.current_generation();
    let doc = nav.latest();
    let after = nav.current_generation();
    let Some(doc) = doc else {
        return Vec::new();
    };
    if before != after {
        return Vec::new();
    }
    let Ok(views) = registry.document_contexts(sid, after, include_init_worlds) else {
        return Vec::new();
    };
    let origin = origin_string(doc.url());
    views
        .iter()
        .map(|v| context_created_event(v, &origin))
        .collect()
}

/// `Page.createIsolatedWorld`（`CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・TASK-44.6・#1217）。
///
/// Playwright が utility world を作るために送る。`{"executionContextId": n}` を返し、セッションが
/// `Runtime.enable` 済みなら `Runtime.executionContextCreated` を**応答より前に**送る（Chromium と同じ
/// 順序。[`HandlerOutput::with_pre_event`]）。同じ名前の world が現在の文書に既にあれば同じ id を返し、
/// イベントは再送しない。
///
/// 検証順: `sessionId` なし → `UNSUPPORTED_PARAMS` → `params` が非オブジェクト・`frameId` 無し/非文字列・
/// `worldName` が非文字列/NUL 含み・`grantUniveralAccess` が非 bool → `INVALID_PARAMS`
/// （未知キーは先に `UNSUPPORTED_PARAMS`）→ `worldName` の長さ超過 → `WORLD_NAME_TOO_LONG` →
/// 未知セッション・自ターゲット以外の `frameId` → `INVALID_PARAMS`（未知と他ターゲットを区別しない）→
/// 止めたターゲット → `TARGET_WAITING_FOR_DEBUGGER` → 評価器が無効 → `EXECUTION_CONTEXTS_UNAVAILABLE`
/// → 確定文書なし・世代の食い違い → `TARGET_DOCUMENT_UNAVAILABLE` → 上限超過 → `TOO_MANY_WORLDS`
/// → ID 枯渇 → `SERVER_ERROR`。失敗時は状態を変えない。
///
/// 差異（`REPAIR-3`）: `grantUniveralAccess` は受け入れるが効果は無い。評価器が無効な構成では、使えない
/// contextId を渡さないためエラーにする（偽の成功を返さない。`SEC-2`）。UA・ヘッダ・`navigator` には触れない。
pub(crate) struct PageCreateIsolatedWorld;

impl CommandHandler for PageCreateIsolatedWorld {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if obj
                .keys()
                .any(|k| k != "frameId" && k != "worldName" && k != "grantUniveralAccess")
            {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let frame_id = obj
                .get("frameId")
                .and_then(Value::as_str)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let world_name = match obj.get("worldName") {
                None => "",
                Some(v) => {
                    let w = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    if w.contains('\0') {
                        return Err(CdpError::INVALID_PARAMS);
                    }
                    w
                }
            };
            if let Some(g) = obj.get("grantUniveralAccess")
                && !g.is_boolean()
            {
                return Err(CdpError::INVALID_PARAMS);
            }
            if world_name.len() > MAX_WORLD_NAME_LEN {
                return Err(CdpError::WORLD_NAME_TOO_LONG);
            }
            let state = ctx.state;
            let registry = state.registry();
            let tid = registry
                .session_target(sid)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if frame_id != tid.as_str() {
                return Err(CdpError::INVALID_PARAMS);
            }
            if registry.is_paused(&tid) {
                return Err(CdpError::TARGET_WAITING_FOR_DEBUGGER);
            }
            if !state.evaluator_enabled() {
                return Err(CdpError::EXECUTION_CONTEXTS_UNAVAILABLE);
            }
            // 世代は「current → latest → current」の順に読み、食い違えば文書なし扱いにする
            // （古い世代のエントリを作らない）。
            let nav = state
                .navigations()
                .get(&tid)
                .ok_or(CdpError::TARGET_DOCUMENT_UNAVAILABLE)?;
            // 文書確認から登録・イベント構築までを `Page.navigate` の世代払い出し・確定と同じゲートで
            // 直列化する（確認後に遷移が始まっても旧世代の contextId を返さない。`CDP-2`）。
            let _gate = crate::page::lock_gate(state.commit_gate());
            let before = nav.current_generation();
            let doc = nav.latest();
            let after = nav.current_generation();
            let Some(doc) = doc else {
                return Err(CdpError::TARGET_DOCUMENT_UNAVAILABLE);
            };
            if before != after {
                return Err(CdpError::TARGET_DOCUMENT_UNAVAILABLE);
            }
            let created = registry
                .create_isolated_world(sid, &tid, after, world_name)
                .map_err(ContextError::to_cdp)?;
            let mut out = HandlerOutput::result(json!({ "executionContextId": created.view.id }));
            if created.announce {
                let mut ev = context_created_event(&created.view, &origin_string(doc.url()));
                ev.session_id = Some(sid.clone());
                out = out.with_pre_event(ev);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: &str) -> SessionId {
        SessionId::parse(n).unwrap()
    }

    fn tid(n: &str) -> TargetId {
        TargetId::parse(n).unwrap()
    }

    fn generation(n: u64) -> NavigationGeneration {
        // 世代は core の NavigationState からしか作れないため、n 回 begin して取り出す。
        let st = fandhe_browser_core::NavigationState::new();
        let mut g = st.begin_navigation().unwrap();
        for _ in 1..n {
            g = st.begin_navigation().unwrap();
        }
        g
    }

    #[test]
    fn cdp7_context_ids_are_monotonic_and_never_reused() {
        let mut t = ExecutionContextTable::new();
        let (s, a) = (sid("S1"), tid("T1"));
        let v1 = t.ensure_document(&s, &a, generation(1), &[]).unwrap();
        assert_eq!(v1.len(), 1);
        assert_eq!(v1[0].id, 1);
        // 遷移で古い世代が消えても、id は戻らない。
        let v2 = t
            .ensure_document(&s, &a, generation(2), &["w".into()])
            .unwrap();
        assert_eq!(v2.iter().map(|v| v.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(t.len(), 2);
        assert_eq!(v2[0].world, World::Main);
        assert_eq!(v2[1].world, World::Isolated("w".into()));
    }

    #[test]
    fn cdp7_context_id_exhaustion_does_not_wrap_and_keeps_table() {
        let mut t = ExecutionContextTable::with_next_id(MAX_CONTEXT_ID);
        let (s, a) = (sid("S1"), tid("T1"));
        let (v, created) = t.create_isolated(&s, &a, generation(1), "w").unwrap();
        assert!(created);
        assert_eq!(v.id, MAX_CONTEXT_ID);
        assert_eq!(
            t.create_isolated(&s, &a, generation(1), "x"),
            Err(ContextError::IdExhausted)
        );
        // main + isolated が 1 件ずつ必要で残り 0 件のときも、部分的に作らない。
        assert_eq!(
            t.ensure_document(&s, &a, generation(1), &[]),
            Err(ContextError::IdExhausted)
        );
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn cdp7_ensure_document_is_all_or_nothing_when_ids_run_short() {
        let mut t = ExecutionContextTable::with_next_id(MAX_CONTEXT_ID);
        let r = t.ensure_document(&sid("S1"), &tid("T1"), generation(1), &["w".into()]);
        assert_eq!(r, Err(ContextError::IdExhausted));
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn cdp7_isolated_world_limit_per_document_and_session() {
        let mut t = ExecutionContextTable::new();
        let (s, a, g) = (sid("S1"), tid("T1"), generation(1));
        for i in 0..MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION {
            t.create_isolated(&s, &a, g, &format!("w{i}")).unwrap();
        }
        assert_eq!(
            t.create_isolated(&s, &a, g, "extra"),
            Err(ContextError::TooManyWorlds)
        );
        assert_eq!(t.len(), MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION);
        // 別セッションは別枠。
        t.create_isolated(&sid("S2"), &a, g, "extra").unwrap();
        // 既存の名前は上限でも再利用できる。
        let (v, created) = t.create_isolated(&s, &a, g, "w0").unwrap();
        assert!(!created);
        assert_eq!(v.id, 1);
    }

    #[test]
    fn cdp2_create_isolated_reuses_same_name_without_new_id() {
        let mut t = ExecutionContextTable::new();
        let (s, a, g) = (sid("S1"), tid("T1"), generation(1));
        let (v1, c1) = t.create_isolated(&s, &a, g, "u").unwrap();
        let (v2, c2) = t.create_isolated(&s, &a, g, "u").unwrap();
        assert!(c1 && !c2);
        assert_eq!(v1.id, v2.id);
        // 空名前も 1 つの isolated world として扱う。
        let (v3, _) = t.create_isolated(&s, &a, g, "").unwrap();
        assert_eq!(v3.id, 2);
    }

    #[test]
    fn cdp2_stale_generation_is_pruned_and_old_document_is_rejected() {
        let mut t = ExecutionContextTable::new();
        let (s1, s2, a) = (sid("S1"), sid("S2"), tid("T1"));
        let (g1, g2) = (generation(1), generation(2));
        t.ensure_document(&s1, &a, g1, &[]).unwrap();
        t.ensure_document(&s2, &a, g1, &[]).unwrap();
        assert_eq!(t.len(), 2);
        // 新しい世代の確定で、別セッションの古いエントリも消える。
        t.ensure_document(&s1, &a, g2, &[]).unwrap();
        assert_eq!(t.len(), 1);
        // 古い世代への登録は新しい表を壊さず拒否する。
        assert_eq!(
            t.create_isolated(&s1, &a, g1, "w"),
            Err(ContextError::StaleDocument)
        );
        assert_eq!(t.len(), 1);
        // 別ターゲットは巻き込まない。
        t.ensure_document(&s1, &tid("T2"), g1, &[]).unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn cdp7_remove_session_drops_only_its_entries() {
        let mut t = ExecutionContextTable::new();
        let (a, g) = (tid("T1"), generation(1));
        t.ensure_document(&sid("S1"), &a, g, &[]).unwrap();
        t.ensure_document(&sid("S2"), &a, g, &[]).unwrap();
        t.remove_session(&sid("S1"));
        assert_eq!(t.len(), 1);
        assert_eq!(t.views_for(&sid("S2"), &a, g).len(), 1);
    }

    #[test]
    fn cdp2_resolve_rejects_unknown_foreign_and_stale_contexts() {
        let mut t = ExecutionContextTable::new();
        let (s, a, g1, g2) = (sid("S1"), tid("T1"), generation(1), generation(2));
        let v = t.ensure_document(&s, &a, g1, &[]).unwrap().remove(0);
        assert_eq!(
            t.resolve(&sid("S2"), v.id, g1),
            Err(ContextLookupError::UnknownContext)
        );
        assert_eq!(
            t.resolve(&s, 999, g1),
            Err(ContextLookupError::UnknownContext)
        );
        assert_eq!(t.resolve(&s, v.id, g1).unwrap().id, v.id);
        assert_eq!(t.resolve(&s, v.id, g2), Err(ContextLookupError::Stale));
        // 世代違いのエントリは削除される。
        assert_eq!(
            t.resolve(&s, v.id, g1),
            Err(ContextLookupError::UnknownContext)
        );
        assert_eq!(
            ContextLookupError::Stale.to_cdp(),
            CdpError::EXECUTION_CONTEXT_STALE
        );
        assert_eq!(
            CdpError::EXECUTION_CONTEXT_STALE.message,
            "execution context is not current"
        );
    }

    #[test]
    fn cdp2_context_created_event_payloads() {
        let view = ContextView {
            id: 7,
            target: tid("T1"),
            generation: generation(1),
            world: World::Main,
            helper_epoch: None,
        };
        let ev = context_created_event(&view, "http://127.0.0.1:80");
        assert_eq!(ev.method, "Runtime.executionContextCreated");
        assert_eq!(
            ev.params,
            json!({"context": {
                "id": 7,
                "origin": "http://127.0.0.1:80",
                "name": "",
                "uniqueId": "ctx-7",
                "auxData": {"frameId": "T1", "isDefault": true, "type": "default"},
            }})
        );
        let iso = ContextView {
            id: 8,
            world: World::Isolated("__pw_utility".into()),
            ..view
        };
        assert_eq!(
            context_created_event(&iso, "").params,
            json!({"context": {
                "id": 8,
                "origin": "",
                "name": "__pw_utility",
                "uniqueId": "ctx-8",
                "auxData": {"frameId": "T1", "isDefault": false, "type": "isolated"},
            }})
        );
        assert_eq!(
            contexts_cleared_event().method,
            "Runtime.executionContextsCleared"
        );
    }

    #[test]
    fn cdp7_helper_epochs_are_monotonic_and_exhaust_without_wrapping() {
        let mut t = ExecutionContextTable::new();
        assert_eq!(t.allocate_helper_epoch(), Ok(1));
        assert_eq!(t.allocate_helper_epoch(), Ok(2));
        t.next_epoch = MAX_CONTEXT_ID;
        assert_eq!(t.allocate_helper_epoch(), Ok(MAX_CONTEXT_ID));
        assert_eq!(t.allocate_helper_epoch(), Err(ContextError::IdExhausted));
        assert_eq!(t.allocate_helper_epoch(), Err(ContextError::IdExhausted));
    }

    #[test]
    fn cdp2_record_helper_epoch_keeps_first_value_and_checks_owner() {
        let mut t = ExecutionContextTable::new();
        let (s, a, g1, g2) = (sid("S1"), tid("T1"), generation(1), generation(2));
        let v = t.create_isolated(&s, &a, g1, "u").map(|(v, _)| v).unwrap();
        assert_eq!(v.helper_epoch, None);
        assert_eq!(t.record_helper_epoch(&s, v.id, g1, 5), Ok(5));
        // 記録済みなら既存の値を返し、上書きしない。
        assert_eq!(t.record_helper_epoch(&s, v.id, g1, 9), Ok(5));
        assert_eq!(t.resolve(&s, v.id, g1).unwrap().helper_epoch, Some(5));
        assert_eq!(t.views_for(&s, &a, g1)[0].helper_epoch, Some(5));
        // 他セッション・未知の id・世代違いは拒否する。
        assert_eq!(
            t.record_helper_epoch(&sid("S2"), v.id, g1, 7),
            Err(ContextLookupError::UnknownContext)
        );
        assert_eq!(
            t.record_helper_epoch(&s, 999, g1, 7),
            Err(ContextLookupError::UnknownContext)
        );
        assert_eq!(
            t.record_helper_epoch(&s, v.id, g2, 7),
            Err(ContextLookupError::Stale)
        );
    }

    #[test]
    fn cdp2_invalidate_removes_only_the_owners_context() {
        let mut t = ExecutionContextTable::new();
        let (s, a, g) = (sid("S1"), tid("T1"), generation(1));
        let v = t.create_isolated(&s, &a, g, "u").map(|(v, _)| v).unwrap();
        assert!(!t.invalidate(&sid("S2"), v.id));
        assert_eq!(t.len(), 1);
        assert!(t.invalidate(&s, v.id));
        assert!(!t.invalidate(&s, v.id));
        assert_eq!(
            t.resolve(&s, v.id, g),
            Err(ContextLookupError::UnknownContext)
        );
        // 失効させた id は再利用されない。
        let (v2, _) = t.create_isolated(&s, &a, g, "u").unwrap();
        assert_eq!(v2.id, v.id + 1);
    }

    #[test]
    fn cdp7_context_error_maps_to_fixed_cdp_errors() {
        assert_eq!(
            ContextError::UnknownSession.to_cdp(),
            CdpError::INVALID_PARAMS
        );
        assert_eq!(ContextError::IdExhausted.to_cdp(), CdpError::SERVER_ERROR);
        assert_eq!(
            ContextError::TooManyWorlds.to_cdp(),
            CdpError::TOO_MANY_WORLDS
        );
        assert_eq!(
            ContextError::StaleDocument.to_cdp(),
            CdpError::TARGET_DOCUMENT_UNAVAILABLE
        );
    }
}
/// ディスパッチャ経由の結合的な単体テスト（`Runtime.enable`・`Page.navigate`・`Page.createIsolatedWorld`。
/// TASK-44.6・#1217）。評価器の有効化は feature とターゲット OS に依存する（`EVALUATOR_EXPECTED`）。
#[cfg(test)]
mod dispatch_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use fandhe_browser_core::{AppState, FetchOptions, ScriptContextHandle};
    use fandhe_browser_profile::Profile;

    use super::*;
    use crate::page::PageNavigate;
    use crate::playwright_compat::{
        PageAddScriptToEvaluateOnNewDocument, RuntimeEnable, RuntimeRunIfWaitingForDebugger,
    };
    use crate::protocol::Dispatcher;
    use crate::server::CdpState;
    use crate::target::{SessionDomain, TargetKind};

    /// この構成で評価器が有効になるか（macOS の boa は fail-closed で無効。core の
    /// `tests/script_context_boa.rs` と同じ扱い）。
    #[allow(dead_code)]
    const EVALUATOR_EXPECTED: bool = cfg!(any(
        feature = "js-v8",
        all(feature = "js-boa", not(target_os = "macos"))
    ));

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = fandhe_browser_profile::test_base_dir();
            Self(base.join(format!(
                "fandhe-cdp-runtime-test-{}-{n}",
                std::process::id()
            )))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state_with(dir: &TempDir, handle: ScriptContextHandle) -> Arc<CdpState> {
        let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
        let app = AppState::with_disabled_renderer(profile).with_script_contexts(handle);
        Arc::new(CdpState::new(Arc::new(app)))
    }

    /// 評価器が無効な状態（既定）。
    fn disabled_state(dir: &TempDir) -> Arc<CdpState> {
        state_with(dir, ScriptContextHandle::disabled())
    }

    fn dispatcher() -> Dispatcher {
        let nav =
            PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true)).unwrap();
        Dispatcher::from_handlers(vec![
            ("Page.navigate", Box::new(nav)),
            (
                "Page.createIsolatedWorld",
                Box::new(PageCreateIsolatedWorld),
            ),
            ("Runtime.enable", Box::new(RuntimeEnable)),
            (
                "Runtime.runIfWaitingForDebugger",
                Box::new(RuntimeRunIfWaitingForDebugger),
            ),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                Box::new(PageAddScriptToEvaluateOnNewDocument),
            ),
        ])
        .unwrap()
    }

    /// 1 リクエストを処理し、送出順（応答前イベント → 応答 → イベント）の JSON フレームを返す。
    async fn call(
        st: &Arc<CdpState>,
        sid: &SessionId,
        id: i64,
        method: &str,
        params: &str,
    ) -> Vec<Value> {
        let text = format!(
            r#"{{"id":{id},"method":"{method}","sessionId":"{}","params":{params}}}"#,
            sid.as_str()
        );
        dispatcher()
            .dispatch(st, &text)
            .await
            .into_frames()
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
    }

    /// 確定済みの初期 `about:blank` を持つページとセッション。
    fn page_session(st: &Arc<CdpState>) -> (TargetId, SessionId) {
        let reg = st.registry();
        let tid = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let sid = reg.attach(&tid).unwrap();
        assert!(st.navigations().commit_initial_blank(reg, &tid));
        (tid, sid)
    }

    /// `waitForDebuggerOnStart` で止めたページとセッション（初期文書は未確定）。
    fn paused_session(st: &Arc<CdpState>) -> (TargetId, SessionId) {
        st.registry()
            .create_target_attached(None, TargetKind::Page, "about:blank", true)
            .unwrap()
    }

    fn methods(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .map(|f| f["method"].as_str().unwrap_or("<response>").to_owned())
            .collect()
    }

    // ---- 評価器が無効な構成（常に実行される。無効 → 0 件・エラー） ----

    #[tokio::test]
    async fn cdp2_runtime_enable_emits_nothing_when_evaluator_disabled() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (_, sid) = page_session(&st);
        let f = call(&st, &sid, 1, "Runtime.enable", "{}").await;
        assert_eq!(
            f,
            vec![json!({"id":1,"sessionId":sid.as_str(),"result":{}})]
        );
        assert!(
            st.registry()
                .is_domain_enabled(&sid, SessionDomain::Runtime)
        );
        assert_eq!(st.registry().exec_context_count(), 0);
    }

    #[tokio::test]
    async fn cdp2_navigate_without_evaluator_emits_no_runtime_events() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (_, sid) = page_session(&st);
        call(&st, &sid, 1, "Runtime.enable", "{}").await;
        let f = call(&st, &sid, 2, "Page.navigate", r#"{"url":"about:blank"}"#).await;
        assert_eq!(
            methods(&f),
            ["<response>", "Page.frameNavigated", "Page.loadEventFired"]
        );
        assert_eq!(st.registry().exec_context_count(), 0);
    }

    #[tokio::test]
    async fn cdp2_create_isolated_world_is_unavailable_when_evaluator_disabled() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (tid, sid) = page_session(&st);
        let p = format!(r#"{{"frameId":"{}","worldName":"u"}}"#, tid.as_str());
        let f = call(&st, &sid, 1, "Page.createIsolatedWorld", &p).await;
        assert_eq!(
            f,
            vec![
                json!({"id":1,"sessionId":sid.as_str(),"error":{"code":-32000,"message":"execution contexts unavailable"}})
            ]
        );
        assert_eq!(st.registry().exec_context_count(), 0);
    }

    // ---- 入力検証（評価器に依らず、ゲートより前に決まるもの） ----

    #[tokio::test]
    async fn cdp2_create_isolated_world_validates_params_in_order() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (tid, sid) = page_session(&st);
        let t = tid.as_str();
        let (_, other_sid) = page_session(&st);
        let err = |f: &[Value]| {
            (
                f[0]["error"]["code"].as_i64().unwrap(),
                f[0]["error"]["message"].as_str().unwrap().to_owned(),
            )
        };
        let cases: Vec<(String, i64, &str)> = vec![
            (
                format!(r#"{{"frameId":"{t}","bogus":1}}"#),
                -32602,
                "unsupported params",
            ),
            ("{}".into(), -32602, "invalid params"),
            (r#"{"frameId":1}"#.into(), -32602, "invalid params"),
            (
                format!(r#"{{"frameId":"{t}","worldName":1}}"#),
                -32602,
                "invalid params",
            ),
            (
                format!(r#"{{"frameId":"{t}","worldName":"a\u0000b"}}"#),
                -32602,
                "invalid params",
            ),
            (
                format!(r#"{{"frameId":"{t}","grantUniveralAccess":"yes"}}"#),
                -32602,
                "invalid params",
            ),
            (
                format!(r#"{{"frameId":"{t}","worldName":"{}"}}"#, "a".repeat(257)),
                -32000,
                "world name too long",
            ),
            // 未知の frameId。
            (r#"{"frameId":"ghost"}"#.into(), -32602, "invalid params"),
        ];
        for (i, (params, code, msg)) in cases.iter().enumerate() {
            let f = call(&st, &sid, 10 + i as i64, "Page.createIsolatedWorld", params).await;
            assert_eq!(err(&f), (*code, (*msg).to_owned()), "{params}");
        }
        // 他ターゲットの frameId は未知と同じ文言（存在を漏らさない）。
        let other_target = st.registry().session_target(&other_sid).unwrap();
        let p = format!(r#"{{"frameId":"{}"}}"#, other_target.as_str());
        let f = call(&st, &sid, 30, "Page.createIsolatedWorld", &p).await;
        assert_eq!(err(&f), (-32602, "invalid params".to_owned()));
        // sessionId なしは unsupported params。
        let out = dispatcher()
            .dispatch(
                &st,
                &format!(
                    r#"{{"id":31,"method":"Page.createIsolatedWorld","params":{{"frameId":"{t}"}}}}"#
                ),
            )
            .await;
        assert_eq!(
            serde_json::from_str::<Value>(&out.response.to_json()).unwrap(),
            json!({"id":31,"error":{"code":-32602,"message":"unsupported params"}})
        );
    }

    #[tokio::test]
    async fn cdp2_create_isolated_world_on_paused_target_is_rejected() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (tid, sid) = paused_session(&st);
        let p = format!(r#"{{"frameId":"{}"}}"#, tid.as_str());
        let f = call(&st, &sid, 1, "Page.createIsolatedWorld", &p).await;
        assert_eq!(f[0]["error"]["message"], "target is waiting for debugger");
    }

    // ---- addScript の worldName 保持と上限（評価器に依らない） ----

    #[tokio::test]
    async fn cdp7_world_names_are_deduplicated_and_limited() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (_, sid) = page_session(&st);
        let add = |i: i64, w: String| {
            let sid = sid.clone();
            let st = Arc::clone(&st);
            async move {
                call(
                    &st,
                    &sid,
                    i,
                    "Page.addScriptToEvaluateOnNewDocument",
                    &format!(r#"{{"source":"","worldName":"{w}"}}"#),
                )
                .await
            }
        };
        add(1, "a".into()).await;
        add(2, "a".into()).await;
        add(3, String::new()).await;
        assert_eq!(st.registry().init_world_names(&sid), vec!["a".to_owned()]);
        assert_eq!(st.registry().init_script_count(&sid), 3);
        for i in 1..MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION {
            let f = add(10 + i as i64, format!("w{i}")).await;
            assert!(f[0].get("result").is_some(), "{f:?}");
        }
        assert_eq!(
            st.registry().init_world_names(&sid).len(),
            MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION
        );
        let before = st.registry().init_script_count(&sid);
        // 9 種類目は拒否し、件数も名前も変えない。
        let f = add(99, "ninth".into()).await;
        assert_eq!(
            f[0]["error"],
            json!({"code": -32000, "message": "too many worlds"})
        );
        assert_eq!(st.registry().init_script_count(&sid), before);
        assert_eq!(
            st.registry().init_world_names(&sid).len(),
            MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION
        );
        // 既存の名前は上限でも登録できる。
        assert!(add(100, "a".into()).await[0].get("result").is_some());
    }

    // ---- registry 層: セッション・ターゲット・context の消滅でエントリが消える（`CDP-7`） ----

    fn seed_context(st: &Arc<CdpState>, sid: &SessionId, tid: &TargetId) {
        let nav = st.navigations().get(tid).unwrap();
        st.registry()
            .create_isolated_world(sid, tid, nav.current_generation(), "w")
            .unwrap();
        assert_eq!(st.registry().exec_context_count(), 1);
    }

    #[test]
    fn cdp7_exec_contexts_are_dropped_on_detach_close_dispose_and_release() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let reg = st.registry();
        // detach
        let (tid, sid) = page_session(&st);
        seed_context(&st, &sid, &tid);
        reg.detach(&sid).unwrap();
        assert_eq!(reg.exec_context_count(), 0);
        // close_target
        let (tid, sid) = page_session(&st);
        seed_context(&st, &sid, &tid);
        reg.close_target(&tid).unwrap();
        assert_eq!(reg.exec_context_count(), 0);
        // dispose_browser_context
        let ctx = reg.create_browser_context().unwrap();
        let tid = reg
            .create_target_in(&ctx, TargetKind::Page, "about:blank")
            .unwrap();
        let sid = reg.attach(&tid).unwrap();
        assert!(st.navigations().commit_initial_blank(reg, &tid));
        seed_context(&st, &sid, &tid);
        reg.dispose_browser_context(&ctx).unwrap();
        assert_eq!(reg.exec_context_count(), 0);
    }

    #[test]
    fn cdp7_context_id_exhaustion_leaves_table_unchanged_via_registry() {
        let dir = TempDir::new();
        let st = disabled_state(&dir);
        let (tid, sid) = page_session(&st);
        let g = st.navigations().get(&tid).unwrap().current_generation();
        st.registry().set_next_context_id_for_test(MAX_CONTEXT_ID);
        let v = st
            .registry()
            .create_isolated_world(&sid, &tid, g, "a")
            .unwrap();
        assert_eq!(v.view.id, MAX_CONTEXT_ID);
        assert_eq!(
            st.registry()
                .create_isolated_world(&sid, &tid, g, "b")
                .unwrap_err(),
            ContextError::IdExhausted
        );
        assert_eq!(st.registry().exec_context_count(), 1);
    }

    // ---- 評価器が有効な構成 ----

    #[cfg(any(feature = "js-v8", all(feature = "js-boa", not(target_os = "macos"))))]
    mod enabled {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        use super::*;
        use fandhe_browser_core::JsConfig;

        fn serve(body: &'static str) -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            thread::spawn(move || {
                for mut s in listener.incoming().flatten() {
                    thread::spawn(move || {
                        let mut head = Vec::new();
                        let mut b = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") && s.read(&mut b).is_ok_and(|n| n == 1) {
                            head.push(b[0]);
                        }
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = s.write_all(resp.as_bytes());
                    });
                }
            });
            port
        }

        fn enabled_state(dir: &TempDir) -> Arc<CdpState> {
            let handle = ScriptContextHandle::new(&JsConfig::default());
            assert_eq!(handle.is_enabled(), EVALUATOR_EXPECTED);
            assert!(handle.is_enabled());
            state_with(dir, handle)
        }

        fn worker_spawns(st: &Arc<CdpState>) -> u64 {
            st.app_state().script_contexts().stats().worker_spawns()
        }

        fn created(ev: &Value, id: u64, name: &str, default: bool, frame: &str, origin: &str) {
            assert_eq!(ev["method"], "Runtime.executionContextCreated", "{ev}");
            assert_eq!(
                ev["params"],
                json!({"context": {
                    "id": id,
                    "origin": origin,
                    "name": name,
                    "uniqueId": format!("ctx-{id}"),
                    "auxData": {
                        "frameId": frame,
                        "isDefault": default,
                        "type": if default { "default" } else { "isolated" },
                    },
                }})
            );
        }

        #[tokio::test]
        async fn cdp2_runtime_enable_emits_execution_context_created_once() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            let f = call(&st, &sid, 1, "Runtime.enable", "{}").await;
            assert_eq!(f.len(), 2, "{f:?}");
            assert_eq!(f[0], json!({"id":1,"sessionId":sid.as_str(),"result":{}}));
            created(&f[1], 1, "", true, tid.as_str(), "");
            assert_eq!(f[1]["sessionId"], sid.as_str());
            // 2 回目は告知しない。
            let f = call(&st, &sid, 2, "Runtime.enable", "{}").await;
            assert_eq!(
                f,
                vec![json!({"id":2,"sessionId":sid.as_str(),"result":{}})]
            );
            assert_eq!(st.registry().exec_context_count(), 1);
            assert_eq!(worker_spawns(&st), 0);
        }

        #[tokio::test]
        async fn cdp2_runtime_enable_on_paused_target_announces_after_resume() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = paused_session(&st);
            // Playwright の順序: Runtime.enable → addScript(worldName) → runIfWaitingForDebugger。
            let f = call(&st, &sid, 1, "Runtime.enable", "{}").await;
            assert_eq!(f.len(), 1, "止めたターゲットでは告知しない: {f:?}");
            call(
                &st,
                &sid,
                2,
                "Page.addScriptToEvaluateOnNewDocument",
                r#"{"source":"","worldName":"__pw_utility"}"#,
            )
            .await;
            let f = call(&st, &sid, 3, "Runtime.runIfWaitingForDebugger", "{}").await;
            assert_eq!(f.len(), 3, "{f:?}");
            created(&f[1], 1, "", true, tid.as_str(), "");
            created(&f[2], 2, "__pw_utility", false, tid.as_str(), "");
            // 続く createIsolatedWorld は同名の既存 world を返し、イベントは再送しない。
            let p = format!(
                r#"{{"frameId":"{}","worldName":"__pw_utility"}}"#,
                tid.as_str()
            );
            let f = call(&st, &sid, 4, "Page.createIsolatedWorld", &p).await;
            assert_eq!(
                f,
                vec![json!({"id":4,"sessionId":sid.as_str(),"result":{"executionContextId":2}})]
            );
            assert_eq!(worker_spawns(&st), 0);
        }

        #[tokio::test]
        async fn cdp2_navigate_emits_runtime_events_in_order() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            call(&st, &sid, 1, "Runtime.enable", "{}").await;
            call(
                &st,
                &sid,
                2,
                "Page.addScriptToEvaluateOnNewDocument",
                r#"{"source":"","worldName":"u"}"#,
            )
            .await;
            let port = serve("<p>x</p>");
            let url = format!("http://127.0.0.1:{port}/p?q=1#h");
            let f = call(
                &st,
                &sid,
                3,
                "Page.navigate",
                &format!(r#"{{"url":"{url}"}}"#),
            )
            .await;
            assert_eq!(
                methods(&f),
                [
                    "<response>",
                    "Runtime.executionContextsCleared",
                    "Page.frameNavigated",
                    "Runtime.executionContextCreated",
                    "Runtime.executionContextCreated",
                    "Page.loadEventFired",
                ]
            );
            let origin = format!("http://127.0.0.1:{port}");
            // 初回の Runtime.enable の main が id 1。新しい文書は旧 id より大きい（再利用なし）。
            created(&f[3], 2, "", true, tid.as_str(), &origin);
            created(&f[4], 3, "u", false, tid.as_str(), &origin);
            assert_eq!(st.registry().exec_context_count(), 2);
            assert_eq!(worker_spawns(&st), 0);
        }

        #[tokio::test]
        async fn cdp2_navigate_with_lifecycle_keeps_init_first_and_load_last() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (_, sid) = page_session(&st);
            st.registry().enable_page(&sid).unwrap();
            st.registry().set_lifecycle_events(&sid, true).unwrap();
            call(&st, &sid, 1, "Runtime.enable", "{}").await;
            let f = call(&st, &sid, 2, "Page.navigate", r#"{"url":"about:blank"}"#).await;
            assert_eq!(
                methods(&f),
                [
                    "<response>",
                    "Page.lifecycleEvent",
                    "Runtime.executionContextsCleared",
                    "Page.frameNavigated",
                    "Runtime.executionContextCreated",
                    "Page.loadEventFired",
                    "Page.lifecycleEvent",
                ]
            );
        }

        #[tokio::test]
        async fn cdp2_navigate_without_runtime_enable_emits_no_runtime_events() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (_, sid) = page_session(&st);
            let f = call(&st, &sid, 1, "Page.navigate", r#"{"url":"about:blank"}"#).await;
            assert_eq!(
                methods(&f),
                ["<response>", "Page.frameNavigated", "Page.loadEventFired"]
            );
            assert_eq!(st.registry().exec_context_count(), 0);
        }

        #[tokio::test]
        async fn cdp2_create_isolated_world_sends_pre_event_then_reuses_same_name() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            call(&st, &sid, 1, "Runtime.enable", "{}").await;
            let p = format!(
                r#"{{"frameId":"{}","worldName":"u","grantUniveralAccess":true}}"#,
                tid.as_str()
            );
            let f = call(&st, &sid, 2, "Page.createIsolatedWorld", &p).await;
            // 応答より前にイベント。
            assert_eq!(
                methods(&f),
                ["Runtime.executionContextCreated", "<response>"]
            );
            created(&f[0], 2, "u", false, tid.as_str(), "");
            assert_eq!(f[0]["sessionId"], sid.as_str());
            assert_eq!(
                f[1],
                json!({"id":2,"sessionId":sid.as_str(),"result":{"executionContextId":2}})
            );
            // 同名・`grantUniveralAccess:false` でも同じ id・イベントなし。
            let p2 = format!(
                r#"{{"frameId":"{}","worldName":"u","grantUniveralAccess":false}}"#,
                tid.as_str()
            );
            let f = call(&st, &sid, 3, "Page.createIsolatedWorld", &p2).await;
            assert_eq!(
                f,
                vec![json!({"id":3,"sessionId":sid.as_str(),"result":{"executionContextId":2}})]
            );
            assert_eq!(worker_spawns(&st), 0);
        }

        #[tokio::test]
        async fn cdp2_create_isolated_world_without_runtime_enable_returns_id_without_event() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            let p = format!(r#"{{"frameId":"{}","worldName":"u"}}"#, tid.as_str());
            let f = call(&st, &sid, 1, "Page.createIsolatedWorld", &p).await;
            assert_eq!(
                f,
                vec![json!({"id":1,"sessionId":sid.as_str(),"result":{"executionContextId":1}})]
            );
            // 後から Runtime.enable すると、作成済みの world も main に続いて告知する。
            let f = call(&st, &sid, 2, "Runtime.enable", "{}").await;
            assert_eq!(f.len(), 3, "{f:?}");
            created(&f[1], 2, "", true, tid.as_str(), "");
            created(&f[2], 1, "u", false, tid.as_str(), "");
        }

        #[tokio::test]
        async fn cdp7_create_isolated_world_limit_is_eight_per_document() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            for i in 0..MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION {
                let p = format!(r#"{{"frameId":"{}","worldName":"w{i}"}}"#, tid.as_str());
                let f = call(&st, &sid, 1 + i as i64, "Page.createIsolatedWorld", &p).await;
                assert!(f[0].get("result").is_some(), "{f:?}");
            }
            let p = format!(r#"{{"frameId":"{}","worldName":"extra"}}"#, tid.as_str());
            let f = call(&st, &sid, 99, "Page.createIsolatedWorld", &p).await;
            assert_eq!(
                f[0]["error"],
                json!({"code": -32000, "message": "too many worlds"})
            );
            assert_eq!(
                st.registry().exec_context_count(),
                MAX_ISOLATED_WORLDS_PER_DOCUMENT_SESSION
            );
        }

        #[tokio::test]
        async fn cdp7_context_id_exhaustion_is_server_error_and_table_unchanged() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            st.registry().set_next_context_id_for_test(MAX_CONTEXT_ID);
            let p = format!(r#"{{"frameId":"{}","worldName":"a"}}"#, tid.as_str());
            let f = call(&st, &sid, 1, "Page.createIsolatedWorld", &p).await;
            assert_eq!(
                f[0]["result"],
                json!({"executionContextId": MAX_CONTEXT_ID})
            );
            let p = format!(r#"{{"frameId":"{}","worldName":"b"}}"#, tid.as_str());
            let f = call(&st, &sid, 2, "Page.createIsolatedWorld", &p).await;
            assert_eq!(
                f[0]["error"],
                json!({"code": -32000, "message": "server error"})
            );
            assert_eq!(st.registry().exec_context_count(), 1);
        }

        #[tokio::test]
        async fn cdp2_create_isolated_world_without_committed_document_is_unavailable() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let reg = st.registry();
            let tid = reg.create_target(TargetKind::Page, "about:blank").unwrap();
            let sid = reg.attach(&tid).unwrap();
            // 遷移状態はあるが確定文書が無い（begin 直後）。
            let nav = st.navigations().get_or_create(reg, &tid);
            nav.begin_navigation().unwrap();
            let p = format!(r#"{{"frameId":"{}"}}"#, tid.as_str());
            let f = call(&st, &sid, 1, "Page.createIsolatedWorld", &p).await;
            assert_eq!(
                f[0]["error"],
                json!({"code": -32000, "message": "document not available for target"})
            );
            assert_eq!(reg.exec_context_count(), 0);
        }

        #[tokio::test]
        async fn cdp2_resolve_context_rejects_stale_generation_after_navigation() {
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let (tid, sid) = page_session(&st);
            call(&st, &sid, 1, "Runtime.enable", "{}").await;
            let nav = st.navigations().get(&tid).unwrap();
            let old = nav.current_generation();
            assert!(st.registry().resolve_context(&sid, 1, old).is_ok());
            call(&st, &sid, 2, "Page.navigate", r#"{"url":"about:blank"}"#).await;
            // 遷移後の古い contextId は未知（遷移の確定で表から消える）。新しい id は現在の世代で解決できる。
            assert_eq!(
                st.registry()
                    .resolve_context(&sid, 1, nav.current_generation()),
                Err(ContextLookupError::UnknownContext)
            );
            assert!(
                st.registry()
                    .resolve_context(&sid, 2, nav.current_generation())
                    .is_ok()
            );
            // 世代だけが食い違う場合は Stale。
            assert_eq!(
                st.registry().resolve_context(&sid, 2, old),
                Err(ContextLookupError::Stale)
            );
        }

        #[test]
        fn cdp7_closed_targets_release_tracked_scopes() {
            // 上限（TrackedScopes）を超える回数のターゲット作成・閉鎖を繰り返しても、閉じた scope が
            // 解放されて追跡数が頭打ちにならないこと（`prune`・`forget`・`get_or_create` の刈り込み）。
            let dir = TempDir::new();
            let st = enabled_state(&dir);
            let reg = st.registry();
            let handle = st.app_state().script_contexts();
            for _ in 0..2200 {
                let tid = reg.create_target(TargetKind::Page, "about:blank").unwrap();
                let nav = st.navigations().get_or_create(reg, &tid);
                let g = nav.begin_navigation().unwrap();
                handle
                    .begin_generation(nav.scope(), g)
                    .expect("tracked scopes must be released");
                reg.close_target(&tid).unwrap();
                st.navigations().prune(reg);
            }
            for _ in 0..1200 {
                let tid = reg.create_target(TargetKind::Page, "about:blank").unwrap();
                let nav = st.navigations().get_or_create(reg, &tid);
                let g = nav.begin_navigation().unwrap();
                handle
                    .begin_generation(nav.scope(), g)
                    .expect("forget releases");
                reg.close_target(&tid).unwrap();
                st.navigations().forget(&tid);
            }
            assert_eq!(handle.stats().worker_spawns(), 0);
        }
    }
}
