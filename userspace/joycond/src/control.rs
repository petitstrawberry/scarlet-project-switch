//! Bounded, nonblocking desktop requests on the same poll loop as the raw halves.
use joycond::{
    Snapshot,
    wire::{self, Frame, Request},
};
use scarlet_os::{
    handle::capability::StreamError,
    poll::{POLLIN, PollHandle},
    socket::{Socket, SocketError},
};
use std::time::{Duration, Instant};

const MAX_CLIENTS: usize = 4;
const LIFETIME: Duration = Duration::from_secs(1);

fn allowed(uid: u32, flags: u32, login_enabled: impl FnOnce(u32) -> bool) -> bool {
    matches!(flags, 0 | scarlet_abi::LOCAL_PEER_MANAGE_DEVICES)
        && (uid == 0 || (uid != u32::MAX && login_enabled(uid)))
}

struct Client {
    socket: Socket,
    request: Frame<{ wire::MAX_REQUEST }>,
    expires: Instant,
}

type Reply = Result<Snapshot, String>;

fn reply(socket: &Socket, result: Reply) {
    if let Ok(bytes) = wire::encode(&result, wire::MAX_REPLY)
        && let Ok(stream) = socket.as_stream()
    {
        // This is the only write on a fresh nonblocking stream. The entire
        // capped reply fits Scarlet's socket buffer even if the peer never reads.
        let _ = stream.write_all(&bytes);
    }
}

impl Client {
    /// Return true while a partial request still needs data.
    fn read(&mut self, handle: &mut impl FnMut(Request) -> Reply) -> bool {
        let Ok(stream) = self.socket.as_stream() else {
            return false;
        };
        let count = match stream.read(self.request.remaining()) {
            Ok(n) => n,
            Err(StreamError::Interrupted) => return true,
            // A non-byte handle record can report readable and return WouldBlock.
            Err(_) => return false,
        };
        let result = match self.request.received(count) {
            Ok(None) => return true,
            Ok(Some(bytes)) => serde_json::from_slice(bytes)
                .map_err(|_| "Invalid Joy-Con request".to_owned())
                .and_then(handle),
            Err(error) => Err(error.into()),
        };
        reply(&self.socket, result);
        false
    }
}

pub struct Server {
    listener: Socket,
    clients: Vec<Client>,
}

impl Server {
    pub fn bind() -> Result<Self, SocketError> {
        let listener = Socket::new()?;
        listener.bind_abstract(wire::ENDPOINT)?;
        listener.listen(MAX_CLIENTS)?;
        Ok(Self {
            listener,
            clients: Vec::with_capacity(MAX_CLIENTS),
        })
    }

    pub fn poll_handles(&mut self, fds: &mut Vec<PollHandle>, now: Instant) {
        self.clients.retain(|c| now < c.expires);
        fds.extend(
            self.clients
                .iter()
                .map(|c| PollHandle::new(c.socket.as_raw() as u32, POLLIN)),
        );
        if self.clients.len() < MAX_CLIENTS {
            fds.push(PollHandle::new(self.listener.as_raw() as u32, POLLIN));
        }
    }

    pub fn dispatch(&mut self, ready: &[PollHandle], mut handle: impl FnMut(Request) -> Reply) {
        let (clients, listener) = ready.split_at(self.clients.len());
        let mut fds = clients.iter();
        // Drop a hung-up peer even if a request was buffered before it closed.
        self.clients.retain_mut(|c| {
            let events = fds.next().unwrap().revents;
            Instant::now() < c.expires && (events == 0 || (events == POLLIN && c.read(&mut handle)))
        });
        // Only this thread accepts. A readable listener has at least one queued connection.
        if listener.first().is_some_and(|fd| fd.revents & POLLIN != 0)
            && let Ok(socket) = self.listener.accept()
            && socket.set_nonblocking(true).is_ok()
        {
            #[cfg(target_os = "scarlet")]
            let login_enabled = scarlet_accounts::login_uid_enabled;
            #[cfg(not(target_os = "scarlet"))]
            let login_enabled = |_| false;

            if wire::credentials(&socket)
                .is_ok_and(|(uid, flags)| allowed(uid, flags, login_enabled))
            {
                self.clients.push(Client {
                    socket,
                    request: Frame::default(),
                    expires: Instant::now() + LIFETIME,
                });
            } else {
                reply(
                    &socket,
                    Err("Joy-Con control requires an unconfined login account".into()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::allowed;
    use scarlet_abi::{LOCAL_PEER_CONFINED as CONFINED, LOCAL_PEER_MANAGE_DEVICES as MANAGE};

    #[test]
    fn only_unconfined_root_and_enabled_desktop_accounts_can_use_control() {
        for flags in [0, MANAGE] {
            assert!(allowed(0, flags, |_| panic!("unexpected root lookup")));
            assert!(allowed(1000, flags, |uid| uid == 1000));
            assert!(!allowed(1001, flags, |uid| uid == 1000));
            assert!(!allowed(u32::MAX, flags, |_| true));
        }
        for uid in [0, 1000] {
            for flags in [CONFINED, CONFINED | MANAGE, u32::MAX, 8] {
                assert!(!allowed(uid, flags, |_| panic!(
                    "unexpected account lookup"
                )));
            }
        }
    }
}
