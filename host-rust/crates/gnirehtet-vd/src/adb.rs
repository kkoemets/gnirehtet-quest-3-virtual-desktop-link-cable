use std::{
    fmt,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TryRecvError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "windows")]
use std::fs;
#[cfg(target_os = "windows")]
use std::sync::{Mutex, OnceLock};

use crate::protocol::SessionId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use wait_timeout::ChildExt;

pub const ANDROID_PACKAGE: &str = "com.genymobile.gnirehtet";
pub const ANDROID_CONTROL_SERVICE: &str = "com.genymobile.gnirehtet/.v4.AdbControlService";
pub const ANDROID_VPN_SERVICE: &str = "com.genymobile.gnirehtet/.v4.VdLinkVpnService";
pub const ACTION_START_V4: &str = "com.genymobile.gnirehtet.v4.START";
pub const ACTION_STOP_V4: &str = "com.genymobile.gnirehtet.v4.STOP";
pub const VIRTUAL_DESKTOP_PACKAGE: &str = "VirtualDesktop.Android";
pub const ANDROID_VERSION_CODE: &str = "65";
pub const ANDROID_VERSION_NAME: &str = "4.1.20";
pub const PLATFORM_TOOLS_VERSION: &str = "37.0.0";
pub const PLATFORM_TOOLS_WINDOWS_URL: &str =
    "https://dl.google.com/android/repository/platform-tools_r37.0.0-win.zip";
pub const PLATFORM_TOOLS_WINDOWS_SHA256: &str =
    "4fe305812db074cea32903a489d061eb4454cbc90a49e8fea677f4b7af764918";
pub const SOCKS_PORT: u16 = 31_416;
pub const CONTROL_PORT: u16 = 31_417;
pub const UDP_STREAM_PORT: u16 = 31_418;
const ANDROID_KEYCODE_WAKEUP: &str = "224";
pub const ADB_DEVICE_COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
pub const ADB_MAPPING_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const ADB_INSTALL_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_ADB_OUTPUT_BYTES: usize = 1024 * 1024;
const OUTPUT_TRUNCATED_MARKER: &[u8] = b"\n[ADB output truncated]\n";
const OUTPUT_CHANNEL_CAPACITY: usize = 16;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CHILD_TERMINATION_GRACE: Duration = Duration::from_millis(100);
const OUTPUT_EVENT_DRAIN_BUDGET: usize = 64;
#[cfg(target_os = "windows")]
const PLATFORM_TOOLS_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReverseMapping {
    pub remote: u16,
    pub local: u16,
}

pub const REVERSE_MAPPINGS: [ReverseMapping; 3] = [
    ReverseMapping {
        remote: SOCKS_PORT,
        local: SOCKS_PORT,
    },
    // HEV's FWD UDP extension gets a dedicated ADB stream so TCP proxy
    // traffic cannot starve latency-sensitive datagrams at the host acceptor.
    ReverseMapping {
        remote: UDP_STREAM_PORT,
        local: UDP_STREAM_PORT,
    },
    // Install control last so a fresh authenticated wake proves both data
    // lanes were restored first after a full ADB reverse reset.
    ReverseMapping {
        remote: CONTROL_PORT,
        local: CONTROL_PORT,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdbOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl AdbOutput {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            status: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

pub trait AdbExecutor: Send + Sync {
    fn execute(&self, args: &[String], timeout: Duration) -> Result<AdbOutput, AdbError>;

    fn recover_server(&self, _timeout: Duration) -> Result<(), AdbError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SystemAdb {
    program: PathBuf,
}

impl SystemAdb {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    fn execute_once(&self, args: &[String], timeout: Duration) -> Result<AdbOutput, AdbError> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_subprocess_window(&mut command);
        let child = command.spawn().map_err(|source| AdbError::Spawn {
            program: self.program.clone(),
            source,
        })?;
        collect_child_output(child, timeout)
    }
}

fn hide_subprocess_window(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(target_os = "windows"))]
    let _ = command;
}

impl AdbExecutor for SystemAdb {
    fn execute(&self, args: &[String], timeout: Duration) -> Result<AdbOutput, AdbError> {
        self.execute_once(args, timeout)
    }

    fn recover_server(&self, timeout: Duration) -> Result<(), AdbError> {
        #[cfg(target_os = "windows")]
        {
            static RECOVERY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            let _recovery = RECOVERY_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .map_err(|_| AdbError::Read("ADB server recovery lock is poisoned".into()))?;
            let mut command = Command::new("taskkill.exe");
            command
                .args(["/F", "/IM", "adb.exe"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            hide_subprocess_window(&mut command);
            if let Ok(child) = command.spawn() {
                let _ = collect_child_output(child, Duration::from_secs(5));
            }
            thread::sleep(Duration::from_millis(250));
        }
        let _ = timeout;
        Ok(())
    }
}

fn collect_child_output(mut child: Child, timeout: Duration) -> Result<AdbOutput, AdbError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut containment = match CommandContainment::assign(&child) {
        Ok(containment) => containment,
        Err(error) => {
            terminate_direct_child(&mut child);
            return Err(AdbError::Containment(error));
        }
    };
    let mut stdout = child.stdout.take().ok_or_else(|| {
        terminate_child_tree(&mut child, &mut containment, None);
        AdbError::Read("ADB stdout was not captured".into())
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        terminate_child_tree(&mut child, &mut containment, None);
        AdbError::Read("ADB stderr was not captured".into())
    })?;

    let (sender, receiver) = mpsc::sync_channel(OUTPUT_CHANNEL_CAPACITY);
    let mut output_readers = OutputReaders::default();
    let stdout_sender = sender.clone();
    let stdout_cancelled = output_readers.cancelled.clone();
    let stdout_reader = thread::Builder::new()
        .name("adb-stdout".into())
        .spawn(move || {
            stream_output(
                &mut stdout,
                OutputStream::Stdout,
                stdout_sender,
                &stdout_cancelled,
            )
        })
        .map_err(|error| {
            terminate_child_tree(&mut child, &mut containment, None);
            AdbError::Read(format!("could not start ADB stdout reader: {error}"))
        })?;
    output_readers.handles.push(stdout_reader);
    let stderr_sender = sender.clone();
    let stderr_cancelled = output_readers.cancelled.clone();
    let stderr_reader = thread::Builder::new()
        .name("adb-stderr".into())
        .spawn(move || {
            stream_output(
                &mut stderr,
                OutputStream::Stderr,
                stderr_sender,
                &stderr_cancelled,
            )
        })
        .map_err(|error| {
            terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
            AdbError::Read(format!("could not start ADB stderr reader: {error}"))
        })?;
    output_readers.handles.push(stderr_reader);
    drop(sender);

    let mut status = None;
    let mut stdout = OutputAccumulator::default();
    let mut stderr = OutputAccumulator::default();
    let mut readers_done = [false; 2];

    loop {
        for _ in 0..OUTPUT_EVENT_DRAIN_BUDGET {
            match receiver.try_recv() {
                Ok(event) => {
                    if let Err(error) =
                        apply_output_event(event, &mut stdout, &mut stderr, &mut readers_done)
                    {
                        terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
                        return Err(error);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !readers_done.iter().all(|done| *done) {
                        terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
                        return Err(AdbError::Read(
                            "ADB output reader stopped without reporting completion".into(),
                        ));
                    }
                    break;
                }
            }
        }

        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
                    return Err(AdbError::Wait(error));
                }
            };
        }
        if let Some(status) = status.filter(|_| readers_done.iter().all(|done| *done)) {
            containment.disarm().map_err(AdbError::Containment)?;
            return Ok(AdbOutput {
                status: status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&stdout.finish()).into_owned(),
                stderr: String::from_utf8_lossy(&stderr.finish()).into_owned(),
            });
        }

        let now = Instant::now();
        if now >= deadline {
            terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
            return Err(AdbError::Timeout(timeout));
        }
        let wait = CHILD_POLL_INTERVAL.min(deadline.saturating_duration_since(now));
        match receiver.recv_timeout(wait) {
            Ok(event) => {
                if let Err(error) =
                    apply_output_event(event, &mut stdout, &mut stderr, &mut readers_done)
                {
                    terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
                    return Err(error);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !readers_done.iter().all(|done| *done) {
                    terminate_child_tree(&mut child, &mut containment, Some(&output_readers));
                    return Err(AdbError::Read(
                        "ADB output reader stopped without reporting completion".into(),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
fn read_bounded_output(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut captured = OutputAccumulator::default();
    let mut buffer = [0; 16 * 1024];
    loop {
        let length = reader.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        captured.push(&buffer[..length]);
    }
    Ok(captured.finish())
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

enum OutputEvent {
    Chunk(OutputStream, Vec<u8>),
    Finished(OutputStream),
    Failed(OutputStream, io::Error),
}

#[derive(Default)]
struct OutputAccumulator {
    captured: Vec<u8>,
    truncated: bool,
}

impl OutputAccumulator {
    fn push(&mut self, bytes: &[u8]) {
        let remaining = MAX_ADB_OUTPUT_BYTES.saturating_sub(self.captured.len());
        let retained = remaining.min(bytes.len());
        self.captured.extend_from_slice(&bytes[..retained]);
        self.truncated |= retained < bytes.len();
    }

    fn finish(mut self) -> Vec<u8> {
        if self.truncated {
            let retained = MAX_ADB_OUTPUT_BYTES.saturating_sub(OUTPUT_TRUNCATED_MARKER.len());
            self.captured.truncate(retained);
            self.captured.extend_from_slice(OUTPUT_TRUNCATED_MARKER);
        }
        self.captured
    }
}

#[derive(Default)]
struct OutputReaders {
    cancelled: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl OutputReaders {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        cancel_synchronous_output_reads(&self.handles);
    }
}

#[cfg(not(target_os = "windows"))]
fn cancel_synchronous_output_reads(_readers: &[thread::JoinHandle<()>]) {}

#[cfg(target_os = "windows")]
fn cancel_synchronous_output_reads(readers: &[thread::JoinHandle<()>]) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::CancelSynchronousIo;

    for reader in readers {
        unsafe {
            CancelSynchronousIo(reader.as_raw_handle().cast());
        }
    }
}

fn stream_output(
    reader: &mut impl Read,
    stream: OutputStream,
    sender: SyncSender<OutputEvent>,
    cancelled: &AtomicBool,
) {
    let mut buffer = [0; 16 * 1024];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(OutputEvent::Finished(stream));
                return;
            }
            Ok(length) => {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                if sender
                    .send(OutputEvent::Chunk(stream, buffer[..length].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(OutputEvent::Failed(stream, error));
                return;
            }
        }
    }
}

fn apply_output_event(
    event: OutputEvent,
    stdout: &mut OutputAccumulator,
    stderr: &mut OutputAccumulator,
    readers_done: &mut [bool; 2],
) -> Result<(), AdbError> {
    match event {
        OutputEvent::Chunk(OutputStream::Stdout, bytes) => stdout.push(&bytes),
        OutputEvent::Chunk(OutputStream::Stderr, bytes) => stderr.push(&bytes),
        OutputEvent::Finished(stream) => readers_done[stream.index()] = true,
        OutputEvent::Failed(stream, error) => {
            readers_done[stream.index()] = true;
            return Err(AdbError::Read(format!(
                "ADB {} capture failed: {error}",
                match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                }
            )));
        }
    }
    Ok(())
}

fn terminate_child_tree(
    child: &mut Child,
    containment: &mut CommandContainment,
    output_readers: Option<&OutputReaders>,
) {
    containment.terminate();
    if let Some(output_readers) = output_readers {
        output_readers.cancel();
    }
    terminate_direct_child(child);
}

fn terminate_direct_child(child: &mut Child) {
    if !child.try_wait().is_ok_and(|status| status.is_some()) {
        let _ = child.kill();
        let _ = child.wait_timeout(CHILD_TERMINATION_GRACE);
    }
}

#[cfg(not(target_os = "windows"))]
struct CommandContainment;

#[cfg(not(target_os = "windows"))]
impl CommandContainment {
    fn assign(_child: &Child) -> io::Result<Self> {
        Ok(Self)
    }

    fn terminate(&mut self) {}

    fn disarm(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
struct CommandContainment {
    handle: windows_sys::Win32::Foundation::HANDLE,
    armed: bool,
}

#[cfg(target_os = "windows")]
impl CommandContainment {
    fn assign(child: &Child) -> io::Result<Self> {
        use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr::null};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let containment = Self {
            handle,
            armed: true,
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(containment)
    }

    fn terminate(&mut self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        if self.armed {
            unsafe {
                TerminateJobObject(self.handle, 1);
            }
        }
    }

    fn disarm(&mut self) -> io::Result<()> {
        use std::ffi::c_void;
        use windows_sys::Win32::System::JobObjects::{
            JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        };

        if !self.armed {
            return Ok(());
        }
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.armed = false;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for CommandContainment {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[derive(Clone)]
pub struct AdbController {
    executor: Arc<dyn AdbExecutor>,
    device_timeout: Duration,
    command_timeout: Duration,
    mapping_timeout: Duration,
    install_timeout: Duration,
    stop_timeout: Duration,
    poll_interval: Duration,
}

impl fmt::Debug for AdbController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdbController")
            .field("device_timeout", &self.device_timeout)
            .field("command_timeout", &self.command_timeout)
            .field("stop_timeout", &self.stop_timeout)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl AdbController {
    pub fn new(executor: Arc<dyn AdbExecutor>) -> Self {
        let mut controller = Self::with_timing(
            executor,
            Duration::from_secs(8),
            Duration::from_secs(5),
            Duration::from_millis(100),
        );
        controller.device_timeout = ADB_DEVICE_COMMAND_TIMEOUT;
        controller
    }

    pub fn with_timing(
        executor: Arc<dyn AdbExecutor>,
        command_timeout: Duration,
        stop_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        Self {
            executor,
            device_timeout: command_timeout,
            command_timeout,
            mapping_timeout: command_timeout.min(ADB_MAPPING_COMMAND_TIMEOUT),
            install_timeout: command_timeout.max(ADB_INSTALL_TIMEOUT),
            stop_timeout,
            poll_interval,
        }
    }

    /// Returns a clone with a tighter deadline for routine background mapping
    /// health. Public start/stop/install commands retain their longer bounds.
    pub fn with_mapping_timeout(&self, timeout: Duration) -> Self {
        let mut controller = self.clone();
        controller.mapping_timeout = controller.mapping_timeout.min(timeout);
        controller
    }

    pub fn install_matching_apk(&self, apk: &Path) -> Result<(), TransactionError> {
        self.require_device()
            .map_err(|failure| TransactionError::single("device_check", failure))?;
        let package_args = strings(&["-d", "shell", "dumpsys", "package", ANDROID_PACKAGE]);
        let installed = self
            .run_checked(&package_args)
            .map_err(|failure| TransactionError::single("apk_probe", failure))?;
        if package_version_matches(&installed.stdout) {
            return Ok(());
        }
        let args = vec![
            "-d".into(),
            "install".into(),
            "-r".into(),
            apk.as_os_str().to_string_lossy().into_owned(),
        ];
        self.run_checked_with(&args, self.install_timeout)
            .map_err(|failure| TransactionError::single("apk_install", failure))?;
        let package = self
            .run_checked(&package_args)
            .map_err(|failure| TransactionError::single("apk_verify", failure))?;
        if !package_version_matches(&package.stdout) {
            return Err(TransactionError::new(
                "apk_verify",
                vec![format!(
                    "installed {ANDROID_PACKAGE} does not match versionCode={ANDROID_VERSION_CODE}, versionName={ANDROID_VERSION_NAME}"
                )],
            ));
        }
        Ok(())
    }

    pub fn start(
        &self,
        session_id: SessionId,
        all_traffic: bool,
    ) -> Result<StartReceipt, TransactionError> {
        let allowed_package = VIRTUAL_DESKTOP_PACKAGE;
        self.require_device()
            .map_err(|failure| TransactionError::single("device_check", failure))?;
        self.wake_android()
            .map_err(|failure| TransactionError::single("device_wake", failure))?;

        // A host process can disappear after Android has committed its VPN
        // session and reverse mappings. Reusing those mappings lets the old
        // control loop keep presenting its stale session ID to a fresh daemon,
        // so make every new host session begin from a verified Android stop.
        self.stop_android_session("start_quiesce")?;

        let mut failures = Vec::new();
        for mapping in REVERSE_MAPPINGS {
            if let Err(error) = self.add_mapping(mapping) {
                failures.push(error.to_string());
                break;
            }
        }
        if !failures.is_empty() {
            self.remove_all_mappings(&mut failures);
            return Err(TransactionError::new("reverse_setup", failures));
        }
        match self.mapping_health() {
            Ok(health) if health.is_healthy() => {}
            Ok(health) => {
                failures.push(format!(
                    "reverse mappings missing after add: {:?}",
                    health.missing
                ));
                self.remove_all_mappings(&mut failures);
                return Err(TransactionError::new("reverse_verify", failures));
            }
            Err(error) => {
                failures.push(format!("reverse mapping verification failed: {error}"));
                self.remove_all_mappings(&mut failures);
                return Err(TransactionError::new("reverse_verify", failures));
            }
        }

        let all_traffic_value = if all_traffic { "true" } else { "false" };
        let args = vec![
            "-d".into(),
            "shell".into(),
            "am".into(),
            "start-foreground-service".into(),
            "--user".into(),
            "0".into(),
            "-n".into(),
            ANDROID_CONTROL_SERVICE.into(),
            "-a".into(),
            ACTION_START_V4.into(),
            "--es".into(),
            "sessionId".into(),
            session_id.to_string(),
            "--es".into(),
            "vdPackage".into(),
            allowed_package.into(),
            "--ei".into(),
            "socksPort".into(),
            SOCKS_PORT.to_string(),
            "--ei".into(),
            "udpPort".into(),
            UDP_STREAM_PORT.to_string(),
            "--ei".into(),
            "controlPort".into(),
            CONTROL_PORT.to_string(),
            "--ez".into(),
            "allTraffic".into(),
            all_traffic_value.into(),
        ];
        if let Err(error) = self.run_checked(&args) {
            failures.push(error.to_string());
            let rollback = self.run_checked(&strings(&[
                "-d",
                "shell",
                "am",
                "start-foreground-service",
                "--user",
                "0",
                "-n",
                ANDROID_CONTROL_SERVICE,
                "-a",
                ACTION_STOP_V4,
            ]));
            if let Err(rollback) = rollback {
                failures.push(format!("Android start rollback: {rollback}"));
            }
            if let Err(verification) = self.verify_vpn_closed() {
                failures.push(format!(
                    "Android start rollback verification: {verification}"
                ));
            }
            self.remove_all_mappings(&mut failures);
            return Err(TransactionError::new("android_start", failures));
        }
        Ok(StartReceipt {
            session_id,
            all_traffic,
            allowed_package: allowed_package.to_owned(),
            mappings: REVERSE_MAPPINGS.to_vec(),
        })
    }

    /// Requests an explicit VPN stop, verifies the service released the VPN,
    /// then removes every product-owned reverse mapping. Mapping cleanup is
    /// attempted even when the stop request or verification fails.
    pub fn stop(&self) -> Result<(), TransactionError> {
        self.require_device()
            .map_err(|failure| TransactionError::single("stop", failure))?;
        self.wake_android()
            .map_err(|failure| TransactionError::single("device_wake", failure))?;
        self.stop_android_session("stop")
    }

    fn wake_android(&self) -> Result<(), AdbError> {
        self.run_checked(&strings(&[
            "-d",
            "shell",
            "input",
            "keyevent",
            ANDROID_KEYCODE_WAKEUP,
        ]))?;
        Ok(())
    }

    fn stop_android_session(&self, phase: &'static str) -> Result<(), TransactionError> {
        let mut failures = Vec::new();
        let args = strings(&[
            "-d",
            "shell",
            "am",
            "start-foreground-service",
            "--user",
            "0",
            "-n",
            ANDROID_CONTROL_SERVICE,
            "-a",
            ACTION_STOP_V4,
        ]);
        let request_error = self.run_checked(&args).err();
        if let Err(error) = self.verify_vpn_closed() {
            if let Some(request_error) = request_error {
                failures.push(format!("stop request: {request_error}"));
            }
            failures.push(format!("VPN closure verification: {error}"));
        }
        self.remove_all_mappings(&mut failures);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TransactionError::new(phase, failures))
        }
    }

    pub fn repair_mappings(&self) -> Result<(), TransactionError> {
        let health = self
            .mapping_health()
            .map_err(|failure| TransactionError::single("repair_probe", failure))?;
        self.repair_missing_mappings(&health.missing)
    }

    /// Recreates every product-owned reverse mapping even when ADB still
    /// reports the old listeners after a host sleep/resume cycle. Established
    /// sockets can outlive those listener records, so resume recovery needs a
    /// fresh mapping generation rather than a presence-only repair.
    pub fn reinstall_mappings(&self) -> Result<(), TransactionError> {
        let mut failures = Vec::new();
        self.remove_all_mappings(&mut failures);
        if !failures.is_empty() {
            return Err(TransactionError::new("resume_reverse_remove", failures));
        }

        for mapping in REVERSE_MAPPINGS {
            if let Err(error) = self.add_mapping(mapping) {
                failures.push(error.to_string());
                break;
            }
        }
        if failures.is_empty() {
            match self.mapping_health() {
                Ok(health) if health.is_healthy() => {}
                Ok(health) => failures.push(format!(
                    "reverse mappings missing after reinstall: {:?}",
                    health.missing
                )),
                Err(error) => {
                    failures.push(format!("reverse mapping verification failed: {error}"))
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            self.remove_all_mappings(&mut failures);
            Err(TransactionError::new("resume_reverse_setup", failures))
        }
    }

    pub fn repair_missing_mappings(
        &self,
        missing: &[ReverseMapping],
    ) -> Result<(), TransactionError> {
        let mut failures = Vec::new();
        for mapping in missing.iter().copied() {
            if !REVERSE_MAPPINGS.contains(&mapping) {
                failures.push(format!("refusing non-product reverse mapping {mapping:?}"));
                continue;
            }
            if let Err(error) = self.add_mapping(mapping) {
                failures.push(error.to_string());
            }
        }
        match self.mapping_health() {
            Ok(health) if health.is_healthy() => {}
            Ok(health) => failures.push(format!(
                "reverse mappings missing after repair: {:?}",
                health.missing
            )),
            Err(error) => failures.push(format!("reverse mapping verification failed: {error}")),
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TransactionError::new("repair", failures))
        }
    }

    /// Completes the happy-path GNR4 stop transaction: STOPPED is only sent by
    /// Android after it closes the VPN descriptor, and this performs a second
    /// dumpsys verification before removing product-owned reverse mappings.
    pub fn finish_control_stop(&self) -> Result<(), TransactionError> {
        let mut failures = Vec::new();
        if let Err(error) = self.verify_vpn_closed() {
            failures.push(format!("VPN closure verification: {error}"));
        }
        self.remove_all_mappings(&mut failures);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TransactionError::new("control_stop", failures))
        }
    }

    pub fn mapping_health(&self) -> Result<MappingHealth, AdbError> {
        let output =
            self.run_checked_with(&strings(&["-d", "reverse", "--list"]), self.mapping_timeout)?;
        let missing = REVERSE_MAPPINGS
            .iter()
            .copied()
            .filter(|mapping| {
                let expected = (
                    format!("tcp:{}", mapping.remote),
                    format!("tcp:{}", mapping.local),
                );
                !parse_reverse_list(&output.stdout).any(|actual| actual == expected)
            })
            .collect();
        Ok(MappingHealth { missing })
    }

    pub fn device_state(&self) -> Result<String, AdbError> {
        Ok(self
            .run_checked_with(&strings(&["-d", "get-state"]), self.mapping_timeout)?
            .stdout
            .trim()
            .to_owned())
    }

    pub fn android_status(&self) -> Result<AndroidVpnStatus, AdbError> {
        self.android_status_with_timeout(self.command_timeout)
    }

    pub(crate) fn android_status_quick(&self) -> Result<AndroidVpnStatus, AdbError> {
        self.android_status_with_timeout(self.command_timeout.min(Duration::from_secs(1)))
    }

    fn android_status_with_timeout(&self, timeout: Duration) -> Result<AndroidVpnStatus, AdbError> {
        let output = self.run_checked_with(
            &strings(&[
                "-d",
                "shell",
                "dumpsys",
                "activity",
                "service",
                ANDROID_VPN_SERVICE,
            ]),
            timeout,
        )?;
        Ok(AndroidVpnStatus::parse(&output.stdout))
    }

    fn verify_vpn_closed(&self) -> Result<(), AdbError> {
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            let output = self.run_checked(&strings(&[
                "-d",
                "shell",
                "dumpsys",
                "activity",
                "service",
                ANDROID_VPN_SERVICE,
            ]))?;
            let normalized = output.stdout.to_ascii_lowercase();
            let process_stopped = normalized.contains("pid=(not running)")
                && !normalized
                    .lines()
                    .map(str::trim)
                    .any(|line| line == "vpnfdopen=true");
            let service_absent = normalized.trim().is_empty()
                || normalized.contains("no services match")
                || normalized.contains("no service records found")
                || process_stopped;
            if service_absent || vpn_descriptor_explicitly_closed(&output.stdout) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AdbError::VpnStillActive);
            }
            thread::sleep(
                self.poll_interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    pub(crate) fn add_mapping(&self, mapping: ReverseMapping) -> Result<(), AdbError> {
        self.run_checked_with(
            &[
                "-d".into(),
                "reverse".into(),
                format!("tcp:{}", mapping.remote),
                format!("tcp:{}", mapping.local),
            ],
            self.mapping_timeout,
        )?;
        Ok(())
    }

    fn remove_all_mappings(&self, failures: &mut Vec<String>) {
        for mapping in REVERSE_MAPPINGS {
            let args = [
                "-d".into(),
                "reverse".into(),
                "--remove".into(),
                format!("tcp:{}", mapping.remote),
            ];
            match self.executor.execute(&args, self.mapping_timeout) {
                Ok(output) if output.is_success() => {}
                Ok(output) if mapping_was_already_absent(&output) => {}
                Ok(output) => failures.push(format!(
                    "remove tcp:{}: ADB exited with {}: {}",
                    mapping.remote,
                    output.status,
                    output.stderr.trim()
                )),
                Err(error) => failures.push(format!("remove tcp:{}: {error}", mapping.remote)),
            }
        }
        match self.run_checked_with(&strings(&["-d", "reverse", "--list"]), self.mapping_timeout) {
            Ok(output) => {
                let owned: Vec<_> = parse_reverse_list(&output.stdout)
                    .filter(|(remote, _)| {
                        REVERSE_MAPPINGS
                            .iter()
                            .any(|mapping| remote == &format!("tcp:{}", mapping.remote))
                    })
                    .collect();
                if !owned.is_empty() {
                    failures.push(format!(
                        "product reverse mappings remain after removal: {owned:?}"
                    ));
                }
            }
            Err(error) => failures.push(format!("could not verify reverse removal: {error}")),
        }
    }

    fn run_checked(&self, args: &[String]) -> Result<AdbOutput, AdbError> {
        self.run_checked_with(args, self.command_timeout)
    }

    /// The first command after installing platform-tools may also have to
    /// start the ADB server. Give that cold path its own deadline and retry a
    /// single timeout; routine mapping probes remain independently bounded.
    fn require_device(&self) -> Result<(), AdbError> {
        let args = strings(&["-d", "get-state"]);
        for attempt in 0..2 {
            match self.run_checked_with(&args, self.device_timeout) {
                Ok(output) if output.stdout.trim() == "device" => return Ok(()),
                Ok(output) => {
                    let state = output.stdout.trim();
                    let state = if state.is_empty() { "unknown" } else { state };
                    return Err(AdbError::DeviceNotReady(device_help(state)));
                }
                Err(AdbError::Timeout(_)) if attempt == 0 => {
                    self.executor.recover_server(self.device_timeout)?;
                    thread::sleep(self.poll_interval);
                }
                Err(error) if adb_reports_missing_device(&error) => {
                    return Err(AdbError::DeviceNotReady(device_help("not found")));
                }
                Err(AdbError::Timeout(_)) => {
                    return Err(AdbError::DeviceNotReady(device_help("not responding")));
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the bounded ADB device check always returns within two attempts")
    }

    fn run_checked_with(&self, args: &[String], timeout: Duration) -> Result<AdbOutput, AdbError> {
        let output = self.executor.execute(args, timeout)?;
        if output.is_success() {
            Ok(output)
        } else {
            Err(AdbError::CommandFailed {
                status: output.status,
                stderr: output.stderr.trim().to_owned(),
            })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartReceipt {
    pub session_id: SessionId,
    pub all_traffic: bool,
    pub allowed_package: String,
    pub mappings: Vec<ReverseMapping>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MappingHealth {
    pub missing: Vec<ReverseMapping>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AndroidVpnStatus {
    pub present: bool,
    pub state: Option<String>,
    pub vpn_fd_open: Option<bool>,
    pub session_id: Option<String>,
    pub last_error: Option<String>,
    pub screen_suspended: Option<bool>,
    pub tx_packets: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub rx_packets: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub control_rtt_samples: Option<u64>,
    pub control_rtt_p99_us: Option<u64>,
    pub control_rtt_max_us: Option<u64>,
    pub control_rtt_histogram: Vec<u64>,
}

impl AndroidVpnStatus {
    fn parse(output: &str) -> Self {
        let mut status = Self::default();
        for line in output.lines().map(str::trim) {
            if line.contains("ServiceRecord") || line.contains("app=ProcessRecord") {
                status.present = true;
            }
            if let Some(value) = line.strip_prefix("gnirehtet.state=") {
                status.present = true;
                status.state = Some(value.to_ascii_lowercase());
            } else if let Some(value) = line.strip_prefix("vpnFdOpen=") {
                status.present = true;
                status.vpn_fd_open = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("sessionId=") {
                status.present = true;
                status.session_id = (value != "none").then(|| value.to_owned());
            } else if let Some(value) = line.strip_prefix("lastError=") {
                status.present = true;
                status.last_error = (value != "none").then(|| value.to_owned());
            } else if let Some(value) = line.strip_prefix("screenSuspended=") {
                status.present = true;
                status.screen_suspended = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("controlRttSamples=") {
                status.control_rtt_samples = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("controlRttP99Us=") {
                status.control_rtt_p99_us = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("controlRttMaxUs=") {
                status.control_rtt_max_us = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("controlRttHistogram=") {
                status.control_rtt_histogram = value
                    .split(',')
                    .filter_map(|count| count.trim().parse().ok())
                    .collect();
            } else if line.starts_with("txPackets=") {
                for field in line.split_whitespace() {
                    let Some((key, value)) = field.split_once('=') else {
                        continue;
                    };
                    let value = value.parse().ok();
                    match key {
                        "txPackets" => status.tx_packets = value,
                        "txBytes" => status.tx_bytes = value,
                        "rxPackets" => status.rx_packets = value,
                        "rxBytes" => status.rx_bytes = value,
                        _ => {}
                    }
                }
            }
        }
        status
    }
}

impl MappingHealth {
    pub fn is_healthy(&self) -> bool {
        self.missing.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum AdbError {
    #[error("failed to start {program}: {source}")]
    Spawn {
        program: PathBuf,
        source: std::io::Error,
    },
    #[error("ADB command exceeded {0:?}")]
    Timeout(Duration),
    #[error("failed while waiting for ADB: {0}")]
    Wait(std::io::Error),
    #[error("failed to contain ADB subprocess tree: {0}")]
    Containment(std::io::Error),
    #[error("failed to capture ADB output: {0}")]
    Read(String),
    #[error("ADB exited with {status}: {stderr}")]
    CommandFailed { status: i32, stderr: String },
    #[error("{0}")]
    DeviceNotReady(String),
    #[error("Android still reports an active VPN after the stop deadline")]
    VpnStillActive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionError {
    pub phase: &'static str,
    pub failures: Vec<String>,
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ADB {} failed", self.phase)?;
        if let [failure] = self.failures.as_slice() {
            write!(formatter, ": {failure}")
        } else if !self.failures.is_empty() {
            write!(formatter, ": {}", self.failures.join("; "))
        } else {
            Ok(())
        }
    }
}

impl std::error::Error for TransactionError {}

impl TransactionError {
    fn new(phase: &'static str, failures: Vec<String>) -> Self {
        Self { phase, failures }
    }

    fn single(phase: &'static str, failure: AdbError) -> Self {
        Self::new(phase, vec![failure.to_string()])
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn package_version_matches(output: &str) -> bool {
    let has_code = output.lines().any(|line| {
        line.trim()
            .strip_prefix("versionCode=")
            .and_then(|value| value.split_whitespace().next())
            == Some(ANDROID_VERSION_CODE)
    });
    let has_name = output
        .lines()
        .any(|line| line.trim() == format!("versionName={ANDROID_VERSION_NAME}"));
    has_code && has_name
}

fn adb_reports_missing_device(error: &AdbError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("no devices")
        || message.contains("no device")
        || message.contains("device not found")
}

fn device_help(state: &str) -> String {
    format!(
        "No Quest 3 is available through USB ({state}). Connect and unlock the headset with a USB data cable, then accept the USB debugging prompt and try again."
    )
}

fn mapping_was_already_absent(output: &AdbOutput) -> bool {
    let message = format!("{} {}", output.stdout, output.stderr).to_ascii_lowercase();
    [
        "cannot remove listener",
        "listener not found",
        "no such listener",
        "not found",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn vpn_descriptor_explicitly_closed(output: &str) -> bool {
    let mut saw_closed = false;
    for value in output
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("vpnFdOpen="))
    {
        match value.parse::<bool>() {
            Ok(true) | Err(_) => return false,
            Ok(false) => saw_closed = true,
        }
    }
    saw_closed
}

fn parse_reverse_list(output: &str) -> impl Iterator<Item = (String, String)> + '_ {
    output.lines().filter_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 3 {
            return None;
        }
        let remote = fields[1];
        let local = fields[2];
        (remote.starts_with("tcp:") && local.starts_with("tcp:"))
            .then(|| (remote.to_owned(), local.to_owned()))
    })
}

#[cfg(target_os = "windows")]
pub fn repair_adb_if_missing(
    configured: PathBuf,
    app_root: &Path,
) -> Result<PathBuf, AdbBootstrapError> {
    let configured_adb = SystemAdb::new(configured.clone());
    if configured_adb
        .execute(&["version".into()], Duration::from_secs(3))
        .is_ok_and(|output| output.is_success())
    {
        return Ok(configured);
    }

    if let Some(adb) = verified_local_adb(app_root) {
        if adb != configured
            && SystemAdb::new(adb.clone())
                .execute(&["version".into()], Duration::from_secs(3))
                .is_ok_and(|output| output.is_success())
        {
            return Ok(adb);
        }
    }

    let install_root = app_root.join(format!("platform-tools-{PLATFORM_TOOLS_VERSION}"));
    let platform_tools = install_root.join("platform-tools");
    let adb = platform_tools.join("adb.exe");
    let api_dll = platform_tools.join("AdbWinApi.dll");
    let usb_dll = platform_tools.join("AdbWinUsbApi.dll");
    let marker = install_root.join(".verified-sha256");
    let required = [adb.as_path(), api_dll.as_path(), usb_dll.as_path()];
    fs::create_dir_all(app_root)?;
    if install_root.exists() {
        fs::remove_dir_all(&install_root)?;
    }
    let archive = app_root.join(format!("platform-tools-{PLATFORM_TOOLS_VERSION}-win.zip"));
    let script = "$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri $env:GNR4_ADB_URL -OutFile $env:GNR4_ADB_ARCHIVE; $actual=(Get-FileHash -Algorithm SHA256 -LiteralPath $env:GNR4_ADB_ARCHIVE).Hash.ToLowerInvariant(); if($actual -ne $env:GNR4_ADB_SHA){Remove-Item -Force $env:GNR4_ADB_ARCHIVE -ErrorAction SilentlyContinue; throw ('SHA-256 mismatch: '+$actual)}; Expand-Archive -LiteralPath $env:GNR4_ADB_ARCHIVE -DestinationPath $env:GNR4_ADB_DEST -Force";
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .env("GNR4_ADB_URL", PLATFORM_TOOLS_WINDOWS_URL)
        .env("GNR4_ADB_SHA", PLATFORM_TOOLS_WINDOWS_SHA256)
        .env("GNR4_ADB_ARCHIVE", &archive)
        .env("GNR4_ADB_DEST", &install_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_subprocess_window(&mut command);
    let child = command.spawn()?;
    let output = collect_child_output(child, PLATFORM_TOOLS_DOWNLOAD_TIMEOUT)
        .map_err(|error| AdbBootstrapError::PowerShell(error.to_string()))?;
    let _ = fs::remove_file(&archive);
    if !output.is_success() {
        let _ = fs::remove_dir_all(&install_root);
        return Err(AdbBootstrapError::PowerShell(
            output.stderr.trim().to_owned(),
        ));
    }
    if !required.iter().all(|path| path.is_file()) {
        let _ = fs::remove_dir_all(&install_root);
        return Err(AdbBootstrapError::IncompleteArchive);
    }
    fs::write(&marker, format!("{PLATFORM_TOOLS_WINDOWS_SHA256}\n"))?;
    Ok(adb)
}

/// Resolves ADB consistently for every command and for the spawned host:
/// explicit override, checksum-marked local platform-tools, then PATH.
pub fn resolve_adb_program(configured: Option<PathBuf>, app_root: &Path) -> PathBuf {
    configured
        .or_else(|| verified_local_adb(app_root))
        .unwrap_or_else(|| PathBuf::from("adb"))
}

#[cfg(target_os = "windows")]
fn verified_local_adb(app_root: &Path) -> Option<PathBuf> {
    let install_root = app_root.join(format!("platform-tools-{PLATFORM_TOOLS_VERSION}"));
    let platform_tools = install_root.join("platform-tools");
    let adb = platform_tools.join("adb.exe");
    let api_dll = platform_tools.join("AdbWinApi.dll");
    let usb_dll = platform_tools.join("AdbWinUsbApi.dll");
    let required = [adb.as_path(), api_dll.as_path(), usb_dll.as_path()];
    let marker = install_root.join(".verified-sha256");
    (required.iter().all(|path| path.is_file())
        && fs::read_to_string(marker)
            .ok()
            .is_some_and(|value| value.trim() == PLATFORM_TOOLS_WINDOWS_SHA256))
    .then_some(adb)
}

#[cfg(not(target_os = "windows"))]
fn verified_local_adb(_app_root: &Path) -> Option<PathBuf> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn repair_adb_if_missing(
    configured: PathBuf,
    _app_root: &Path,
) -> Result<PathBuf, AdbBootstrapError> {
    Ok(configured)
}

#[derive(Debug, Error)]
pub enum AdbBootstrapError {
    #[error("ADB bootstrap I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("official platform-tools download failed: {0}")]
    PowerShell(String),
    #[error("verified platform-tools archive did not contain adb.exe and its required DLLs")]
    IncompleteArchive,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::Cursor,
        sync::{atomic::AtomicUsize, Mutex},
    };

    use super::*;

    #[derive(Default)]
    struct MockAdb {
        calls: Mutex<Vec<Vec<String>>>,
        timeouts: Mutex<Vec<(Vec<String>, Duration)>>,
        results: Mutex<VecDeque<Result<AdbOutput, AdbError>>>,
        recoveries: AtomicUsize,
    }

    impl MockAdb {
        fn with_results(results: Vec<Result<AdbOutput, AdbError>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                timeouts: Mutex::new(Vec::new()),
                results: Mutex::new(results.into()),
                recoveries: AtomicUsize::new(0),
            }
        }
    }

    impl AdbExecutor for MockAdb {
        fn execute(&self, args: &[String], timeout: Duration) -> Result<AdbOutput, AdbError> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.timeouts.lock().unwrap().push((args.to_vec(), timeout));
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(AdbOutput::success("")))
        }

        fn recover_server(&self, _timeout: Duration) -> Result<(), AdbError> {
            self.recoveries.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn controller(mock: Arc<MockAdb>) -> AdbController {
        AdbController::with_timing(
            mock,
            Duration::from_millis(10),
            Duration::ZERO,
            Duration::ZERO,
        )
    }

    fn assert_control_service(args: &[String], action: &str) {
        assert!(args.iter().map(String::as_str).eq([
            "-d",
            "shell",
            "am",
            "start-foreground-service",
            "--user",
            "0",
            "-n",
            ANDROID_CONTROL_SERVICE,
            "-a",
            action,
        ]));
    }

    #[test]
    fn failed_start_rolls_back_every_mapping() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("device")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("No services match")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput {
                status: 1,
                stdout: String::new(),
                stderr: "reverse refused".into(),
            }),
        ]));
        let error = controller(mock.clone())
            .start(SessionId([7; 16]), false)
            .unwrap_err();
        assert_eq!(error.phase, "reverse_setup");
        let calls = mock.calls.lock().unwrap();
        for mapping in REVERSE_MAPPINGS {
            assert!(calls.iter().any(|args| {
                args.windows(2)
                    .any(|pair| pair == ["--remove", &format!("tcp:{}", mapping.remote)])
            }));
        }
    }

    #[test]
    fn stop_cleanup_runs_even_when_stop_request_fails() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("device")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput {
                status: 1,
                stdout: String::new(),
                stderr: "service unavailable".into(),
            }),
            Ok(AdbOutput::success(format!(
                "ServiceRecord {{{ANDROID_VPN_SERVICE}}}\ngnirehtet.state=CONNECTED\nvpnFdOpen=true"
            ))),
        ]));
        let result = controller(mock.clone()).stop();
        assert!(result.is_err());
        let calls = mock.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "--remove"))
                .count(),
            REVERSE_MAPPINGS.len()
        );
        assert!(calls.iter().any(|args| {
            args.iter().map(String::as_str).eq([
                "-d",
                "shell",
                "input",
                "keyevent",
                ANDROID_KEYCODE_WAKEUP,
            ])
        }));
        assert!(!calls.iter().any(|args| args.iter().any(|arg| arg == "-W")));
        let stop = calls
            .iter()
            .find(|args| args.iter().any(|arg| arg == ACTION_STOP_V4))
            .unwrap();
        assert_control_service(stop, ACTION_STOP_V4);
    }

    #[test]
    fn already_stopped_is_idempotent_even_if_control_service_command_fails() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("device")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput {
                status: 1,
                stdout: String::new(),
                stderr: "control service unavailable".into(),
            }),
            Ok(AdbOutput::success("No services match")),
        ]));
        controller(mock).stop().unwrap();
    }

    #[test]
    fn stop_without_a_device_returns_one_error_and_skips_teardown_commands() {
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput {
            status: 1,
            stdout: String::new(),
            stderr: "adb.exe: no devices found".into(),
        })]));
        let error = controller(mock.clone()).stop().unwrap_err();
        assert_eq!(error.phase, "stop");
        assert_eq!(error.failures.len(), 1);
        assert!(error.to_string().contains("Connect and unlock the headset"));
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].iter().map(String::as_str).eq(["-d", "get-state"]));
    }

    #[test]
    fn start_uses_the_android_v4_all_traffic_contract() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("device")),             // device check
            Ok(AdbOutput::success("")),                   // wake headset
            Ok(AdbOutput::success("")),                   // stale STOP
            Ok(AdbOutput::success("No services match")),  // VPN closed
            Ok(AdbOutput::success("")),                   // remove SOCKS
            Ok(AdbOutput::success("")),                   // remove UDP
            Ok(AdbOutput::success("")),                   // remove control
            Ok(AdbOutput::success("")),                   // verify removed
            Ok(AdbOutput::success("")),                   // add SOCKS
            Ok(AdbOutput::success("")),                   // add UDP
            Ok(AdbOutput::success("")),                   // add control
            Ok(AdbOutput::success(
                "UsbFfs tcp:31417 tcp:31417\nUsbFfs tcp:31416 tcp:31416\nUsbFfs tcp:31418 tcp:31418\n",
            )), // verify mappings
            Ok(AdbOutput::success("Starting")), // fresh START
        ]));
        let receipt = AdbController::new(mock.clone())
            .start(SessionId([0x11; 16]), true)
            .unwrap();
        assert_eq!(receipt.allowed_package, VIRTUAL_DESKTOP_PACKAGE);
        let calls = mock.calls.lock().unwrap();
        assert!(!calls.iter().any(|args| args.iter().any(|arg| arg == "-W")));
        let stop_index = calls
            .iter()
            .position(|args| args.iter().any(|arg| arg == ACTION_STOP_V4))
            .unwrap();
        let wake_index = calls
            .iter()
            .position(|args| args.iter().any(|arg| arg == ANDROID_KEYCODE_WAKEUP))
            .unwrap();
        let start = calls
            .iter()
            .find(|args| args.iter().any(|arg| arg == ACTION_START_V4))
            .unwrap();
        let start_index = calls
            .iter()
            .position(|args| args.iter().any(|arg| arg == ACTION_START_V4))
            .unwrap();
        assert!(wake_index < stop_index);
        assert!(stop_index < start_index);
        assert_control_service(&calls[stop_index], ACTION_STOP_V4);
        assert!(start.iter().take(10).map(String::as_str).eq([
            "-d",
            "shell",
            "am",
            "start-foreground-service",
            "--user",
            "0",
            "-n",
            ANDROID_CONTROL_SERVICE,
            "-a",
            ACTION_START_V4,
        ]));
        assert!(!calls.iter().any(|args| {
            args.windows(2)
                .any(|arguments| arguments == ["am", "start"])
        }));
        assert!(!calls
            .iter()
            .any(|args| args.iter().any(|argument| argument == "broadcast")));
        assert!(!calls.iter().any(|args| args.iter().any(|argument| {
            argument.ends_with("/.v4.AdbControlActivity")
                || argument.ends_with("/.v4.AdbControlReceiver")
        })));
        assert!(!calls.iter().any(|args| {
            args.windows(2)
                .any(|arguments| arguments == ["force-stop", ANDROID_PACKAGE])
        }));
        assert!(!calls.iter().any(|args| {
            args.windows(2)
                .any(|arguments| arguments == ["force-stop", VIRTUAL_DESKTOP_PACKAGE])
        }));
        for required in [
            ANDROID_CONTROL_SERVICE,
            "sessionId",
            "vdPackage",
            VIRTUAL_DESKTOP_PACKAGE,
            "socksPort",
            "31416",
            "udpPort",
            "31418",
            "controlPort",
            "31417",
            "allTraffic",
            "true",
        ] {
            assert!(
                start.iter().any(|arg| arg == required),
                "missing {required}"
            );
        }
        assert!(start
            .windows(3)
            .any(|arguments| { arguments == ["--es", "vdPackage", VIRTUAL_DESKTOP_PACKAGE] }));
        drop(calls);
        let timeouts = mock.timeouts.lock().unwrap();
        assert!(timeouts.iter().any(|(args, timeout)| {
            args.iter().map(String::as_str).eq(["-d", "get-state"])
                && *timeout == ADB_DEVICE_COMMAND_TIMEOUT
        }));
        assert!(timeouts.iter().any(|(args, timeout)| {
            args.iter().any(|argument| argument == "reverse")
                && *timeout == ADB_MAPPING_COMMAND_TIMEOUT
        }));
        assert!(timeouts.iter().any(|(args, timeout)| {
            args.iter().any(|argument| argument == ACTION_START_V4)
                && *timeout == Duration::from_secs(8)
        }));
    }

    #[test]
    fn adb_output_capture_is_bounded_while_draining_the_stream() {
        let input = vec![b'x'; MAX_ADB_OUTPUT_BYTES * 2];
        let captured = read_bounded_output(&mut Cursor::new(input)).unwrap();
        assert_eq!(captured.len(), MAX_ADB_OUTPUT_BYTES);
        assert!(captured.ends_with(OUTPUT_TRUNCATED_MARKER));
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_capture_preserves_both_streams_and_exit_status() {
        let child = Command::new("sh")
            .args(["-c", "printf stdout; printf stderr >&2; exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = collect_child_output(child, Duration::from_secs(1)).unwrap();
        assert_eq!(output.status, 7);
        assert_eq!(output.stdout, "stdout");
        assert_eq!(output.stderr, "stderr");
    }

    #[cfg(unix)]
    #[test]
    fn inherited_output_pipe_cannot_extend_the_command_deadline() {
        let child = Command::new("sh")
            .args(["-c", "(trap '' HUP; sleep 1) &"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let result = collect_child_output(child, Duration::from_millis(25));
        assert!(matches!(result, Err(AdbError::Timeout(_))));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn stopped_state_does_not_override_an_open_vpn_descriptor() {
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(
            format!(
                "ServiceRecord {{{ANDROID_VPN_SERVICE}}}\ngnirehtet.state=STOPPED\nvpnFdOpen=true"
            ),
        ))]));
        let result = controller(mock).finish_control_stop();
        assert!(result.is_err());
    }

    #[test]
    fn stopped_android_service_process_is_a_closed_vpn() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success(format!(
                "SERVICE {ANDROID_VPN_SERVICE} 31534bb pid=(not running)"
            ))),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
        ]));
        controller(mock).finish_control_stop().unwrap();
    }

    #[test]
    fn stopped_android_process_does_not_override_an_open_vpn_descriptor() {
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(
            format!("SERVICE {ANDROID_VPN_SERVICE} 31534bb pid=(not running)\nvpnFdOpen=true"),
        ))]));
        assert!(controller(mock).finish_control_stop().is_err());
    }

    #[test]
    fn contradictory_descriptor_fields_fail_closed() {
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(
            format!(
                "ServiceRecord {{{ANDROID_VPN_SERVICE}}}\ngnirehtet.state=STOPPED\nvpnFdOpen=true\nvpnFdOpen=false"
            ),
        ))]));
        assert!(controller(mock).finish_control_stop().is_err());
    }

    #[test]
    fn descriptor_field_alone_marks_the_v4_service_present() {
        let status = AndroidVpnStatus::parse("vpnFdOpen=true\n");
        assert!(status.present);
        assert_eq!(status.vpn_fd_open, Some(true));
    }

    #[test]
    fn install_verifies_matching_package_version() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("device")),
            Ok(AdbOutput::success("Package not found")),
            Ok(AdbOutput::success("Success")),
            Ok(AdbOutput::success(
                "versionCode=65 minSdk=29 targetSdk=36\nversionName=4.1.20\n",
            )),
        ]));
        AdbController::new(mock.clone())
            .install_matching_apk(Path::new("embedded.apk"))
            .unwrap();
        let calls = mock.calls.lock().unwrap();
        assert!(calls.iter().any(|args| {
            args.iter()
                .map(String::as_str)
                .eq(["-d", "install", "-r", "embedded.apk"])
        }));
        assert!(calls.iter().any(|args| {
            args.iter().map(String::as_str).eq([
                "-d",
                "shell",
                "dumpsys",
                "package",
                ANDROID_PACKAGE,
            ])
        }));
        drop(calls);
        let timeouts = mock.timeouts.lock().unwrap();
        assert!(timeouts.iter().any(|(args, timeout)| {
            args.iter().any(|argument| argument == "install") && *timeout == ADB_INSTALL_TIMEOUT
        }));
        assert!(timeouts.iter().any(|(args, timeout)| {
            args.iter().any(|argument| argument == "get-state")
                && *timeout == ADB_DEVICE_COMMAND_TIMEOUT
        }));
    }

    #[test]
    fn cold_device_timeout_is_retried_once_with_the_device_deadline() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Err(AdbError::Timeout(ADB_DEVICE_COMMAND_TIMEOUT)),
            Ok(AdbOutput::success("device")),
            Ok(AdbOutput::success("Package not found")),
            Ok(AdbOutput::success("Success")),
            Ok(AdbOutput::success(
                "versionCode=65 minSdk=29 targetSdk=36\nversionName=4.1.20\n",
            )),
        ]));
        AdbController::new(mock.clone())
            .install_matching_apk(Path::new("embedded.apk"))
            .unwrap();

        let timeouts = mock.timeouts.lock().unwrap();
        let device_checks: Vec<_> = timeouts
            .iter()
            .filter(|(args, _)| args.iter().map(String::as_str).eq(["-d", "get-state"]))
            .collect();
        assert_eq!(device_checks.len(), 2);
        assert!(device_checks
            .iter()
            .all(|(_, timeout)| *timeout == ADB_DEVICE_COMMAND_TIMEOUT));
        assert_eq!(mock.recoveries.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn matching_apk_is_not_reinstalled_during_retry() {
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("device")),
            Ok(AdbOutput::success(
                "versionCode=65 minSdk=29 targetSdk=36\nversionName=4.1.20\n",
            )),
        ]));
        AdbController::new(mock.clone())
            .install_matching_apk(Path::new("embedded.apk"))
            .unwrap();

        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls
            .iter()
            .any(|args| args.iter().any(|argument| argument == "install")));
    }

    #[test]
    fn unauthorized_device_fails_immediately_with_actionable_error() {
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(
            "unauthorized",
        ))]));
        let error = AdbController::new(mock.clone())
            .install_matching_apk(Path::new("embedded.apk"))
            .unwrap_err();
        assert_eq!(error.phase, "device_check");
        assert!(error.failures[0].contains("accept the USB debugging prompt"));
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn missing_device_has_one_concise_layman_friendly_error() {
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput {
            status: 1,
            stdout: String::new(),
            stderr: "error: no devices found".into(),
        })]));
        let error = AdbController::new(mock.clone())
            .install_matching_apk(Path::new("embedded.apk"))
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Connect and unlock the headset"));
        assert!(message.contains("accept the USB debugging prompt"));
        assert!(!message.contains("[\""));
        assert_eq!(error.failures.len(), 1);
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn health_requires_all_owned_mappings() {
        let stdout =
            "UsbFfs tcp:31416 tcp:31416\nUsbFfs tcp:31417 tcp:31417\nUsbFfs tcp:31418 tcp:31418\n";
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(stdout))]));
        assert!(controller(mock).mapping_health().unwrap().is_healthy());
    }

    #[test]
    fn resume_reinstall_removes_then_recreates_every_owned_mapping() {
        let mappings =
            "UsbFfs tcp:31416 tcp:31416\nUsbFfs tcp:31417 tcp:31417\nUsbFfs tcp:31418 tcp:31418\n";
        let mock = Arc::new(MockAdb::with_results(vec![
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success("")),
            Ok(AdbOutput::success(mappings)),
        ]));

        controller(mock.clone()).reinstall_mappings().unwrap();

        let calls = mock.calls.lock().unwrap();
        let remove_positions: Vec<_> = calls
            .iter()
            .enumerate()
            .filter_map(|(index, args)| args.iter().any(|arg| arg == "--remove").then_some(index))
            .collect();
        let add_positions: Vec<_> = calls
            .iter()
            .enumerate()
            .filter_map(|(index, args)| {
                (args.get(1).is_some_and(|arg| arg == "reverse")
                    && !args.iter().any(|arg| arg == "--remove")
                    && !args.iter().any(|arg| arg == "--list"))
                .then_some(index)
            })
            .collect();
        assert_eq!(remove_positions.len(), REVERSE_MAPPINGS.len());
        assert_eq!(add_positions.len(), REVERSE_MAPPINGS.len());
        assert!(remove_positions.iter().max() < add_positions.iter().min());
    }

    #[test]
    fn background_mapping_timeout_is_tightened_without_changing_public_deadlines() {
        let stdout =
            "UsbFfs tcp:31416 tcp:31416\nUsbFfs tcp:31417 tcp:31417\nUsbFfs tcp:31418 tcp:31418\n";
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(stdout))]));
        AdbController::new(mock.clone())
            .with_mapping_timeout(Duration::from_millis(500))
            .mapping_health()
            .unwrap();
        assert_eq!(
            mock.timeouts.lock().unwrap().as_slice(),
            &[(
                strings(&["-d", "reverse", "--list"]),
                Duration::from_millis(500)
            )]
        );
    }

    #[test]
    fn health_rejects_prefix_and_substring_false_positives() {
        let stdout =
            "UsbFfs tcp:314160 tcp:314160\nUsbFfs tcp:31417 tcp:314170\nmalformed tcp:31416 tcp:31416 extra\n";
        let mock = Arc::new(MockAdb::with_results(vec![Ok(AdbOutput::success(stdout))]));
        let health = controller(mock).mapping_health().unwrap();
        assert_eq!(health.missing.len(), REVERSE_MAPPINGS.len());
    }

    #[test]
    fn pinned_platform_tools_digest_is_lowercase_sha256() {
        assert_eq!(PLATFORM_TOOLS_WINDOWS_SHA256.len(), 64);
        assert!(PLATFORM_TOOLS_WINDOWS_SHA256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert!(PLATFORM_TOOLS_WINDOWS_URL.starts_with("https://dl.google.com/"));
    }

    #[test]
    fn explicit_adb_override_wins_deterministically() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("configured-adb");
        assert_eq!(
            resolve_adb_program(Some(configured.clone()), directory.path()),
            configured
        );
    }

    #[test]
    fn parses_payload_free_android_rtt_and_traffic_status() {
        let status = AndroidVpnStatus::parse(
            "gnirehtet.state=CONNECTED\nvpnFdOpen=true\nsessionId=00112233-4455-6677-8899-aabbccddeeff\nlastError=none\nscreenSuspended=true\ntxPackets=10 txBytes=20 rxPackets=30 rxBytes=40\ncontrolRttSamples=5\ncontrolRttP99Us=2000\ncontrolRttMaxUs=3000\ncontrolRttHistogram=1,2,2,0,0,0,0,0,0,0\n",
        );
        assert!(status.present);
        assert_eq!(status.state.as_deref(), Some("connected"));
        assert_eq!(status.screen_suspended, Some(true));
        assert_eq!(status.tx_bytes, Some(20));
        assert_eq!(status.control_rtt_p99_us, Some(2_000));
        assert_eq!(status.control_rtt_histogram.len(), 10);
    }
}
