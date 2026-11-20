//! `--init`: the container's PID 1 stays ours and runs the program as its
//! child, as tini does for Docker. Signals sent to the container go to the
//! program, orphans re-parented to PID 1 are reaped, and PID 1 exits with
//! the program's status. A program that ignores SIGTERM as PID 1 (the
//! kernel drops signals PID 1 has no handler for) now stops on it.

use std::ffi::CStr;
use std::io;

use super::sys::child_fail;

/// Signals left to their default action in PID 1: the ones raised by its
/// own faults, and terminal job control.
const NOT_FORWARDED: [libc::c_int; 9] =
    [libc::SIGFPE, libc::SIGILL, libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGTRAP, libc::SIGSYS, libc::SIGTTIN, libc::SIGTTOU];

/// Runs `exe` as PID 1's child and supervises it. Called by the container's
/// first process in place of its exec, after everything else is set up
/// (user, capabilities, seccomp), so both run with the same rights.
/// `tty`: the program gets its own process group and the terminal's
/// foreground, so ^C reaches it once, not through PID 1 as well.
pub fn run(exe: &CStr, argv: *const *const libc::c_char, envp: *const *const libc::c_char, tty: bool) -> ! {
    // SAFETY: signal-mask and process calls on local sets, in the
    // single-threaded child; the child branch only execs or exits.
    unsafe {
        // Blocked signals queue even for PID 1, which the kernel would
        // otherwise drop without a handler; sigtimedwait takes them below.
        let mut forward: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut forward);
        for s in NOT_FORWARDED {
            libc::sigdelset(&mut forward, s);
        }
        libc::sigprocmask(libc::SIG_SETMASK, &forward, std::ptr::null_mut());
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);

        let child = libc::fork();
        if child < 0 {
            child_fail(2, "init", io::Error::last_os_error());
        }
        if child == 0 {
            if tty {
                libc::setpgid(0, 0);
                libc::tcsetpgrp(0, libc::getpgrp());
            }
            // The program starts as Docker would start it: nothing blocked,
            // default actions.
            libc::signal(libc::SIGTTIN, libc::SIG_DFL);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            libc::execve(exe.as_ptr(), argv, envp);
            let err = io::Error::last_os_error();
            // tini runs the program with execvp, which hands a file that is
            // no program (a script without a shebang) to /bin/sh.
            if err.raw_os_error() == Some(libc::ENOEXEC) {
                let sh = c"/bin/sh";
                let mut args = vec![sh.as_ptr(), exe.as_ptr()];
                let mut i = 1;
                while !(*argv).is_null() && !(*argv.add(i)).is_null() {
                    args.push(*argv.add(i));
                    i += 1;
                }
                args.push(std::ptr::null());
                libc::execve(sh.as_ptr(), args.as_ptr(), envp);
            }
            super::sys::exec_fail(2, exe, err, true);
        }
        // The agent's descriptors (DNS sockets, other containers' pipes) stay out of PID 1.
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);

        let wake = libc::timespec { tv_sec: 1, tv_nsec: 0 };
        loop {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let sig = libc::sigtimedwait(&forward, &mut info, &wake);
            if sig > 0 && sig != libc::SIGCHLD {
                libc::kill(child, sig);
            }
            if let Some(code) = reap(child) {
                libc::_exit(code);
            }
        }
    }
}

/// Reaps every exited child; the program's exit status once it has gone.
fn reap(child: libc::pid_t) -> Option<i32> {
    let mut code = None;
    loop {
        let mut status = 0;
        // SAFETY: plain waitpid on any child, without blocking.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return code;
        }
        if pid == child {
            code = Some(exit_code(status));
        }
    }
}

/// A wait status as a shell reports it: the exit code, or 128 + the signal.
fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_read_as_a_shell_reports_them() {
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGTERM), 128 + 15);
    }
}
