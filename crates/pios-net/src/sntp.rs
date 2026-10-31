//! SNTPv4 (RFC 4330) request/response handling.

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
pub const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// A 48-byte client request carrying our transmit time.
pub fn request(transmit_unix_nanos: u128) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = (4 << 3) | 3; // LI=0, VN=4, Mode=3 (client)
    p[40..48].copy_from_slice(&to_ntp(transmit_unix_nanos).to_be_bytes());
    p
}

fn to_ntp(unix_nanos: u128) -> u64 {
    let secs = (unix_nanos / 1_000_000_000) as u64 + NTP_UNIX_OFFSET;
    let frac = (((unix_nanos % 1_000_000_000) as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

fn from_ntp(ts: u64) -> i128 {
    let secs = (ts >> 32) as i128 - NTP_UNIX_OFFSET as i128;
    let nanos = (((ts & 0xffff_ffff) as u128 * 1_000_000_000) >> 32) as i128;
    secs * 1_000_000_000 + nanos
}

/// Compute the clock offset in nanoseconds (server - local) from a reply,
/// given local send (t1) and receive (t4) times. Validates the reply.
pub fn offset(reply: &[u8], t1_unix_nanos: u128, t4_unix_nanos: u128) -> Option<i128> {
    if reply.len() < 48 {
        return None;
    }
    let mode = reply[0] & 0x7;
    let li = reply[0] >> 6;
    let stratum = reply[1];
    if mode != 4 || li == 3 || stratum == 0 || stratum > 15 {
        return None;
    }
    let ts = |at: usize| u64::from_be_bytes(reply[at..at + 8].try_into().unwrap());
    // The server must echo our transmit timestamp as its originate timestamp.
    if ts(24) != to_ntp(t1_unix_nanos) {
        return None;
    }
    let t2 = from_ntp(ts(32));
    let t3 = from_ntp(ts(40));
    let (t1, t4) = (t1_unix_nanos as i128, t4_unix_nanos as i128);
    Some(((t2 - t1) + (t3 - t4)) / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_offset() {
        let t1: u128 = 1_000_000_000 * 1_000_000_000; // local clock at send
        let t4 = t1 + 20_000_000; // 20 ms round trip
        let server_ahead: u128 = 5 * 1_000_000_000; // server is 5 s ahead
        let mut r = [0u8; 48];
        r[0] = (4 << 3) | 4;
        r[1] = 2;
        r[24..32].copy_from_slice(&to_ntp(t1).to_be_bytes());
        r[32..40].copy_from_slice(&to_ntp(t1 + 10_000_000 + server_ahead).to_be_bytes());
        r[40..48].copy_from_slice(&to_ntp(t1 + 10_000_000 + server_ahead).to_be_bytes());
        let off = offset(&r, t1, t4).unwrap();
        // Within NTP fraction rounding.
        assert!((off - server_ahead as i128).abs() < 1_000, "offset {off}");
    }

    #[test]
    fn rejects_kiss_of_death_and_mismatch() {
        let mut r = [0u8; 48];
        r[0] = (4 << 3) | 4;
        r[1] = 0; // stratum 0 = KoD
        assert!(offset(&r, 1, 2).is_none());
        r[1] = 2;
        assert!(offset(&r, 1, 2).is_none()); // originate timestamp mismatch
    }
}
