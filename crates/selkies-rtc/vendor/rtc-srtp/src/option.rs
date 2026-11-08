use shared::replay_detector::*;

/// A factory for the [`ReplayDetector`] a context
/// should use.
///
/// A factory rather than a value because each SSRC in a session needs its own detector
/// state. Remote contexts default to a 64-packet sliding window; pass a different factory to
/// widen it or to disable replay protection.
pub type ContextOption = Box<dyn Fn() -> Box<dyn ReplayDetector> + Send + Sync>;

pub(crate) const MAX_SEQUENCE_NUMBER: u16 = 65535;
pub(crate) const MAX_SRTCP_INDEX: usize = 0x7FFFFFFF;

/// srtp_replay_protection sets SRTP replay protection window size.
pub fn srtp_replay_protection(window_size: usize) -> ContextOption {
    Box::new(move || -> Box<dyn ReplayDetector> {
        Box::new(WrappedSlidingWindowDetector::new(
            window_size,
            MAX_SEQUENCE_NUMBER as u64,
        ))
    })
}

/// Sets SRTCP replay protection window size.
pub fn srtcp_replay_protection(window_size: usize) -> ContextOption {
    Box::new(move || -> Box<dyn ReplayDetector> {
        Box::new(WrappedSlidingWindowDetector::new(
            window_size,
            MAX_SRTCP_INDEX as u64,
        ))
    })
}

/// srtp_no_replay_protection disables SRTP replay protection.
pub fn srtp_no_replay_protection() -> ContextOption {
    Box::new(|| -> Box<dyn ReplayDetector> { Box::<NoOpReplayDetector>::default() })
}

/// srtcp_no_replay_protection disables SRTCP replay protection.
pub fn srtcp_no_replay_protection() -> ContextOption {
    Box::new(|| -> Box<dyn ReplayDetector> { Box::<NoOpReplayDetector>::default() })
}
