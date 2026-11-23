//! Xbox Series X Frame Pacer (`SetFrameIntervalX`, `ScheduleFrameEventX`, `WaitFrameEventX`,
//! `GetFrameStatisticsX`, `PresentX` token tracking & `ImmediateThresholdPercent`, `SuspendX`/`ResumeX`).
//!
//! Uses an injectable `FrameClock` (`SteadyFrameClock` for real runtime pacing, `ManualFrameClock`
//! for deterministic unit/integration testing).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use xsx_abi::com::{HResult, E_INVALIDARG, E_INVALID_STATE, E_WAIT_TIMEOUT, S_FALSE, S_OK};

pub const FRAME_EVENT_ORIGIN: u32 = 0;

pub const STAT_FRAME_INTERVAL: u32 = 0x01;
pub const STAT_FRAME_EVENT: u32 = 0x02;
pub const STAT_RENDER: u32 = 0x04;
pub const STAT_PRESENT: u32 = 0x08;
pub const STAT_DISPLAY: u32 = 0x10;

pub const INFINITE_TIMEOUT_MS: u32 = 0xFFFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentSync {
    Vblank,
    Immediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenIssue {
    None,
    NullToken,
    Stale,
    Repeated,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStatistic {
    pub stat_type: u32,
    pub plane_index: u32,
    pub token: u64,
    pub time_us: u64,
    pub percent_scanned: f32,
}

/// Compute whether a late frame tears (`PresentSync::Immediate`) or waits for next vblank,
/// matching Xbox `D3D12XBOX_PRESENT_PARAMETERS::ImmediateThresholdPercent`.
pub fn decide_present_sync(
    late_by_us: u64,
    interval_us: u32,
    threshold_percent: f32,
) -> PresentSync {
    if !(threshold_percent > 0.0) || late_by_us == 0 || interval_us == 0 {
        return PresentSync::Vblank;
    }
    let scanned = 100.0 * (late_by_us % interval_us as u64) as f64 / interval_us as f64;
    if scanned <= (threshold_percent.min(100.0) as f64) {
        PresentSync::Immediate
    } else {
        PresentSync::Vblank
    }
}

pub fn scan_percent(late_by_us: u64, interval_us: u32) -> f32 {
    if interval_us == 0 {
        0.0
    } else {
        (100.0 * (late_by_us % interval_us as u64) as f64 / interval_us as f64) as f32
    }
}

/// Clock source for [`FramePacer`].
pub trait FrameClock: Send + Sync {
    fn now_us(&self) -> u64;
    fn is_manual(&self) -> bool {
        false
    }
}

/// Real monotonic wall-clock implementation backed by `std::time::Instant`.
pub struct SteadyFrameClock {
    start: Instant,
}

impl SteadyFrameClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SteadyFrameClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameClock for SteadyFrameClock {
    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

/// Deterministic manual clock for tests.
pub struct ManualFrameClock {
    now_us: AtomicU64,
}

impl ManualFrameClock {
    pub fn new(initial_us: u64) -> Self {
        Self {
            now_us: AtomicU64::new(initial_us),
        }
    }

    pub fn advance_us(&self, delta_us: u64) {
        self.now_us.fetch_add(delta_us, Ordering::SeqCst);
    }
}

impl FrameClock for ManualFrameClock {
    fn now_us(&self) -> u64 {
        self.now_us.load(Ordering::SeqCst)
    }

    fn is_manual(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
struct FrameRecord {
    token: u64,
    origin_us: u64,
    due_vblank_us: u64,
    presented: bool,
    present_us: u64,
    displayed: bool,
    display_us: u64,
    percent_scanned: f32,
}

#[derive(Debug, Default)]
struct PacerState {
    interval_set: bool,
    armed: bool,
    suspended: bool,
    period_us: u32,
    latency: u32,
    epoch_us: u64,
    offset_us: u32,
    last_origin: i64,
    next_token: u64,
    last_presented_token: u64,
    in_flight: VecDeque<u64>,
    records: VecDeque<FrameRecord>,
}

/// Thread-safe Xbox Series X frame pacer and token tracker.
pub struct FramePacer {
    clock: Arc<dyn FrameClock>,
    state: Mutex<PacerState>,
    cvar: Condvar,
}

impl FramePacer {
    pub fn new(clock: Arc<dyn FrameClock>) -> Self {
        Self {
            clock,
            state: Mutex::new(PacerState {
                next_token: 1,
                ..Default::default()
            }),
            cvar: Condvar::new(),
        }
    }

    pub fn new_steady() -> Self {
        Self::new(Arc::new(SteadyFrameClock::new()))
    }

    pub fn wake_all(&self) {
        self.cvar.notify_all();
    }

    pub fn is_registered(&self) -> bool {
        self.state.lock().unwrap().interval_set
    }

    pub fn is_suspended(&self) -> bool {
        self.state.lock().unwrap().suspended
    }

    pub fn set_frame_interval(&self, period_us: u32, latency: u32, _flags: u32) -> HResult {
        if period_us == 0 {
            return E_INVALIDARG;
        }
        let mut st = self.state.lock().unwrap();
        if st.suspended {
            return E_INVALID_STATE;
        }
        st.period_us = period_us;
        st.latency = latency.max(1);
        st.epoch_us = self.clock.now_us();
        st.interval_set = true;
        st.offset_us = st.offset_us.min(period_us - 1);
        st.last_origin = 0;
        self.cvar.notify_all();
        S_OK
    }

    pub fn schedule_frame_event(&self, event: u32, offset_us: u32, _flags: u32) -> HResult {
        if event != FRAME_EVENT_ORIGIN {
            return E_INVALIDARG;
        }
        let mut st = self.state.lock().unwrap();
        if st.suspended || !st.interval_set {
            return E_INVALID_STATE;
        }
        if offset_us >= st.period_us {
            return E_INVALIDARG;
        }
        st.offset_us = offset_us;
        st.armed = true;
        let now = self.clock.now_us();
        let base = st.epoch_us + st.offset_us as u64;
        st.last_origin = if now >= base {
            ((now - base) / st.period_us as u64) as i64
        } else {
            0
        };
        self.cvar.notify_all();
        S_OK
    }

    pub fn wait_frame_event(
        &self,
        event: u32,
        timeout_ms: u32,
        _flags: u32,
    ) -> Result<u64, HResult> {
        if event != FRAME_EVENT_ORIGIN {
            return Err(E_INVALIDARG);
        }
        let mut st = self.state.lock().unwrap();
        let start_us = self.clock.now_us();
        let deadline_us = if timeout_ms == INFINITE_TIMEOUT_MS {
            u64::MAX
        } else {
            start_us.saturating_add(timeout_ms as u64 * 1000)
        };

        loop {
            if st.suspended || !st.interval_set || !st.armed {
                return Err(E_INVALID_STATE);
            }
            let now = self.clock.now_us();
            Self::retire_locked(&mut st, now);

            let next_origin_idx = st.last_origin + 1;
            let next_origin_us =
                st.epoch_us + (next_origin_idx as u64) * (st.period_us as u64) + st.offset_us as u64;

            if now >= next_origin_us && (st.in_flight.len() as u32) < st.latency {
                // Skip missed origins to newest passed origin
                let passed_idx = ((now - st.epoch_us - st.offset_us as u64) / st.period_us as u64) as i64;
                let chosen_idx = passed_idx.max(next_origin_idx);
                let origin_us =
                    st.epoch_us + (chosen_idx as u64) * (st.period_us as u64) + st.offset_us as u64;
                let due_vblank_us = st.epoch_us + ((chosen_idx + 1) as u64) * (st.period_us as u64);
                st.last_origin = chosen_idx;
                let token = st.next_token;
                st.next_token += 1;
                if st.records.len() >= 64 {
                    st.records.pop_front();
                }
                st.records.push_back(FrameRecord {
                    token,
                    origin_us,
                    due_vblank_us,
                    presented: false,
                    present_us: 0,
                    displayed: false,
                    display_us: 0,
                    percent_scanned: 0.0,
                });
                return Ok(token);
            }

            if now >= deadline_us {
                return Err(E_WAIT_TIMEOUT);
            }
            if self.clock.is_manual() {
                // Non-blocking in manual single-thread mode when time hasn't reached next origin
                return Err(E_WAIT_TIMEOUT);
            }
            let wait_until = next_origin_us.min(deadline_us);
            let sleep_us = wait_until.saturating_sub(now).min(2000);
            let (guard, _) = self
                .cvar
                .wait_timeout(st, Duration::from_micros(sleep_us.max(50)))
                .unwrap();
            st = guard;
        }
    }

    pub fn check_present_token(&self, token: u64) -> Result<TokenIssue, HResult> {
        let st = self.state.lock().unwrap();
        if token == 0 {
            return Ok(TokenIssue::NullToken);
        }
        if token >= st.next_token {
            return Err(E_INVALIDARG);
        }
        if token == st.last_presented_token {
            return Ok(TokenIssue::Repeated);
        }
        if token < st.last_presented_token {
            return Ok(TokenIssue::Stale);
        }
        Ok(TokenIssue::None)
    }

    pub fn present_timing(&self, token: u64) -> (u64, u32) {
        let st = self.state.lock().unwrap();
        let now = self.clock.now_us();
        let late = st
            .records
            .iter()
            .find(|r| r.token == token)
            .map(|r| now.saturating_sub(r.due_vblank_us))
            .unwrap_or(0);
        (late, st.period_us)
    }

    pub fn commit_present_token(&self, token: u64, sync: PresentSync) {
        let mut st = self.state.lock().unwrap();
        let now = self.clock.now_us();
        Self::retire_locked(&mut st, now);
        if token != 0 {
            st.last_presented_token = st.last_presented_token.max(token);
            let period = st.period_us;
            let epoch_us = st.epoch_us;
            let mut due_disp = now;
            if let Some(rec) = st.records.iter_mut().find(|r| r.token == token) {
                rec.presented = true;
                rec.present_us = now;
                let late = now.saturating_sub(rec.due_vblank_us);
                if sync == PresentSync::Immediate {
                    rec.displayed = true;
                    rec.display_us = now;
                    rec.percent_scanned = scan_percent(late, period);
                } else {
                    due_disp = if period > 0 {
                        let next_vb = ((now.saturating_sub(epoch_us)) / period as u64 + 1)
                            * period as u64
                            + epoch_us;
                        next_vb.max(rec.due_vblank_us)
                    } else {
                        now
                    };
                    rec.display_us = due_disp;
                    if now >= due_disp {
                        rec.displayed = true;
                    }
                }
            }
            if sync == PresentSync::Vblank {
                st.in_flight.push_back(due_disp);
            }
        }
        self.cvar.notify_all();
    }

    pub fn get_frame_statistics(
        &self,
        token: u64,
        type_mask: u32,
        max_out: usize,
    ) -> (HResult, Vec<FrameStatistic>) {
        let mut st = self.state.lock().unwrap();
        let now = self.clock.now_us();
        Self::retire_locked(&mut st, now);

        let rec = st.records.iter().find(|r| r.token == token);
        let mut found = Vec::new();
        let mut expected = 0usize;

        for (bit, present, time, pct) in [
            (
                STAT_FRAME_EVENT,
                rec.is_some(),
                rec.map(|r| r.origin_us).unwrap_or(0),
                0.0,
            ),
            (
                STAT_PRESENT,
                rec.map(|r| r.presented).unwrap_or(false),
                rec.map(|r| r.present_us).unwrap_or(0),
                0.0,
            ),
            (
                STAT_DISPLAY,
                rec.map(|r| r.displayed).unwrap_or(false),
                rec.map(|r| r.display_us).unwrap_or(0),
                rec.map(|r| r.percent_scanned).unwrap_or(0.0),
            ),
        ] {
            if (type_mask & bit) != 0 {
                expected += 1;
                if present {
                    found.push(FrameStatistic {
                        stat_type: bit,
                        plane_index: 0,
                        token,
                        time_us: time,
                        percent_scanned: pct,
                    });
                }
            }
        }
        found.truncate(max_out);
        let hr = if !found.is_empty() && found.len() == expected {
            S_OK
        } else {
            S_FALSE
        };
        (hr, found)
    }

    pub fn suspend(&self) {
        let mut st = self.state.lock().unwrap();
        st.suspended = true;
        st.armed = false;
        self.cvar.notify_all();
    }

    pub fn resume(&self) {
        let mut st = self.state.lock().unwrap();
        st.suspended = false;
        self.cvar.notify_all();
    }

    fn retire_locked(st: &mut PacerState, now: u64) {
        while let Some(&due) = st.in_flight.front() {
            if now >= due {
                st.in_flight.pop_front();
            } else {
                break;
            }
        }
        for rec in &mut st.records {
            if rec.presented && !rec.displayed && now >= rec.display_us {
                rec.displayed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paces_frames_tracks_tokens_and_decides_tearing() {
        let clock = Arc::new(ManualFrameClock::new(0));
        let pacer = FramePacer::new(clock.clone());

        assert_eq!(pacer.set_frame_interval(16_667, 2, 0), S_OK);
        assert_eq!(pacer.schedule_frame_event(FRAME_EVENT_ORIGIN, 0, 0), S_OK);

        // Before first vblank -> timeout
        assert_eq!(
            pacer.wait_frame_event(FRAME_EVENT_ORIGIN, 0, 0),
            Err(E_WAIT_TIMEOUT)
        );

        // Advance to vblank 1 -> yields token 1
        clock.advance_us(16_667);
        let tok1 = pacer.wait_frame_event(FRAME_EVENT_ORIGIN, 0, 0).unwrap();
        assert_eq!(tok1, 1);
        assert_eq!(pacer.check_present_token(tok1), Ok(TokenIssue::None));
        assert_eq!(pacer.check_present_token(99), Err(E_INVALIDARG));

        // Tearing threshold check
        assert_eq!(decide_present_sync(1_000, 16_667, 20.0), PresentSync::Immediate);
        assert_eq!(decide_present_sync(8_000, 16_667, 20.0), PresentSync::Vblank);
        assert_eq!(decide_present_sync(1_000, 16_667, 0.0), PresentSync::Vblank);

        pacer.commit_present_token(tok1, PresentSync::Vblank);
        let (hr, stats) = pacer.get_frame_statistics(tok1, STAT_FRAME_EVENT | STAT_PRESENT, 4);
        assert_eq!(hr, S_OK);
        assert_eq!(stats.len(), 2);

        // Suspend disables waits until resumed & rescheduled
        pacer.suspend();
        assert_eq!(
            pacer.wait_frame_event(FRAME_EVENT_ORIGIN, 0, 0),
            Err(E_INVALID_STATE)
        );
        pacer.resume();
        assert_eq!(pacer.schedule_frame_event(FRAME_EVENT_ORIGIN, 0, 0), S_OK);
    }
}
