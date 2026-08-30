//! HEV SOCKS5 `FWD UDP` (command 0x05) UDP-in-TCP framing and relay.
//!
//! This codec follows the pinned hev-socks5-core implementation: the leading
//! `u16` is the UDP payload length (despite older prose calling it the total
//! message length), `HDRLEN` includes those two bytes through `DST.PORT`, and
//! the address itself uses RFC 1928 ATYP encoding.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{lookup_host, UdpSocket},
    sync::mpsc,
    time,
};

use crate::diagnostics::{LatencyHistogram, LatencyHistogramSnapshot};

pub const SOCKS_ATYP_IPV4: u8 = 0x01;
pub const SOCKS_ATYP_DOMAIN: u8 = 0x03;
pub const SOCKS_ATYP_IPV6: u8 = 0x04;
pub const MAX_UDP_PAYLOAD: usize = 65_507;
pub const MAX_DOMAIN_LEN: usize = 255;
pub const VIRTUAL_DESKTOP_STREAMER_PORTS: [u16; 4] = [38_810, 38_820, 38_830, 38_840];
pub const DEFAULT_ASSOCIATION_BYTE_BUDGET: usize = 256 * 1024;
pub const DEFAULT_GLOBAL_QUEUE_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const MAX_WRITE_BATCH_BYTES: usize = u8::MAX as usize + MAX_UDP_PAYLOAD;
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Endpoint {
    Socket(SocketAddr),
    Domain(String, u16),
}

impl Endpoint {
    pub async fn resolve(&self) -> Result<SocketAddr, UdpRelayError> {
        match self {
            Self::Socket(address) => Ok(*address),
            Self::Domain(host, port) => lookup_host((host.as_str(), *port))
                .await?
                .next()
                .ok_or_else(|| UdpRelayError::Unresolved(host.clone())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HevUdpFrame {
    pub endpoint: Endpoint,
    pub payload: Vec<u8>,
}

impl HevUdpFrame {
    pub fn encode(&self) -> Result<Vec<u8>, HevFrameError> {
        let mut output = Vec::with_capacity(self.encoded_len()?);
        self.encode_into(&mut output)?;
        Ok(output)
    }

    fn encoded_len(&self) -> Result<usize, HevFrameError> {
        if self.payload.len() > MAX_UDP_PAYLOAD {
            return Err(HevFrameError::PayloadTooLarge(self.payload.len()));
        }
        let address = encode_endpoint(&self.endpoint)?;
        let header_len = 3usize + address.len();
        u8::try_from(header_len).map_err(|_| HevFrameError::HeaderTooLarge(header_len))?;
        Ok(header_len + self.payload.len())
    }

    fn encode_into(&self, output: &mut Vec<u8>) -> Result<(), HevFrameError> {
        if self.payload.len() > MAX_UDP_PAYLOAD {
            return Err(HevFrameError::PayloadTooLarge(self.payload.len()));
        }
        let address = encode_endpoint(&self.endpoint)?;
        let header_len = 3usize + address.len();
        let header_len =
            u8::try_from(header_len).map_err(|_| HevFrameError::HeaderTooLarge(header_len))?;
        output.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        output.push(header_len);
        output.extend_from_slice(&address);
        output.extend_from_slice(&self.payload);
        Ok(())
    }

    /// Pure parser entry point intended for property tests and fuzzers.
    pub fn decode(input: &[u8]) -> Result<Self, HevFrameError> {
        if input.len() < 3 {
            return Err(HevFrameError::Truncated);
        }
        let payload_len = u16::from_be_bytes([input[0], input[1]]) as usize;
        if payload_len > MAX_UDP_PAYLOAD {
            return Err(HevFrameError::PayloadTooLarge(payload_len));
        }
        let header_len = input[2] as usize;
        if header_len < 3 || header_len > input.len() {
            return Err(HevFrameError::InvalidHeaderLength(header_len));
        }
        let expected = header_len
            .checked_add(payload_len)
            .ok_or(HevFrameError::PayloadTooLarge(payload_len))?;
        if input.len() != expected {
            return Err(HevFrameError::LengthMismatch {
                expected,
                actual: input.len(),
            });
        }
        let (endpoint, consumed) = decode_endpoint(&input[3..header_len])?;
        if consumed + 3 != header_len {
            return Err(HevFrameError::InvalidHeaderLength(header_len));
        }
        Ok(Self {
            endpoint,
            payload: input[header_len..].to_vec(),
        })
    }

    pub async fn read_from<R>(reader: &mut R) -> Result<Self, HevFrameError>
    where
        R: AsyncRead + Unpin,
    {
        let mut prefix = [0; 3];
        reader.read_exact(&mut prefix).await?;
        let payload_len = u16::from_be_bytes([prefix[0], prefix[1]]) as usize;
        if payload_len > MAX_UDP_PAYLOAD {
            return Err(HevFrameError::PayloadTooLarge(payload_len));
        }
        let header_len = prefix[2] as usize;
        if header_len < 3 {
            return Err(HevFrameError::InvalidHeaderLength(header_len));
        }
        let total = header_len
            .checked_add(payload_len)
            .ok_or(HevFrameError::PayloadTooLarge(payload_len))?;
        let mut encoded = Vec::with_capacity(total);
        encoded.extend_from_slice(&prefix);
        encoded.resize(total, 0);
        reader.read_exact(&mut encoded[3..]).await?;
        Self::decode(&encoded)
    }

    pub async fn write_to<W>(&self, writer: &mut W) -> Result<(), HevFrameError>
    where
        W: AsyncWrite + Unpin,
    {
        writer.write_all(&self.encode()?).await?;
        writer.flush().await?;
        Ok(())
    }
}

pub(crate) fn encode_endpoint(endpoint: &Endpoint) -> Result<Vec<u8>, HevFrameError> {
    let mut output = Vec::new();
    match endpoint {
        Endpoint::Socket(SocketAddr::V4(address)) => {
            output.push(SOCKS_ATYP_IPV4);
            output.extend_from_slice(&address.ip().octets());
            output.extend_from_slice(&address.port().to_be_bytes());
        }
        Endpoint::Socket(SocketAddr::V6(address)) => {
            output.push(SOCKS_ATYP_IPV6);
            output.extend_from_slice(&address.ip().octets());
            output.extend_from_slice(&address.port().to_be_bytes());
        }
        Endpoint::Domain(host, port) => {
            let bytes = host.as_bytes();
            if bytes.is_empty() || bytes.len() > MAX_DOMAIN_LEN {
                return Err(HevFrameError::InvalidDomainLength(bytes.len()));
            }
            output.push(SOCKS_ATYP_DOMAIN);
            output.push(bytes.len() as u8);
            output.extend_from_slice(bytes);
            output.extend_from_slice(&port.to_be_bytes());
        }
    }
    Ok(output)
}

pub(crate) fn decode_endpoint(input: &[u8]) -> Result<(Endpoint, usize), HevFrameError> {
    let Some(&atyp) = input.first() else {
        return Err(HevFrameError::TruncatedAddress);
    };
    match atyp {
        SOCKS_ATYP_IPV4 => {
            if input.len() < 7 {
                return Err(HevFrameError::TruncatedAddress);
            }
            let ip = Ipv4Addr::new(input[1], input[2], input[3], input[4]);
            let port = u16::from_be_bytes([input[5], input[6]]);
            Ok((Endpoint::Socket(SocketAddr::new(ip.into(), port)), 7))
        }
        SOCKS_ATYP_IPV6 => {
            if input.len() < 19 {
                return Err(HevFrameError::TruncatedAddress);
            }
            let mut octets = [0; 16];
            octets.copy_from_slice(&input[1..17]);
            let port = u16::from_be_bytes([input[17], input[18]]);
            Ok((
                Endpoint::Socket(SocketAddr::new(Ipv6Addr::from(octets).into(), port)),
                19,
            ))
        }
        SOCKS_ATYP_DOMAIN => {
            let Some(&length) = input.get(1) else {
                return Err(HevFrameError::TruncatedAddress);
            };
            let length = length as usize;
            if length == 0 || length > MAX_DOMAIN_LEN || input.len() < length + 4 {
                return Err(HevFrameError::InvalidDomainLength(length));
            }
            let host = std::str::from_utf8(&input[2..2 + length])
                .map_err(|_| HevFrameError::InvalidDomain)?
                .to_owned();
            let port_offset = 2 + length;
            let port = u16::from_be_bytes([input[port_offset], input[port_offset + 1]]);
            Ok((Endpoint::Domain(host, port), length + 4))
        }
        other => Err(HevFrameError::UnsupportedAddressType(other)),
    }
}

#[derive(Debug, Error)]
pub enum HevFrameError {
    #[error("HEV UDP frame is truncated")]
    Truncated,
    #[error("HEV UDP address is truncated")]
    TruncatedAddress,
    #[error("HEV UDP payload length {0} exceeds the bound")]
    PayloadTooLarge(usize),
    #[error("HEV UDP header length {0} is invalid")]
    InvalidHeaderLength(usize),
    #[error("HEV UDP header length {0} cannot fit on wire")]
    HeaderTooLarge(usize),
    #[error("HEV UDP frame length mismatch: expected {expected}, got {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("unsupported SOCKS address type {0:#04x}")]
    UnsupportedAddressType(u8),
    #[error("invalid SOCKS domain length {0}")]
    InvalidDomainLength(usize),
    #[error("SOCKS domain is not UTF-8")]
    InvalidDomain,
    #[error("HEV UDP I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct FwdUdpConfig {
    pub queue_capacity: usize,
    pub max_queue_age: Duration,
    pub idle_timeout: Duration,
    pub association_byte_budget: usize,
}

impl Default for FwdUdpConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 64,
            max_queue_age: Duration::from_millis(10),
            // Android owns idle expiry at 120 seconds. Keep the host side
            // slightly longer so an association is never torn down first.
            idle_timeout: Duration::from_secs(130),
            association_byte_budget: DEFAULT_ASSOCIATION_BYTE_BUDGET,
        }
    }
}

#[derive(Clone)]
pub struct UdpStats(Arc<UdpStatsInner>);

struct UdpStatsInner {
    active_flows: AtomicUsize,
    tx_datagrams: AtomicU64,
    tx_bytes: AtomicU64,
    rx_datagrams: AtomicU64,
    rx_bytes: AtomicU64,
    dropped_queue_age: AtomicU64,
    dropped_queue_full: AtomicU64,
    malformed_frames: AtomicU64,
    queued_bytes: AtomicUsize,
    max_queued_bytes: usize,
    queue_residence: LatencyHistogram,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UdpStatsSnapshot {
    pub active_flows: usize,
    pub tx_datagrams: u64,
    pub tx_bytes: u64,
    pub rx_datagrams: u64,
    pub rx_bytes: u64,
    pub dropped_queue_age: u64,
    pub dropped_queue_full: u64,
    pub malformed_frames: u64,
    pub queued_bytes: usize,
    pub queue_residence_us: LatencyHistogramSnapshot,
}

impl UdpStats {
    pub fn snapshot(&self) -> UdpStatsSnapshot {
        UdpStatsSnapshot {
            active_flows: self.0.active_flows.load(Ordering::Relaxed),
            tx_datagrams: self.0.tx_datagrams.load(Ordering::Relaxed),
            tx_bytes: self.0.tx_bytes.load(Ordering::Relaxed),
            rx_datagrams: self.0.rx_datagrams.load(Ordering::Relaxed),
            rx_bytes: self.0.rx_bytes.load(Ordering::Relaxed),
            dropped_queue_age: self.0.dropped_queue_age.load(Ordering::Relaxed),
            dropped_queue_full: self.0.dropped_queue_full.load(Ordering::Relaxed),
            malformed_frames: self.0.malformed_frames.load(Ordering::Relaxed),
            queued_bytes: self.0.queued_bytes.load(Ordering::Relaxed),
            queue_residence_us: self.0.queue_residence.snapshot(),
        }
    }

    #[cfg(test)]
    fn with_byte_budget(max_queued_bytes: usize) -> Self {
        Self(Arc::new(UdpStatsInner::new(max_queued_bytes)))
    }
}

impl Default for UdpStats {
    fn default() -> Self {
        Self(Arc::new(UdpStatsInner::new(
            DEFAULT_GLOBAL_QUEUE_BYTE_BUDGET,
        )))
    }
}

impl UdpStatsInner {
    fn new(max_queued_bytes: usize) -> Self {
        Self {
            active_flows: AtomicUsize::new(0),
            tx_datagrams: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_datagrams: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            dropped_queue_age: AtomicU64::new(0),
            dropped_queue_full: AtomicU64::new(0),
            malformed_frames: AtomicU64::new(0),
            queued_bytes: AtomicUsize::new(0),
            max_queued_bytes,
            queue_residence: LatencyHistogram::default(),
        }
    }
}

struct QueuedFrame {
    frame: HevUdpFrame,
    queued_at: Instant,
    _reservation: QueueReservation,
}

struct QueuedReply {
    frame: HevUdpFrame,
    queued_at: Instant,
    _reservation: QueueReservation,
}

struct QueueBudget {
    association_bytes: Arc<AtomicUsize>,
    association_limit: usize,
    stats: UdpStats,
}

impl QueueBudget {
    fn reserve(&self, bytes: usize) -> Option<QueueReservation> {
        if !try_reserve(&self.association_bytes, self.association_limit, bytes) {
            return None;
        }
        if !try_reserve(
            &self.stats.0.queued_bytes,
            self.stats.0.max_queued_bytes,
            bytes,
        ) {
            self.association_bytes.fetch_sub(bytes, Ordering::Relaxed);
            return None;
        }
        Some(QueueReservation {
            bytes,
            association_bytes: self.association_bytes.clone(),
            stats: self.stats.clone(),
        })
    }
}

struct QueueReservation {
    bytes: usize,
    association_bytes: Arc<AtomicUsize>,
    stats: UdpStats,
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        self.association_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
        self.stats
            .0
            .queued_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

fn try_reserve(counter: &AtomicUsize, limit: usize, bytes: usize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(bytes).filter(|next| *next <= limit)
        })
        .is_ok()
}

fn accounted_frame_bytes(frame: &HevUdpFrame) -> usize {
    frame.payload.len().saturating_add(64)
}

enum EnqueueResult {
    Queued,
    Dropped,
    Closed,
}

fn try_queue_request(
    sender: &mpsc::Sender<Result<QueuedFrame, HevFrameError>>,
    frame: HevUdpFrame,
    budget: &QueueBudget,
    stats: &UdpStats,
) -> EnqueueResult {
    let Some(reservation) = budget.reserve(accounted_frame_bytes(&frame)) else {
        stats.0.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
        return EnqueueResult::Dropped;
    };
    match sender.try_send(Ok(QueuedFrame {
        frame,
        queued_at: Instant::now(),
        _reservation: reservation,
    })) {
        Ok(()) => EnqueueResult::Queued,
        Err(mpsc::error::TrySendError::Full(_)) => {
            stats.0.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
            EnqueueResult::Dropped
        }
        Err(mpsc::error::TrySendError::Closed(_)) => EnqueueResult::Closed,
    }
}

/// Runs one full-cone HEV FWD UDP association. The IPv4 and IPv6 host sockets
/// remain stable for the lifetime of the SOCKS TCP connection.
pub async fn relay_fwd_udp<R, W>(
    mut reader: R,
    mut writer: W,
    config: FwdUdpConfig,
    stats: UdpStats,
) -> Result<(), UdpRelayError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let ipv4 = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    ipv4.set_broadcast(true)?;
    let ipv6 = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await?;
    stats.0.active_flows.fetch_add(1, Ordering::Relaxed);
    let _flow_guard = FlowGuard(stats.clone());
    let budget = Arc::new(QueueBudget {
        association_bytes: Arc::new(AtomicUsize::new(0)),
        association_limit: config.association_byte_budget,
        stats: stats.clone(),
    });

    let (request_tx, mut request_rx) =
        mpsc::channel::<Result<QueuedFrame, HevFrameError>>(config.queue_capacity);
    let (reply_tx, mut reply_rx) = mpsc::channel::<QueuedReply>(config.queue_capacity);
    let reader_stats = stats.clone();
    let reader_budget = budget.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = match HevUdpFrame::read_from(&mut reader).await {
                Ok(frame) => frame,
                Err(HevFrameError::Io(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok::<(), UdpRelayError>(());
                }
                Err(error) => {
                    reader_stats
                        .0
                        .malformed_frames
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = request_tx.send(Err(error)).await;
                    return Ok(());
                }
            };
            match try_queue_request(&request_tx, frame, &reader_budget, &reader_stats) {
                EnqueueResult::Queued | EnqueueResult::Dropped => {}
                EnqueueResult::Closed => return Ok(()),
            }
        }
    });
    let writer_stats = stats.clone();
    let writer_budget = budget.clone();
    let max_reply_age = config.max_queue_age;
    let mut writer_task = tokio::spawn(async move {
        while let Some(first) = reply_rx.recv().await {
            let mut batch = Vec::with_capacity(MAX_WRITE_BATCH_BYTES);
            let mut batch_reservations = Vec::with_capacity(8);
            for reply in std::iter::once(first)
                .chain(std::iter::from_fn(|| reply_rx.try_recv().ok()))
                .take(8)
            {
                let queue_age = reply.queued_at.elapsed();
                writer_stats.0.queue_residence.record(queue_age);
                if queue_age > max_reply_age {
                    writer_stats
                        .0
                        .dropped_queue_age
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let encoded_len = reply.frame.encoded_len()?;
                if !batch.is_empty()
                    && batch.len().saturating_add(encoded_len) > MAX_WRITE_BATCH_BYTES
                {
                    writer.write_all(&batch).await?;
                    writer.flush().await?;
                    batch.clear();
                    batch_reservations.clear();
                }
                let Some(batch_reservation) = writer_budget.reserve(encoded_len) else {
                    writer_stats
                        .0
                        .dropped_queue_full
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                reply.frame.encode_into(&mut batch)?;
                batch_reservations.push(batch_reservation);
            }
            if !batch.is_empty() {
                writer.write_all(&batch).await?;
                writer.flush().await?;
            }
        }
        Ok::<(), UdpRelayError>(())
    });
    // Dropping the outer relay future (for example when the control heartbeat
    // degrades on headset sleep) must not detach either stream task.
    let _task_abort_guard =
        AbortTasksOnDrop(vec![reader_task.abort_handle(), writer_task.abort_handle()]);

    let mut ipv4_buffer = vec![0; MAX_UDP_PAYLOAD];
    let mut ipv6_buffer = vec![0; MAX_UDP_PAYLOAD];
    let idle = time::sleep(config.idle_timeout);
    tokio::pin!(idle);
    let relay_result: Result<(), UdpRelayError> = loop {
        tokio::select! {
            request = request_rx.recv() => {
                let Some(request) = request else { break Ok(()); };
                let request = match request {
                    Ok(request) => request,
                    Err(error) => break Err(error.into()),
                };
                idle.as_mut().reset(time::Instant::now() + config.idle_timeout);
                let queue_age = request.queued_at.elapsed();
                stats.0.queue_residence.record(queue_age);
                if queue_age > config.max_queue_age {
                    stats.0.dropped_queue_age.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let Some(resolve_budget) = config
                    .max_queue_age
                    .checked_sub(request.queued_at.elapsed())
                else {
                    stats.0.dropped_queue_age.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let target = match time::timeout(resolve_budget, request.frame.endpoint.resolve()).await {
                    Ok(result) => result?,
                    Err(_) => {
                        stats.0.dropped_queue_age.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                let socket = if target.is_ipv4() { &ipv4 } else { &ipv6 };
                socket.send_to(&request.frame.payload, target).await?;
                stats.0.tx_datagrams.fetch_add(1, Ordering::Relaxed);
                stats.0.tx_bytes.fetch_add(request.frame.payload.len() as u64, Ordering::Relaxed);
            }
            result = ipv4.recv_from(&mut ipv4_buffer) => {
                let (length, source) = result?;
                idle.as_mut().reset(time::Instant::now() + config.idle_timeout);
                stats.0.rx_datagrams.fetch_add(1, Ordering::Relaxed);
                stats.0.rx_bytes.fetch_add(length as u64, Ordering::Relaxed);
                let Some(reservation) = budget.reserve(length.saturating_add(64)) else {
                    stats.0.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                if reply_tx.try_send(QueuedReply {
                    frame: HevUdpFrame {
                        endpoint: Endpoint::Socket(source),
                        payload: ipv4_buffer[..length].to_vec(),
                    },
                    queued_at: Instant::now(),
                    _reservation: reservation,
                }).is_err() {
                    stats.0.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                }
            }
            result = ipv6.recv_from(&mut ipv6_buffer) => {
                let (length, source) = result?;
                idle.as_mut().reset(time::Instant::now() + config.idle_timeout);
                stats.0.rx_datagrams.fetch_add(1, Ordering::Relaxed);
                stats.0.rx_bytes.fetch_add(length as u64, Ordering::Relaxed);
                let Some(reservation) = budget.reserve(length.saturating_add(64)) else {
                    stats.0.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                if reply_tx.try_send(QueuedReply {
                    frame: HevUdpFrame {
                        endpoint: Endpoint::Socket(source),
                        payload: ipv6_buffer[..length].to_vec(),
                    },
                    queued_at: Instant::now(),
                    _reservation: reservation,
                }).is_err() {
                    stats.0.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ = reply_tx.closed() => break Err(UdpRelayError::WriterClosed),
            _ = &mut idle => break Ok(()),
        }
    };

    reader_task.abort();
    drop(reply_tx);
    if relay_result.is_err() && !writer_task.is_finished() {
        writer_task.abort();
    }
    let writer_result = match time::timeout(WRITER_SHUTDOWN_TIMEOUT, &mut writer_task).await {
        Ok(result) => result,
        Err(_) => {
            writer_task.abort();
            let _ = writer_task.await;
            return Err(UdpRelayError::WriterShutdownTimeout);
        }
    };
    relay_result?;
    match writer_result {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(UdpRelayError::Task(error.to_string())),
    }
}

struct FlowGuard(UdpStats);

struct AbortTasksOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortTasksOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl Drop for FlowGuard {
    fn drop(&mut self) {
        self.0 .0.active_flows.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Error)]
pub enum UdpRelayError {
    #[error(transparent)]
    Frame(#[from] HevFrameError),
    #[error("UDP relay I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not resolve {0}")]
    Unresolved(String),
    #[error("UDP relay task failed: {0}")]
    Task(String),
    #[error("UDP reply writer closed unexpectedly")]
    WriterClosed,
    #[error("UDP reply writer did not shut down within its deadline")]
    WriterShutdownTimeout,
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn host_udp_association_outlives_android_idle_expiry() {
        assert_eq!(
            FwdUdpConfig::default().idle_timeout,
            Duration::from_secs(130)
        );
    }

    #[test]
    fn matches_pinned_hev_ipv4_wire_shape() {
        let frame = HevUdpFrame {
            endpoint: Endpoint::Socket("192.0.2.1:4660".parse().unwrap()),
            payload: b"abc".to_vec(),
        };
        assert_eq!(
            frame.encode().unwrap(),
            vec![0, 3, 10, 1, 192, 0, 2, 1, 0x12, 0x34, b'a', b'b', b'c']
        );
    }

    #[test]
    fn ipv6_and_domain_round_trip() {
        for endpoint in [
            Endpoint::Socket("[2001:db8::1]:5353".parse().unwrap()),
            Endpoint::Domain("example.test".into(), 443),
        ] {
            let frame = HevUdpFrame {
                endpoint,
                payload: vec![0, 1, 2, 3],
            };
            assert_eq!(
                HevUdpFrame::decode(&frame.encode().unwrap()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn byte_budget_and_channel_saturation_drop_without_growth() {
        let stats = UdpStats::with_byte_budget(70_000);
        let budget = QueueBudget {
            association_bytes: Arc::new(AtomicUsize::new(0)),
            association_limit: 70_000,
            stats: stats.clone(),
        };
        let first = budget.reserve(65_507 + 64).unwrap();
        assert!(budget.reserve(65_507 + 64).is_none());
        assert!(stats.snapshot().queued_bytes <= 70_000);

        let (sender, _receiver) = mpsc::channel(1);
        let frame = HevUdpFrame {
            endpoint: Endpoint::Socket("127.0.0.1:9".parse().unwrap()),
            payload: vec![0; 1_500],
        };
        let separate = UdpStats::with_byte_budget(10_000);
        let separate_budget = QueueBudget {
            association_bytes: Arc::new(AtomicUsize::new(0)),
            association_limit: 10_000,
            stats: separate.clone(),
        };
        assert!(matches!(
            try_queue_request(&sender, frame.clone(), &separate_budget, &separate),
            EnqueueResult::Queued
        ));
        assert!(matches!(
            try_queue_request(&sender, frame, &separate_budget, &separate),
            EnqueueResult::Dropped
        ));
        assert_eq!(separate.snapshot().dropped_queue_full, 1);
        drop(first);
        assert_eq!(stats.snapshot().queued_bytes, 0);
    }

    #[tokio::test]
    async fn cancelling_udp_association_drops_all_stream_tasks_and_accounting() {
        let (client, relay) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, _client_writer) = tokio::io::split(client);
        let (relay_reader, relay_writer) = tokio::io::split(relay);
        let stats = UdpStats::default();
        let relay_stats = stats.clone();
        let task = tokio::spawn(async move {
            relay_fwd_udp(
                relay_reader,
                relay_writer,
                FwdUdpConfig::default(),
                relay_stats,
            )
            .await
        });

        time::timeout(Duration::from_millis(500), async {
            while stats.snapshot().active_flows != 1 {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;

        time::timeout(Duration::from_millis(500), async {
            while stats.snapshot().active_flows != 0 || stats.snapshot().queued_bytes != 0 {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let closed = time::timeout(Duration::from_millis(500), client_reader.read_u8())
            .await
            .expect("detached UDP stream task kept the association open");
        assert!(closed.is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_input_never_panics(input in prop::collection::vec(any::<u8>(), 0..70_000)) {
            let _ = HevUdpFrame::decode(&input);
        }

        #[test]
        fn ipv4_payload_round_trips(
            octets in any::<[u8; 4]>(),
            port in any::<u16>(),
            payload in prop::collection::vec(any::<u8>(), 0..4096),
        ) {
            let frame = HevUdpFrame {
                endpoint: Endpoint::Socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port)),
                payload,
            };
            prop_assert_eq!(HevUdpFrame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
    }
}
