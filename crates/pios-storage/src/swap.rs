//! Linux swap area header (mkswap equivalent).

pub const PAGE: usize = 4096;

/// Build the first page of a swap device of `size_bytes`.
pub fn header(size_bytes: u64, uuid: [u8; 16]) -> Vec<u8> {
    let mut p = vec![0u8; PAGE];
    let pages = (size_bytes / PAGE as u64) as u32;
    p[1024..1028].copy_from_slice(&1u32.to_le_bytes()); // version
    p[1028..1032].copy_from_slice(&pages.saturating_sub(1).to_le_bytes()); // last_page
    // nr_badpages stays 0.
    p[1036..1052].copy_from_slice(&uuid);
    p[1052..1056].copy_from_slice(b"zram");
    p[PAGE - 10..].copy_from_slice(b"SWAPSPACE2");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout() {
        let h = header(256 << 20, [7; 16]);
        assert_eq!(h.len(), PAGE);
        assert_eq!(&h[PAGE - 10..], b"SWAPSPACE2");
        assert_eq!(u32::from_le_bytes(h[1024..1028].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(h[1028..1032].try_into().unwrap()), 65535);
    }
}
