use std::time::Duration;

/// Run `op` up to `attempts` times, sleeping `delay` between failures.
/// Windows uses 5 attempts / 60 ms (about 250 ms total) for a busy `OpenClipboard`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn retry<T, E>(
    attempts: u32,
    delay: Duration,
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut n = 1;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if n >= attempts => return Err(e),
            Err(_) => {
                std::thread::sleep(delay);
                n += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn succeeds_after_busy_attempts() {
        let mut calls = 0;
        let r = retry(5, Duration::ZERO, || {
            calls += 1;
            if calls < 4 { Err("busy") } else { Ok(calls) }
        });
        assert_eq!(r, Ok(4));
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let mut calls = 0;
        let r: Result<(), _> = retry(5, Duration::ZERO, || {
            calls += 1;
            Err("busy")
        });
        assert_eq!(r, Err("busy"));
        assert_eq!(calls, 5);
    }
}
