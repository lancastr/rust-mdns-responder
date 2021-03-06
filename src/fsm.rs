use dns_parser::{self, QueryClass, QueryType, Name, RRData};
use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind::WouldBlock;
use std::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;
use futures::{Poll, Async, Future, Stream};
use futures::sync::mpsc;
use tokio::net::UdpSocket;
use tokio::reactor::Handle;

use super::{DEFAULT_TTL, MDNS_PORT};
use address_family::AddressFamily;
use net;
use services::{Services, ServiceData};

pub type AnswerBuilder = dns_parser::Builder<dns_parser::Answers>;

#[derive(Clone, Debug)]
pub enum Command {
    SendUnsolicited {
        svc: ServiceData,
        ttl: u32,
        include_ip: bool
    },
    Shutdown,
}

pub struct FSM<AF: AddressFamily> {
    socket: Arc<Mutex<UdpSocket>>,
    join_handle: Option<thread::JoinHandle<()>>,
    services: Services,
    commands: mpsc::UnboundedReceiver<Command>,
    outgoing: VecDeque<(Vec<u8>, SocketAddr)>,
    _af: PhantomData<AF>,
}

impl <AF: AddressFamily> FSM<AF> {
    pub fn new(handle: &Handle, services: &Services)
        -> io::Result<(FSM<AF>, mpsc::UnboundedSender<Command>)>
    {
        let std_socket = AF::bind()?;
        let socket = UdpSocket::from_socket(std_socket, handle)?;
        let socket_arc = Arc::new(Mutex::new(socket));
        let (tx, rx) = mpsc::unbounded();

        let socket_weak = Arc::downgrade(&socket_arc);
        let handle = thread::spawn(move || {
            trace!("Joining thread started");
            'outer: loop {
                socket_weak.upgrade()
                    .map(|socket| AF::join_multicast(&socket.lock().unwrap()));

                let start = time::Instant::now();
                while start.elapsed() <= time::Duration::from_secs(10) {
                    if let Some(_) = socket_weak.upgrade() {
                        thread::sleep(::std::time::Duration::from_millis(100));
                    } else {
                        break 'outer;
                    }
                }
            };
            trace!("Joining thread ended");
        });

        let fsm = FSM {
            socket: socket_arc,
            join_handle: Some(handle),
            services: services.clone(),
            commands: rx,
            outgoing: VecDeque::new(),
            _af: PhantomData,
        };

        Ok((fsm, tx))
    }

    fn recv_packets(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            let (bytes, addr) = match self.socket.lock().unwrap().recv_from(&mut buf) {
                Ok((bytes, addr)) => (bytes, addr),
                Err(ref ioerr) if ioerr.kind() == WouldBlock => break,
                Err(err) => return Err(err),
            };

            if bytes >= buf.len() {
                warn!("buffer too small for packet from {:?}", addr);
                continue;
            }

            self.handle_packet(&buf[..bytes], addr);
        }
        Ok(())
    }

    fn handle_packet(&mut self, buffer: &[u8], addr: SocketAddr) {
        trace!("received packet from {:?}", addr);

        let packet = match dns_parser::Packet::parse(buffer) {
            Ok(packet) => packet,
            Err(error) => {
                warn!("couldn't parse packet from {:?}: {}", addr, error);
                return;
            }
        };

        if !packet.header.query {
            trace!("received packet from {:?} with no query", addr);
            return;
        }

        if packet.header.truncated {
            warn!("dropping truncated packet from {:?}", addr);
            return;
        }

        let mut unicast_builder = dns_parser::Builder::new_response(packet.header.id, false).move_to::<dns_parser::Answers>();
        let mut multicast_builder = dns_parser::Builder::new_response(packet.header.id, false).move_to::<dns_parser::Answers>();
        unicast_builder.set_max_size(None);
        multicast_builder.set_max_size(None);

        for question in packet.questions {
            if question.qclass == QueryClass::IN || question.qclass == QueryClass::Any {
                if !question.qu {
                    multicast_builder = self.handle_question(&question, multicast_builder);
                }
                unicast_builder = self.handle_question(&question, unicast_builder);
            }
        }

        if !multicast_builder.is_empty() {
            let response = multicast_builder.build().unwrap_or_else(|x| x);
            let addr = SocketAddr::new(AF::mdns_group(), MDNS_PORT);
            self.outgoing.push_back((response, addr));
        }

        if !unicast_builder.is_empty() {
            let response = unicast_builder.build().unwrap_or_else(|x| x);
            self.outgoing.push_back((response, addr));
        }
    }

    fn handle_question(&self, question: &dns_parser::Question, mut builder: AnswerBuilder) -> AnswerBuilder {
        let services = self.services.read().unwrap();

        match question.qtype {
            QueryType::A |
            QueryType::AAAA |
            QueryType::All if question.qname == *services.get_hostname() => {
                builder = self.add_ip_rr(services.get_hostname(), builder, DEFAULT_TTL);
            }
            QueryType::PTR => {
                for svc in services.find_by_type(&question.qname) {
                    builder = svc.add_ptr_rr(builder, DEFAULT_TTL);
                    builder = svc.add_srv_rr(services.get_hostname(), builder, DEFAULT_TTL);
                    builder = svc.add_txt_rr(builder, DEFAULT_TTL);
                    builder = self.add_ip_rr(services.get_hostname(), builder, DEFAULT_TTL);
                }
            }
            QueryType::SRV => {
                if let Some(svc) = services.find_by_name(&question.qname) {
                    builder = svc.add_srv_rr(services.get_hostname(), builder, DEFAULT_TTL);
                    builder = self.add_ip_rr(services.get_hostname(), builder, DEFAULT_TTL);
                }
            }
            QueryType::TXT => {
                if let Some(svc) = services.find_by_name(&question.qname) {
                    builder = svc.add_txt_rr(builder, DEFAULT_TTL);
                }
            }
            _ => ()
        }

        builder
    }

    fn add_ip_rr(&self, hostname: &Name, mut builder: AnswerBuilder, ttl: u32) -> AnswerBuilder {
        for iface in net::getifaddrs() {
            if iface.is_loopback() {
                continue;
            }

            match iface.ip() {
                Some(IpAddr::V4(ip)) if !AF::v6() => {
                    builder = builder.add_answer(hostname, QueryClass::IN, ttl, &RRData::A(ip))
                }
                Some(IpAddr::V6(ip)) if AF::v6() => {
                    builder = builder.add_answer(hostname, QueryClass::IN, ttl, &RRData::AAAA(ip))
                }
                _ => ()
            }
        }

        builder
    }

    fn send_unsolicited(&mut self, svc: &ServiceData, ttl: u32, include_ip: bool) {
        let mut builder = dns_parser::Builder::new_response(0, false).move_to::<dns_parser::Answers>();
        builder.set_max_size(None);

        let services = self.services.read().unwrap();

        builder = svc.add_ptr_rr(builder, ttl);
        builder = svc.add_srv_rr(services.get_hostname(), builder, ttl);
        builder = svc.add_txt_rr(builder, ttl);
        if include_ip {
            builder = self.add_ip_rr(services.get_hostname(), builder, ttl);
        }

        if !builder.is_empty() {
            let response = builder.build().unwrap_or_else(|x| x);
            let addr = SocketAddr::new(AF::mdns_group(), MDNS_PORT);
            self.outgoing.push_back((response, addr));
        }
    }
}

impl <AF: AddressFamily> Drop for FSM<AF> {
    fn drop(&mut self) {
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

/*
   impl rotor::Machine for FSM {
    type Context = ();
    type Seed = void::Void;

    fn create(seed: Self::Seed, _scope: &mut Scope<Self::Context>) -> rotor::Response<Self, rotor::Void> {
        void::unreachable(seed)
    }

    fn ready(self, _events: EventSet, _scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        self.recv_packets().unwrap();
        rotor::Response::ok(self)
    }

    fn spawned(self, _scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        unimplemented!()
    }

    fn timeout(self, _scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        unimplemented!()
    }

    fn wakeup(self, scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        loop {
            match self.rx.try_recv() {
                Ok(Command::Shutdown) => {
                    scope.shutdown_loop();
                    return rotor::Response::done();
                }
                Ok(Command::SendUnsolicited { svc, ttl, include_ip }) => {
                    match self.send_unsolicited(&svc, ttl, include_ip) {
                        Ok(_) => (),
                        Err(e) => {
                            warn!("Error sending unsolicited: {:?}", e);
                            return rotor::Response::error(Box::new(e));
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    warn!("responder disconnected without shutdown");
                    scope.shutdown_loop();
                    return rotor::Response::done();
                }
                Err(TryRecvError::Empty) => {
                    break;
                }
            }
        }

        rotor::Response::ok(self)
    }
}
*/

impl <AF: AddressFamily> Future for FSM<AF> {
    type Item = ();
    type Error = io::Error;
    fn poll(&mut self) -> Poll<(), io::Error> {
        while let Async::Ready(cmd) = self.commands.poll().unwrap() {
            match cmd {
                Some(Command::Shutdown) => return Ok(Async::Ready(())),
                Some(Command::SendUnsolicited { svc, ttl, include_ip }) => {
                    self.send_unsolicited(&svc, ttl, include_ip);
                }
                None => {
                    warn!("responder disconnected without shutdown");
                    return Ok(Async::Ready(()));
                }
            }
        }

        loop {
            let async = self.socket.lock().unwrap().poll_read();
            if let Async::Ready(()) = async {
                self.recv_packets()?;
            } else {
                break;
            }
        }

        loop {
            if let Some(&(ref response, ref addr)) = self.outgoing.front() {
                trace!("sending packet to {:?}", addr);

                match self.socket.lock().unwrap().send_to(response, addr) {
                    Ok(_) => (),
                    Err(ref ioerr) if ioerr.kind() == WouldBlock => break,
                    Err(err) => warn!("error sending packet {:?}", err),
                }
            } else {
                break;
            }

            self.outgoing.pop_front();
        }

        Ok(Async::NotReady)
    }
}
