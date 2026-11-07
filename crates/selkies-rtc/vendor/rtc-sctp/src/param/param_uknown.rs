use std::any::Any;
use std::fmt::{Debug, Display, Formatter};

use bytes::{Bytes, BytesMut};

use crate::param::Param;
use crate::param::param_header::{PARAM_HEADER_LENGTH, ParamHeader};
use crate::param::param_type::ParamType;
use shared::error::Result;

/// This type is meant to represent ANY parameter for un/remarshaling purposes, where we do not have a more specific type for it.
/// This means we do not really understand the semantics of the param but can represent it.
///
/// This is useful for usage in e.g.`ParamUnrecognized` where we want to report some unrecognized params back to the sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamUnknown {
    typ: u16,
    value: Bytes,
}

impl Display for ParamUnknown {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ParamUnknown( {} {:?} )", self.header(), self.value)
    }
}

impl Param for ParamUnknown {
    fn header(&self) -> ParamHeader {
        ParamHeader {
            typ: ParamType::Unknown {
                param_type: self.typ,
            },
            value_length: self.value.len() as u16,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn unmarshal(raw: &Bytes) -> Result<Self>
    where
        Self: Sized,
    {
        let header = ParamHeader::unmarshal(raw)?;
        let value = raw.slice(PARAM_HEADER_LENGTH..PARAM_HEADER_LENGTH + header.value_length());
        Ok(Self {
            typ: header.typ.into(),
            value,
        })
    }

    fn marshal_to(&self, buf: &mut BytesMut) -> Result<usize> {
        self.header().marshal_to(buf)?;
        buf.extend_from_slice(&self.value);
        Ok(buf.len())
    }

    fn value_length(&self) -> usize {
        self.value.len()
    }

    fn clone_to(&self) -> Box<dyn Param> {
        Box::new(self.clone())
    }
}
