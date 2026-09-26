use alvr_common::{
    ToAny,
    anyhow::{Result, bail},
    warn,
};
use flume::TryRecvError;
use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};
use std::{collections::HashMap, net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket}};

pub struct WelcomeSocket {
    mdns_receiver: Receiver<ServiceEvent>,
    legacy_socket: Option<UdpSocket>,
}

impl WelcomeSocket {
    pub fn new() -> Result<Self> {
        let mdns_receiver = ServiceDaemon::new()?.browse(alvr_sockets::MDNS_SERVICE_TYPE)?;

        let legacy_socket = UdpSocket::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 9943))
            .map(|socket| { socket.set_nonblocking(true).ok(); socket })
            .ok();
        Ok(Self { mdns_receiver, legacy_socket })
    }

    // Returns: client IP, client hostname
    pub fn recv_all(&self) -> Result<HashMap<String, IpAddr>> {
        let mut clients = HashMap::new();

        loop {
            match self.mdns_receiver.try_recv() {
                Ok(event) => {
                    if let ServiceEvent::ServiceResolved(info) = event {
                        let hostname = info
                            .get_property_val_str(alvr_sockets::MDNS_DEVICE_ID_KEY)
                            .unwrap_or_else(|| info.get_hostname());
                        let addresses = info.get_addresses();
                        let address = addresses
                            .iter()
                            .copied()
                            .find(IpAddr::is_ipv4)
                            .or_else(|| addresses.iter().copied().next())
                            .to_any()?;
                        warn!(
                            "ALVR mDNS resolved: hostname={}, addresses={:?}, selected={}",
                            hostname,
                            addresses,
                            address
                        );

                        let client_protocol = info
                            .get_property_val_str(alvr_sockets::MDNS_PROTOCOL_KEY)
                            .to_any()?;
                        let server_protocol = alvr_common::protocol_id();
                        let client_is_dev = client_protocol.contains("-dev");
                        let server_is_dev = server_protocol.contains("-dev");

                        if client_protocol != server_protocol {
                            let reason = if client_is_dev && server_is_dev {
                                "Please use matching nightly versions."
                            } else if client_is_dev {
                                "Please use nightly server or stable client."
                            } else if server_is_dev {
                                "Please use stable server or nightly client."
                            } else {
                                "Please use matching stable versions."
                            };
                            let protocols = format!(
                                "Protocols: server={server_protocol}, client={client_protocol}"
                            );
                            warn!("Found incompatible client {hostname}! {reason}\n{protocols}");
                        }

                        clients.insert(hostname.into(), address);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(e) => bail!(e),
            }
        }

        if let Some(socket) = &self.legacy_socket {
            let mut buf = [0u8; 2048];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((size, peer)) if size > 0 => {
                        clients.entry(format!("legacy-{}", peer.ip())).or_insert(peer.ip());
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => { warn!("Legacy UDP discovery receive error: {e}"); break; }
                }
            }
        }
        // Current-client direct-IP fallback. This only substitutes discovery;
        // the normal trust and ALVR protocol handshake still run afterwards.
        if let Ok(ip) = std::env::var("ALVR_DIRECT_CLIENT_IP") {
            if let Ok(address) = ip.parse::<IpAddr>() {
                clients.entry(format!("direct-{address}")).or_insert(address);
            }
        }

        Ok(clients)
    }
}
