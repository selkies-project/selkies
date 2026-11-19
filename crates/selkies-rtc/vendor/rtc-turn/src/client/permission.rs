#[derive(Default, Copy, Clone, PartialEq, Debug)]
pub(crate) enum PermState {
    #[default]
    Idle = 0,
    Permitted = 1,
}

impl From<u8> for PermState {
    fn from(v: u8) -> Self {
        match v {
            0 => PermState::Idle,
            _ => PermState::Permitted,
        }
    }
}

#[derive(Default)]
pub(crate) struct Permission {
    st: PermState,
}

impl Permission {
    pub(crate) fn set_state(&mut self, state: PermState) {
        self.st = state;
    }

    pub(crate) fn state(&self) -> PermState {
        self.st
    }
}
