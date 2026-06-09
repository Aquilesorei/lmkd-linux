use std::io::Write;
use std::os::unix::net::UnixStream;
use crate::protocol::PluginMessage;
use crate::socket::socket_path;

/// Connects to the mgd core daemon and sends the Identify message.
pub fn connect_and_identify(name: &str, version: &str, capabilities: Vec<&str>) -> UnixStream {
    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to connect to mgd: {e}");
            std::process::exit(1);
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
