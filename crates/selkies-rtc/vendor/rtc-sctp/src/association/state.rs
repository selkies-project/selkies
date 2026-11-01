use std::fmt;

/// association state enums
#[derive(Default, Debug, Copy, Clone, PartialEq)]
pub(crate) enum AssociationState {
    #[default]
    Closed = 0,
    CookieWait = 1,
    CookieEchoed = 2,
    Established = 3,
    ShutdownAckSent = 4,
    ShutdownPending = 5,
    ShutdownReceived = 6,
    ShutdownSent = 7,
}

impl From<u8> for AssociationState {
    fn from(v: u8) -> AssociationState {
        match v {
            1 => AssociationState::CookieWait,
            2 => AssociationState::CookieEchoed,
            3 => AssociationState::Established,
            4 => AssociationState::ShutdownAckSent,
            5 => AssociationState::ShutdownPending,
            6 => AssociationState::ShutdownReceived,
            7 => AssociationState::ShutdownSent,
            _ => AssociationState::Closed,
        }
    }
}

impl fmt::Display for AssociationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            AssociationState::Closed => "Closed",
            AssociationState::CookieWait => "CookieWait",
            AssociationState::CookieEchoed => "CookieEchoed",
            AssociationState::Established => "Established",
            AssociationState::ShutdownPending => "ShutdownPending",
            AssociationState::ShutdownSent => "ShutdownSent",
            AssociationState::ShutdownReceived => "ShutdownReceived",
            AssociationState::ShutdownAckSent => "ShutdownAckSent",
        };
        write!(f, "{}", s)
    }
}

impl AssociationState {
    pub(crate) fn is_drained(&self) -> bool {
        matches!(
            *self,
            AssociationState::ShutdownSent
                | AssociationState::ShutdownAckSent
                | AssociationState::ShutdownPending
                | AssociationState::ShutdownReceived
        )
    }
}

/// ack mode (for testing)
#[derive(Default, Debug, Copy, Clone, PartialEq)]
pub(crate) enum AckMode {
    #[default]
    Normal,
    NoDelay,
    AlwaysDelay,
}

impl fmt::Display for AckMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            AckMode::Normal => "Normal",
            AckMode::NoDelay => "NoDelay",
            AckMode::AlwaysDelay => "AlwaysDelay",
        };
        write!(f, "{}", s)
    }
}

/// ack transmission state
#[derive(Default, Debug, Copy, Clone, PartialEq)]
pub(crate) enum AckState {
    #[default]
    Idle, // ack timer is off
    Immediate, // will send ack immediately
    Delay,     // ack timer is on (ack is being delayed)
}

impl fmt::Display for AckState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            AckState::Idle => "Idle",
            AckState::Immediate => "Immediate",
            AckState::Delay => "Delay",
        };
        write!(f, "{}", s)
    }
}
