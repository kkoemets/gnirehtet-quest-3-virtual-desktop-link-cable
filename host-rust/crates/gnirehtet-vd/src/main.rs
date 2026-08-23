use std::{
    path::PathBuf,
    process::{Child, Command as ProcessCommand, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "windows")]
use std::io::Write;
#[cfg(any(target_os = "windows", test))]
use std::sync::Mutex as StdMutex;

#[cfg(any(target_os = "windows", test))]
use std::sync::atomic::Ordering;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use gnirehtet_vd::{
    adb::{
        repair_adb_if_missing, resolve_adb_program, AdbController, SystemAdb,
        ADB_MAPPING_COMMAND_TIMEOUT, VIRTUAL_DESKTOP_PACKAGE,
    },
    diagnostics::{Diagnostics, DEFAULT_FILE_COUNT, DEFAULT_MAX_BYTES, DEFAULT_TOTAL_BYTES},
    embedded,
    protocol::SessionId,
    runtime::{
        admin_command, doctor, process_is_running, read_daemon_pid, terminate_daemon,
        AdminResponse, AppPaths, HostRuntime, OperationGuard, RuntimeConfig, StateStore,
    },
    state::{HostState, StateSnapshot},
};
#[cfg(any(target_os = "windows", test))]
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(any(target_os = "windows", test))]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[derive(Debug, Parser)]
#[command(
    name = "quest-vd-wired",
    version,
    about = "Quest 3 Virtual Desktop wired link"
)]
struct Cli {
    /// Override local state/log directory (or set GNIREHTET_VD_HOME).
    #[arg(long, global = true, hide = true)]
    home: Option<PathBuf>,

    /// ADB executable; the same configured path is used for every operation.
    #[arg(long, global = true, hide = true)]
    adb: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the host and the Quest VPN transactionally.
    Start(StartArgs),
    /// Explicitly stop the Quest VPN, verify closure, then remove mappings.
    Stop,
    /// Show current lifecycle and host health.
    Status(StatusArgs),
    /// Recreate only the product-owned ADB reverse mappings.
    Repair,
    /// Diagnose ADB, mappings, and Virtual Desktop Streamer without changing VD.
    Doctor,
    /// Capture or export local-only diagnostics.
    Diagnostics(DiagnosticsArgs),
    /// Print the native host version.
    Version,
    /// Internal foreground runtime used by `start`.
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

#[derive(Debug, Args)]
struct StartArgs {
    /// Route only Virtual Desktop instead of the default whole-Quest connection.
    #[arg(long, conflicts_with = "all_traffic")]
    virtual_desktop_only: bool,

    /// Deprecated compatibility flag; all Quest traffic is already the default.
    #[arg(long)]
    #[arg(hide = true, conflicts_with = "virtual_desktop_only")]
    all_traffic: bool,
}

impl StartArgs {
    fn routes_all_traffic(&self) -> bool {
        self.all_traffic || !self.virtual_desktop_only
    }

    #[cfg(any(target_os = "windows", test))]
    fn from_all_traffic(all_traffic: bool) -> Self {
        Self {
            virtual_desktop_only: !all_traffic,
            all_traffic: false,
        }
    }
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DiagnosticsArgs {
    #[command(subcommand)]
    command: DiagnosticsCommand,
}

#[derive(Debug, Subcommand)]
enum DiagnosticsCommand {
    /// Capture process and relay health counters locally.
    Capture {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: u64,
    },
    /// Export a redacted JSONL support bundle. Nothing is uploaded.
    Export { path: PathBuf },
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long)]
    session: SessionId,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover(cli.home.clone()).context("locating host state directory")?;
    let configured_adb = cli
        .adb
        .clone()
        .or_else(|| std::env::var_os("GNIREHTET_VD_ADB").map(PathBuf::from));
    let adb_program = resolve_adb_program(configured_adb, &paths.root);
    let adb = AdbController::new(Arc::new(SystemAdb::new(adb_program.clone())));

    match cli.command {
        None => no_argument_entry(paths, adb_program, adb).await,
        Some(Command::Daemon(args)) => run_foreground(args.session, paths, adb, adb_program).await,
        Some(command) => {
            #[cfg(target_os = "windows")]
            {
                if command_runs_client_side(&command) {
                    let output =
                        execute_public_command(&adb_program, &paths, &adb, command).await?;
                    println!("{output}");
                    return Ok(());
                }
                let command = BrokerCommand::try_from(command)?;
                let response = send_broker_command(&paths, &adb_program, command).await?;
                emit_broker_response(response)
            }
            #[cfg(not(target_os = "windows"))]
            {
                let output = execute_public_command(&adb_program, &paths, &adb, command).await?;
                println!("{output}");
                Ok(())
            }
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn command_runs_client_side(command: &Command) -> bool {
    matches!(
        command,
        Command::Diagnostics(DiagnosticsArgs {
            command: DiagnosticsCommand::Capture { .. }
        })
    )
}

async fn execute_public_command(
    adb_program: &std::path::Path,
    paths: &AppPaths,
    adb: &AdbController,
    command: Command,
) -> Result<String> {
    match command {
        Command::Start(args) => start(adb_program, paths, adb, args).await,
        Command::Stop => stop(paths, adb).await,
        Command::Status(args) => status(paths, adb, args).await,
        Command::Repair => {
            let repaired_program = repair_adb_if_missing(adb_program.to_owned(), &paths.root)?;
            let repaired_adb = AdbController::new(Arc::new(SystemAdb::new(repaired_program)));
            repair(paths, &repaired_adb)
        }
        Command::Doctor => Ok(serde_json::to_string_pretty(&doctor(paths, adb))?),
        Command::Diagnostics(args) => diagnostics(paths, adb, args).await,
        Command::Version => Ok(format!(
            "quest-vd-wired {} (GNR4)",
            env!("CARGO_PKG_VERSION")
        )),
        Command::Daemon(_) => bail!("internal daemon commands cannot enter the public broker"),
    }
}

#[cfg(any(target_os = "windows", test))]
const BROKER_PROTOCOL_VERSION: u16 = 6;
#[cfg(any(target_os = "windows", test))]
const MAX_BROKER_FRAME: usize = 256 * 1024;
#[cfg(any(target_os = "windows", test))]
const MAX_BROKER_TEXT: usize = 32 * 1024;
#[cfg(any(target_os = "windows", test))]
const BROKER_IO_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
struct BrokerGateState {
    active: bool,
    shutting_down: bool,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Default)]
struct BrokerGate(Arc<StdMutex<BrokerGateState>>);

#[cfg(any(target_os = "windows", test))]
impl BrokerGate {
    fn begin_command(&self) -> Option<BrokerCommandGuard> {
        let mut state = self.0.lock().ok()?;
        if state.active || state.shutting_down {
            return None;
        }
        state.active = true;
        Some(BrokerCommandGuard(self.clone()))
    }

    #[cfg(test)]
    fn begin_shutdown(&self) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        if state.active || state.shutting_down {
            return false;
        }
        state.shutting_down = true;
        true
    }

    #[cfg(test)]
    fn cancel_shutdown(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.shutting_down = false;
        }
    }

    fn finish_command(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.active = false;
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> (bool, bool) {
        self.0
            .lock()
            .map(|state| (state.active, state.shutting_down))
            .unwrap()
    }
}

#[cfg(any(target_os = "windows", test))]
struct BrokerCommandGuard(BrokerGate);

#[cfg(any(target_os = "windows", test))]
impl Drop for BrokerCommandGuard {
    fn drop(&mut self) {
        self.0.finish_command();
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum BrokerCommand {
    Start { all_traffic: bool },
    Stop,
    Status { json: bool },
    Repair,
    Doctor,
    DiagnosticsCapture { duration: u64 },
    DiagnosticsExport { path: PathBuf },
    Version,
}

#[cfg(any(target_os = "windows", test))]
impl BrokerCommand {
    fn into_public_command(self) -> Result<Command> {
        Ok(match self {
            Self::Start { all_traffic } => Command::Start(StartArgs::from_all_traffic(all_traffic)),
            Self::Stop => Command::Stop,
            Self::Status { json } => Command::Status(StatusArgs { json }),
            Self::Repair => Command::Repair,
            Self::Doctor => Command::Doctor,
            Self::DiagnosticsCapture { duration } => {
                if !(1..=3600).contains(&duration) {
                    bail!("diagnostics capture duration must be between 1 and 3600 seconds");
                }
                Command::Diagnostics(DiagnosticsArgs {
                    command: DiagnosticsCommand::Capture { duration },
                })
            }
            Self::DiagnosticsExport { path } => Command::Diagnostics(DiagnosticsArgs {
                command: DiagnosticsCommand::Export { path },
            }),
            Self::Version => Command::Version,
        })
    }

    fn response_timeout(&self) -> Duration {
        match self {
            // First Start may also download and verify Android platform-tools.
            Self::Start { .. } => Duration::from_secs(15 * 60),
            Self::Stop => Duration::from_secs(30),
            Self::Status { .. } | Self::Doctor => Duration::from_secs(15),
            // The checksum-verified platform-tools bootstrap has its own
            // ten-minute subprocess deadline.
            Self::Repair => Duration::from_secs(660),
            Self::DiagnosticsCapture { duration } => {
                Duration::from_secs((*duration).min(3600) + 30)
            }
            Self::DiagnosticsExport { .. } => Duration::from_secs(60),
            Self::Version => Duration::from_secs(5),
        }
    }
}

#[cfg(any(target_os = "windows", test))]
impl TryFrom<Command> for BrokerCommand {
    type Error = anyhow::Error;

    fn try_from(command: Command) -> Result<Self> {
        Ok(match command {
            Command::Start(args) => Self::Start {
                all_traffic: args.routes_all_traffic(),
            },
            Command::Stop => Self::Stop,
            Command::Status(args) => Self::Status { json: args.json },
            Command::Repair => Self::Repair,
            Command::Doctor => Self::Doctor,
            Command::Diagnostics(args) => match args.command {
                DiagnosticsCommand::Capture { duration } => Self::DiagnosticsCapture { duration },
                DiagnosticsCommand::Export { path } => Self::DiagnosticsExport { path },
            },
            Command::Version => Self::Version,
            Command::Daemon(_) => bail!("the internal daemon cannot be forwarded to the broker"),
        })
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Deserialize, Serialize)]
struct BrokerRequest {
    protocol_version: u16,
    instance_root: PathBuf,
    adb_program: PathBuf,
    command: BrokerCommand,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct BrokerResponse {
    protocol_version: u16,
    exit_code: u8,
    stdout: String,
    stderr: String,
}

#[cfg(any(target_os = "windows", test))]
impl BrokerResponse {
    fn success(stdout: String) -> Self {
        Self {
            protocol_version: BROKER_PROTOCOL_VERSION,
            exit_code: 0,
            stdout: bounded_broker_text(stdout),
            stderr: String::new(),
        }
    }

    fn failure(error: impl std::fmt::Display) -> Self {
        Self {
            protocol_version: BROKER_PROTOCOL_VERSION,
            exit_code: 1,
            stdout: String::new(),
            stderr: bounded_broker_text(format!("Error: {error}")),
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn bounded_broker_text(mut text: String) -> String {
    const SUFFIX: &str = "\n[broker output truncated]\n";
    if text.len() > MAX_BROKER_TEXT {
        let mut boundary = MAX_BROKER_TEXT.saturating_sub(SUFFIX.len());
        while !text.is_char_boundary(boundary) {
            boundary = boundary.saturating_sub(1);
        }
        text.truncate(boundary);
        text.push_str(SUFFIX);
        return text;
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(any(target_os = "windows", test))]
async fn read_broker_frame<T, S>(stream: &mut S) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await? as usize;
    if length > MAX_BROKER_FRAME {
        bail!("broker frame length {length} exceeds the {MAX_BROKER_FRAME}-byte limit");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("decoding broker message")
}

#[cfg(any(target_os = "windows", test))]
async fn write_broker_frame<T, S>(stream: &mut S, value: &T) -> Result<()>
where
    T: Serialize,
    S: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(value).context("encoding broker message")?;
    if bytes.len() > MAX_BROKER_FRAME {
        bail!(
            "broker frame length {} exceeds the {MAX_BROKER_FRAME}-byte limit",
            bytes.len()
        );
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone)]
struct BrokerContext {
    paths: AppPaths,
    environment: Arc<StdMutex<BrokerEnvironment>>,
    adb_lane: Arc<tokio::sync::Mutex<()>>,
    gate: BrokerGate,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone)]
struct BrokerEnvironment {
    adb_program: PathBuf,
    adb: AdbController,
}

#[cfg(any(target_os = "windows", test))]
impl BrokerContext {
    fn new(paths: AppPaths, adb_program: PathBuf, adb: AdbController, gate: BrokerGate) -> Self {
        Self {
            paths,
            environment: Arc::new(StdMutex::new(BrokerEnvironment { adb_program, adb })),
            adb_lane: Arc::new(tokio::sync::Mutex::new(())),
            gate,
        }
    }

    fn environment(&self) -> Result<BrokerEnvironment> {
        self.environment
            .lock()
            .map(|environment| environment.clone())
            .map_err(|_| anyhow::Error::msg("broker ADB context lock is poisoned"))
    }

    fn replace_adb(&self, adb_program: PathBuf) -> Result<AdbController> {
        let adb = AdbController::new(Arc::new(SystemAdb::new(adb_program.clone())));
        let mut environment = self
            .environment
            .lock()
            .map_err(|_| anyhow::Error::msg("broker ADB context lock is poisoned"))?;
        environment.adb_program = adb_program;
        environment.adb = adb.clone();
        Ok(adb)
    }

    fn ensure_adb(&self) -> Result<BrokerEnvironment> {
        self.ensure_adb_with(|program, root| Ok(repair_adb_if_missing(program, root)?))
    }

    fn ensure_adb_with<F>(&self, repair: F) -> Result<BrokerEnvironment>
    where
        F: FnOnce(PathBuf, &std::path::Path) -> Result<PathBuf>,
    {
        let environment = self.environment()?;
        let adb_program = repair(environment.adb_program.clone(), &self.paths.root)?;
        if adb_program == environment.adb_program {
            return Ok(environment);
        }
        let adb = self.replace_adb(adb_program.clone())?;
        Ok(BrokerEnvironment { adb_program, adb })
    }

    async fn execute(&self, command: Command) -> Result<String> {
        match command {
            Command::Version => Ok(format!(
                "quest-vd-wired {} (GNR4)",
                env!("CARGO_PKG_VERSION")
            )),
            Command::Diagnostics(DiagnosticsArgs {
                command: DiagnosticsCommand::Export { path },
            }) => {
                let diagnostics = Diagnostics::open(&self.paths.logs)?;
                let exported = diagnostics.export(path)?;
                Ok(format!(
                    "redacted support bundle written to {}",
                    exported.display()
                ))
            }
            Command::Diagnostics(DiagnosticsArgs {
                command: DiagnosticsCommand::Capture { duration },
            }) => self.capture_diagnostics(duration).await,
            command => self.execute_adb_command(command).await,
        }
    }

    async fn execute_adb_command(&self, command: Command) -> Result<String> {
        let _adb_lane = self.adb_lane.lock().await;
        match command {
            Command::Start(args) => {
                let environment = self.ensure_adb()?;
                start(
                    &environment.adb_program,
                    &self.paths,
                    &environment.adb,
                    args,
                )
                .await
            }
            Command::Repair => {
                let environment = self.ensure_adb()?;
                repair(&self.paths, &environment.adb)
            }
            command => {
                let environment = self.environment()?;
                execute_public_command(
                    &environment.adb_program,
                    &self.paths,
                    &environment.adb,
                    command,
                )
                .await
            }
        }
    }

    async fn capture_diagnostics(&self, duration: u64) -> Result<String> {
        let diagnostics = Diagnostics::open(&self.paths.logs)?;
        let capture_id = SessionId::random().to_string();
        diagnostics.record(
            "capture_start",
            json!({"capture_id": &capture_id, "duration_seconds": duration}),
        )?;
        {
            let _adb_lane = self.adb_lane.lock().await;
            let environment = self.environment()?;
            diagnostics.record(
                "doctor_snapshot",
                json!({"capture_id": &capture_id, "boundary": "start", "report": doctor(&self.paths, &environment.adb)}),
            )?;
        }
        tokio::time::sleep(Duration::from_secs(duration)).await;
        {
            let _adb_lane = self.adb_lane.lock().await;
            let environment = self.environment()?;
            diagnostics.record(
                "doctor_snapshot",
                json!({"capture_id": &capture_id, "boundary": "end", "report": doctor(&self.paths, &environment.adb)}),
            )?;
        }
        diagnostics.record("capture_end", json!({"capture_id": &capture_id}))?;
        Ok(format!("captured {duration}s of local diagnostics"))
    }

    #[cfg(target_os = "windows")]
    async fn diagnose_and_fix(&self, desired_on: bool) -> Result<String> {
        let _adb_lane = self.adb_lane.lock().await;
        let environment = self.ensure_adb()?;
        let daemon_running =
            read_daemon_pid(&self.paths.daemon_pid).is_some_and(process_is_running);
        if desired_on {
            if daemon_running || runtime_may_be_active(&self.paths) {
                stop(&self.paths, &environment.adb).await?;
            }
            if !tray_desired_on() || tray_exit_requested() {
                return Ok("wired link left off as requested".into());
            }
            start(
                &environment.adb_program,
                &self.paths,
                &environment.adb,
                StartArgs::from_all_traffic(true),
            )
            .await
        } else {
            let report = doctor(&self.paths, &environment.adb);
            if let Err(error) = report.adb_state {
                bail!("Quest connection check failed: {error}");
            }
            Ok("ADB and local tools are ready".into())
        }
    }
}

#[cfg(any(target_os = "windows", test))]
async fn handle_broker_connection<S>(mut stream: S, context: &BrokerContext) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _command_guard = context
        .gate
        .begin_command()
        .context("per-user broker is busy or shutting down")?;
    let request: BrokerRequest =
        tokio::time::timeout(BROKER_IO_TIMEOUT, read_broker_frame(&mut stream))
            .await
            .context("broker request timed out")??;
    let response = if request.protocol_version != BROKER_PROTOCOL_VERSION {
        BrokerResponse::failure(format!(
            "broker protocol mismatch: client {}, broker {}",
            request.protocol_version, BROKER_PROTOCOL_VERSION
        ))
    } else if request.instance_root != context.paths.root {
        BrokerResponse::failure(format!(
            "this per-user broker owns {}; exit its tray before using --home {}",
            context.paths.root.display(),
            request.instance_root.display()
        ))
    } else if request.adb_program != context.environment()?.adb_program {
        BrokerResponse::failure(
            "this per-user broker owns a different ADB executable; exit its tray before changing --adb",
        )
    } else {
        match request.command.into_public_command() {
            Ok(command) => {
                #[cfg(target_os = "windows")]
                let result = match WINDOWS_TRAY_COORDINATOR.get() {
                    Some(coordinator) if command_requires_lifecycle_serialization(&command) => {
                        coordinator.execute_external(command).await
                    }
                    None => context.execute(command).await,
                    Some(_) => context.execute(command).await,
                };
                #[cfg(not(target_os = "windows"))]
                let result = context.execute(command).await;
                match result {
                    Ok(output) => BrokerResponse::success(output),
                    Err(error) => BrokerResponse::failure(format!("{error:#}")),
                }
            }
            Err(error) => BrokerResponse::failure(error),
        }
    };
    tokio::time::timeout(
        BROKER_IO_TIMEOUT,
        write_broker_frame(&mut stream, &response),
    )
    .await
    .context("broker response timed out")??;
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn command_requires_lifecycle_serialization(command: &Command) -> bool {
    matches!(command, Command::Start(_) | Command::Stop | Command::Repair)
}

#[cfg(target_os = "windows")]
fn prepare_broker_pipe() -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let name = gnirehtet_vd::runtime::windows_broker_pipe_name()?;
    gnirehtet_vd::runtime::create_secure_named_pipe(&name, true, 1)
        .context("creating the per-user broker pipe")
}

#[cfg(target_os = "windows")]
async fn serve_broker(
    mut server: tokio::net::windows::named_pipe::NamedPipeServer,
    context: BrokerContext,
) -> Result<()> {
    let name = gnirehtet_vd::runtime::windows_broker_pipe_name()?;
    loop {
        server.connect().await?;
        // Deliberately await the complete command before creating the next pipe
        // instance. This is the broker's one-command-at-a-time ownership point.
        let _ = handle_broker_connection(server, &context).await;
        server = gnirehtet_vd::runtime::create_secure_named_pipe(&name, false, 1)
            .context("recreating the per-user broker pipe")?;
    }
}

#[cfg(target_os = "windows")]
fn spawn_broker_process(paths: &AppPaths, adb_program: &std::path::Path) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let executable = std::env::current_exe().context("locating current executable")?;
    ProcessCommand::new(executable)
        .arg("--home")
        .arg(&paths.root)
        .arg("--adb")
        .arg(adb_program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .context("starting the per-user command broker")?;
    Ok(())
}

#[cfg(target_os = "windows")]
async fn open_or_start_broker_client<F>(
    name: &str,
    wait: Duration,
    start: F,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient>
where
    F: FnOnce() -> Result<()>,
{
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

    let mut start = Some(start);
    let operation = async {
        loop {
            match ClientOptions::new().open(name) {
                Ok(client) => return Ok(client),
                Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                    if let Some(start) = start.take() {
                        start().context("starting the per-user command broker")?;
                    }
                }
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {}
                Err(error) => {
                    return Err(
                        anyhow::Error::new(error).context("opening the per-user broker pipe")
                    )
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(wait, operation)
        .await
        .context("timed out waiting for the per-user command broker")?
}

#[cfg(target_os = "windows")]
async fn send_broker_command(
    paths: &AppPaths,
    adb_program: &std::path::Path,
    command: BrokerCommand,
) -> Result<BrokerResponse> {
    let response_timeout = command.response_timeout();
    let request = BrokerRequest {
        protocol_version: BROKER_PROTOCOL_VERSION,
        instance_root: paths.root.clone(),
        adb_program: adb_program.to_owned(),
        command,
    };
    let operation = async {
        let name = gnirehtet_vd::runtime::windows_broker_pipe_name()?;
        let mut stream = open_or_start_broker_client(&name, Duration::from_secs(5), || {
            spawn_broker_process(paths, adb_program)
        })
        .await?;
        write_broker_frame(&mut stream, &request).await?;
        let response: BrokerResponse = read_broker_frame(&mut stream).await?;
        if response.protocol_version != BROKER_PROTOCOL_VERSION {
            bail!(
                "another Quest VD Wired version is already running (broker protocol {}, expected {}); turn Wired link off, choose Exit, then start {}",
                response.protocol_version,
                BROKER_PROTOCOL_VERSION,
                env!("CARGO_PKG_VERSION")
            );
        }
        Ok(response)
    };
    tokio::time::timeout(response_timeout + Duration::from_secs(5), operation)
        .await
        .context("the per-user broker command exceeded its bounded deadline")?
}

#[cfg(target_os = "windows")]
fn emit_broker_response(response: BrokerResponse) -> Result<()> {
    std::io::stdout().write_all(response.stdout.as_bytes())?;
    std::io::stdout().flush()?;
    std::io::stderr().write_all(response.stderr.as_bytes())?;
    std::io::stderr().flush()?;
    if response.exit_code != 0 {
        std::process::exit(response.exit_code.into());
    }
    Ok(())
}

async fn run_foreground(
    session_id: SessionId,
    paths: AppPaths,
    adb: AdbController,
    adb_program: PathBuf,
) -> Result<()> {
    HostRuntime::new(RuntimeConfig::new(session_id, paths, adb, adb_program))?
        .run()
        .await?;
    Ok(())
}

async fn start(
    daemon_adb: &std::path::Path,
    paths: &AppPaths,
    adb: &AdbController,
    args: StartArgs,
) -> Result<String> {
    let _operation = OperationGuard::acquire(&paths.operation_lock)
        .context("another start/stop/repair operation is active")?;
    if let Some(pid) = read_daemon_pid(&paths.daemon_pid).filter(|pid| process_is_running(*pid)) {
        bail!("host daemon is already running as PID {pid}");
    }
    if runtime_may_be_active(paths) {
        bail!("host runtime activity exists without a verified daemon identity; refusing APK install/start");
    }
    let apk_path = paths.root.join("gnirehtet-v4.apk");
    embedded::materialize(&apk_path)?;
    adb.install_matching_apk(&apk_path)?;
    let session = SessionId::random();
    let mut daemon = spawn_daemon(daemon_adb, paths, session)?;
    let daemon_pid = daemon.id();
    if let Err(error) = wait_for_runtime_ready(paths, daemon_pid).await {
        let _ = terminate_spawned_daemon(&mut daemon);
        return Err(error);
    }

    let all_traffic = args.routes_all_traffic();
    let start_result = adb.start(session, all_traffic);
    if let Err(error) = start_result {
        let _ = terminate_spawned_daemon(&mut daemon);
        let snapshot = StateSnapshot {
            state: HostState::Error,
            session_id: Some(session.to_string()),
            missed_heartbeats: 0,
            reason: Some("Android start transaction failed".into()),
        };
        let _ = StateStore::new(&paths.status).write(&snapshot, None);
        return Err(error.into());
    }
    if let Err(error) = wait_for_connected(paths, session, &mut daemon) {
        terminate_spawned_daemon(&mut daemon)
            .context("quiescing host monitor before startup rollback")?;
        let rollback_error = adb.stop().err();
        let reason = if rollback_error.is_some() {
            "Android did not acknowledge GNR4 STARTED; rollback could not verify VPN closure"
        } else {
            "Android did not acknowledge GNR4 STARTED; rollback completed"
        };
        let snapshot = StateSnapshot {
            state: HostState::Error,
            session_id: Some(session.to_string()),
            missed_heartbeats: 0,
            reason: Some(reason.into()),
        };
        let _ = StateStore::new(&paths.status).write(&snapshot, None);
        if let Some(rollback_error) = rollback_error {
            return Err(error.context(format!(
                "startup rollback failed and VPN state remains unverified: {rollback_error}"
            )));
        }
        return Err(error);
    }
    let routing = if all_traffic {
        "all Quest traffic"
    } else {
        VIRTUAL_DESKTOP_PACKAGE
    };
    Ok(format!(
        "wired link connected (session {session}, routing {routing}, daemon PID {daemon_pid})"
    ))
}

async fn stop(paths: &AppPaths, adb: &AdbController) -> Result<String> {
    let _operation = OperationGuard::acquire(&paths.operation_lock)
        .context("another start/stop/repair operation is active")?;
    // The health monitor uses sub-second mapping commands and checks the Stop
    // flag between them. This envelope still covers one in-flight monitor
    // command plus the six-second peer acknowledgement deadline.
    let control_stop = admin_command(paths, "stop", Duration::from_secs(8)).await;
    let stop_result = match control_stop {
        Ok(response) if admin_stop_acknowledges_suppression(&response) => {
            match adb.finish_control_stop() {
                Ok(()) => Ok(()),
                Err(_) => adb.stop(),
            }
        }
        // Only this typed acknowledgement proves the stop handler suppressed
        // and drained repairs. Authentication/unknown-command errors do not.
        Ok(response) if response.repairs_suppressed => adb.stop(),
        command_failure => {
            let command_error = match command_failure {
                Ok(response) => response
                    .error
                    .unwrap_or_else(|| "repairs were not suppressed".into()),
                Err(error) => error.to_string(),
            };
            // No authenticated response means suppression is unproven. If an
            // exact daemon identity exists, terminating that verified handle
            // closes its mandatory kill-on-close Job Object and quiesces every
            // inherited ADB child before teardown continues. Never infer
            // safety from a missing/corrupt PID file while listeners/lock live.
            if read_daemon_pid(&paths.daemon_pid).is_some() {
                terminate_daemon(&paths.daemon_pid, Duration::from_secs(3))
                    .context("quiescing verified daemon after command-lane failure")?;
                let _ = std::fs::remove_file(&paths.daemon_pid);
                // The daemon Job Object kills its ADB client descendants, but
                // the persistent ADB server may already have accepted the last
                // reverse request. Let that bounded command window drain, then
                // require a fresh Stop transaction to remove and verify.
                thread::sleep(ADB_MAPPING_COMMAND_TIMEOUT);
                bail!(
                    "host command lane failed ({command_error}); the verified daemon and ADB clients were quiesced and the mapping-command drain elapsed without changing the VPN; run `quest-vd-wired stop` again"
                );
            } else if runtime_may_be_active(paths) {
                bail!(
                    "host command lane failed ({command_error}) and daemon identity is unavailable while runtime activity is still visible; refusing ADB teardown to avoid racing mapping repair"
                );
            }
            adb.stop()
        }
    };
    if read_daemon_pid(&paths.daemon_pid).is_some() {
        if stop_result.is_ok() {
            let shutdown = admin_command(paths, "shutdown", Duration::from_secs(3)).await;
            if shutdown.is_ok_and(|response| response.ok) {
                if wait_for_daemon_exit(paths, Duration::from_secs(3))
                    .await
                    .is_err()
                {
                    terminate_daemon(&paths.daemon_pid, Duration::from_secs(3))
                        .context("terminating verified daemon after graceful timeout")?;
                }
            } else {
                terminate_daemon(&paths.daemon_pid, Duration::from_secs(3))
                    .context("terminating verified stopped daemon")?;
            }
        } else {
            terminate_daemon(&paths.daemon_pid, Duration::from_secs(3))
                .context("terminating verified daemon after failed stop")?;
        }
    }
    let _ = std::fs::remove_file(&paths.daemon_pid);
    let store = StateStore::new(&paths.status);
    match stop_result {
        Ok(()) => {
            store.write(
                &StateSnapshot {
                    state: HostState::Stopped,
                    session_id: None,
                    missed_heartbeats: 0,
                    reason: None,
                },
                None,
            )?;
            Ok("wired link stopped; Quest VPN closure verified and mappings removed".into())
        }
        Err(error) => {
            store.write(
                &StateSnapshot {
                    state: HostState::Error,
                    session_id: None,
                    missed_heartbeats: 0,
                    reason: Some("explicit stop could not be verified".into()),
                },
                None,
            )?;
            Err(error.into())
        }
    }
}

fn admin_stop_acknowledges_suppression(response: &AdminResponse) -> bool {
    response.ok && response.repairs_suppressed
}

async fn status(paths: &AppPaths, adb: &AdbController, args: StatusArgs) -> Result<String> {
    let mut status = StateStore::new(&paths.status).read_or_stopped();
    status.daemon_running = read_daemon_pid(&paths.daemon_pid).is_some();
    if status.daemon_running {
        if let Ok(response) = admin_command(paths, "status", Duration::from_secs(1)).await {
            if response.ok {
                if let Some(live) = response.status {
                    status.lifecycle = live;
                }
            }
        }
    } else {
        status.runtime_ready = false;
        reflect_missing_runtime(&mut status.lifecycle);
    }
    let android = adb.android_status();
    if args.json {
        Ok(serde_json::to_string_pretty(&json!({
            "host": status,
            "android": android.as_ref().ok(),
            "android_error": android.as_ref().err().map(ToString::to_string),
            "logging": {
                "directory": &paths.logs,
                "format": "jsonl",
                "max_bytes_per_file": DEFAULT_MAX_BYTES,
                "file_count": DEFAULT_FILE_COUNT,
                "max_total_bytes": DEFAULT_TOTAL_BYTES,
                "oldest_file_deleted_on_rotation": true,
            },
        }))?)
    } else {
        let mut lines = vec![
            format!("state: {:?}", status.lifecycle.state),
            format!(
                "daemon: {}",
                if status.daemon_running {
                    "running"
                } else {
                    "not running"
                }
            ),
            format!(
                "logs: {} ({} x {} MiB; oldest auto-deleted)",
                paths.logs.display(),
                DEFAULT_FILE_COUNT,
                DEFAULT_MAX_BYTES / (1024 * 1024)
            ),
        ];
        if let Some(reason) = status.lifecycle.reason {
            lines.push(format!("reason: {reason}"));
        }
        if let Some(telemetry) = status.telemetry {
            lines.push(format!(
                "queue residence: p99={}us max={}us; queued={} bytes",
                telemetry.relay.udp.queue_residence_us.p99_us,
                telemetry.relay.udp.queue_residence_us.max_us,
                telemetry.relay.udp.queued_bytes
            ));
            lines.push(format!(
                "control: generation={} reconnects={} echo-p99={}us",
                telemetry.control.connection_generation,
                telemetry.control.reconnects,
                telemetry.control.heartbeat_echo_service_us.p99_us
            ));
            lines.push(format!(
                "adb: device={} mappings={} reconnect-generation={}{}",
                telemetry.adb.device_available,
                telemetry.adb.mappings_healthy,
                telemetry.adb.reconnect_generation,
                telemetry
                    .adb
                    .last_error
                    .as_deref()
                    .map(|category| format!(" category={category}"))
                    .unwrap_or_default()
            ));
        }
        match android {
            Ok(android) => lines.push(format!(
                "android: {} fd_open={:?} rtt_p99={:?}us rtt_max={:?}us",
                android.state.as_deref().unwrap_or("not running"),
                android.vpn_fd_open,
                android.control_rtt_p99_us,
                android.control_rtt_max_us
            )),
            Err(error) => lines.push(format!("android status unavailable: {error}")),
        }
        Ok(lines.join("\n"))
    }
}

fn reflect_missing_runtime(snapshot: &mut StateSnapshot) {
    match snapshot.state {
        HostState::Preparing | HostState::Connected | HostState::Degraded => {
            snapshot.state = HostState::Degraded;
            snapshot.reason =
                Some("host runtime is not running; Quest VPN state is retained".into());
        }
        HostState::Stopping => {
            snapshot.state = HostState::Error;
            snapshot.reason =
                Some("host runtime exited during Stop; VPN state is unverified".into());
        }
        HostState::Stopped | HostState::Error => {}
    }
}

fn repair(paths: &AppPaths, adb: &AdbController) -> Result<String> {
    let _operation = OperationGuard::acquire(&paths.operation_lock)
        .context("another start/stop/repair operation is active")?;
    if read_daemon_pid(&paths.daemon_pid).is_some() || runtime_may_be_active(paths) {
        bail!("host daemon is active; its serialized ADB monitor owns mapping repair");
    }
    adb.repair_mappings()?;
    Ok("product ADB reverse mappings repaired".into())
}

async fn diagnostics(
    paths: &AppPaths,
    adb: &AdbController,
    args: DiagnosticsArgs,
) -> Result<String> {
    let diagnostics = Diagnostics::open(&paths.logs)?;
    match args.command {
        DiagnosticsCommand::Capture { duration } => {
            let capture_id = SessionId::random().to_string();
            diagnostics.record(
                "capture_start",
                json!({"capture_id": &capture_id, "duration_seconds": duration}),
            )?;
            diagnostics.record(
                "doctor_snapshot",
                json!({"capture_id": &capture_id, "boundary": "start", "report": doctor(paths, adb)}),
            )?;
            tokio::time::sleep(Duration::from_secs(duration)).await;
            diagnostics.record(
                "doctor_snapshot",
                json!({"capture_id": &capture_id, "boundary": "end", "report": doctor(paths, adb)}),
            )?;
            diagnostics.record("capture_end", json!({"capture_id": &capture_id}))?;
            Ok(format!("captured {duration}s of local diagnostics"))
        }
        DiagnosticsCommand::Export { path } => {
            let exported = diagnostics.export(path)?;
            Ok(format!(
                "redacted support bundle written to {}",
                exported.display()
            ))
        }
    }
}

fn spawn_daemon(adb: &std::path::Path, paths: &AppPaths, session: SessionId) -> Result<Child> {
    let executable = std::env::current_exe().context("locating current executable")?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("--home")
        .arg(&paths.root)
        .arg("daemon")
        .arg("--session")
        .arg(session.to_string())
        .arg("--adb")
        .arg(adb)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    command.spawn().context("starting host daemon")
}

fn terminate_spawned_daemon(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    let _ = child.wait()?;
    Ok(())
}

async fn wait_for_runtime_ready(paths: &AppPaths, pid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_is_running(pid) {
            bail!("host daemon exited before binding listeners");
        }
        let persisted_ready = StateStore::new(&paths.status).read().is_ok_and(|status| {
            status.daemon_pid == Some(pid) && status.daemon_running && status.runtime_ready
        });
        if persisted_ready {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("host daemon did not become ready within 3 seconds")
}

fn runtime_may_be_active(paths: &AppPaths) -> bool {
    let lock_owner_running = std::fs::read_to_string(&paths.runtime_lock)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .is_some_and(process_is_running);
    lock_owner_running
}

async fn wait_for_daemon_exit(paths: &AppPaths, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if read_daemon_pid(&paths.daemon_pid).is_none() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("daemon did not exit before {timeout:?}")
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Eq, PartialEq)]
enum NoArgumentTrayAction<T> {
    StartPrimary(T),
    ExitWithoutBroker,
}

#[cfg(any(target_os = "windows", test))]
fn no_argument_tray_action<T>(instance: Option<T>) -> NoArgumentTrayAction<T> {
    match instance {
        Some(instance) => NoArgumentTrayAction::StartPrimary(instance),
        None => NoArgumentTrayAction::ExitWithoutBroker,
    }
}

async fn no_argument_entry(
    paths: AppPaths,
    adb_program: PathBuf,
    adb: AdbController,
) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // Keep the console subsystem for CLI use, but detach the no-argument
        // Explorer/tray path from any inherited console without hiding a
        // caller's shared terminal window.
        unsafe {
            windows_sys::Win32::System::Console::FreeConsole();
        }
        let instance = match no_argument_tray_action(acquire_tray_instance()?) {
            NoArgumentTrayAction::StartPrimary(instance) => instance,
            // The existing window was already notified. Exit immediately: a
            // broker connection here could spawn copies while it starts up.
            NoArgumentTrayAction::ExitWithoutBroker => return Ok(()),
        };
        let class_name = instance.class_name.clone();
        // Prepare the first, ACL-restricted pipe before exposing the tray icon.
        // A visible primary instance therefore always has a listening broker.
        let server = prepare_broker_pipe()?;
        let gate = BrokerGate::default();
        let context = BrokerContext::new(paths, adb_program, adb, gate.clone());
        let broker_context = context.clone();
        let runtime = tokio::runtime::Handle::current();
        let coordinator = TrayCoordinatorHandle::spawn(context.clone(), runtime.clone());
        WINDOWS_TRAY_COORDINATOR
            .set(coordinator)
            .map_err(|_| anyhow::Error::msg("tray coordinator was initialized more than once"))?;
        let mut broker = tokio::spawn(serve_broker(server, broker_context));
        let mut tray = tokio::task::spawn_blocking(move || run_windows_tray(instance, context));
        tokio::select! {
            tray_result = &mut tray => {
                broker.abort();
                let _ = broker.await;
                tray_result
                    .context("notification-area task failed")?
                    .context("running Windows notification-area shell")
            }
            broker_result = &mut broker => {
                let error = match broker_result {
                    Ok(Ok(())) => anyhow::Error::msg("per-user command broker exited unexpectedly"),
                    Ok(Err(error)) => error,
                    Err(error) => anyhow::Error::new(error).context("per-user broker task failed"),
                };
                request_tray_exit(&class_name);
                let _ = tray.await;
                Err(error)
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (adb_program, adb);
        if let Ok(response) = admin_command(&paths, "status", Duration::from_secs(1)).await {
            println!("{}", serde_json::to_string_pretty(&response)?);
            return Ok(());
        }
        bail!("no-argument notification-area mode is Windows-only; use `quest-vd-wired start`")
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrayCommand {
    ToggleLink,
    DiagnoseAndFix,
    Exit,
}

#[cfg(any(target_os = "windows", test))]
const TRAY_TOGGLE_ID: usize = 1_001;
#[cfg(any(target_os = "windows", test))]
const TRAY_DIAGNOSE_ID: usize = 1_002;
#[cfg(any(target_os = "windows", test))]
const TRAY_EXIT_ID: usize = 1_003;

#[cfg(any(target_os = "windows", test))]
fn tray_command_from_id(id: usize) -> Option<TrayCommand> {
    match id {
        TRAY_TOGGLE_ID => Some(TrayCommand::ToggleLink),
        TRAY_DIAGNOSE_ID => Some(TrayCommand::DiagnoseAndFix),
        TRAY_EXIT_ID => Some(TrayCommand::Exit),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn tray_command_id(command: TrayCommand) -> usize {
    match command {
        TrayCommand::ToggleLink => TRAY_TOGGLE_ID,
        TrayCommand::DiagnoseAndFix => TRAY_DIAGNOSE_ID,
        TrayCommand::Exit => TRAY_EXIT_ID,
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TrayMenuEntry {
    command: Option<TrayCommand>,
    label: Option<&'static str>,
    checked: bool,
}

#[cfg(any(target_os = "windows", test))]
fn tray_menu_entries(desired_on: bool) -> [TrayMenuEntry; 4] {
    [
        TrayMenuEntry {
            command: Some(TrayCommand::ToggleLink),
            label: Some("Wired link"),
            checked: desired_on,
        },
        TrayMenuEntry {
            command: Some(TrayCommand::DiagnoseAndFix),
            label: Some("Diagnose and fix"),
            checked: false,
        },
        TrayMenuEntry {
            command: None,
            label: None,
            checked: false,
        },
        TrayMenuEntry {
            command: Some(TrayCommand::Exit),
            label: Some("Exit"),
            checked: false,
        },
    ]
}

#[cfg(target_os = "windows")]
static WINDOWS_TRAY_RUNTIME: std::sync::OnceLock<WindowsTrayRuntime> = std::sync::OnceLock::new();
#[cfg(target_os = "windows")]
static WINDOWS_TRAY_COORDINATOR: std::sync::OnceLock<TrayCoordinatorHandle> =
    std::sync::OnceLock::new();
#[cfg(target_os = "windows")]
static WINDOWS_TRAY_ICONS: std::sync::OnceLock<WindowsTrayIcons> = std::sync::OnceLock::new();
#[cfg(target_os = "windows")]
static TRAY_INTENT: AtomicU8 = AtomicU8::new(if TRAY_DEFAULT_ON {
    TRAY_INTENT_ON
} else {
    TRAY_INTENT_OFF
});
#[cfg(target_os = "windows")]
static TRAY_DIAGNOSE_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "windows")]
static TRAY_WINDOW: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_os = "windows")]
static TRAY_LAST_ERROR: StdMutex<Option<String>> = StdMutex::new(None);

#[cfg(any(target_os = "windows", test))]
const TRAY_ICON_OFF_ICO: &[u8] = include_bytes!("../assets/tray-off.ico");
#[cfg(any(target_os = "windows", test))]
const TRAY_ICON_ON_ICO: &[u8] = include_bytes!("../assets/tray-on.ico");
#[cfg(any(target_os = "windows", test))]
const TRAY_DEFAULT_ON: bool = true;
#[cfg(target_os = "windows")]
const TRAY_INTENT_ON: u8 = 0;
#[cfg(target_os = "windows")]
const TRAY_INTENT_OFF: u8 = 1;
#[cfg(target_os = "windows")]
const TRAY_INTENT_EXITING: u8 = 2;

#[cfg(target_os = "windows")]
fn tray_desired_on() -> bool {
    TRAY_INTENT.load(Ordering::Acquire) == TRAY_INTENT_ON
}

#[cfg(target_os = "windows")]
fn tray_exit_requested() -> bool {
    TRAY_INTENT.load(Ordering::Acquire) == TRAY_INTENT_EXITING
}
#[cfg(test)]
fn tray_initial_desired_on() -> bool {
    TRAY_DEFAULT_ON
}
#[cfg(any(target_os = "windows", test))]
const TRAY_RETRY_INITIAL: Duration = Duration::from_millis(750);
#[cfg(any(target_os = "windows", test))]
const TRAY_RETRY_MAX: Duration = Duration::from_secs(8);

#[cfg(any(target_os = "windows", test))]
fn next_tray_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(TRAY_RETRY_MAX)
}

#[cfg(any(target_os = "windows", test))]
fn ico_image_for_size(ico: &[u8], desired: u32) -> Option<&[u8]> {
    if ico.len() < 6
        || u16::from_le_bytes([ico[0], ico[1]]) != 0
        || u16::from_le_bytes([ico[2], ico[3]]) != 1
    {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
    let entries_end = 6usize.checked_add(count.checked_mul(16)?)?;
    if count == 0 || entries_end > ico.len() {
        return None;
    }
    let mut selected = None;
    let mut selected_distance = u32::MAX;
    let mut selected_width = 0;
    for index in 0..count {
        let entry = 6 + index * 16;
        let width = if ico[entry] == 0 {
            256
        } else {
            u32::from(ico[entry])
        };
        let height = if ico[entry + 1] == 0 {
            256
        } else {
            u32::from(ico[entry + 1])
        };
        if width != height {
            continue;
        }
        let length = u32::from_le_bytes(ico[entry + 8..entry + 12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(ico[entry + 12..entry + 16].try_into().ok()?) as usize;
        let Some(end) = offset.checked_add(length) else {
            continue;
        };
        if offset < entries_end || end > ico.len() || length == 0 {
            continue;
        }
        let distance = width.abs_diff(desired);
        if distance < selected_distance || (distance == selected_distance && width > selected_width)
        {
            selected = Some(&ico[offset..end]);
            selected_distance = distance;
            selected_width = width;
        }
    }
    selected
}

#[cfg(target_os = "windows")]
struct WindowsTrayIcons {
    off: usize,
    on: usize,
}

#[cfg(target_os = "windows")]
impl WindowsTrayIcons {
    fn load() -> std::io::Result<Self> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateIconFromResourceEx, DestroyIcon, LR_DEFAULTCOLOR,
        };

        fn load_one(ico: &[u8]) -> std::io::Result<usize> {
            let image = ico_image_for_size(ico, 32)
                .ok_or_else(|| std::io::Error::other("embedded tray icon is malformed"))?;
            let icon = unsafe {
                CreateIconFromResourceEx(
                    image.as_ptr(),
                    image
                        .len()
                        .try_into()
                        .map_err(|_| std::io::Error::other("embedded tray icon is too large"))?,
                    1,
                    0x0003_0000,
                    32,
                    32,
                    LR_DEFAULTCOLOR,
                )
            };
            if icon.is_null() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(icon as usize)
            }
        }

        let off = load_one(TRAY_ICON_OFF_ICO)?;
        match load_one(TRAY_ICON_ON_ICO) {
            Ok(on) => Ok(Self { off, on }),
            Err(error) => {
                unsafe {
                    DestroyIcon(off as windows_sys::Win32::UI::WindowsAndMessaging::HICON);
                }
                Err(error)
            }
        }
    }

    fn get() -> std::io::Result<&'static Self> {
        if let Some(icons) = WINDOWS_TRAY_ICONS.get() {
            return Ok(icons);
        }
        let icons = Self::load()?;
        let _ = WINDOWS_TRAY_ICONS.set(icons);
        WINDOWS_TRAY_ICONS
            .get()
            .ok_or_else(|| std::io::Error::other("tray icons were not initialized"))
    }
}

#[cfg(target_os = "windows")]
struct WindowsTrayRuntime {
    context: BrokerContext,
}

#[cfg(target_os = "windows")]
#[derive(Clone)]
struct TrayCoordinatorHandle {
    sender: tokio::sync::mpsc::UnboundedSender<TrayCoordinatorRequest>,
}

#[cfg(any(target_os = "windows", test))]
enum TrayCoordinatorRequest {
    Wake {
        report_stop_error: bool,
    },
    HostResume,
    Diagnose,
    Exit,
    External {
        command: Command,
        reply: tokio::sync::oneshot::Sender<std::result::Result<String, String>>,
    },
}

#[cfg(any(target_os = "windows", test))]
type TrayCoordinatorFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[cfg(any(target_os = "windows", test))]
trait TrayCoordinatorBackend: Send + Sync + 'static {
    fn host_resume(&self) -> TrayCoordinatorFuture<'_, Result<()>>;
    fn diagnose(&self) -> TrayCoordinatorFuture<'_, Result<()>>;
    fn execute_external(&self, command: Command) -> TrayCoordinatorFuture<'_, Result<String>>;
    fn reconcile(&self) -> TrayCoordinatorFuture<'_, Result<bool>>;
    fn exit_requested(&self) -> bool;
    fn verified_off(&self) -> bool;
    fn report_error(&self, error: String);
    fn diagnosis_complete(&self);
    fn exit_complete(&self);
}

#[cfg(target_os = "windows")]
struct WindowsTrayBackend {
    context: BrokerContext,
}

#[cfg(target_os = "windows")]
impl TrayCoordinatorBackend for WindowsTrayBackend {
    fn host_resume(&self) -> TrayCoordinatorFuture<'_, Result<()>> {
        Box::pin(async move {
            if !tray_desired_on()
                || !read_daemon_pid(&self.context.paths.daemon_pid).is_some_and(process_is_running)
            {
                return Ok(());
            }
            let mut last_error = None;
            for attempt in 0..3 {
                match admin_command(&self.context.paths, "host_resume", Duration::from_secs(4))
                    .await
                {
                    Ok(response) if response.ok => return Ok(()),
                    Ok(response) => {
                        last_error = Some(
                            response
                                .error
                                .unwrap_or_else(|| "runtime rejected host resume recovery".into()),
                        );
                    }
                    Err(error) => last_error = Some(error.to_string()),
                }
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            bail!(
                "runtime did not accept host resume recovery: {}",
                last_error.unwrap_or_else(|| "unknown error".into())
            )
        })
    }

    fn diagnose(&self) -> TrayCoordinatorFuture<'_, Result<()>> {
        Box::pin(async move {
            self.context
                .diagnose_and_fix(tray_desired_on())
                .await
                .map(|_| ())
        })
    }

    fn execute_external(&self, command: Command) -> TrayCoordinatorFuture<'_, Result<String>> {
        Box::pin(async move { self.context.execute(command).await })
    }

    fn reconcile(&self) -> TrayCoordinatorFuture<'_, Result<bool>> {
        Box::pin(async move { reconcile_tray_desired_state(&self.context).await })
    }

    fn exit_requested(&self) -> bool {
        tray_exit_requested()
    }

    fn verified_off(&self) -> bool {
        TrayHostObservation::capture(&self.context.paths).is_verified_off()
    }

    fn report_error(&self, error: String) {
        report_tray_error(error);
    }

    fn diagnosis_complete(&self) {
        TRAY_DIAGNOSE_ACTIVE.store(false, Ordering::Release);
    }

    fn exit_complete(&self) {
        let hwnd = TRAY_WINDOW.load(Ordering::Acquire) as windows_sys::Win32::Foundation::HWND;
        if !hwnd.is_null() {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    hwnd,
                    TRAY_EXIT_COMPLETE_MESSAGE,
                    0,
                    0,
                );
            }
        }
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrayReconcileAction {
    None,
    Start,
    Stop,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy)]
struct TrayHostObservation {
    lifecycle: HostState,
    daemon_running: bool,
    runtime_visible: bool,
}

#[cfg(any(target_os = "windows", test))]
impl TrayHostObservation {
    #[cfg(target_os = "windows")]
    fn capture(paths: &AppPaths) -> Self {
        let lifecycle = StateStore::new(&paths.status)
            .read_or_stopped()
            .lifecycle
            .state;
        let daemon_running = read_daemon_pid(&paths.daemon_pid).is_some_and(process_is_running);
        Self {
            lifecycle,
            daemon_running,
            runtime_visible: runtime_may_be_active(paths),
        }
    }

    fn is_verified_off(self) -> bool {
        !self.daemon_running && !self.runtime_visible && self.lifecycle == HostState::Stopped
    }
}

#[cfg(any(target_os = "windows", test))]
fn tray_reconcile_action(
    desired_on: bool,
    observation: TrayHostObservation,
) -> TrayReconcileAction {
    if !desired_on {
        return if observation.is_verified_off() {
            TrayReconcileAction::None
        } else {
            TrayReconcileAction::Stop
        };
    }
    if observation.daemon_running {
        return if matches!(
            observation.lifecycle,
            HostState::Preparing | HostState::Connected | HostState::Degraded
        ) {
            TrayReconcileAction::None
        } else {
            TrayReconcileAction::Stop
        };
    }
    if observation.runtime_visible
        || matches!(
            observation.lifecycle,
            HostState::Connected | HostState::Degraded | HostState::Stopping | HostState::Error
        )
    {
        TrayReconcileAction::Stop
    } else {
        TrayReconcileAction::Start
    }
}

#[cfg(any(target_os = "windows", test))]
fn tray_should_report_reconcile_error(report_stop_error: bool, exit_requested: bool) -> bool {
    report_stop_error || exit_requested
}

#[cfg(any(target_os = "windows", test))]
fn tray_tooltip(desired_on: bool, observation: Option<TrayHostObservation>) -> &'static str {
    if !desired_on {
        return if observation.is_some_and(TrayHostObservation::is_verified_off) {
            "Wired link: off"
        } else {
            "Wired link: stopping"
        };
    }
    if observation
        .is_some_and(|state| state.daemon_running && state.lifecycle == HostState::Connected)
    {
        "Quest VD Wired — connected"
    } else if observation.is_some_and(|state| state.lifecycle == HostState::Degraded) {
        "Wired link: headset asleep — reconnecting"
    } else {
        "Wired link: connecting — allow USB debugging"
    }
}

#[cfg(target_os = "windows")]
impl TrayCoordinatorHandle {
    fn spawn(context: BrokerContext, runtime: tokio::runtime::Handle) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        runtime.spawn(run_tray_coordinator(
            receiver,
            WindowsTrayBackend { context },
        ));
        Self { sender }
    }

    fn toggle(&self) {
        let desired_on = loop {
            let current = TRAY_INTENT.load(Ordering::Acquire);
            if current == TRAY_INTENT_EXITING {
                return;
            }
            let next = if current == TRAY_INTENT_ON {
                TRAY_INTENT_OFF
            } else {
                TRAY_INTENT_ON
            };
            if TRAY_INTENT
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break next == TRAY_INTENT_ON;
            }
        };
        let _ = self.sender.send(TrayCoordinatorRequest::Wake {
            report_stop_error: !desired_on,
        });
    }

    fn diagnose(&self) {
        if tray_exit_requested()
            || TRAY_DIAGNOSE_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        if self.sender.send(TrayCoordinatorRequest::Diagnose).is_err() {
            TRAY_DIAGNOSE_ACTIVE.store(false, Ordering::Release);
        }
    }

    fn host_resume(&self) {
        if tray_exit_requested() || !tray_desired_on() {
            return;
        }
        let _ = self.sender.send(TrayCoordinatorRequest::HostResume);
    }

    fn exit(&self) {
        TRAY_INTENT.store(TRAY_INTENT_EXITING, Ordering::Release);
        let _ = self.sender.send(TrayCoordinatorRequest::Exit);
    }

    async fn execute_external(&self, command: Command) -> Result<String> {
        match command {
            Command::Start(_) => loop {
                let current = TRAY_INTENT.load(Ordering::Acquire);
                if current == TRAY_INTENT_EXITING {
                    bail!(
                        "the notification-area app is exiting; start it again after Exit completes"
                    );
                }
                if TRAY_INTENT
                    .compare_exchange(current, TRAY_INTENT_ON, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            },
            Command::Stop => {
                let _ = TRAY_INTENT.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    (current != TRAY_INTENT_EXITING).then_some(TRAY_INTENT_OFF)
                });
            }
            _ => {}
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        self.sender
            .send(TrayCoordinatorRequest::External { command, reply })
            .map_err(|_| anyhow::Error::msg("tray coordinator is unavailable"))?;
        response
            .await
            .map_err(|_| anyhow::Error::msg("tray coordinator stopped before replying"))?
            .map_err(anyhow::Error::msg)
    }
}

#[cfg(target_os = "windows")]
fn report_tray_error(error: impl Into<String>) {
    if let Ok(mut last_error) = TRAY_LAST_ERROR.lock() {
        *last_error = Some(error.into());
    }
    let hwnd = TRAY_WINDOW.load(Ordering::Acquire) as windows_sys::Win32::Foundation::HWND;
    if !hwnd.is_null() {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                hwnd,
                TRAY_ERROR_MESSAGE,
                0,
                0,
            );
        }
    }
}

#[cfg(target_os = "windows")]
async fn reconcile_tray_desired_state(context: &BrokerContext) -> Result<bool> {
    let observation = TrayHostObservation::capture(&context.paths);
    match tray_reconcile_action(tray_desired_on(), observation) {
        TrayReconcileAction::None => {}
        TrayReconcileAction::Start => {
            context
                .execute(Command::Start(StartArgs::from_all_traffic(true)))
                .await?;
        }
        TrayReconcileAction::Stop => {
            context.execute(Command::Stop).await?;
        }
    }
    let observation = TrayHostObservation::capture(&context.paths);
    Ok(tray_reconcile_action(tray_desired_on(), observation) == TrayReconcileAction::None)
}

#[cfg(any(target_os = "windows", test))]
async fn run_tray_coordinator<B>(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<TrayCoordinatorRequest>,
    backend: B,
) where
    B: TrayCoordinatorBackend,
{
    let mut retry_delay = TRAY_RETRY_INITIAL;
    let mut report_next_stop_error = false;
    let mut report_exit_error = false;
    loop {
        let request = tokio::select! {
            request = receiver.recv() => request,
            _ = tokio::time::sleep(retry_delay) => {
                Some(TrayCoordinatorRequest::Wake { report_stop_error: false })
            },
        };
        let Some(request) = request else {
            return;
        };
        let explicit_request = !matches!(
            &request,
            TrayCoordinatorRequest::Wake {
                report_stop_error: false
            }
        );
        if explicit_request {
            retry_delay = TRAY_RETRY_INITIAL;
        }
        match request {
            TrayCoordinatorRequest::Wake { report_stop_error } => {
                report_next_stop_error |= report_stop_error;
            }
            TrayCoordinatorRequest::HostResume => {
                if let Err(error) = backend.host_resume().await {
                    backend.report_error(format!(
                        "The wired link could not recover after Windows resumed: {error:#}"
                    ));
                }
            }
            TrayCoordinatorRequest::Diagnose => {
                if let Err(error) = backend.diagnose().await {
                    backend.report_error(format!("Diagnose and fix could not finish: {error:#}"));
                }
                backend.diagnosis_complete();
            }
            TrayCoordinatorRequest::Exit => {
                report_exit_error = true;
            }
            TrayCoordinatorRequest::External { command, reply } => {
                let result = backend
                    .execute_external(command)
                    .await
                    .map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
            }
        }

        let converged = match backend.reconcile().await {
            Ok(converged) => converged,
            Err(error) => {
                retry_delay = next_tray_retry_delay(retry_delay);
                if tray_should_report_reconcile_error(report_next_stop_error, report_exit_error) {
                    backend.report_error(format!(
                        "The wired link could not stop yet. Reconnect and unlock the headset, then try again: {error:#}"
                    ));
                    report_next_stop_error = false;
                    report_exit_error = false;
                }
                continue;
            }
        };

        if converged {
            retry_delay = TRAY_RETRY_INITIAL;
        }
        report_next_stop_error = false;
        if backend.exit_requested() && backend.verified_off() {
            backend.exit_complete();
            return;
        }
    }
}

#[cfg(target_os = "windows")]
const TRAY_CALLBACK_MESSAGE: u32 = 0x8000 + 41;
#[cfg(target_os = "windows")]
const TRAY_SHOW_EXISTING_MESSAGE: u32 = 0x8000 + 42;
#[cfg(target_os = "windows")]
const TRAY_EXIT_COMPLETE_MESSAGE: u32 = 0x8000 + 43;
#[cfg(target_os = "windows")]
const TRAY_ERROR_MESSAGE: u32 = 0x8000 + 44;
#[cfg(target_os = "windows")]
const TRAY_ICON_TIMER_ID: usize = 41;
#[cfg(any(target_os = "windows", test))]
const WINDOWS_WM_POWERBROADCAST: u32 = 0x0218;
#[cfg(any(target_os = "windows", test))]
const WINDOWS_PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

#[cfg(any(target_os = "windows", test))]
fn windows_power_event_is_resume(message: u32, wparam: usize) -> bool {
    message == WINDOWS_WM_POWERBROADCAST && wparam == WINDOWS_PBT_APMRESUMEAUTOMATIC
}

#[cfg(target_os = "windows")]
struct TrayInstance {
    mutex: usize,
    class_name: Vec<u16>,
}

#[cfg(target_os = "windows")]
impl Drop for TrayInstance {
    fn drop(&mut self) {
        // SAFETY: `mutex` owns one successful CreateMutexW handle.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(
                self.mutex as windows_sys::Win32::Foundation::HANDLE,
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn wide_windows(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn acquire_tray_instance() -> Result<Option<TrayInstance>> {
    use std::ptr::null;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{FindWindowW, PostMessageW},
    };

    let sid = gnirehtet_vd::runtime::windows_current_user_sid()?;
    let class_name = wide_windows(&format!("GnirehtetVdTray_{sid}"));
    let mutex_name = wide_windows(&format!(r"Local\GnirehtetVdTray_{sid}"));
    let mutex = unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        return Err(std::io::Error::last_os_error()).context("creating tray singleton mutex");
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(mutex);
            let existing = FindWindowW(class_name.as_ptr(), null());
            if !existing.is_null() {
                PostMessageW(existing, TRAY_SHOW_EXISTING_MESSAGE, 0, 0);
            }
        }
        return Ok(None);
    }
    Ok(Some(TrayInstance {
        mutex: mutex as usize,
        class_name,
    }))
}

#[cfg(target_os = "windows")]
fn request_tray_exit(class_name: &[u16]) {
    use std::ptr::null;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

    for _ in 0..40 {
        let window = unsafe { FindWindowW(class_name.as_ptr(), null()) };
        if !window.is_null() {
            unsafe {
                PostMessageW(window, WM_CLOSE, 0, 0);
            }
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "windows")]
fn run_windows_tray(
    instance_guard: TrayInstance,
    broker_context: BrokerContext,
) -> std::io::Result<()> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NOTIFYICONDATAW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, MessageBoxW,
                PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
                TrackPopupMenu, TranslateMessage, MB_ICONINFORMATION, MB_OK, MF_CHECKED,
                MF_SEPARATOR, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_CLOSE, WM_COMMAND,
                WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
            },
        },
    };

    const ICON_ID: u32 = 1;

    fn apply_tooltip(icon: &mut NOTIFYICONDATAW, text: &str) {
        icon.szTip.fill(0);
        for (destination, source) in icon.szTip.iter_mut().zip(text.encode_utf16()) {
            *destination = source;
        }
    }

    fn refresh_tray_icon(hwnd: HWND) {
        let observation = WINDOWS_TRAY_RUNTIME
            .get()
            .map(|tray| TrayHostObservation::capture(&tray.context.paths));
        let desired_on = tray_desired_on();
        let Ok(icons) = WindowsTrayIcons::get() else {
            return;
        };
        let mut icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: ICON_ID,
            uFlags: NIF_ICON | NIF_TIP,
            hIcon: if desired_on { icons.on } else { icons.off }
                as windows_sys::Win32::UI::WindowsAndMessaging::HICON,
            ..Default::default()
        };
        apply_tooltip(&mut icon, tray_tooltip(desired_on, observation));
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &icon);
        }
    }

    fn show_menu(hwnd: HWND) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }
        for entry in tray_menu_entries(tray_desired_on()) {
            let flags = if entry.command.is_some() {
                MF_STRING | if entry.checked { MF_CHECKED } else { 0 }
            } else {
                MF_SEPARATOR
            };
            let id = entry.command.map_or(0, tray_command_id);
            let label = entry.label.map(wide_windows);
            unsafe {
                AppendMenuW(
                    menu,
                    flags,
                    id,
                    label.as_ref().map_or(null(), |value| value.as_ptr()),
                );
            }
        }
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point);
            SetForegroundWindow(hwnd);
        }
        let selected = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                hwnd,
                null(),
            )
        } as usize;
        unsafe {
            DestroyMenu(menu);
            PostMessageW(hwnd, WM_NULL, 0, 0);
        }
        if let Some(command) = tray_command_from_id(selected) {
            dispatch_tray_command(command, hwnd);
        }
    }

    fn dispatch_tray_command(command: TrayCommand, hwnd: HWND) {
        let Some(coordinator) = WINDOWS_TRAY_COORDINATOR.get() else {
            return;
        };
        match command {
            TrayCommand::ToggleLink => coordinator.toggle(),
            TrayCommand::DiagnoseAndFix => coordinator.diagnose(),
            TrayCommand::Exit => coordinator.exit(),
        }
        refresh_tray_icon(hwnd);
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            message if windows_power_event_is_resume(message, wparam) => {
                if let Some(coordinator) = WINDOWS_TRAY_COORDINATOR.get() {
                    coordinator.host_resume();
                }
                1
            }
            TRAY_CALLBACK_MESSAGE => {
                if matches!(lparam as u32, WM_RBUTTONUP | WM_LBUTTONDBLCLK) {
                    show_menu(hwnd);
                }
                0
            }
            TRAY_SHOW_EXISTING_MESSAGE => {
                show_menu(hwnd);
                0
            }
            TRAY_EXIT_COMPLETE_MESSAGE => {
                DestroyWindow(hwnd);
                0
            }
            TRAY_ERROR_MESSAGE => {
                if let Ok(mut last_error) = TRAY_LAST_ERROR.lock() {
                    if let Some(error) = last_error.take() {
                        let message = wide_windows(&error);
                        let title = wide_windows("Quest VD Wired needs attention");
                        MessageBoxW(
                            null_mut(),
                            message.as_ptr(),
                            title.as_ptr(),
                            MB_OK | MB_ICONINFORMATION,
                        );
                    }
                }
                0
            }
            WM_COMMAND => {
                if let Some(command) = tray_command_from_id(wparam & 0xffff) {
                    dispatch_tray_command(command, hwnd);
                }
                0
            }
            WM_TIMER if wparam == TRAY_ICON_TIMER_ID => {
                refresh_tray_icon(hwnd);
                0
            }
            WM_CLOSE => {
                if let Some(coordinator) = WINDOWS_TRAY_COORDINATOR.get() {
                    coordinator.exit();
                }
                0
            }
            WM_DESTROY => {
                TRAY_WINDOW.store(0, Ordering::Release);
                KillTimer(hwnd, TRAY_ICON_TIMER_ID);
                let icon = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: ICON_ID,
                    ..Default::default()
                };
                Shell_NotifyIconW(NIM_DELETE, &icon);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    WINDOWS_TRAY_RUNTIME
        .set(WindowsTrayRuntime {
            context: broker_context,
        })
        .map_err(|_| std::io::Error::other("tray runtime was initialized more than once"))?;
    let class_name = instance_guard.class_name.clone();
    let tray_icons = WindowsTrayIcons::get()?;

    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: tray_icons.on as windows_sys::Win32::UI::WindowsAndMessaging::HICON,
        lpszClassName: class_name.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut icon = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: if tray_desired_on() {
            tray_icons.on
        } else {
            tray_icons.off
        } as windows_sys::Win32::UI::WindowsAndMessaging::HICON,
        ..Default::default()
    };
    apply_tooltip(&mut icon, tray_tooltip(tray_desired_on(), None));
    if unsafe { Shell_NotifyIconW(NIM_ADD, &icon) } == 0 {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(std::io::Error::last_os_error());
    }
    TRAY_WINDOW.store(hwnd as usize, Ordering::Release);
    refresh_tray_icon(hwnd);
    if let Some(coordinator) = WINDOWS_TRAY_COORDINATOR.get() {
        let _ = coordinator.sender.send(TrayCoordinatorRequest::Wake {
            report_stop_error: false,
        });
    }
    if unsafe { SetTimer(hwnd, TRAY_ICON_TIMER_ID, 500, None) } == 0 {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(std::io::Error::last_os_error());
    }

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == 0 {
            break;
        }
        if result == -1 {
            unsafe {
                DestroyWindow(hwnd);
            }
            return Err(std::io::Error::last_os_error());
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    drop(instance_guard);
    Ok(())
}

fn wait_for_connected(paths: &AppPaths, session: SessionId, daemon: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let store = StateStore::new(&paths.status);
    let session_text = session.to_string();
    while Instant::now() < deadline {
        let status = store.read_or_stopped();
        if status.lifecycle.session_id.as_deref() == Some(session_text.as_str()) {
            match status.lifecycle.state {
                HostState::Connected => return Ok(()),
                HostState::Error => {
                    bail!(
                        "Android v4 reported an error before STARTED: {}",
                        status
                            .lifecycle
                            .reason
                            .as_deref()
                            .unwrap_or("unknown error")
                    )
                }
                _ => {}
            }
        }
        if daemon.try_wait()?.is_some() {
            bail!("host daemon exited before Android acknowledged STARTED");
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out after 30 seconds waiting for Android GNR4 STARTED")
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn duplicate_no_argument_launch_exits_without_broker() {
        assert_eq!(
            no_argument_tray_action::<()>(None),
            NoArgumentTrayAction::ExitWithoutBroker
        );
        assert_eq!(
            no_argument_tray_action(Some(7)),
            NoArgumentTrayAction::StartPrimary(7)
        );
    }

    #[test]
    fn public_start_defaults_to_all_traffic_and_keeps_vd_only_explicit() {
        assert!(Cli::try_parse_from([
            "quest-vd-wired",
            "start",
            "--vd-package",
            "com.example.unexpected",
        ])
        .is_err());

        let default = Cli::try_parse_from(["quest-vd-wired", "start"]).unwrap();
        let Some(Command::Start(default)) = default.command else {
            panic!("start command was not parsed");
        };
        assert!(default.routes_all_traffic());

        let vd_only =
            Cli::try_parse_from(["quest-vd-wired", "start", "--virtual-desktop-only"]).unwrap();
        let Some(Command::Start(vd_only)) = vd_only.command else {
            panic!("start command was not parsed");
        };
        assert!(!vd_only.routes_all_traffic());

        let compatibility =
            Cli::try_parse_from(["quest-vd-wired", "start", "--all-traffic"]).unwrap();
        let Some(Command::Start(compatibility)) = compatibility.command else {
            panic!("start command was not parsed");
        };
        assert!(compatibility.routes_all_traffic());
        assert!(Cli::try_parse_from([
            "quest-vd-wired",
            "start",
            "--all-traffic",
            "--virtual-desktop-only",
        ])
        .is_err());
    }

    #[test]
    fn public_cli_uses_the_quest_vd_wired_name() {
        assert_eq!(Cli::command().get_name(), "quest-vd-wired");
    }

    #[test]
    fn ok_without_typed_repair_suppression_is_not_a_safe_stop_ack() {
        let response = AdminResponse {
            ok: true,
            repairs_suppressed: false,
            error: None,
            status: None,
        };
        assert!(!admin_stop_acknowledges_suppression(&response));
    }

    #[test]
    fn missing_runtime_never_reports_a_stale_connected_or_stopping_state() {
        let mut connected = StateSnapshot {
            state: HostState::Connected,
            session_id: Some("00112233-4455-6677-8899-aabbccddeeff".into()),
            missed_heartbeats: 0,
            reason: None,
        };
        reflect_missing_runtime(&mut connected);
        assert_eq!(connected.state, HostState::Degraded);
        assert!(connected.reason.as_deref().unwrap().contains("not running"));

        let mut stopping = connected;
        stopping.state = HostState::Stopping;
        reflect_missing_runtime(&mut stopping);
        assert_eq!(stopping.state, HostState::Error);
        assert!(stopping.reason.as_deref().unwrap().contains("unverified"));
    }

    #[test]
    fn daemon_cli_rejects_zero_session_nonce() {
        assert!(Cli::try_parse_from([
            "quest-vd-wired",
            "daemon",
            "--session",
            "00000000-0000-0000-0000-000000000000",
        ])
        .is_err());
    }

    #[test]
    fn normal_help_hides_development_and_daemon_controls() {
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("--home"));
        assert!(!help.contains("--adb"));
        assert!(!help.contains("daemon"));
        for command in [
            "start",
            "stop",
            "status",
            "repair",
            "doctor",
            "diagnostics",
            "version",
        ] {
            assert!(help.contains(command), "public help omitted {command}");
        }
    }

    #[test]
    fn tray_menu_exposes_only_toggle_diagnose_and_exit() {
        assert_eq!(
            tray_command_from_id(TRAY_TOGGLE_ID),
            Some(TrayCommand::ToggleLink)
        );
        assert_eq!(
            tray_command_from_id(TRAY_DIAGNOSE_ID),
            Some(TrayCommand::DiagnoseAndFix)
        );
        assert_eq!(tray_command_from_id(TRAY_EXIT_ID), Some(TrayCommand::Exit));
        assert_eq!(tray_command_from_id(0), None);

        let on = tray_menu_entries(true);
        let visible: Vec<_> = on.iter().filter_map(|entry| entry.label).collect();
        assert_eq!(visible, ["Wired link", "Diagnose and fix", "Exit"]);
        assert!(on[0].checked);
        assert!(!tray_menu_entries(false)[0].checked);
    }

    fn tray_observation(
        lifecycle: HostState,
        daemon_running: bool,
        runtime_visible: bool,
    ) -> TrayHostObservation {
        TrayHostObservation {
            lifecycle,
            daemon_running,
            runtime_visible,
        }
    }

    #[test]
    fn tray_defaults_on_and_bounds_persistent_retry_backoff() {
        assert!(tray_initial_desired_on());
        assert_eq!(TRAY_RETRY_INITIAL, Duration::from_millis(750));
        assert_eq!(
            next_tray_retry_delay(TRAY_RETRY_INITIAL),
            Duration::from_millis(1500)
        );
        assert_eq!(
            next_tray_retry_delay(Duration::from_secs(6)),
            TRAY_RETRY_MAX
        );
        assert_eq!(next_tray_retry_delay(TRAY_RETRY_MAX), TRAY_RETRY_MAX);
    }

    #[test]
    fn tray_policy_suppresses_duplicate_start_and_cleans_stale_runtime() {
        let clean = tray_observation(HostState::Stopped, false, false);
        assert_eq!(
            tray_reconcile_action(true, clean),
            TrayReconcileAction::Start
        );
        for lifecycle in [
            HostState::Preparing,
            HostState::Connected,
            HostState::Degraded,
        ] {
            assert_eq!(
                tray_reconcile_action(true, tray_observation(lifecycle, true, true)),
                TrayReconcileAction::None
            );
        }
        assert_eq!(
            tray_reconcile_action(true, tray_observation(HostState::Connected, false, false)),
            TrayReconcileAction::Stop
        );
        assert_eq!(
            tray_reconcile_action(true, tray_observation(HostState::Stopped, false, true)),
            TrayReconcileAction::Stop
        );
    }

    #[test]
    fn tray_off_and_exit_require_verified_stop_without_automatic_alerts() {
        let clean = tray_observation(HostState::Stopped, false, false);
        let active = tray_observation(HostState::Connected, true, true);
        assert_eq!(
            tray_reconcile_action(false, clean),
            TrayReconcileAction::None
        );
        assert_eq!(
            tray_reconcile_action(false, active),
            TrayReconcileAction::Stop
        );
        assert!(!tray_should_report_reconcile_error(false, false));
        assert!(tray_should_report_reconcile_error(true, false));
        assert!(tray_should_report_reconcile_error(false, true));
    }

    #[test]
    fn tray_tooltips_separate_requested_color_from_actual_state() {
        let clean = tray_observation(HostState::Stopped, false, false);
        let connected = tray_observation(HostState::Connected, true, true);
        let sleeping = tray_observation(HostState::Degraded, true, true);
        assert_eq!(tray_tooltip(false, Some(clean)), "Wired link: off");
        assert_eq!(tray_tooltip(false, Some(connected)), "Wired link: stopping");
        assert_eq!(
            tray_tooltip(true, Some(connected)),
            "Quest VD Wired — connected"
        );
        assert_eq!(
            tray_tooltip(true, Some(sleeping)),
            "Wired link: headset asleep — reconnecting"
        );
        assert!(tray_tooltip(true, None).contains("allow USB debugging"));
    }

    #[derive(Clone)]
    struct FakeTrayBackend {
        inner: Arc<FakeTrayBackendInner>,
    }

    enum FakeReconcileOutcome {
        Error,
        Intermediate,
        Converged,
    }

    struct FakeTrayBackendInner {
        outcomes: StdMutex<std::collections::VecDeque<FakeReconcileOutcome>>,
        calls: StdMutex<Vec<Instant>>,
        errors: StdMutex<Vec<String>>,
        resume_calls: std::sync::atomic::AtomicUsize,
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        operation_delay: Duration,
    }

    impl FakeTrayBackend {
        fn new(outcomes: impl IntoIterator<Item = bool>, operation_delay: Duration) -> Self {
            Self::with_steps(
                outcomes.into_iter().map(|success| {
                    if success {
                        FakeReconcileOutcome::Converged
                    } else {
                        FakeReconcileOutcome::Error
                    }
                }),
                operation_delay,
            )
        }

        fn with_steps(
            outcomes: impl IntoIterator<Item = FakeReconcileOutcome>,
            operation_delay: Duration,
        ) -> Self {
            Self {
                inner: Arc::new(FakeTrayBackendInner {
                    outcomes: StdMutex::new(outcomes.into_iter().collect()),
                    calls: StdMutex::new(Vec::new()),
                    errors: StdMutex::new(Vec::new()),
                    resume_calls: std::sync::atomic::AtomicUsize::new(0),
                    active: std::sync::atomic::AtomicUsize::new(0),
                    max_active: std::sync::atomic::AtomicUsize::new(0),
                    operation_delay,
                }),
            }
        }

        fn call_times(&self) -> Vec<Instant> {
            self.inner.calls.lock().unwrap().clone()
        }
    }

    impl TrayCoordinatorBackend for FakeTrayBackend {
        fn host_resume(&self) -> TrayCoordinatorFuture<'_, Result<()>> {
            Box::pin(async move {
                self.inner.resume_calls.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        }

        fn diagnose(&self) -> TrayCoordinatorFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn execute_external(&self, _command: Command) -> TrayCoordinatorFuture<'_, Result<String>> {
            Box::pin(async { Ok(String::new()) })
        }

        fn reconcile(&self) -> TrayCoordinatorFuture<'_, Result<bool>> {
            Box::pin(async move {
                let active = self.inner.active.fetch_add(1, Ordering::AcqRel) + 1;
                self.inner.max_active.fetch_max(active, Ordering::AcqRel);
                self.inner.calls.lock().unwrap().push(Instant::now());
                tokio::time::sleep(self.inner.operation_delay).await;
                self.inner.active.fetch_sub(1, Ordering::AcqRel);
                match self
                    .inner
                    .outcomes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(FakeReconcileOutcome::Converged)
                {
                    FakeReconcileOutcome::Error => Err(anyhow::Error::msg("automatic failure")),
                    FakeReconcileOutcome::Intermediate => Ok(false),
                    FakeReconcileOutcome::Converged => Ok(true),
                }
            })
        }

        fn exit_requested(&self) -> bool {
            false
        }

        fn verified_off(&self) -> bool {
            false
        }

        fn report_error(&self, error: String) {
            self.inner.errors.lock().unwrap().push(error);
        }

        fn diagnosis_complete(&self) {}

        fn exit_complete(&self) {}
    }

    #[tokio::test]
    async fn tray_coordinator_serializes_queued_reconciliation() {
        let backend = FakeTrayBackend::new([true; 5], Duration::from_millis(25));
        let inspect = backend.clone();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(run_tray_coordinator(receiver, backend));
        for _ in 0..5 {
            sender
                .send(TrayCoordinatorRequest::Wake {
                    report_stop_error: false,
                })
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while inspect.call_times().len() < 5 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(sender);
        worker.await.unwrap();
        assert_eq!(inspect.inner.max_active.load(Ordering::Acquire), 1);
        assert!(inspect.inner.errors.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tray_coordinator_dispatches_host_resume_before_reconciliation() {
        let backend = FakeTrayBackend::new([true], Duration::ZERO);
        let inspect = backend.clone();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(run_tray_coordinator(receiver, backend));
        sender.send(TrayCoordinatorRequest::HostResume).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while inspect.inner.resume_calls.load(Ordering::Acquire) == 0
                || inspect.call_times().is_empty()
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        drop(sender);
        worker.await.unwrap();
        assert_eq!(inspect.inner.resume_calls.load(Ordering::Acquire), 1);
        assert_eq!(inspect.call_times().len(), 1);
    }

    #[test]
    fn only_automatic_windows_resume_requests_recovery() {
        assert!(windows_power_event_is_resume(0x0218, 0x0012));
        assert!(!windows_power_event_is_resume(0x0218, 0x0007));
        assert!(!windows_power_event_is_resume(0x000F, 0x0012));
    }

    #[tokio::test]
    async fn tray_coordinator_retries_automatic_failures_silently_with_backoff() {
        let backend = FakeTrayBackend::with_steps(
            [
                FakeReconcileOutcome::Error,
                FakeReconcileOutcome::Intermediate,
                FakeReconcileOutcome::Error,
                FakeReconcileOutcome::Converged,
            ],
            Duration::from_millis(5),
        );
        let inspect = backend.clone();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(run_tray_coordinator(receiver, backend));
        sender
            .send(TrayCoordinatorRequest::Wake {
                report_stop_error: false,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            while inspect.call_times().len() < 4 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(sender);
        worker.await.unwrap();

        let calls = inspect.call_times();
        assert_eq!(calls.len(), 4);
        assert!(calls[1].duration_since(calls[0]) >= Duration::from_millis(1400));
        assert!(calls[2].duration_since(calls[1]) >= Duration::from_millis(1400));
        assert!(calls[3].duration_since(calls[2]) >= Duration::from_millis(2900));
        assert_eq!(inspect.inner.max_active.load(Ordering::Acquire), 1);
        assert!(inspect.inner.errors.lock().unwrap().is_empty());
    }

    #[test]
    fn embedded_tray_icons_contain_distinct_exact_32_pixel_images() {
        let off = ico_image_for_size(TRAY_ICON_OFF_ICO, 32).unwrap();
        let on = ico_image_for_size(TRAY_ICON_ON_ICO, 32).unwrap();
        assert!(!off.is_empty());
        assert!(!on.is_empty());
        assert_ne!(off, on);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_accepts_both_embedded_tray_icons() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

        let icons = WindowsTrayIcons::load().unwrap();
        assert_ne!(icons.off, 0);
        assert_ne!(icons.on, 0);
        assert_ne!(icons.off, icons.on);
        unsafe {
            DestroyIcon(icons.off as HICON);
            DestroyIcon(icons.on as HICON);
        }
    }

    #[test]
    fn tray_icon_parser_rejects_malformed_headers() {
        assert_eq!(ico_image_for_size(&[], 32), None);
        assert_eq!(ico_image_for_size(&[0, 0, 2, 0, 0, 0], 32), None);
        assert_eq!(ico_image_for_size(&[0, 0, 1, 0, 1, 0], 32), None);
    }

    #[test]
    fn tray_icon_parser_skips_bad_entries_and_prefers_larger_ties() {
        fn entry(width: u8, height: u8, length: u32, offset: u32) -> [u8; 16] {
            let mut entry = [0; 16];
            entry[0] = width;
            entry[1] = height;
            entry[8..12].copy_from_slice(&length.to_le_bytes());
            entry[12..16].copy_from_slice(&offset.to_le_bytes());
            entry
        }

        let mut ico = vec![0, 0, 1, 0, 3, 0];
        ico.extend_from_slice(&entry(24, 23, u32::MAX, u32::MAX));
        ico.extend_from_slice(&entry(24, 24, 1, 54));
        ico.extend_from_slice(&entry(40, 40, 1, 55));
        ico.extend_from_slice(&[24, 40]);

        assert_eq!(ico_image_for_size(&ico, 32), Some(&[40][..]));
    }

    #[test]
    fn broker_accepts_only_typed_public_commands() {
        assert_eq!(
            BrokerCommand::try_from(Command::Start(StartArgs::from_all_traffic(true))).unwrap(),
            BrokerCommand::Start { all_traffic: true }
        );
        assert_eq!(
            BrokerCommand::try_from(Command::Status(StatusArgs { json: true })).unwrap(),
            BrokerCommand::Status { json: true }
        );
        assert_eq!(
            BrokerCommand::try_from(Command::Diagnostics(DiagnosticsArgs {
                command: DiagnosticsCommand::Capture { duration: 42 },
            }))
            .unwrap(),
            BrokerCommand::DiagnosticsCapture { duration: 42 }
        );
        assert!(BrokerCommand::try_from(Command::Daemon(DaemonArgs {
            session: SessionId([0x11; 16]),
        }))
        .is_err());
        assert!(BrokerCommand::DiagnosticsCapture { duration: 0 }
            .into_public_command()
            .is_err());

        assert!(command_requires_lifecycle_serialization(&Command::Start(
            StartArgs::from_all_traffic(true)
        )));
        assert!(command_requires_lifecycle_serialization(&Command::Stop));
        assert!(command_requires_lifecycle_serialization(&Command::Repair));
        assert!(!command_requires_lifecycle_serialization(&Command::Version));
    }

    #[test]
    fn diagnostics_capture_waits_in_client_without_occupying_broker_gate() {
        let capture = Command::Diagnostics(DiagnosticsArgs {
            command: DiagnosticsCommand::Capture { duration: 600 },
        });
        let export = Command::Diagnostics(DiagnosticsArgs {
            command: DiagnosticsCommand::Export {
                path: PathBuf::from("bundle.jsonl"),
            },
        });
        assert!(command_runs_client_side(&capture));
        assert!(!command_runs_client_side(&export));
        assert!(BrokerGate::default().begin_command().is_some());
    }

    #[test]
    fn coordinator_accepts_diagnose_exit_and_external_requests() {
        let (reply, _response) = tokio::sync::oneshot::channel();
        let requests = [
            TrayCoordinatorRequest::Diagnose,
            TrayCoordinatorRequest::Exit,
            TrayCoordinatorRequest::External {
                command: Command::Version,
                reply,
            },
        ];
        assert_eq!(requests.len(), 3);
    }

    #[test]
    fn broker_deadlines_are_bounded_by_command_shape() {
        assert_eq!(BROKER_PROTOCOL_VERSION, 6);
        assert_eq!(
            BrokerCommand::Version.response_timeout(),
            Duration::from_secs(5)
        );
        assert_eq!(
            BrokerCommand::Start { all_traffic: false }.response_timeout(),
            Duration::from_secs(15 * 60)
        );
        assert_eq!(
            BrokerCommand::DiagnosticsCapture { duration: u64::MAX }.response_timeout(),
            Duration::from_secs(3630)
        );
    }

    #[test]
    fn broker_response_preserves_stream_and_exit_semantics() {
        let success = BrokerResponse::success("done".into());
        assert_eq!(success.exit_code, 0);
        assert_eq!(success.stdout, "done\n");
        assert!(success.stderr.is_empty());

        let failure = BrokerResponse::failure("failed");
        assert_eq!(failure.exit_code, 1);
        assert!(failure.stdout.is_empty());
        assert_eq!(failure.stderr, "Error: failed\n");
    }

    #[tokio::test]
    async fn broker_frames_round_trip_and_reject_oversized_lengths() {
        let request = BrokerRequest {
            protocol_version: BROKER_PROTOCOL_VERSION,
            instance_root: PathBuf::from("test-root"),
            adb_program: PathBuf::from("test-adb"),
            command: BrokerCommand::Status { json: true },
        };
        let (mut sender, mut receiver) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move { write_broker_frame(&mut sender, &request).await });
        let decoded: BrokerRequest = read_broker_frame(&mut receiver).await.unwrap();
        write.await.unwrap().unwrap();
        assert_eq!(decoded.protocol_version, BROKER_PROTOCOL_VERSION);
        assert_eq!(decoded.command, BrokerCommand::Status { json: true });

        let (mut sender, mut receiver) = tokio::io::duplex(16);
        sender
            .write_u32((MAX_BROKER_FRAME + 1) as u32)
            .await
            .unwrap();
        assert!(read_broker_frame::<BrokerRequest, _>(&mut receiver)
            .await
            .is_err());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn broker_uses_the_first_pipe_connection_for_the_command() {
        let pipe_name = format!(
            r"\\.\pipe\gnirehtet-vd-broker-test-{}-{}",
            std::process::id(),
            SessionId::random()
        );
        let mut server =
            gnirehtet_vd::runtime::create_secure_named_pipe(&pipe_name, true, 1).unwrap();
        let accepted = tokio::spawn(async move {
            server.connect().await.unwrap();
            let request: BrokerRequest = read_broker_frame(&mut server).await.unwrap();
            assert_eq!(request.command, BrokerCommand::Version);
            write_broker_frame(&mut server, &BrokerResponse::success("ready".into()))
                .await
                .unwrap();
        });
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let start_counter = starts.clone();
        let mut client =
            open_or_start_broker_client(&pipe_name, Duration::from_secs(2), move || {
                start_counter.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .await
            .unwrap();
        write_broker_frame(
            &mut client,
            &BrokerRequest {
                protocol_version: BROKER_PROTOCOL_VERSION,
                instance_root: PathBuf::from("test-root"),
                adb_program: PathBuf::from("test-adb"),
                command: BrokerCommand::Version,
            },
        )
        .await
        .unwrap();
        let response: BrokerResponse = read_broker_frame(&mut client).await.unwrap();
        accepted.await.unwrap();
        assert_eq!(starts.load(Ordering::Relaxed), 0);
        assert_eq!(response.stdout, "ready\n");
    }

    #[tokio::test]
    async fn broker_executes_in_process_and_propagates_output() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let instance_root = paths.root.clone();
        let adb_program = directory.path().join("unused-adb");
        let context = BrokerContext::new(
            paths,
            adb_program.clone(),
            AdbController::new(Arc::new(SystemAdb::new(adb_program.clone()))),
            BrokerGate::default(),
        );
        let (mut client, server) = tokio::io::duplex(4096);
        let handler = tokio::spawn(async move { handle_broker_connection(server, &context).await });
        write_broker_frame(
            &mut client,
            &BrokerRequest {
                protocol_version: BROKER_PROTOCOL_VERSION,
                instance_root,
                adb_program,
                command: BrokerCommand::Version,
            },
        )
        .await
        .unwrap();
        let response: BrokerResponse = read_broker_frame(&mut client).await.unwrap();
        handler.await.unwrap().unwrap();
        assert_eq!(response.exit_code, 0);
        assert!(response.stdout.contains(env!("CARGO_PKG_VERSION")));
        assert!(response.stderr.is_empty());
    }

    #[test]
    fn broker_gate_makes_command_and_shutdown_atomic() {
        let gate = BrokerGate::default();
        let command = gate.begin_command().unwrap();
        assert_eq!(gate.snapshot(), (true, false));
        assert!(!gate.begin_shutdown());
        drop(command);
        assert_eq!(gate.snapshot(), (false, false));

        assert!(gate.begin_shutdown());
        assert_eq!(gate.snapshot(), (false, true));
        assert!(gate.begin_command().is_none());
        gate.cancel_shutdown();
        assert_eq!(gate.snapshot(), (false, false));
    }

    #[test]
    fn broker_publishes_automatically_bootstrapped_adb() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let original = PathBuf::from("adb");
        let context = BrokerContext::new(
            paths,
            original.clone(),
            AdbController::new(Arc::new(SystemAdb::new(original))),
            BrokerGate::default(),
        );
        let verified = directory.path().join("platform-tools/adb.exe");
        let ensured = context
            .ensure_adb_with(|program, root| {
                assert_eq!(program, PathBuf::from("adb"));
                assert_eq!(root, directory.path());
                Ok(verified.clone())
            })
            .unwrap();
        assert_eq!(ensured.adb_program, verified);
        assert_eq!(context.environment().unwrap().adb_program, verified);
    }

    #[tokio::test]
    async fn broker_context_clones_share_one_adb_lane() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path().to_owned())).unwrap();
        let adb_program = directory.path().join("unused-adb");
        let context = BrokerContext::new(
            paths,
            adb_program.clone(),
            AdbController::new(Arc::new(SystemAdb::new(adb_program))),
            BrokerGate::default(),
        );
        let clone = context.clone();
        let held = context.adb_lane.lock().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(25), clone.adb_lane.lock())
                .await
                .is_err()
        );
        drop(held);
        let _reacquired = tokio::time::timeout(Duration::from_millis(100), clone.adb_lane.lock())
            .await
            .unwrap();
    }
}
