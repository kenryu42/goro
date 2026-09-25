//! Single instance: a running Goro listens on a per-user local socket, and later `goro`
//! invocations hand their request to it and exit.
//!
//! Protocol: the client sends one line, `open\t<path>` (empty path: detect), and the
//! server answers `ok`.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream, prelude::*,
};

/// The per-user socket name for `id` (`"goro"` in production; tests use unique ids).
pub fn socket_name(id: &str) -> std::io::Result<Name<'static>> {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into());
    if cfg!(any(target_os = "linux", windows)) {
        // Linux: abstract namespace; Windows: named pipe. Neither leaves files behind.
        format!("{id}-{user}.sock")
            .to_ns_name::<GenericNamespaced>()
            .map(Name::into_owned)
    } else {
        // macOS and other Unices: a socket file in the per-user temporary directory.
        std::env::temp_dir()
            .join(format!("{id}-{user}.sock"))
            .into_os_string()
            .to_fs_name::<GenericFilePath>()
            .map(Name::into_owned)
    }
}

/// Ask a running instance to open `target`. Fails if none is listening.
pub fn send_open(name: &Name<'_>, target: Option<&Path>) -> std::io::Result<()> {
    let stream = Stream::connect(name.borrow())?;
    let mut writer = &stream;
    let path = target.map(encode_path).unwrap_or_default();
    writer.write_all(b"open\t")?;
    writer.write_all(&path)?;
    writer.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("unexpected reply {reply:?}")))
    }
}

/// Listen on `name` and call `on_open` for each request, on a background thread.
/// Replaces a stale socket left by a crashed instance.
pub fn serve(
    name: Name<'static>,
    on_open: impl Fn(Option<PathBuf>) + Send + 'static,
) -> std::io::Result<()> {
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    std::thread::Builder::new()
        .name("goro-ipc".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = Vec::new();
                let mut reader = BufReader::new(&stream);
                if reader.read_until(b'\n', &mut line).is_err() {
                    continue;
                }
                let Some(rest) = line.strip_prefix(b"open\t") else {
                    continue;
                };
                let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
                let target = (!rest.is_empty()).then(|| decode_path(rest));
                on_open(target);
                let mut writer = &stream;
                let _ = writer.write_all(b"ok\n");
            }
        })?;
    Ok(())
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn encode_path(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(unix))]
fn decode_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn unique_id() -> String {
        format!("goro-test-{}-{}", std::process::id(), line!())
    }

    #[test]
    fn requests_reach_the_running_instance() {
        let id = unique_id();
        let name = socket_name(&id).unwrap();
        assert!(send_open(&name, None).is_err(), "nothing listening yet");

        let (tx, rx) = mpsc::channel();
        serve(socket_name(&id).unwrap(), move |target| {
            tx.send(target).unwrap()
        })
        .unwrap();
        send_open(&name, Some(Path::new("/work/some repo"))).unwrap();
        send_open(&name, None).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(PathBuf::from("/work/some repo"))
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), None);
    }
}
