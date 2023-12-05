use core::fmt::Debug;

use embedded_nal_async::{SocketAddr, SocketAddrV4, UdpStack, UnconnectedUdp};

use crate::dhcp;

#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    Format(dhcp::Error),
}

impl<E> From<dhcp::Error> for Error<E> {
    fn from(value: dhcp::Error) -> Self {
        Self::Format(value)
    }
}

pub mod raw {
    use core::fmt::Debug;

    use crate::{
        asynch::tcp::{RawSocket, RawStack},
        dhcp::raw,
    };

    use embedded_io::ErrorKind;

    use embedded_nal_async::{ConnectedUdp, SocketAddr, SocketAddrV4, UdpStack, UnconnectedUdp};

    #[derive(Debug)]
    pub enum Error<E> {
        Io(E),
        UnsupportedProtocol,
        RawError(raw::Error),
    }

    impl<E> From<raw::Error> for Error<E> {
        fn from(value: raw::Error) -> Self {
            Self::RawError(value)
        }
    }

    impl<E> embedded_io_async::Error for Error<E>
    where
        E: embedded_io_async::Error,
    {
        fn kind(&self) -> ErrorKind {
            match self {
                Self::Io(err) => err.kind(),
                Self::UnsupportedProtocol => ErrorKind::InvalidInput,
                Self::RawError(_) => ErrorKind::InvalidData,
            }
        }
    }

    pub struct ConnectedUdp2RawSocket<T>(T, SocketAddrV4, SocketAddrV4);

    impl<T> ConnectedUdp for ConnectedUdp2RawSocket<T>
    where
        T: RawSocket,
    {
        type Error = Error<T::Error>;

        async fn send(&mut self, data: &[u8]) -> Result<(), Self::Error> {
            send(
                &mut self.0,
                SocketAddr::V4(self.1),
                SocketAddr::V4(self.2),
                data,
            )
            .await
        }

        async fn receive_into(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
            let (len, _, _) = receive_into(&mut self.0, Some(self.1), Some(self.2), buffer).await?;

            Ok(len)
        }
    }

    pub struct UnconnectedUdp2RawSocket<T>(T, Option<SocketAddrV4>);

    impl<T> UnconnectedUdp for UnconnectedUdp2RawSocket<T>
    where
        T: RawSocket,
    {
        type Error = Error<T::Error>;

        async fn send(
            &mut self,
            local: SocketAddr,
            remote: SocketAddr,
            data: &[u8],
        ) -> Result<(), Self::Error> {
            send(&mut self.0, local, remote, data).await
        }

        async fn receive_into(
            &mut self,
            buffer: &mut [u8],
        ) -> Result<(usize, SocketAddr, SocketAddr), Self::Error> {
            receive_into(&mut self.0, None, self.1, buffer).await
        }
    }

    pub struct Udp2RawStack<T>(T, T::Interface)
    where
        T: RawStack;

    impl<T> UdpStack for Udp2RawStack<T>
    where
        T: RawStack,
    {
        type Error = Error<T::Error>;

        type Connected = ConnectedUdp2RawSocket<T::Socket>;

        type UniquelyBound = UnconnectedUdp2RawSocket<T::Socket>;

        type MultiplyBound = UnconnectedUdp2RawSocket<T::Socket>;

        async fn connect_from(
            &self,
            local: SocketAddr,
            remote: SocketAddr,
        ) -> Result<(SocketAddr, Self::Connected), Self::Error> {
            let (SocketAddr::V4(localv4), SocketAddr::V4(remotev4)) = (local, remote) else {
                Err(Error::UnsupportedProtocol)?
            };

            let socket = self.0.bind(&self.1).await.map_err(Self::Error::Io)?;

            Ok((local, ConnectedUdp2RawSocket(socket, localv4, remotev4)))
        }

        async fn bind_single(
            &self,
            local: SocketAddr,
        ) -> Result<(SocketAddr, Self::UniquelyBound), Self::Error> {
            let SocketAddr::V4(localv4) = local else {
                Err(Error::UnsupportedProtocol)?
            };

            let socket = self.0.bind(&self.1).await.map_err(Self::Error::Io)?;

            Ok((local, UnconnectedUdp2RawSocket(socket, Some(localv4))))
        }

        async fn bind_multiple(
            &self,
            local: SocketAddr,
        ) -> Result<Self::MultiplyBound, Self::Error> {
            let SocketAddr::V4(local) = local else {
                Err(Error::UnsupportedProtocol)?
            };

            let socket = self.0.bind(&self.1).await.map_err(Self::Error::Io)?;

            Ok(UnconnectedUdp2RawSocket(socket, Some(local)))
        }
    }

    async fn send<T: RawSocket>(
        mut socket: T,
        local: SocketAddr,
        remote: SocketAddr,
        data: &[u8],
    ) -> Result<(), Error<T::Error>> {
        let (SocketAddr::V4(local), SocketAddr::V4(remote)) = (local, remote) else {
            Err(Error::UnsupportedProtocol)?
        };

        let mut buf = [0; 1500];

        let data = raw::ip_udp_encode(&mut buf, local, remote, |buf| {
            if data.len() <= buf.len() {
                buf[..data.len()].copy_from_slice(data);

                Ok(data.len())
            } else {
                Err(raw::Error::BufferOverflow)
            }
        })?;

        socket.send(data).await.map_err(Error::Io)
    }

    async fn receive_into<T: RawSocket>(
        mut socket: T,
        filter_src: Option<SocketAddrV4>,
        filter_dst: Option<SocketAddrV4>,
        buffer: &mut [u8],
    ) -> Result<(usize, SocketAddr, SocketAddr), Error<T::Error>> {
        let mut buf = [0; 1500];

        let (local, remote, len) = loop {
            let len = socket.receive_into(&mut buf).await.map_err(Error::Io)?;

            match raw::ip_udp_decode(&buf[..len], filter_src, filter_dst) {
                Ok(Some((local, remote, data))) => break (local, remote, data.len()),
                Ok(None) => continue,
                Err(raw::Error::InvalidFormat) | Err(raw::Error::InvalidChecksum) => continue,
                Err(other) => Err(other)?,
            }
        };

        if len <= buffer.len() {
            buffer[..len].copy_from_slice(&buf[..len]);

            Ok((len, SocketAddr::V4(local), SocketAddr::V4(remote)))
        } else {
            Err(raw::Error::BufferOverflow.into())
        }
    }
}

pub mod client {
    use core::fmt::Debug;

    use embassy_futures::select::{select, Either};
    use embassy_time::{Duration, Instant, Timer};

    use embedded_nal_async::{ConnectedUdp, Ipv4Addr};

    use log::{info, warn};

    use rand_core::RngCore;

    pub use super::*;

    pub use crate::dhcp::Settings;
    use crate::dhcp::{Options, Packet};

    #[derive(Clone, Debug)]
    pub struct Configuration {
        pub socket: SocketAddrV4,
        pub mac: [u8; 6],
        pub timeout: Duration,
    }

    impl Configuration {
        pub const fn new(mac: [u8; 6]) -> Self {
            Self {
                socket: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68),
                mac,
                timeout: Duration::from_secs(10),
            }
        }
    }

    /// A simple asynchronous DHCP client.
    ///
    /// The client takes a socket factory (either operating on raw sockets or UDP datagrams) and
    /// then takes care of the all the negotiations with the DHCP server, as in discovering servers,
    /// negotiating initial IP, and then keeping the lease of that IP up to date.
    ///
    /// Note that it is unlikely that a non-raw socket factory would actually even work, due to the peculiarities of the
    /// DHCP protocol, where a lot of UDP packets are send (and often broadcasted) by the client before the client actually has an assigned IP.
    pub struct Client<'a, T, F> {
        stack: F,
        buf: &'a mut [u8],
        client: dhcp::client::Client<T>,
        socket: SocketAddrV4,
        timeout: Duration,
        pub settings: Option<(Settings, Instant)>,
    }

    impl<'a, T, F> Client<'a, T, F>
    where
        T: RngCore,
        F: UdpStack,
    {
        pub fn new(stack: F, buf: &'a mut [u8], rng: T, conf: &Configuration) -> Self {
            info!("Creating DHCP client with configuration {conf:?}");

            Self {
                stack,
                buf,
                client: dhcp::client::Client { rng, mac: conf.mac },
                socket: conf.socket,
                timeout: conf.timeout,
                settings: None,
            }
        }

        /// Runs the DHCP client with the supplied socket factory, and takes care of
        /// all aspects of negotiating an IP with the first DHCP server that replies to the discovery requests.
        ///
        /// From the POV of the user, this method will return only in two cases, which are exactly the cases where the user is expected to take an action:
        /// - When an initial/new IP lease was negotiated; in that case, `Some(Settings)` is returned, and the user should assign the returned IP settings
        ///   to the network interface using platform-specific means
        /// - When the IP lease was lost; in that case, `None` is returned, and the user should de-assign all IP settings from the network interface using
        ///   platform-specific means
        ///
        /// In both cases, user is expected to call `run` again, so that the IP lease is kept up to date / a new lease is re-negotiated
        ///
        /// Note that dropping this future is also safe in that it won't remove the current lease, so the user can renew
        /// the operation of the client by just calling `run` later on. Of course, if the future is not polled, the client
        /// would be unable - during that time - to check for lease timeout and the lease might not be renewed on time.
        ///
        /// But in any case, if the lease is expired or the DHCP server does not acknowledge the lease renewal, the client will
        /// automatically restart the DHCP servers' discovery from the very beginning.
        pub async fn run(&mut self) -> Result<Option<Settings>, Error<F::Error>> {
            loop {
                if let Some((settings, acquired)) = self.settings.as_ref() {
                    // Keep the lease
                    let now = Instant::now();

                    if now - *acquired
                        >= Duration::from_secs(settings.lease_time_secs.unwrap_or(7200) as u64 / 3)
                    {
                        info!("Renewing DHCP lease...");

                        if let Some(settings) = self
                            .request(settings.server_ip.unwrap(), settings.ip)
                            .await?
                        {
                            self.settings = Some((settings, Instant::now()));
                        } else {
                            // Lease was not renewed; let the user know
                            self.settings = None;

                            return Ok(None);
                        }
                    } else {
                        Timer::after(Duration::from_secs(60)).await;
                    }
                } else {
                    // Look for offers
                    let offer = self.discover().await?;

                    if let Some(settings) = self.request(offer.server_ip.unwrap(), offer.ip).await?
                    {
                        // IP acquired; let the user know
                        self.settings = Some((settings.clone(), Instant::now()));

                        return Ok(Some(settings));
                    }
                }
            }
        }

        /// This method allows the user to inform the DHCP server that the currently leased IP (if any) is no longer used
        /// by the client.
        ///
        /// Useful when the program runnuing the DHCP client is about to exit.
        pub async fn release(&mut self) -> Result<(), Error<F::Error>> {
            if let Some((settings, _)) = self.settings.as_ref().cloned() {
                let server_ip = settings.server_ip.unwrap();
                let (_, mut socket) = self
                    .stack
                    .connect_from(
                        SocketAddr::V4(self.socket),
                        SocketAddr::V4(SocketAddrV4::new(server_ip, self.socket.port())),
                    )
                    .await
                    .map_err(Error::Io)?;

                let mut opt_buf = Options::buf();
                let request = self.client.release(&mut opt_buf, 0, settings.ip);

                socket
                    .send(request.encode(self.buf)?)
                    .await
                    .map_err(Error::Io)?;
            }

            self.settings = None;

            Ok(())
        }

        async fn discover(&mut self) -> Result<Settings, Error<F::Error>> {
            info!("Discovering DHCP servers...");

            let start = Instant::now();

            loop {
                let mut socket = self
                    .stack
                    .bind_multiple(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68)))
                    .await
                    .map_err(Error::Io)?;

                let mut opt_buf = Options::buf();

                let (request, xid) = self.client.discover(
                    &mut opt_buf,
                    (Instant::now() - start).as_secs() as _,
                    None,
                );

                socket
                    .send(
                        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68)),
                        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 67)),
                        request.encode(self.buf)?,
                    )
                    .await
                    .map_err(Error::Io)?;

                let offer_start = Instant::now();

                while Instant::now() - offer_start < self.timeout {
                    let timer = Timer::after(Duration::from_secs(3));

                    if let Either::First(result) =
                        select(socket.receive_into(self.buf), timer).await
                    {
                        let (len, _local, _remote) = result.map_err(Error::Io)?;
                        let reply = Packet::decode(&self.buf[..len])?;

                        if self.client.is_offer(&reply, xid) {
                            let settings: Settings = (&reply).into();

                            info!(
                                "IP {} offered by DHCP server {}",
                                settings.ip,
                                settings.server_ip.unwrap()
                            );

                            return Ok(settings);
                        }
                    }
                }

                drop(socket);

                info!("No DHCP offers received, sleeping for a while...");

                Timer::after(Duration::from_secs(3)).await;
            }
        }

        async fn request(
            &mut self,
            server_ip: Ipv4Addr,
            ip: Ipv4Addr,
        ) -> Result<Option<Settings>, Error<F::Error>> {
            for _ in 0..3 {
                info!("Requesting IP {ip} from DHCP server {server_ip}");

                let mut socket = self
                    .stack
                    .bind_multiple(SocketAddr::V4(SocketAddrV4::new(server_ip, 68)))
                    .await
                    .map_err(Error::Io)?;

                let start = Instant::now();

                let mut opt_buf = Options::buf();

                let (request, xid) =
                    self.client
                        .request(&mut opt_buf, (Instant::now() - start).as_secs() as _, ip);

                socket
                    .send(
                        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68)),
                        SocketAddr::V4(SocketAddrV4::new(server_ip, 67)),
                        request.encode(self.buf)?,
                    )
                    .await
                    .map_err(Error::Io)?;

                let request_start = Instant::now();

                while Instant::now() - request_start < self.timeout {
                    let timer = Timer::after(Duration::from_secs(10));

                    if let Either::First(result) =
                        select(socket.receive_into(self.buf), timer).await
                    {
                        let (len, _local, _remote) = result.map_err(Error::Io)?;
                        let packet = &self.buf[..len];

                        let reply = Packet::decode(packet)?;

                        if self.client.is_ack(&reply, xid) {
                            let settings = (&reply).into();

                            info!("IP {} leased successfully", ip);

                            return Ok(Some(settings));
                        } else if self.client.is_nak(&reply, xid) {
                            info!("IP {} not acknowledged", ip);

                            return Ok(None);
                        }
                    }
                }

                drop(socket);
            }

            warn!("IP request was not replied");

            Ok(None)
        }
    }
}

pub mod server {
    use core::fmt::Debug;

    use embassy_time::Duration;

    use embedded_nal_async::Ipv4Addr;

    use log::info;

    use self::dhcp::{Options, Packet};

    pub use super::*;

    #[derive(Clone, Debug)]
    pub struct Configuration<'a> {
        pub socket: SocketAddrV4,
        pub ip: Ipv4Addr,
        pub gateways: &'a [Ipv4Addr],
        pub subnet: Option<Ipv4Addr>,
        pub dns: &'a [Ipv4Addr],
        pub range_start: Ipv4Addr,
        pub range_end: Ipv4Addr,
        pub lease_duration_secs: u32,
    }

    /// A simple asynchronous DHCP server.
    ///
    /// The client takes a socket factory (either operating on raw sockets or UDP datagrams) and
    /// then processes all incoming BOOTP requests, by updating its internal simple database of leases, and issuing replies.
    pub struct Server<'a, const N: usize, F> {
        stack: F,
        buf: &'a mut [u8],
        socket: SocketAddrV4,
        server_options: dhcp::server::ServerOptions<'a>,
        pub server: dhcp::server::Server<N>,
    }

    impl<'a, const N: usize, F> Server<'a, N, F>
    where
        F: UdpStack,
    {
        pub fn new(stack: F, buf: &'a mut [u8], conf: &Configuration<'a>) -> Self {
            info!("Creating DHCP server with configuration {conf:?}");

            Self {
                stack,
                buf,
                socket: conf.socket,
                server_options: dhcp::server::ServerOptions {
                    ip: conf.ip,
                    gateways: conf.gateways,
                    subnet: conf.subnet,
                    dns: conf.dns,
                    lease_duration: Duration::from_secs(conf.lease_duration_secs as _),
                },
                server: dhcp::server::Server {
                    range_start: conf.range_start,
                    range_end: conf.range_end,
                    leases: heapless::LinearMap::new(),
                },
            }
        }

        /// Runs the DHCP server wth the supplied socket factory, processing incoming DHCP requests.
        ///
        /// Note that dropping this future is safe in that it won't remove the internal leases' database,
        /// so users are free to drop the future in case they would like to take a snapshot of the leases or inspect them otherwise.
        pub async fn run(&mut self) -> Result<(), Error<F::Error>> {
            let mut socket = self
                .stack
                .bind_multiple(SocketAddr::V4(self.socket))
                .await
                .map_err(Error::Io)?;

            loop {
                let (len, local, remote) =
                    socket.receive_into(self.buf).await.map_err(Error::Io)?;
                let packet = &self.buf[..len];

                let request = Packet::decode(packet)?;

                let mut opt_buf = Options::buf();

                if let Some(request) =
                    self.server
                        .handle_request(&mut opt_buf, &self.server_options, &request)
                {
                    socket
                        .send(
                            local,
                            if request.broadcast {
                                SocketAddr::V4(SocketAddrV4::new(
                                    Ipv4Addr::BROADCAST,
                                    remote.port(),
                                ))
                            } else {
                                remote
                            },
                            request.encode(self.buf)?,
                        )
                        .await
                        .map_err(Error::Io)?;
                }
            }
        }
    }
}
