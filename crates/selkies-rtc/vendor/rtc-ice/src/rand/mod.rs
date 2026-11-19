use crypto::RTCRandom;
use shared::error::{Error, Result};
use shared::util::generate_crypto_random_string;
use stun::message::{Message, Setter, TransactionId};

#[cfg(test)]
mod rand_test;

const RUNES_ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const RUNES_CANDIDATE_ID_FOUNDATION: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/+";

const LEN_UFRAG: usize = 16;
const LEN_PWD: usize = 32;

/// <https://tools.ietf.org/html/rfc5245#section-15.1>
/// candidate-id = "candidate" ":" foundation
/// foundation   = 1*32ice-char
/// ice-char     = ALPHA / DIGIT / "+" / "/"
///
/// Candidate IDs provide local uniqueness and diagnostics; they are not credentials or
/// cryptographic identities. This standalone helper therefore uses `rand`'s thread-local CSPRNG
/// rather than requiring a crypto provider.
pub fn generate_cand_id() -> String {
    format!(
        "candidate:{}",
        generate_crypto_random_string(32, RUNES_CANDIDATE_ID_FOUNDATION)
    )
}

fn generate_string_with_random(
    length: usize,
    alphabet: &[u8],
    random: &dyn RTCRandom,
) -> Result<String> {
    debug_assert!(!alphabet.is_empty() && alphabet.len() <= u8::MAX as usize + 1);

    let acceptance_limit = 256 - (256 % alphabet.len());
    let mut output = String::with_capacity(length);
    let mut random_bytes = [0_u8; 64];
    while output.len() < length {
        random
            .fill(&mut random_bytes)
            .map_err(|error| Error::Crypto(error.to_string()))?;
        for byte in random_bytes {
            if byte as usize >= acceptance_limit {
                continue;
            }
            output.push(alphabet[byte as usize % alphabet.len()] as char);
            if output.len() == length {
                break;
            }
        }
    }
    Ok(output)
}

/// Generates an ICE password with `rand`'s thread-local CSPRNG.
///
/// ICE agents use the provider-backed internal variant. This function remains as a standalone
/// compatibility helper for callers that do not own an [`crypto::RTCCryptoProvider`].
pub fn generate_pwd() -> String {
    generate_crypto_random_string(LEN_PWD, RUNES_ALPHA)
}

/// Generates an ICE password using an explicitly supplied cryptographically secure random source.
pub(crate) fn generate_pwd_with_random(random: &dyn RTCRandom) -> Result<String> {
    generate_string_with_random(LEN_PWD, RUNES_ALPHA, random)
}

/// Generates the 64-bit ICE role-conflict tie breaker from the agent's random provider.
pub(crate) fn generate_tie_breaker(random: &dyn RTCRandom) -> Result<u64> {
    let mut bytes = [0_u8; std::mem::size_of::<u64>()];
    random
        .fill(&mut bytes)
        .map_err(|error| Error::Crypto(error.to_string()))?;
    Ok(u64::from_be_bytes(bytes))
}

/// Generates an ICE username fragment with `rand`'s thread-local CSPRNG.
///
/// ICE agents use the provider-backed internal variant. This function remains as a standalone
/// compatibility helper for callers that do not own an [`crypto::RTCCryptoProvider`].
pub fn generate_ufrag() -> String {
    generate_crypto_random_string(LEN_UFRAG, RUNES_ALPHA)
}

/// Generates an ICE username fragment using an explicitly supplied cryptographically secure random
/// source.
pub(crate) fn generate_ufrag_with_random(random: &dyn RTCRandom) -> Result<String> {
    generate_string_with_random(LEN_UFRAG, RUNES_ALPHA, random)
}

/// Sets a fresh STUN transaction ID drawn from the agent's random provider.
///
/// [`TransactionId::new`] draws from `rand`. The agent owns a [`crypto::RTCCryptoProvider`], so
/// its connectivity checks take their IDs from the same source as its credentials and tie
/// breaker. A failing source fails the `Message::build` this setter is applied in.
pub(crate) struct RandomTransactionId<'a>(pub(crate) &'a dyn RTCRandom);

impl Setter for RandomTransactionId<'_> {
    fn add_to(&self, m: &mut Message) -> Result<()> {
        let mut id = TransactionId::default();
        self.0
            .fill(&mut id.0)
            .map_err(|error| Error::Crypto(error.to_string()))?;
        id.add_to(m)
    }
}
