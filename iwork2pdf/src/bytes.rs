//! Bounds-checked fixed-width integer reads from byte slices.
//!
//! Each returns `None` when the slice is too short, so callers can use `?` instead of panicking
//! on truncated input (Rust slices panic on out-of-range indexing, unlike Python's slicing).

/// Reads `N` bytes at `offset` as an array.
fn take<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

/// Little-endian `u16` at `offset`.
pub fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    take(bytes, offset).map(u16::from_le_bytes)
}

/// Little-endian `u32` at `offset`.
pub fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    take(bytes, offset).map(u32::from_le_bytes)
}

/// Little-endian `u64` at `offset`.
pub fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    take(bytes, offset).map(u64::from_le_bytes)
}

/// Big-endian `u16` at `offset`.
pub fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    take(bytes, offset).map(u16::from_be_bytes)
}

/// Big-endian `u32` at `offset`.
pub fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    take(bytes, offset).map(u32::from_be_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_byte_orders() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(le_u16(&bytes, 0), Some(0x0201));
        assert_eq!(be_u16(&bytes, 0), Some(0x0102));
        assert_eq!(le_u32(&bytes, 4), Some(0x0807_0605));
        assert_eq!(be_u32(&bytes, 4), Some(0x0506_0708));
        assert_eq!(le_u64(&bytes, 0), Some(0x0807_0605_0403_0201));
    }

    #[test]
    fn short_input_is_none() {
        assert_eq!(le_u32(&[1, 2, 3], 0), None);
        assert_eq!(be_u16(&[1, 2], 1), None);
        assert_eq!(le_u16(&[1, 2], usize::MAX), None);
    }
}
