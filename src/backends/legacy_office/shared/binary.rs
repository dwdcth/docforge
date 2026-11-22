// 吸收自 firecrawl/anydoc 0.2.5（MIT，`src/shared/binary.rs`），见 THIRD_PARTY_NOTICES.md。
//! Shared primitives for the legacy OLE2 binary formats (DOC, PPT):
//! checked little-endian integer readers and bounded compound-file stream
//! reading.

#[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
use crate::backends::legacy_office::error::ConvertError;
#[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
use crate::backends::legacy_office::package::limits;
#[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
use std::io::Read;

/// Little-endian `u16` at `off`; `None` when out of bounds.
pub(crate) fn get_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..)?.get(..2)?.try_into().ok()?))
}

/// Little-endian `u32` at `off`; `None` when out of bounds.
pub(crate) fn get_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..)?.get(..4)?.try_into().ok()?))
}

/// The UTF-16LE code units of `bytes`; a trailing odd byte is dropped.
pub(crate) fn utf16le_units(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair))
}

/// Read a named stream from an OLE2 compound file. A missing stream is
/// `MissingPart`; the read is hard-capped at `MAX_ENTRY_BYTES` so a corrupt
/// sector chain cannot expand without bound.
#[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
pub(crate) fn read_ole_stream<R: Read + std::io::Seek>(
    ole: &mut cfb::CompoundFile<R>,
    name: &str,
) -> Result<Vec<u8>, ConvertError> {
    let stream =
        ole.open_stream(name).map_err(|_| ConvertError::MissingPart { part: name.to_string() })?;
    let mut bytes = Vec::new();
    let read = stream.take(limits::MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes).map_err(|e| {
        ConvertError::Malformed {
            part: Some(name.to_string()),
            detail: format!("unreadable stream: {e}"),
        }
    })? as u64;
    if read > limits::MAX_ENTRY_BYTES {
        return Err(ConvertError::ResourceLimit {
            limit: "max_entry_bytes",
            detail: format!("{name} stream exceeds the read cap"),
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_reads_reject_overflowing_offsets() {
        assert_eq!(get_u16(&[1, 2, 3, 4], usize::MAX), None);
        assert_eq!(get_u32(&[1, 2, 3, 4], usize::MAX), None);
    }
}
