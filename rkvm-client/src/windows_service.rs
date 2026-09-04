use std::error::Error as StdError;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LookupPrivilegeValueW,
    SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, TokenUIAccess,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSUserName,
    WTS_CURRENT_SERVER_HANDLE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenEventW,
    OpenProcessToken, ResumeThread, SetEvent, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
};

const SERVICE_NAME: &str = "rkvm-client";
const NO_SESSION: u32 = u32::MAX;
const EVENT_SYNCHRONIZE: u32 = 0x0010_0000;
const AGENT_RETRY: Duration = Duration::from_secs(2);
const AGENT_STOP_TIMEOUT_MS: u32 = 5_000;

static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();
static AGENT_GENERATION: AtomicU64 = AtomicU64::new(1);

type BoxError = Box<dyn StdError + Send + Sync>;

define_windows_service!(ffi_service_main, service_main);

pub fn run(config_path: PathBuf) -> windows_service::Result<()> {
    let _ = CONFIG_PATH.set(config_path);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

pub async fn wait_for_stop_event(name: String) -> io::Result<()> {
    let name = wide_null(name);
    let event = OwnedHandle::new(unsafe { OpenEventW(EVENT_SYNCHRONIZE, 0, name.as_ptr()) })?;
    loop {
        match unsafe { WaitForSingleObject(event.raw(), 0) } {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_TIMEOUT => tokio::time::sleep(Duration::from_millis(100)).await,
            WAIT_FAILED => return Err(io::Error::last_os_error()),
            result => {
                return Err(io::Error::other(format!(
                    "unexpected stop-event wait result {result}"
                )))
            }
        }
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let Some(config_path) = CONFIG_PATH.get().cloned() else {
        return;
    };
    let log_path = sibling(&config_path, "service.log");
    append_log(&log_path, "service entry started");
    if let Err(err) = run_service(&config_path, &log_path) {
        append_log(&log_path, &format!("service failed: {err}"));
    }
}

fn run_service(config_path: &Path, log_path: &Path) -> Result<(), BoxError> {
    let (control_tx, control_rx) = mpsc::channel();
    let event_handler = move |event| -> ServiceControlHandlerResult {
        match event {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = control_tx.send(Control::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(change) => {
                let _ = control_tx.send(Control::SessionChange {
                    session_id: change.notification.session_id,
                    reason: change.reason,
                });
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        ServiceExitCode::NO_ERROR,
    )?;

    append_log(log_path, "service reported running");
    supervise_agents(config_path, log_path, &control_rx, &status_handle)?;

    set_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::NO_ERROR,
    )?;
    append_log(log_path, "service stopped");
    Ok(())
}

fn set_status(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    controls: ServiceControlAccept,
    exit_code: ServiceExitCode,
) -> windows_service::Result<()> {
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: controls,
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Desktop {
    Default,
    Winlogon,
}

impl Desktop {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "winsta0\\default",
            Self::Winlogon => "winsta0\\winlogon",
        }
    }
}

enum Control {
    Stop,
    SessionChange {
        session_id: u32,
        reason: SessionChangeReason,
    },
}

fn supervise_agents(
    config_path: &Path,
    log_path: &Path,
    controls: &mpsc::Receiver<Control>,
    status_handle: &ServiceStatusHandle,
) -> Result<(), BoxError> {
    let agent_log_path = sibling(config_path, "client-service.log");
    let mut target_session = active_console_session();
    let mut target_desktop = initial_desktop(target_session);
    let mut agent: Option<Agent> = None;
    let mut retry_at = Instant::now();

    loop {
        match controls.recv_timeout(Duration::from_millis(250)) {
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                set_status(
                    status_handle,
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    ServiceExitCode::NO_ERROR,
                )?;
                if let Some(mut running) = agent.take() {
                    running.stop(log_path);
                }
                return Ok(());
            }
            Ok(Control::SessionChange { session_id, reason }) => {
                append_log(
                    log_path,
                    &format!("session change {reason:?} for session {session_id}"),
                );
                if session_id == target_session {
                    let next_desktop = desktop_after_change(reason, target_desktop);
                    if next_desktop != target_desktop {
                        append_log(
                            log_path,
                            &format!(
                                "switching session {session_id} agent from {} to {}",
                                target_desktop.name(),
                                next_desktop.name()
                            ),
                        );
                        if let Some(mut running) = agent.take() {
                            running.stop(log_path);
                        }
                        target_desktop = next_desktop;
                        retry_at = Instant::now();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let current_session = active_console_session();
        if current_session != target_session {
            append_log(
                log_path,
                &format!(
                    "active console session changed from {target_session} to {current_session}"
                ),
            );
            if let Some(mut running) = agent.take() {
                running.stop(log_path);
            }
            target_session = current_session;
            target_desktop = initial_desktop(target_session);
            retry_at = Instant::now();
        }

        if let Some(running) = agent.as_ref() {
            if !running.is_running()? {
                append_log(log_path, &format!("desktop agent {} exited", running.pid));
                agent = None;
                retry_at = Instant::now() + AGENT_RETRY;
            }
        }

        if agent.is_none() && target_session != NO_SESSION && Instant::now() >= retry_at {
            match Agent::spawn(config_path, &agent_log_path, target_session, target_desktop) {
                Ok(started) => {
                    append_log(
                        log_path,
                        &format!(
                            "started desktop agent {} in session {} on {}",
                            started.pid,
                            target_session,
                            target_desktop.name()
                        ),
                    );
                    agent = Some(started);
                }
                Err(err) => {
                    append_log(
                        log_path,
                        &format!(
                            "failed to start desktop agent in session {} on {}: {}",
                            target_session,
                            target_desktop.name(),
                            err
                        ),
                    );
                    retry_at = Instant::now() + AGENT_RETRY;
                }
            }
        }
    }
}

fn active_console_session() -> u32 {
    unsafe { WTSGetActiveConsoleSessionId() }
}

fn initial_desktop(session_id: u32) -> Desktop {
    if session_id != NO_SESSION && session_has_user(session_id) {
        Desktop::Default
    } else {
        Desktop::Winlogon
    }
}

fn desktop_after_change(reason: SessionChangeReason, current: Desktop) -> Desktop {
    match reason {
        SessionChangeReason::SessionLogon => Desktop::Default,
        SessionChangeReason::SessionLogoff => Desktop::Winlogon,
        // The virtual HID device is independent of the interactive desktop.
        // Keeping the same agent here also preserves its authenticated TCP
        // connection across a lock/unlock cycle. SendInput fallback remains
        // harmlessly connected while the secure desktop rejects its events,
        // then works again as soon as the default desktop is restored.
        SessionChangeReason::SessionLock | SessionChangeReason::SessionUnlock => current,
        _ => current,
    }
}

fn session_has_user(session_id: u32) -> bool {
    let mut buffer = null_mut();
    let mut bytes = 0;
    let success = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSUserName,
            &mut buffer,
            &mut bytes,
        )
    };
    if success == 0 {
        return false;
    }
    let has_user = !buffer.is_null() && bytes >= 2 && unsafe { *buffer != 0 };
    unsafe {
        WTSFreeMemory(buffer.cast());
    }
    has_user
}

struct Agent {
    process: OwnedHandle,
    stop_event: OwnedHandle,
    _job: OwnedHandle,
    pid: u32,
}

impl Agent {
    fn spawn(
        config_path: &Path,
        log_path: &Path,
        session_id: u32,
        desktop: Desktop,
    ) -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let event_name = format!(
            "Global\\rkvm-client-stop-{}-{}",
            unsafe { GetCurrentProcessId() },
            AGENT_GENERATION.fetch_add(1, Ordering::Relaxed)
        );
        let event_name_wide = wide_null(&event_name);
        let stop_event =
            OwnedHandle::new(unsafe { CreateEventW(null(), 1, 0, event_name_wide.as_ptr()) })?;

        // SendInput is otherwise rejected on the protected Winlogon desktop.
        // Keep UIAccess off normal-desktop agents and verify Windows retained
        // the flag before starting a protected-desktop agent.
        let token = primary_token_for_session(session_id, desktop == Desktop::Winlogon)?;
        let application = wide_null(executable.as_os_str());
        let mut command_line = build_command_line([
            executable.as_os_str(),
            OsStr::new("--desktop-agent"),
            OsStr::new("--stop-event"),
            OsStr::new(&event_name),
            OsStr::new("--log-file"),
            log_path.as_os_str(),
            config_path.as_os_str(),
        ]);
        let mut desktop_name = wide_null(desktop.name());
        let current_directory = config_path.parent().unwrap_or_else(|| Path::new("."));
        let current_directory = wide_null(current_directory.as_os_str());
        let startup = STARTUPINFOW {
            cb: mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: desktop_name.as_mut_ptr(),
            ..STARTUPINFOW::default()
        };
        let mut process_info = PROCESS_INFORMATION::default();

        let created = unsafe {
            CreateProcessAsUserW(
                token.raw(),
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                null(),
                current_directory.as_ptr(),
                &startup,
                &mut process_info,
            )
        };
        if created == 0 {
            return Err(io::Error::last_os_error());
        }

        let process = OwnedHandle::new(process_info.hProcess)?;
        let thread = OwnedHandle::new(process_info.hThread)?;
        let job = create_kill_on_close_job()?;
        if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
            unsafe {
                TerminateProcess(process.raw(), 1);
            }
            return Err(io::Error::last_os_error());
        }
        if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
            unsafe {
                TerminateProcess(process.raw(), 1);
            }
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            process,
            stop_event,
            _job: job,
            pid: process_info.dwProcessId,
        })
    }

    fn is_running(&self) -> io::Result<bool> {
        match unsafe { WaitForSingleObject(self.process.raw(), 0) } {
            WAIT_TIMEOUT => Ok(true),
            WAIT_OBJECT_0 => Ok(false),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            result => Err(io::Error::other(format!(
                "unexpected agent wait result {result}"
            ))),
        }
    }

    fn stop(&mut self, log_path: &Path) {
        unsafe {
            SetEvent(self.stop_event.raw());
        }
        match unsafe { WaitForSingleObject(self.process.raw(), AGENT_STOP_TIMEOUT_MS) } {
            WAIT_OBJECT_0 => {
                append_log(
                    log_path,
                    &format!("desktop agent {} stopped cleanly", self.pid),
                );
            }
            WAIT_TIMEOUT => {
                append_log(
                    log_path,
                    &format!("desktop agent {} did not stop; terminating it", self.pid),
                );
                unsafe {
                    TerminateProcess(self.process.raw(), 1);
                    WaitForSingleObject(self.process.raw(), 1_000);
                }
            }
            _ => {
                append_log(
                    log_path,
                    &format!("failed while waiting for desktop agent {}", self.pid),
                );
            }
        }
    }
}

fn primary_token_for_session(session_id: u32, ui_access: bool) -> io::Result<OwnedHandle> {
    let mut source = null_mut();
    let source_access = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES;
    if unsafe { OpenProcessToken(GetCurrentProcess(), source_access, &mut source) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let source = OwnedHandle::new(source)?;
    enable_privilege(source.raw(), "SeTcbPrivilege")?;
    enable_privilege(source.raw(), "SeAssignPrimaryTokenPrivilege")?;
    enable_privilege(source.raw(), "SeIncreaseQuotaPrivilege")?;

    let mut token = null_mut();
    if unsafe {
        DuplicateTokenEx(
            source.raw(),
            TOKEN_ALL_ACCESS,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle::new(token)?;

    if unsafe {
        SetTokenInformation(
            token.raw(),
            TokenSessionId,
            (&session_id as *const u32).cast(),
            mem::size_of::<u32>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if ui_access {
        enable_ui_access(token.raw())?;
    }
    Ok(token)
}

fn enable_ui_access(token: HANDLE) -> io::Result<()> {
    let enabled = 1u32;
    if unsafe {
        SetTokenInformation(
            token,
            TokenUIAccess,
            (&enabled as *const u32).cast(),
            mem::size_of::<u32>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut actual = 0u32;
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token,
            TokenUIAccess,
            (&mut actual as *mut u32).cast(),
            mem::size_of::<u32>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if actual == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows did not retain the requested UIAccess token flag",
        ));
    }
    Ok(())
}

fn enable_privilege(token: HANDLE, name: &str) -> io::Result<()> {
    let name = wide_null(name);
    let mut luid = LUID::default();
    if unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut luid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    unsafe {
        SetLastError(0);
    }
    if unsafe { AdjustTokenPrivileges(token, 0, &privileges, 0, null_mut(), null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        return Err(io::Error::from_raw_os_error(ERROR_NOT_ALL_ASSIGNED as i32));
    }
    Ok(())
}

fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn sibling(config_path: &Path, name: &str) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}

fn append_log(path: &Path, message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{timestamp}] {message}");
    }
}

fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn build_command_line<'a>(arguments: impl IntoIterator<Item = &'a OsStr>) -> Vec<u16> {
    let mut command_line = Vec::new();
    for argument in arguments {
        if !command_line.is_empty() {
            command_line.push(' ' as u16);
        }
        push_quoted_argument(&mut command_line, argument);
    }
    command_line.push(0);
    command_line
}

fn push_quoted_argument(command_line: &mut Vec<u16>, argument: &OsStr) {
    let argument: Vec<u16> = argument.encode_wide().collect();
    let needs_quotes = argument.is_empty()
        || argument
            .iter()
            .any(|value| matches!(*value, 0x09 | 0x20 | 0x22));
    if !needs_quotes {
        command_line.extend(argument);
        return;
    }

    command_line.push('"' as u16);
    let mut backslashes = 0;
    for value in argument {
        if value == '\\' as u16 {
            backslashes += 1;
            continue;
        }
        if value == '"' as u16 {
            command_line.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
        } else {
            command_line.extend(std::iter::repeat_n('\\' as u16, backslashes));
        }
        backslashes = 0;
        command_line.push(value);
    }
    command_line.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
    command_line.push('"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    fn command_line(arguments: &[&str]) -> String {
        let encoded = build_command_line(arguments.iter().map(OsStr::new));
        OsString::from_wide(&encoded[..encoded.len() - 1])
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn quotes_service_agent_arguments() {
        assert_eq!(
            command_line(&["rkvm-client.exe", "C:\\path with spaces\\client.toml"]),
            "rkvm-client.exe \"C:\\path with spaces\\client.toml\""
        );
        assert_eq!(
            command_line(&["rkvm-client.exe", "ends with slash \\"]),
            "rkvm-client.exe \"ends with slash \\\\\""
        );
    }

    #[test]
    fn preserves_the_agent_across_lock_and_unlock() {
        assert_eq!(
            desktop_after_change(SessionChangeReason::SessionLock, Desktop::Default),
            Desktop::Default
        );
        assert_eq!(
            desktop_after_change(SessionChangeReason::SessionUnlock, Desktop::Winlogon),
            Desktop::Winlogon
        );
    }

    #[test]
    fn changes_desktop_for_logon_and_logoff() {
        assert_eq!(
            desktop_after_change(SessionChangeReason::SessionLogon, Desktop::Winlogon),
            Desktop::Default
        );
        assert_eq!(
            desktop_after_change(SessionChangeReason::SessionLogoff, Desktop::Default),
            Desktop::Winlogon
        );
    }
}
