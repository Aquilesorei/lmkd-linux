use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CRIU_CANDIDATES: &[&str] = &[
    "/usr/sbin/criu",
    "/usr/bin/criu",
    "/sbin/criu",
    "/bin/criu",
    "/usr/local/sbin/criu",
    "/usr/local/bin/criu",
];

const EXIT_INVALID_ARGS: i32 = 1;
const EXIT_SECURITY_FAIL: i32 = 2;
const EXIT_CRIU_FAIL: i32 = 3;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(EXIT_INVALID_ARGS);
    }

    let caller_uid = unsafe { libc::getuid() };

    match args[1].as_str() {
        "dump" => {
            if args.len() < 4 {
                eprintln!("Error: dump requires PID and images-dir");
                std::process::exit(EXIT_INVALID_ARGS);
            }
            let pid: u32 = match args[2].parse() {
                Ok(p) => p,
                Err(_) => {
                    eprintln!("Error: invalid PID");
                    std::process::exit(EXIT_INVALID_ARGS);
                }
            };
            let images_dir = &args[3];

            if let Err(e) = validate_dump(caller_uid, pid, images_dir) {
                eprintln!("Security Error: {e}");
                std::process::exit(EXIT_SECURITY_FAIL);
            }

            execute_criu("dump", pid, images_dir);
        }
        "restore" => {
            if args.len() < 3 {
                eprintln!("Error: restore requires images-dir");
                std::process::exit(EXIT_INVALID_ARGS);
            }
            let images_dir = &args[2];

            if let Err(e) = validate_restore(caller_uid, images_dir) {
                eprintln!("Security Error: {e}");
                std::process::exit(EXIT_SECURITY_FAIL);
            }

            execute_criu("restore", 0, images_dir);
        }
        _ => {
            print_usage();
            std::process::exit(EXIT_INVALID_ARGS);
        }
    }
}

fn print_usage() {
    eprintln!("Usage: mgd-checkpoint dump <pid> <images-dir>");
    eprintln!("       mgd-checkpoint restore <images-dir>");
}

fn get_user_home(uid: u32) -> Result<String, String> {
    unsafe {
        let pwd = libc::getpwuid(uid);
        if pwd.is_null() {
            return Err("failed to get user passwd entry".to_string());
        }
        let home_str = std::ffi::CStr::from_ptr((*pwd).pw_dir)
            .to_string_lossy()
            .into_owned();
        Ok(home_str)
    }
}

fn validate_dump(caller_uid: u32, pid: u32, images_dir: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    // 1. Process Ownership: verify /proc/<pid> is owned by the calling user
    let proc_path = format!("/proc/{}", pid);
    let proc_meta = fs::metadata(&proc_path)
        .map_err(|e| format!("target PID does not exist or is inaccessible: {e}"))?;
    if proc_meta.uid() != caller_uid {
        return Err("target process is not owned by the calling user".to_string());
    }

    // 2. Cgroup Check: target process must reside inside user.slice
    let cgroup_data = fs::read_to_string(format!("/proc/{}/cgroup", pid))
        .map_err(|e| format!("failed to read target cgroup: {e}"))?;
    if !mgd_common::process::is_cgroup_in_user_slice(&cgroup_data) {
        return Err("target process is not in the user.slice cgroup".to_string());
    }

    // 3. Images Directory Check
    validate_dir(caller_uid, images_dir)?;

    Ok(())
}

fn validate_restore(caller_uid: u32, images_dir: &str) -> Result<(), String> {
    validate_dir(caller_uid, images_dir)
}

fn validate_dir(caller_uid: u32, dir: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let path = Path::new(dir);
    if !path.exists() {
        return Err("images directory does not exist".to_string());
    }

    let canonical = path.canonicalize()
        .map_err(|e| format!("failed to resolve images directory: {e}"))?;
    let meta = fs::metadata(&canonical)
        .map_err(|e| format!("failed to read images directory metadata: {e}"))?;
    
    // Directory must be owned by the calling user
    if meta.uid() != caller_uid {
        return Err("images directory is not owned by the calling user".to_string());
    }

    // Directory must be located under user's home directory
    let home = get_user_home(caller_uid)?;
    let path_str = canonical.to_string_lossy();
    if !path_str.starts_with(&home) {
        return Err("images directory is outside the user's home directory".to_string());
    }

    Ok(())
}

fn resolve_criu_path() -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;

    for candidate in CRIU_CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.exists() {
            let c_str = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| "NUL byte in path")?;
            let is_exec = unsafe { libc::access(c_str.as_ptr(), libc::X_OK) == 0 };
            if is_exec {
                return Ok(path);
            }
        }
    }
    Err("criu binary not found in root-controlled paths".to_string())
}

/// Sets ambient capabilities so they are inherited by the child CRIU process
fn setup_ambient_capabilities() -> Result<(), String> {
    const CAP_SYS_PTRACE: u32 = 19;
    const CAP_NET_ADMIN: u32 = 12;
    const CAP_CHECKPOINT_RESTORE: u32 = 40;

    unsafe {
        let mut header = CapHeader {
            version: 0x20080522, // _LINUX_CAPABILITY_VERSION_3
            pid: 0,
        };
        let mut data = [CapData::default(); 2];

        // 1. Get current capability sets
        if libc::syscall(libc::SYS_capget, &mut header as *mut _, data.as_mut_ptr()) != 0 {
            return Err(format!("capget failed: {}", std::io::Error::last_os_error()));
        }

        // 2. Add our capabilities to the Inheritable set
        // (Must be in Permitted to be moved to Inheritable)
        for cap in &[CAP_SYS_PTRACE, CAP_NET_ADMIN, CAP_CHECKPOINT_RESTORE] {
            let idx = (cap / 32) as usize;
            let bit = 1 << (cap % 32);
            if (data[idx].permitted & bit) != 0 {
                data[idx].inheritable |= bit;
            }
        }

        // 3. Apply capability sets
        if libc::syscall(libc::SYS_capset, &mut header as *mut _, data.as_ptr()) != 0 {
            return Err(format!("capset failed: {}", std::io::Error::last_os_error()));
        }

        // 4. Raise ambient capabilities
        let mut raised = 0u32;
        for cap in &[CAP_SYS_PTRACE, CAP_NET_ADMIN, CAP_CHECKPOINT_RESTORE] {
            let idx = (cap / 32) as usize;
            let bit = 1 << (cap % 32);
            if (data[idx].permitted & bit) != 0 {
                if libc::prctl(47, 2, *cap as libc::c_ulong, 0, 0) != 0 {
                    return Err(format!(
                        "prctl PR_CAP_AMBIENT_RAISE ({cap}) failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                raised += 1;
            }
        }
        if raised == 0 {
            return Err(
                "no required capabilities in Permitted set — run install.sh to apply setcap on mgd-checkpoint".to_string()
            );
        }
    }
    Ok(())
}

fn execute_criu(action: &str, pid: u32, images_dir: &str) {
    let criu_bin = match resolve_criu_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("CRIU Error: {e}");
            std::process::exit(EXIT_CRIU_FAIL);
        }
    };

    if let Err(e) = setup_ambient_capabilities() {
        // Log capability warning but still try to run (might be running as root already)
        eprintln!("Capability Warning: {e}");
    }

    let mut cmd = Command::new(&criu_bin);
    cmd.env_clear(); // Purge all env vars for security

    match action {
        "dump" => {
            cmd.args([
                "dump",
                "--tree", &pid.to_string(),
                "--images-dir", images_dir,
                "--shell-job",
                "--leave-stopped",
                "--ext-unix-sk",
                "--tcp-established",
                "--file-locks",
            ]);
        }
        "restore" => {
            cmd.args([
                "restore",
                "--images-dir", images_dir,
                "--shell-job",
                "--restore-detached",
            ]);
        }
        _ => unreachable!(),
    };

    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Execution Error: failed to execute CRIU: {e}");
            std::process::exit(EXIT_CRIU_FAIL);
        }
    };

    if status.success() {
        std::process::exit(0);
    } else {
        eprintln!("CRIU Error: child process exited with code {:?}", status.code());
        std::process::exit(EXIT_CRIU_FAIL);
    }
}
