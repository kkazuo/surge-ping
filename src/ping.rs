use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use log::trace;
use parking_lot::Mutex;
use rand::random;
use tokio::{
    sync::{broadcast, mpsc},
    task,
    time::timeout,
};

use crate::client::{AsyncSocket, Message};
use crate::error::{Result, SurgeError};
use crate::icmp::{icmpv4, icmpv6, IcmpPacket};

type Token = (u16, u16);

#[derive(Debug, Clone)]
struct Cache {
    inner: Arc<Mutex<HashMap<Token, Instant>>>,
}

impl Cache {
    fn new() -> Cache {
        Cache {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn insert(&self, ident: u16, seq_cnt: u16, time: Instant) {
        self.inner.lock().insert((ident, seq_cnt), time);
    }

    fn remove(&self, ident: u16, seq_cnt: u16) -> Option<Instant> {
        self.inner.lock().remove(&(ident, seq_cnt))
    }
}

/// A Ping struct represents the state of one particular ping instance.
///
/// # Examples
/// ```
/// use std::time::Duration;
///
/// use surge_ping::Pinger;
///
/// #[tokio::main]
/// async fn main() {
///     let mut pinger = Pinger::new("114.114.114.114".parse().unwrap()).unwrap();
///     pinger.size(56).timeout(Duration::from_secs(1));
///     let result = pinger.ping(0).await;
///     println!("{:?}", result);
/// }
///
pub struct Pinger {
    pub destination: IpAddr,
    pub ident: u16,
    pub size: usize,
    timeout: Duration,
    socket: AsyncSocket,
    rx: mpsc::Receiver<Message>,
    cache: Cache,
    shutdown_notify: broadcast::Sender<()>,
}

impl Drop for Pinger {
    fn drop(&mut self) {
        if self.shutdown_notify.send(()).is_err() {
            trace!("notify shutdown error");
        }
    }
}

impl Pinger {
    pub(crate) fn new(
        host: IpAddr,
        socket: AsyncSocket,
        rx: mpsc::Receiver<Message>,
        shutdown_notify: broadcast::Sender<()>,
    ) -> Pinger {
        Pinger {
            destination: host,
            ident: random(),
            size: 56,
            timeout: Duration::from_secs(2),
            socket,
            rx,
            cache: Cache::new(),
            shutdown_notify,
        }
    }

    /// Set the identification of ICMP.
    pub fn ident(&mut self, val: u16) -> &mut Pinger {
        self.ident = val;
        self
    }

    /// Set the packet size.(default: 56)
    pub fn size(&mut self, size: usize) -> &mut Pinger {
        self.size = size;
        self
    }

    /// The timeout of each Ping, in seconds. (default: 2s)
    pub fn timeout(&mut self, timeout: Duration) -> &mut Pinger {
        self.timeout = timeout;
        self
    }

    async fn recv_reply(&mut self, seq_cnt: u16) -> Result<(IcmpPacket, Duration)> {
        loop {
            let message = self.rx.recv().await.ok_or(SurgeError::NetworkError)?;
            let packet = match self.destination {
                IpAddr::V4(_) => icmpv4::Icmpv4Packet::decode(&message.packet).map(IcmpPacket::V4),
                IpAddr::V6(a) => {
                    icmpv6::Icmpv6Packet::decode(&message.packet, a).map(IcmpPacket::V6)
                }
            };
            match packet {
                Ok(packet) => {
                    if packet.check_reply_packet(self.destination, seq_cnt, self.ident) {
                        if let Some(ins) = self.cache.remove(self.ident, seq_cnt) {
                            return Ok((packet, message.when - ins));
                        }
                    }
                }
                Err(SurgeError::EchoRequestPacket) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Send Ping request with sequence number.
    pub async fn ping(&mut self, seq_cnt: u16) -> Result<(IcmpPacket, Duration)> {
        let sender = self.socket.clone();
        let mut packet = match self.destination {
            IpAddr::V4(_) => icmpv4::make_icmpv4_echo_packet(self.ident, seq_cnt, self.size)?,
            IpAddr::V6(_) => icmpv6::make_icmpv6_echo_packet(self.ident, seq_cnt, self.size)?,
        };
        // let mut packet = EchoRequest::new(self.host, self.ident, seq_cnt, self.size).encode()?;
        let sock_addr = SocketAddr::new(self.destination, 0);
        let ident = self.ident;
        let cache = self.cache.clone();
        task::spawn(async move {
            if let Err(e) = sender.send_to(&mut packet, &sock_addr).await {
                trace!("socket send packet error: {}", e)
            }
            cache.insert(ident, seq_cnt, Instant::now());
        });

        match timeout(self.timeout, self.recv_reply(seq_cnt)).await {
            Ok(reply) => reply.map_err(|err| {
                self.cache.remove(ident, seq_cnt);
                err
            }),
            Err(_) => {
                self.cache.remove(ident, seq_cnt);
                Err(SurgeError::Timeout { seq: seq_cnt })
            }
        }
    }
}
