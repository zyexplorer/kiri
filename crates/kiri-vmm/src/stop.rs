use std::io;
use std::time::{Duration, Instant};

pub(crate) fn retire(
    mut stopped: impl FnMut() -> io::Result<bool>,
    mut force: impl FnMut(Duration) -> io::Result<()>,
    graceful: Duration,
    forced: Duration,
) -> io::Result<()> {
    if wait(&mut stopped, Instant::now() + graceful)? {
        return Ok(());
    }
    let deadline = Instant::now() + forced;
    force(deadline.saturating_duration_since(Instant::now()))?;
    if wait(&mut stopped, deadline)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the previous VM did not stop",
        ))
    }
}

fn wait(stopped: &mut impl FnMut() -> io::Result<bool>, deadline: Instant) -> io::Result<bool> {
    loop {
        if stopped()? {
            return Ok(true);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(left.min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_force_request_does_not_prove_the_machine_stopped() {
        let forced = Cell::new(false);
        let result = retire(
            || Ok(false),
            |_| {
                forced.set(true);
                Ok(())
            },
            Duration::ZERO,
            Duration::ZERO,
        );
        assert!(forced.get());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn confirmed_natural_stop_needs_no_force_request() {
        retire(
            || Ok(true),
            |_| panic!("a stopped VM must not be forced"),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
    }

    #[test]
    fn confirmed_forced_stop_succeeds() {
        let stopped = Cell::new(false);
        retire(
            || Ok(stopped.get()),
            |_| {
                stopped.set(true);
                Ok(())
            },
            Duration::ZERO,
            Duration::from_millis(100),
        )
        .unwrap();
    }

    #[test]
    fn status_and_force_errors_are_not_stop_confirmation() {
        let status = retire(
            || Err(io::Error::other("status failed")),
            |_| panic!("unknown status must fail closed"),
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(status.unwrap_err().to_string(), "status failed");
        let force = retire(
            || Ok(false),
            |_| Err(io::Error::other("stop failed")),
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(force.unwrap_err().to_string(), "stop failed");
    }
}
