//! MBR partition table editing (the Pi 2 boot ROM only understands MBR).

pub const SECTOR: u64 = 512;
/// Leave growth alone if less than 1 MiB would be gained.
const MIN_GAIN_SECTORS: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    pub kind: u8,
    pub start: u32,
    pub sectors: u32,
}

fn entry_offset(partno: usize) -> usize {
    446 + 16 * (partno - 1)
}

pub fn is_valid(mbr: &[u8; 512]) -> bool {
    mbr[510] == 0x55 && mbr[511] == 0xaa
}

/// Read primary partition `partno` (1..=4).
pub fn partition(mbr: &[u8; 512], partno: usize) -> Option<Partition> {
    if !(1..=4).contains(&partno) || !is_valid(mbr) {
        return None;
    }
    let e = &mbr[entry_offset(partno)..entry_offset(partno) + 16];
    let p = Partition {
        kind: e[4],
        start: u32::from_le_bytes(e[8..12].try_into().unwrap()),
        sectors: u32::from_le_bytes(e[12..16].try_into().unwrap()),
    };
    (p.kind != 0 && p.sectors != 0).then_some(p)
}

/// Extend partition `partno` to the end of a disk of `disk_sectors`.
/// Returns the new partition on change. Refuses if another partition lies
/// behind it, so only the last partition can grow.
pub fn grow_to_end(mbr: &mut [u8; 512], partno: usize, disk_sectors: u64) -> Option<Partition> {
    let p = partition(mbr, partno)?;
    let end = p.start as u64 + p.sectors as u64;
    let others_behind = (1..=4).filter(|&n| n != partno).filter_map(|n| partition(mbr, n)).any(|o| o.start as u64 >= end);
    let new_sectors = disk_sectors.saturating_sub(p.start as u64).min(u32::MAX as u64);
    if others_behind || new_sectors < p.sectors as u64 + MIN_GAIN_SECTORS {
        return None;
    }
    let off = entry_offset(partno);
    mbr[off + 12..off + 16].copy_from_slice(&(new_sectors as u32).to_le_bytes());
    // Mark CHS end as "beyond 8 GB" so tools rely on LBA.
    mbr[off + 5..off + 8].copy_from_slice(&[0xfe, 0xff, 0xff]);
    Some(Partition { sectors: new_sectors as u32, ..p })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(parts: &[(u8, u32, u32)]) -> [u8; 512] {
        let mut m = [0u8; 512];
        for (i, &(kind, start, sectors)) in parts.iter().enumerate() {
            let o = entry_offset(i + 1);
            m[o + 4] = kind;
            m[o + 8..o + 12].copy_from_slice(&start.to_le_bytes());
            m[o + 12..o + 16].copy_from_slice(&sectors.to_le_bytes());
        }
        m[510] = 0x55;
        m[511] = 0xaa;
        m
    }

    #[test]
    fn grows_last_partition() {
        let mut m = table(&[(0x0c, 8192, 262144), (0x83, 270336, 524288), (0x83, 794624, 524288), (0x83, 1318912, 131072)]);
        let disk = 31_116_288; // 16 GB card
        let p = grow_to_end(&mut m, 4, disk).unwrap();
        assert_eq!(p.start as u64 + p.sectors as u64, disk);
        assert_eq!(partition(&m, 4).unwrap(), p);
        // Already full: no change.
        assert!(grow_to_end(&mut m, 4, disk).is_none());
    }

    #[test]
    fn refuses_non_last_partition() {
        let mut m = table(&[(0x0c, 8192, 1000), (0x83, 20000, 1000)]);
        assert!(grow_to_end(&mut m, 1, 1_000_000).is_none());
    }

    #[test]
    fn rejects_missing_signature() {
        let mut m = table(&[(0x83, 2048, 2048)]);
        m[510] = 0;
        assert!(partition(&m, 1).is_none());
    }
}
