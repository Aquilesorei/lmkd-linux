// mgd-psi-trigger: arm the kernel PSI pressure trigger and proxy events to the
// daemon via stdout.
//
// Usage: mgd-psi-trigger <stall_us>
//   stall_us — threshold in microseconds for "some" stall in a 2s window
//              e.g. 100000 = 5% of 2,000,000 µs
//
// Stdout: one byte (0x01) per pressure event. Daemon polls this pipe with
//         POLLIN; the main PSI fd stays entirely inside this process.
//
// Exit codes: 0 clean, 1 bad args, 2 open/arm failed.
//
// Kernel 7.x+: /proc/pressure/memory triggers are broken (EINVAL). Walks the
// cgroup hierarchy upward to find the highest writable memory.pressure file.

use std::io::{self, Write};
use std::os::unix::io::AsRawFd;

/// Walk the cgroup hierarchy upward from this process, returning the highest
/// (broadest-scope) writable memory.pressure file for PSI trigger arming.
fn find_trigger_path() -> Option<String> {
    let cgroup_content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cgroup_content
        .lines()
        .find(|l| l.starts_with("0::"))?
        .trim_start_matches("0::")
        .trim_matches('/');

    if rel.is_empty() {
        return None;
    }

    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let mut best: Option<String> = None;
    for len in (1..=parts.len()).rev() {
        let path = format!("/sys/fs/cgroup/{}/memory.pressure", parts[..len].join("/"));
        if std::fs::OpenOptions::new().read(true).write(true).open(&path).is_ok() {
            best = Some(path);
        } else {
            break;
        }
    }
    best
}

fn main() -> ! {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: mgd-psi-trigger <stall_us>");
        std::process::exit(1);
    }
    let stall_us: u64 = match args[1].parse() {
        Ok(v) if v > 0 => v,
        _ => {
            eprintln!("mgd-psi-trigger: stall_us must be a positive integer");
            std::process::exit(1);
        }
    };

    let psi_path = find_trigger_path().unwrap_or_else(|| {
        eprintln!("mgd-psi-trigger: no writable cgroup PSI file found");
        std::process::exit(2);
    });

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&psi_path)
        .unwrap_or_else(|e| {
            eprintln!("mgd-psi-trigger: open {psi_path}: {e}");
            std::process::exit(2);
        });

    // 2s window: valid on kernel <7.x ([500ms,10s]) and 7.x+ (min 2s, must be multiple of 2s).
    let window_us: u64 = 2_000_000;
    let threshold = (stall_us as f64 * window_us as f64 / 1_000_000.0) as u64;
    let trigger = format!("some {threshold} {window_us}");
    if let Err(e) = (&file).write_all(trigger.as_bytes()) {
        eprintln!("mgd-psi-trigger: arm '{trigger}' on {psi_path}: {e}");
        std::process::exit(2);
    }

    let mut out = io::stdout().lock();

    let psi_fd = file.as_raw_fd();

    loop {
        let mut pfd = libc::pollfd {
            fd: psi_fd,
            events: libc::POLLPRI,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 5000) };
        if ret > 0 {
         
            if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                eprintln!("mgd-psi-trigger: poll error/hangup on psi fd");
                std::process::exit(2);
            }
            if (pfd.revents & libc::POLLPRI) != 0 {
                if let Err(e) = out.write_all(&[0x01]) {
                    if e.kind() == io::ErrorKind::BrokenPipe {
                        std::process::exit(0);
                    }
                    eprintln!("mgd-psi-trigger: write to daemon pipe: {e}");
                    std::process::exit(2);
                }
                let _ = out.flush();
            }
        } else if ret == 0 {
            // Timeout: probe whether daemon closed its end of the pipe.
            let mut probe = libc::pollfd {
                fd: out.as_raw_fd(),
                events: 0,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut probe, 1, 0) };
            if r > 0 && (probe.revents & libc::POLLHUP) != 0 {
                std::process::exit(0);
            }
        } else {
            let errno = unsafe { *libc::__errno_location() };
            if errno != libc::EINTR {
                eprintln!("mgd-psi-trigger: poll failed errno={errno}");
                std::process::exit(2);
            }
        }
    }
}
