//! Single instance: a running Goro listens on a per-user local socket, and later `goro`
//! invocations (and agent hooks) hand their requests to it.
//!
//! Protocol: the client sends one line, `<verb>\t<path>`; an empty path means "detect".
//! - `open`: open or focus a repository; answered with `ok`.
//! - `turn`: an agent hook recorded a turn in this repository; answered with `ok`.
//! - `wait`: open the repository and hold the connection until the user submits a
//!   review; answered with `review` and the markdown, or `cancelled`.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use futures::channel::oneshot;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream, prelude::*,
};

pub enum Request {
    Open(Option<PathBuf>),
    TurnRecorded(PathBuf),
    /// The review markdown goes to the sender; dropping it cancels.
    Wait(Option<PathBuf>, oneshot::Sender<String>),
}

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
    expect_ok(request(name, "open", target)?)
}

/// Tell a running instance that a turn was recorded in `root`.
pub fn send_turn_recorded(name: &Name<'_>, root: &Path) -> std::io::Result<()> {
    expect_ok(request(name, "turn", Some(root))?)
}

/// Open `target` in a running instance and block until the user submits a review.
/// `Ok(None)` if they cancelled (closed the window without sending).
pub fn wait_for_review(name: &Name<'_>, target: Option<&Path>) -> std::io::Result<Option<String>> {
    let stream = request(name, "wait", target)?;
    let mut reader = BufReader::new(&stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    match status.trim() {
        "review" => {
            let mut markdown = String::new();
            reader.read_to_string(&mut markdown)?;
            Ok(Some(markdown))
        }
        _ => Ok(None),
    }
}

fn request(name: &Name<'_>, verb: &str, target: Option<&Path>) -> std::io::Result<Stream> {
    let stream = Stream::connect(name.borrow())?;
    let mut writer = &stream;
    let path = target.map(encode_path).unwrap_or_default();
    writer.write_all(verb.as_bytes())?;
    writer.write_all(b"\t")?;
    writer.write_all(&path)?;
    writer.write_all(b"\n")?;
    Ok(stream)
}

fn expect_ok(stream: Stream) -> std::io::Result<()> {
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("unexpected reply {reply:?}")))
    }
}

/// Listen on `name` and call `on_request` for each request, on background threads.
/// Replaces a stale socket left by a crashed instance.
pub fn serve(
    name: Name<'static>,
    on_request: impl Fn(Request) + Send + Sync + 'static,
) -> std::io::Result<()> {
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    let on_request = std::sync::Arc::new(on_request);
    std::thread::Builder::new()
        .name("goro-ipc".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let on_request = on_request.clone();
                // Each connection on its own thread: a `wait` holds its connection open.
                let _ = std::thread::Builder::new()
                    .name("goro-ipc-conn".into())
                    .spawn(move || handle(stream, &*on_request));
            }
        })?;
    Ok(())
}

fn handle(stream: Stream, on_request: &dyn Fn(Request)) {
    let mut line = Vec::new();
    if BufReader::new(&stream)
        .read_until(b'\n', &mut line)
        .is_err()
    {
        return;
    }
    let line = line.strip_suffix(b"\n").unwrap_or(&line);
    let Some(tab) = line.iter().position(|b| *b == b'\t') else {
        return;
    };
    let (verb, path) = (&line[..tab], &line[tab + 1..]);
    let target = (!path.is_empty()).then(|| decode_path(path));
    let mut writer = &stream;
    match verb {
        b"open" => {
            on_request(Request::Open(target));
            let _ = writer.write_all(b"ok\n");
        }
        b"turn" => {
            if let Some(root) = target {
                on_request(Request::TurnRecorded(root));
            }
            let _ = writer.write_all(b"ok\n");
        }
        b"wait" => {
            let (tx, rx) = oneshot::channel();
            on_request(Request::Wait(target, tx));
            match futures::executor::block_on(rx) {
                Ok(markdown) => {
                    let _ = writer.write_all(b"review\n");
                    let _ = writer.write_all(markdown.as_bytes());
                }
                Err(_) => {
                    let _ = writer.write_all(b"cancelled\n");
                }
            }
        }
        _ => {}
    }
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

    fn serve_test(id: &str) -> mpsc::Receiver<Request> {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        serve(socket_name(id).unwrap(), move |request| {
            tx.lock().unwrap().send(request).unwrap();
        })
        .unwrap();
        rx
    }

    #[test]
    fn open_and_turn_requests_reach_the_running_instance() {
        let id = format!("goro-test-open-{}", std::process::id());
        let name = socket_name(&id).unwrap();
        assert!(send_open(&name, None).is_err(), "nothing listening yet");
        let rx = serve_test(&id);
        send_open(&name, Some(Path::new("/work/some repo"))).unwrap();
        send_open(&name, None).unwrap();
        send_turn_recorded(&name, Path::new("/work/r")).unwrap();
        let next = || rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(next(), Request::Open(Some(p)) if p == Path::new("/work/some repo")));
        assert!(matches!(next(), Request::Open(None)));
        assert!(matches!(next(), Request::TurnRecorded(p) if p == Path::new("/work/r")));
    }

    #[test]
    fn wait_returns_the_review_or_cancellation() {
        let id = format!("goro-test-wait-{}", std::process::id());
        let name = socket_name(&id).unwrap();
        let rx = serve_test(&id);
        let responder = std::thread::spawn(move || {
            let Request::Wait(target, reply) = rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected a wait request");
            };
            assert_eq!(target.as_deref(), Some(Path::new("/w")));
            reply.send("# Review comments (1)\n".into()).unwrap();
            let Request::Wait(_, reply) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else {
                panic!("expected a wait request");
            };
            drop(reply);
        });
        assert_eq!(
            wait_for_review(&name, Some(Path::new("/w")))
                .unwrap()
                .as_deref(),
            Some("# Review comments (1)\n")
        );
        assert_eq!(wait_for_review(&name, None).unwrap(), None, "cancelled");
        responder.join().unwrap();
    }
}
