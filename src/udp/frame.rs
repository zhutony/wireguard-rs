use std::io;
use std::net::{SocketAddr, Ipv4Addr, SocketAddrV4, IpAddr};

use futures::{Async, Future, Poll, Stream, Sink, StartSend, AsyncSink, future, stream, unsync::mpsc};
use udp::{ConnectedUdpSocket, UdpSocket};
use tokio_core::reactor::Handle;

/// Encoding of frames via buffers.
///
/// This trait is used when constructing an instance of `UdpFramed` and provides
/// the `In` and `Out` types which are decoded and encoded from the socket,
/// respectively.
///
/// Because UDP is a connectionless protocol, the `decode` method receives the
/// address where data came from and the `encode` method is also responsible for
/// determining the remote host to which the datagram should be sent
///
/// The trait itself is implemented on a type that can track state for decoding
/// or encoding, which is particularly useful for streaming parsers. In many
/// cases, though, this type will simply be a unit struct (e.g. `struct
/// HttpCodec`).
pub trait UdpCodec {
    /// The type of decoded frames.
    type In;

    /// The type of frames to be encoded.
    type Out;

    /// Attempts to decode a frame from the provided buffer of bytes.
    ///
    /// This method is called by `UdpFramed` on a single datagram which has been
    /// read from a socket. The `buf` argument contains the data that was
    /// received from the remote address, and `src` is the address the data came
    /// from. Note that typically this method should require the entire contents
    /// of `buf` to be valid or otherwise return an error with trailing data.
    ///
    /// Finally, if the bytes in the buffer are malformed then an error is
    /// returned indicating why. This informs `Framed` that the stream is now
    /// corrupt and should be terminated.
    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> io::Result<Self::In>;

    /// Encodes a frame into the buffer provided.
    ///
    /// This method will encode `msg` into the byte buffer provided by `buf`.
    /// The `buf` provided is an internal buffer of the `Framed` instance and
    /// will be written out when possible.
    ///
    /// The encode method also determines the destination to which the buffer
    /// should be directed, which will be returned as a `SocketAddr`.
    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr;
}

pub enum Socket {
    Unconnected(UdpSocket),
    Connected(ConnectedUdpSocket),
}

/// A unified `Stream` and `Sink` interface to an underlying `UdpSocket`, using
/// the `UdpCodec` trait to encode and decode frames.
///
/// You can acquire a `UdpFramed` instance by using the `UdpSocket::framed`
/// adapter.
#[must_use = "sinks do nothing unless polled"]
pub struct UdpFramed<C> {
    socket: Socket,
    codec: C,
    rd: Vec<u8>,
    wr: Vec<u8>,
    out_addr: SocketAddr,
    flushed: bool,
}

impl<C> UdpFramed<C> {
    pub fn handle(&self) -> &Handle {
        match self.socket {
            Socket::Unconnected(ref socket) => &socket.handle,
            Socket::Connected(ref socket) => &socket.inner.handle,
        }
    }
}

impl<C: UdpCodec> Stream for UdpFramed<C> {
    type Item = C::In;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<C::In>, io::Error> {
        let (n, addr) = match self.socket {
            Socket::Unconnected(ref socket) => try_nb!(socket.recv_from(&mut self.rd)),
            Socket::Connected(ref socket)   => (try_nb!(socket.inner.recv(&mut self.rd)), socket.addr),
        };
        trace!("received {} bytes, decoding", n);
        let frame = self.codec.decode(&addr, &self.rd[..n])?;
        trace!("frame decoded from buffer");
        Ok(Async::Ready(Some(frame)))
    }
}

impl<C: UdpCodec> Sink for UdpFramed<C> {
    type SinkItem = C::Out;
    type SinkError = io::Error;

    fn start_send(&mut self, item: C::Out) -> StartSend<C::Out, io::Error> {
        trace!("sending frame");

        if !self.flushed {
            match self.poll_complete()? {
                Async::Ready(()) => {},
                Async::NotReady => return Ok(AsyncSink::NotReady(item)),
            }
        }

        self.out_addr = self.codec.encode(item, &mut self.wr);
        self.flushed = false;
        trace!("frame encoded; length={}", self.wr.len());

        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        if self.flushed {
            return Ok(Async::Ready(()))
        }

        trace!("flushing frame; length={}", self.wr.len());
        let n = match self.socket {
            Socket::Unconnected(ref socket) => try_nb!(socket.send_to(&self.wr, &self.out_addr)),
            Socket::Connected(ref socket)   => try_nb!(socket.inner.send(&self.wr)) // TODO check to make sure the address is the connected address
        };
        trace!("written {}", n);

        let wrote_all = n == self.wr.len();
        self.wr.clear();
        self.flushed = true;

        if wrote_all {
            Ok(Async::Ready(()))
        } else {
            Err(io::Error::new(io::ErrorKind::Other,
                               "failed to write entire datagram to socket"))
        }
    }

    fn close(&mut self) -> Poll<(), io::Error> {
        try_ready!(self.poll_complete());
        Ok(().into())
    }
}

pub fn new<C: UdpCodec>(socket: Socket, codec: C) -> UdpFramed<C> {
    UdpFramed {
        socket,
        codec,
        out_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
        rd: vec![0; 64 * 1024],
        wr: Vec::with_capacity(8 * 1024),
        flushed: true,
    }
}

impl<C> UdpFramed<C> {
    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise being
    /// worked with.
    pub fn get_ref(&self) -> &UdpSocket {
        match self.socket {
            Socket::Connected(ref socket) => &socket.inner,
            Socket::Unconnected(ref socket) => socket
        }
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise being
    /// worked with.
    pub fn get_mut(&mut self) -> &mut UdpSocket {
        match self.socket {
            Socket::Connected(ref mut socket) => &mut socket.inner,
            Socket::Unconnected(ref mut socket) => socket
        }
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise being
    /// worked with.
    pub fn into_inner(self) -> UdpSocket {
        match self.socket {
            Socket::Connected(socket) => socket.inner,
            Socket::Unconnected(socket) => socket
        }
    }
}

pub type PeerServerMessage = (SocketAddr, Vec<u8>);
pub struct VecUdpCodec;
impl UdpCodec for VecUdpCodec {
    type In = PeerServerMessage;
    type Out = PeerServerMessage;

    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> io::Result<Self::In> {
        let unmapped_ip = match src.ip() {
            IpAddr::V6(v6addr) => {
                if let Some(v4addr) = v6addr.to_ipv4() {
                    IpAddr::V4(v4addr)
                } else {
                    IpAddr::V6(v6addr)
                }
            }
            v4addr => v4addr
        };
        Ok((SocketAddr::new(unmapped_ip, src.port()), buf.to_vec()))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        let (mut addr, mut data) = msg;
        buf.append(&mut data);
        let mapped_ip = match addr.ip() {
            IpAddr::V4(v4addr) => IpAddr::V6(v4addr.to_ipv6_mapped()),
            v6addr => v6addr
        };
        addr.set_ip(mapped_ip);
        addr
    }
}

pub struct UdpChannel {
    pub ingress : stream::SplitStream<UdpFramed<VecUdpCodec>>,
    pub egress  : mpsc::Sender<PeerServerMessage>,
        handle  : Handle,
}

impl From<UdpFramed<VecUdpCodec>> for UdpChannel {
    fn from(framed: UdpFramed<VecUdpCodec>) -> Self {
        let handle = framed.handle().clone();
        let (udp_sink, ingress) = framed.split();
        let (egress, egress_rx) = mpsc::channel(1024);
        let udp_writethrough    = udp_sink
            .sink_map_err(|_| ())
            .send_all(egress_rx.and_then(|(addr, packet)| {
                          trace!("sending UDP packet to {:?}", &addr);
                          future::ok((addr, packet))
                      })
                      .map_err(|_| { info!("udp sink error"); () }))
            .then(|_| Ok(()));

        handle.spawn(udp_writethrough);

        UdpChannel { egress, ingress, handle }
    }
}

impl UdpChannel {
    pub fn send(&self, message: PeerServerMessage) {
        self.handle.spawn(self.egress.clone().send(message).then(|_| Ok(())));
    }
}