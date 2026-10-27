// SPDX-License-Identifier: GPL-2.0-only
//! Checked FullMAC WPA2/CCMP wire records (fwil_types.h).
//! No credential enters this protocol; only derived traffic keys are installed.
use alloc::{vec, vec::Vec};
use core::{
    ptr::write_volatile,
    sync::atomic::{Ordering, compiler_fence},
};
pub struct KeyWire(pub [u8; 164]);
pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe {
            write_volatile(byte, 0);
        }
    }
    compiler_fence(Ordering::SeqCst);
}
pub struct CommandBuffer(pub Vec<u8>);
impl Drop for CommandBuffer {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}
impl Drop for KeyWire {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}
impl KeyWire {
    pub fn ccmp(
        index: u32,
        pairwise: bool,
        peer: [u8; 6],
        key: &[u8],
        rx_pn: &[u8],
    ) -> Result<Self, &'static str> {
        if index > 3
            || key.len() != 16
            || rx_pn.len() != 6
            || (pairwise && (index != 0 || peer == [0; 6] || peer[0] & 1 != 0))
        {
            return Err("invalid CCMP firmware key");
        }
        let mut out = Self([0; 164]);
        out.0[..4].copy_from_slice(&index.to_le_bytes());
        out.0[4..8].copy_from_slice(&16u32.to_le_bytes());
        out.0[8..24].copy_from_slice(key);
        out.0[112..116].copy_from_slice(&4u32.to_le_bytes()); // AES-CCM.
        if pairwise {
            out.0[156..162].copy_from_slice(&peer);
        } else {
            out.0[116..120].copy_from_slice(&2u32.to_le_bytes());
        } // Primary GTK slot.
        out.0[132..136].copy_from_slice(&1u32.to_le_bytes());
        out.0[140..144].copy_from_slice(&rx_pn[2..6]);
        out.0[144..146].copy_from_slice(&rx_pn[..2]);
        Ok(out)
    }
    pub fn delete(index: u32, pairwise: bool, peer: [u8; 6]) -> Result<Self, &'static str> {
        if index > 3 || (pairwise && (index != 0 || peer == [0; 6] || peer[0] & 1 != 0)) {
            return Err("invalid firmware key reference");
        }
        let mut out = Self([0; 164]);
        out.0[..4].copy_from_slice(&index.to_le_bytes());
        if pairwise {
            out.0[156..162].copy_from_slice(&peer);
        } else {
            out.0[116..120].copy_from_slice(&2u32.to_le_bytes());
        }
        Ok(out)
    }
}
/// A fixed BSSID join uses the legacy command's 48-byte struct without a
/// channel list. Channel selection remains firmware/regulatory policy.
/// Legacy C_SET_SSID payload: ssid_le plus assoc_params. With a chanspec the
/// firmware associates directly instead of running its own find scan.
pub fn join(ssid: &[u8], peer: [u8; 6], chanspec: Option<u16>) -> Result<Vec<u8>, &'static str> {
    let wildcard = peer == [0xff; 6];
    if ssid.is_empty() || ssid.len() > 32 || peer == [0; 6] || (!wildcard && peer[0] & 1 != 0) {
        return Err("invalid station join");
    }
    let mut out = vec![0u8; if chanspec.is_some() { 50 } else { 48 }];
    out[..4].copy_from_slice(&(ssid.len() as u32).to_le_bytes());
    out[4..4 + ssid.len()].copy_from_slice(ssid);
    out[36..42].copy_from_slice(&peer);
    if let Some(chanspec) = chanspec {
        out[44..48].copy_from_slice(&1u32.to_le_bytes());
        out[48..50].copy_from_slice(&chanspec.to_le_bytes());
    }
    Ok(out)
}
/// Extended join (brcmf_ext_join_params_le): SSID plus scan defaults and the
/// association parameters, with the C-struct padding the firmware expects.
pub fn ext_join(
    ssid: &[u8],
    peer: [u8; 6],
    chanspec: Option<u16>,
) -> Result<([u8; 72], usize), &'static str> {
    let wildcard = peer == [0xff; 6];
    if ssid.is_empty() || ssid.len() > 32 || peer == [0; 6] || (!wildcard && peer[0] & 1 != 0) {
        return Err("invalid station join");
    }
    let mut out = [0; 72];
    out[..4].copy_from_slice(&(ssid.len() as u32).to_le_bytes());
    out[4..4 + ssid.len()].copy_from_slice(ssid);
    out[36] = 0xff;
    for offset in [40, 44, 48, 52] {
        out[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    out[56..62].copy_from_slice(&peer);
    let mut length = 68;
    if let Some(chanspec) = chanspec {
        out[64..68].copy_from_slice(&1u32.to_le_bytes());
        out[68..70].copy_from_slice(&chanspec.to_le_bytes());
        length = 70;
    }
    Ok((out, length))
}
/// Return exactly one complete RSN element from the actual AP's IE list.
pub fn rsn(ies: &[u8]) -> Result<&[u8], &'static str> {
    let mut rest = ies;
    let mut found = None;
    while !rest.is_empty() {
        if rest.len() < 2 {
            return Err("truncated IE header");
        }
        let n = rest[1] as usize + 2;
        if n > rest.len() {
            return Err("truncated IE payload");
        }
        if rest[0] == 48 {
            if found.is_some() || !(22..=64).contains(&n) {
                return Err("ambiguous or unsupported AP RSN element");
            }
            found = Some(&rest[..n]);
        }
        rest = &rest[n..];
    }
    found.ok_or("AP has no RSN element")
}
/// Preserve the AP's RSNE and RSNXE together. Supplicant M3 validation compares
/// both against beacon evidence, including on mixed WPA2/WPA3 networks.
pub fn association_ies(ies: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = rsn(ies)?.to_vec();
    let mut rest = ies;
    let mut extension = None;
    while !rest.is_empty() {
        if rest.len() < 2 {
            return Err("truncated IE header");
        }
        let n = rest[1] as usize + 2;
        if n > rest.len() {
            return Err("truncated IE payload");
        }
        if rest[0] == 244 {
            if extension.is_some() || !(3..=18).contains(&n) {
                return Err("invalid or duplicate AP RSNXE");
            }
            extension = Some(&rest[..n]);
        }
        rest = &rest[n..];
    }
    if let Some(extension) = extension {
        out.extend_from_slice(extension);
    }
    if out.len() > 64 {
        return Err("AP security IEs exceed ABI bound");
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn association_evidence_retains_rsnxe_and_rejects_ambiguous_extensions() {
        let ie = [
            48, 20, 1, 0, 0, 15, 172, 4, 1, 0, 0, 15, 172, 4, 1, 0, 0, 15, 172, 2, 0, 0,
        ];
        let mut beacon = vec![0, 1, b'x', 244, 1, 0x20];
        beacon.extend_from_slice(&ie);
        let evidence = association_ies(&beacon).unwrap();
        assert_eq!(&evidence[..22], &ie);
        assert_eq!(&evidence[22..], &[244, 1, 0x20]);
        beacon.extend_from_slice(&[244, 1, 0x20]);
        assert!(association_ies(&beacon).is_err());
        assert!(association_ies(&[244, 2, 0x20]).is_err());
    }
    #[test]
    fn pairwise_group_and_delete_records_preserve_pn_and_zero_reserved_fields() {
        let wire = KeyWire::ccmp(0, true, [2; 6], &[0xa5; 16], &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(&wire.0[8..24], &[0xa5; 16]);
        assert_eq!(&wire.0[24..112], &[0; 88]);
        assert_eq!(&wire.0[140..146], &[3, 4, 5, 6, 1, 2]);
        assert_eq!(&wire.0[156..162], &[2; 6]);
        assert_eq!(&wire.0[116..120], &[0; 4]);
        let group = KeyWire::ccmp(2, false, [0; 6], &[1; 16], &[0; 6]).unwrap();
        assert_eq!(group.0[116], 2);
        assert_eq!(&group.0[156..164], &[0; 8]);
        let deleted = KeyWire::delete(0, true, [2; 6]).unwrap();
        assert_eq!(&deleted.0[4..156], &[0; 152]);
        assert!(KeyWire::ccmp(1, true, [2; 6], &[1; 16], &[0; 6]).is_err());
        assert!(KeyWire::ccmp(0, true, [3; 6], &[1; 16], &[0; 6]).is_err());
    }
    #[test]
    fn join_preserves_raw_ssid_and_ie_selection_rejects_duplicates_and_truncation() {
        let joined = join(&[0xff, 0, 9], [2; 6], None).unwrap();
        assert_eq!(joined[0], 3);
        assert_eq!(&joined[4..7], &[0xff, 0, 9]);
        assert_eq!(&joined[36..42], &[2; 6]);
        assert_eq!(&joined[42..], &[0; 6]);
        let pinned = join(&[1, 2], [2; 6], Some(0x1024)).unwrap();
        assert_eq!(pinned.len(), 50);
        assert_eq!(&pinned[44..48], &1u32.to_le_bytes());
        assert_eq!(&pinned[48..50], &0x1024u16.to_le_bytes());
        assert!(join(&[1], [0xff; 6], None).is_ok());
        assert!(join(&[1], [3; 6], None).is_err());
        assert!(join(&[], [2; 6], None).is_err());
        let (pinned, length) = ext_join(&[7, 8], [2; 6], Some(0x1024)).unwrap();
        assert_eq!(length, 70);
        assert_eq!(&pinned[64..68], &1u32.to_le_bytes());
        assert_eq!(&pinned[68..70], &0x1024u16.to_le_bytes());
        assert_eq!(ext_join(&[7], [2; 6], None).unwrap().1, 68);
        let ie = [
            48, 20, 1, 0, 0, 15, 172, 4, 1, 0, 0, 15, 172, 4, 1, 0, 0, 15, 172, 2, 0, 0,
        ];
        assert_eq!(rsn(&ie).unwrap(), ie);
        let mut duplicate = [0; 44];
        duplicate[..22].copy_from_slice(&ie);
        duplicate[22..].copy_from_slice(&ie);
        assert!(rsn(&duplicate).is_err());
        assert!(rsn(&ie[..21]).is_err());
    }
}
