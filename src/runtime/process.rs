use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdout, Command};

/// Owns one runtime command's group, including descendants that retain pipes
/// after the wrapper exits. The leader must remain unreaped until the group is
/// terminated: its reserved PID prevents our PGID from being reused.
pub(super) struct CommandGroup {
    child: Child,
    pgid: Option<libc::pid_t>,
}

impl CommandGroup {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        let pgid = libc::pid_t::try_from(child.id().expect("newly spawned child has a PID"))
            .expect("child PID fits pid_t");
        Ok(Self {
            child,
            pgid: Some(pgid),
        })
    }

    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Observe exit without releasing the leader's PID. Do not use `Child::wait`
    /// concurrently with I/O: a wrapper can exit before its descendants do.
    pub(super) async fn exited(&self) -> io::Result<()> {
        while !self.has_exited()? {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    fn has_exited(&self) -> io::Result<bool> {
        let pid = self.pgid.expect("exit observation precedes group cleanup");
        // SAFETY: zero is a valid initial siginfo_t, and waitid writes only
        // to this live, correctly aligned value. WNOWAIT never reaps it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.cast_unsigned(),
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            return Ok(false);
        }
        // SAFETY: waitid initialized info; si_pid is valid for child exit.
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// After `exited()`, even a successful command does not transfer ownership of
    /// background processes. Kill remaining group members before reaping the
    /// leader, then retire the group identity so Drop cannot signal a reused ID.
    pub(super) async fn finish(&mut self) -> io::Result<ExitStatus> {
        self.terminate()?;
        self.child.wait().await
    }

    fn terminate(&mut self) -> io::Result<()> {
        if let Some(pgid) = self.pgid {
            // SAFETY: spawn created this group and we have not reaped its
            // leader. The negative, nonzero PID targets only our owned group.
            if unsafe { libc::kill(-pgid, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                let empty_group = error.raw_os_error() == Some(libc::ESRCH);
                #[cfg(target_os = "macos")]
                let empty_group = empty_group
                    || (error.raw_os_error() == Some(libc::EPERM) && self.only_zombie_leader()?);
                if !empty_group {
                    return Err(error);
                }
            }
            self.pgid = None;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn only_zombie_leader(&self) -> io::Result<bool> {
        // Darwin returns EPERM, not ESRCH, when an unreaped zombie is the
        // group's sole member. Verify that exact case; never ignore EPERM for
        // an inaccessible live leader or any remaining descendant.
        if !self.has_exited()? {
            return Ok(false);
        }
        let pgid = self.pgid.expect("group identity is retained until cleanup");
        let mut members: [libc::pid_t; 2] = [0; 2];
        let buffer_bytes = i32::try_from(std::mem::size_of_val(&members))
            .expect("two PIDs fit a proc_listpids buffer");
        // SAFETY: PROC_PGRP_ONLY (2) lists members of our still-reserved PGID.
        // The buffer holds two PIDs: any additional member rules out this case.
        let bytes = unsafe {
            libc::proc_listpids(
                2,
                pgid.cast_unsigned(),
                members.as_mut_ptr().cast(),
                buffer_bytes,
            )
        };
        if bytes == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(bytes == buffer_bytes / 2 && members[0] == pgid)
    }
}

impl Drop for CommandGroup {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("ztheme: cannot terminate runtime process group: {error}");
        }
        // Child's kill_on_drop then kills/reaps the immediate child via Tokio's
        // orphan queue if cancellation prevented finish(). No later group kill
        // is scheduled, so reaping cannot leave a stale PGID cleanup task.
    }
}

#[cfg(test)]
mod tests {
    use super::CommandGroup;
    use tokio::process::Command;

    #[tokio::test]
    async fn successful_command_without_descendants_keeps_its_exit_status() {
        let mut group = CommandGroup::spawn(&mut Command::new("/usr/bin/true")).unwrap();
        group.exited().await.unwrap();
        assert!(group.finish().await.unwrap().success());
    }
}
