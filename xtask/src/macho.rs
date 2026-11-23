//! A Mach-O executable's UUID (its `LC_UUID` load command), rewritten for one
//! `app-dev` copy. macOS keeps an app's Local Network permission against the
//! UUIDs of its executable, not its bundle id alone: every app-dev copy of
//! one build (Kiri Repro, Kiri Gate, ...) shares the binary's UUID, so a copy
//! that was never allowed takes local-network access away from one that
//! was, and its pulls from a registry on the LAN fail with "No route to
//! host" (os error 65). Each copy gets a UUID of its own, derived from the
//! binary's and the bundle id: the same for the same build and name. Only
//! the 16 UUID bytes change; the copy is signed again afterwards.

use std::io;

const LC_UUID: u32 = 0x1b;

/// Replaces the UUID of every architecture in `bin` (a thin 32/64-bit Mach-O
/// or a fat one) with one derived from it and the bundle id. Errors when the file
/// is not a Mach-O or an architecture has no `LC_UUID`.
pub fn restamp_uuid(bin: &mut [u8], bundle_id: &str) -> io::Result<()> {
    let be = |at: usize| -> io::Result<u32> { read_u32(bin, at, true) };
    match be(0)? {
        // fat_header, then fat_arch (20 bytes) or fat_arch_64 (32 bytes) entries, big-endian.
        m @ (0xcafe_babe | 0xcafe_babf) => {
            let wide = m == 0xcafe_babf;
            let n = be(4)? as usize;
            let mut slices = Vec::with_capacity(n);
            for i in 0..n {
                let e = 8 + i * if wide { 32 } else { 20 };
                let (off, size) = if wide {
                    (read_u64(bin, e + 8)?, read_u64(bin, e + 16)?)
                } else {
                    (u64::from(be(e + 8)?), u64::from(be(e + 12)?))
                };
                slices.push((off as usize, size as usize));
            }
            for (off, size) in slices {
                let end = off.checked_add(size).filter(|&e| e <= bin.len()).ok_or_else(|| bad("fat slice out of range"))?;
                restamp_thin(&mut bin[off..end], bundle_id)?;
            }
            Ok(())
        }
        _ => restamp_thin(bin, bundle_id),
    }
}

fn restamp_thin(bin: &mut [u8], bundle_id: &str) -> io::Result<()> {
    let header = match read_u32(bin, 0, false)? {
        0xfeed_facf => 32,
        0xfeed_face => 28,
        _ => return Err(bad("not a little-endian Mach-O")),
    };
    let ncmds = read_u32(bin, 16, false)?;
    let mut at = header;
    for _ in 0..ncmds {
        let (cmd, size) = (read_u32(bin, at, false)?, read_u32(bin, at + 4, false)? as usize);
        if size < 8 {
            return Err(bad("load command too small"));
        }
        if cmd == LC_UUID {
            let uuid = bin.get_mut(at + 8..at + 24).ok_or_else(|| bad("LC_UUID out of range"))?;
            let new = derive(uuid.try_into().expect("16 bytes"), bundle_id);
            uuid.copy_from_slice(&new);
            return Ok(());
        }
        at += size;
    }
    Err(bad("no LC_UUID"))
}

/// FNV-1a (128-bit) of the old UUID and the bundle id, marked as an RFC 4122
/// name-based UUID as ld's own are.
fn derive(old: [u8; 16], bundle_id: &str) -> [u8; 16] {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut h = OFFSET;
    for b in old.iter().chain(b"\0").chain(bundle_id.as_bytes()) {
        h = (h ^ u128::from(*b)).wrapping_mul(PRIME);
    }
    let mut u = h.to_be_bytes();
    u[6] = (u[6] & 0x0f) | 0x50;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

fn read_u32(b: &[u8], at: usize, big: bool) -> io::Result<u32> {
    let v: [u8; 4] = b.get(at..at + 4).ok_or_else(|| bad("truncated"))?.try_into().expect("4 bytes");
    Ok(if big { u32::from_be_bytes(v) } else { u32::from_le_bytes(v) })
}

fn read_u64(b: &[u8], at: usize) -> io::Result<u64> {
    let v: [u8; 8] = b.get(at..at + 8).ok_or_else(|| bad("truncated"))?.try_into().expect("8 bytes");
    Ok(u64::from_be_bytes(v))
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("Mach-O: {why}"))
}

#[cfg(test)]
/// The UUID of a thin Mach-O, for tests and the build's log line.
pub fn uuid_of(bin: &[u8]) -> Option<[u8; 16]> {
    let header = match read_u32(bin, 0, false).ok()? {
        0xfeed_facf => 32,
        0xfeed_face => 28,
        _ => return None,
    };
    let mut at = header;
    for _ in 0..read_u32(bin, 16, false).ok()? {
        let (cmd, size) = (read_u32(bin, at, false).ok()?, read_u32(bin, at + 4, false).ok()? as usize);
        if cmd == LC_UUID {
            return bin.get(at + 8..at + 24)?.try_into().ok();
        }
        if size < 8 {
            return None;
        }
        at += size;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: [u8; 16] = [0x31, 0xbe, 0x63, 0x97, 0x19, 0xec, 0x3b, 0xe8, 0xb5, 0x45, 0x95, 0x7e, 0x65, 0xd1, 0x35, 0x7c];

    /// A thin arm64 Mach-O header with three load commands: a dummy one, LC_UUID, another dummy, then some "code".
    fn thin() -> Vec<u8> {
        let mut b = Vec::new();
        for v in [0xfeed_facfu32, 0x0100_000c, 0, 2, 3, 16 + 24 + 16, 0, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64 stand-in
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&[0xaa; 8]);
        b.extend_from_slice(&LC_UUID.to_le_bytes());
        b.extend_from_slice(&24u32.to_le_bytes());
        b.extend_from_slice(&UUID);
        b.extend_from_slice(&0x32u32.to_le_bytes());
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&[0xbb; 8]);
        b.extend_from_slice(&[0xcc; 64]);
        b
    }

    /// Where the UUID's bytes are in `thin()`.
    const AT: std::ops::Range<usize> = 32 + 16 + 8..32 + 16 + 24;

    #[test]
    fn each_name_gets_its_own_uuid_and_nothing_else_changes() {
        let orig = thin();
        let (mut a, mut b, mut a2) = (orig.clone(), orig.clone(), orig.clone());
        restamp_uuid(&mut a, "dev.kiri.app.repro").unwrap();
        restamp_uuid(&mut b, "dev.kiri.app.lanprobe").unwrap();
        restamp_uuid(&mut a2, "dev.kiri.app.repro").unwrap();
        let (ua, ub) = (uuid_of(&a).unwrap(), uuid_of(&b).unwrap());
        assert_ne!(ua, UUID);
        assert_ne!(ua, ub);
        assert_eq!(a, a2, "the same build and name give the same UUID");
        for v in [&a, &b] {
            assert_eq!(v.len(), orig.len());
            assert_eq!(v[..AT.start], orig[..AT.start]);
            assert_eq!(v[AT.end..], orig[AT.end..]);
        }
        // Still an RFC 4122 name-based UUID.
        assert_eq!((ua[6] >> 4, ua[8] >> 6), (5, 2));
    }

    #[test]
    fn every_slice_of_a_fat_binary_is_restamped() {
        let slice = thin();
        let off = 4096usize;
        let mut fat = Vec::new();
        for v in [0xcafe_babeu32, 2] {
            fat.extend_from_slice(&v.to_be_bytes());
        }
        for i in 0..2 {
            for v in [0x0100_000c_u32, 0, (off * (i + 1)) as u32, slice.len() as u32, 12] {
                fat.extend_from_slice(&v.to_be_bytes());
            }
        }
        fat.resize(off * 3, 0);
        fat[off..off + slice.len()].copy_from_slice(&slice);
        fat[2 * off..2 * off + slice.len()].copy_from_slice(&slice);
        let before = fat.clone();
        restamp_uuid(&mut fat, "dev.kiri.app.gate").unwrap();
        for s in [off, 2 * off] {
            assert_ne!(uuid_of(&fat[s..]).unwrap(), UUID);
        }
        let changed = before.iter().zip(&fat).filter(|(x, y)| x != y).count();
        assert!(changed > 0 && changed <= 32);
    }

    #[test]
    fn a_file_that_is_not_a_mach_o_or_has_no_uuid_is_refused() {
        assert!(restamp_uuid(&mut b"#!/bin/sh\necho hi\n".to_vec(), "x").is_err());
        let mut no_uuid = thin();
        no_uuid[32 + 16..32 + 20].copy_from_slice(&0x2u32.to_le_bytes()); // LC_UUID -> LC_SYMTAB
        assert!(restamp_uuid(&mut no_uuid, "x").is_err());
    }

    /// The xtask binary itself is a real Mach-O on a Mac: restamping it keeps
    /// every byte outside the 16 UUID bytes.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_binary_keeps_every_other_byte() {
        let orig = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let mut copy = orig.clone();
        restamp_uuid(&mut copy, "dev.kiri.app.lanprobe").unwrap();
        assert_ne!(uuid_of(&copy), uuid_of(&orig));
        let diff: Vec<usize> = orig.iter().zip(&copy).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
        assert!(!diff.is_empty() && diff.len() <= 16 && diff.last().unwrap() - diff[0] < 16);
    }
}
