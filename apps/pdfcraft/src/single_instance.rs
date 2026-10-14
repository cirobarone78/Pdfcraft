//! Windows document-only handoff through the socket introduced by #715/#602.
//!
//! Socket ACLs inherit from the profile directory. `uds_windows` exposes no peer token;
//! fresh user-protected challenges additionally guard paths and queue admission even if
//! another account can reach or replace that path. This authenticates possession of the
//! Windows user's data-protection credentials, not an application or logon session.
//! See `docs/security/windows-single-instance.md` for guarantees and exclusions.
//!
//! All failures still open an independent window. Launch options are never forwarded.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pdfcraft_ui_egui::{OsEvent, OsEventsFn};

const VERSION: u64 = 1;
const MAX_REQUEST: usize = 1 << 20;
// Allow bounded Windows protection overhead, while separately limiting plaintext.
const MAX_FRAME: usize = MAX_REQUEST + 4096;
const MAX_HELLO: usize = 4096;
const MAX_PATHS: usize = 256;
const MAX_PATH_BYTES: usize = 32_768;
const MAX_BATCHES: usize = 16;
const MAX_QUEUED_PATHS: usize = 1024;
const MAX_QUEUED_BYTES: usize = 4 << 20;
const MAX_CONNECTIONS: usize = 8;
#[cfg(windows)]
const WORKERS: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);
type Nonce = [u8; 32];

pub enum Claim {
    HandedOff,
    Primary(Server),
    Alone,
}

#[derive(Default)]
struct Inbox {
    paths: Vec<String>,
    bytes: usize,
    batches: usize,
    wake: Option<Box<dyn Fn() + Send>>,
}

pub struct Server {
    inbox: Arc<Mutex<Inbox>>,
    #[cfg(windows)]
    listener: Option<Listener>,
}

impl Server {
    fn new() -> Self {
        Self {
            inbox: Arc::default(),
            #[cfg(windows)]
            listener: None,
        }
    }

    #[cfg(test)]
    fn deliver(&self, paths: Vec<String>) -> io::Result<()> {
        deliver(&self.inbox, paths)
    }

    pub fn connect(&self, ctx: &egui::Context) -> OsEventsFn {
        let wake = ctx.clone();
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner).wake = Some(Box::new(move || wake.request_repaint()));
        let inbox = Arc::clone(&self.inbox);
        let ctx = ctx.clone();
        Box::new(move || {
            let paths = {
                let mut inbox = inbox.lock().unwrap_or_else(PoisonError::into_inner);
                inbox.bytes = 0;
                inbox.batches = 0;
                std::mem::take(&mut inbox.paths)
            };
            if paths.is_empty() {
                return Vec::new();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            vec![OsEvent::Open(paths)]
        })
    }
}

fn deliver(inbox: &Mutex<Inbox>, paths: Vec<String>) -> io::Result<()> {
    let bytes = validate_paths(&paths)?;
    let mut inbox = inbox.lock().unwrap_or_else(PoisonError::into_inner);
    if inbox.batches >= MAX_BATCHES
        || inbox.paths.len().saturating_add(paths.len()) > MAX_QUEUED_PATHS
        || inbox.bytes.saturating_add(bytes) > MAX_QUEUED_BYTES
    {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "document inbox is full"));
    }
    inbox.bytes += bytes;
    inbox.batches += 1;
    inbox.paths.extend(paths);
    if let Some(wake) = &inbox.wake {
        wake();
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn validate_paths(paths: &[String]) -> io::Result<usize> {
    if paths.is_empty() || paths.len() > MAX_PATHS {
        return Err(invalid("invalid document path count"));
    }
    let mut bytes = 0usize;
    for path in paths {
        if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') || !std::path::Path::new(path).is_absolute() {
            return Err(invalid("invalid absolute document path"));
        }
        bytes = bytes.saturating_add(path.len());
        if bytes > MAX_REQUEST {
            return Err(invalid("document paths are too large"));
        }
    }
    Ok(bytes)
}

fn encode(value: serde_json::Value) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&value).map_err(|_| invalid("cannot encode open protocol"))?;
    if bytes.len() > MAX_REQUEST {
        return Err(invalid("open protocol message is too large"));
    }
    Ok(bytes)
}

fn object(bytes: &[u8], kind: &str, keys: &[&str]) -> io::Result<serde_json::Map<String, serde_json::Value>> {
    if bytes.len() > MAX_REQUEST {
        return Err(invalid("open protocol message is too large"));
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid("invalid open protocol JSON"))?;
    let obj = value.as_object().ok_or_else(|| invalid("not an open protocol object"))?;
    if obj.len() != keys.len()
        || !keys.iter().all(|key| obj.contains_key(*key))
        || obj.get("version").and_then(|v| v.as_u64()) != Some(VERSION)
        || obj.get("kind").and_then(|v| v.as_str()) != Some(kind)
    {
        return Err(invalid("unsupported open protocol schema or version"));
    }
    Ok(obj.clone())
}

fn nonce(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> io::Result<Nonce> {
    serde_json::from_value(obj.get(key).cloned().ok_or_else(|| invalid("missing nonce"))?).map_err(|_| invalid("invalid nonce"))
}

fn same_nonce(a: &Nonce, b: &Nonce) -> bool {
    a.iter().zip(b).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

fn challenge(server: Nonce) -> io::Result<Vec<u8>> {
    encode(serde_json::json!({"version": VERSION, "kind": "challenge", "server": server}))
}

fn parse_challenge(bytes: &[u8]) -> io::Result<Nonce> {
    nonce(&object(bytes, "challenge", &["version", "kind", "server"])?, "server")
}

fn request(paths: &[String], server: Nonce, client: Nonce) -> io::Result<Vec<u8>> {
    validate_paths(paths)?;
    encode(serde_json::json!({"version": VERSION, "kind": "open", "server": server, "client": client, "open": paths}))
}

fn parse_request(bytes: &[u8], server: &Nonce) -> io::Result<(Vec<String>, Nonce)> {
    let obj = object(bytes, "open", &["version", "kind", "server", "client", "open"])?;
    if !same_nonce(&nonce(&obj, "server")?, server) {
        return Err(invalid("open challenge mismatch"));
    }
    let client = nonce(&obj, "client")?;
    let paths: Vec<String> =
        serde_json::from_value(obj.get("open").cloned().ok_or_else(|| invalid("missing paths"))?).map_err(|_| invalid("invalid paths"))?;
    validate_paths(&paths)?;
    Ok((paths, client))
}

fn acknowledgment(server: Nonce, client: Nonce) -> io::Result<Vec<u8>> {
    encode(serde_json::json!({"version": VERSION, "kind": "queued", "server": server, "client": client}))
}

fn parse_acknowledgment(bytes: &[u8], server: &Nonce, client: &Nonce) -> io::Result<()> {
    let obj = object(bytes, "queued", &["version", "kind", "server", "client"])?;
    if !same_nonce(&nonce(&obj, "server")?, server) || !same_nonce(&nonce(&obj, "client")?, client) {
        return Err(invalid("open acknowledgment mismatch"));
    }
    Ok(())
}

fn read_frame(reader: &mut impl Read, cap: usize) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > cap {
        return Err(invalid("invalid open frame size"));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn write_frame(writer: &mut impl Write, bytes: &[u8], cap: usize) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > cap {
        return Err(invalid("invalid open frame size"));
    }
    let len = u32::try_from(bytes.len()).map_err(|_| invalid("invalid open frame size"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

/// A total deadline, rather than a fresh timeout for each byte of a trickled request.
struct DeadlineIo<S> {
    inner: S,
    deadline: Instant,
    stop: Arc<AtomicBool>,
}

fn check_deadline(deadline: Instant, stop: &AtomicBool) -> io::Result<()> {
    if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "open exchange ended"));
    }
    Ok(())
}

impl<S: Read> Read for DeadlineIo<S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            check_deadline(self.deadline, &self.stop)?;
            match self.inner.read(bytes) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => std::thread::sleep(POLL),
                result => return result,
            }
        }
    }
}

impl<S: Write> Write for DeadlineIo<S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            check_deadline(self.deadline, &self.stop)?;
            match self.inner.write(bytes) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => std::thread::sleep(POLL),
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        check_deadline(self.deadline, &self.stop)?;
        self.inner.flush()
    }
}

struct Permit(Arc<AtomicUsize>);
impl Permit {
    fn acquire(count: &Arc<AtomicUsize>) -> Option<Self> {
        let mut current = count.load(Ordering::Acquire);
        while current < MAX_CONNECTIONS {
            match count.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Self(Arc::clone(count))),
                Err(actual) => current = actual,
            }
        }
        None
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Preserve the launch's path handling; validation failure only disables handoff.
pub fn absolute(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| std::path::absolute(p).map_or_else(|_| p.clone(), |a| a.to_string_lossy().into_owned())).collect()
}

/// Options and bare launches keep their own window, just as #715 specifies.
pub fn may_hand_off(new_window: bool, files: &[String], has_options: bool, has_control: bool, create_images: bool) -> bool {
    !new_window && !files.is_empty() && !has_options && !has_control && !create_images
}

pub fn claim(files: &[String], may_hand_off: bool) -> Claim {
    #[cfg(windows)]
    {
        match socket_path() {
            Ok(path) => claim_at(&path, files, may_hand_off),
            Err(e) => {
                log::info!("single instance off: endpoint unavailable ({e})");
                Claim::Alone
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (files, may_hand_off);
        Claim::Alone
    }
}

#[cfg(windows)]
fn socket_path() -> io::Result<std::path::PathBuf> {
    let token = winsafe::HPROCESS::GetCurrentProcess()
        .OpenProcessToken(winsafe::co::TOKEN::QUERY | winsafe::co::TOKEN::IMPERSONATE)
        .map_err(io::Error::other)?;
    let winsafe::TokenInfo::SessionId(session) =
        token.GetTokenInformation(winsafe::co::TOKEN_INFORMATION_CLASS::SessionId).map_err(io::Error::other)?
    else {
        return Err(invalid("Windows session is unavailable"));
    };
    let name = socket_name(*session, pdfcraft_ui_egui::portable::data_dir().is_some());
    let folder =
        winsafe::SHGetKnownFolderPath(&winsafe::co::KNOWNFOLDERID::LocalAppData, winsafe::co::KF::DEFAULT, Some(&token)).map_err(io::Error::other)?;
    let folder = std::path::PathBuf::from(folder);
    if !folder.is_absolute() {
        return Err(invalid("Windows profile is unavailable"));
    }
    Ok(folder.join("PdfCraft").join(name))
}

fn socket_name(session: u32, portable: bool) -> String {
    format!("{}instance-{session}.sock", if portable { "portable-" } else { "" })
}

#[cfg(windows)]
fn check_parents(path: &std::path::Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)) => {}
        _ => return Err(invalid("open endpoint is not a local absolute path")),
    }
    for dir in path.parent().into_iter().flat_map(std::path::Path::ancestors) {
        let metadata = match std::fs::symlink_metadata(dir) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid("open endpoint parent is not a plain directory"));
        }
    }
    Ok(())
}

/// The original startup lock, held through bind/listen or confirmed handoff. A failure
/// keeps an independent window; no unsynchronized cleanup/bind is attempted.
#[cfg(windows)]
fn election(socket: &std::path::Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    check_parents(socket)?;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    check_parents(socket)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        // Open the reparse point itself, and don't allow deletion/replacement while locked.
        .custom_flags(0x0020_0000)
        .share_mode(3)
        .open(socket.with_extension("lock"))?;
    if file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(invalid("open election lock is a reparse point"));
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(POLL),
            Err(e) => return Err(io::Error::other(e)),
        }
    }
}

/// `uds_windows` has no connect timeout. One bounded attempt runs away from the UI;
/// a late result is dropped when its receiver has timed out, without removing the socket.
#[cfg(windows)]
fn connect(path: &std::path::Path) -> io::Result<uds_windows::UnixStream> {
    let path = path.to_owned();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("open-connect".into()).spawn(move || {
        let _ = tx.send(uds_windows::UnixStream::connect(path));
    })?;
    rx.recv_timeout(TIMEOUT).map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "open connection timed out"))?
}

#[cfg(windows)]
fn claim_at(path: &std::path::Path, files: &[String], may_hand_off: bool) -> Claim {
    let Ok(_election) = election(path) else {
        log::info!("single instance off: election unavailable; opening a new window");
        return Claim::Alone;
    };
    for _ in 0..3 {
        use std::os::windows::fs::MetadataExt;
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            // Windows sockets are reparse points. Never erase an unrelated regular file.
            if metadata.file_attributes() & 0x400 == 0 {
                return Claim::Alone;
            }
        }
        match connect(path) {
            Ok(stream) => {
                if !may_hand_off {
                    return Claim::Alone;
                }
                return match hand_off(&stream, files) {
                    Ok(()) => Claim::HandedOff,
                    Err(e) => {
                        log::warn!("couldn't hand the files to the running PdfCraft ({e}); opening a new window");
                        Claim::Alone
                    }
                };
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {}
            Err(e) => {
                log::info!("single instance off: connection failed ({e}); opening a new window");
                return Claim::Alone;
            }
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                log::info!("single instance off: can't clear endpoint ({e})");
                return Claim::Alone;
            }
        }
        match uds_windows::UnixListener::bind(path) {
            Ok(listener) => return serve(listener),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
            Err(e) => {
                log::info!("single instance off: can't listen ({e})");
                return Claim::Alone;
            }
        }
    }
    Claim::Alone
}

#[cfg(windows)]
#[path = "single_instance_windows.rs"]
mod security;

#[cfg(windows)]
fn hand_off(stream: &uds_windows::UnixStream, files: &[String]) -> io::Result<()> {
    validate_paths(files)?;
    stream.set_nonblocking(true)?;
    let mut io = DeadlineIo { inner: stream, deadline: Instant::now() + TIMEOUT, stop: Arc::default() };
    let crypto = security::Protection::new()?;
    let sealed = read_frame(&mut io, MAX_HELLO)?;
    let server = parse_challenge(&crypto.unprotect(&sealed, io.deadline, &io.stop)?)?;
    let client = crypto.nonce()?;
    let sealed = crypto.protect(&request(files, server, client)?, io.deadline, &io.stop)?;
    write_frame(&mut io, &sealed, MAX_FRAME)?;
    let sealed = read_frame(&mut io, MAX_HELLO)?;
    parse_acknowledgment(&crypto.unprotect(&sealed, io.deadline, &io.stop)?, &server, &client)
}

#[cfg(windows)]
fn receive(stream: &uds_windows::UnixStream, inbox: &Mutex<Inbox>, deadline: Instant, stop: Arc<AtomicBool>) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut io = DeadlineIo { inner: stream, deadline, stop };
    let crypto = security::Protection::new()?;
    let server = crypto.nonce()?;
    let sealed = crypto.protect(&challenge(server)?, io.deadline, &io.stop)?;
    write_frame(&mut io, &sealed, MAX_HELLO)?;
    let sealed = read_frame(&mut io, MAX_FRAME)?;
    let (paths, client) = parse_request(&crypto.unprotect(&sealed, io.deadline, &io.stop)?, &server)?;
    check_deadline(io.deadline, &io.stop)?;
    deliver(inbox, paths)?;
    // Upstream behavior: ACK loss still lets the launcher fall back, possibly duplicating opens.
    let sealed = crypto.protect(&acknowledgment(server, client)?, io.deadline, &io.stop)?;
    write_frame(&mut io, &sealed, MAX_HELLO)
}

#[cfg(windows)]
struct Connection {
    stream: uds_windows::UnixStream,
    deadline: Instant,
    _permit: Permit,
}

#[cfg(windows)]
struct Listener {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
#[cfg(windows)]
impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// A fixed worker pool, at most eight active/waiting connections and one launch-owned listener.
#[cfg(windows)]
fn serve(listener: uds_windows::UnixListener) -> Claim {
    match start_listener(listener) {
        Ok(server) => Claim::Primary(server),
        Err(e) => {
            log::info!("single instance off: listener failed ({e}); opening a new window");
            Claim::Alone
        }
    }
}

#[cfg(windows)]
fn start_listener(listener: uds_windows::UnixListener) -> io::Result<Server> {
    listener.set_nonblocking(true)?;
    // Check crypto availability before advertising a primary; no unauthenticated downgrade.
    let _ = security::Protection::new()?;
    let mut server = Server::new();
    let stop = Arc::<AtomicBool>::default();
    let mut guard = Listener { stop: Arc::clone(&stop), threads: Vec::new() };
    let (tx, rx) = std::sync::mpsc::sync_channel::<Connection>(MAX_CONNECTIONS - WORKERS);
    let rx = Arc::new(Mutex::new(rx));
    for i in 0..WORKERS {
        let (rx, inbox, stop) = (Arc::clone(&rx), Arc::clone(&server.inbox), Arc::clone(&stop));
        guard.threads.push(std::thread::Builder::new().name(format!("open-receiver-{i}")).spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let result = rx.lock().unwrap_or_else(PoisonError::into_inner).try_recv();
                match result {
                    Ok(connection) => {
                        if let Err(e) = receive(&connection.stream, &inbox, connection.deadline, Arc::clone(&stop)) {
                            log::debug!("single instance: rejected exchange ({e})");
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => std::thread::sleep(POLL),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        })?);
    }
    let accept_stop = Arc::clone(&stop);
    let count = Arc::<AtomicUsize>::default();
    guard.threads.push(std::thread::Builder::new().name("single-instance".into()).spawn(move || {
        while !accept_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Some(permit) = Permit::acquire(&count) {
                        let _ = tx.try_send(Connection { stream, deadline: Instant::now() + TIMEOUT, _permit: permit });
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => {
                    log::debug!("single instance: accept failed ({e})");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    })?);
    server.listener = Some(guard);
    Ok(server)
}

#[cfg(test)]
#[path = "single_instance_tests.rs"]
mod tests;
