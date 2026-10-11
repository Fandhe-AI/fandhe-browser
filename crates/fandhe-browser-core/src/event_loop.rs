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

#[derive(Debug, Clone)]
struct Entry {
    /// 順序キー `(期限ms, 登録順)`。interval の発火中は `order` に無い。
    key: (u64, u64),
    interval_ms: Option<u64>,
}

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
            },
        );
        Ok(id)
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// タイマーを取り消す。存在しない ID は何もしない。
    pub fn clear(&mut self, id: TimerId) {
        if let Some(entry) = self.entries.remove(&id) {
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
        if !is_interval {
            self.entries.remove(&id);
        }
        NextStep::Due(id)
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
}
