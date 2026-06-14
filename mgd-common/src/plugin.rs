use std::io::Write;
use std::os::unix::net::UnixStream;
use crate::protocol::PluginMessage;
use crate::socket::socket_path;

/// Connects to the mgd core daemon and sends the Identify message.
pub fn connect_and_identify(name: &str, version: &str, capabilities: Vec<&str>) -> UnixStream {
    let path = socket_path();
    let mut retries = 0;
    let mut stream = loop {
        match UnixStream::connect(&path) {
            Ok(s) => break s,
            Err(e) => {
                retries += 1;
                if retries >= 20 {
                    eprintln!("Failed to connect to mgd after 20 attempts: {e}");
                    std::process::exit(1);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };

    let identify = PluginMessage::Identify {
        name: name.to_string(),
        version: version.to_string(),
        capabilities: capabilities.into_iter().map(|s| s.to_string()).collect(),
    };
    let _ = writeln!(stream, "{}", serde_json::to_string(&identify).unwrap());
    stream
}
