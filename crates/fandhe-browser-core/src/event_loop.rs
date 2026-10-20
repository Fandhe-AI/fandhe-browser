//! 仮想時間のタイマーキュー（TASK-110・MS-6・Issue #782・ビヘイビア `JS-7`・`JS-6`）。
//!
//! ページ内 JS の `setTimeout` / `setInterval` を、実時間を待たずに期限順に発火させるための
//! スケジューラ。JS エンジンには依存せず、タイマーの登録・取り消し・次に発火すべきものの選択と
//! 上限の判定だけを持つ。
//!
//! 呼び出し元は次の 2 か所。
//!
//! - [`crate::dom_bridge::DomBridge`] の `timerSet` / `timerClear` op（shim `timers.js` から
//!   呼ばれる。遅延値はページ由来の untrusted 入力としてここで正規化する）
//! - [`crate::page_runner`] のイベントループ段（[`TimerQueue::next`] で次の発火を決め、
//!   shim のディスパッチャー `__fandheRunTimer` を評価する）
//!
//! # 上限（`JS-6`。超過は fail-closed で打ち切り、種別と値を診断に残す）
//!
//! | 上限 | 既定 | 境界 |
//! | ---- | ---- | ---- |
//! | 発火回数 | 1,000 | 1,000 回目は発火し、1,001 回目で打ち切る |
//! | 仮想時間 | 30 秒 | 期限が 30,000ms までは発火し、それより後で打ち切る |
//! | 登録済み（保留中）タイマー数 | 1,000 | 1,001 件目の登録を拒否する |
//!
//! 「登録済み」は保留中（まだ発火も取り消しもされていない）の件数で数える。発火済みの
//! one-shot と取り消し済みのタイマーは枠を返す。再帰的な `setTimeout` が発火回数の上限より先に
//! 登録数の上限に当たって種別を誤ることを避けるため。
//!
//! # 遅延の正規化（[`normalize_delay`]）
//!
//! NaN・負数・-∞・0 は 0ms、小数は切り捨て、+∞ と 2,147,483,647ms 超は 2,147,483,647ms
//! （[`MAX_TIMER_DELAY_MS`]）へ丸める。実ブラウザは 2^31-1 超をオーバーフローさせて 0 として
//! 扱うが、本実装は暴走の起点にしないため上限値へ丸める（意図的な差異）。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - HTML のネスト時 4ms クランプは行わない。
//! - 上限値の設定ファイルからの読み込みは TASK-112（#784）の担当。

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// 発火回数の既定上限（`JS-6`）。
pub const DEFAULT_MAX_TIMER_FIRES: usize = 1000;
/// 仮想時間の既定上限（30 秒。`JS-6`）。
pub const DEFAULT_MAX_VIRTUAL_TIME: Duration = Duration::from_secs(30);
/// 保留中タイマー数の既定上限（`JS-6`）。
pub const DEFAULT_MAX_REGISTERED_TIMERS: usize = 1000;
/// 遅延の丸め上限（ミリ秒。`i32::MAX`）。
pub const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// イベントループの上限設定（`JS-6`・`JS-7`）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EventLoopLimits {
    max_timer_fires: usize,
    max_virtual_time: Duration,
    max_registered_timers: usize,
}

impl Default for EventLoopLimits {
    fn default() -> Self {
        Self {
            max_timer_fires: DEFAULT_MAX_TIMER_FIRES,
            max_virtual_time: DEFAULT_MAX_VIRTUAL_TIME,
            max_registered_timers: DEFAULT_MAX_REGISTERED_TIMERS,
        }
    }
}

impl EventLoopLimits {
    /// 発火回数の上限を設定する。
    #[must_use]
    pub fn with_max_timer_fires(mut self, limit: usize) -> Self {
        self.max_timer_fires = limit;
        self
    }

    /// 仮想時間の上限を設定する。
    #[must_use]
    pub fn with_max_virtual_time(mut self, limit: Duration) -> Self {
        self.max_virtual_time = limit;
        self
    }

    /// 保留中タイマー数の上限を設定する。
    #[must_use]
    pub fn with_max_registered_timers(mut self, limit: usize) -> Self {
        self.max_registered_timers = limit;
        self
    }

    /// 発火回数の上限。
    pub fn max_timer_fires(&self) -> usize {
        self.max_timer_fires
    }

    /// 仮想時間の上限。
    pub fn max_virtual_time(&self) -> Duration {
        self.max_virtual_time
    }

    /// 保留中タイマー数の上限。
    pub fn max_registered_timers(&self) -> usize {
        self.max_registered_timers
    }
}

/// タイマー ID（1 始まり。0 は「登録されなかった」を表すため使わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u32);

impl TimerId {
    /// 数値表現。
    pub fn get(self) -> u32 {
        self.0
    }

    /// 検証済みの数値から作る（存在しない ID の取り消しは no-op のため安全）。
    pub(crate) fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

/// 登録を拒否した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterRefused {
    /// 保留中のタイマー数が上限に達している。
    TooManyTimers,
    /// ID を払い出し切った。
    IdsExhausted,
}

/// 発火を止めた上限の種別（保留中タイマー数は登録時に判定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimerLimitKind {
    /// 発火回数の上限。
    TimerFires,
    /// 仮想時間の上限。
    VirtualTime,
}

/// [`TimerQueue::next`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NextStep {
    /// このタイマーを発火する（仮想時計は期限まで進み、発火回数に数え済み）。
    Due(TimerId),
    /// 保留中のタイマーが無い（正常終了）。
    Idle,
    /// 上限に達した。キューは変更しない。
    Limit(TimerLimitKind),
}

/// ページ内 JS から渡された遅延（ミリ秒）を安全な整数へ丸める（untrusted 入力。`JS-7`）。
pub fn normalize_delay(delay_ms: f64) -> u64 {
    // NaN は比較がすべて偽になるため先に弾く。
    if delay_ms.is_nan() || delay_ms <= 0.0 {
        return 0;
    }
    if delay_ms >= MAX_TIMER_DELAY_MS as f64 {
        return MAX_TIMER_DELAY_MS;
    }
    // 0 より大きく上限未満の有限値のため、切り捨てても u64 に収まる。
    delay_ms.floor() as u64
}

#[derive(Clone)]
struct Entry {
    /// 順序キー `(期限ms, 登録順)`。interval の発火中は `order` に無い。
    key: (u64, u64),
    interval_ms: Option<u64>,
    /// 登録時に渡された（正規化済みの）遅延。診断の報告用（Issue #1176）。
    delay_ms: u64,
    /// 登録元の呼び出し stack（opt-in 時だけ。untrusted。Issue #1176）。
    origin: Option<String>,
}

/// 保留中タイマー 1 件の要約（診断用。Issue #1176）。
///
/// `Debug` は手実装で、`origin`（untrusted。URL・トークンを含み得る）は長さだけを出す（`SEC-2`）。
#[derive(Clone, PartialEq, Eq)]
pub struct PendingEntry {
    /// タイマー ID。
    pub id: TimerId,
    /// 期限（仮想時刻・ミリ秒）。
    pub due_ms: u64,
    /// 登録時の遅延（ミリ秒）。
    pub delay_ms: u64,
    /// interval か。
    pub repeat: bool,
    /// 登録元の stack（opt-in 時だけ。untrusted のため `Debug` を通さず扱うこと）。
    pub origin: Option<String>,
}

/// `origin` の中身を出さず長さだけを表示する `Debug` 用ヘルパー。
fn origin_len(origin: &Option<String>) -> Option<usize> {
    origin.as_ref().map(String::len)
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("key", &self.key)
            .field("interval_ms", &self.interval_ms)
            .field("delay_ms", &self.delay_ms)
            .field("origin_len", &origin_len(&self.origin))
            .finish()
    }
}

impl std::fmt::Debug for PendingEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingEntry")
            .field("id", &self.id)
            .field("due_ms", &self.due_ms)
            .field("delay_ms", &self.delay_ms)
            .field("repeat", &self.repeat)
            .field("origin_len", &origin_len(&self.origin))
            .finish()
    }
}

/// 登録元 stack を保持する件数の上限（`JS-6`。超過分は捨てる）。
pub const MAX_RETAINED_TIMER_ORIGINS: usize = 64;
/// 登録元 stack の保持長の上限（UTF-8 バイト）。
pub const MAX_TIMER_ORIGIN_BYTES: usize = 1024;

/// 仮想時計つきのタイマーキュー。
///
/// 順序キーは取り消し時に即削除し、両マップの大きさは保留中の件数以下に保つ
/// （set / clear の繰り返しでの肥大化を防ぐ）。
#[derive(Debug, Default)]
pub struct TimerQueue {
    order: BTreeMap<(u64, u64), TimerId>,
    entries: HashMap<TimerId, Entry>,
    next_id: u32,
    next_seq: u64,
    now_ms: u64,
    fired: usize,
    registration_refused: bool,
    /// `next` が選び、まだ shim に消費されていない発火（`JS-6`）。
    armed: Option<TimerId>,
    /// `origin` を保持しているエントリー数（`set_origin` の上限判定を O(1) にする）。
    retained_origins: usize,
}

impl TimerQueue {
    /// 空のキューを作る（仮想時刻 0）。
    pub fn new() -> Self {
        Self::default()
    }

    /// タイマーを登録する。`delay_ms` は [`normalize_delay`] 済みの値。
    ///
    /// 拒否したときは [`TimerQueue::registration_refused`] が立ち、キューは変わらない。
    pub fn register(
        &mut self,
        delay_ms: u64,
        repeat: bool,
        limits: &EventLoopLimits,
    ) -> Result<TimerId, RegisterRefused> {
        if self.entries.len() >= limits.max_registered_timers {
            self.registration_refused = true;
            return Err(RegisterRefused::TooManyTimers);
        }
        let id = self.next_id.checked_add(1).ok_or_else(|| {
            self.registration_refused = true;
            RegisterRefused::IdsExhausted
        })?;
        self.next_id = id;
        let id = TimerId(id);
        let key = (self.now_ms.saturating_add(delay_ms), self.take_seq());
        self.order.insert(key, id);
        self.entries.insert(
            id,
            Entry {
                key,
                interval_ms: repeat.then_some(delay_ms),
                delay_ms,
                origin: None,
            },
        );
        Ok(id)
    }

    fn release_origin(&mut self, entry: &Entry) {
        if entry.origin.is_some() {
            self.retained_origins = self.retained_origins.saturating_sub(1);
        }
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// 保留中タイマーの登録元 stack を保存する（診断用。Issue #1176）。
    ///
    /// 登録中でない ID・保持件数が [`MAX_RETAINED_TIMER_ORIGINS`] に達した後の要求は捨てる。
    /// 文字列は [`MAX_TIMER_ORIGIN_BYTES`] へ文字境界で切り詰める。エントリーと同時に消えるため、
    /// 発火済みの one-shot・取り消し済みの分は自動的に解放される。
    pub fn set_origin(&mut self, id: TimerId, stack: &str) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        if entry.origin.is_none() {
            if self.retained_origins >= MAX_RETAINED_TIMER_ORIGINS {
                return;
            }
            self.retained_origins += 1;
        }
        let mut end = stack.len().min(MAX_TIMER_ORIGIN_BYTES);
        while !stack.is_char_boundary(end) {
            end -= 1;
        }
        entry.origin = Some(stack[..end].to_string());
    }

    /// 期限順に先頭から最大 `limit` 件の保留中タイマーを返す（診断用。Issue #1176）。
    ///
    /// 走査は `limit` で打ち切る。interval の発火中で `order` に無いものは対象外。
    pub fn pending_entries(&self, limit: usize) -> Vec<PendingEntry> {
        self.order
            .iter()
            .take(limit)
            .filter_map(|(key, id)| {
                let e = self.entries.get(id)?;
                Some(PendingEntry {
                    id: *id,
                    due_ms: key.0,
                    delay_ms: e.delay_ms,
                    repeat: e.interval_ms.is_some(),
                    origin: e.origin.clone(),
                })
            })
            .collect()
    }

    /// タイマーを取り消す。存在しない ID は何もしない。
    pub fn clear(&mut self, id: TimerId) {
        if let Some(entry) = self.entries.remove(&id) {
            self.release_origin(&entry);
            self.order.remove(&entry.key);
        }
    }

    /// 次に発火すべきタイマーを選ぶ。判定順は仮想時間 → 発火回数。
    pub fn next(&mut self, limits: &EventLoopLimits) -> NextStep {
        let Some((&key, &id)) = self.order.first_key_value() else {
            return NextStep::Idle;
        };
        let limit_ms = u64::try_from(limits.max_virtual_time.as_millis()).unwrap_or(u64::MAX);
        if key.0 > limit_ms {
            return NextStep::Limit(TimerLimitKind::VirtualTime);
        }
        if self.fired >= limits.max_timer_fires {
            return NextStep::Limit(TimerLimitKind::TimerFires);
        }
        self.order.remove(&key);
        self.now_ms = self.now_ms.max(key.0);
        self.fired = self.fired.saturating_add(1);
        let is_interval = self
            .entries
            .get(&id)
            .is_some_and(|e| e.interval_ms.is_some());
        if !is_interval && let Some(entry) = self.entries.remove(&id) {
            self.release_origin(&entry);
        }
        self.armed = Some(id);
        NextStep::Due(id)
    }

    /// `next` が選んだ発火を一度だけ消費する。許可が無い・ID が違うなら false（`JS-6`）。
    ///
    /// shim のディスパッチャーがコールバック実行前に呼ぶ。ページが直接ディスパッチャーを呼んでも
    /// 発火回数・仮想時間の上限を回避できないようにする。
    pub fn consume_armed(&mut self, id: TimerId) -> bool {
        if self.armed == Some(id) {
            self.armed = None;
            true
        } else {
            false
        }
    }

    /// コールバック評価後に interval を積み直す。取り消し済み・one-shot は何もしない。
    ///
    /// 評価中の `clearInterval` が効くよう、積み直しは評価の後に行う。新しい登録順を
    /// 振るため、同じ期限の他のタイマーを飢えさせない。
    pub fn rearm_if_alive(&mut self, id: TimerId) {
        let seq = self.take_seq();
        let now = self.now_ms;
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        let Some(interval) = entry.interval_ms else {
            return;
        };
        if self.order.contains_key(&entry.key) {
            return;
        }
        let key = (now.saturating_add(interval), seq);
        entry.key = key;
        self.order.insert(key, id);
    }

    /// ID が登録中、または `next` が選んだ未消費の発火かを返す（shim の保持掃除用。`JS-6`）。
    pub fn is_alive(&self, id: TimerId) -> bool {
        self.entries.contains_key(&id) || self.armed == Some(id)
    }

    /// 保留中のタイマー数。
    pub fn pending(&self) -> usize {
        self.entries.len()
    }

    /// 仮想時計の現在値。
    pub fn virtual_now(&self) -> Duration {
        Duration::from_millis(self.now_ms)
    }

    /// 発火済みの回数。
    pub fn fired(&self) -> usize {
        self.fired
    }

    /// 保留中タイマー数の上限で登録を拒否したか（ランナーがリソース上限として打ち切る）。
    pub fn registration_refused(&self) -> bool {
        self.registration_refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> EventLoopLimits {
        EventLoopLimits::default()
    }

    fn drain(q: &mut TimerQueue, l: &EventLoopLimits) -> Vec<u32> {
        let mut out = Vec::new();
        while let NextStep::Due(id) = q.next(l) {
            out.push(id.get());
            q.rearm_if_alive(id);
        }
        out
    }

    #[test]
    fn js_7_fires_in_deadline_order_and_advances_virtual_clock() {
        let l = limits();
        let mut q = TimerQueue::new();
        let a = q.register(100, false, &l).expect("a");
        let b = q.register(50, false, &l).expect("b");
        assert_eq!(drain(&mut q, &l), vec![b.get(), a.get()]);
        assert_eq!(q.virtual_now(), Duration::from_millis(100));
        assert_eq!(q.pending(), 0);
        assert_eq!(q.fired(), 2);
    }

    #[test]
    fn js_7_same_deadline_fires_in_registration_order() {
        let l = limits();
        let mut q = TimerQueue::new();
        let ids: Vec<u32> = (0..3)
            .map(|_| q.register(10, false, &l).expect("r").get())
            .collect();
        assert_eq!(drain(&mut q, &l), ids);
    }

    #[test]
    fn js_7_clear_removes_timer_and_frees_slot() {
        let l = limits();
        let mut q = TimerQueue::new();
        let a = q.register(10, false, &l).expect("a");
        let b = q.register(20, false, &l).expect("b");
        q.clear(a);
        q.clear(a);
        assert_eq!(q.pending(), 1);
        assert_eq!(drain(&mut q, &l), vec![b.get()]);
    }

    #[test]
    fn js_7_interval_rearms_until_cleared_in_callback() {
        let l = limits();
        let mut q = TimerQueue::new();
        let id = q.register(10, true, &l).expect("i");
        let mut times = Vec::new();
        while let NextStep::Due(d) = q.next(&l) {
            times.push(q.virtual_now().as_millis());
            if times.len() == 3 {
                q.clear(d);
            }
            q.rearm_if_alive(d);
        }
        assert_eq!(times, vec![10, 20, 30]);
        assert_eq!(q.pending(), 0);
        let _ = id;
    }

    #[test]
    fn js_7_normalize_delay_table() {
        assert_eq!(normalize_delay(f64::NAN), 0);
        assert_eq!(normalize_delay(-1.0), 0);
        assert_eq!(normalize_delay(f64::NEG_INFINITY), 0);
        assert_eq!(normalize_delay(0.0), 0);
        assert_eq!(normalize_delay(1.9), 1);
        assert_eq!(normalize_delay(2_147_483_647.0), 2_147_483_647);
        assert_eq!(normalize_delay(2_147_483_648.0), 2_147_483_647);
        assert_eq!(normalize_delay(f64::INFINITY), 2_147_483_647);
    }

    #[test]
    fn js_6_registered_timers_limit_boundary() {
        let l = limits();
        let mut q = TimerQueue::new();
        for _ in 0..1000 {
            q.register(1, false, &l).expect("within limit");
        }
        assert!(!q.registration_refused());
        assert_eq!(
            q.register(1, false, &l),
            Err(RegisterRefused::TooManyTimers)
        );
        assert!(q.registration_refused());
        assert_eq!(q.pending(), 1000);
    }

    #[test]
    fn js_6_cleared_timer_returns_registration_slot() {
        let l = EventLoopLimits::default().with_max_registered_timers(2);
        let mut q = TimerQueue::new();
        let a = q.register(1, false, &l).expect("a");
        q.register(1, false, &l).expect("b");
        q.clear(a);
        q.register(1, false, &l).expect("slot freed");
        assert!(!q.registration_refused());
    }

    #[test]
    fn js_6_timer_fire_limit_boundary() {
        let l = limits();
        let mut q = TimerQueue::new();
        q.register(0, true, &l).expect("interval");
        let mut fires = 0;
        let end = loop {
            match q.next(&l) {
                NextStep::Due(id) => {
                    fires += 1;
                    q.rearm_if_alive(id);
                }
                other => break other,
            }
        };
        assert_eq!(fires, 1000);
        assert_eq!(end, NextStep::Limit(TimerLimitKind::TimerFires));
        assert_eq!(q.pending(), 1);
    }

    #[test]
    fn js_6_virtual_time_limit_boundary() {
        let l = limits();
        let mut q = TimerQueue::new();
        let on_edge = q.register(30_000, false, &l).expect("edge");
        q.register(30_001, false, &l).expect("over");
        assert_eq!(q.next(&l), NextStep::Due(on_edge));
        assert_eq!(q.next(&l), NextStep::Limit(TimerLimitKind::VirtualTime));
        assert_eq!(q.virtual_now(), Duration::from_secs(30));
        assert_eq!(q.pending(), 1);
    }

    #[test]
    fn js_6_id_exhaustion_refuses_registration() {
        let l = limits();
        let mut q = TimerQueue::new();
        q.next_id = u32::MAX;
        assert_eq!(q.register(1, false, &l), Err(RegisterRefused::IdsExhausted));
        assert!(q.registration_refused());
    }

    #[test]
    fn js_6_huge_delay_saturates_deadline() {
        let l = limits();
        let mut q = TimerQueue::new();
        q.now_ms = u64::MAX - 1;
        q.register(MAX_TIMER_DELAY_MS, false, &l).expect("r");
        assert_eq!(q.next(&l), NextStep::Limit(TimerLimitKind::VirtualTime));
    }

    /// JS-7: 保留中の一覧は期限順・件数上限つきで、発火と取り消しで消える（Issue #1176）。
    #[test]
    fn js_7_pending_entries_are_deadline_ordered_and_bounded() {
        let l = limits();
        let mut q = TimerQueue::new();
        let late = q.register(500, false, &l).expect("late");
        let early = q.register(10, true, &l).expect("early");
        let mid = q.register(100, false, &l).expect("mid");
        let got = q.pending_entries(2);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].id, got[0].due_ms, got[0].delay_ms), (early, 10, 10));
        assert!(got[0].repeat);
        assert_eq!((got[1].id, got[1].due_ms, got[1].delay_ms), (mid, 100, 100));
        assert!(!got[1].repeat);
        assert_eq!(q.pending_entries(0).len(), 0);
        q.clear(mid);
        assert_eq!(q.next(&l), NextStep::Due(early));
        // interval は発火中は order に無いため対象外。積み直すと戻る。
        assert_eq!(q.pending_entries(5).len(), 1);
        q.rearm_if_alive(early);
        let got = q.pending_entries(5);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].id, got[0].due_ms), (early, 20));
        assert_eq!((got[1].id, got[1].due_ms), (late, 500));
    }

    /// JS-6: 登録元 stack は切り詰め・件数上限つきで、エントリーと一緒に消える（Issue #1176）。
    #[test]
    fn sec_2_debug_of_queue_and_pending_entry_hides_origin_text() {
        let l = limits();
        let mut q = TimerQueue::new();
        let id = q.register(1, false, &l).expect("id");
        q.set_origin(id, "at https://x.test/?token=SECRET");
        let entries = q.pending_entries(8);
        let d1 = format!("{:?}", entries);
        let d2 = format!("{:?}", q);
        for d in [&d1, &d2] {
            assert!(!d.contains("SECRET"), "{d}");
            assert!(!d.contains("x.test"), "{d}");
        }
        assert!(d1.contains("origin_len: Some(31)"), "{d1}");
    }

    #[test]
    fn js_6_timer_origin_is_truncated_bounded_and_released() {
        let l = limits();
        let mut q = TimerQueue::new();
        let first = q.register(1, false, &l).expect("first");
        // 1024 バイト境界にまたがる多バイト文字でも文字境界で切る。
        let long = format!("{}{}", "a".repeat(1023), "あ");
        q.set_origin(first, &long);
        let got = q.pending_entries(1);
        assert_eq!(got[0].origin.as_deref(), Some("a".repeat(1023).as_str()));
        q.set_origin(TimerId::from_raw(999), "x");
        for _ in 0..(MAX_RETAINED_TIMER_ORIGINS + 5) {
            let id = q.register(2, false, &l).expect("r");
            q.set_origin(id, "s");
        }
        let retained = q
            .pending_entries(usize::MAX)
            .iter()
            .filter(|e| e.origin.is_some())
            .count();
        assert_eq!(retained, MAX_RETAINED_TIMER_ORIGINS);
        assert_eq!(q.next(&l), NextStep::Due(first));
        q.consume_armed(first);
        q.clear(first);
        assert_eq!(q.pending_entries(1)[0].id.get(), 2);
    }
}
