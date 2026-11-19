//! Closing or dropping a session ends the program and everything it started,
//! even a child that ignores the polite request or has left the console, and
//! never blocks the caller.
//!
//! On Unix the program is a shell that ignores SIGHUP with a child that does
//! the same, so only the SIGKILL after the grace period ends them. On Windows
//! closing the ConPTY already ends every process attached to its console (it
//! terminates them even when they ignore the close event), so the child is
//! started detached from the console: only the job object ends it. The
//! Windows program is this test binary run again with `--ignored` on a helper
//! test.

use std::process::Command;
use std::time::{Duration, Instant};

use term::{ExitState, Session, SessionConfig, visible_text};

/// Set for the helper processes, so `--include-ignored` never runs them by hand.
const HELPER_ENV: &str = "TERM_CLOSE_HELPER";
const SETTLE: Duration = Duration::from_secs(15);

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SETTLE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The ids the helper printed: its own and its child's.
fn read_pids(session: &Session) -> (u32, u32) {
    let mut pids = (None, None);
    wait_until("the program to print its process ids", || {
        let text = visible_text(&session.lock());
        let find = |tag: &str| {
            text.iter()
                .find_map(|row| row.strip_prefix(tag)?.trim().parse::<u32>().ok())
        };
        pids = (find("PARENT "), find("CHILD "));
        pids.0.is_some() && pids.1.is_some()
    });
    (pids.0.unwrap_or_default(), pids.1.unwrap_or_default())
}

fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        let filter = format!("PID eq {pid}");
        let out = Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .expect("run tasklist");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|out| out.status.success())
    }
}

#[cfg(windows)]
fn stubborn() -> SessionConfig {
    let exe = std::env::current_exe().expect("the test binary's path");
    SessionConfig {
        program: exe.to_string_lossy().into_owned(),
        args: ["--ignored", "--exact", "stubborn_parent", "--nocapture"]
            .map(String::from)
            .to_vec(),
        env: [(HELPER_ENV.to_owned(), "1".to_owned())].into(),
        ..SessionConfig::default()
    }
}

#[cfg(unix)]
fn stubborn() -> SessionConfig {
    let script = "trap '' HUP; echo PARENT $$; \
        sh -c 'trap \"\" HUP; echo CHILD $$; exec sleep 300' & wait";
    SessionConfig {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        env: [(HELPER_ENV.to_owned(), "1".to_owned())].into(),
        ..SessionConfig::default()
    }
}

/// Starts the stubborn program and waits until both processes are up.
fn start() -> (Session, u32, u32) {
    let session = Session::spawn(stubborn()).expect("start the program");
    let (parent, child) = read_pids(&session);
    assert!(alive(parent) && alive(child), "both processes run");
    assert_eq!(session.process_id(), Some(parent));
    (session, parent, child)
}

fn assert_tree_gone(parent: u32, child: u32) {
    wait_until("the processes to end", || !alive(parent) && !alive(child));
}

#[test]
fn close_ends_the_whole_tree_without_waiting_and_reports_the_status() {
    let (session, parent, child) = start();

    session.close();
    // Unix: the program ignores SIGHUP, so nothing can have ended it yet
    // unless `close` waited for the SIGKILL.
    #[cfg(unix)]
    assert_eq!(
        session.exit_state(),
        ExitState::Running,
        "close returned at once"
    );

    wait_until("the exit status", || {
        session.exit_state() != ExitState::Running
    });
    let status = session.exit_status().expect("the kill reports a status");
    assert!(!status.success(), "{status:?}");
    assert_tree_gone(parent, child);

    // Closing again does nothing.
    session.close();
}

#[test]
fn dropping_a_session_ends_the_whole_tree() {
    let (session, parent, child) = start();
    drop(session);
    assert_tree_gone(parent, child);
}

#[test]
fn closing_a_program_that_already_exited_reports_its_own_status() {
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/c".to_owned(), "exit 3".to_owned()])
    } else {
        ("sh", vec!["-c".to_owned(), "exit 3".to_owned()])
    };
    let session = Session::spawn(SessionConfig {
        program: program.to_owned(),
        args,
        ..SessionConfig::default()
    })
    .expect("start the program");
    wait_until("the exit", || session.exit_state() != ExitState::Running);
    session.close();
    assert_eq!(session.exit_status().and_then(|s| s.code()), Some(3));
}

/// The Windows program under test: it ignores the close event and starts a
/// child outside the console.
#[test]
#[ignore = "helper process of the close tests"]
fn stubborn_parent() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    ignore_close_requests();
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut command = Command::new(exe);
    command.args(["--ignored", "--exact", "stubborn_child", "--nocapture"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console: closing the ConPTY does not reach it.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    let mut child = command.spawn().expect("start the child");
    println!("PARENT {}", std::process::id());
    println!("CHILD {}", child.id());
    let _ = child.wait();
}

#[test]
#[ignore = "helper process of the close tests"]
fn stubborn_child() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    ignore_close_requests();
    std::thread::sleep(Duration::from_secs(300));
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn ignore_close_requests() {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_event: u32) -> BOOL {
        1
    }
    // SAFETY: `handler` is a plain function with the signature the API wants
    // and touches nothing; it lives for the whole process.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(unix)]
fn ignore_close_requests() {}
