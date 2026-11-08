use bytes::{Bytes, BytesMut};
use std::io::{Read, Write};

use super::content::*;
use shared::error::Result;

// Application data messages are carried by the record layer and are
// fragmented, compressed, and encrypted based on the current connection
// state.  The messages are treated as transparent data to the record
// layer.
/// ## Specifications
///
/// * [RFC 5246 §10]
///
/// [RFC 5246 §10]: https://tools.ietf.org/html/rfc5246#section-10
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ApplicationData {
    /// The application payload.
    pub data: BytesMut,
}

impl ApplicationData {
    /// The record content type this message is carried in.
    pub fn content_type(&self) -> ContentType {
        ContentType::ApplicationData
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.data)?;

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        // Read straight into the BytesMut-backed Vec instead of staging in a
        // temporary Vec and copying the whole payload a second time.
        let mut data: Vec<u8> = vec![];
        reader.read_to_end(&mut data)?;

        // `Bytes::from(Vec)` is zero-copy and the buffer is uniquely owned
        // here, so `try_into_mut` succeeds without the second full-payload
        // copy the old `BytesMut::from(&data[..])` performed.
        Ok(ApplicationData {
            data: Bytes::from(data)
                .try_into_mut()
                .unwrap_or_else(|b| BytesMut::from(&b[..])),
        })
    }
}
