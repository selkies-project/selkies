use super::{param_header::*, param_type::*, *};

use bytes::{Bytes, BytesMut};
use rand::RngExt;
use std::fmt;

#[derive(Default, Debug, Clone, PartialEq)]
pub(crate) struct ParamStateCookie {
    pub(crate) cookie: Bytes,
}

/// String makes paramStateCookie printable
impl fmt::Display for ParamStateCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?}", self.header(), self.cookie)
    }
}

impl Param for ParamStateCookie {
    fn header(&self) -> ParamHeader {
        ParamHeader {
            typ: ParamType::StateCookie,
            value_length: self.value_length() as u16,
        }
    }

    fn unmarshal(raw: &Bytes) -> Result<Self> {
        let header = ParamHeader::unmarshal(raw)?;
        let cookie = raw.slice(PARAM_HEADER_LENGTH..PARAM_HEADER_LENGTH + header.value_length());
        Ok(ParamStateCookie { cookie })
    }

    fn marshal_to(&self, buf: &mut BytesMut) -> Result<usize> {
        self.header().marshal_to(buf)?;
        buf.extend_from_slice(&self.cookie);
        Ok(buf.len())
    }

    fn value_length(&self) -> usize {
        self.cookie.len()
    }

    fn clone_to(&self) -> Box<dyn Param> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ParamStateCookie {
    pub(crate) fn new() -> Self {
        // This 256-bit value is an unpredictable challenge echoed by the peer. SCTP remains usable
        // without an RTC crypto provider, so this deliberately uses `rand`'s thread-local CSPRNG.
        let mut cookie = BytesMut::new();
        cookie.resize(32, 0);
        rand::rng().fill(cookie.as_mut());

        ParamStateCookie {
            cookie: cookie.freeze(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_state_cookies_have_full_entropy_width() {
        assert_eq!(ParamStateCookie::new().cookie.len(), 32);
    }

    #[test]
    fn generated_state_cookies_are_distinct() {
        assert_ne!(
            ParamStateCookie::new().cookie,
            ParamStateCookie::new().cookie
        );
    }
}
