//! One newline-terminated JSON request and reply per local stream.
use super::{Edit, Snapshot};

pub const ENDPOINT: &str = "scarlet.joycond.v1";
pub const MAX_REQUEST: usize = 192;
pub const MAX_REPLY: usize = 2048;

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum Request {
    List,
    Apply { revision: u64, edit: Edit },
}

pub fn encode(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, String> {
    let mut frame = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    frame.push(b'\n');
    if frame.len() > limit {
        return Err("Joy-Con service frame too large".into());
    }
    Ok(frame)
}

pub struct Frame<const N: usize> {
    data: [u8; N],
    used: usize,
}

impl<const N: usize> Default for Frame<N> {
    fn default() -> Self {
        Self {
            data: [0; N],
            used: 0,
        }
    }
}

impl<const N: usize> Frame<N> {
    pub fn remaining(&mut self) -> &mut [u8] {
        &mut self.data[self.used..]
    }

    pub fn received(&mut self, count: usize) -> Result<Option<&[u8]>, &'static str> {
        if count == 0 {
            return Err("Joy-Con service stream closed");
        }
        if count > N - self.used {
            return Err("invalid Joy-Con stream length");
        }
        let start = self.used;
        self.used += count;
        if let Some(end) = self.data[start..self.used].iter().position(|&b| b == b'\n') {
            if start + end + 1 != self.used {
                return Err("unexpected data after Joy-Con frame");
            }
            return Ok(Some(&self.data[..start + end]));
        }
        if self.used == N {
            return Err("Joy-Con service frame too large");
        }
        Ok(None)
    }
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "riscv32",
    target_arch = "riscv64"
))]
pub fn credentials(socket: &scarlet_os::socket::Socket) -> Result<(u32, u32), String> {
    let mut bytes = [0xff; 8];
    // SAFETY: the kernel fills this eight-byte peer UID and flags record synchronously.
    let rc = unsafe {
        socket.as_handle().control(
            scarlet_abi::SCTL_LOCAL_PEER_CREDENTIALS,
            bytes.as_mut_ptr() as usize,
        )
    }
    .map_err(|e| format!("Joy-Con peer credentials: {e:?}"))?;
    if rc != 0 {
        return Err("unavailable Joy-Con peer credentials".into());
    }
    Ok((
        u32::from_le_bytes(bytes[..4].try_into().unwrap()),
        u32::from_le_bytes(bytes[4..].try_into().unwrap()),
    ))
}

#[cfg(target_os = "scarlet")]
pub(crate) fn exchange(request: Request) -> Result<Snapshot, String> {
    use scarlet_os::{handle::capability::StreamError, socket::Socket};
    use std::time::{Duration, Instant};

    let socket = Socket::new()
        .and_then(|s| {
            s.connect_abstract(ENDPOINT)?;
            Ok(s)
        })
        .map_err(|e| format!("Joy-Con service unavailable ({e:?})"))?;
    if credentials(&socket)? != (0, scarlet_abi::LOCAL_PEER_MANAGE_DEVICES) {
        return Err("Joy-Con endpoint does not belong to a privileged system service".into());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let set_timeout = |op| -> Result<(), String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Joy-Con service timed out".into());
        }
        // SAFETY: both socket timeout controls take scalar milliseconds, zero disables them.
        unsafe {
            socket
                .as_handle()
                .control(op, remaining.as_millis().max(1) as usize)
        }
        .map_err(|e| format!("Joy-Con socket timeout: {e:?}"))?;
        Ok(())
    };
    let stream = socket
        .as_stream()
        .map_err(|e| format!("Joy-Con socket: {e:?}"))?;
    let bytes = encode(&request, MAX_REQUEST)?;
    let mut pending = bytes.as_slice();
    while !pending.is_empty() {
        set_timeout(scarlet_abi::SCTL_SOCKET_SET_WRITE_TIMEOUT_MS)?;
        match stream.write(pending) {
            Ok(n) if (1..=pending.len()).contains(&n) => pending = &pending[n..],
            Err(StreamError::Interrupted) => continue,
            other => return Err(format!("Joy-Con request failed ({other:?})")),
        }
    }
    let mut frame = Frame::<MAX_REPLY>::default();
    loop {
        set_timeout(scarlet_abi::SCTL_SOCKET_SET_READ_TIMEOUT_MS)?;
        match stream.read(frame.remaining()) {
            Ok(n) => {
                if let Some(bytes) = frame.received(n)? {
                    return serde_json::from_slice::<Result<Snapshot, String>>(bytes)
                        .map_err(|_| "invalid Joy-Con service reply")?;
                }
            }
            Err(StreamError::Interrupted) => continue,
            Err(e) => return Err(format!("Joy-Con reply failed ({e:?})")),
        }
    }
}

#[cfg(not(target_os = "scarlet"))]
pub(crate) fn exchange(_: Request) -> Result<Snapshot, String> {
    Err("Joy-Con layout control requires Scarlet".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Half, Layout, Orientation, Pad};
    use scarlet_switch_joycon_abi::SLOTS_PER_SIDE;

    #[test]
    fn maximum_snapshot_and_commands_fit_and_round_trip_fragmented() {
        let pads = (0..SLOTS_PER_SIDE as u8)
            .flat_map(|slot| {
                [Half::Left(slot), Half::Right(slot)].map(|half| Pad {
                    player: slot % 4 + 1,
                    layout: Layout::Single {
                        half,
                        orientation: Orientation::Sideways,
                    },
                })
            })
            .collect();
        let result = Ok::<_, String>(Snapshot {
            revision: u64::MAX,
            pads,
        });
        let bytes = encode(&result, MAX_REPLY).unwrap();
        let mut frame = Frame::<MAX_REPLY>::default();
        for byte in &bytes[..bytes.len() - 1] {
            frame.remaining()[0] = *byte;
            assert_eq!(frame.received(1), Ok(None));
        }
        frame.remaining()[0] = b'\n';
        let decoded =
            serde_json::from_slice::<Result<Snapshot, String>>(frame.received(1).unwrap().unwrap())
                .unwrap();
        assert_eq!(decoded, result);

        for edit in [
            Edit::Orient {
                half: Half::Right(7),
                orientation: Orientation::Sideways,
            },
            Edit::Combine { left: 3, right: 7 },
            Edit::Split { left: 2, right: 6 },
        ] {
            let request = Request::Apply {
                revision: u64::MAX,
                edit,
            };
            let bytes = encode(&request, MAX_REQUEST).unwrap();
            assert_eq!(serde_json::from_slice::<Request>(&bytes).unwrap(), request);
        }
    }

    #[test]
    fn incomplete_oversize_and_trailing_frames_are_rejected() {
        for bytes in [b"".as_slice(), b"12345678", b"1\n2\n"] {
            let mut frame = Frame::<8>::default();
            frame.remaining()[..bytes.len()].copy_from_slice(bytes);
            assert!(frame.received(bytes.len()).is_err());
        }
        let mut frame = Frame::<8>::default();
        frame.remaining()[..3].copy_from_slice(b"123");
        assert_eq!(frame.received(3), Ok(None));
        assert!(frame.received(0).is_err());
        assert!(encode(&Request::List, 3).is_err());
    }
}
