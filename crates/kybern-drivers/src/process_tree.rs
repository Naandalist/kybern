use kybern_protocol::process::NoWindow;
use std::process::Stdio;
use std::time::Duration;

/// How long a stopped provider gets to exit before it is killed. Claude Code
/// and Codex rotate OAuth refresh tokens: the server retires the old token as
/// soon as it answers, so a process killed before it saves the new one leaves
/// a dead token on disk and signs the user out. Claude Code's own shutdown
/// waits up to 2 s for a refresh in flight.
pub(crate) const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Own only the isolated process group/tree created for this spawn. Dropping
/// it asks the group to exit and kills what remains after `TERMINATE_GRACE`.
/// On Unix, the caller must spawn the child with `process_group(0)`.
///
/// On Windows the tree is a job object that the root joins right after spawn.
/// Its descendants inherit the job, so the forced stop reaches them even when
/// the root has already exited: a `.cmd`, `node` or bun launcher that quit,
/// or a root that tokio's `kill_on_drop` killed first. Windows keeps no parent
/// links past a dead process, so `taskkill /T` alone cannot find them.
pub struct ProcessTree {
    pid: u32,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl ProcessTree {
    /// Own the tree of `pid`, a process the caller just spawned and still
    /// holds, so its id cannot have been recycled.
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(windows)]
            job: job::Job::adopt(pid),
        }
    }

    /// Own an interactive sign-in. On Windows its descendants are not put in
    /// a job: a login can start the user's browser as its child, and the job
    /// would kill that browser at the forced stop. Only the root's live tree
    /// is stopped, as before jobs. On Unix this is the same as [`Self::new`].
    pub fn interactive(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(windows)]
            job: None,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Ask every process in the group to exit. Windows has no such signal for
    /// console processes; there, closing stdin is the graceful stop, and a
    /// process that keeps running is killed when the grace runs out.
    pub(crate) fn terminate(&self) {
        #[cfg(unix)]
        signal_group(self.pid, "-TERM");
    }

    /// Send Ctrl+C (SIGINT) to every process in the group, the way a terminal
    /// does, so a CLI can wind down its own turn. Windows has no such signal
    /// for a detached console process, so it reports `false` and the caller
    /// goes straight to `terminate` and the forced stop.
    pub(crate) fn interrupt(&self) -> bool {
        #[cfg(unix)]
        {
            signal_group(self.pid, "-INT")
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Kill the group now, for a process that ignored `terminate`.
    pub(crate) fn kill_now(mut self) {
        self.take_reaper().force_kill();
        std::mem::forget(self);
    }

    fn take_reaper(&mut self) -> Reaper {
        Reaper {
            pid: self.pid,
            #[cfg(windows)]
            job: self.job.take(),
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
        let reaper = self.take_reaper();
        // Drop cannot wait, so a detached thread finishes the job. It stops
        // polling as soon as the group is gone, so it never signals a
        // recycled id. A Windows job is killed at the deadline whether or not
        // its root is still alive: a launcher's leftovers get the same grace
        // as the root, and never outlive it.
        let _ = std::thread::Builder::new().name("kybern-reap".into()).spawn(move || {
            let deadline = std::time::Instant::now() + TERMINATE_GRACE;
            while std::time::Instant::now() < deadline {
                if !reaper.alive() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            reaper.force_kill();
        });
    }
}

/// What the reaper thread needs once the `ProcessTree` is gone.
struct Reaper {
    pid: u32,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl Reaper {
    #[cfg(unix)]
    fn alive(&self) -> bool {
        signal_group(self.pid, "-0")
    }

    #[cfg(unix)]
    fn force_kill(&self) {
        signal_group(self.pid, "-KILL");
    }

    #[cfg(windows)]
    fn alive(&self) -> bool {
        match &self.job {
            Some(job) => job.has_processes(),
            None => root_alive(self.pid),
        }
    }

    #[cfg(windows)]
    fn force_kill(&self) {
        match &self.job {
            Some(job) => job.kill(),
            // Without a job, only a live root still links its descendants.
            None if root_alive(self.pid) => {
                let _ = std::process::Command::new("taskkill")
                    .no_window()
                    .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            None => {}
        }
    }
}

#[cfg(unix)]
fn signal_group(group: u32, signal: &str) -> bool {
    std::process::Command::new("/bin/kill")
        .no_window()
        .args([signal, "--", &format!("-{group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn root_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .no_window()
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
}

/// One job object per provider tree.
///
/// - No `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The daemon exiting (an update
///   restart, a crash) must not hard-kill a provider that is saving a rotated
///   OAuth token, or an updater halfway through an install. Their stdin
///   closes, which is the graceful stop, as before jobs. Only the forced stop
///   of a `ProcessTree` kills the job.
/// - The root joins just after `spawn` returns; it is not created suspended,
///   because std and tokio give no handle to its main thread to resume. A
///   child the root starts in that window stays outside the job. The window
///   is microseconds, a launcher spends far longer loading its runtime before
///   it spawns anything, and an escaped child is no worse off than before.
/// - A process in another job (the daemon's own, under a terminal or service
///   host) can still join a nested job on Windows 8 and later. If that fails
///   anyway, the tree falls back to `taskkill /T` on the live root.
#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
        QueryInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    pub(super) struct Job(HANDLE);

    // SAFETY: a job handle is a kernel handle; any thread may use or close it,
    // and the job calls used here are safe to make concurrently.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A new job holding `pid`, or `None` if it cannot be assigned.
        pub(super) fn adopt(pid: u32) -> Option<Self> {
            // SAFETY: plain Win32 calls. Every handle opened here is closed
            // here or owned by the returned `Job`.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    tracing::debug!(pid, error = %std::io::Error::last_os_error(), "could not create a job for the provider");
                    return None;
                }
                let job = Job(handle);
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    tracing::debug!(pid, error = %std::io::Error::last_os_error(), "could not open the provider for its job");
                    return None;
                }
                let assigned = AssignProcessToJobObject(job.0, process) != 0;
                let error = std::io::Error::last_os_error();
                CloseHandle(process);
                if !assigned {
                    tracing::debug!(pid, %error, "could not put the provider in a job");
                    return None;
                }
                Some(job)
            }
        }

        /// Whether any process is still in the job. A failed query counts as
        /// alive, so the tree is still killed at the deadline.
        pub(super) fn has_processes(&self) -> bool {
            // SAFETY: the buffer is a zeroed accounting struct of the size passed.
            unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let ok = QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                ) != 0;
                !ok || info.ActiveProcesses > 0
            }
        }

        pub(super) fn kill(&self) {
            // SAFETY: `self.0` is a live job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is owned and closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// A worker must not outlive the session handle or a cancelled startup future.
pub(crate) struct SessionTask(pub tokio::task::JoinHandle<()>);
impl Drop for SessionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Collect a short-lived probe/job without leaving launcher descendants behind
/// when its timeout or caller cancels the future.
pub(crate) async fn output(command: &mut tokio::process::Command) -> std::io::Result<std::process::Output> {
    // `ProcessTree` stops a cancelled command. Tokio's kill-on-drop would
    // SIGKILL it first, even while it saves a refreshed login.
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|error| crate::binary::explain_spawn_error(error, command.as_std().get_program()))?;
    let tree = ProcessTree::new(child.id().expect("spawned provider command"));
    let result = child.wait_with_output().await;
    drop(tree);
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::process::Command;

    #[tokio::test]
    async fn collected_commands_preserve_output_and_cancel_their_descendants() {
        let result = output(Command::new("sh").args(["-c", "printf out; printf err >&2"])).await.unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"err");
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        command.current_dir(root.path()).args(["-c", "(sleep 0.3; touch escaped) & wait"]);
        assert!(tokio::time::timeout(Duration::from_millis(40), output(&mut command)).await.is_err());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!root.path().join("escaped").exists());
    }

    /// A cancelled probe gets SIGTERM, not SIGKILL, so a CLI that is saving a
    /// rotated login can finish the write before it exits.
    #[tokio::test]
    async fn cancelled_commands_may_finish_saving_before_they_exit() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        command.current_dir(root.path()).args(["-c", "trap 'sleep 0.3; touch saved; exit 0' TERM; touch started; sleep 30 & wait"]);
        let started = root.path().join("started");
        let run = tokio::spawn(async move { output(&mut command).await });
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        run.abort();
        let saved = root.path().join("saved");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !saved.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the cancelled command was killed before it could save");
    }

    #[tokio::test]
    async fn processes_that_ignore_terminate_are_killed_after_the_grace() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        // The ignored signal is inherited, so the whole group ignores SIGTERM.
        command.current_dir(root.path()).args(["-c", "trap '' TERM; sleep 30 & echo $! > pid; wait"]);
        let pid_file = root.path().join("pid");
        let run = tokio::spawn(async move { output(&mut command).await });
        while std::fs::read_to_string(&pid_file).map_or(true, |text| text.trim().is_empty()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
        run.abort();
        let alive = || std::process::Command::new("/bin/kill").args(["-0", &pid]).stderr(Stdio::null()).status().unwrap().success();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(alive(), "SIGTERM-ignoring process was killed before the grace period");
        tokio::time::sleep(TERMINATE_GRACE).await;
        assert!(!alive(), "process {pid} survived the grace period");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::process::Command;

    /// A launcher (`cmd`) that starts a long-lived `ping` grandchild, the
    /// shape of an npm `.cmd` shim or a `node` wrapper. With `then_exit`, the
    /// launcher quits once the grandchild runs; otherwise it keeps running.
    fn launcher(then_exit: bool) -> Command {
        let tail = if then_exit { "" } else { " & ping -t 127.0.0.1 >nul" };
        let mut command = Command::new("cmd").no_window();
        // The short wait first keeps the grandchild's start well clear of the
        // job assignment just after spawn.
        command
            .args(["/d", "/c", &format!("ping -n 2 127.0.0.1 >nul & start /b ping -t 127.0.0.1 >nul{tail}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// The launcher's `ping -t` child, found by its recorded parent id, which
    /// Windows keeps after the parent exits. Independent of the job under test.
    fn grandchild(launcher: u32) -> u32 {
        let query = format!(
            "Get-CimInstance Win32_Process -Filter 'ParentProcessId={launcher}' | Where-Object {{ $_.CommandLine -like '*ping*-t*' }} | Select-Object -First 1 -ExpandProperty ProcessId"
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let output = std::process::Command::new("powershell")
                .no_window()
                .args(["-NoProfile", "-NonInteractive", "-Command", &query])
                .output()
                .expect("powershell");
            if let Ok(pid) = String::from_utf8_lossy(&output.stdout).trim().parse::<u32>() {
                return pid;
            }
            assert!(Instant::now() < deadline, "launcher {launcher} never started its grandchild");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether `pid` exits within `within`.
    async fn gone_within(pid: u32, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while root_alive(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        true
    }

    fn kill(pid: u32) {
        let _ = std::process::Command::new("taskkill").no_window().args(["/PID", &pid.to_string(), "/F"]).output();
    }

    /// OpenCode's `Server` drops its `kill_on_drop` child before the tree:
    /// tokio kills only the launcher, and the grandchild must still go.
    #[tokio::test]
    async fn a_killed_launchers_grandchild_is_stopped_with_the_tree() {
        let mut command = launcher(false);
        command.kill_on_drop(true);
        let child = command.spawn().unwrap();
        let tree = ProcessTree::new(child.id().unwrap());
        assert!(tree.job.is_some(), "the launcher did not join a job");
        let root = tree.pid();
        let grandchild = tokio::task::spawn_blocking(move || grandchild(root)).await.unwrap();
        drop(child);
        drop(tree);
        if !gone_within(grandchild, TERMINATE_GRACE + Duration::from_secs(10)).await {
            kill(grandchild);
            panic!("grandchild {grandchild} outlived its killed launcher");
        }
    }

    /// A launcher that exits on its own leaves no link to its child; the job
    /// still reaches it.
    #[tokio::test]
    async fn an_exited_launchers_grandchild_is_stopped_with_the_tree() {
        let mut child = launcher(true).spawn().unwrap();
        let tree = ProcessTree::new(child.id().unwrap());
        let root = tree.pid();
        let grandchild = tokio::task::spawn_blocking(move || grandchild(root)).await.unwrap();
        child.wait().await.unwrap();
        assert!(root_alive(grandchild), "grandchild exited with its launcher");
        drop(tree);
        if !gone_within(grandchild, TERMINATE_GRACE + Duration::from_secs(10)).await {
            kill(grandchild);
            panic!("grandchild {grandchild} outlived its exited launcher");
        }
    }

    /// The job is only killed once the grace runs out, as on Unix.
    #[tokio::test]
    async fn a_running_tree_keeps_its_grace() {
        let mut command = Command::new("ping").no_window();
        command.args(["-t", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null());
        let _child = command.spawn().unwrap();
        let tree = ProcessTree::new(_child.id().unwrap());
        let pid = tree.pid();
        let dropped = Instant::now();
        drop(tree);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(root_alive(pid), "the tree was killed before the grace period");
        assert!(gone_within(pid, TERMINATE_GRACE + Duration::from_secs(10)).await, "process {pid} survived the grace period");
        assert!(dropped.elapsed() >= TERMINATE_GRACE, "the tree was killed before the grace period");
    }
}
