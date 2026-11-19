//! 操作モードの `/ai/snapshot` の同期と、ref・セレクタの解決（TASK-125・`AISNAP-20`・`AISNAP-17`）。
//!
//! - 同期: [`sync_interactive`] が、確定結果が操作モードのとき core の [`AppState::dom_snapshot`] で
//!   ホストの DOM を取り直し、[`build_interactive_document`] で検査して [`AppState::replace_interactive`] で
//!   差し替える。直前の同期から [`MIN_SYNC_INTERVAL`] 未満なら取り直さない（設計 `servo-host-design.md` §14.6）。
//!   失敗したら古い内容を返さず [`SyncFailure`] を返し、core の確定結果を無効にする（`AISNAP-20`・`AISNAP-21`。
//!   2026-11-19 のオーナー判断）。無効にした後は新しい navigate まで `/ai/snapshot` は [`SyncFailure::SessionLost`]、
//!   CDP は文書なしになる。同期の途中で新しい navigate が始まった場合（[`SyncFailure::Stale`]）は、core が世代を
//!   見て無効化を拒否する（新しい navigate を壊さないため）。混雑で後回しにした場合（TASK-128・TASK-129 の `busy`）も無効にしない形で足す
//! - 伏せる値の第 2 層は core が保存の前に行う（`fandhe_browser_core::interactive` の doc）
//! - 解決: [`resolve_target`] が同期済みの写しの上で ref かセレクタを一意に解決し、ホストの番号を返す。
//!   セレクタはホストへ渡さない（設計 §14.4）。呼び出し元は `POST /ai/act`（TASK-128）
//!
//! # 未実装（REPAIR-3）
//!
//! - 最小間隔の例外（間に `act`・`wait` があれば取り直す）は、`act`・`wait`（TASK-128・TASK-129）が
//!   [`SyncTracker`] の記録を消す形で足す

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use fandhe_browser_core::dom::NodeId;
use fandhe_browser_core::interactive::{InteractiveDocument, build_interactive_document};
use fandhe_browser_core::render::RenderError;
use fandhe_browser_core::state::PageMode;
use fandhe_browser_core::{AppState, NavigationGeneration, StateError, query_selector_all_str};

use crate::snapshot::build::build_snapshot_with_holders;

/// 読み取り契機の同期の最小間隔（設計 §14.6）。
pub const MIN_SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// ref・セレクタの長さの上限（バイト。`wait` の条件の文字列と同じ。設計 §14.6）。
pub const MAX_TARGET_BYTES: usize = 1024;

/// 最後に同期した世代と時刻（[`MIN_SYNC_INTERVAL`] の判定に使う）。[`crate::plugin_api::AiState`] が持つ。
///
/// ロックは同期の判定から保存・記録までを直列化する役も持つ（[`sync_interactive`]）。
#[derive(Debug, Default)]
pub struct SyncTracker {
    last: Mutex<Option<(NavigationGeneration, Instant)>>,
}

impl SyncTracker {
    /// 記録なしで作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// `generation` のページをいま取得した、と記録する（`POST /ai/navigate` が読み込みと同時に DOM を
    /// 取得したとき。直後の `/ai/snapshot` で取り直さない。TASK-127）。
    pub fn record(&self, generation: NavigationGeneration) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((generation, Instant::now()));
    }
}

/// 最後の同期 `last` に対し、`generation` のページを `now` に取り直すべきか。
fn due(
    last: Option<(NavigationGeneration, Instant)>,
    generation: NavigationGeneration,
    now: Instant,
) -> bool {
    match last {
        Some((g, at)) if g == generation => now.saturating_duration_since(at) >= MIN_SYNC_INTERVAL,
        _ => true,
    }
}

/// 同期の失敗（`AISNAP-20`「同期に失敗したら古い内容を返さずエラー」・`AISNAP-21`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncFailure {
    /// ホストが破棄・再起動された（新しい navigate まで戻らない。`AISNAP-21`）。
    SessionLost,
    /// 描画層が無効。
    RenderingDisabled,
    /// 同期の間に新しい navigate が始まった、または確定結果が操作モードでなくなった。
    Stale,
    /// ページが操作モードの条件を満たさなくなった（主フレームの `navigator.webdriver` が `true` でない。
    /// SEC-5・#1609）。`interactive_unavailable` で返す（TASK-127。2026-11-19 のオーナー判断）。
    Unavailable,
    /// ホストの応答の検査・取得に失敗した。
    Failed,
}

impl SyncFailure {
    fn from_render(e: &RenderError) -> Self {
        match e {
            RenderError::SessionLost => Self::SessionLost,
            RenderError::RenderingDisabled => Self::RenderingDisabled,
            RenderError::PageNotLoaded => Self::Stale,
            RenderError::Unsupported | RenderError::InteractiveUnavailable => Self::Unavailable,
            // `dom_snapshot` の時間切れは render の `HostRenderer` がホストを破棄して `SessionLost` で返す
            // （設計 §14.6・TASK-123）。ここへ `Timeout` などが来たら同期の失敗として扱う。
            _ => Self::Failed,
        }
    }

    fn from_state(e: &StateError) -> Self {
        match e {
            StateError::SessionMismatch => Self::SessionLost,
            StateError::Superseded { .. } | StateError::NotStarted | StateError::NotInteractive => {
                Self::Stale
            }
            _ => Self::Failed,
        }
    }
}

/// 確定結果が操作モードなら、必要に応じてホストの DOM を取り直して差し替える。静的モード・未ナビゲート
/// では何もしない（未ナビゲートの扱いは呼び出し元の `/ai/snapshot`）。ブロッキングする。
///
/// 判定・取得・検査・保存・記録を [`SyncTracker`] のロック 1 つで直列化する。並行する要求が
/// それぞれ取得すると、先に取得した古い DOM が後から保存されて巻き戻り、最小間隔も守れないため。
/// 後の要求は前の同期の完了を待ち、その記録を見て取り直さない。ロックの順は「同期 → 描画ゲート →
/// ナビゲーション」で、描画ゲートを持ったまま同期のロックを取る経路は無い。
pub fn sync_interactive(app: &AppState, tracker: &SyncTracker) -> Result<(), SyncFailure> {
    let mut last = tracker.last.lock().unwrap_or_else(PoisonError::into_inner);
    let nav = app.navigation();
    let Some((generation, latest)) = nav.latest_with_generation() else {
        // 前の同期の失敗で無効にしたページは、新しい navigate まで失われたまま（`AISNAP-21`）。
        return if nav.interactive_lost() {
            Err(SyncFailure::SessionLost)
        } else {
            Ok(())
        };
    };
    if latest.mode() != PageMode::Interactive || !due(*last, generation, Instant::now()) {
        return Ok(());
    }
    let result = app
        .dom_snapshot()
        .map_err(|e| SyncFailure::from_render(&e))
        .and_then(|snapshot| build_interactive_document(snapshot).map_err(|_| SyncFailure::Failed))
        .and_then(|sync| {
            app.replace_interactive(generation, sync)
                .map_err(|e| SyncFailure::from_state(&e))
        });
    if let Err(e) = result {
        // 新しい navigate が始まっていれば core が世代とモードを見て拒否する（その結果は消さない）。
        // `PageNotLoaded` は同じ世代でも出る（ホストに現在のページが無い）ため、`Stale` でも無効化を試み、
        // 無効にできたら新しい navigate ではなくページが失われたとして `SessionLost` を返す。
        let invalidated = nav.invalidate_interactive(generation).is_ok();
        return Err(if invalidated && e == SyncFailure::Stale {
            SyncFailure::SessionLost
        } else {
            e
        });
    }
    *last = Some((generation, Instant::now()));
    Ok(())
}

/// 操作の対象の指定（`AISNAP-17`。ref かセレクタのどちらか）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Target {
    /// `/ai/snapshot` の `ref`。
    Ref(String),
    /// CSS セレクタ（本体の写しの上で解決する）。
    Selector(String),
}

/// 解決した対象（写しの要素とホストの番号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resolved {
    node: NodeId,
    number: u32,
}

impl Resolved {
    /// 写し（[`InteractiveDocument::document`]）の要素。照合値（`tag`・`id`・`name`・`type`）はここから読む。
    pub fn node(&self) -> NodeId {
        self.node
    }
    /// ホストの番号（`act` の `node`）。
    pub fn number(&self) -> u32 {
        self.number
    }
}

/// 解決の失敗（`AISNAP-17` の `not_found`・`ambiguous`・`invalid_params`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveError {
    /// 一致する要素が無い、または番号の無い要素（再パースで補われた要素）だけが一致した。
    NotFound,
    /// 2 つ以上の要素が一致した（推測で選ばない）。
    Ambiguous,
    /// ref・セレクタが空・長すぎる・構文が不正。
    InvalidTarget,
    /// 要約の構築に失敗した。
    Snapshot,
}

/// 同期済みの写しの上で `target` を一意に解決する（設計 §14.4）。ref は `/ai/snapshot` と同じ規則で
/// 発行したもの（静的モードと同じ。`AISNAP-20`）と照合する。
pub fn resolve_target(
    doc: &InteractiveDocument,
    target: &Target,
) -> Result<Resolved, ResolveError> {
    let document = doc.document();
    let nodes: Vec<NodeId> = match target {
        Target::Ref(r) => {
            check_len(r)?;
            let (_, holders) =
                build_snapshot_with_holders(document).map_err(|_| ResolveError::Snapshot)?;
            let mut nodes: Vec<NodeId> = holders
                .iter()
                .filter(|h| h.r#ref == *r)
                .map(|h| h.node)
                .collect();
            nodes.dedup();
            nodes
        }
        Target::Selector(s) => {
            check_len(s)?;
            query_selector_all_str(document, document.root(), s)
                .map_err(|_| ResolveError::InvalidTarget)?
        }
    };
    match nodes.as_slice() {
        [] => Err(ResolveError::NotFound),
        [node] => doc
            .host_number(*node)
            .map(|number| Resolved {
                node: *node,
                number,
            })
            .ok_or(ResolveError::NotFound),
        _ => Err(ResolveError::Ambiguous),
    }
}

fn check_len(s: &str) -> Result<(), ResolveError> {
    if s.is_empty() || s.len() > MAX_TARGET_BYTES {
        return Err(ResolveError::InvalidTarget);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fandhe_browser_core::interactive::{HostDomSnapshot, InteractivePageId};

    fn doc(html: &str) -> fandhe_browser_core::interactive::InteractiveSync {
        let snap = HostDomSnapshot::new(
            InteractivePageId::new("s", 1).unwrap(),
            "https://a.example/",
            html.to_owned(),
            32,
            "snap",
        )
        .unwrap();
        build_interactive_document(snap).unwrap()
    }

    /// `AISNAP-17`: セレクタは一意のときだけ番号へ解決し、複数一致・不一致・番号なしは失敗にする。
    #[test]
    fn aisnap17_selector_resolves_only_unique_numbered_element() {
        let d = doc(
            r#"<html data-fandhe-n="0"><head data-fandhe-n="1"></head><body data-fandhe-n="2"><button id="a" data-fandhe-n="3">A</button><button data-fandhe-n="4">B</button><table data-fandhe-n="5"><tr data-fandhe-n="6"><td data-fandhe-n="7">x</td></tr></table></body></html>"#,
        );
        let sel = |s: &str| resolve_target(d.document(), &Target::Selector(s.into()));
        assert_eq!(sel("#a").map(|r| r.number()), Ok(3));
        assert_eq!(sel("button"), Err(ResolveError::Ambiguous));
        assert_eq!(sel("input"), Err(ResolveError::NotFound));
        // tbody は再パースで補われた要素で番号が無い。
        assert_eq!(sel("tbody"), Err(ResolveError::NotFound));
        assert_eq!(sel(""), Err(ResolveError::InvalidTarget));
        assert_eq!(sel("[["), Err(ResolveError::InvalidTarget));
        assert_eq!(
            sel(&"a".repeat(MAX_TARGET_BYTES + 1)),
            Err(ResolveError::InvalidTarget)
        );
    }

    /// `AISNAP-17`・`AISNAP-20`: ref は要約と同じ規則で発行したものと照合し、その要素の番号を返す。
    #[test]
    fn aisnap20_ref_resolves_to_host_number() {
        let d = doc(
            r#"<html data-fandhe-n="0"><head data-fandhe-n="1"></head><body data-fandhe-n="2"><a href="/x" data-fandhe-n="3">Next</a><button data-fandhe-n="4">Go</button></body></html>"#,
        );
        let d = d.document();
        let (_, holders) = build_snapshot_with_holders(d.document()).unwrap();
        let go = holders.iter().find(|h| h.name == "Go").unwrap();
        let r = resolve_target(d, &Target::Ref(go.r#ref.clone())).unwrap();
        assert_eq!(r.number(), 4);
        assert_eq!(r.node(), go.node);
        assert_eq!(
            resolve_target(d, &Target::Ref("e00000000".into())),
            Err(ResolveError::NotFound)
        );
    }

    /// 設計 §14.6: 同じ世代で 1 秒未満なら取り直さない。世代が変われば取り直す。
    #[test]
    fn aisnap20_sync_interval() {
        let s = fandhe_browser_core::NavigationState::new();
        let g1 = s.begin_navigation().unwrap();
        let g2 = s.begin_navigation().unwrap();
        let now = Instant::now();
        assert!(due(None, g1, now));
        let last = Some((g1, now));
        assert!(!due(last, g1, now + Duration::from_millis(999)));
        assert!(due(last, g1, now + MIN_SYNC_INTERVAL));
        assert!(due(last, g2, now));
    }
}
