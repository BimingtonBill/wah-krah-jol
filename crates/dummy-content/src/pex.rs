//! Deterministic Papyrus `.pex` bytecode fixtures.

use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

/// Magic number at the start of every Skyrim `.pex` file (`0xFA57C0DE`).
pub const SKYRIM_MAGIC: u32 = 0xFA57_C0DE;

/// Generates a minimal Skyrim (`3.2`) script whose class is `object_name`.
///
/// The script defines a single empty state containing a `Run` function that
/// returns `None`, which is enough to exercise transpilation and linking.
pub fn minimal(object_name: &str) -> Result<Vec<u8>> {
    ensure!(!object_name.is_empty(), "PEX object name is empty");
    ensure!(
        object_name.is_ascii(),
        "PEX object name is not ASCII: {object_name:?}"
    );
    ensure!(
        !object_name.contains('\0'),
        "PEX object name contains a NUL byte"
    );
    ensure!(
        object_name.len() <= u16::MAX as usize,
        "PEX object name exceeds 65535 bytes"
    );

    fn be16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn be32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn string(out: &mut Vec<u8>, value: &str) -> Result<()> {
        be16(
            out,
            u16::try_from(value.len()).map_err(|_| eyre!("PEX string exceeds 65535 bytes"))?,
        );
        out.extend_from_slice(value.as_bytes());
        Ok(())
    }

    let strings = [object_name, "", "ObjectReference", "Run", "None"];
    let mut bytes = SKYRIM_MAGIC.to_be_bytes().to_vec();
    bytes.extend_from_slice(&[3, 2]);
    be16(&mut bytes, 1);
    bytes.extend_from_slice(&0u64.to_be_bytes());
    for value in ["test.psc", "user", "machine"] {
        string(&mut bytes, value)?;
    }
    be16(
        &mut bytes,
        u16::try_from(strings.len()).map_err(|_| eyre!("PEX string table overflow"))?,
    );
    for value in strings {
        string(&mut bytes, value)?;
    }
    bytes.push(0);
    be16(&mut bytes, 0);
    be16(&mut bytes, 1);
    be16(&mut bytes, 0);
    be32(&mut bytes, 0);
    be16(&mut bytes, 2);
    be16(&mut bytes, 1);
    be32(&mut bytes, 0);
    be16(&mut bytes, 1);
    be16(&mut bytes, 0);
    be16(&mut bytes, 0);
    be16(&mut bytes, 1);
    be16(&mut bytes, 1);
    be16(&mut bytes, 1);
    be16(&mut bytes, 3);
    be16(&mut bytes, 4);
    be16(&mut bytes, 1);
    be32(&mut bytes, 0);
    bytes.push(0);
    be16(&mut bytes, 0);
    be16(&mut bytes, 0);
    be16(&mut bytes, 1);
    bytes.extend_from_slice(&[26, 0]);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_skyrim_pex_header_and_object_name() {
        let bytes = minimal("One").unwrap();
        assert_eq!(bytes[..4], SKYRIM_MAGIC.to_be_bytes());
        assert_eq!(bytes[4..6], [3, 2]);
        assert!(
            bytes.windows(3).any(|window| window == b"One"),
            "object name is missing from the string table"
        );
    }

    #[test]
    fn same_object_name_is_byte_identical() {
        assert_eq!(minimal("One").unwrap(), minimal("One").unwrap());
    }

    #[test]
    fn rejects_invalid_object_names() {
        for name in ["", "bad\0name", "n\u{f6}n"] {
            assert!(minimal(name).is_err(), "name {name:?} was accepted");
        }
    }
}
