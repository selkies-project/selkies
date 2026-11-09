use super::*;

use std::io::{BufReader, BufWriter};

#[test]
fn test_extension_use_extended_master_secret() -> Result<()> {
    let raw_extension_use_extended_master_secret = vec![0x00, 0x00];
    let parsed_extension_use_extended_master_secret =
        ExtensionUseExtendedMasterSecret { supported: true };

    let mut raw = vec![];
    {
        let mut writer = BufWriter::<&mut Vec<u8>>::new(raw.as_mut());
        parsed_extension_use_extended_master_secret.marshal(&mut writer)?;
    }

    assert!(
        raw == raw_extension_use_extended_master_secret,
        "extension_use_extended_master_secret marshal"
    );

    let mut reader = BufReader::new(raw.as_slice());
    let new_extension_use_extended_master_secret =
        ExtensionUseExtendedMasterSecret::unmarshal(&mut reader)?;

    assert!(
        new_extension_use_extended_master_secret == parsed_extension_use_extended_master_secret,
        "extension_use_extended_master_secret unmarshal"
    );

    Ok(())
}
