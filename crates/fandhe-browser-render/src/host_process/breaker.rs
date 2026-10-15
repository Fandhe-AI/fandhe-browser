//! ホスト再起動のサーキットブレーカー（RENDER-4。TASK-38・#994・設計 §2.7）。
//!
//! [`super::HostProcess`] が起動前に [`CircuitBreaker::check`] を呼び、失敗（クラッシュ・
//! タイムアウト・プロトコル違反・起動失敗）のたびに [`CircuitBreaker::record_failure`] を呼ぶ。
//! 設計は「60 秒内に 3 回失敗したら一定時間再起動しない」と定めるが、冷却時間の値は
//! 定めていないため [`BREAKER_COOLDOWN`] は暫定値（60 秒）。時刻は引数で受け取り、
//! テストで実時間を待たずに検証できるようにしている。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::HostProcessError;

/// 失敗を数える窓の長さ（設計 §2.7）。
pub const BREAKER_WINDOW: Duration = Duration::from_secs(60);
/// 窓内でこの回数に達するとブレーカーが作動する（設計 §2.7）。
pub const BREAKER_THRESHOLD: usize = 3;
/// 作動後に再起動を止める時間。設計に値の定めが無いため暫定（TASK-38）。
pub const BREAKER_COOLDOWN: Duration = Duration::from_secs(60);

/// 失敗履歴と作動状態。
#[derive(Debug, Default)]
pub struct CircuitBreaker {
    failures: VecDeque<Instant>,
    open_until: Option<Instant>,
    /// 冷却明けの 1 回だけの試行中。ここで失敗したら即座に再作動する。
    half_open: bool,
}

impl CircuitBreaker {
    /// 空のブレーカーを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 起動してよいか確認する。作動中なら [`HostProcessError::CircuitOpen`]。
    /// 冷却が明けていれば半開き状態にして 1 回だけ通す。
    pub fn check(&mut self, now: Instant) -> Result<(), HostProcessError> {
        if let Some(until) = self.open_until {
            if now < until {
                return Err(HostProcessError::CircuitOpen);
            }
            self.open_until = None;
            self.half_open = true;
        }
        Ok(())
    }

    /// 失敗を記録する。
    pub fn record_failure(&mut self, now: Instant) {
        if self.half_open {
            self.trip(now);
            return;
        }
        self.failures.push_back(now);
        while let Some(front) = self.failures.front() {
            if now.saturating_duration_since(*front) > BREAKER_WINDOW {
                self.failures.pop_front();
            } else {
                break;
            }
        }
        if self.failures.len() >= BREAKER_THRESHOLD {
            self.trip(now);
        }
    }

    /// 成功（正常な応答）を記録する。履歴と半開き状態を解除する。
    pub fn record_success(&mut self) {
        self.failures.clear();
        self.half_open = false;
    }

    fn trip(&mut self, now: Instant) {
        self.failures.clear();
        self.half_open = false;
        self.open_until = Some(now + BREAKER_COOLDOWN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn render4_three_failures_within_window_open_the_breaker() {
        let t0 = Instant::now();
        let mut b = CircuitBreaker::new();
        b.record_failure(at(t0, 0));
        b.record_failure(at(t0, 10));
        assert!(b.check(at(t0, 11)).is_ok());
        b.record_failure(at(t0, 20));
        assert!(matches!(
            b.check(at(t0, 21)),
            Err(HostProcessError::CircuitOpen)
        ));
        assert!(matches!(
            b.check(at(t0, 79)),
            Err(HostProcessError::CircuitOpen)
        ));
    }

    #[test]
    fn render4_failures_outside_window_are_not_counted() {
        let t0 = Instant::now();
        let mut b = CircuitBreaker::new();
        b.record_failure(at(t0, 0));
        b.record_failure(at(t0, 30));
        b.record_failure(at(t0, 70));
        assert!(b.check(at(t0, 71)).is_ok());
    }

    #[test]
    fn render4_half_open_allows_one_try_and_failure_reopens() {
        let t0 = Instant::now();
        let mut b = CircuitBreaker::new();
        for i in 0..3 {
            b.record_failure(at(t0, i));
        }
        // 3 回目の失敗は t=2。冷却は 60 秒なので t=62 で半開きになる。
        assert!(b.check(at(t0, 62)).is_ok());
        b.record_failure(at(t0, 63));
        assert!(matches!(
            b.check(at(t0, 64)),
            Err(HostProcessError::CircuitOpen)
        ));
        assert!(b.check(at(t0, 63 + 60)).is_ok());
        b.record_success();
        b.record_failure(at(t0, 130));
        assert!(b.check(at(t0, 131)).is_ok());
    }
}
