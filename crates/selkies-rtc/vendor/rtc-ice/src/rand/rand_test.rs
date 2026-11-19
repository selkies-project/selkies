use super::*;
use crypto::{CryptoError, RTCRandom};
use shared::error::Result;

struct FixedRandom(u8);

impl RTCRandom for FixedRandom {
    fn fill(&self, output: &mut [u8]) -> std::result::Result<(), CryptoError> {
        output.fill(self.0);
        Ok(())
    }
}

struct FailingRandom;

impl RTCRandom for FailingRandom {
    fn fill(&self, _output: &mut [u8]) -> std::result::Result<(), CryptoError> {
        Err(CryptoError::RandomnessFailed)
    }
}

#[test]
fn provider_backed_credentials_have_required_lengths_and_alphabet() -> Result<()> {
    let random = FixedRandom(0);
    let ufrag = generate_ufrag_with_random(&random)?;
    let password = generate_pwd_with_random(&random)?;

    assert_eq!(ufrag.len(), LEN_UFRAG);
    assert_eq!(password.len(), LEN_PWD);
    assert!(ufrag.bytes().all(|byte| RUNES_ALPHA.contains(&byte)));
    assert!(password.bytes().all(|byte| RUNES_ALPHA.contains(&byte)));
    Ok(())
}

#[test]
fn provider_backed_credentials_propagate_randomness_failure() {
    assert!(matches!(
        generate_pwd_with_random(&FailingRandom),
        Err(Error::Crypto(_))
    ));
}

#[test]
fn provider_backed_tie_breaker_uses_all_random_bytes() -> Result<()> {
    assert_eq!(
        generate_tie_breaker(&FixedRandom(0x2a))?,
        u64::from_be_bytes([0x2a; 8])
    );
    Ok(())
}

#[test]
fn provider_backed_tie_breaker_propagates_randomness_failure() {
    assert!(matches!(
        generate_tie_breaker(&FailingRandom),
        Err(Error::Crypto(_))
    ));
}

#[test]
fn test_random_generator_collision() -> Result<()> {
    let test_cases = vec![
        (
            "CandidateID",
            0, /*||-> String {
                   generate_cand_id()
               },*/
        ),
        (
            "PWD", 1, /*||-> String {
                  generate_pwd()
              },*/
        ),
        (
            "Ufrag", 2, /*|| ->String {
                  generate_ufrag()
              },*/
        ),
    ];

    const N: usize = 10;
    const ITERATION: usize = 10;

    for (name, test_case) in test_cases {
        for _ in 0..ITERATION {
            let mut rs = vec![];

            for _ in 0..N {
                let s = if test_case == 0 {
                    generate_cand_id()
                } else if test_case == 1 {
                    generate_pwd()
                } else {
                    generate_ufrag()
                };

                rs.push(s);
            }

            assert_eq!(rs.len(), N, "{name} Failed to generate randoms");

            for i in 0..N {
                for j in i + 1..N {
                    assert_ne!(
                        rs[i], rs[j],
                        "{}: generateRandString caused collision: {} == {}",
                        name, rs[i], rs[j],
                    );
                }
            }
        }
    }

    Ok(())
}
