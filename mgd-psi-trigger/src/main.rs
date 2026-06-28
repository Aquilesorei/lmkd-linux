// mgd-psi-trigger: arm the kernel PSI pressure trigger and proxy events to the
// daemon via stdout.
//
// Requires cap_perfmon+ep (set by install.sh --privileged).
//
// Usage: mgd-psi-trigger <stall_us>
//   stall_us — threshold in microseconds for "some" stall in a 1s window
//              e.g. 50000 = 5% of 1,000,000 µs
//
// Stdout: one byte (0x01) per pressure event. Daemon polls this pipe with
//         POLLIN; the main PSI fd stays entirely inside this process.
//
// Exit codes: 0 clean, 1 bad args, 2 open/arm failed.

use std::io::{self, Write};
use std::os::unix::io::AsRawFd;

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

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/proc/pressure/memory")
        .unwrap_or_else(|e| {
            eprintln!("mgd-psi-trigger: open /proc/pressure/memory: {e}");
            std::process::exit(2);
        });

    let trigger = format!("some {stall_us} 1000000");
    if let Err(e) = (&file).write_all(trigger.as_bytes()) {
        eprintln!("mgd-psi-trigger: arm '{trigger}': {e}");
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
