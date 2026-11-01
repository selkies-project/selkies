use std::time::{Duration, Instant};

// A lone pending SACK flushes after this: the receive path acknowledges every second packet
// anyway, and a peer sets its retransmission timer from the round trip this delay inflates.
pub(crate) const ACK_INTERVAL: u64 = 20;
const MAX_INIT_RETRANS: usize = 8;
const PATH_MAX_RETRANS: usize = 5;
const NO_MAX_RETRANS: usize = usize::MAX;
const TIMER_COUNT: usize = 6;

#[derive(Debug, Copy, Clone)]
/// Retransmission limits for the association's timers.
///
/// Each field caps how many times the corresponding timer may fire before the association is
/// abandoned. `Default` follows the RFC 9260 recommendations.
pub struct TimerConfig {
    /// How many times INIT may be retransmitted (T1-init) before the association fails.
    pub max_t1_init_retrans: usize,
    /// How many times COOKIE-ECHO may be retransmitted (T1-cookie).
    pub max_t1_cookie_retrans: usize,
    /// How many times SHUTDOWN may be retransmitted (T2-shutdown).
    pub max_t2_shutdown_retrans: usize,
    /// How many times a DATA chunk may be retransmitted on T3-rtx expiry.
    ///
    /// Defaults to unlimited, leaving reliability to the per-stream partial-reliability settings.
    pub max_t3_rtx_retrans: usize,
    /// How many times a RE-CONFIG chunk (stream reset) may be retransmitted.
    pub max_reconfig_retrans: usize,
    /// How many times a delayed SACK may be retransmitted.
    pub max_ack_retrans: usize,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            max_t1_init_retrans: MAX_INIT_RETRANS,
            max_t1_cookie_retrans: MAX_INIT_RETRANS,
            max_t2_shutdown_retrans: NO_MAX_RETRANS,
            max_t3_rtx_retrans: PATH_MAX_RETRANS,
            max_reconfig_retrans: PATH_MAX_RETRANS,
            max_ack_retrans: PATH_MAX_RETRANS,
        }
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub(crate) enum Timer {
    T1Init = 0,
    T1Cookie = 1,
    T2Shutdown = 2,
    T3RTX = 3,
    Reconfig = 4,
    Ack = 5,
}

impl Timer {
    pub(crate) const VALUES: [Self; TIMER_COUNT] = [
        Timer::T1Init,
        Timer::T1Cookie,
        Timer::T2Shutdown,
        Timer::T3RTX,
        Timer::Reconfig,
        Timer::Ack,
    ];
}

/// A table of data associated with each distinct kind of `Timer`
#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct TimerTable {
    data: [Option<Instant>; TIMER_COUNT],
    retrans: [usize; TIMER_COUNT],
    max_retrans: [usize; TIMER_COUNT],
    shutdown_interval: Option<u64>,
    reconfig_interval: Option<u64>,
}

impl TimerTable {
    pub fn new(time_config: TimerConfig) -> Self {
        TimerTable {
            max_retrans: [
                time_config.max_t1_init_retrans,     //T1Init
                time_config.max_t1_cookie_retrans,   //T1Cookie
                time_config.max_t2_shutdown_retrans, //T2Shutdown
                time_config.max_t3_rtx_retrans,      //T3RTX
                time_config.max_reconfig_retrans,    //Reconfig
                time_config.max_ack_retrans,         //Ack
            ],
            ..Default::default()
        }
    }

    pub fn set(&mut self, timer: Timer, time: Option<Instant>) {
        self.data[timer as usize] = time;
    }

    pub fn get(&self, timer: Timer) -> Option<Instant> {
        self.data[timer as usize]
    }

    pub fn next_timeout(&self) -> Option<Instant> {
        self.data.iter().filter_map(|&x| x).min()
    }

    fn control_interval(&mut self, timer: Timer) -> Option<&mut Option<u64>> {
        match timer {
            Timer::T2Shutdown => Some(&mut self.shutdown_interval),
            Timer::Reconfig => Some(&mut self.reconfig_interval),
            _ => None,
        }
    }

    pub fn start(&mut self, timer: Timer, now: Instant, interval: u64) {
        // T3 uses the destination's current RTO, which already includes backoff.
        // Successful DATA acknowledgments reset its error counter independently.
        let interval = if let Some(current) = self.control_interval(timer) {
            // Seed T2/RECONFIG from the current DATA RTO, then double their own
            // interval on expiry. Rearming after serialization must neither
            // compound T3 backoff nor apply this control timer's backoff twice.
            // See RFC 9260 9.2 and RFC 6525 5.1.1.
            *current.get_or_insert(interval)
        } else if matches!(timer, Timer::Ack | Timer::T3RTX) {
            interval
        } else {
            calculate_next_timeout(interval, self.retrans[timer as usize])
        };

        let time = now + Duration::from_millis(interval);
        self.data[timer as usize] = Some(time);
    }

    /// Start an idle timer without hiding an unhandled expiration. A due
    /// deadline stays armed until timeout processing consumes it, unless an
    /// explicit protocol restart first clears it (for T3, acknowledgment of
    /// the earliest outstanding TSN under RFC 9260 section 6.3.2 R3).
    pub fn start_if_idle(&mut self, timer: Timer, now: Instant, interval: u64) {
        if self.data[timer as usize].is_none() {
            self.start(timer, now, interval);
        }
    }

    pub fn stop(&mut self, timer: Timer) {
        self.data[timer as usize] = None;
        self.reset_retrans(timer);
        if let Some(interval) = self.control_interval(timer) {
            *interval = None;
        }
    }

    pub fn reset_retrans(&mut self, timer: Timer) {
        self.retrans[timer as usize] = 0;
    }

    pub fn is_expired(&mut self, timer: Timer, after: Instant) -> (bool, bool, usize) {
        let expired = self.data[timer as usize].is_some_and(|x| x <= after);
        let mut failure = false;
        if expired && let Some(interval) = self.control_interval(timer) {
            *interval = interval.map(|previous| calculate_next_timeout(previous, 1));
        }
        if expired {
            self.retrans[timer as usize] += 1;
            if self.retrans[timer as usize] > self.max_retrans[timer as usize] {
                failure = true;
            }
        }

        (expired, failure, self.retrans[timer as usize])
    }
}

// RFC 9260 §16 recommends a one-second initial RTO.  This value is used by
// both T1-init and T1-cookie before an RTT measurement is available.
const RTO_INITIAL: u64 = 1000; // msec
// A lost message nothing follows (a cursor, a clipboard, a control reply) is repaired by
// T3 alone, so the floor is its whole cost. Chrome's dcSCTP floor: it stays above a peer's
// delayed SACK (200 ms in dcSCTP) plus a round trip, so a peer that ignores the I-bit is
// not sent spurious retransmissions.
const RTO_MIN: u64 = 400; // msec
const RTO_MAX: u64 = 60000; // msec
const RTO_ALPHA: u64 = 1;
const RTO_BETA: u64 = 2;
const RTO_BASE: u64 = 8;

/// rtoManager manages Rtx timeout values.
/// This is an implementation of RFC 9260 sec 6.3.1.
#[derive(Default, Debug)]
pub(crate) struct RtoManager {
    pub(crate) srtt: u64,
    pub(crate) rttvar: f64,
    pub(crate) rto: u64,
    pub(crate) no_update: bool,
}

impl RtoManager {
    /// newRTOManager creates a new rtoManager.
    pub(crate) fn new() -> Self {
        RtoManager {
            rto: RTO_INITIAL,
            ..Default::default()
        }
    }

    /// set_new_rtt takes a newly measured RTT then adjust the RTO in msec.
    pub(crate) fn set_new_rtt(&mut self, rtt: u64) -> u64 {
        if self.no_update {
            return self.srtt;
        }

        if self.srtt == 0 {
            // First measurement
            self.srtt = rtt;
            self.rttvar = rtt as f64 / 2.0;
        } else {
            // Subsequent rtt measurement
            self.rttvar = ((RTO_BASE - RTO_BETA) as f64 * self.rttvar
                + RTO_BETA as f64 * (self.srtt as i64 - rtt as i64).abs() as f64)
                / RTO_BASE as f64;
            self.srtt = ((RTO_BASE - RTO_ALPHA) * self.srtt + RTO_ALPHA * rtt) / RTO_BASE;
        }

        self.rto = (self.srtt + (4.0 * self.rttvar) as u64).clamp(RTO_MIN, RTO_MAX);

        self.srtt
    }

    /// get_rto simply returns the current RTO in msec.
    pub(crate) fn get_rto(&self) -> u64 {
        self.rto
    }

    /// RFC 9260 6.3.3 E2: retain backoff until a new RTT measurement updates RTO.
    pub(crate) fn backoff(&mut self) {
        if !self.no_update {
            self.rto = self.rto.saturating_mul(2).min(RTO_MAX);
        }
    }

    /// reset resets the RTO variables to the initial values.
    pub(crate) fn reset(&mut self) {
        if self.no_update {
            return;
        }

        self.srtt = 0;
        self.rttvar = 0.0;
        self.rto = RTO_INITIAL;
    }

    /// set RTO value for testing
    pub(crate) fn set_rto(&mut self, rto: u64, no_update: bool) {
        self.rto = rto;
        self.no_update = no_update;
    }
}

fn calculate_next_timeout(rto: u64, n_rtos: usize) -> u64 {
    // RFC 4096 sec 6.3.3.  Handle T3-rtx Expiration
    //   E2)  For the destination address for which the timer expires, set RTO
    //        <- RTO * 2 ("back off the timer").  The maximum value discussed
    //        in rule C7 above (RTO.max) may be used to provide an upper bound
    //        to this doubling operation.
    if n_rtos < 31 {
        std::cmp::min(rto << n_rtos, RTO_MAX)
    } else {
        RTO_MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rto_backoff_respects_pinned_rto() {
        let mut rto = RtoManager::new();
        rto.set_rto(1000, true);
        rto.backoff();
        assert_eq!(1000, rto.get_rto());
        rto.set_rto(1000, false);
        rto.backoff();
        assert_eq!(2000, rto.get_rto());
        rto.set_rto(RTO_MAX - 1, false);
        rto.backoff();
        assert_eq!(RTO_MAX, rto.get_rto());
    }

    #[test]
    fn rto_manager_uses_rfc9260_initial_rto() {
        assert_eq!(RTO_INITIAL, 1000);
        assert_eq!(RtoManager::new().get_rto(), RTO_INITIAL);
    }

    #[test]
    fn t1_init_starts_at_initial_rto_before_an_rtt_is_measured() {
        let now = Instant::now();
        let mut timers = TimerTable::new(TimerConfig::default());

        timers.start(Timer::T1Init, now, RtoManager::new().get_rto());

        assert_eq!(timers.next_timeout(), Some(now + Duration::from_secs(1)));
    }

    #[test]
    fn control_backoff_uses_one_interval_until_stopped() {
        for timer in [Timer::T2Shutdown, Timer::Reconfig] {
            let mut timers = TimerTable::new(TimerConfig {
                max_reconfig_retrans: usize::MAX,
                ..TimerConfig::default()
            });
            let mut now = Instant::now();
            // A new procedure uses the current RTO, including previous DATA
            // backoff. Later DATA RTT updates/backoffs cannot compound it.
            timers.start(timer, now, 8000);
            assert_eq!(Some(now + Duration::from_secs(8)), timers.get(timer));
            for interval in [16, 32, 60, 60] {
                now = timers.get(timer).unwrap();
                assert_eq!((true, false), {
                    let (expired, failed, _) = timers.is_expired(timer, now);
                    (expired, failed)
                });
                timers.set(timer, None);
                for data_rto in [60_000, 1000] {
                    timers.start(timer, now, data_rto);
                    assert_eq!(Some(now + Duration::from_secs(interval)), timers.get(timer));
                }
            }
            timers.stop(timer);
            timers.start(timer, now, 3000);
            assert_eq!(Some(now + Duration::from_secs(3)), timers.get(timer));
            assert_eq!((false, false, 0), timers.is_expired(timer, now));
        }
    }
}
