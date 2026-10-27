// SPDX-License-Identifier: GPL-2.0-only
//! Bounded firmware event and BSS decoding (fwil_types.h, see NOTICE).
fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

/// D11AC format negotiated by GET_VERSION=2, using a 20 MHz control channel.
pub fn chanspec_20mhz(frequency: u32) -> Result<u16, &'static str> {
    match frequency {
        2484 => Ok(0x100e),
        2412..=2472 if (frequency - 2407) % 5 == 0 => Ok(0x1000 | ((frequency - 2407) / 5) as u16),
        5160..=5885 if frequency % 5 == 0 => Ok(0xd000 | ((frequency - 5000) / 5) as u16),
        _ => Err("unsupported channel frequency"),
    }
}

pub fn iovar(name: &str, value: &[u8], output: &mut [u8]) -> Result<usize, &'static str> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| (33..127).contains(&b)) {
        return Err("invalid firmware variable name");
    }
    let length = name
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_add(value.len()))
        .ok_or("variable length overflow")?;
    if length > output.len() || length > 1518 {
        return Err("variable request too large");
    }
    output[..name.len()].copy_from_slice(name.as_bytes());
    output[name.len()] = 0;
    output[name.len() + 1..length].copy_from_slice(value);
    Ok(length)
}
/// Primary BSS uses ordinary iovars. Other BSSes use `bsscfg:name\0`,
/// followed by the BSS index and value (Linux brcmf_create_bsscfg).
pub fn iovar_bsscfg(
    name: &str,
    index: u32,
    value: &[u8],
    output: &mut [u8],
) -> Result<usize, &'static str> {
    if index == 0 {
        return iovar(name, value, output);
    }
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| (33..127).contains(&b)) {
        return Err("invalid firmware variable name");
    }
    let length = name
        .len()
        .checked_add(12)
        .and_then(|n| n.checked_add(value.len()))
        .ok_or("variable length overflow")?;
    if length > output.len() || length > 1518 {
        return Err("variable request too large");
    }
    output[..7].copy_from_slice(b"bsscfg:");
    output[7..7 + name.len()].copy_from_slice(name.as_bytes());
    output[name.len() + 7] = 0;
    output[name.len() + 8..name.len() + 12].copy_from_slice(&index.to_le_bytes());
    output[name.len() + 12..length].copy_from_slice(value);
    Ok(length)
}
pub fn passive_escan(sync: u16) -> Result<[u8; 72], &'static str> {
    if sync == 0 {
        return Err("invalid scan synchronization ID");
    }
    let mut output = [0; 72];
    output[..4].copy_from_slice(&1u32.to_le_bytes());
    output[4..6].copy_from_slice(&1u16.to_le_bytes());
    output[6..8].copy_from_slice(&sync.to_le_bytes());
    output[44..50].fill(255);
    output[50] = 2;
    output[51] = 1;
    output[52..68].fill(255); // Default probes/dwell/home times; all channels.
    Ok(output)
}
#[derive(Debug)]
pub struct Event<'a> {
    pub kind: u32,
    pub status: u32,
    pub reason: u32,
    pub flags: u16,
    pub interface: u8,
    pub bss: u8,
    pub peer: [u8; 6],
    pub data: &'a [u8],
}
pub fn event(input: &[u8]) -> Result<Event<'_>, &'static str> {
    if input.len() < 72
        || input[12..14] != [0x88, 0x6c]
        || input[19..22] != [0, 0x10, 0x18]
        || input[22..24] != [0, 1]
    {
        return Err("invalid Broadcom event header");
    }
    let version = u16::from_be_bytes(input[24..26].try_into().unwrap());
    if ![1, 2].contains(&version) {
        return Err("unsupported event version");
    }
    let length = be32(input, 44) as usize;
    let data = input
        .get(72..72usize.checked_add(length).ok_or("event length overflow")?)
        .ok_or("truncated event payload")?;
    Ok(Event {
        kind: be32(input, 28),
        status: be32(input, 32),
        reason: be32(input, 36),
        flags: u16::from_be_bytes(input[26..28].try_into().unwrap()),
        peer: input[48..54].try_into().unwrap(),
        interface: input[70],
        bss: input[71],
        data,
    })
}
pub struct Bss<'a> {
    pub bssid: [u8; 6],
    pub ssid: &'a [u8],
    pub rssi: i16,
    pub channel: u8,
    pub capability: u16,
    pub ies: &'a [u8],
}
pub struct Scan<'a> {
    pub sync: u16,
    remaining: u16,
    data: &'a [u8],
}
pub fn scan(input: &[u8]) -> Result<Scan<'_>, &'static str> {
    if input.len() < 12 {
        return Err("truncated scan header");
    }
    let length = le32(input, 0) as usize;
    if length < 12 || length > input.len() || le16(input, 10) > 64 {
        return Err("invalid scan length/count");
    }
    Ok(Scan {
        sync: le16(input, 8),
        remaining: le16(input, 10),
        data: &input[12..length],
    })
}
impl<'a> Iterator for Scan<'a> {
    type Item = Result<Bss<'a>, &'static str>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        if self.data.len() < 8 {
            self.remaining = 0;
            return Some(Err("truncated BSS header"));
        }
        let length = le32(self.data, 4) as usize;
        if length < 126 || length > self.data.len() {
            self.remaining = 0;
            return Some(Err("invalid BSS length"));
        }
        let (record, rest) = self.data.split_at(length);
        self.data = rest;
        Some(bss(record))
    }
}

/// Decode one versioned BSS record, including GET_BSS_INFO replies.
pub fn bss(record: &[u8]) -> Result<Bss<'_>, &'static str> {
    if record.len() < 126 || le32(record, 0) != 109 {
        return Err("invalid BSS record");
    }
    let declared = le32(record, 4) as usize;
    if declared < 126 || declared > record.len() {
        return Err("BSS record length outside reply");
    }
    let record = &record[..declared];
    let n = record[18] as usize;
    if n > 32 {
        return Err("invalid BSS SSID length");
    }
    let offset = le16(record, 116) as usize;
    let length = le32(record, 120) as usize;
    let ies = if length == 0 {
        &[][..]
    } else {
        if offset < 126 {
            return Err("BSS IEs overlap header");
        }
        let end = offset.checked_add(length).ok_or("BSS IE length overflow")?;
        record.get(offset..end).ok_or("BSS IEs outside record")?
    };
    Ok(Bss {
        bssid: record[8..14].try_into().unwrap(),
        ssid: &record[19..19 + n],
        rssi: le16(record, 78) as i16,
        channel: if record[88] != 0 {
            record[88]
        } else {
            le16(record, 72) as u8
        },
        capability: le16(record, 16),
        ies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ioctl_v2_channels_encode_band_bandwidth_and_channel_fourteen() {
        assert_eq!(chanspec_20mhz(5180), Ok(0xd024));
        assert_eq!(chanspec_20mhz(2412), Ok(0x1001));
        assert_eq!(chanspec_20mhz(2484), Ok(0x100e));
        assert!(chanspec_20mhz(2413).is_err());
    }
    #[test]
    fn bsscfg_primary_uses_plain_iovar_and_secondary_has_explicit_prefix() {
        let mut out = [0; 64];
        let n = iovar_bsscfg("wsec", 0, &4u32.to_le_bytes(), &mut out).unwrap();
        assert_eq!(&out[..n], b"wsec\0\x04\0\0\0");
        let n = iovar_bsscfg("wsec", 2, &4u32.to_le_bytes(), &mut out).unwrap();
        assert_eq!(&out[..n], b"bsscfg:wsec\0\x02\0\0\0\x04\0\0\0");
        assert!(iovar_bsscfg("wsec", 2, &[0; 4], &mut [0; 19]).is_err());
    }
    #[test]
    fn scan_request_is_passive_and_events_require_complete_bounded_vendor_headers() {
        let request = passive_escan(7).unwrap();
        assert_eq!(le16(&request, 6), 7);
        assert_eq!(request[51], 1);
        assert_eq!(&request[44..50], &[255; 6]);
        assert!(passive_escan(0).is_err());
        let mut frame = [0; 76];
        frame[12..14].copy_from_slice(&[0x88, 0x6c]);
        frame[19..22].copy_from_slice(&[0, 0x10, 0x18]);
        frame[23] = 1;
        frame[25] = 2;
        frame[28..32].copy_from_slice(&69u32.to_be_bytes());
        frame[44..48].copy_from_slice(&4u32.to_be_bytes());
        assert_eq!(event(&frame).unwrap().kind, 69);
        assert_eq!(event(&frame).unwrap().data.len(), 4);
        frame[47] = 5;
        assert!(event(&frame).is_err());
        assert!(event(&frame[..71]).is_err());
    }
    #[test]
    fn bss_records_keep_raw_ssids_and_reject_ie_and_record_overruns() {
        let mut data = [0; 144];
        data[..4].copy_from_slice(&144u32.to_le_bytes());
        data[8..10].copy_from_slice(&7u16.to_le_bytes());
        data[10..12].copy_from_slice(&1u16.to_le_bytes());
        let bss = &mut data[12..];
        bss[..4].copy_from_slice(&109u32.to_le_bytes());
        bss[4..8].copy_from_slice(&132u32.to_le_bytes());
        bss[18] = 3;
        bss[19..22].copy_from_slice(&[0xff, 0, 10]);
        bss[78..80].copy_from_slice(&(-50i16).to_le_bytes());
        bss[88] = 1;
        bss[116..118].copy_from_slice(&128u16.to_le_bytes());
        bss[120..124].copy_from_slice(&4u32.to_le_bytes());
        let result = scan(&data).unwrap().next().unwrap().unwrap();
        assert_eq!(result.ssid, &[0xff, 0, 10]);
        assert_eq!(result.rssi, -50);
        assert_eq!(result.ies.len(), 4);
        data[12 + 123] = 1;
        assert!(scan(&data).unwrap().next().unwrap().is_err());
        data[12 + 7] = 1;
        assert!(scan(&data).unwrap().next().unwrap().is_err());
    }
}
