use std::{
    io::Error as StdIoError,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream, UdpSocket as TokioUdpSocket},
    sync::{watch, Semaphore},
    time,
};

use crate::{
    diagnostics::Diagnostics,
    udp::{
        decode_endpoint, encode_endpoint, relay_fwd_udp, Endpoint, FwdUdpConfig, HevFrameError,
        UdpStats,
    },
};

pub const SOCKS_VERSION: u8 = 0x05;
pub const AUTH_NONE: u8 = 0x00;
pub const AUTH_UNACCEPTABLE: u8 = 0xff;
pub const CMD_CONNECT: u8 = 0x01;
pub const CMD_FWD_UDP: u8 = 0x05;
const VIRTUAL_DESKTOP_STREAMER_PORTS: [u16; 4] = [38_810, 38_820, 38_830, 38_840];

#[derive(Clone, Default)]
pub struct VirtualDesktopFlowMonitor {
    active: Arc<[AtomicUsize; VIRTUAL_DESKTOP_STREAMER_PORTS.len()]>,
}

impl VirtualDesktopFlowMonitor {
    fn begin(&self, destination_port: u16) -> Option<VirtualDesktopFlowGuard> {
        let index = VIRTUAL_DESKTOP_STREAMER_PORTS
            .iter()
            .position(|port| *port == destination_port)?;
        self.active[index].fetch_add(1, Ordering::Relaxed);
        Some(VirtualDesktopFlowGuard {
            monitor: self.clone(),
            index,
        })
    }

    pub fn snapshot(&self) -> VirtualDesktopFlowSnapshot {
        let active_flows = std::array::from_fn(|index| self.active[index].load(Ordering::Relaxed));
        VirtualDesktopFlowSnapshot {
            connected: active_flows.iter().all(|count| *count > 0),
            active_ports: active_flows.iter().filter(|count| **count > 0).count(),
            active_flows,
        }
    }
}

struct VirtualDesktopFlowGuard {
    monitor: VirtualDesktopFlowMonitor,
    index: usize,
}

impl Drop for VirtualDesktopFlowGuard {
    fn drop(&mut self) {
        self.monitor.active[self.index].fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct VirtualDesktopFlowSnapshot {
    pub connected: bool,
    pub active_ports: usize,
    pub active_flows: [usize; VIRTUAL_DESKTOP_STREAMER_PORTS.len()],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowEndpoint {
    Quest,
    Streamer,
}

impl FlowEndpoint {
    fn as_str(self) -> &'static str {
        match self {
            Self::Quest => "quest",
            Self::Streamer => "streamer",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowTerminalEvent {
    ReadEof,
    ReadError,
    WriteError,
}

impl FlowTerminalEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadEof => "read_eof",
            Self::ReadError => "read_error",
            Self::WriteError => "write_error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FlowTerminal {
    endpoint: FlowEndpoint,
    event: FlowTerminalEvent,
}

#[derive(Default)]
struct FlowObservation {
    first_terminal: StdMutex<Option<FlowTerminal>>,
}

impl FlowObservation {
    fn record(&self, endpoint: FlowEndpoint, event: FlowTerminalEvent) {
        let Ok(mut first_terminal) = self.first_terminal.lock() else {
            return;
        };
        if first_terminal.is_none() {
            *first_terminal = Some(FlowTerminal { endpoint, event });
        }
    }

    fn first_terminal(&self) -> Option<FlowTerminal> {
        self.first_terminal.lock().ok().and_then(|value| *value)
    }
}

struct ObservedIo<'a, T> {
    inner: T,
    endpoint: FlowEndpoint,
    observation: &'a FlowObservation,
}

impl<'a, T> ObservedIo<'a, T> {
    fn new(inner: T, endpoint: FlowEndpoint, observation: &'a FlowObservation) -> Self {
        Self {
            inner,
            endpoint,
            observation,
        }
    }

    fn record_error(&self, event: FlowTerminalEvent, result: &Poll<Result<(), StdIoError>>) {
        if matches!(result, Poll::Ready(Err(_))) {
            self.observation.record(self.endpoint, event);
        }
    }
}

impl<T> AsyncRead for ObservedIo<'_, T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut io::ReadBuf<'_>,
    ) -> Poll<Result<(), StdIoError>> {
        let this = self.get_mut();
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(context, buffer);
        match &result {
            Poll::Ready(Ok(())) if buffer.filled().len() == filled_before => this
                .observation
                .record(this.endpoint, FlowTerminalEvent::ReadEof),
            Poll::Ready(Err(_)) => this
                .observation
                .record(this.endpoint, FlowTerminalEvent::ReadError),
            _ => {}
        }
        result
    }
}

impl<T> AsyncWrite for ObservedIo<'_, T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, StdIoError>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(context, buffer);
        if matches!(result, Poll::Ready(Err(_))) {
            this.observation
                .record(this.endpoint, FlowTerminalEvent::WriteError);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Result<(), StdIoError>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(context);
        this.record_error(FlowTerminalEvent::WriteError, &result);
        result
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), StdIoError>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(context);
        this.record_error(FlowTerminalEvent::WriteError, &result);
        result
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocksCommand {
    Connect,
    FwdUdp,
}

/// Commands accepted by one listener. The two commands are intentionally
/// never multiplexed onto the same host acceptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocksCommandPolicy {
    ConnectOnly,
    FwdUdpOnly,
}

impl SocksCommandPolicy {
    fn allows(self, command: SocksCommand) -> bool {
        matches!(
            (self, command),
            (Self::ConnectOnly, SocksCommand::Connect) | (Self::FwdUdpOnly, SocksCommand::FwdUdp)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocksRequest {
    pub command: SocksCommand,
    pub destination: Endpoint,
}

impl SocksRequest {
    /// Pure request parser for tests and fuzzers. Authentication negotiation is
    /// intentionally parsed separately by the bounded stream handler.
    pub fn decode(input: &[u8]) -> Result<Self, SocksError> {
        if input.len() < 4 {
            return Err(SocksError::Malformed("truncated request"));
        }
        if input[0] != SOCKS_VERSION || input[2] != 0 {
            return Err(SocksError::Malformed("invalid version or reserved byte"));
        }
        let command = match input[1] {
            CMD_CONNECT => SocksCommand::Connect,
            CMD_FWD_UDP => SocksCommand::FwdUdp,
            other => return Err(SocksError::UnsupportedCommand(other)),
        };
        let (destination, consumed) = decode_endpoint(&input[3..])?;
        if consumed + 3 != input.len() {
            return Err(SocksError::Malformed("trailing request bytes"));
        }
        Ok(Self {
            command,
            destination,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SocksConfig {
    pub bind: SocketAddr,
    pub command_policy: SocksCommandPolicy,
    pub max_connections: usize,
    pub handshake_timeout: Duration,
    pub connect_timeout: Duration,
    pub fwd_udp: FwdUdpConfig,
}

impl Default for SocksConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::adb::SOCKS_PORT),
            command_policy: SocksCommandPolicy::ConnectOnly,
            max_connections: 64,
            handshake_timeout: Duration::from_secs(3),
            connect_timeout: Duration::from_secs(10),
            fwd_udp: FwdUdpConfig::default(),
        }
    }
}

#[derive(Clone)]
pub struct SocksServer {
    config: SocksConfig,
    stats: SocksStats,
    udp_stats: UdpStats,
    virtual_desktop_flows: VirtualDesktopFlowMonitor,
    diagnostics: Option<Diagnostics>,
    relay_gate: Option<RelayGate>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RelayGateState {
    enabled: bool,
    generation: u64,
}

/// Invalidates every active relay flow on confirmed suspend or carrier loss,
/// while keeping the listeners alive for a fresh wake connection.
#[derive(Clone, Debug)]
pub struct RelayGate {
    sender: watch::Sender<RelayGateState>,
}

impl Default for RelayGate {
    fn default() -> Self {
        let (sender, _) = watch::channel(RelayGateState::default());
        Self { sender }
    }
}

impl RelayGate {
    pub fn set_enabled(&self, enabled: bool) {
        self.sender.send_if_modified(|state| {
            if state.enabled == enabled {
                return false;
            }
            state.enabled = enabled;
            state.generation = state.generation.saturating_add(1);
            true
        });
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.sender.borrow().enabled
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.sender.borrow().generation
    }

    fn active_generation(&self) -> Option<u64> {
        let state = *self.sender.borrow();
        state.enabled.then_some(state.generation)
    }

    async fn wait_for_invalidation(&self, generation: u64) {
        let mut receiver = self.sender.subscribe();
        loop {
            let state = *receiver.borrow_and_update();
            if !state.enabled || state.generation != generation {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

impl SocksServer {
    pub fn new(config: SocksConfig) -> Result<Self, SocksError> {
        if !config.bind.ip().is_loopback() {
            return Err(SocksError::NonLoopbackBind(config.bind));
        }
        if config.max_connections == 0
            || config.fwd_udp.queue_capacity == 0
            || config.fwd_udp.association_byte_budget == 0
        {
            return Err(SocksError::Malformed("capacity must be non-zero"));
        }
        Ok(Self {
            config,
            stats: SocksStats::default(),
            udp_stats: UdpStats::default(),
            virtual_desktop_flows: VirtualDesktopFlowMonitor::default(),
            diagnostics: None,
            relay_gate: None,
        })
    }

    pub fn with_diagnostics(mut self, diagnostics: Diagnostics) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    pub fn with_relay_gate(mut self, relay_gate: RelayGate) -> Self {
        self.relay_gate = Some(relay_gate);
        self
    }

    pub fn stats(&self) -> SocksStatsSnapshot {
        let mut snapshot = self.stats.snapshot();
        snapshot.udp = self.udp_stats.snapshot();
        snapshot.virtual_desktop = self.virtual_desktop_flows.snapshot();
        snapshot
    }

    pub fn virtual_desktop_flows(&self) -> VirtualDesktopFlowMonitor {
        self.virtual_desktop_flows.clone()
    }

    pub async fn serve(self) -> Result<(), SocksError> {
        let listener = TcpListener::bind(self.config.bind).await?;
        self.serve_on(listener).await
    }

    pub async fn serve_on(self, listener: TcpListener) -> Result<(), SocksError> {
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            return Err(SocksError::NonLoopbackBind(address));
        }
        let permits = Arc::new(Semaphore::new(self.config.max_connections));
        loop {
            let (stream, _) = listener.accept().await?;
            let relay_lease = match &self.relay_gate {
                Some(gate) => match gate.active_generation() {
                    Some(generation) => Some((gate.clone(), generation)),
                    None => {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                },
                None => None,
            };
            let permit = match permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            self.stats.accepted.fetch_add(1, Ordering::Relaxed);
            let server = self.clone();
            tokio::spawn(async move {
                server.stats.active.fetch_add(1, Ordering::Relaxed);
                let result = match relay_lease {
                    Some((gate, generation)) => {
                        tokio::select! {
                            result = server.handle(stream) => result,
                            _ = gate.wait_for_invalidation(generation) => Err(SocksError::RelayInactive),
                        }
                    }
                    None => server.handle(stream).await,
                };
                server.stats.active.fetch_sub(1, Ordering::Relaxed);
                drop(permit);
                if let (Err(error), Some(diagnostics)) = (result, &server.diagnostics) {
                    let _ = diagnostics.record(
                        "socks_connection_error",
                        json!({"category": error.category()}),
                    );
                }
            });
        }
    }

    async fn handle(&self, mut client: TcpStream) -> Result<(), SocksError> {
        let handshake = time::timeout(self.config.handshake_timeout, async {
            negotiate_auth(&mut client).await?;
            read_request(&mut client).await
        })
        .await
        .map_err(|_| SocksError::HandshakeTimeout)?;
        let request = match handshake {
            Ok(request) => request,
            Err(error @ SocksError::UnsupportedCommand(_)) => {
                let _ = write_reply(
                    &mut client,
                    0x07,
                    Endpoint::Socket(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
                )
                .await;
                return Err(error);
            }
            Err(error @ SocksError::UnsupportedAddressType(_)) => {
                let _ = write_reply(
                    &mut client,
                    0x08,
                    Endpoint::Socket(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
                )
                .await;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if !self.config.command_policy.allows(request.command) {
            let _ = write_reply(
                &mut client,
                0x07,
                Endpoint::Socket(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
            )
            .await;
            return Err(SocksError::CommandNotAllowed {
                command: request.command,
                policy: self.config.command_policy,
            });
        }
        match request.command {
            SocksCommand::Connect => self.handle_connect(client, request.destination).await,
            SocksCommand::FwdUdp => {
                write_reply(
                    &mut client,
                    0,
                    Endpoint::Socket(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)),
                )
                .await?;
                let (reader, writer) = client.into_split();
                relay_fwd_udp(
                    reader,
                    writer,
                    self.config.fwd_udp.clone(),
                    self.udp_stats.clone(),
                )
                .await?;
                Ok(())
            }
        }
    }

    async fn handle_connect(
        &self,
        mut client: TcpStream,
        destination: Endpoint,
    ) -> Result<(), SocksError> {
        let destination_port = endpoint_port(&destination);
        let vd_streamer_flow = VIRTUAL_DESKTOP_STREAMER_PORTS.contains(&destination_port);
        let flow_started = time::Instant::now();
        let upstream = time::timeout(self.config.connect_timeout, async {
            let target = destination.resolve().await?;
            let canonical_local_target = VIRTUAL_DESKTOP_STREAMER_PORTS.contains(&destination_port)
                && socket_address_is_local(target);
            let connect_target = if canonical_local_target {
                primary_routed_ipv4_target(target.port()).await?
            } else {
                target
            };
            let redirected_local_target = connect_target.ip() != target.ip();
            let upstream = TcpStream::connect(connect_target)
                .await
                .map_err(crate::udp::UdpRelayError::from)?;
            Ok::<_, crate::udp::UdpRelayError>((upstream, redirected_local_target))
        })
        .await;
        let (upstream, redirected_local_target) = match upstream {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                let _ = write_reply(
                    &mut client,
                    0x05,
                    Endpoint::Socket(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
                )
                .await;
                return Err(error.into());
            }
            Err(_) => {
                let _ = write_reply(
                    &mut client,
                    0x04,
                    Endpoint::Socket(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
                )
                .await;
                return Err(SocksError::ConnectTimeout);
            }
        };
        if redirected_local_target {
            if let Some(diagnostics) = &self.diagnostics {
                let _ = diagnostics.record(
                    "vd_streamer_local_target_canonicalized",
                    json!({"destination_port": destination_port}),
                );
            }
        }
        client.set_nodelay(true)?;
        upstream.set_nodelay(true)?;
        let bound = upstream.local_addr()?;
        write_reply(&mut client, 0, Endpoint::Socket(bound)).await?;
        let _virtual_desktop_flow = self.virtual_desktop_flows.begin(destination_port);
        if vd_streamer_flow {
            if let Some(diagnostics) = &self.diagnostics {
                let _ = diagnostics.record(
                    "vd_streamer_flow_opened",
                    json!({"destination_port": destination_port}),
                );
            }
        }
        let flow_observation = FlowObservation::default();
        let mut client = ObservedIo::new(client, FlowEndpoint::Quest, &flow_observation);
        let mut upstream = ObservedIo::new(upstream, FlowEndpoint::Streamer, &flow_observation);
        let transfer = io::copy_bidirectional(&mut client, &mut upstream).await;
        if vd_streamer_flow {
            if let Some(diagnostics) = &self.diagnostics {
                let duration_ms = flow_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let first_terminal = flow_observation.first_terminal();
                let first_terminal_side = first_terminal
                    .map(|terminal| terminal.endpoint.as_str())
                    .unwrap_or("unknown");
                let first_terminal_event = first_terminal
                    .map(|terminal| terminal.event.as_str())
                    .unwrap_or("unknown");
                let fields = match &transfer {
                    Ok((from_client, from_upstream)) => json!({
                        "destination_port": destination_port,
                        "duration_ms": duration_ms,
                        "first_terminal_event": first_terminal_event,
                        "first_terminal_side": first_terminal_side,
                        "outcome": "eof",
                        "tx_bytes": from_client,
                        "rx_bytes": from_upstream,
                    }),
                    Err(error) => json!({
                        "destination_port": destination_port,
                        "duration_ms": duration_ms,
                        "error_category": format!("{:?}", error.kind()),
                        "first_terminal_event": first_terminal_event,
                        "first_terminal_side": first_terminal_side,
                        "outcome": "io_error",
                    }),
                };
                let _ = diagnostics.record("vd_streamer_flow_ended", fields);
            }
        }
        let (from_client, from_upstream) = transfer?;
        self.stats
            .tcp_tx_bytes
            .fetch_add(from_client, Ordering::Relaxed);
        self.stats
            .tcp_rx_bytes
            .fetch_add(from_upstream, Ordering::Relaxed);
        Ok(())
    }
}

fn endpoint_port(endpoint: &Endpoint) -> u16 {
    match endpoint {
        Endpoint::Socket(address) => address.port(),
        Endpoint::Domain(_, port) => *port,
    }
}

fn socket_address_is_local(mut target: SocketAddr) -> bool {
    target.set_port(0);
    let socket = match target {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    };
    socket.and_then(|socket| socket.bind(target)).is_ok()
}

async fn primary_routed_ipv4_target(
    destination_port: u16,
) -> Result<SocketAddr, crate::udp::UdpRelayError> {
    let probe = TokioUdpSocket::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)).await?;
    probe
        .connect(SocketAddr::new(Ipv4Addr::new(192, 0, 2, 1).into(), 9))
        .await?;
    let local = probe.local_addr()?;
    match local.ip() {
        std::net::IpAddr::V4(address) if !address.is_unspecified() && !address.is_loopback() => {
            Ok(SocketAddr::new(address.into(), destination_port))
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "primary routed IPv4 address is unavailable",
        )
        .into()),
    }
}

async fn negotiate_auth<S>(stream: &mut S) -> Result<(), SocksError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut header = [0; 2];
    stream.read_exact(&mut header).await?;
    if header[0] != SOCKS_VERSION || header[1] == 0 {
        return Err(SocksError::Malformed("invalid authentication greeting"));
    }
    let mut methods = vec![0; header[1] as usize];
    stream.read_exact(&mut methods).await?;
    let selected = if methods.contains(&AUTH_NONE) {
        AUTH_NONE
    } else {
        AUTH_UNACCEPTABLE
    };
    stream.write_all(&[SOCKS_VERSION, selected]).await?;
    stream.flush().await?;
    if selected == AUTH_UNACCEPTABLE {
        return Err(SocksError::NoAcceptableAuthentication);
    }
    Ok(())
}

async fn read_request<R>(reader: &mut R) -> Result<SocksRequest, SocksError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0; 4];
    reader.read_exact(&mut prefix).await?;
    let tail_len = match prefix[3] {
        crate::udp::SOCKS_ATYP_IPV4 => 4 + 2,
        crate::udp::SOCKS_ATYP_IPV6 => 16 + 2,
        crate::udp::SOCKS_ATYP_DOMAIN => {
            let length = reader.read_u8().await? as usize;
            if length == 0 || length > crate::udp::MAX_DOMAIN_LEN {
                return Err(SocksError::Malformed("invalid domain length"));
            }
            let mut encoded = prefix.to_vec();
            encoded.push(length as u8);
            encoded.resize(encoded.len() + length + 2, 0);
            reader.read_exact(&mut encoded[5..]).await?;
            return SocksRequest::decode(&encoded);
        }
        other => return Err(SocksError::UnsupportedAddressType(other)),
    };
    let mut encoded = prefix.to_vec();
    encoded.resize(encoded.len() + tail_len, 0);
    reader.read_exact(&mut encoded[4..]).await?;
    SocksRequest::decode(&encoded)
}

async fn write_reply<W>(writer: &mut W, reply: u8, endpoint: Endpoint) -> Result<(), SocksError>
where
    W: AsyncWrite + Unpin,
{
    let address = encode_endpoint(&endpoint)?;
    let mut encoded = Vec::with_capacity(3 + address.len());
    encoded.extend_from_slice(&[SOCKS_VERSION, reply, 0]);
    encoded.extend_from_slice(&address);
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Clone, Default)]
struct SocksStats(Arc<SocksStatsInner>);

#[derive(Default)]
struct SocksStatsInner {
    accepted: AtomicU64,
    rejected: AtomicU64,
    active: AtomicUsize,
    tcp_tx_bytes: AtomicU64,
    tcp_rx_bytes: AtomicU64,
}

impl std::ops::Deref for SocksStats {
    type Target = SocksStatsInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl SocksStats {
    fn snapshot(&self) -> SocksStatsSnapshot {
        SocksStatsSnapshot {
            accepted_connections: self.accepted.load(Ordering::Relaxed),
            rejected_connections: self.rejected.load(Ordering::Relaxed),
            active_connections: self.active.load(Ordering::Relaxed),
            tcp_tx_bytes: self.tcp_tx_bytes.load(Ordering::Relaxed),
            tcp_rx_bytes: self.tcp_rx_bytes.load(Ordering::Relaxed),
            udp: Default::default(),
            virtual_desktop: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SocksStatsSnapshot {
    pub accepted_connections: u64,
    pub rejected_connections: u64,
    pub active_connections: usize,
    pub tcp_tx_bytes: u64,
    pub tcp_rx_bytes: u64,
    pub udp: crate::udp::UdpStatsSnapshot,
    #[serde(default)]
    pub virtual_desktop: VirtualDesktopFlowSnapshot,
}

#[derive(Debug, Error)]
pub enum SocksError {
    #[error("SOCKS listener must be loopback, got {0}")]
    NonLoopbackBind(SocketAddr),
    #[error("malformed SOCKS request: {0}")]
    Malformed(&'static str),
    #[error("SOCKS client offered no acceptable authentication method")]
    NoAcceptableAuthentication,
    #[error("unsupported SOCKS command {0:#04x}")]
    UnsupportedCommand(u8),
    #[error("SOCKS command {command:?} is not allowed on the {policy:?} listener")]
    CommandNotAllowed {
        command: SocksCommand,
        policy: SocksCommandPolicy,
    },
    #[error("unsupported SOCKS address type {0:#04x}")]
    UnsupportedAddressType(u8),
    #[error("SOCKS CONNECT timed out")]
    ConnectTimeout,
    #[error("SOCKS authentication or request timed out")]
    HandshakeTimeout,
    #[error("relay session became inactive")]
    RelayInactive,
    #[error(transparent)]
    Frame(#[from] HevFrameError),
    #[error(transparent)]
    Udp(#[from] crate::udp::UdpRelayError),
    #[error("SOCKS I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl SocksError {
    fn category(&self) -> &'static str {
        match self {
            Self::NonLoopbackBind(_) => "non_loopback_bind",
            Self::Malformed(_) => "malformed",
            Self::NoAcceptableAuthentication => "authentication",
            Self::UnsupportedCommand(_) => "unsupported_command",
            Self::CommandNotAllowed { .. } => "command_not_allowed",
            Self::UnsupportedAddressType(_) => "unsupported_address_type",
            Self::ConnectTimeout => "connect_timeout",
            Self::HandshakeTimeout => "handshake_timeout",
            Self::RelayInactive => "relay_inactive",
            Self::Frame(_) => "udp_frame",
            Self::Udp(_) => "udp_relay",
            Self::Io(_) => "io",
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use tokio::net::UdpSocket;

    use super::*;
    use crate::udp::HevUdpFrame;

    #[test]
    fn rejects_non_loopback_listener() {
        let config = SocksConfig {
            bind: "0.0.0.0:1080".parse().unwrap(),
            ..Default::default()
        };
        assert!(matches!(
            SocksServer::new(config),
            Err(SocksError::NonLoopbackBind(_))
        ));
    }

    #[test]
    fn parses_fwd_udp_request() {
        let bytes = [5, CMD_FWD_UDP, 0, 1, 127, 0, 0, 1, 0x12, 0x34];
        let request = SocksRequest::decode(&bytes).unwrap();
        assert_eq!(request.command, SocksCommand::FwdUdp);
        assert_eq!(
            request.destination,
            Endpoint::Socket("127.0.0.1:4660".parse().unwrap())
        );
    }

    #[test]
    fn virtual_desktop_health_requires_all_four_live_ports() {
        let monitor = VirtualDesktopFlowMonitor::default();
        let mut guards = Vec::new();
        for port in VIRTUAL_DESKTOP_STREAMER_PORTS {
            guards.push(monitor.begin(port).unwrap());
        }
        assert_eq!(
            monitor.snapshot(),
            VirtualDesktopFlowSnapshot {
                connected: true,
                active_ports: 4,
                active_flows: [1, 1, 1, 1],
            }
        );

        guards.pop();
        assert_eq!(monitor.snapshot().active_ports, 3);
        assert!(!monitor.snapshot().connected);
    }

    #[test]
    fn local_address_detection_requires_a_bindable_address() {
        assert!(socket_address_is_local("127.0.0.1:38810".parse().unwrap()));
        assert!(!socket_address_is_local("192.0.2.1:38810".parse().unwrap()));
    }

    #[tokio::test]
    async fn flow_observation_records_the_endpoint_that_closes_first() {
        let (quest_peer, quest_relay) = io::duplex(64);
        let observation = FlowObservation::default();
        let mut observed = ObservedIo::new(quest_relay, FlowEndpoint::Quest, &observation);

        drop(quest_peer);
        let mut byte = [0; 1];
        assert_eq!(observed.read(&mut byte).await.unwrap(), 0);
        assert_eq!(
            observation.first_terminal(),
            Some(FlowTerminal {
                endpoint: FlowEndpoint::Quest,
                event: FlowTerminalEvent::ReadEof,
            })
        );
    }

    async fn connect_through_socks(address: SocketAddr, destination: SocketAddr) -> TcpStream {
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        client.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [5, 0]);
        let mut request = vec![5, CMD_CONNECT, 0];
        request.extend_from_slice(&encode_endpoint(&Endpoint::Socket(destination)).unwrap());
        client.write_all(&request).await.unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        client
    }

    async fn connect_udp_through_socks(address: SocketAddr) -> TcpStream {
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        client.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [5, 0]);
        client
            .write_all(&[5, CMD_FWD_UDP, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        client
    }

    async fn assert_udp_echo(client: &mut TcpStream, echo_address: SocketAddr, payload: &[u8]) {
        HevUdpFrame {
            endpoint: Endpoint::Socket(echo_address),
            payload: payload.to_vec(),
        }
        .write_to(client)
        .await
        .unwrap();
        let response = HevUdpFrame::read_from(client).await.unwrap();
        assert_eq!(response.endpoint, Endpoint::Socket(echo_address));
        assert_eq!(response.payload, payload);
    }

    #[tokio::test]
    async fn connect_preserves_a_large_bidirectional_stream() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_address = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let (mut reader, mut writer) = stream.split();
            io::copy(&mut reader, &mut writer).await.unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = SocksServer::new(SocksConfig {
            bind: address,
            ..Default::default()
        })
        .unwrap();
        let stats = server.clone();
        let task = tokio::spawn(server.serve_on(listener));

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        client.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [5, 0]);
        let mut request = vec![5, CMD_CONNECT, 0];
        request.extend_from_slice(&encode_endpoint(&Endpoint::Socket(echo_address)).unwrap());
        client.write_all(&request).await.unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        let payload: Vec<u8> = (0..(2 * 1024 * 1024 + 1))
            .map(|index| (index % 251) as u8)
            .collect();
        let expected = payload.clone();
        let byte_count = payload.len() as u64;
        let (mut reader, mut writer) = client.into_split();
        let sender = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let mut echoed = vec![0; expected.len()];
        reader.read_exact(&mut echoed).await.unwrap();
        sender.await.unwrap();
        assert_eq!(echoed, expected);

        time::timeout(Duration::from_secs(5), async {
            while stats.stats().active_connections != 0 {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(stats.stats().tcp_tx_bytes, byte_count);
        assert_eq!(stats.stats().tcp_rx_bytes, byte_count);
        task.abort();
    }

    #[tokio::test]
    async fn explicit_suspend_closes_old_tcp_flows_and_reopens_the_same_listener() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_address = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut reader, mut writer) = stream.split();
                    let _ = io::copy(&mut reader, &mut writer).await;
                });
            }
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gate = RelayGate::default();
        gate.set_enabled(true);
        let server = SocksServer::new(SocksConfig {
            bind: address,
            ..Default::default()
        })
        .unwrap()
        .with_relay_gate(gate.clone());
        let stats = server.clone();
        let task = tokio::spawn(server.serve_on(listener));

        let mut first = connect_through_socks(address, echo_address).await;
        first.write_all(b"before-sleep").await.unwrap();
        let mut echoed = [0; 12];
        first.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"before-sleep");
        assert_eq!(stats.stats().active_connections, 1);

        gate.set_enabled(false);
        let closed = time::timeout(Duration::from_millis(500), first.read_u8())
            .await
            .expect("old flow stayed open after explicit suspend");
        assert!(closed.is_err());
        time::timeout(Duration::from_millis(500), async {
            while stats.stats().active_connections != 0 {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let mut rejected = TcpStream::connect(address).await.unwrap();
        rejected.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        let rejected_read =
            time::timeout(Duration::from_millis(500), rejected.read_exact(&mut auth))
                .await
                .expect("degraded relay left a new connection pending");
        assert!(rejected_read.is_err());

        gate.set_enabled(true);
        let mut second = connect_through_socks(address, echo_address).await;
        second.write_all(b"after-wake").await.unwrap();
        let mut echoed = [0; 10];
        second.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"after-wake");

        task.abort();
        echo_task.abort();
    }

    #[tokio::test]
    async fn explicit_suspend_closes_udp_flow_and_wake_accepts_a_fresh_one() {
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_address = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            let mut buffer = [0; 2048];
            loop {
                let (length, peer) = echo.recv_from(&mut buffer).await.unwrap();
                echo.send_to(&buffer[..length], peer).await.unwrap();
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gate = RelayGate::default();
        gate.set_enabled(true);
        let server = SocksServer::new(SocksConfig {
            bind: address,
            command_policy: SocksCommandPolicy::FwdUdpOnly,
            ..Default::default()
        })
        .unwrap()
        .with_relay_gate(gate.clone());
        let task = tokio::spawn(server.serve_on(listener));

        let mut first = connect_udp_through_socks(address).await;
        assert_udp_echo(&mut first, echo_address, b"before-sleep").await;

        gate.set_enabled(false);
        let closed = time::timeout(Duration::from_millis(500), first.read_u8())
            .await
            .expect("UDP flow stayed open after explicit suspend");
        assert!(closed.is_err());

        let mut rejected = TcpStream::connect(address).await.unwrap();
        rejected.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        let rejected_read =
            time::timeout(Duration::from_millis(500), rejected.read_exact(&mut auth))
                .await
                .expect("suspended UDP listener left a new connection pending");
        assert!(rejected_read.is_err());

        gate.set_enabled(true);
        let mut second = connect_udp_through_socks(address).await;
        assert_udp_echo(&mut second, echo_address, b"after-wake").await;

        task.abort();
        echo_task.abort();
    }

    async fn assert_command_rejected(policy: SocksCommandPolicy, command: u8) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = SocksServer::new(SocksConfig {
            bind: address,
            command_policy: policy,
            ..Default::default()
        })
        .unwrap();
        let task = tokio::spawn(server.serve_on(listener));

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        client.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [5, 0]);
        client
            .write_all(&[5, command, 0, 1, 127, 0, 0, 1, 0, 9])
            .await
            .unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0x07);
        task.abort();
    }

    #[tokio::test]
    async fn split_listeners_reject_the_other_lane_command() {
        assert_command_rejected(SocksCommandPolicy::ConnectOnly, CMD_FWD_UDP).await;
        assert_command_rejected(SocksCommandPolicy::FwdUdpOnly, CMD_CONNECT).await;
    }

    #[tokio::test]
    async fn stalled_handshake_releases_the_bounded_connection_slot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = SocksServer::new(SocksConfig {
            bind: address,
            max_connections: 1,
            handshake_timeout: Duration::from_millis(30),
            ..Default::default()
        })
        .unwrap();
        let task = tokio::spawn(server.serve_on(listener));

        let stalled = TcpStream::connect(address).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        time::timeout(Duration::from_millis(100), client.read_exact(&mut auth))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(auth, [5, 0]);

        drop(stalled);
        task.abort();
    }

    proptest! {
        #[test]
        fn arbitrary_request_never_panics(input in prop::collection::vec(any::<u8>(), 0..1024)) {
            let _ = SocksRequest::decode(&input);
        }
    }
}
