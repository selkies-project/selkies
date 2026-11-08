use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime};

/// Bytes of randomness in a handshake random, excluding the timestamp.
pub const RANDOM_BYTES_LENGTH: usize = 28;
/// Total length of a handshake random: 4 bytes of timestamp plus 28 random.
pub const HANDSHAKE_RANDOM_LENGTH: usize = RANDOM_BYTES_LENGTH + 4;

/// ## Specifications
///
/// * [RFC 4346 §7.4.1.2]
///
/// [RFC 4346 §7.4.1.2]: https://tools.ietf.org/html/rfc4346#section-7.4.1.2
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandshakeRandom {
    /// The sender's clock at the time the random was generated.
    pub gmt_unix_time: SystemTime,
    /// 28 bytes of randomness, which feed key derivation.
    pub random_bytes: [u8; RANDOM_BYTES_LENGTH],
}

impl Default for HandshakeRandom {
    fn default() -> Self {
        HandshakeRandom {
            gmt_unix_time: SystemTime::UNIX_EPOCH,
            random_bytes: [0u8; RANDOM_BYTES_LENGTH],
        }
    }
}

impl HandshakeRandom {
    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        4 + RANDOM_BYTES_LENGTH
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let secs = match self.gmt_unix_time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as u32,
            Err(_) => 0,
        };
        writer.write_u32::<BigEndian>(secs)?;
        writer.write_all(&self.random_bytes)?;

        writer.flush()
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> io::Result<Self> {
        let secs = reader.read_u32::<BigEndian>()?;
        let gmt_unix_time = if let Some(unix_time) =
            SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs as u64, 0))
        {
            unix_time
        } else {
            SystemTime::UNIX_EPOCH
        };

        let mut random_bytes = [0u8; RANDOM_BYTES_LENGTH];
        reader.read_exact(&mut random_bytes)?;

        Ok(HandshakeRandom {
            gmt_unix_time,
            random_bytes,
        })
    }

    // populate fills the HandshakeRandom with random values
    // may be called multiple times
    /// Fills in the current time and fresh random bytes.
    pub fn populate(&mut self, random: &dyn crypto::RTCRandom) -> shared::error::Result<()> {
        self.gmt_unix_time = SystemTime::now(); // Exemption: wall-clock is correct here to generate randomness
        random
            .fill(&mut self.random_bytes)
            .map_err(|error| shared::error::Error::Crypto(error.to_string()))?;
        Ok(())
    }
}
