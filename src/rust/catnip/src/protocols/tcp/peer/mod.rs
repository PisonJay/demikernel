mod connection;
mod isn_generator;
mod runtime;

#[cfg(test)]
mod tests;

use super::{
    error::TcpError,
    segment::{TcpSegment, TcpSegmentDecoder, DEFAULT_MSS},
};
use crate::{
    prelude::*,
    protocols::{arp, ip, ipv4},
    r#async::{Async, WhenAny},
};
use connection::{TcpConnection, TcpConnectionId};
use isn_generator::IsnGenerator;
use rand::seq::SliceRandom;
use runtime::TcpRuntime;
use std::{
    any::Any,
    collections::{HashMap, HashSet, VecDeque},
    convert::TryFrom,
    num::Wrapping,
    rc::Rc,
    time::Instant,
};

pub struct TcpPeer<'a> {
    active_connections: HashMap<ipv4::Endpoint, TcpConnection>,
    available_private_ports: VecDeque<ip::Port>, // todo: shared state.
    connections: HashMap<TcpConnectionId, TcpConnection>,
    isn_generator: IsnGenerator,
    listening_on_ports: HashSet<ip::Port>,
    open_ports: HashSet<ip::Port>,
    rt: TcpRuntime<'a>,
    async_work: WhenAny<'a, ()>,
}

impl<'a> TcpPeer<'a> {
    pub fn new(rt: Runtime<'a>, arp: arp::Peer<'a>) -> TcpPeer<'a> {
        // initialize the pool of available private ports.
        let available_private_ports = {
            let mut ports = Vec::new();
            for i in ip::Port::first_private_port().into()..65535 {
                ports.push(ip::Port::try_from(i).unwrap());
            }
            let mut rng = rt.borrow_rng();
            ports.shuffle(&mut *rng);
            VecDeque::from(ports)
        };
        let isn_generator = IsnGenerator::new(&rt);
        let rt = TcpRuntime::new(rt, arp);

        TcpPeer {
            active_connections: HashMap::new(),
            available_private_ports,
            connections: HashMap::new(),
            isn_generator,
            listening_on_ports: HashSet::new(),
            open_ports: HashSet::new(),
            rt,
            async_work: WhenAny::new(),
        }
    }

    pub fn receive(&mut self, datagram: ipv4::Datagram<'_>) -> Result<()> {
        trace!("TcpPeer::receive(...)");
        let segment = TcpSegmentDecoder::try_from(datagram)?;
        let ipv4_header = segment.ipv4().header();
        let tcp_header = segment.header();
        // i haven't yet seen anything that explicitly disallows categories of
        // IP addresses but it seems sensible to drop datagrams where the
        // source address does not really support a connection.
        let remote_ipv4_addr = ipv4_header.src_addr();
        if remote_ipv4_addr.is_broadcast()
            || remote_ipv4_addr.is_multicast()
            || remote_ipv4_addr.is_unspecified()
        {
            return Err(Fail::Malformed {
                details: "only unicast addresses are supported by TCP",
            });
        }

        let local_port = match tcp_header.dest_port() {
            Some(p) => p,
            None => {
                return Err(Fail::Malformed {
                    details: "destination port is zero",
                })
            }
        };

        debug!("local_port => {:?}", local_port);
        debug!("open_ports => {:?}", self.open_ports);
        if self.open_ports.contains(&local_port) {
            if tcp_header.rst() {
                self.rt.rt().emit_effect(Effect::TcpError(
                    TcpError::ConnectionRefused {},
                ));
                Ok(())
            } else {
                unimplemented!();
            }
        } else {
            let remote_port = match tcp_header.src_port() {
                Some(p) => p,
                None => {
                    return Err(Fail::Malformed {
                        details: "source port is zero",
                    })
                }
            };

            let mut ack_num = tcp_header.seq_num()
                + Wrapping(u32::try_from(segment.text().len())?);
            // from [TCP/IP Illustrated](https://learning.oreilly.com/library/view/TCP_IP+Illustrated,+Volume+1:+The+Protocols/9780132808200/ch13.html#ch13):
            // > Although there is no data in the arriving segment, the SYN
            // > bit logically occupies 1 byte of sequence number space;
            // > therefore, in this example the ACK number in the reset
            // > segment is set to the ISN, plus the data length (0), plus 1
            // > for the SYN bit.
            if tcp_header.syn() {
                ack_num += Wrapping(1);
            }

            self.async_work.add(
                self.rt.cast(
                    TcpSegment::default()
                        .dest_ipv4_addr(remote_ipv4_addr)
                        .dest_port(remote_port)
                        .src_port(local_port)
                        .ack_num(ack_num)
                        .rst(),
                ),
            );
            Ok(())
        }
    }

    pub fn connect(&mut self, remote_endpoint: ipv4::Endpoint) -> Result<()> {
        self.start_active_connection(remote_endpoint)
    }

    pub fn start_active_connection(
        &mut self,
        remote_endpoint: ipv4::Endpoint,
    ) -> Result<()> {
        let options = self.rt.rt().options();
        let local_port = self.acquire_private_port()?;
        let local_ipv4_addr = options.my_ipv4_addr;
        let cxn_id = TcpConnectionId {
            local: ipv4::Endpoint::new(options.my_ipv4_addr, local_port),
            remote: remote_endpoint,
        };
        let isn = self.isn_generator.next(&cxn_id);
        let cxn = TcpConnection::new(cxn_id.clone());
        assert!(self
            .active_connections
            .insert(cxn_id.remote.clone(), cxn)
            .is_none());
        assert!(self.open_ports.replace(local_port).is_none());

        let rt = self.rt.clone();
        self.async_work.add(self.rt.rt().start_coroutine(move || {
            r#await!(
                rt.cast(
                    TcpSegment::default()
                        .src_ipv4_addr(local_ipv4_addr)
                        .src_port(local_port)
                        .dest_ipv4_addr(cxn_id.remote.address())
                        .dest_port(cxn_id.remote.port())
                        .seq_num(isn)
                        .mss(DEFAULT_MSS)
                        .syn()
                ),
                rt.rt().now()
            )?;

            let x: Rc<dyn Any> = Rc::new(());
            Ok(x)
        }));
        Ok(())
    }

    fn acquire_private_port(&mut self) -> Result<ip::Port> {
        if let Some(p) = self.available_private_ports.pop_front() {
            Ok(p)
        } else {
            Err(Fail::ResourceExhausted {
                details: "no more private ports",
            })
        }
    }

    fn release_private_port(&mut self, port: ip::Port) {
        assert!(port.is_private());
        self.available_private_ports.push_back(port);
    }
}

impl<'a> Async<()> for TcpPeer<'a> {
    fn poll(&self, now: Instant) -> Option<Result<()>> {
        self.async_work.poll(now).map(|r| r.map(|_| ()))
    }
}
