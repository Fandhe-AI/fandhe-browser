//! egress プロキシの可観測性カウンタ（許可・拒否の理由別・上限超過。RENDER-3・TASK-38・#1434）。
//!
//! render crate にはロギング基盤が無く、cli の stdout を汚さないためログは出さず、
//! 件数のスナップショット [`EgressProxyStats`] で観測させる（REPAIR-4）。
//! 本モジュールは Unix 専用（待ち受けの実装が Unix のみのため）。

use std::sync::atomic::{AtomicU64, Ordering};

use super::EgressProxyStats;

/// カウンタを加算する事象。`EgressProxyStats` のフィールドと 1 対 1 に対応する。
#[derive(Clone, Copy, Debug)]
pub(super) enum Event {
    Accepted,
    Allowed,
    DeniedAddress,
    MethodNotAllowed,
    BadRequest,
    HeaderTooLarge,
    HeaderDeadline,
    OverCapacity,
    ResolveFailed,
    UpstreamFailed,
    IdleTimeout,
    SpawnFailed,
}

/// 内部カウンタ（Relaxed。件数は観測用で、他の状態との順序保証は要らない）。
#[derive(Default)]
pub(super) struct Counters {
    accepted: AtomicU64,
    allowed: AtomicU64,
    denied_address: AtomicU64,
    method_not_allowed: AtomicU64,
    bad_request: AtomicU64,
    header_too_large: AtomicU64,
    header_deadline: AtomicU64,
    over_capacity: AtomicU64,
    resolve_failed: AtomicU64,
    upstream_failed: AtomicU64,
    idle_timeout: AtomicU64,
    spawn_failed: AtomicU64,
}

impl Counters {
    /// 事象を 1 件加算する。
    pub(super) fn bump(&self, event: Event) {
        let slot = match event {
            Event::Accepted => &self.accepted,
            Event::Allowed => &self.allowed,
            Event::DeniedAddress => &self.denied_address,
            Event::MethodNotAllowed => &self.method_not_allowed,
            Event::BadRequest => &self.bad_request,
            Event::HeaderTooLarge => &self.header_too_large,
            Event::HeaderDeadline => &self.header_deadline,
            Event::OverCapacity => &self.over_capacity,
            Event::ResolveFailed => &self.resolve_failed,
            Event::UpstreamFailed => &self.upstream_failed,
            Event::IdleTimeout => &self.idle_timeout,
            Event::SpawnFailed => &self.spawn_failed,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// 現時点の件数を構造体で返す。
    pub(super) fn snapshot(&self) -> EgressProxyStats {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        EgressProxyStats {
            accepted: get(&self.accepted),
            allowed: get(&self.allowed),
            denied_address: get(&self.denied_address),
            method_not_allowed: get(&self.method_not_allowed),
            bad_request: get(&self.bad_request),
            header_too_large: get(&self.header_too_large),
            header_deadline: get(&self.header_deadline),
            over_capacity: get(&self.over_capacity),
            resolve_failed: get(&self.resolve_failed),
            upstream_failed: get(&self.upstream_failed),
            idle_timeout: get(&self.idle_timeout),
            spawn_failed: get(&self.spawn_failed),
        }
    }
}
