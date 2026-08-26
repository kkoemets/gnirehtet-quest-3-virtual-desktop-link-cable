use std::{
    collections::HashSet,
    env, fs, io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "windows")]
use std::io::Read;
#[cfg(target_os = "windows")]
use wait_timeout::ChildExt;

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    process::{Child as TokioChild, Command as TokioCommand},
    sync::{Mutex, Notify, Semaphore},
    task, time,
};

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use crate::{
    adb::{AdbController, AndroidVpnStatus, CONTROL_PORT, SOCKS_PORT, UDP_STREAM_PORT},
    control::{
        ControlConfig, ControlHandle, ControlServer, StateObserver, SuspendObserver, WakeObserver,
        CONTROL_TRANSPORT_LOST_REASON,
    },
    diagnostics::{
        DiagnosticWriteMetrics, Diagnostics, DEFAULT_FILE_COUNT, DEFAULT_MAX_BYTES,
        DEFAULT_TOTAL_BYTES,
    },
    protocol::{AndroidMetricsV1, SessionId},
    socks::{RelayGate, SocksCommandPolicy, SocksConfig, SocksServer, VirtualDesktopFlowMonitor},
    state::{HostState, StateSnapshot},
};

const MAX_ADMIN_MESSAGE: usize = 4 * 1024;
const MAX_ADMIN_CONNECTIONS: usize = 8;
const ADMIN_IO_TIMEOUT: Duration = Duration::from_secs(3);
const TRANSPORT_RECOVERY_REASON: &str = "rebuilding the wired transport generation";
const TRANSPORT_RECOVERY_MAX_ATTEMPTS: u32 = 2;
const TRANSPORT_RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(1);
const TRANSPORT_RECOVERY_REARM_DELAY: Duration = Duration::from_secs(30);
const TRANSPORT_RECOVERY_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const VIRTUAL_DESKTOP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(any(target_os = "windows", test))]
const VIRTUAL_DESKTOP_CONNECTION_STABLE: Duration = Duration::from_secs(10);
#[cfg(any(target_os = "windows", test))]
const VIRTUAL_DESKTOP_WATCHDOG_LOSS_GRACE: Duration = Duration::from_secs(8);
#[cfg(target_os = "windows")]
pub const VIRTUAL_DESKTOP_RECOVERY_TASK: &str = "Quest VD Wired - Virtual Desktop Recovery";
#[cfg(target_os = "windows")]
const VIRTUAL_DESKTOP_SERVICE_NAME: &str = "VirtualDesktop.Service.exe";
#[cfg(target_os = "windows")]
const VIRTUAL_DESKTOP_RECOVERY_ARGUMENTS: &str = r#"-NoProfile -NonInteractive -WindowStyle Hidden -Command "$ErrorActionPreference='Stop';$session=(Get-Process -Id $PID).SessionId;$p=@(Get-Process -Name 'VirtualDesktop.Streamer' -ErrorAction SilentlyContinue|Where-Object {$_.SessionId -eq $session});if($p){$p|Stop-Process -Force};Restart-Service -Name VirtualDesktop.Service.exe -Force""#;

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub root: PathBuf,
    pub logs: PathBuf,
    pub status: PathBuf,
    pub daemon_pid: PathBuf,
    pub operation_lock: PathBuf,
    pub runtime_lock: PathBuf,
    pub admin_token: PathBuf,
    #[cfg(unix)]
    pub admin_socket: PathBuf,
}

impl AppPaths {
    pub fn discover(override_root: Option<PathBuf>) -> io::Result<Self> {
        let root = if let Some(root) = override_root {
            root
        } else if let Some(root) = env::var_os("GNIREHTET_VD_HOME") {
            PathBuf::from(root)
        } else if cfg!(target_os = "windows") {
            env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(env::temp_dir)
                .join("GnirehtetVD")
        } else {
            env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .unwrap_or_else(env::temp_dir)
                .join("gnirehtet-vd")
        };
        fs::create_dir_all(&root)?;
        let logs = root.join("logs");
        fs::create_dir_all(&logs)?;
        Ok(Self {
            status: root.join("status.json"),
            daemon_pid: root.join("daemon.pid"),
            operation_lock: root.join("operation.lock"),
            runtime_lock: root.join("runtime.lock"),
            admin_token: root.join("admin.token"),
            #[cfg(unix)]
            admin_socket: root.join("admin.sock"),
            logs,
            root,
        })
    }
}

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
    lock: Arc<StdMutex<()>>,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(StdMutex::new(())),
        }
    }

    pub fn write(&self, snapshot: &StateSnapshot, daemon_pid: Option<u32>) -> io::Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| io::Error::other("state store lock poisoned"))?;
        let existing = fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedStatus>(&bytes).ok());
        let telemetry = existing
            .as_ref()
            .and_then(|status| status.telemetry.clone());
        let runtime_ready = daemon_pid.is_some()
            && existing
                .as_ref()
                .is_some_and(|status| status.daemon_pid == daemon_pid && status.runtime_ready);
        self.write_document(snapshot, daemon_pid, runtime_ready, telemetry)
    }

    pub fn write_with_telemetry(
        &self,
        snapshot: &StateSnapshot,
        daemon_pid: Option<u32>,
        telemetry: RuntimeTelemetry,
    ) -> io::Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| io::Error::other("state store lock poisoned"))?;
        let runtime_ready = daemon_pid.is_some()
            && fs::read(&self.path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<PersistedStatus>(&bytes).ok())
                .is_some_and(|status| status.daemon_pid == daemon_pid && status.runtime_ready);
        self.write_document(snapshot, daemon_pid, runtime_ready, Some(telemetry))
    }

    pub fn write_runtime_not_ready(
        &self,
        snapshot: &StateSnapshot,
        daemon_pid: u32,
    ) -> io::Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| io::Error::other("state store lock poisoned"))?;
        self.write_document(snapshot, Some(daemon_pid), false, None)
    }

    pub fn write_runtime_ready(&self, snapshot: &StateSnapshot, daemon_pid: u32) -> io::Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| io::Error::other("state store lock poisoned"))?;
        let telemetry = fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedStatus>(&bytes).ok())
            .and_then(|status| status.telemetry);
        self.write_document(snapshot, Some(daemon_pid), true, telemetry)
    }

    fn write_document(
        &self,
        snapshot: &StateSnapshot,
        daemon_pid: Option<u32>,
        runtime_ready: bool,
        telemetry: Option<RuntimeTelemetry>,
    ) -> io::Result<()> {
        let document = PersistedStatus {
            lifecycle: snapshot.clone(),
            daemon_pid,
            // The writer owns this process identity already. Probing here
            // would launch a Windows liveness subprocess on every heartbeat
            // and telemetry tick; external reads perform the bounded probe.
            daemon_running: daemon_pid.is_some(),
            runtime_ready: daemon_pid.is_some() && runtime_ready,
            updated_unix_ms: unix_millis(),
            telemetry,
        };
        let temporary = self.path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&document).map_err(io::Error::other)?,
        )?;
        replace_file(&temporary, &self.path)
    }

    pub fn read(&self) -> io::Result<PersistedStatus> {
        let bytes = fs::read(&self.path)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    pub fn read_or_stopped(&self) -> PersistedStatus {
        self.read().unwrap_or_else(|_| PersistedStatus::stopped())
    }
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedStatus {
    pub lifecycle: StateSnapshot,
    pub daemon_pid: Option<u32>,
    pub daemon_running: bool,
    #[serde(default)]
    pub runtime_ready: bool,
    pub updated_unix_ms: u128,
    #[serde(default)]
    pub telemetry: Option<RuntimeTelemetry>,
}

impl PersistedStatus {
    pub fn stopped() -> Self {
        Self {
            lifecycle: StateSnapshot {
                state: HostState::Stopped,
                session_id: None,
                missed_heartbeats: 0,
                reason: None,
            },
            daemon_pid: None,
            daemon_running: false,
            runtime_ready: false,
            updated_unix_ms: unix_millis(),
            telemetry: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeTelemetry {
    pub control: crate::control::ControlMetricsSnapshot,
    #[serde(default)]
    pub android: Option<AndroidMetricsV1>,
    pub relay: crate::socks::SocksStatsSnapshot,
    #[serde(default)]
    pub adb: AdbMonitorSnapshot,
    pub process: crate::diagnostics::ProcessSample,
}

#[derive(Clone, Copy, Debug, Default)]
struct RuntimePersistenceMetrics {
    diagnostics: DiagnosticWriteMetrics,
    status_write_us: u64,
    failed: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdbMonitorSnapshot {
    pub active: bool,
    pub repair_suppressed: bool,
    pub device_available: bool,
    pub mappings_healthy: bool,
    pub reconnect_generation: u64,
    #[serde(default)]
    pub mapping_probe_count: u64,
    #[serde(default)]
    pub mapping_probe_last_us: u64,
    #[serde(default)]
    pub mapping_probe_max_us: u64,
    #[serde(default)]
    pub transport_recovery: TransportRecoverySnapshot,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportRecoveryState {
    #[default]
    Idle,
    Pending,
    WaitingForDevice,
    RebuildingMappings,
    AwaitingControl,
    RestartingVirtualDesktop,
    Recovered,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportRecoveryTrigger {
    HostResume,
    UsbReconnect,
}

fn should_reset_virtual_desktop_service(
    trigger: TransportRecoveryTrigger,
    _relaunch_attempt: u32,
) -> bool {
    matches!(
        trigger,
        TransportRecoveryTrigger::HostResume | TransportRecoveryTrigger::UsbReconnect
    )
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransportRecoverySnapshot {
    pub generation: u64,
    pub attempt: u32,
    pub state: TransportRecoveryState,
    pub trigger: Option<TransportRecoveryTrigger>,
}

#[derive(Clone, Default)]
pub struct AdbHealthMonitor {
    stopping: Arc<AtomicBool>,
    authenticated_reconcile: Arc<AtomicBool>,
    reconnect_generation: Arc<AtomicU64>,
    mapping_probe_count: Arc<AtomicU64>,
    mapping_probe_last_us: Arc<AtomicU64>,
    mapping_probe_max_us: Arc<AtomicU64>,
    transport_recovery: Arc<StdMutex<TransportRecoverySnapshot>>,
    transport_auto_rearm_generation: Arc<AtomicU64>,
    transport_rearm_scheduled: Arc<StdMutex<HashSet<u64>>>,
    usb_reconnect_pending: Arc<AtomicBool>,
    status: Arc<StdMutex<AdbMonitorSnapshot>>,
    operation: Arc<Mutex<()>>,
    changed: Arc<Notify>,
}

#[derive(Clone)]
struct AdbMonitorConfig {
    adb: AdbController,
    adb_program: PathBuf,
    session_id: SessionId,
    all_traffic: bool,
    virtual_desktop_flows: VirtualDesktopFlowMonitor,
}

#[derive(Clone, Copy)]
struct RelayEligibility {
    carrier_healthy: bool,
    control_authenticated: bool,
    control_loss_pending: bool,
    control_epoch: u64,
}

#[derive(Clone)]
struct RelayGateController {
    gate: RelayGate,
    eligibility: Arc<StdMutex<RelayEligibility>>,
    control_loss_grace: Duration,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
struct VirtualDesktopWatchdogState {
    armed: bool,
    connected_since: Option<time::Instant>,
    disconnected_since: Option<time::Instant>,
}

#[cfg(any(target_os = "windows", test))]
impl VirtualDesktopWatchdogState {
    fn observe(&mut self, now: time::Instant, connected: bool, recovery_allowed: bool) -> bool {
        if connected {
            self.disconnected_since = None;
            let connected_since = self.connected_since.get_or_insert(now);
            if now.duration_since(*connected_since) >= VIRTUAL_DESKTOP_CONNECTION_STABLE {
                self.armed = true;
            }
            return false;
        }

        self.connected_since = None;
        if !self.armed {
            self.disconnected_since = None;
            return false;
        }
        let disconnected_since = self.disconnected_since.get_or_insert(now);
        if recovery_allowed
            && now.duration_since(*disconnected_since) >= VIRTUAL_DESKTOP_WATCHDOG_LOSS_GRACE
        {
            self.disconnected_since = Some(now);
            return true;
        }
        false
    }
}

impl RelayGateController {
    fn new(gate: RelayGate) -> Self {
        Self::with_control_loss_grace(gate, Duration::from_secs(3))
    }

    fn with_control_loss_grace(gate: RelayGate, control_loss_grace: Duration) -> Self {
        Self {
            gate,
            eligibility: Arc::new(StdMutex::new(RelayEligibility {
                carrier_healthy: true,
                control_authenticated: false,
                control_loss_pending: false,
                control_epoch: 0,
            })),
            control_loss_grace,
        }
    }

    fn control_connected(&self) {
        if let Ok(mut eligibility) = self.eligibility.lock() {
            eligibility.control_epoch = eligibility.control_epoch.wrapping_add(1);
            eligibility.control_loss_pending = false;
            eligibility.control_authenticated = true;
            self.gate.set_enabled(eligibility.carrier_healthy);
        }
    }

    fn authenticated_wake(&self) {
        if let Ok(mut eligibility) = self.eligibility.lock() {
            eligibility.control_epoch = eligibility.control_epoch.wrapping_add(1);
            eligibility.control_loss_pending = false;
            eligibility.control_authenticated = false;
            self.gate.set_enabled(false);
        }
    }

    fn control_degraded(&self) {
        let epoch = if let Ok(mut eligibility) = self.eligibility.lock() {
            if !eligibility.control_authenticated || eligibility.control_loss_pending {
                return;
            }
            eligibility.control_epoch = eligibility.control_epoch.wrapping_add(1);
            eligibility.control_loss_pending = true;
            eligibility.control_epoch
        } else {
            return;
        };
        let controller = self.clone();
        tokio::spawn(async move {
            time::sleep(controller.control_loss_grace).await;
            controller.expire_control_loss(epoch);
        });
    }

    fn expire_control_loss(&self, epoch: u64) {
        let _ = self.control_inactive_if_epoch(epoch);
    }

    fn pending_control_epoch(&self) -> Option<u64> {
        self.eligibility.lock().ok().and_then(|eligibility| {
            (eligibility.control_authenticated && eligibility.control_loss_pending)
                .then_some(eligibility.control_epoch)
        })
    }

    fn control_inactive_if_epoch(&self, epoch: u64) -> bool {
        if let Ok(mut eligibility) = self.eligibility.lock() {
            if eligibility.control_epoch != epoch || !eligibility.control_authenticated {
                return false;
            }
            eligibility.control_epoch = eligibility.control_epoch.wrapping_add(1);
            eligibility.control_loss_pending = false;
            eligibility.control_authenticated = false;
            self.gate.set_enabled(false);
            return true;
        }
        false
    }

    fn control_inactive(&self) {
        if let Ok(mut eligibility) = self.eligibility.lock() {
            eligibility.control_epoch = eligibility.control_epoch.wrapping_add(1);
            eligibility.control_loss_pending = false;
            eligibility.control_authenticated = false;
            self.gate.set_enabled(false);
        }
    }

    fn carrier_lost(&self) {
        if let Ok(mut eligibility) = self.eligibility.lock() {
            eligibility.control_epoch = eligibility.control_epoch.wrapping_add(1);
            eligibility.control_loss_pending = false;
            eligibility.carrier_healthy = false;
            eligibility.control_authenticated = false;
            self.gate.set_enabled(false);
        }
    }

    fn carrier_healthy(&self) {
        if let Ok(mut eligibility) = self.eligibility.lock() {
            eligibility.carrier_healthy = true;
            self.gate.set_enabled(eligibility.control_authenticated);
        }
    }
}

impl AdbHealthMonitor {
    pub fn snapshot(&self) -> AdbMonitorSnapshot {
        let mut snapshot = self
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default();
        snapshot.repair_suppressed = self.stopping.load(Ordering::Acquire);
        snapshot.reconnect_generation = self.reconnect_generation.load(Ordering::Relaxed);
        snapshot.mapping_probe_count = self.mapping_probe_count.load(Ordering::Relaxed);
        snapshot.mapping_probe_last_us = self.mapping_probe_last_us.load(Ordering::Relaxed);
        snapshot.mapping_probe_max_us = self.mapping_probe_max_us.load(Ordering::Relaxed);
        snapshot.transport_recovery = self.transport_recovery_snapshot();
        snapshot
    }

    fn transport_recovery_snapshot(&self) -> TransportRecoverySnapshot {
        self.transport_recovery
            .lock()
            .map(|state| *state)
            .unwrap_or_default()
    }

    fn sync_transport_recovery_status(&self, last_error: Option<String>) {
        let recovery = self.transport_recovery_snapshot();
        if let Ok(mut status) = self.status.lock() {
            status.transport_recovery = recovery;
            status.last_error = last_error;
            if matches!(
                recovery.state,
                TransportRecoveryState::Pending
                    | TransportRecoveryState::WaitingForDevice
                    | TransportRecoveryState::RebuildingMappings
                    | TransportRecoveryState::Failed
            ) {
                status.mappings_healthy = false;
            }
        }
    }

    pub fn notify_host_resume(&self) -> u64 {
        self.usb_reconnect_pending.store(false, Ordering::Release);
        self.notify_transport_recovery(TransportRecoveryTrigger::HostResume)
    }

    fn notify_transport_recovery(&self, trigger: TransportRecoveryTrigger) -> u64 {
        self.transport_auto_rearm_generation
            .store(0, Ordering::Release);
        let generation = if let Ok(mut recovery) = self.transport_recovery.lock() {
            recovery.generation = recovery.generation.wrapping_add(1);
            recovery.attempt = 0;
            recovery.state = TransportRecoveryState::Pending;
            recovery.trigger = Some(trigger);
            recovery.generation
        } else {
            0
        };
        self.sync_transport_recovery_status(Some("transport_recovery_pending".into()));
        self.changed.notify_one();
        generation
    }

    fn rearm_failed_transport(
        &self,
        generation: u64,
        trigger: TransportRecoveryTrigger,
    ) -> Option<u64> {
        if self.stopping.load(Ordering::Acquire)
            || self.transport_auto_rearm_generation.load(Ordering::Acquire) == generation
        {
            return None;
        }
        let next_generation = if let Ok(mut recovery) = self.transport_recovery.lock() {
            if recovery.generation != generation || recovery.state != TransportRecoveryState::Failed
            {
                return None;
            }
            recovery.generation = recovery.generation.wrapping_add(1);
            recovery.attempt = 0;
            recovery.state = TransportRecoveryState::Pending;
            recovery.trigger = Some(trigger);
            recovery.generation
        } else {
            return None;
        };
        self.transport_auto_rearm_generation
            .store(next_generation, Ordering::Release);
        self.sync_transport_recovery_status(Some("transport_recovery_rearmed".into()));
        self.changed.notify_one();
        Some(next_generation)
    }

    fn schedule_transport_recovery_rearm(
        &self,
        generation: u64,
        trigger: TransportRecoveryTrigger,
        diagnostics: &Diagnostics,
    ) {
        self.schedule_transport_recovery_rearm_after(
            generation,
            trigger,
            diagnostics,
            TRANSPORT_RECOVERY_REARM_DELAY,
        );
    }

    fn schedule_transport_recovery_rearm_after(
        &self,
        generation: u64,
        trigger: TransportRecoveryTrigger,
        diagnostics: &Diagnostics,
        delay: Duration,
    ) {
        if self.transport_auto_rearm_generation.load(Ordering::Acquire) == generation {
            return;
        }
        let scheduled = self
            .transport_rearm_scheduled
            .lock()
            .is_ok_and(|mut scheduled| scheduled.insert(generation));
        if !scheduled {
            return;
        }
        let _ = diagnostics.record(
            "transport_recovery_rearm_scheduled",
            json!({
                "generation": generation,
                "trigger": trigger,
                "delay_ms": delay.as_millis().min(u64::MAX as u128) as u64,
            }),
        );
        let monitor = self.clone();
        let diagnostics = diagnostics.clone();
        tokio::spawn(async move {
            time::sleep(delay).await;
            if let Ok(mut scheduled) = monitor.transport_rearm_scheduled.lock() {
                scheduled.remove(&generation);
            }
            if let Some(next_generation) = monitor.rearm_failed_transport(generation, trigger) {
                let _ = diagnostics.record(
                    "transport_recovery_rearmed",
                    json!({
                        "previous_generation": generation,
                        "generation": next_generation,
                        "trigger": trigger,
                    }),
                );
            }
        });
    }

    fn transport_requires_recovery(&self) -> bool {
        matches!(
            self.transport_recovery_snapshot().state,
            TransportRecoveryState::Pending
                | TransportRecoveryState::WaitingForDevice
                | TransportRecoveryState::RebuildingMappings
                | TransportRecoveryState::Failed
        )
    }

    fn wait_for_recovery_device(&self) {
        if let Ok(mut recovery) = self.transport_recovery.lock() {
            recovery.attempt = 0;
            recovery.state = TransportRecoveryState::WaitingForDevice;
        }
        self.sync_transport_recovery_status(Some("transport_recovery_waiting_for_device".into()));
    }

    fn begin_transport_recovery_attempt(&self) -> Option<(u64, u32, TransportRecoveryTrigger)> {
        let attempt = if let Ok(mut recovery) = self.transport_recovery.lock() {
            if !matches!(
                recovery.state,
                TransportRecoveryState::Pending | TransportRecoveryState::WaitingForDevice
            ) || recovery.attempt >= TRANSPORT_RECOVERY_MAX_ATTEMPTS
            {
                return None;
            }
            recovery.attempt += 1;
            recovery.state = TransportRecoveryState::RebuildingMappings;
            (recovery.generation, recovery.attempt, recovery.trigger?)
        } else {
            return None;
        };
        self.sync_transport_recovery_status(Some("transport_recovery_rebuilding".into()));
        Some(attempt)
    }

    fn finish_transport_recovery_attempt(&self, generation: u64, succeeded: bool) -> bool {
        let mut retry = false;
        if let Ok(mut recovery) = self.transport_recovery.lock() {
            if recovery.generation != generation {
                return false;
            }
            if succeeded {
                recovery.state = TransportRecoveryState::AwaitingControl;
            } else if recovery.attempt < TRANSPORT_RECOVERY_MAX_ATTEMPTS {
                recovery.state = TransportRecoveryState::Pending;
                retry = true;
            } else {
                recovery.state = TransportRecoveryState::Failed;
            }
        }
        self.sync_transport_recovery_status((!succeeded).then(|| {
            if retry {
                "transport_recovery_retry".into()
            } else {
                "transport_recovery_failed".into()
            }
        }));
        retry
    }

    fn begin_virtual_desktop_restart(&self) -> Option<(u64, u32, TransportRecoveryTrigger)> {
        let recovery = if let Ok(mut recovery) = self.transport_recovery.lock() {
            if recovery.state != TransportRecoveryState::AwaitingControl {
                return None;
            }
            recovery.state = TransportRecoveryState::RestartingVirtualDesktop;
            (recovery.generation, recovery.attempt, recovery.trigger?)
        } else {
            return None;
        };
        self.sync_transport_recovery_status(Some(
            "transport_recovery_restarting_virtual_desktop".into(),
        ));
        Some(recovery)
    }

    fn finish_virtual_desktop_restart(
        &self,
        generation: u64,
        attempt: u32,
        succeeded: bool,
    ) -> Option<bool> {
        let retry = if let Ok(mut recovery) = self.transport_recovery.lock() {
            if recovery.generation != generation
                || recovery.attempt != attempt
                || recovery.state != TransportRecoveryState::RestartingVirtualDesktop
            {
                return None;
            }
            if succeeded {
                recovery.state = TransportRecoveryState::Recovered;
                false
            } else if recovery.attempt < TRANSPORT_RECOVERY_MAX_ATTEMPTS {
                recovery.state = TransportRecoveryState::Pending;
                true
            } else {
                recovery.state = TransportRecoveryState::Failed;
                false
            }
        } else {
            return None;
        };
        self.sync_transport_recovery_status((!succeeded).then(|| {
            if retry {
                "transport_recovery_virtual_desktop_retry".into()
            } else {
                "transport_recovery_virtual_desktop_failed".into()
            }
        }));
        Some(retry)
    }

    fn expire_transport_recovery_control(&self, generation: u64, attempt: u32) -> Option<bool> {
        let retry = if let Ok(mut recovery) = self.transport_recovery.lock() {
            if recovery.generation != generation
                || recovery.attempt != attempt
                || recovery.state != TransportRecoveryState::AwaitingControl
            {
                return None;
            }
            if recovery.attempt < TRANSPORT_RECOVERY_MAX_ATTEMPTS {
                recovery.state = TransportRecoveryState::Pending;
                true
            } else {
                recovery.state = TransportRecoveryState::Failed;
                false
            }
        } else {
            return None;
        };
        self.sync_transport_recovery_status(Some(if retry {
            "transport_recovery_control_retry".into()
        } else {
            "transport_recovery_control_failed".into()
        }));
        if retry {
            self.changed.notify_one();
        }
        Some(retry)
    }

    fn note_usb_disconnected(&self) {
        if self.transport_recovery_snapshot().trigger != Some(TransportRecoveryTrigger::HostResume)
            || !self.transport_requires_recovery()
        {
            self.usb_reconnect_pending.store(true, Ordering::Release);
        }
    }

    fn notify_usb_reconnected_if_needed(&self) -> bool {
        if !self.usb_reconnect_pending.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.notify_transport_recovery(TransportRecoveryTrigger::UsbReconnect);
        true
    }

    fn record_mapping_probe(&self, duration: Duration) {
        let duration_us = duration.as_micros().min(u64::MAX as u128) as u64;
        self.mapping_probe_count.fetch_add(1, Ordering::Relaxed);
        self.mapping_probe_last_us
            .store(duration_us, Ordering::Relaxed);
        self.mapping_probe_max_us
            .fetch_max(duration_us, Ordering::Relaxed);
    }

    /// Convenience shutdown path that suppresses and drains repair work.
    pub async fn suppress_repairs(&self) {
        self.begin_suppress_repairs();
        self.drain_repairs().await;
    }

    /// Nonblocking first phase of explicit Stop. New repair work observes the
    /// flag immediately, allowing the authenticated STOP frame to reach
    /// Android without waiting behind an existing mapping command.
    pub fn begin_suppress_repairs(&self) {
        self.stopping.store(true, Ordering::Release);
        self.changed.notify_waiters();
        if let Ok(mut status) = self.status.lock() {
            status.active = false;
            status.repair_suppressed = true;
        }
    }

    /// Drains the at-most-one bounded mapping command that was already in
    /// flight when Stop began.
    pub async fn drain_repairs(&self) {
        let _operation = self.operation.lock().await;
    }

    pub fn notify_state_change(&self) {
        self.changed.notify_one();
    }

    pub fn notify_authenticated_connection(&self) {
        self.authenticated_reconcile.store(true, Ordering::Release);
        self.changed.notify_one();
    }

    async fn run(
        self,
        config: AdbMonitorConfig,
        control: ControlHandle,
        store: StateStore,
        diagnostics: Diagnostics,
        relay_gate: RelayGateController,
    ) {
        const HEALTHY_INTERVAL: Duration = Duration::from_secs(2);
        const MONITOR_ADB_TIMEOUT: Duration = Duration::from_millis(500);
        const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
        const MAX_BACKOFF: Duration = Duration::from_secs(5);

        let config = AdbMonitorConfig {
            adb: config.adb.with_mapping_timeout(MONITOR_ADB_TIMEOUT),
            ..config
        };
        let adb = &config.adb;
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            let mut child = match spawn_track_devices(&config.adb_program) {
                Ok(child) => child,
                Err(_) => {
                    self.track_failed(adb, &control, &store, &diagnostics, "track_spawn_failed")
                        .await;
                    if sleep_or_stop(&self, backoff).await {
                        return;
                    }
                    backoff = next_backoff(backoff, MAX_BACKOFF);
                    continue;
                }
            };
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill().await;
                self.track_failed(
                    adb,
                    &control,
                    &store,
                    &diagnostics,
                    "track_stdout_unavailable",
                )
                .await;
                if sleep_or_stop(&self, backoff).await {
                    return;
                }
                backoff = next_backoff(backoff, MAX_BACKOFF);
                continue;
            };
            let previous = self.snapshot();
            self.update_status(
                true,
                previous.device_available,
                previous.mappings_healthy,
                None,
            );
            let mut stdout = stdout;
            let mut read_buffer = [0u8; 4096];
            let mut decoder = TrackDevicesDecoder::default();
            let mut device_available = None;
            let mut healthy =
                time::interval_at(time::Instant::now() + HEALTHY_INTERVAL, HEALTHY_INTERVAL);
            healthy.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            let mut tracker_failed = None;
            loop {
                tokio::select! {
                    read = stdout.read(&mut read_buffer) => {
                        match read {
                            Ok(0) => {
                                tracker_failed = Some("track_exited");
                                break;
                            }
                            Ok(length) => {
                                let updates = match decoder.push(&read_buffer[..length]) {
                                    Ok(updates) => updates,
                                    Err(_) => {
                                        tracker_failed = Some("track_decode_failed");
                                        break;
                                    }
                                };
                                for available in updates {
                                    device_available = Some(available);
                                    if available {
                                        self.notify_usb_reconnected_if_needed();
                                    }
                                    let forced = self
                                        .authenticated_reconcile
                                        .swap(false, Ordering::AcqRel);
                                    let completed_recovery = forced
                                        && self
                                            .complete_transport_recovery(&config, &diagnostics)
                                            .await;
                                    if !completed_recovery
                                        && !self.recover_transport(
                                            &config,
                                            &control,
                                            &store,
                                            &diagnostics,
                                            &relay_gate,
                                            available,
                                        ).await
                                    {
                                        self.reconcile(
                                            adb,
                                            &control,
                                            &store,
                                            &diagnostics,
                                            &relay_gate,
                                            available || forced,
                                        ).await;
                                        if forced {
                                            self.retry_authenticated_reconcile_if_needed();
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                tracker_failed = Some("track_read_failed");
                                break;
                            }
                        }
                    }
                    _ = healthy.tick() => {
                        // A track process that remains alive for a complete
                        // health interval is stable enough to reset restart
                        // backoff. Fast crash loops continue toward the cap.
                        // Do not continuously invoke `adb reverse --list`
                        // while all mappings and the authenticated control
                        // lane are healthy: that command shares the active USB
                        // transport with forwarded traffic. Device/control
                        // changes still notify this monitor immediately, and
                        // an unhealthy result keeps retrying on this tick.
                        backoff = INITIAL_BACKOFF;
                        let forced = self
                            .authenticated_reconcile
                            .swap(false, Ordering::AcqRel);
                        let completed_recovery = forced
                            && self
                                .complete_transport_recovery(&config, &diagnostics)
                                .await;
                        if !completed_recovery && self.transport_requires_recovery() {
                            self.recover_transport(
                                &config,
                                &control,
                                &store,
                                &diagnostics,
                                &relay_gate,
                                cached_device_is_available(device_available),
                            ).await;
                        } else if !completed_recovery && should_reconcile_on_healthy_tick(
                            forced,
                            device_available,
                            self.snapshot().mappings_healthy,
                        ) {
                            self.reconcile(
                                adb,
                                &control,
                                &store,
                                &diagnostics,
                                &relay_gate,
                                true,
                            ).await;
                            if forced {
                                self.retry_authenticated_reconcile_if_needed();
                            }
                        }
                    }
                    _ = self.changed.notified() => {
                        if self.stopping.load(Ordering::Acquire) {
                            break;
                        }
                        let forced = self
                            .authenticated_reconcile
                            .swap(false, Ordering::AcqRel);
                        let completed_recovery = forced
                            && self
                                .complete_transport_recovery(&config, &diagnostics)
                                .await;
                        if !completed_recovery && self.transport_requires_recovery() {
                            self.recover_transport(
                                &config,
                                &control,
                                &store,
                                &diagnostics,
                                &relay_gate,
                                cached_device_is_available(device_available),
                            ).await;
                        } else if !completed_recovery
                            && (forced || cached_device_is_available(device_available))
                        {
                            self.reconcile(
                                adb,
                                &control,
                                &store,
                                &diagnostics,
                                &relay_gate,
                                true,
                            ).await;
                            if forced {
                                self.retry_authenticated_reconcile_if_needed();
                            }
                        }
                    }
                }
                if self.stopping.load(Ordering::Acquire) {
                    break;
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            if let Some(category) = tracker_failed {
                self.track_failed(adb, &control, &store, &diagnostics, category)
                    .await;
                if sleep_or_stop(&self, backoff).await {
                    return;
                }
                backoff = next_backoff(backoff, MAX_BACKOFF);
            }
        }
    }

    fn retry_authenticated_reconcile_if_needed(&self) {
        if self.stopping.load(Ordering::Acquire) || self.snapshot().mappings_healthy {
            return;
        }
        self.authenticated_reconcile.store(true, Ordering::Release);
        let changed = self.changed.clone();
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(100)).await;
            changed.notify_one();
        });
    }

    async fn complete_transport_recovery(
        &self,
        config: &AdbMonitorConfig,
        diagnostics: &Diagnostics,
    ) -> bool {
        let Some((generation, attempt, trigger)) = self.begin_virtual_desktop_restart() else {
            return false;
        };
        let _ = diagnostics.record(
            "transport_recovery_virtual_desktop_restart",
            json!({
                "generation": generation,
                "attempt": attempt,
                "trigger": trigger,
            }),
        );

        let _operation = self.operation.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return true;
        }
        let mut failure_category = "virtual_desktop_connection_timeout";
        for relaunch_attempt in 1..=3 {
            if should_reset_virtual_desktop_service(trigger, relaunch_attempt) {
                let quiesce_adb = config.adb.clone();
                let quiesced = task::spawn_blocking(move || {
                    quiesce_adb
                        .stop_virtual_desktop()
                        .map_err(|_| "virtual_desktop_stop")
                })
                .await;
                if self.transport_recovery_snapshot().generation != generation {
                    return true;
                }
                match quiesced {
                    Ok(Ok(())) => {}
                    Ok(Err(category)) => {
                        failure_category = category;
                        continue;
                    }
                    Err(_) => {
                        failure_category = "virtual_desktop_stop_task";
                        continue;
                    }
                }
                let service_recovery = task::spawn_blocking(restart_virtual_desktop_service).await;
                if self.transport_recovery_snapshot().generation != generation {
                    return true;
                }
                let service_recovery = match service_recovery {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err("virtual_desktop_service_recovery"),
                    Err(_) => Err("virtual_desktop_service_recovery_task"),
                };
                let _ = diagnostics.record(
                    "transport_recovery_virtual_desktop_service",
                    json!({
                        "generation": generation,
                        "attempt": attempt,
                        "relaunch_attempt": relaunch_attempt,
                        "trigger": trigger,
                        "success": service_recovery.is_ok(),
                    }),
                );
                if let Err(category) = service_recovery {
                    failure_category = category;
                    continue;
                }
            }

            let recovery_adb = config.adb.clone();
            let restart = task::spawn_blocking(move || {
                recovery_adb
                    .restart_virtual_desktop()
                    .map_err(|_| "virtual_desktop_restart")
            })
            .await;
            if self.transport_recovery_snapshot().generation != generation {
                return true;
            }
            let result = match restart {
                Ok(Ok(())) => {
                    wait_for_virtual_desktop_connection(
                        &config.virtual_desktop_flows,
                        self,
                        generation,
                        VIRTUAL_DESKTOP_CONNECTION_TIMEOUT,
                    )
                    .await
                }
                Ok(Err(category)) => Err(category),
                Err(_) => Err("recovery_task"),
            };
            if result.is_ok() {
                self.finish_virtual_desktop_restart(generation, attempt, true);
                let _ = diagnostics.record(
                    "transport_recovery_completed",
                    json!({
                        "generation": generation,
                        "attempt": attempt,
                        "relaunch_attempt": relaunch_attempt,
                        "trigger": trigger,
                    }),
                );
                return true;
            }
            failure_category = result.unwrap_err();
            let _ = diagnostics.record(
                "transport_recovery_relaunch_failed",
                json!({
                    "generation": generation,
                    "attempt": attempt,
                    "relaunch_attempt": relaunch_attempt,
                    "trigger": trigger,
                    "category": failure_category,
                }),
            );
        }

        let retry = self
            .finish_virtual_desktop_restart(generation, attempt, false)
            .unwrap_or(false);
        let _ = diagnostics.record(
            "transport_recovery_failed",
            json!({
                "generation": generation,
                "attempt": attempt,
                "trigger": trigger,
                "will_retry": retry,
                "category": failure_category,
            }),
        );
        if retry {
            let changed = self.changed.clone();
            tokio::spawn(async move {
                time::sleep(TRANSPORT_RECOVERY_RETRY_DELAY).await;
                changed.notify_one();
            });
        } else {
            self.schedule_transport_recovery_rearm(generation, trigger, diagnostics);
        }
        true
    }

    async fn recover_transport(
        &self,
        config: &AdbMonitorConfig,
        control: &ControlHandle,
        store: &StateStore,
        diagnostics: &Diagnostics,
        relay_gate: &RelayGateController,
        device_available: bool,
    ) -> bool {
        if !self.transport_requires_recovery() {
            return false;
        }
        relay_gate.carrier_lost();
        if self.stopping.load(Ordering::Acquire) {
            return true;
        }
        if !device_available {
            self.wait_for_recovery_device();
            self.update_status(
                true,
                false,
                false,
                Some("transport_recovery_waiting_for_device".into()),
            );
            return true;
        }
        let Some((recovery_generation, attempt, trigger)) = self.begin_transport_recovery_attempt()
        else {
            return true;
        };
        self.update_status(
            true,
            true,
            false,
            Some("transport_recovery_rebuilding".into()),
        );
        let _ = diagnostics.record(
            "transport_recovery_attempt",
            json!({
                "generation": recovery_generation,
                "attempt": attempt,
                "trigger": trigger,
            }),
        );

        let _operation = self.operation.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return true;
        }
        let recovery_adb = config.adb.clone();
        let session_id = config.session_id;
        let all_traffic = config.all_traffic;
        let restart_android = trigger == TransportRecoveryTrigger::UsbReconnect && attempt > 1;
        let strategy = if restart_android {
            "android_restart"
        } else {
            "mapping_reinstall"
        };
        let result = task::spawn_blocking(move || -> Result<(), &'static str> {
            if restart_android {
                recovery_adb
                    .start(session_id, all_traffic)
                    .map(|_| ())
                    .map_err(|_| "android_restart")?;
            } else {
                recovery_adb
                    .reinstall_mappings()
                    .map_err(|_| "mapping_rebuild")?;
            }
            Ok(())
        })
        .await;
        if self.transport_recovery_snapshot().generation != recovery_generation {
            return true;
        }
        match result {
            Ok(Ok(())) => {
                // A control connection can race mapping recreation. Cancel it
                // after the fresh listeners are installed so only a HELLO from
                // this recovery generation can reopen the relay.
                relay_gate.control_inactive();
                let snapshot = control.reset_transport(TRANSPORT_RECOVERY_REASON).await;
                let _ = store.write(&snapshot, Some(std::process::id()));
                let reconnect_generation = self
                    .reconnect_generation
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                self.finish_transport_recovery_attempt(recovery_generation, true);
                self.update_status(true, true, true, None);
                relay_gate.carrier_healthy();
                let _ = diagnostics.record(
                    "transport_recovery_ready",
                    json!({
                        "generation": recovery_generation,
                        "attempt": attempt,
                        "trigger": trigger,
                        "strategy": strategy,
                        "reconnect_generation": reconnect_generation,
                        "waiting_for": "authenticated_control",
                    }),
                );
                let monitor = self.clone();
                let diagnostics = diagnostics.clone();
                tokio::spawn(async move {
                    time::sleep(TRANSPORT_RECOVERY_CONTROL_TIMEOUT).await;
                    if let Some(will_retry) =
                        monitor.expire_transport_recovery_control(recovery_generation, attempt)
                    {
                        let _ = diagnostics.record(
                            "transport_recovery_control_timeout",
                            json!({
                                "generation": recovery_generation,
                                "attempt": attempt,
                                "trigger": trigger,
                                "will_retry": will_retry,
                            }),
                        );
                        if !will_retry {
                            monitor.schedule_transport_recovery_rearm(
                                recovery_generation,
                                trigger,
                                &diagnostics,
                            );
                        }
                    }
                });
            }
            Ok(Err(failure_category)) => {
                let retry = self.finish_transport_recovery_attempt(recovery_generation, false);
                self.update_status(
                    true,
                    true,
                    false,
                    Some(if retry {
                        "transport_recovery_retry".into()
                    } else {
                        "transport_recovery_failed".into()
                    }),
                );
                let _ = diagnostics.record(
                    "transport_recovery_failed",
                    json!({
                        "generation": recovery_generation,
                        "attempt": attempt,
                        "trigger": trigger,
                        "will_retry": retry,
                        "category": failure_category,
                    }),
                );
                if retry {
                    let changed = self.changed.clone();
                    tokio::spawn(async move {
                        time::sleep(TRANSPORT_RECOVERY_RETRY_DELAY).await;
                        changed.notify_one();
                    });
                } else {
                    self.schedule_transport_recovery_rearm(
                        recovery_generation,
                        trigger,
                        diagnostics,
                    );
                }
            }
            Err(_) => {
                let retry = self.finish_transport_recovery_attempt(recovery_generation, false);
                self.update_status(
                    true,
                    true,
                    false,
                    Some(if retry {
                        "transport_recovery_retry".into()
                    } else {
                        "transport_recovery_failed".into()
                    }),
                );
                let _ = diagnostics.record(
                    "transport_recovery_failed",
                    json!({
                        "generation": recovery_generation,
                        "attempt": attempt,
                        "trigger": trigger,
                        "will_retry": retry,
                        "category": "recovery_task",
                    }),
                );
                if retry {
                    let changed = self.changed.clone();
                    tokio::spawn(async move {
                        time::sleep(TRANSPORT_RECOVERY_RETRY_DELAY).await;
                        changed.notify_one();
                    });
                } else {
                    self.schedule_transport_recovery_rearm(
                        recovery_generation,
                        trigger,
                        diagnostics,
                    );
                }
            }
        }
        true
    }

    async fn reconcile(
        &self,
        adb: &AdbController,
        control: &ControlHandle,
        store: &StateStore,
        diagnostics: &Diagnostics,
        relay_gate: &RelayGateController,
        device_available: bool,
    ) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let lifecycle = control.snapshot().await;
        let lifecycle_active =
            matches!(lifecycle.state, HostState::Connected | HostState::Degraded);
        if !device_available {
            if lifecycle_active {
                self.note_usb_disconnected();
            }
            relay_gate.carrier_lost();
            if lifecycle_active {
                self.record_loss(control, store, diagnostics, false, "device_unavailable")
                    .await;
            } else {
                self.update_status(true, false, false, Some("device_unavailable".into()));
            }
            return;
        }
        if !lifecycle_active {
            self.update_status(true, true, false, None);
            return;
        }

        let _operation = self.operation.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let probe_adb = adb.clone();
        let probe_started = Instant::now();
        let health = task::spawn_blocking(move || probe_adb.mapping_health()).await;
        self.record_mapping_probe(probe_started.elapsed());
        match health {
            Ok(Ok(health)) if health.is_healthy() => {
                self.update_status(true, true, true, None);
                relay_gate.carrier_healthy();
                if lifecycle.state == HostState::Degraded {
                    self.reconcile_android_sleep(adb, diagnostics, relay_gate)
                        .await;
                }
            }
            Ok(Ok(health)) => {
                let missing = health.missing;
                let mut repair_failed = false;
                for mapping in missing {
                    // Explicit Stop sets this flag before waiting on the
                    // operation mutex. Check between bounded mapping commands
                    // so Stop waits for at most one ADB timeout, not the whole
                    // three-lane repair transaction.
                    if self.stopping.load(Ordering::Acquire) {
                        return;
                    }
                    let repair_adb = adb.clone();
                    match task::spawn_blocking(move || repair_adb.add_mapping(mapping)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {
                            repair_failed = true;
                            break;
                        }
                        Err(_) => {
                            repair_failed = true;
                            break;
                        }
                    }
                }
                if self.stopping.load(Ordering::Acquire) {
                    return;
                }
                if !repair_failed {
                    let verify_adb = adb.clone();
                    repair_failed = !matches!(
                        task::spawn_blocking(move || verify_adb.mapping_health()).await,
                        Ok(Ok(health)) if health.is_healthy()
                    );
                }
                if repair_failed {
                    relay_gate.carrier_lost();
                    self.record_loss(control, store, diagnostics, true, "mapping_repair_failed")
                        .await;
                } else {
                    let generation = self
                        .reconnect_generation
                        .fetch_add(1, Ordering::Relaxed)
                        .saturating_add(1);
                    self.update_status(true, true, true, None);
                    relay_gate.carrier_healthy();
                    let _ = diagnostics.record(
                        "adb_mapping_repaired",
                        json!({"reconnect_generation": generation}),
                    );
                }
            }
            Ok(Err(_)) => {
                self.record_loss(control, store, diagnostics, false, "mapping_probe_failed")
                    .await;
            }
            Err(_) => {
                self.record_loss(
                    control,
                    store,
                    diagnostics,
                    false,
                    "mapping_probe_worker_failed",
                )
                .await;
            }
        }
    }

    async fn reconcile_android_sleep(
        &self,
        adb: &AdbController,
        diagnostics: &Diagnostics,
        relay_gate: &RelayGateController,
    ) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let Some(control_epoch) = relay_gate.pending_control_epoch() else {
            return;
        };
        let status_adb = adb.clone();
        let status = task::spawn_blocking(move || status_adb.android_status_quick()).await;
        if matches!(
            status,
            Ok(Ok(status))
                if status.screen_suspended == Some(true)
                    && status.state.as_deref() == Some("degraded")
        ) && relay_gate.control_inactive_if_epoch(control_epoch)
        {
            let _ = diagnostics.record("android_sleep_confirmed", json!({}));
        }
    }

    async fn track_failed(
        &self,
        adb: &AdbController,
        control: &ControlHandle,
        store: &StateStore,
        diagnostics: &Diagnostics,
        category: &'static str,
    ) {
        if category == "track_decode_failed" {
            let previous = self.snapshot();
            let probe_adb = adb.clone();
            let device_confirmed = matches!(
                task::spawn_blocking(move || probe_adb.device_state()).await,
                Ok(Ok(state)) if state == "device"
            );
            if device_confirmed {
                self.update_status(
                    false,
                    true,
                    previous.device_available && previous.mappings_healthy,
                    Some(category.into()),
                );
                let _ = diagnostics.record(
                    "adb_monitor_failure",
                    json!({"category": category, "device_confirmed": true}),
                );
                return;
            }
        }
        self.update_status(false, false, false, Some(category.into()));
        if matches!(
            control.snapshot().await.state,
            HostState::Connected | HostState::Degraded
        ) {
            self.note_usb_disconnected();
            self.record_loss(control, store, diagnostics, false, category)
                .await;
        }
        // `record_loss` describes a live monitor observing carrier loss. A
        // failed track process is different: clear that active bit throughout
        // restart backoff so status never presents stale monitor health.
        self.update_status(false, false, false, Some(category.into()));
        let _ = diagnostics.record(
            "adb_monitor_failure",
            json!({"category": category, "device_confirmed": false}),
        );
    }

    fn update_status(
        &self,
        active: bool,
        device_available: bool,
        mappings_healthy: bool,
        last_error: Option<String>,
    ) -> bool {
        if let Ok(mut status) = self.status.lock() {
            let next = AdbMonitorSnapshot {
                active,
                repair_suppressed: self.stopping.load(Ordering::Acquire),
                device_available,
                mappings_healthy,
                reconnect_generation: self.reconnect_generation.load(Ordering::Relaxed),
                mapping_probe_count: self.mapping_probe_count.load(Ordering::Relaxed),
                mapping_probe_last_us: self.mapping_probe_last_us.load(Ordering::Relaxed),
                mapping_probe_max_us: self.mapping_probe_max_us.load(Ordering::Relaxed),
                transport_recovery: self.transport_recovery_snapshot(),
                last_error,
            };
            let changed = *status != next;
            *status = next;
            changed
        } else {
            false
        }
    }

    async fn record_loss(
        &self,
        control: &ControlHandle,
        store: &StateStore,
        diagnostics: &Diagnostics,
        device_available: bool,
        category: &'static str,
    ) {
        if !self.update_status(true, device_available, false, Some(category.into())) {
            return;
        }
        let snapshot = control
            .transport_lost(format!("ADB carrier unavailable ({category})"))
            .await;
        let _ = store.write(&snapshot, Some(std::process::id()));
        let _ = diagnostics.record(
            "adb_health_degraded",
            json!({
                "device_available": device_available,
                "mapping_state": "unhealthy",
                "category": if device_available { "mapping" } else { "device" },
            }),
        );
    }
}

const MAX_TRACK_DEVICES_PAYLOAD: usize = 16 * 1024;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum TrackDevicesMode {
    #[default]
    Unknown,
    Framed,
    Text,
}

#[derive(Default)]
struct TrackDevicesDecoder {
    mode: TrackDevicesMode,
    buffer: Vec<u8>,
    text: TrackDevicesTextParser,
    last_update: Option<bool>,
}

impl TrackDevicesDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<bool>, TrackDevicesDecodeError> {
        self.buffer.extend_from_slice(bytes);
        let mut updates = Vec::new();
        loop {
            if self.mode == TrackDevicesMode::Unknown {
                if self.buffer.len() < 4 {
                    return Ok(updates);
                }
                self.mode = if self.buffer[..4].iter().all(u8::is_ascii_hexdigit) {
                    let length = parse_track_length(&self.buffer[..4])?;
                    if length > MAX_TRACK_DEVICES_PAYLOAD {
                        return Err(TrackDevicesDecodeError::Oversized(length));
                    }
                    TrackDevicesMode::Framed
                } else {
                    TrackDevicesMode::Text
                };
            }

            match self.mode {
                TrackDevicesMode::Unknown => unreachable!(),
                TrackDevicesMode::Framed => {
                    if self.buffer.len() < 4 {
                        return Ok(updates);
                    }
                    let length = parse_track_length(&self.buffer[..4])?;
                    if length > MAX_TRACK_DEVICES_PAYLOAD {
                        return Err(TrackDevicesDecodeError::Oversized(length));
                    }
                    if self.buffer.len() < 4 + length {
                        return Ok(updates);
                    }
                    let available = parse_track_snapshot(&self.buffer[4..4 + length])?;
                    self.buffer.drain(..4 + length);
                    if self.last_update != Some(available) {
                        self.last_update = Some(available);
                        updates.push(available);
                    }
                }
                TrackDevicesMode::Text => {
                    while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                        let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
                        line.pop();
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        let line = std::str::from_utf8(&line)
                            .map_err(|_| TrackDevicesDecodeError::InvalidUtf8)?;
                        if let Some(available) = self.text.push_line(line) {
                            if self.last_update != Some(available) {
                                self.last_update = Some(available);
                                updates.push(available);
                            }
                        }
                    }
                    if self.buffer.len() > MAX_TRACK_DEVICES_PAYLOAD {
                        return Err(TrackDevicesDecodeError::Oversized(self.buffer.len()));
                    }
                    return Ok(updates);
                }
            }
        }
    }
}

fn parse_track_length(bytes: &[u8]) -> Result<usize, TrackDevicesDecodeError> {
    if bytes.len() != 4 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(TrackDevicesDecodeError::InvalidLength);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| TrackDevicesDecodeError::InvalidLength)?;
    usize::from_str_radix(text, 16).map_err(|_| TrackDevicesDecodeError::InvalidLength)
}

fn parse_track_snapshot(payload: &[u8]) -> Result<bool, TrackDevicesDecodeError> {
    let text = std::str::from_utf8(payload).map_err(|_| TrackDevicesDecodeError::InvalidUtf8)?;
    Ok(text.lines().any(|line| {
        line.split_once('\t')
            .is_some_and(|(_, state)| state.split_whitespace().next() == Some("device"))
    }))
}

#[derive(Debug, Error, Eq, PartialEq)]
enum TrackDevicesDecodeError {
    #[error("ADB track-devices frame has an invalid length")]
    InvalidLength,
    #[error("ADB track-devices frame is {0} bytes, exceeding the bound")]
    Oversized(usize),
    #[error("ADB track-devices output is not UTF-8")]
    InvalidUtf8,
}

#[derive(Default)]
struct TrackDevicesTextParser {
    in_snapshot: bool,
    device_available: bool,
}

impl TrackDevicesTextParser {
    fn push_line(&mut self, line: &str) -> Option<bool> {
        let line = line.trim_end_matches('\r');
        if line == "List of devices attached" {
            let completed = self.in_snapshot.then_some(self.device_available);
            self.in_snapshot = true;
            self.device_available = false;
            return completed;
        }
        if line.is_empty() {
            let available = self.in_snapshot && self.device_available;
            self.in_snapshot = false;
            self.device_available = false;
            return Some(available);
        }
        // Current ADB builds normally print the human-readable header, while
        // some platform-tools versions stream only the tab-separated snapshot.
        // Treat the first valid device-state line as an implicit snapshot start.
        if !self.in_snapshot && line.contains('\t') {
            self.in_snapshot = true;
            self.device_available = false;
        }
        if !self.in_snapshot {
            return None;
        }
        let available = line
            .split_once('\t')
            .is_some_and(|(_, state)| state.split_whitespace().next() == Some("device"));
        self.device_available |= available;
        available.then_some(true)
    }
}

fn spawn_track_devices(adb_program: &Path) -> io::Result<TokioChild> {
    let mut command = TokioCommand::new(adb_program);
    command
        .arg("track-devices")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()
}

#[cfg(target_os = "windows")]
async fn wait_for_virtual_desktop_connection(
    flows: &VirtualDesktopFlowMonitor,
    monitor: &AdbHealthMonitor,
    recovery_generation: u64,
    timeout: Duration,
) -> Result<(), &'static str> {
    let deadline = time::Instant::now() + timeout;
    let mut stable_since = None;
    loop {
        if monitor.stopping.load(Ordering::Acquire)
            || monitor.transport_recovery_snapshot().generation != recovery_generation
        {
            return Err("recovery_superseded");
        }
        if flows.snapshot().connected {
            let connected_since = stable_since.get_or_insert_with(time::Instant::now);
            if connected_since.elapsed() >= VIRTUAL_DESKTOP_CONNECTION_STABLE {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if time::Instant::now() >= deadline {
            return Err("virtual_desktop_connection_timeout");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(target_os = "windows")]
fn virtual_desktop_watchdog_recovery_allowed(monitor: &AdbHealthMonitor) -> bool {
    !monitor.stopping.load(Ordering::Acquire)
        && matches!(
            monitor.transport_recovery_snapshot().state,
            TransportRecoveryState::Idle | TransportRecoveryState::Recovered
        )
}

#[cfg(target_os = "windows")]
async fn wait_for_virtual_desktop_watchdog_connection(
    flows: &VirtualDesktopFlowMonitor,
    monitor: &AdbHealthMonitor,
) -> Result<(), &'static str> {
    let deadline = time::Instant::now() + VIRTUAL_DESKTOP_CONNECTION_TIMEOUT;
    let mut stable_since = None;
    loop {
        if !virtual_desktop_watchdog_recovery_allowed(monitor) {
            return Err("recovery_superseded");
        }
        if flows.snapshot().connected {
            let connected_since = stable_since.get_or_insert_with(time::Instant::now);
            if connected_since.elapsed() >= VIRTUAL_DESKTOP_CONNECTION_STABLE {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if time::Instant::now() >= deadline {
            return Err("virtual_desktop_connection_timeout");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(target_os = "windows")]
async fn run_virtual_desktop_watchdog(
    flows: VirtualDesktopFlowMonitor,
    monitor: AdbHealthMonitor,
    control: ControlHandle,
    adb: AdbController,
    diagnostics: Diagnostics,
) {
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
    let mut state = VirtualDesktopWatchdogState::default();
    let mut interval = time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if monitor.stopping.load(Ordering::Acquire) {
            return;
        }
        let lifecycle_connected = control.snapshot().await.state == HostState::Connected;
        let recovery_allowed =
            lifecycle_connected && virtual_desktop_watchdog_recovery_allowed(&monitor);
        if !state.observe(
            time::Instant::now(),
            flows.snapshot().connected,
            recovery_allowed,
        ) {
            continue;
        }

        let _ = diagnostics.record("vd_watchdog_recovery_started", json!({}));
        let recovery = {
            let _operation = monitor.operation.lock().await;
            if !virtual_desktop_watchdog_recovery_allowed(&monitor) || flows.snapshot().connected {
                Err("recovery_superseded")
            } else {
                let quiesce_adb = adb.clone();
                let quiesce =
                    task::spawn_blocking(move || quiesce_adb.stop_virtual_desktop()).await;
                if !matches!(quiesce, Ok(Ok(()))) {
                    Err("virtual_desktop_stop")
                } else if !virtual_desktop_watchdog_recovery_allowed(&monitor) {
                    Err("recovery_superseded")
                } else {
                    let service = task::spawn_blocking(restart_virtual_desktop_service).await;
                    if !matches!(service, Ok(Ok(()))) {
                        Err("virtual_desktop_service_recovery")
                    } else if !virtual_desktop_watchdog_recovery_allowed(&monitor) {
                        Err("recovery_superseded")
                    } else {
                        let restart_adb = adb.clone();
                        match task::spawn_blocking(move || restart_adb.restart_virtual_desktop())
                            .await
                        {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(_)) => Err("virtual_desktop_restart"),
                            Err(_) => Err("virtual_desktop_restart_task"),
                        }
                    }
                }
            }
        };
        let result = match recovery {
            Ok(()) => wait_for_virtual_desktop_watchdog_connection(&flows, &monitor).await,
            Err(category) => Err(category),
        };
        let _ = diagnostics.record(
            if result.is_ok() {
                "vd_watchdog_recovery_completed"
            } else {
                "vd_watchdog_recovery_failed"
            },
            json!({"category": result.err()}),
        );
    }
}

#[cfg(not(target_os = "windows"))]
async fn wait_for_virtual_desktop_connection(
    _flows: &VirtualDesktopFlowMonitor,
    _monitor: &AdbHealthMonitor,
    _recovery_generation: u64,
    _timeout: Duration,
) -> Result<(), &'static str> {
    Ok(())
}

async fn sleep_or_stop(monitor: &AdbHealthMonitor, duration: Duration) -> bool {
    tokio::select! {
        _ = time::sleep(duration) => monitor.stopping.load(Ordering::Acquire),
        _ = monitor.changed.notified() => monitor.stopping.load(Ordering::Acquire),
    }
}

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

fn cached_device_is_available(device_available: Option<bool>) -> bool {
    // Unavailable updates are reconciled when track-devices emits them. Replaying
    // an old `false` on a timer or lifecycle notification can erase the fresh
    // control authentication that arrives during wake, leaving the relay gated.
    device_available == Some(true)
}

fn should_reconcile_on_healthy_tick(
    forced: bool,
    device_available: Option<bool>,
    mappings_healthy: bool,
) -> bool {
    forced || (cached_device_is_available(device_available) && !mappings_healthy)
}

pub struct OperationGuard {
    path: PathBuf,
    _file: fs::File,
}

impl OperationGuard {
    pub fn acquire(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let open = || {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
        };
        let mut file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let owner = fs::read_to_string(&path)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if owner.is_some_and(process_is_running) {
                    return Err(error);
                }
                fs::remove_file(&path)?;
                open()?
            }
            Err(error) => return Err(error),
        };
        use std::io::Write;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self { path, _file: file })
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub control_bind: SocketAddr,
    pub socks_bind: SocketAddr,
    pub udp_bind: SocketAddr,
    pub session_id: SessionId,
    pub all_traffic: bool,
    pub paths: AppPaths,
    pub adb: AdbController,
    pub adb_program: PathBuf,
}

impl RuntimeConfig {
    pub fn new(
        session_id: SessionId,
        all_traffic: bool,
        paths: AppPaths,
        adb: AdbController,
        adb_program: PathBuf,
    ) -> Self {
        Self {
            control_bind: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), CONTROL_PORT),
            socks_bind: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), SOCKS_PORT),
            udp_bind: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), UDP_STREAM_PORT),
            session_id,
            all_traffic,
            paths,
            adb,
            adb_program,
        }
    }
}

pub struct HostRuntime {
    config: RuntimeConfig,
}

impl HostRuntime {
    pub fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        if !config.control_bind.ip().is_loopback() {
            return Err(RuntimeError::NonLoopback(config.control_bind));
        }
        if !config.socks_bind.ip().is_loopback() {
            return Err(RuntimeError::NonLoopback(config.socks_bind));
        }
        if !config.udp_bind.ip().is_loopback() {
            return Err(RuntimeError::NonLoopback(config.udp_bind));
        }
        Ok(Self { config })
    }

    pub async fn run(self) -> Result<(), RuntimeError> {
        install_runtime_kill_job()?;
        let _runtime_guard = OperationGuard::acquire(&self.config.paths.runtime_lock)?;
        let diagnostics = Diagnostics::open(&self.config.paths.logs)?;
        diagnostics.record(
            "runtime_started",
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "logging": {
                    "format": "jsonl",
                    "max_bytes_per_file": DEFAULT_MAX_BYTES,
                    "file_count": DEFAULT_FILE_COUNT,
                    "max_total_bytes": DEFAULT_TOTAL_BYTES,
                    "oldest_file_deleted_on_rotation": true,
                },
                "runtime_sample_interval_ms": 1_000,
            }),
        )?;
        let store = StateStore::new(&self.config.paths.status);
        write_daemon_identity(&self.config.paths.daemon_pid, self.config.session_id)?;
        store.write_runtime_not_ready(
            &StateSnapshot {
                state: HostState::Preparing,
                session_id: Some(self.config.session_id.to_string()),
                missed_heartbeats: 0,
                reason: None,
            },
            std::process::id(),
        )?;
        let relay_gate = RelayGate::default();
        let relay_gate_controller = RelayGateController::new(relay_gate.clone());
        let adb_monitor = AdbHealthMonitor::default();
        let observer_store = store.clone();
        let observer_diagnostics = diagnostics.clone();
        let observer_relay_gate = relay_gate_controller.clone();
        let observer_adb_monitor = adb_monitor.clone();
        let observer: StateObserver = Arc::new(move |snapshot| {
            update_relay_gate(&observer_relay_gate, snapshot);
            if snapshot.state == HostState::Connected {
                observer_adb_monitor.notify_authenticated_connection();
            } else {
                observer_adb_monitor.notify_state_change();
            }
            if let Err(error) = observer_store.write(snapshot, Some(std::process::id())) {
                let _ = observer_diagnostics.record(
                    "state_persistence_error",
                    json!({"error_kind": format!("{:?}", error.kind())}),
                );
            }
        });
        let suspend_relay_gate = relay_gate_controller.clone();
        let suspend_observer: SuspendObserver = Arc::new(move || {
            suspend_relay_gate.control_inactive();
        });
        let wake_relay_gate = relay_gate_controller.clone();
        let wake_adb_monitor = adb_monitor.clone();
        let wake_observer: WakeObserver = Arc::new(move || {
            wake_relay_gate.authenticated_wake();
            wake_adb_monitor.notify_authenticated_connection();
        });

        let control = ControlServer::new(ControlConfig {
            bind: self.config.control_bind,
            session_id: self.config.session_id,
        })?
        .with_diagnostics(diagnostics.clone())
        .with_observer(observer)
        .with_suspend_observer(suspend_observer)
        .with_wake_observer(wake_observer);
        let control_handle = control.command_handle();
        let shutdown = Arc::new(Notify::new());
        let admin = AdminServer::new(
            self.config.paths.clone(),
            load_or_create_admin_token(&self.config.paths.admin_token)?,
            control_handle.clone(),
            adb_monitor.clone(),
            shutdown.clone(),
        );
        let tcp_socks = SocksServer::new(SocksConfig {
            bind: self.config.socks_bind,
            command_policy: SocksCommandPolicy::ConnectOnly,
            ..Default::default()
        })?
        .with_diagnostics(diagnostics.clone())
        .with_relay_gate(relay_gate.clone());
        let udp_socks = SocksServer::new(SocksConfig {
            bind: self.config.udp_bind,
            command_policy: SocksCommandPolicy::FwdUdpOnly,
            ..Default::default()
        })?
        .with_diagnostics(diagnostics.clone())
        .with_relay_gate(relay_gate.clone());
        let control_listener = TcpListener::bind(self.config.control_bind).await?;
        let tcp_listener = TcpListener::bind(self.config.socks_bind).await?;
        let udp_listener = TcpListener::bind(self.config.udp_bind).await?;
        let metrics_tcp_socks = tcp_socks.clone();
        let metrics_udp_socks = udp_socks.clone();
        let virtual_desktop_flows = tcp_socks.virtual_desktop_flows();
        let metrics_control = control.clone();
        let metrics_control_handle = control_handle.clone();
        let metrics_adb = adb_monitor.clone();
        let metrics_store = store.clone();
        let metrics_diagnostics = diagnostics.clone();
        let runtime_ready = Arc::new(AtomicBool::new(false));
        let metrics_runtime_ready = runtime_ready.clone();
        let process_generation = unix_millis()
            .saturating_mul(1_000)
            .saturating_add(u128::from(std::process::id()));
        let metrics = tokio::spawn(async move {
            const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
            let mut interval = time::interval(SAMPLE_INTERVAL);
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            let mut last_sample_started = None;
            let mut previous_persistence = RuntimePersistenceMetrics::default();
            loop {
                interval.tick().await;
                let sample_started = Instant::now();
                let sample_interval_us = last_sample_started
                    .replace(sample_started)
                    .map(|previous: Instant| {
                        sample_started
                            .saturating_duration_since(previous)
                            .as_micros()
                            .min(u64::MAX as u128) as u64
                    })
                    .unwrap_or(SAMPLE_INTERVAL.as_micros() as u64);
                let scheduler_lag_us =
                    sample_interval_us.saturating_sub(SAMPLE_INTERVAL.as_micros() as u64);
                let process = crate::diagnostics::process_sample();
                let mut relay = metrics_udp_socks.stats();
                let tcp = metrics_tcp_socks.stats();
                relay.accepted_connections = relay
                    .accepted_connections
                    .saturating_add(tcp.accepted_connections);
                relay.rejected_connections = relay
                    .rejected_connections
                    .saturating_add(tcp.rejected_connections);
                relay.active_connections = relay
                    .active_connections
                    .saturating_add(tcp.active_connections);
                relay.tcp_tx_bytes = relay.tcp_tx_bytes.saturating_add(tcp.tcp_tx_bytes);
                relay.tcp_rx_bytes = relay.tcp_rx_bytes.saturating_add(tcp.tcp_rx_bytes);
                relay.virtual_desktop = tcp.virtual_desktop;
                let control = metrics_control.metrics();
                let telemetry = RuntimeTelemetry {
                    control,
                    android: metrics_control.android_metrics(),
                    relay,
                    adb: metrics_adb.snapshot(),
                    process,
                };
                let snapshot = metrics_control_handle.snapshot().await;
                let fields = json!({
                    "process": {
                        "pid": std::process::id(),
                        "role": "daemon",
                        "generation": process_generation,
                    },
                    "runtime_ready": metrics_runtime_ready.load(Ordering::Acquire),
                    "lifecycle": &snapshot,
                    "telemetry": &telemetry,
                    "timing": {
                        "sample_interval_us": sample_interval_us,
                        "scheduler_lag_us": scheduler_lag_us,
                        "previous_diagnostics_write_us": previous_persistence.diagnostics.duration_us,
                        "previous_log_rotated": previous_persistence.diagnostics.rotated,
                        "previous_status_write_us": previous_persistence.status_write_us,
                        "previous_persistence_failed": previous_persistence.failed,
                    },
                });
                let persistence_diagnostics = metrics_diagnostics.clone();
                let persistence_store = metrics_store.clone();
                previous_persistence = task::spawn_blocking(move || {
                    let diagnostics_result =
                        persistence_diagnostics.record_with_metrics("runtime_sample", fields);
                    let status_started = Instant::now();
                    let status_result = persistence_store.write_with_telemetry(
                        &snapshot,
                        Some(std::process::id()),
                        telemetry,
                    );
                    let diagnostics_failed = diagnostics_result.is_err();
                    RuntimePersistenceMetrics {
                        diagnostics: diagnostics_result.unwrap_or_default(),
                        status_write_us: status_started.elapsed().as_micros().min(u64::MAX as u128)
                            as u64,
                        failed: diagnostics_failed || status_result.is_err(),
                    }
                })
                .await
                .unwrap_or(RuntimePersistenceMetrics {
                    failed: true,
                    ..Default::default()
                });
            }
        });

        let monitor = tokio::spawn(adb_monitor.clone().run(
            AdbMonitorConfig {
                adb: self.config.adb.clone(),
                adb_program: self.config.adb_program.clone(),
                session_id: self.config.session_id,
                all_traffic: self.config.all_traffic,
                virtual_desktop_flows: virtual_desktop_flows.clone(),
            },
            control_handle.clone(),
            store.clone(),
            diagnostics.clone(),
            relay_gate_controller,
        ));
        #[cfg(target_os = "windows")]
        let virtual_desktop_watchdog = tokio::spawn(run_virtual_desktop_watchdog(
            virtual_desktop_flows,
            adb_monitor.clone(),
            control_handle.clone(),
            self.config.adb.clone(),
            diagnostics.clone(),
        ));

        let mut control_task = tokio::spawn(async move {
            control
                .serve_on(control_listener)
                .await
                .map_err(RuntimeError::from)
        });
        let mut tcp_task = tokio::spawn(async move {
            tcp_socks
                .serve_on(tcp_listener)
                .await
                .map_err(RuntimeError::from)
        });
        let mut udp_task = tokio::spawn(async move {
            udp_socks
                .serve_on(udp_listener)
                .await
                .map_err(RuntimeError::from)
        });
        let mut admin_task = tokio::spawn(admin.serve());
        wait_for_admin_ready(&self.config.paths).await?;
        store.write_runtime_ready(&control_handle.snapshot().await, std::process::id())?;
        runtime_ready.store(true, Ordering::Release);

        let result = tokio::select! {
            result = &mut control_task => flatten_runtime_task(result),
            result = &mut tcp_task => flatten_runtime_task(result),
            result = &mut udp_task => flatten_runtime_task(result),
            result = &mut admin_task => flatten_runtime_task(result),
            _ = shutdown.notified() => Ok(()),
            result = tokio::signal::ctrl_c() => result.map_err(RuntimeError::Io),
        };
        adb_monitor.suppress_repairs().await;
        monitor.abort();
        #[cfg(target_os = "windows")]
        virtual_desktop_watchdog.abort();
        metrics.abort();
        control_task.abort();
        tcp_task.abort();
        udp_task.abort();
        admin_task.abort();
        let _ = fs::remove_file(&self.config.paths.daemon_pid);
        result
    }
}

async fn wait_for_admin_ready(paths: &AppPaths) -> Result<(), RuntimeError> {
    let deadline = time::Instant::now() + Duration::from_secs(3);
    loop {
        if admin_command(paths, "status", Duration::from_millis(150))
            .await
            .is_ok_and(|response| response.ok)
        {
            return Ok(());
        }
        if time::Instant::now() >= deadline {
            return Err(RuntimeError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "per-user admin endpoint did not become ready within 3 seconds",
            )));
        }
        time::sleep(Duration::from_millis(25)).await;
    }
}

fn flatten_runtime_task(
    result: Result<Result<(), RuntimeError>, tokio::task::JoinError>,
) -> Result<(), RuntimeError> {
    result.map_err(|error| RuntimeError::Io(io::Error::other(error)))?
}

fn update_relay_gate(relay_gate: &RelayGateController, snapshot: &StateSnapshot) {
    match snapshot.state {
        HostState::Connected => relay_gate.control_connected(),
        HostState::Degraded if snapshot.reason.as_deref() == Some(TRANSPORT_RECOVERY_REASON) => {
            relay_gate.carrier_lost();
        }
        HostState::Degraded
            if snapshot.reason.as_deref() == Some(CONTROL_TRANSPORT_LOST_REASON) =>
        {
            relay_gate.control_degraded();
        }
        HostState::Stopped | HostState::Stopping | HostState::Error => {
            relay_gate.control_inactive();
        }
        HostState::Preparing | HostState::Degraded => {}
    }
}

#[derive(Clone)]
pub struct AdminServer {
    #[cfg(unix)]
    paths: AppPaths,
    token: String,
    control: ControlHandle,
    adb_monitor: AdbHealthMonitor,
    shutdown: Arc<Notify>,
    max_connections: usize,
    io_timeout: Duration,
}

impl AdminServer {
    pub fn new(
        paths: AppPaths,
        token: String,
        control: ControlHandle,
        adb_monitor: AdbHealthMonitor,
        shutdown: Arc<Notify>,
    ) -> Self {
        #[cfg(target_os = "windows")]
        let _ = paths;
        Self {
            #[cfg(unix)]
            paths,
            token,
            control,
            adb_monitor,
            shutdown,
            max_connections: MAX_ADMIN_CONNECTIONS,
            io_timeout: ADMIN_IO_TIMEOUT,
        }
    }

    #[cfg(all(test, unix))]
    fn with_limits(mut self, max_connections: usize, io_timeout: Duration) -> Self {
        self.max_connections = max_connections;
        self.io_timeout = io_timeout;
        self
    }

    pub async fn serve(self) -> Result<(), RuntimeError> {
        #[cfg(unix)]
        {
            return self.serve_unix().await;
        }
        #[cfg(target_os = "windows")]
        {
            return self.serve_windows().await;
        }
        #[allow(unreachable_code)]
        Err(RuntimeError::AdminTransportUnsupported)
    }

    #[cfg(unix)]
    async fn serve_unix(self) -> Result<(), RuntimeError> {
        if self.paths.admin_socket.exists() {
            match UnixStream::connect(&self.paths.admin_socket).await {
                Ok(_) => {
                    return Err(RuntimeError::Io(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "per-user admin socket is already serving",
                    )))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&self.paths.admin_socket)?;
                }
                Err(error) => return Err(RuntimeError::Io(error)),
            }
        }
        let listener = UnixListener::bind(&self.paths.admin_socket)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&self.paths.admin_socket, fs::Permissions::from_mode(0o600))?;
        let _cleanup = UnixSocketCleanup(self.paths.admin_socket.clone());
        self.serve_unix_on(listener).await
    }

    #[cfg(unix)]
    async fn serve_unix_on(self, listener: UnixListener) -> Result<(), RuntimeError> {
        let permits = Arc::new(Semaphore::new(self.max_connections));
        loop {
            let (stream, _) = listener.accept().await?;
            let permit = match permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            let server = self.clone();
            tokio::spawn(async move {
                let _ = server.handle(stream).await;
                drop(permit);
            });
        }
    }

    #[cfg(target_os = "windows")]
    async fn serve_windows(self) -> Result<(), RuntimeError> {
        let pipe_name = windows_admin_pipe_name()?;
        let permits = Arc::new(Semaphore::new(self.max_connections));
        let mut first = true;
        loop {
            // Reserve userspace capacity before allocating the next kernel
            // instance. Creating a ninth instance while eight handlers are
            // active would otherwise hit the pipe instance limit and tear down
            // the complete host runtime.
            let permit = permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| RuntimeError::AdminTransportClosed)?;
            let server = match create_secure_named_pipe(&pipe_name, first, self.max_connections) {
                Ok(server) => server,
                Err(error)
                    if error.raw_os_error()
                        == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32) =>
                {
                    drop(permit);
                    time::sleep(Duration::from_millis(25)).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            first = false;
            server.connect().await?;
            let service = self.clone();
            tokio::spawn(async move {
                let _ = service.handle(server).await;
                drop(permit);
            });
        }
    }

    async fn handle<S>(&self, mut stream: S) -> Result<(), RuntimeError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let request: AdminRequest = time::timeout(self.io_timeout, read_admin_message(&mut stream))
            .await
            .map_err(|_| RuntimeError::AdminTimeout)??;
        if !constant_time_eq(request.token.as_bytes(), self.token.as_bytes()) {
            time::timeout(
                self.io_timeout,
                write_admin_message(
                    &mut stream,
                    &AdminResponse {
                        ok: false,
                        repairs_suppressed: false,
                        error: Some("authentication failed".into()),
                        status: None,
                    },
                ),
            )
            .await
            .map_err(|_| RuntimeError::AdminTimeout)??;
            return Ok(());
        }
        let response = match request.command.as_str() {
            "host_resume" => {
                let status = self.control.snapshot().await;
                let status = if matches!(status.state, HostState::Connected | HostState::Degraded) {
                    self.adb_monitor.notify_host_resume();
                    self.control
                        .reset_transport(TRANSPORT_RECOVERY_REASON)
                        .await
                } else {
                    status
                };
                AdminResponse {
                    ok: true,
                    repairs_suppressed: false,
                    error: None,
                    status: Some(status),
                }
            }
            "stop" => {
                // Stop new mapping work first, then send STOP immediately so
                // Android can close the VPN descriptor without waiting behind
                // an already-running health command. Drain that one bounded
                // command before acknowledging serialized cleanup to the host.
                self.adb_monitor.begin_suppress_repairs();
                let stop_result = self.control.request_stop(Duration::from_secs(6)).await;
                self.adb_monitor.drain_repairs().await;
                match stop_result {
                    Ok(()) => AdminResponse {
                        ok: true,
                        repairs_suppressed: true,
                        error: None,
                        status: Some(self.control.snapshot().await),
                    },
                    Err(error) => AdminResponse {
                        ok: false,
                        repairs_suppressed: true,
                        error: Some(error.to_string()),
                        status: Some(self.control.snapshot().await),
                    },
                }
            }
            "status" => AdminResponse {
                ok: true,
                repairs_suppressed: false,
                error: None,
                status: Some(self.control.snapshot().await),
            },
            "shutdown" => {
                let status = self.control.snapshot().await;
                let stopped = status.state == HostState::Stopped;
                AdminResponse {
                    ok: stopped,
                    repairs_suppressed: self.adb_monitor.snapshot().repair_suppressed,
                    error: (!stopped).then(|| "daemon shutdown requires stopped state".into()),
                    status: Some(status),
                }
            }
            _ => AdminResponse {
                ok: false,
                repairs_suppressed: false,
                error: Some("unknown command".into()),
                status: None,
            },
        };
        time::timeout(self.io_timeout, write_admin_message(&mut stream, &response))
            .await
            .map_err(|_| RuntimeError::AdminTimeout)??;
        if request.command == "shutdown" && response.ok {
            self.shutdown.notify_one();
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct AdminRequest {
    token: String,
    command: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AdminResponse {
    pub ok: bool,
    #[serde(default)]
    pub repairs_suppressed: bool,
    pub error: Option<String>,
    pub status: Option<StateSnapshot>,
}

pub async fn admin_command(
    paths: &AppPaths,
    command: &str,
    timeout: Duration,
) -> Result<AdminResponse, RuntimeError> {
    let token = fs::read_to_string(&paths.admin_token)?.trim().to_owned();
    let operation = async {
        #[cfg(unix)]
        let mut stream = UnixStream::connect(&paths.admin_socket).await?;
        #[cfg(target_os = "windows")]
        let mut stream = open_named_pipe_client(&windows_admin_pipe_name()?).await?;
        #[cfg(not(any(unix, target_os = "windows")))]
        return Err(RuntimeError::AdminTransportUnsupported);
        write_admin_message(
            &mut stream,
            &AdminRequest {
                token,
                command: command.to_owned(),
            },
        )
        .await?;
        read_admin_message(&mut stream).await
    };
    time::timeout(timeout, operation)
        .await
        .map_err(|_| RuntimeError::AdminTimeout)?
}

async fn read_admin_message<T, S>(stream: &mut S) -> Result<T, RuntimeError>
where
    T: for<'de> Deserialize<'de>,
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await? as usize;
    if length > MAX_ADMIN_MESSAGE {
        return Err(RuntimeError::AdminMessageTooLarge(length));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(RuntimeError::Json)
}

async fn write_admin_message<T, S>(stream: &mut S, value: &T) -> Result<(), RuntimeError>
where
    T: Serialize,
    S: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(value).map_err(RuntimeError::Json)?;
    if bytes.len() > MAX_ADMIN_MESSAGE {
        return Err(RuntimeError::AdminMessageTooLarge(bytes.len()));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(unix)]
struct UnixSocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for UnixSocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(target_os = "windows")]
async fn open_named_pipe_client(
    pipe_name: &str,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    const ERROR_PIPE_BUSY_CODE: i32 = windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32;
    loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY_CODE) => {
                time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "windows")]
pub fn create_secure_named_pipe(
    pipe_name: &str,
    first: bool,
    max_instances: usize,
) -> io::Result<NamedPipeServer> {
    use std::ffi::c_void;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let descriptor = current_user_only_security_descriptor()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .max_instances(max_instances)
        .reject_remote_clients(true);
    // SAFETY: `attributes` and its LocalAlloc-owned security descriptor stay
    // alive for the complete CreateNamedPipeW call. The handle does not retain
    // the pointer after creation.
    unsafe {
        options.create_with_security_attributes_raw(
            pipe_name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

#[cfg(target_os = "windows")]
fn windows_admin_pipe_name() -> io::Result<String> {
    Ok(format!(
        r"\\.\pipe\gnirehtet-vd-{}",
        windows_current_user_sid()?.replace('-', "_")
    ))
}

/// Returns the separate per-user command-broker pipe name.
///
/// The data-plane daemon keeps using `windows_admin_pipe_name()`: separating
/// these lanes lets the always-on product instance serialize public commands
/// without weakening or overloading the daemon's authenticated control pipe.
#[cfg(target_os = "windows")]
pub fn windows_broker_pipe_name() -> io::Result<String> {
    Ok(format!(
        r"\\.\pipe\gnirehtet-vd-broker-{}",
        windows_current_user_sid()?.replace('-', "_")
    ))
}

#[cfg(target_os = "windows")]
struct LocalSecurityDescriptor(*mut std::ffi::c_void);

#[cfg(target_os = "windows")]
impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by LocalAlloc inside the SDDL
        // conversion API and is released exactly once here.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
fn current_user_only_security_descriptor() -> io::Result<LocalSecurityDescriptor> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

    let sddl = user_only_pipe_sddl(&windows_current_user_sid()?);
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = null_mut();
    // SAFETY: `wide` is NUL-terminated and `descriptor` is a valid output
    // pointer. Revision 1 is SDDL_REVISION_1.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalSecurityDescriptor(descriptor))
}

#[cfg(any(target_os = "windows", test))]
fn user_only_pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

#[cfg(target_os = "windows")]
pub fn windows_current_user_sid() -> io::Result<String> {
    use std::ptr::null_mut;
    use windows_sys::{
        core::PWSTR,
        Win32::{
            Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, HANDLE},
            Security::{
                Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
                TOKEN_USER,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: this owns one successful OpenProcessToken handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut token = null_mut();
    // SAFETY: the pseudo process handle is always valid and `token` is an
    // initialized output pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Token(token);
    let mut required = 0;
    // The sizing call is expected to fail with insufficient buffer while
    // returning the exact required length.
    let sized = unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
    if sized != 0
        || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER
        || required < std::mem::size_of::<TOKEN_USER>() as u32
    {
        return Err(io::Error::last_os_error());
    }
    let word_count = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut aligned = vec![0usize; word_count];
    // SAFETY: `aligned` has native pointer alignment and at least the byte size
    // returned by the API. It remains live while reading TOKEN_USER.User.Sid.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            aligned.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*(aligned.as_ptr().cast::<TOKEN_USER>()) };
    let mut string_sid: PWSTR = null_mut();
    // SAFETY: TOKEN_USER owns a valid SID for the lifetime of `aligned` and the
    // output pointer is released with LocalFree below.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string_sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = (0..)
        .position(|index| unsafe { *string_sid.add(index) == 0 })
        .ok_or_else(|| io::Error::other("Windows SID string was not terminated"))?;
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, length) })
        .map_err(io::Error::other);
    unsafe {
        LocalFree(string_sid.cast());
    }
    sid
}

fn load_or_create_admin_token(path: &Path) -> io::Result<String> {
    if let Ok(token) = fs::read_to_string(path) {
        let token = token.trim();
        if token
            .parse::<SessionId>()
            .is_ok_and(|session| session != SessionId::ZERO)
        {
            return Ok(token.to_owned());
        }
    }
    let token = SessionId::random().to_string();
    #[cfg(unix)]
    {
        use std::{
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    fs::write(path, &token)?;
    Ok(token)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum StreamerProbe {
    Unsupported,
    CheckFailed,
    Unknown,
    NotRunning,
    RunningNotListening,
    Listening {
        tcp_listener_count: usize,
        udp_endpoint_count: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DoctorReport {
    pub host_runtime_ready: bool,
    pub control_connected: bool,
    pub adb_state: Result<String, String>,
    pub reverse_mappings_healthy: bool,
    pub reverse_mapping_error: Option<String>,
    pub virtual_desktop_streamer: StreamerProbe,
    pub android_vpn: Result<AndroidVpnStatus, String>,
    pub tunnel_available: bool,
}

pub fn doctor(paths: &AppPaths, adb: &AdbController) -> DoctorReport {
    let live_daemon = read_daemon_pid(&paths.daemon_pid).filter(|pid| process_is_running(*pid));
    let persisted = StateStore::new(&paths.status).read().ok();
    let host_runtime_ready = persisted.as_ref().is_some_and(|status| {
        live_daemon.is_some()
            && status.daemon_pid == live_daemon
            && status.daemon_running
            && status.runtime_ready
    });
    let control_connected = persisted.as_ref().is_some_and(|status| {
        host_runtime_ready
            && status.lifecycle.state == HostState::Connected
            && status.lifecycle.missed_heartbeats == 0
    });
    let adb_state = adb.device_state().map_err(|error| error.to_string());
    let mapping = adb.mapping_health();
    let (reverse_mappings_healthy, reverse_mapping_error) = match mapping {
        Ok(health) => (health.is_healthy(), None),
        Err(error) => (false, Some(error.to_string())),
    };
    let virtual_desktop_streamer = probe_virtual_desktop_streamer();
    let android_vpn = adb.android_status().map_err(|error| error.to_string());
    let android_ready = android_vpn.as_ref().is_ok_and(|status| {
        status.vpn_fd_open == Some(true)
            && matches!(status.state.as_deref(), Some("connected" | "degraded"))
    });
    DoctorReport {
        tunnel_available: tunnel_layers_available(
            host_runtime_ready,
            control_connected,
            adb_state.as_deref() == Ok("device"),
            reverse_mappings_healthy,
            android_ready,
            &virtual_desktop_streamer,
        ),
        host_runtime_ready,
        control_connected,
        adb_state,
        reverse_mappings_healthy,
        reverse_mapping_error,
        virtual_desktop_streamer,
        android_vpn,
    }
}

fn tunnel_layers_available(
    host_runtime_ready: bool,
    control_connected: bool,
    adb_ready: bool,
    mappings_ready: bool,
    android_ready: bool,
    _streamer: &StreamerProbe,
) -> bool {
    host_runtime_ready && control_connected && adb_ready && mappings_ready && android_ready
}

#[cfg(target_os = "windows")]
fn probe_virtual_desktop_streamer() -> StreamerProbe {
    // Read-only: this intentionally does not start, stop, or kill VD.
    let script = "try {$p=Get-Process -Name 'VirtualDesktop.Streamer' -ErrorAction SilentlyContinue; if($null -eq $p){'NOT_RUNNING';exit}; $ids=@($p.Id); $tcp=@(Get-NetTCPConnection -State Listen -ErrorAction Stop | Where-Object {$ids -contains $_.OwningProcess}).Count; $udp=@(Get-NetUDPEndpoint -ErrorAction Stop | Where-Object {$ids -contains $_.OwningProcess}).Count; if(($tcp+$udp) -eq 0){'NOT_LISTENING'}else{'LISTENING '+$tcp+' '+$udp}} catch {'CHECK_FAILED'}";
    let output = command_text_with_timeout(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        Duration::from_secs(3),
    );
    let Ok((status, text)) = output else {
        return StreamerProbe::CheckFailed;
    };
    if !status.success() {
        return StreamerProbe::CheckFailed;
    }
    let text = text.trim();
    if text == "NOT_RUNNING" {
        StreamerProbe::NotRunning
    } else if text == "NOT_LISTENING" {
        StreamerProbe::RunningNotListening
    } else if text == "CHECK_FAILED" {
        StreamerProbe::CheckFailed
    } else if let Some(value) = text.strip_prefix("LISTENING ") {
        let mut counts = value.split_whitespace();
        let tcp_listener_count = counts.next().and_then(|value| value.parse().ok());
        let udp_endpoint_count = counts.next().and_then(|value| value.parse().ok());
        match (tcp_listener_count, udp_endpoint_count) {
            (Some(tcp_listener_count), Some(udp_endpoint_count)) => StreamerProbe::Listening {
                tcp_listener_count,
                udp_endpoint_count,
            },
            _ => StreamerProbe::Unknown,
        }
    } else {
        StreamerProbe::Unknown
    }
}

#[cfg(not(target_os = "windows"))]
fn probe_virtual_desktop_streamer() -> StreamerProbe {
    StreamerProbe::Unsupported
}

pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        windows_process_snapshot(pid).is_ok_and(|snapshot| snapshot.active)
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        false
    }
}

#[cfg(not(target_os = "windows"))]
fn install_runtime_kill_job() -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_runtime_kill_job() -> io::Result<()> {
    use std::{ffi::c_void, ptr::null};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::GetCurrentProcess,
        },
    };

    static JOB_HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    if JOB_HANDLE.get().is_some() {
        return Ok(());
    }
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(error);
    }
    if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(error);
    }
    // Intentionally retain the handle until process teardown. When the daemon
    // is terminated, Windows closes this last handle and atomically kills any
    // in-flight ADB descendants before a later explicit-stop retry can remove
    // mappings.
    JOB_HANDLE
        .set(job as usize)
        .map_err(|_| io::Error::other("runtime Job Object was installed concurrently"))?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn terminate_process(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;
    #[cfg(not(unix))]
    return Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported OS"));
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("daemon termination command failed"))
    }
}

#[cfg(target_os = "windows")]
fn command_text_with_timeout(
    program: &str,
    arguments: &[&str],
    timeout: Duration,
) -> io::Result<(std::process::ExitStatus, String)> {
    use std::os::windows::process::CommandExt;
    const MAX_COMMAND_OUTPUT: u64 = 64 * 1024;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{program} exceeded {timeout:?}"),
            ));
        }
    };
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("subprocess stdout was not captured"))?
        .take(MAX_COMMAND_OUTPUT)
        .read_to_end(&mut output)?;
    Ok((status, String::from_utf8_lossy(&output).into_owned()))
}

#[cfg(target_os = "windows")]
pub fn virtual_desktop_recovery_task_ready() -> io::Result<bool> {
    let (status, xml) = command_text_with_timeout(
        "schtasks.exe",
        &["/Query", "/TN", VIRTUAL_DESKTOP_RECOVERY_TASK, "/XML"],
        Duration::from_secs(5),
    )?;
    if !status.success() {
        return Ok(false);
    }
    let xml = xml.replace('\0', "").to_ascii_lowercase();
    let sid = windows_current_user_sid()?.to_ascii_lowercase();
    let required = [
        format!("<userid>{sid}</userid>"),
        "<logontype>interactivetoken</logontype>".into(),
        "<runlevel>highestavailable</runlevel>".into(),
        "<command>c:\\windows\\system32\\windowspowershell\\v1.0\\powershell.exe</command>".into(),
        format!(
            "<arguments>{}</arguments>",
            VIRTUAL_DESKTOP_RECOVERY_ARGUMENTS.to_ascii_lowercase()
        ),
    ];
    Ok(required.iter().all(|value| xml.contains(value)))
}

#[cfg(target_os = "windows")]
pub fn install_virtual_desktop_recovery_task() -> io::Result<()> {
    let arguments = VIRTUAL_DESKTOP_RECOVERY_ARGUMENTS.replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop';$a=New-ScheduledTaskAction -Execute 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe' -Argument '{arguments}';$p=New-ScheduledTaskPrincipal -UserId ([System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value) -LogonType Interactive -RunLevel Highest;$s=New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew;Register-ScheduledTask -TaskName 'Quest VD Wired - Virtual Desktop Recovery' -Action $a -Principal $p -Settings $s -Force | Out-Null"
    );
    let (status, _) = command_text_with_timeout(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
        Duration::from_secs(20),
    )?;
    if !status.success() {
        return Err(io::Error::other(
            "registering the Virtual Desktop recovery task failed",
        ));
    }
    if !virtual_desktop_recovery_task_ready()? {
        return Err(io::Error::other(
            "the Virtual Desktop recovery task did not match its fixed definition",
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn virtual_desktop_service_pid() -> io::Result<u32> {
    let service_name = VIRTUAL_DESKTOP_SERVICE_NAME;
    let script = format!(
        "$s=Get-CimInstance Win32_Service -Filter \"Name='{service_name}'\" -ErrorAction Stop;if($s.State -eq 'Running'){{$s.ProcessId}}else{{0}}"
    );
    let (status, output) = command_text_with_timeout(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
        Duration::from_secs(5),
    )?;
    if !status.success() {
        return Err(io::Error::other(
            "querying the Virtual Desktop service failed",
        ));
    }
    output
        .trim()
        .parse::<u32>()
        .map_err(|_| io::Error::other("Virtual Desktop service returned an invalid process ID"))
}

#[cfg(target_os = "windows")]
fn virtual_desktop_streamer_pid() -> io::Result<u32> {
    let script = "$session=(Get-Process -Id $PID).SessionId;$p=Get-Process -Name 'VirtualDesktop.Streamer' -ErrorAction SilentlyContinue|Where-Object {$_.SessionId -eq $session}|Select-Object -First 1;if($null -eq $p){0}else{$p.Id}";
    let (status, output) = command_text_with_timeout(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        Duration::from_secs(5),
    )?;
    if !status.success() {
        return Err(io::Error::other("querying Virtual Desktop Streamer failed"));
    }
    output
        .trim()
        .parse::<u32>()
        .map_err(|_| io::Error::other("Virtual Desktop Streamer returned an invalid process ID"))
}

#[cfg(target_os = "windows")]
fn virtual_desktop_streamer_cloud_ready(pid: u32) -> io::Result<bool> {
    let script = format!(
        "$c=@(Get-NetTCPConnection -OwningProcess {pid} -State Established -ErrorAction SilentlyContinue);$cloud=@($c|Where-Object {{$_.RemotePort -eq 443}}).Count -gt 0;$broker=@($c|Where-Object {{$_.RemotePort -ge 38810 -and $_.RemotePort -le 38820}}).Count -gt 0;if($cloud -and $broker){{1}}else{{0}}"
    );
    let (status, output) = command_text_with_timeout(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
        Duration::from_secs(5),
    )?;
    if !status.success() {
        return Err(io::Error::other(
            "querying Virtual Desktop Streamer readiness failed",
        ));
    }
    match output.trim() {
        "1" => Ok(true),
        "0" => Ok(false),
        _ => Err(io::Error::other(
            "Virtual Desktop Streamer returned invalid readiness",
        )),
    }
}

#[cfg(target_os = "windows")]
pub fn restart_virtual_desktop_service() -> io::Result<()> {
    if !virtual_desktop_recovery_task_ready()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the Virtual Desktop recovery task is unavailable",
        ));
    }
    let previous_pid = virtual_desktop_service_pid()?;
    let previous_streamer_pid = virtual_desktop_streamer_pid()?;
    let (status, _) = command_text_with_timeout(
        "schtasks.exe",
        &["/Run", "/TN", VIRTUAL_DESKTOP_RECOVERY_TASK],
        Duration::from_secs(5),
    )?;
    if !status.success() {
        return Err(io::Error::other(
            "starting the Virtual Desktop recovery task failed",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        let pid = virtual_desktop_service_pid().unwrap_or_default();
        let streamer_pid = virtual_desktop_streamer_pid().unwrap_or_default();
        if pid != 0
            && (previous_pid == 0 || pid != previous_pid)
            && streamer_pid != 0
            && (previous_streamer_pid == 0 || streamer_pid != previous_streamer_pid)
            && virtual_desktop_streamer_cloud_ready(streamer_pid).unwrap_or(false)
        {
            std::thread::sleep(Duration::from_secs(5));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Virtual Desktop service and Streamer did not become ready",
    ))
}

#[cfg(not(target_os = "windows"))]
pub fn restart_virtual_desktop_service() -> io::Result<()> {
    Ok(())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("host listeners must be loopback, got {0}")]
    NonLoopback(SocketAddr),
    #[error(transparent)]
    Control(#[from] crate::control::ControlError),
    #[error(transparent)]
    Socks(#[from] crate::socks::SocksError),
    #[error("host runtime I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("admin message length {0} exceeds the bound")]
    AdminMessageTooLarge(usize),
    #[error("admin command timed out")]
    AdminTimeout,
    #[error("per-user admin transport is unsupported on this platform")]
    AdminTransportUnsupported,
    #[error("per-user admin transport capacity was closed")]
    AdminTransportClosed,
    #[error("admin JSON failed: {0}")]
    Json(serde_json::Error),
}

pub fn read_daemon_pid(path: &Path) -> Option<u32> {
    #[cfg(target_os = "windows")]
    {
        let identity: DaemonIdentity = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let current = windows_process_snapshot(identity.pid).ok()?;
        daemon_identity_matches(&identity, &current).then_some(identity.pid)
    }
    #[cfg(not(target_os = "windows"))]
    {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, Deserialize, Serialize)]
struct DaemonIdentity {
    role: String,
    pid: u32,
    creation_time_100ns: u64,
    session_id: String,
    executable_path: String,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug)]
struct WindowsProcessSnapshot {
    active: bool,
    creation_time_100ns: u64,
    executable_path: String,
}

#[cfg(any(target_os = "windows", test))]
fn daemon_identity_matches(identity: &DaemonIdentity, snapshot: &WindowsProcessSnapshot) -> bool {
    identity.role == "gnirehtet-vd-daemon"
        && identity
            .session_id
            .parse::<SessionId>()
            .is_ok_and(|session| session != SessionId::ZERO)
        && snapshot.active
        && snapshot.creation_time_100ns == identity.creation_time_100ns
        && snapshot
            .executable_path
            .eq_ignore_ascii_case(&identity.executable_path)
}

#[cfg(target_os = "windows")]
struct WindowsProcessHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for WindowsProcessHandle {
    fn drop(&mut self) {
        // SAFETY: this owns one successful OpenProcess handle.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
fn open_windows_process(pid: u32, access: u32) -> io::Result<WindowsProcessHandle> {
    let handle = unsafe { windows_sys::Win32::System::Threading::OpenProcess(access, 0, pid) };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(WindowsProcessHandle(handle))
    }
}

#[cfg(target_os = "windows")]
fn windows_process_snapshot(pid: u32) -> io::Result<WindowsProcessSnapshot> {
    let handle = open_windows_process(
        pid,
        windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    windows_process_snapshot_from_handle(handle.0)
}

#[cfg(target_os = "windows")]
fn windows_process_snapshot_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<WindowsProcessSnapshot> {
    use windows_sys::Win32::{
        Foundation::{FILETIME, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, GetProcessTimes, QueryFullProcessImageNameW},
    };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut exit_code = 0;
    if unsafe { GetExitCodeProcess(handle, &mut exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut executable = vec![0u16; 32_768];
    let mut executable_length = executable.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(handle, 0, executable.as_mut_ptr(), &mut executable_length)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let executable_path =
        String::from_utf16(&executable[..executable_length as usize]).map_err(io::Error::other)?;
    Ok(WindowsProcessSnapshot {
        active: exit_code == STILL_ACTIVE as u32,
        creation_time_100ns: (u64::from(creation.dwHighDateTime) << 32)
            | u64::from(creation.dwLowDateTime),
        executable_path,
    })
}

#[cfg(target_os = "windows")]
fn write_daemon_identity(path: &Path, session_id: SessionId) -> io::Result<()> {
    let pid = std::process::id();
    let snapshot = windows_process_snapshot(pid)?;
    if !snapshot.active {
        return Err(io::Error::other("current host process is not active"));
    }
    fs::write(
        path,
        serde_json::to_vec(&DaemonIdentity {
            role: "gnirehtet-vd-daemon".into(),
            pid,
            creation_time_100ns: snapshot.creation_time_100ns,
            session_id: session_id.to_string(),
            executable_path: snapshot.executable_path,
        })
        .map_err(io::Error::other)?,
    )
}

#[cfg(not(target_os = "windows"))]
fn write_daemon_identity(path: &Path, _session_id: SessionId) -> io::Result<()> {
    fs::write(path, std::process::id().to_string())
}

/// Terminates only the exact daemon instance described by the identity file.
/// On Windows the creation time, non-zero GNR4 session, and full executable
/// path are verified on the same process handle used for termination, closing
/// the PID-reuse/TOCTOU window.
pub fn terminate_daemon(identity_path: &Path, timeout: Duration) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            },
        };

        let identity: DaemonIdentity =
            serde_json::from_slice(&fs::read(identity_path)?).map_err(io::Error::other)?;
        let handle = open_windows_process(
            identity.pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
        )?;
        let snapshot = windows_process_snapshot_from_handle(handle.0)?;
        if !daemon_identity_matches(&identity, &snapshot) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon identity no longer matches the live process",
            ));
        }
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        match unsafe { WaitForSingleObject(handle.0, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "verified daemon did not exit before the deadline",
            )),
            _ => Err(io::Error::last_os_error()),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = timeout;
        let pid = read_daemon_pid(identity_path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "daemon identity is unavailable")
        })?;
        terminate_process(pid)
    }
}

#[cfg(test)]
mod tests {
    use crate::adb::{
        AdbError, AdbExecutor, AdbOutput, ACTION_START_V4, ACTION_STOP_V4, REVERSE_MAPPINGS,
    };
    #[cfg(unix)]
    use crate::protocol::{Frame, MessageType};

    use super::*;

    struct MonitorMockAdb {
        calls: StdMutex<Vec<Vec<String>>>,
        device_available: AtomicBool,
        mapping_adds: AtomicU64,
        mapping_delay_ms: AtomicU64,
        screen_suspended: AtomicBool,
        vpn_active: AtomicBool,
    }

    impl Default for MonitorMockAdb {
        fn default() -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                device_available: AtomicBool::new(true),
                mapping_adds: AtomicU64::new(0),
                mapping_delay_ms: AtomicU64::new(0),
                screen_suspended: AtomicBool::new(false),
                vpn_active: AtomicBool::new(true),
            }
        }
    }

    impl AdbExecutor for MonitorMockAdb {
        fn execute(&self, args: &[String], _timeout: Duration) -> Result<AdbOutput, AdbError> {
            self.calls.lock().unwrap().push(args.to_vec());
            if args.iter().any(|argument| argument == "get-state") {
                return Ok(AdbOutput::success(
                    if self.device_available.load(Ordering::Relaxed) {
                        "device\n"
                    } else {
                        "offline\n"
                    },
                ));
            }
            if args.iter().any(|argument| argument == "resolve-activity") {
                return Ok(AdbOutput::success(
                    "VirtualDesktop.Android/test.VirtualDesktopActivity\n",
                ));
            }
            if args.iter().any(|argument| argument == ACTION_STOP_V4) {
                self.vpn_active.store(false, Ordering::Relaxed);
                return Ok(AdbOutput::success("Stopping"));
            }
            if args.iter().any(|argument| argument == ACTION_START_V4) {
                self.vpn_active.store(true, Ordering::Relaxed);
                return Ok(AdbOutput::success("Starting"));
            }
            if args.iter().any(|argument| argument == "activity") {
                if !self.vpn_active.load(Ordering::Relaxed) {
                    return Ok(AdbOutput::success("No services match"));
                }
                return Ok(AdbOutput::success(format!(
                    "gnirehtet.state={}\nscreenSuspended={}\n",
                    if self.screen_suspended.load(Ordering::Relaxed) {
                        "DEGRADED"
                    } else {
                        "CONNECTED"
                    },
                    self.screen_suspended.load(Ordering::Relaxed),
                )));
            }
            if args.iter().any(|argument| argument == "--list") {
                if self.mapping_adds.load(Ordering::Relaxed) >= REVERSE_MAPPINGS.len() as u64 {
                    let mappings = REVERSE_MAPPINGS
                        .iter()
                        .map(|mapping| {
                            format!("UsbFfs tcp:{} tcp:{}\n", mapping.remote, mapping.local)
                        })
                        .collect::<String>();
                    return Ok(AdbOutput::success(mappings));
                }
                return Ok(AdbOutput::success(""));
            }
            if args.iter().any(|argument| argument == "--remove") {
                let _ =
                    self.mapping_adds
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                            Some(count.saturating_sub(1))
                        });
                return Ok(AdbOutput::success(""));
            }
            if args.get(1).is_some_and(|argument| argument == "reverse") {
                self.mapping_adds.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(
                    self.mapping_delay_ms.load(Ordering::Relaxed),
                ));
            }
            Ok(AdbOutput::success(""))
        }
    }

    #[test]
    fn status_store_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("status.json"));
        let snapshot = StateSnapshot {
            state: HostState::Degraded,
            session_id: Some("0011".into()),
            missed_heartbeats: 3,
            reason: Some("test".into()),
        };
        store.write(&snapshot, None).unwrap();
        assert_eq!(store.read().unwrap().lifecycle.state, HostState::Degraded);
    }

    #[test]
    fn new_daemon_cannot_inherit_stale_runtime_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("status.json"));
        let snapshot = StateSnapshot {
            state: HostState::Preparing,
            session_id: Some("new-session".into()),
            missed_heartbeats: 0,
            reason: None,
        };
        store.write_runtime_ready(&snapshot, 41).unwrap();
        assert!(store.read().unwrap().runtime_ready);

        store.write_runtime_not_ready(&snapshot, 42).unwrap();
        let status = store.read().unwrap();
        assert_eq!(status.daemon_pid, Some(42));
        assert!(!status.runtime_ready);
        store.write(&snapshot, Some(42)).unwrap();
        assert!(!store.read().unwrap().runtime_ready);
        store.write_runtime_ready(&snapshot, 42).unwrap();
        assert!(store.read().unwrap().runtime_ready);
    }

    #[test]
    fn tunnel_health_ignores_streamer_listener_but_requires_host_control() {
        let streamer = StreamerProbe::RunningNotListening;
        assert!(tunnel_layers_available(
            true, true, true, true, true, &streamer
        ));
        assert!(!tunnel_layers_available(
            false, true, true, true, true, &streamer
        ));
        assert!(!tunnel_layers_available(
            true, false, true, true, true, &streamer
        ));
    }

    #[test]
    fn operation_lock_serializes_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operation.lock");
        let first = OperationGuard::acquire(&path).unwrap();
        assert_eq!(
            OperationGuard::acquire(&path).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(first);
        OperationGuard::acquire(path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admin_endpoint_requires_token_and_returns_live_status() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        fs::write(&paths.admin_token, "correct").unwrap();
        let session = SessionId([0x55; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        let server = AdminServer::new(
            paths.clone(),
            "correct".into(),
            control.command_handle(),
            AdbHealthMonitor::default(),
            Arc::new(Notify::new()),
        );
        let task = tokio::spawn(server.serve());
        wait_for_unix_socket(&paths.admin_socket).await;

        let mut stream = UnixStream::connect(&paths.admin_socket).await.unwrap();
        write_admin_message(
            &mut stream,
            &AdminRequest {
                token: "wrong".into(),
                command: "status".into(),
            },
        )
        .await
        .unwrap();
        let response: AdminResponse = read_admin_message(&mut stream).await.unwrap();
        assert!(!response.ok);
        assert!(!response.repairs_suppressed);

        let response = admin_command(&paths, "status", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(response.ok);
        assert_eq!(response.status.unwrap().state, HostState::Preparing);
        task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admin_lane_bounds_idle_connections_and_releases_permits() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        fs::write(&paths.admin_token, "bounded").unwrap();
        let session = SessionId([0x66; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        let server = AdminServer::new(
            paths.clone(),
            "bounded".into(),
            control.command_handle(),
            AdbHealthMonitor::default(),
            Arc::new(Notify::new()),
        )
        .with_limits(1, Duration::from_millis(100));
        let task = tokio::spawn(server.serve());
        wait_for_unix_socket(&paths.admin_socket).await;

        let stalled = UnixStream::connect(&paths.admin_socket).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut rejected = UnixStream::connect(&paths.admin_socket).await.unwrap();
        let rejected_read = time::timeout(Duration::from_millis(50), rejected.read_u8())
            .await
            .expect("saturated connection should be closed promptly");
        assert!(rejected_read.is_err());

        tokio::time::sleep(Duration::from_millis(110)).await;
        let response = admin_command(&paths, "status", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(response.ok);

        drop(stalled);
        task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graceful_shutdown_notifies_only_after_stopped_state() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let token = SessionId([0x69; 16]).to_string();
        fs::write(&paths.admin_token, &token).unwrap();
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: SessionId([0x6a; 16]),
        })
        .unwrap();
        let state = control.state();
        let shutdown = Arc::new(Notify::new());
        let server = AdminServer::new(
            paths.clone(),
            token,
            control.command_handle(),
            AdbHealthMonitor::default(),
            shutdown.clone(),
        );
        let task = tokio::spawn(server.serve());
        wait_for_unix_socket(&paths.admin_socket).await;

        let rejected = admin_command(&paths, "shutdown", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(!rejected.ok);
        assert!(
            time::timeout(Duration::from_millis(20), shutdown.notified())
                .await
                .is_err()
        );

        {
            let mut state = state.lock().await;
            state.begin_stop().unwrap();
            state.stopped().unwrap();
        }
        let accepted = admin_command(&paths, "shutdown", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(accepted.ok);
        time::timeout(Duration::from_secs(1), shutdown.notified())
            .await
            .unwrap();
        task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admin_stop_reaches_android_before_delayed_repair_drain() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let token = SessionId([0x6c; 16]).to_string();
        fs::write(&paths.admin_token, &token).unwrap();
        let session = SessionId([0x6d; 16]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap();
        let control = ControlServer::new(ControlConfig {
            bind,
            session_id: session,
        })
        .unwrap();
        let state = control.state();
        let control_task = tokio::spawn(control.clone().serve_on(listener));
        let mut android = tokio::net::TcpStream::connect(bind).await.unwrap();
        Frame::new(MessageType::Hello, session, Vec::new())
            .write_to(&mut android)
            .await
            .unwrap();
        Frame::read_from(&mut android).await.unwrap();
        Frame::new(MessageType::Started, session, Vec::new())
            .write_to(&mut android)
            .await
            .unwrap();
        time::timeout(Duration::from_secs(1), async {
            while state.lock().await.state() != HostState::Connected {
                time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        let monitor = AdbHealthMonitor::default();
        let delayed_repair = monitor.operation.clone().lock_owned().await;
        let admin = AdminServer::new(
            paths.clone(),
            token,
            control.command_handle(),
            monitor,
            Arc::new(Notify::new()),
        );
        let admin_task = tokio::spawn(admin.serve());
        wait_for_unix_socket(&paths.admin_socket).await;
        let request_paths = paths.clone();
        let stop_request = tokio::spawn(async move {
            admin_command(&request_paths, "stop", Duration::from_secs(2)).await
        });

        let stop = time::timeout(Duration::from_millis(250), Frame::read_from(&mut android))
            .await
            .expect("STOP must not wait behind mapping repair")
            .unwrap();
        assert_eq!(stop.message_type, MessageType::Stop);
        Frame::new(MessageType::Stopped, session, Vec::new())
            .write_to(&mut android)
            .await
            .unwrap();
        time::sleep(Duration::from_millis(25)).await;
        assert!(!stop_request.is_finished());
        drop(delayed_repair);
        let response = stop_request.await.unwrap().unwrap();
        assert!(response.ok);
        assert!(response.repairs_suppressed);

        admin_task.abort();
        control_task.abort();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_named_pipe_survives_concurrent_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let token = SessionId([0x67; 16]).to_string();
        fs::write(&paths.admin_token, &token).unwrap();
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: SessionId([0x68; 16]),
        })
        .unwrap();
        let server = AdminServer::new(
            paths.clone(),
            token,
            control.command_handle(),
            AdbHealthMonitor::default(),
            Arc::new(Notify::new()),
        );
        let server_task = tokio::spawn(server.serve());

        time::timeout(Duration::from_secs(2), async {
            loop {
                if admin_command(&paths, "status", Duration::from_millis(250))
                    .await
                    .is_ok_and(|response| response.ok)
                {
                    break;
                }
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let mut clients = Vec::new();
        for _ in 0..(MAX_ADMIN_CONNECTIONS * 2) {
            let paths = paths.clone();
            clients.push(tokio::spawn(async move {
                admin_command(&paths, "status", Duration::from_secs(3)).await
            }));
        }
        let outcome = time::timeout(Duration::from_secs(10), async {
            for client in clients {
                let response = client
                    .await
                    .map_err(|error| format!("admin client task failed: {error}"))?
                    .map_err(|error| format!("admin client request failed: {error}"))?;
                if !response.ok {
                    return Err(format!("admin client was rejected: {:?}", response.error));
                }
            }
            Ok::<(), String>(())
        })
        .await;
        let server_running = !server_task.is_finished();
        server_task.abort();
        let _ = server_task.await;
        assert!(server_running);
        outcome
            .expect("concurrent named-pipe clients exceeded the test deadline")
            .unwrap();
    }

    #[cfg(unix)]
    async fn wait_for_unix_socket(path: &Path) {
        time::timeout(Duration::from_secs(1), async {
            while !path.exists() {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn invalid_persisted_admin_token_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("admin.token");
        fs::write(&path, "predictable").unwrap();
        let token = load_or_create_admin_token(&path).unwrap();
        assert_ne!(token, "predictable");
        assert!(token
            .parse::<SessionId>()
            .is_ok_and(|session| session != SessionId::ZERO));
    }

    #[test]
    fn token_comparison_handles_length_and_content_mismatches() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"samf"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }

    #[test]
    fn daemon_identity_rejects_pid_reuse_and_wrong_image_or_session() {
        let session = SessionId([0x44; 16]).to_string();
        let identity = DaemonIdentity {
            role: "gnirehtet-vd-daemon".into(),
            pid: 42,
            creation_time_100ns: 100,
            session_id: session,
            executable_path: r"C:\Program Files\Quest VD Wired\quest-vd-wired.exe".into(),
        };
        let valid = WindowsProcessSnapshot {
            active: true,
            creation_time_100ns: 100,
            executable_path: r"c:\program files\quest vd wired\QUEST-VD-WIRED.EXE".into(),
        };
        assert!(daemon_identity_matches(&identity, &valid));

        let mut reused = valid.clone();
        reused.creation_time_100ns += 1;
        assert!(!daemon_identity_matches(&identity, &reused));
        let mut wrong_image = valid.clone();
        wrong_image.executable_path = r"C:\Other\quest-vd-wired.exe".into();
        assert!(!daemon_identity_matches(&identity, &wrong_image));
        let mut zero_session = identity.clone();
        zero_session.session_id = SessionId::ZERO.to_string();
        assert!(!daemon_identity_matches(&zero_session, &valid));
    }

    #[test]
    fn named_pipe_dacl_grants_only_the_current_user_sid() {
        assert_eq!(
            user_only_pipe_sddl("S-1-5-21-1-2-3-1001"),
            "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
    }

    #[test]
    fn adb_health_backoff_caps_at_five_seconds() {
        let maximum = Duration::from_secs(5);
        let first = next_backoff(Duration::from_millis(250), maximum);
        let second = next_backoff(first, maximum);
        let third = next_backoff(second, maximum);
        let fourth = next_backoff(third, maximum);
        let fifth = next_backoff(fourth, maximum);
        assert_eq!(first, Duration::from_millis(500));
        assert_eq!(second, Duration::from_secs(1));
        assert_eq!(third, Duration::from_secs(2));
        assert_eq!(fourth, Duration::from_secs(4));
        assert_eq!(fifth, maximum);
    }

    #[test]
    fn stale_unavailable_cache_does_not_erase_fresh_wake_authentication() {
        for cached in [None, Some(false)] {
            let relay_gate = RelayGate::default();
            let relay_controller = RelayGateController::new(relay_gate.clone());

            relay_controller.carrier_lost();
            relay_controller.authenticated_wake();
            relay_controller.control_connected();
            relay_controller.carrier_healthy();
            assert!(relay_gate.is_enabled());

            // This is the predicate used by both timer and lifecycle wakeups.
            // Entering it for a stale false would erase the fresh control auth.
            if cached_device_is_available(cached) {
                relay_controller.carrier_lost();
            }

            assert!(
                relay_gate.is_enabled(),
                "stale cache was replayed: {cached:?}"
            );
        }
        assert!(cached_device_is_available(Some(true)));
    }

    #[test]
    fn virtual_desktop_watchdog_arms_only_after_a_stable_connection() {
        let mut watchdog = VirtualDesktopWatchdogState::default();
        let started = time::Instant::now();

        assert!(!watchdog.observe(started, false, true));
        assert!(!watchdog.observe(started + Duration::from_secs(30), false, true));
        assert!(!watchdog.observe(started + Duration::from_secs(31), true, true));
        assert!(!watchdog.observe(
            started + Duration::from_secs(31) + VIRTUAL_DESKTOP_CONNECTION_STABLE,
            true,
            true,
        ));
        assert!(watchdog.armed);
    }

    #[test]
    fn virtual_desktop_watchdog_waits_for_loss_grace_and_recovery_eligibility() {
        let mut watchdog = VirtualDesktopWatchdogState::default();
        let started = time::Instant::now();
        watchdog.observe(started, true, true);
        watchdog.observe(started + VIRTUAL_DESKTOP_CONNECTION_STABLE, true, true);
        let lost = started + VIRTUAL_DESKTOP_CONNECTION_STABLE + Duration::from_secs(1);

        assert!(!watchdog.observe(lost, false, true));
        assert!(!watchdog.observe(lost + VIRTUAL_DESKTOP_WATCHDOG_LOSS_GRACE, false, false,));
        assert!(watchdog.observe(
            lost + VIRTUAL_DESKTOP_WATCHDOG_LOSS_GRACE + Duration::from_millis(1),
            false,
            true,
        ));
    }

    #[test]
    fn healthy_timer_avoids_repeated_adb_transport_probes() {
        assert!(!should_reconcile_on_healthy_tick(false, Some(true), true));
        assert!(!should_reconcile_on_healthy_tick(false, Some(false), false));
        assert!(!should_reconcile_on_healthy_tick(false, None, false));
        assert!(should_reconcile_on_healthy_tick(false, Some(true), false));
        assert!(should_reconcile_on_healthy_tick(true, Some(true), true));
    }

    #[test]
    fn authenticated_wake_rotates_stale_flows_before_immediate_reenable() {
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());

        relay_controller.control_connected();
        let connected_generation = relay_gate.generation();
        assert!(relay_gate.is_enabled());

        relay_controller.authenticated_wake();
        assert!(!relay_gate.is_enabled());
        assert!(relay_gate.generation() > connected_generation);
        let suspended_generation = relay_gate.generation();

        relay_controller.control_connected();
        assert!(relay_gate.is_enabled());
        assert!(relay_gate.generation() > suspended_generation);
    }

    #[test]
    fn host_resume_recovery_is_bounded_to_two_attempts() {
        let monitor = AdbHealthMonitor::default();
        let generation = monitor.notify_host_resume();
        assert_eq!(generation, 1);
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((1, 1, TransportRecoveryTrigger::HostResume))
        );
        assert!(monitor.finish_transport_recovery_attempt(1, false));
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((1, 2, TransportRecoveryTrigger::HostResume))
        );
        assert!(!monitor.finish_transport_recovery_attempt(1, false));
        assert_eq!(monitor.begin_transport_recovery_attempt(), None);
        assert_eq!(
            monitor.snapshot().transport_recovery.state,
            TransportRecoveryState::Failed
        );
    }

    #[test]
    fn terminal_recovery_rearms_exactly_once_per_external_generation() {
        let monitor = AdbHealthMonitor::default();
        let generation = monitor.notify_host_resume();
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((generation, 1, TransportRecoveryTrigger::HostResume))
        );
        assert!(monitor.finish_transport_recovery_attempt(generation, false));
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((generation, 2, TransportRecoveryTrigger::HostResume))
        );
        assert!(!monitor.finish_transport_recovery_attempt(generation, false));

        let fallback_generation = monitor
            .rearm_failed_transport(generation, TransportRecoveryTrigger::HostResume)
            .unwrap();
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.transport_recovery.generation, fallback_generation);
        assert_eq!(snapshot.transport_recovery.attempt, 0);
        assert_eq!(
            snapshot.transport_recovery.state,
            TransportRecoveryState::Pending
        );
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("transport_recovery_rearmed")
        );

        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((fallback_generation, 1, TransportRecoveryTrigger::HostResume,))
        );
        assert!(monitor.finish_transport_recovery_attempt(fallback_generation, false));
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((fallback_generation, 2, TransportRecoveryTrigger::HostResume,))
        );
        assert!(!monitor.finish_transport_recovery_attempt(fallback_generation, false));
        assert_eq!(
            monitor
                .rearm_failed_transport(fallback_generation, TransportRecoveryTrigger::HostResume,),
            None
        );

        let next_external_generation = monitor.notify_host_resume();
        assert!(next_external_generation > fallback_generation);
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((
                next_external_generation,
                1,
                TransportRecoveryTrigger::HostResume,
            ))
        );
    }

    #[tokio::test]
    async fn terminal_recovery_scheduler_rearms_once_after_its_delay() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let monitor = AdbHealthMonitor::default();
        let generation = monitor.notify_host_resume();
        monitor.begin_transport_recovery_attempt().unwrap();
        assert!(monitor.finish_transport_recovery_attempt(generation, false));
        monitor.begin_transport_recovery_attempt().unwrap();
        assert!(!monitor.finish_transport_recovery_attempt(generation, false));

        for _ in 0..2 {
            monitor.schedule_transport_recovery_rearm_after(
                generation,
                TransportRecoveryTrigger::HostResume,
                &diagnostics,
                Duration::from_millis(10),
            );
        }
        time::sleep(Duration::from_millis(50)).await;

        let fallback = monitor.snapshot().transport_recovery;
        assert_eq!(fallback.generation, generation + 1);
        assert_eq!(fallback.attempt, 0);
        assert_eq!(fallback.state, TransportRecoveryState::Pending);
        assert!(monitor.transport_rearm_scheduled.lock().unwrap().is_empty());

        monitor.begin_transport_recovery_attempt().unwrap();
        assert!(monitor.finish_transport_recovery_attempt(fallback.generation, false));
        monitor.begin_transport_recovery_attempt().unwrap();
        assert!(!monitor.finish_transport_recovery_attempt(fallback.generation, false));
        monitor.schedule_transport_recovery_rearm_after(
            fallback.generation,
            TransportRecoveryTrigger::HostResume,
            &diagnostics,
            Duration::from_millis(10),
        );
        time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            monitor.snapshot().transport_recovery.state,
            TransportRecoveryState::Failed
        );
    }

    #[test]
    fn transport_recovery_resets_the_virtual_desktop_service_before_the_first_relaunch() {
        assert!(should_reset_virtual_desktop_service(
            TransportRecoveryTrigger::HostResume,
            1,
        ));
        assert!(should_reset_virtual_desktop_service(
            TransportRecoveryTrigger::UsbReconnect,
            1,
        ));
        assert!(should_reset_virtual_desktop_service(
            TransportRecoveryTrigger::UsbReconnect,
            2,
        ));
    }

    #[tokio::test]
    async fn host_resume_rebuilds_mappings_before_waiting_for_fresh_control() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x75; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let executor = Arc::new(MonitorMockAdb::default());
        executor
            .mapping_adds
            .store(REVERSE_MAPPINGS.len() as u64, Ordering::Relaxed);
        let config = AdbMonitorConfig {
            adb: AdbController::new(executor.clone()),
            adb_program: PathBuf::new(),
            session_id: session,
            all_traffic: true,
            virtual_desktop_flows: VirtualDesktopFlowMonitor::default(),
        };
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();
        assert!(relay_gate.is_enabled());

        monitor.notify_host_resume();
        assert!(
            monitor
                .recover_transport(
                    &config,
                    &control.command_handle(),
                    &store,
                    &diagnostics,
                    &relay_controller,
                    true,
                )
                .await
        );

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.reconnect_generation, 1);
        assert!(snapshot.mappings_healthy);
        assert_eq!(
            snapshot.transport_recovery.state,
            TransportRecoveryState::AwaitingControl
        );
        assert!(!relay_gate.is_enabled());
        {
            let calls = executor.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|args| args.iter().any(|argument| argument == "--remove"))
                    .count(),
                REVERSE_MAPPINGS.len()
            );
            assert!(!calls.iter().any(|args| {
                args.windows(2)
                    .any(|arguments| arguments == ["force-stop", "VirtualDesktop.Android"])
            }));
        }

        relay_controller.control_connected();
        monitor.notify_authenticated_connection();
        assert!(
            monitor
                .complete_transport_recovery(&config, &diagnostics)
                .await
        );
        assert_eq!(
            monitor.snapshot().transport_recovery.state,
            TransportRecoveryState::Recovered
        );
        assert!(executor.calls.lock().unwrap().iter().any(|args| {
            args.windows(2)
                .any(|arguments| arguments == ["force-stop", "VirtualDesktop.Android"])
        }));
        assert!(relay_gate.is_enabled());
    }

    #[tokio::test]
    async fn usb_reconnect_preserves_android_session_and_restarts_vd_after_fresh_control() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x76; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let executor = Arc::new(MonitorMockAdb::default());
        executor
            .mapping_adds
            .store(REVERSE_MAPPINGS.len() as u64, Ordering::Relaxed);
        let config = AdbMonitorConfig {
            adb: AdbController::new(executor.clone()),
            adb_program: PathBuf::new(),
            session_id: session,
            all_traffic: false,
            virtual_desktop_flows: VirtualDesktopFlowMonitor::default(),
        };
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();

        monitor.note_usb_disconnected();
        assert!(monitor.notify_usb_reconnected_if_needed());
        assert!(
            monitor
                .recover_transport(
                    &config,
                    &control.command_handle(),
                    &store,
                    &diagnostics,
                    &relay_controller,
                    true,
                )
                .await
        );

        let snapshot = monitor.snapshot();
        assert_eq!(
            snapshot.transport_recovery.trigger,
            Some(TransportRecoveryTrigger::UsbReconnect)
        );
        assert_eq!(
            snapshot.transport_recovery.state,
            TransportRecoveryState::AwaitingControl
        );
        assert!(!relay_gate.is_enabled());
        {
            let calls = executor.calls.lock().unwrap();
            assert!(!calls.iter().any(|args| args
                .iter()
                .any(|argument| argument == ACTION_STOP_V4 || argument == ACTION_START_V4)));
            assert!(!calls.iter().any(|args| {
                args.windows(2)
                    .any(|arguments| arguments == ["force-stop", "VirtualDesktop.Android"])
            }));
            assert_eq!(
                calls
                    .iter()
                    .filter(|args| args.iter().any(|argument| argument == "--remove"))
                    .count(),
                REVERSE_MAPPINGS.len()
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|args| {
                        args.get(1).is_some_and(|argument| argument == "reverse")
                            && !args.iter().any(|argument| argument == "--list")
                            && !args.iter().any(|argument| argument == "--remove")
                    })
                    .count(),
                REVERSE_MAPPINGS.len()
            );
        }

        relay_controller.control_connected();
        monitor.notify_authenticated_connection();
        assert!(
            monitor
                .complete_transport_recovery(&config, &diagnostics)
                .await
        );
        assert_eq!(
            monitor.snapshot().transport_recovery.state,
            TransportRecoveryState::Recovered
        );
        let calls = executor.calls.lock().unwrap();
        let vd_stop = calls
            .iter()
            .position(|args| {
                args.windows(2)
                    .any(|arguments| arguments == ["force-stop", "VirtualDesktop.Android"])
            })
            .unwrap();
        let vd_start = calls
            .iter()
            .position(|args| {
                args.windows(2).any(|arguments| {
                    arguments == ["-n", "VirtualDesktop.Android/test.VirtualDesktopActivity"]
                })
            })
            .unwrap();
        assert!(vd_stop < vd_start);
        assert!(relay_gate.is_enabled());
    }

    #[test]
    fn usb_reconnect_control_timeout_retries_once_then_fails() {
        let monitor = AdbHealthMonitor::default();
        monitor.note_usb_disconnected();
        assert!(monitor.notify_usb_reconnected_if_needed());
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((1, 1, TransportRecoveryTrigger::UsbReconnect))
        );
        assert!(!monitor.finish_transport_recovery_attempt(1, true));
        assert_eq!(monitor.expire_transport_recovery_control(1, 1), Some(true));
        assert_eq!(
            monitor.begin_transport_recovery_attempt(),
            Some((1, 2, TransportRecoveryTrigger::UsbReconnect))
        );
        assert!(!monitor.finish_transport_recovery_attempt(1, true));
        assert_eq!(monitor.expire_transport_recovery_control(1, 2), Some(false));
        assert_eq!(
            monitor.snapshot().transport_recovery.state,
            TransportRecoveryState::Failed
        );
    }

    #[tokio::test]
    async fn transient_degradation_keeps_the_relay_generation_alive() {
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::with_control_loss_grace(
            relay_gate.clone(),
            Duration::from_millis(30),
        );
        let session = SessionId([0x79; 16]).to_string();
        let snapshot = |state| StateSnapshot {
            state,
            session_id: Some(session.clone()),
            missed_heartbeats: 0,
            reason: (state == HostState::Degraded)
                .then(|| CONTROL_TRANSPORT_LOST_REASON.to_owned()),
        };

        update_relay_gate(&relay_controller, &snapshot(HostState::Connected));
        let connected_generation = relay_gate.generation();
        assert!(relay_gate.is_enabled());

        update_relay_gate(&relay_controller, &snapshot(HostState::Degraded));
        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), connected_generation);

        update_relay_gate(&relay_controller, &snapshot(HostState::Connected));
        time::sleep(Duration::from_millis(50)).await;
        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), connected_generation);

        update_relay_gate(&relay_controller, &snapshot(HostState::Stopping));
        assert!(!relay_gate.is_enabled());
        assert!(relay_gate.generation() > connected_generation);
    }

    #[tokio::test]
    async fn heartbeat_only_degradation_never_resets_healthy_flows() {
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::with_control_loss_grace(
            relay_gate.clone(),
            Duration::from_millis(20),
        );
        let session = SessionId([0x7a; 16]).to_string();
        update_relay_gate(
            &relay_controller,
            &StateSnapshot {
                state: HostState::Connected,
                session_id: Some(session.clone()),
                missed_heartbeats: 0,
                reason: None,
            },
        );
        let connected_generation = relay_gate.generation();
        update_relay_gate(
            &relay_controller,
            &StateSnapshot {
                state: HostState::Degraded,
                session_id: Some(session),
                missed_heartbeats: 3,
                reason: Some("three heartbeat intervals missed; VPN remains active".into()),
            },
        );

        time::sleep(Duration::from_millis(40)).await;
        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), connected_generation);
    }

    #[tokio::test]
    async fn sustained_control_loss_invalidates_flows_until_fresh_started() {
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::with_control_loss_grace(
            relay_gate.clone(),
            Duration::from_millis(20),
        );
        let session = SessionId([0x7b; 16]).to_string();
        let snapshot = |state| StateSnapshot {
            state,
            session_id: Some(session.clone()),
            missed_heartbeats: 0,
            reason: (state == HostState::Degraded)
                .then(|| CONTROL_TRANSPORT_LOST_REASON.to_owned()),
        };

        update_relay_gate(&relay_controller, &snapshot(HostState::Connected));
        let connected_generation = relay_gate.generation();
        update_relay_gate(&relay_controller, &snapshot(HostState::Degraded));
        assert!(relay_gate.is_enabled());

        time::timeout(Duration::from_secs(1), async {
            while relay_gate.is_enabled() {
                time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("sustained control loss did not close relay flows");
        assert!(!relay_gate.is_enabled());
        assert!(relay_gate.generation() > connected_generation);

        update_relay_gate(&relay_controller, &snapshot(HostState::Connected));
        assert!(relay_gate.is_enabled());
    }

    #[tokio::test]
    async fn wake_token_cancels_stale_timer_and_sleep_probe() {
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::with_control_loss_grace(
            relay_gate.clone(),
            Duration::from_millis(30),
        );

        relay_controller.control_connected();
        relay_controller.control_degraded();
        let stale_epoch = relay_controller.pending_control_epoch().unwrap();
        relay_controller.control_connected();
        let wake_generation = relay_gate.generation();

        assert_eq!(relay_controller.pending_control_epoch(), None);
        assert!(!relay_controller.control_inactive_if_epoch(stale_epoch));
        time::sleep(Duration::from_millis(50)).await;
        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), wake_generation);
    }

    #[tokio::test]
    async fn stale_degraded_reconcile_cannot_probe_reconnected_generation() {
        let directory = tempfile::tempdir().unwrap();
        let diagnostics = Diagnostics::open(directory.path()).unwrap();
        let executor = Arc::new(MonitorMockAdb::default());
        executor.screen_suspended.store(true, Ordering::Relaxed);
        let adb = AdbController::new(executor.clone());
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());

        relay_controller.control_connected();
        relay_controller.control_degraded();
        relay_controller.control_connected();
        let wake_generation = relay_gate.generation();
        monitor
            .reconcile_android_sleep(&adb, &diagnostics, &relay_controller)
            .await;

        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), wake_generation);
        assert!(!executor
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|args| args.iter().any(|argument| argument == "activity")));
    }

    #[tokio::test]
    async fn repeated_degradation_does_not_extend_the_cleanup_deadline() {
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::with_control_loss_grace(
            relay_gate.clone(),
            Duration::from_millis(60),
        );

        relay_controller.control_connected();
        relay_controller.control_degraded();
        time::sleep(Duration::from_millis(40)).await;
        relay_controller.control_degraded();
        time::sleep(Duration::from_millis(35)).await;

        assert!(!relay_gate.is_enabled());
    }

    #[tokio::test]
    async fn confirmed_android_sleep_closes_flows_when_control_is_degraded() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7d; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        control
            .command_handle()
            .transport_lost("test control loss")
            .await;
        let executor = Arc::new(MonitorMockAdb::default());
        executor
            .mapping_adds
            .store(REVERSE_MAPPINGS.len() as u64, Ordering::Relaxed);
        executor.screen_suspended.store(true, Ordering::Relaxed);
        let adb = AdbController::new(executor.clone());
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();
        relay_controller.control_degraded();

        monitor
            .reconcile(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                &relay_controller,
                true,
            )
            .await;

        assert!(!relay_gate.is_enabled());
        monitor
            .reconcile(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                &relay_controller,
                true,
            )
            .await;
        assert_eq!(
            executor
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|args| args.iter().any(|argument| argument == "activity"))
                .count(),
            1,
        );
    }

    #[tokio::test]
    async fn unconfirmed_control_degradation_keeps_flows_alive_during_grace() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7e; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        control
            .command_handle()
            .transport_lost("test control loss")
            .await;
        let executor = Arc::new(MonitorMockAdb::default());
        executor
            .mapping_adds
            .store(REVERSE_MAPPINGS.len() as u64, Ordering::Relaxed);
        let adb = AdbController::new(executor);
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();
        relay_controller.control_degraded();
        let connected_generation = relay_gate.generation();

        monitor
            .reconcile(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                &relay_controller,
                true,
            )
            .await;

        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), connected_generation);
    }

    #[tokio::test]
    async fn adb_monitor_repairs_all_three_lanes_and_counts_generation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x77; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let executor = Arc::new(MonitorMockAdb::default());
        let adb = AdbController::with_timing(
            executor.clone(),
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::ZERO,
        );
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();
        monitor
            .reconcile(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                &relay_controller,
                true,
            )
            .await;

        assert_eq!(monitor.snapshot().reconnect_generation, 1);
        assert!(monitor.snapshot().mappings_healthy);
        assert!(relay_gate.is_enabled());
        let calls = executor.calls.lock().unwrap();
        for mapping in REVERSE_MAPPINGS {
            assert!(calls.iter().any(|args| {
                args.iter()
                    .any(|argument| argument == &format!("tcp:{}", mapping.remote))
                    && !args.iter().any(|argument| argument == "--remove")
            }));
        }
        assert!(!calls
            .iter()
            .any(|args| args.iter().any(|argument| argument == "get-state")));
    }

    #[tokio::test]
    async fn adb_monitor_never_repairs_after_explicit_suppression() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x78; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let executor = Arc::new(MonitorMockAdb::default());
        let adb = AdbController::new(executor.clone());
        let monitor = AdbHealthMonitor::default();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate);
        monitor.suppress_repairs().await;
        monitor
            .reconcile(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                &relay_controller,
                true,
            )
            .await;

        assert!(executor.calls.lock().unwrap().is_empty());
        assert!(monitor.snapshot().repair_suppressed);
    }

    #[tokio::test]
    async fn explicit_stop_preempts_multi_mapping_repair_between_commands() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7b; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let executor = Arc::new(MonitorMockAdb::default());
        executor.mapping_delay_ms.store(75, Ordering::Relaxed);
        let adb = AdbController::with_timing(
            executor.clone(),
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::ZERO,
        );
        let monitor = AdbHealthMonitor::default();
        let reconcile_monitor = monitor.clone();
        let reconcile_control = control.command_handle();
        let reconcile_store = store.clone();
        let reconcile_diagnostics = diagnostics.clone();
        let reconcile_adb = adb.clone();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();
        let reconcile_relay_gate = relay_controller.clone();
        let repair = tokio::spawn(async move {
            reconcile_monitor
                .reconcile(
                    &reconcile_adb,
                    &reconcile_control,
                    &reconcile_store,
                    &reconcile_diagnostics,
                    &reconcile_relay_gate,
                    true,
                )
                .await;
        });
        time::timeout(Duration::from_secs(1), async {
            while executor.mapping_adds.load(Ordering::Relaxed) == 0 {
                time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        let started = std::time::Instant::now();
        monitor.suppress_repairs().await;
        repair.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(executor.mapping_adds.load(Ordering::Relaxed), 1);
        assert!(monitor.snapshot().repair_suppressed);
        assert!(relay_gate.is_enabled());
    }

    #[tokio::test]
    async fn confirmed_device_loss_invalidates_active_relay_flows() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7c; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let relay_gate = RelayGate::default();
        let relay_controller = RelayGateController::new(relay_gate.clone());
        relay_controller.control_connected();
        let connected_generation = relay_gate.generation();
        let monitor = AdbHealthMonitor::default();
        let adb = AdbController::new(Arc::new(MonitorMockAdb::default()));

        monitor
            .reconcile(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                &relay_controller,
                false,
            )
            .await;

        assert!(!relay_gate.is_enabled());
        assert!(relay_gate.generation() > connected_generation);
        assert_eq!(control.state().lock().await.state(), HostState::Degraded);
        assert_eq!(
            monitor.snapshot().last_error.as_deref(),
            Some("device_unavailable")
        );
        update_relay_gate(
            &relay_controller,
            &StateSnapshot {
                state: HostState::Connected,
                session_id: Some(session.to_string()),
                missed_heartbeats: 0,
                reason: None,
            },
        );
        assert!(!relay_gate.is_enabled());
        relay_controller.carrier_healthy();
        assert!(relay_gate.is_enabled());
    }

    #[test]
    fn track_devices_decoder_handles_every_frame_split_and_empty_snapshot() {
        let connected = b"0016private-serial\tdevice\n";
        let stream = [connected.as_slice(), b"0000"].concat();
        for split in 0..=stream.len() {
            let mut decoder = TrackDevicesDecoder::default();
            let mut updates = decoder.push(&stream[..split]).unwrap();
            updates.extend(decoder.push(&stream[split..]).unwrap());
            assert_eq!(updates, vec![true, false], "split at {split}");
        }
        let mut decoder = TrackDevicesDecoder::default();
        assert_eq!(decoder.push(&stream).unwrap(), vec![true, false]);
        let mut decoder = TrackDevicesDecoder::default();
        let mut updates = Vec::new();
        for byte in &stream {
            updates.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(updates, vec![true, false]);
    }

    #[test]
    fn track_devices_decoder_supports_headered_and_headerless_text_fallback() {
        let mut decoder = TrackDevicesDecoder::default();
        let first = b"List of devices attached\r\nprivate-serial\tdevice product:quest\r\n\r\n";
        let mut updates = decoder.push(&first[..7]).unwrap();
        updates.extend(decoder.push(&first[7..]).unwrap());
        assert_eq!(updates, vec![true]);

        let mut decoder = TrackDevicesDecoder::default();
        assert_eq!(
            decoder
                .push(b"private-serial\tunauthorized\n\nprivate-serial\tdevice\n\n")
                .unwrap(),
            vec![false, true]
        );
    }

    #[test]
    fn malformed_or_oversized_framed_track_output_fails_closed() {
        let mut malformed = TrackDevicesDecoder::default();
        assert_eq!(malformed.push(b"0000").unwrap(), vec![false]);
        assert_eq!(
            malformed.push(b"00g1"),
            Err(TrackDevicesDecodeError::InvalidLength)
        );
        let mut oversized = TrackDevicesDecoder::default();
        assert_eq!(
            oversized.push(b"ffff"),
            Err(TrackDevicesDecodeError::Oversized(65_535))
        );
        let mut invalid_utf8 = TrackDevicesDecoder::default();
        assert_eq!(
            invalid_utf8.push(b"0001\xff"),
            Err(TrackDevicesDecodeError::InvalidUtf8)
        );
    }

    #[tokio::test]
    async fn tracker_decode_failure_preserves_a_confirmed_carrier() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7a; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let monitor = AdbHealthMonitor::default();
        let adb = AdbController::new(Arc::new(MonitorMockAdb::default()));
        let relay_gate = RelayGate::default();
        relay_gate.set_enabled(true);
        let connected_generation = relay_gate.generation();
        monitor.update_status(true, true, true, None);
        monitor
            .track_failed(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                "track_decode_failed",
            )
            .await;
        let snapshot = monitor.snapshot();
        assert!(!snapshot.active);
        assert!(snapshot.device_available);
        assert!(snapshot.mappings_healthy);
        assert!(relay_gate.is_enabled());
        assert_eq!(relay_gate.generation(), connected_generation);
        assert_eq!(control.state().lock().await.state(), HostState::Connected);
        assert!(!monitor.usb_reconnect_pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn tracker_decode_failure_clears_an_unconfirmed_carrier() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7b; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let monitor = AdbHealthMonitor::default();
        let mock = Arc::new(MonitorMockAdb::default());
        mock.device_available.store(false, Ordering::Relaxed);
        let adb = AdbController::new(mock);
        monitor.update_status(true, true, true, None);
        monitor
            .track_failed(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                "track_decode_failed",
            )
            .await;

        let snapshot = monitor.snapshot();
        assert!(!snapshot.active);
        assert!(!snapshot.device_available);
        assert!(!snapshot.mappings_healthy);
        assert_eq!(control.state().lock().await.state(), HostState::Degraded);
        assert!(monitor.usb_reconnect_pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn tracker_transport_failure_remains_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let diagnostics = Diagnostics::open(&paths.logs).unwrap();
        let store = StateStore::new(&paths.status);
        let session = SessionId([0x7c; 16]);
        let control = ControlServer::new(ControlConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            session_id: session,
        })
        .unwrap();
        control
            .state()
            .lock()
            .await
            .peer_started(session, std::time::Instant::now())
            .unwrap();
        let monitor = AdbHealthMonitor::default();
        let adb = AdbController::new(Arc::new(MonitorMockAdb::default()));
        monitor.update_status(true, true, true, None);
        monitor
            .track_failed(
                &adb,
                &control.command_handle(),
                &store,
                &diagnostics,
                "track_read_failed",
            )
            .await;

        let snapshot = monitor.snapshot();
        assert!(!snapshot.active);
        assert!(!snapshot.device_available);
        assert!(!snapshot.mappings_healthy);
        assert_eq!(control.state().lock().await.state(), HostState::Degraded);
        assert!(monitor.usb_reconnect_pending.load(Ordering::Acquire));
    }
}
