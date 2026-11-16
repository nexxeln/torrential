//! Other processes and this one: whether a process runs, asking one to stop, and hearing that
//! this one is asked to. Unix uses signals. Windows uses console control events, which reach an
//! engine through the console it runs in, visible or not.

/// Whether the process `pid` is running.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Asks the process `pid` to stop, as SIGTERM does.
#[cfg(unix)]
pub fn interrupt(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

/// Waits until this process is asked to stop: SIGTERM, or Ctrl-C in its terminal.
#[cfg(unix)]
pub async fn asked_to_stop() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else { return false };
    // A process still running has not signalled its handle yet.
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    let _ = unsafe { CloseHandle(process) };
    running
}

/// Asks the process `pid` to stop with Ctrl-C on its console. A process can be on one console at a
/// time, so a helper, `torrential daemon interrupt`, attaches to the engine's console and sends it,
/// and this process keeps its own terminal.
#[cfg(windows)]
pub fn interrupt(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .args(["daemon", "interrupt", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

/// Sends Ctrl-C to every process on the console of `pid`, from this process, which ignores it.
/// Only `torrential daemon interrupt` calls it, since it leaves this process on that console.
#[cfg(windows)]
pub fn send_interrupt(pid: u32) -> std::io::Result<()> {
    use windows::Win32::System::Console::{
        AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    let sent = unsafe {
        let _ = FreeConsole();
        AttachConsole(pid)
            .and_then(|()| SetConsoleCtrlHandler(None, true))
            .and_then(|()| GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0))
    };
    sent.map_err(crate::fsutil::win32)
}

/// Waits until this process is asked to stop: Ctrl-C or Ctrl-Break on its console, its console
/// closing, or the person logging off or the system shutting down.
#[cfg(windows)]
pub async fn asked_to_stop() {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    // A process started in a group of its own ignores Ctrl-C until it says otherwise.
    let _ = unsafe { SetConsoleCtrlHandler(None, false) };
    let (Ok(mut c), Ok(mut brk), Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) =
        (ctrl_c(), ctrl_break(), ctrl_close(), ctrl_logoff(), ctrl_shutdown())
    else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = c.recv() => {}
        _ = brk.recv() => {}
        _ = close.recv() => {}
        _ = logoff.recv() => {}
        _ = shutdown.recv() => {}
    }
}
