//! The D-Bus side: `org.freedesktop.impl.portal.ScreenCast`, which xdg-desktop-portal calls on
//! behalf of an application, and the request and session objects that go with it.
//!
//! A call goes: `CreateSession` (a session object appears), `SelectSources` (what kind of
//! source, one or several), `Start` (the picker runs, a capture and a PipeWire stream are made
//! per chosen source, the PipeWire node ids go back). `Session.Close` or the source going away
//! (a window closing) ends it all.
//!
//! Responses: 0 success, 1 cancelled by the user, 2 anything else.
//!
//! This follows the zbus 5 API.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, ObjectServer};

use crate::config::Config;
use crate::model::{self, ChoiceError, Source};
use crate::stream::{self, PwStream};
use crate::wayland::{self, CaptureEvent};

const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

/// The only cursor mode advertised by `AvailableCursorModes`.
const CURSOR_HIDDEN: u32 = 1;

type Results = HashMap<String, OwnedValue>;

fn value<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("a value without file descriptors")
}

/// Everything the objects share.
pub struct Backend {
    pub wl: wayland::Handle,
    pub config: Config,
    sessions: Mutex<HashMap<String, Arc<SessionData>>>,
    connection: OnceLock<Connection>,
    next_stream: AtomicU64,
}

impl Backend {
    pub fn new(wl: wayland::Handle, config: Config) -> Arc<Self> {
        Arc::new(Self {
            wl,
            config,
            sessions: Mutex::new(HashMap::new()),
            connection: OnceLock::new(),
            next_stream: AtomicU64::new(1),
        })
    }

    pub fn set_connection(&self, connection: Connection) {
        let _ = self.connection.set(connection);
    }

    fn session(&self, path: &str) -> Option<Arc<SessionData>> {
        lock(&self.sessions).get(path).cloned()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one session knows.
#[derive(Default)]
struct SessionData {
    inner: Mutex<SessionInner>,
}

struct SessionInner {
    types: u32,
    multiple: bool,
    started: bool,
    cursor_mode: u32,
    persist_mode: u32,
    restore_token: Option<String>,
    captures: Vec<ActiveCapture>,
}

impl Default for SessionInner {
    fn default() -> Self {
        Self {
            types: model::TYPE_MONITOR,
            multiple: false,
            started: false,
            cursor_mode: CURSOR_HIDDEN,
            persist_mode: 0,
            restore_token: None,
            captures: Vec::new(),
        }
    }
}

struct ActiveCapture {
    wl_id: u64,
    /// Dropping it ends the PipeWire stream.
    _pw: PwStream,
}

/// Run a blocking job on a thread of its own and wait for it without blocking the D-Bus
/// executor.
async fn blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(job());
    });
    rx.recv().await.expect("a blocking job panicked")
}

/// End a session: its captures and streams go, its object leaves the bus.
async fn close_session(backend: &Arc<Backend>, path: &str, emit_closed: bool) {
    let Some(data) = lock(&backend.sessions).remove(path) else {
        return;
    };
    let captures = std::mem::take(&mut lock(&data.inner).captures);
    let wl = backend.wl.clone();
    blocking(move || {
        for capture in &captures {
            wl.stop(capture.wl_id);
        }
        // Dropping the streams ends their threads.
        drop(captures);
    })
    .await;
    if let Some(connection) = backend.connection.get() {
        if emit_closed {
            if let Ok(emitter) = SignalEmitter::new(connection, path.to_string()) {
                let _ = SessionObject::closed(&emitter).await;
            }
        }
        let _ = connection
            .object_server()
            .remove::<SessionObject, _>(path.to_string())
            .await;
    }
    tracing::info!("session {path} closed");
}

// ---------------------------------------------------------------------------------------------
// Request: only there so the frontend can cancel a `Start` that waits for the picker.

#[derive(Default)]
struct Cancel {
    cancelled: AtomicBool,
    /// The picker's process id while it runs.
    picker: AtomicI32,
}

impl Cancel {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let pid = self.picker.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: a signal to a process we started and have not yet waited for.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

struct RequestObject {
    cancel: Arc<Cancel>,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    fn close(&self) {
        self.cancel.cancel();
    }
}

// ---------------------------------------------------------------------------------------------
// Session

struct SessionObject {
    backend: Arc<Backend>,
    path: String,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    async fn close(&self) {
        close_session(&self.backend, &self.path, false).await;
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

// ---------------------------------------------------------------------------------------------
// ScreenCast

pub struct ScreenCast {
    pub backend: Arc<Backend>,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> (u32, Results) {
        let path = session_handle.to_string();
        tracing::info!("CreateSession from {app_id:?}: {path}");
        lock(&self.backend.sessions).insert(path.clone(), Arc::new(SessionData::default()));
        let object = SessionObject {
            backend: self.backend.clone(),
            path: path.clone(),
        };
        if let Err(err) = server.at(session_handle.clone(), object).await {
            tracing::error!("could not export the session object: {err}");
            lock(&self.backend.sessions).remove(&path);
            return (RESPONSE_OTHER, Results::new());
        }
        let mut results = Results::new();
        results.insert("session_id".into(), value(path));
        (RESPONSE_OK, results)
    }

    async fn select_sources(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let Some(session) = self.backend.session(session_handle.as_str()) else {
            return (RESPONSE_OTHER, Results::new());
        };
        let types = options
            .get("types")
            .and_then(|v| v.downcast_ref::<u32>().ok());
        let multiple = options
            .get("multiple")
            .and_then(|v| v.downcast_ref::<bool>().ok())
            .unwrap_or(false);

        // Parse the modes before taking the lock, so the guard is not held across any
        // await that follows. `bad_cursor_mode` and `bad_persist_mode` are decided here
        // and acted on after the scope ends.
        let cursor_mode = options
            .get("cursor_mode")
            .and_then(|v| v.downcast_ref::<u32>().ok());
        let persist_mode = options
            .get("persist_mode")
            .and_then(|v| v.downcast_ref::<u32>().ok());
        let restore_token = options
            .get("restore_token")
            .and_then(|v| v.downcast_ref::<String>().ok());

        let bad_cursor = cursor_mode.is_some_and(|m| m != CURSOR_HIDDEN);
        let bad_persist = persist_mode.is_some_and(|p| p > 2);

        // Apply the settings, then drop the guard before any await.
        {
            let mut inner = lock(&session.inner);
            inner.types = model::effective_types(types);
            inner.multiple = multiple;
            if let Some(mode) = cursor_mode.filter(|m| *m == CURSOR_HIDDEN) {
                inner.cursor_mode = mode;
            }
            if let Some(pm) = persist_mode.filter(|p| *p <= 2) {
                inner.persist_mode = pm;
            }
            if let Some(token) = restore_token.clone() {
                inner.restore_token = Some(token);
            }
            jdbg!(
                "dbus SelectSources types={types:?} multiple={multiple} cursor_mode={} persist_mode={}",
                inner.cursor_mode,
                inner.persist_mode
            );
            tracing::info!(
                "SelectSources: types {} multiple {} cursor {} persist {}",
                inner.types,
                inner.multiple,
                inner.cursor_mode,
                inner.persist_mode
            );
        }

        // Per the spec, an unadvertised cursor mode or an invalid persist mode closes the
        // session. Do that after the lock is released.
        if bad_cursor {
            tracing::warn!(
                "SelectSources: cursor_mode {cursor_mode:?} not advertised (only {CURSOR_HIDDEN}), closing session"
            );
            close_session(&self.backend, session_handle.as_str(), true).await;
            return (RESPONSE_OTHER, Results::new());
        }
        if bad_persist {
            tracing::warn!(
                "SelectSources: persist_mode {persist_mode:?} out of range, closing session"
            );
            close_session(&self.backend, session_handle.as_str(), true).await;
            return (RESPONSE_OTHER, Results::new());
        }

        (RESPONSE_OK, Results::new())
    }

    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> (u32, Results) {
        let path = session_handle.to_string();
        let Some(session) = self.backend.session(&path) else {
            return (RESPONSE_OTHER, Results::new());
        };
        tracing::info!("Start from {app_id:?}: {path}");
        jdbg!("dbus Start: app_id={app_id:?} session={path}");

        let cancel = Arc::new(Cancel::default());
        let request_path = OwnedObjectPath::from(handle);
        let _ = server
            .at(
                request_path.clone(),
                RequestObject {
                    cancel: cancel.clone(),
                },
            )
            .await;
        let outcome = start_session(&self.backend, &path, &session, &cancel).await;
        let _ = server.remove::<RequestObject, _>(request_path).await;

        match outcome {
            Ok((streams, restore_token)) => {
                jdbg!(
                    "dbus Start OK: returning streams {streams:?} restore_token={restore_token:?}"
                );
                let mut results = Results::new();
                results.insert("streams".into(), value(Value::new(streams)));
                if let Some(token) = restore_token {
                    results.insert("restore_token".into(), value(token));
                }
                (RESPONSE_OK, results)
            }
            Err(Failure::Cancelled) => (RESPONSE_CANCELLED, Results::new()),
            Err(Failure::Other(why)) => {
                tracing::error!("Start failed: {why}");
                (RESPONSE_OTHER, Results::new())
            }
        }
    }

    /// `org.freedesktop.impl.portal.ScreenCast.OpenPipeWireRemote`: return a file descriptor
    /// for a PipeWire connection on which the session's stream nodes are visible. The client
    /// passes it to `pw_context_connect_fd`.
    ///
    /// We hand out a connected socket to the daemon (the same thing `pw-cli` and the other
    /// backends do), not a `pw_core` fd. `pipewire-rs` does not expose the core fd, and the
    /// daemon socket is the documented interface anyway.
    async fn open_pipe_wire_remote(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        let path = session_handle.to_string();
        if self.backend.session(&path).is_none() {
            return Err(zbus::fdo::Error::Failed(format!("no session at {path}")));
        }
        let opened = blocking(open_pipewire_socket)
            .await
            .map_err(zbus::fdo::Error::Failed)?;
        tracing::info!("OpenPipeWireRemote: handed a remote fd to the client");
        Ok(opened)
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        model::AVAILABLE_TYPES
    }

    /// Hidden only: a window's picture never has a cursor.
    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        CURSOR_HIDDEN
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

enum Failure {
    Cancelled,
    Other(String),
}

/// One stream of the `streams` result: `(pipewire node id, properties)`.
type StreamEntry = (u32, HashMap<String, OwnedValue>);

async fn start_session(
    backend: &Arc<Backend>,
    path: &str,
    session: &Arc<SessionData>,
    cancel: &Arc<Cancel>,
) -> Result<(Vec<StreamEntry>, Option<String>), Failure> {
    let (types, multiple) = {
        let mut inner = lock(&session.inner);
        if inner.started {
            return Err(Failure::Other("the session was already started".into()));
        }
        inner.started = true;
        (inner.types, inner.multiple)
    };
    if types == 0 {
        return Err(Failure::Other(
            "no source type this portal can share".into(),
        ));
    }

    // What can be shared, and which of it the picker is shown.
    let wl = backend.wl.clone();
    let sources = blocking(move || wl.list_sources())
        .await
        .map_err(|e| Failure::Other(format!("{e:#}")))?;
    let entries = model::picker_entries(&sources, types);
    if entries.is_empty() {
        return Err(Failure::Other("nothing to share".into()));
    }

    let chooser = backend.config.chooser.clone();
    let input = entries
        .iter()
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let picker_cancel = cancel.clone();
    let output = blocking(move || run_picker(&chooser, &input, &picker_cancel))
        .await
        .map_err(Failure::Other)?;
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    let chosen = match model::parse_choice(&output, &entries, multiple) {
        Ok(chosen) => chosen,
        Err(ChoiceError::Cancelled) => return Err(Failure::Cancelled),
        Err(err) => {
            return Err(Failure::Other(format!(
                "rejected the picker's answer: {err:?}"
            )))
        }
    };

    let mut streams: Vec<StreamEntry> = Vec::new();
    for index in chosen {
        let source = sources[index].clone();
        match open_stream(backend, path, session, source).await {
            Ok(entry) => streams.push(entry),
            Err(why) => {
                // Do not leave the ones already made behind.
                close_session(backend, path, false).await;
                return Err(Failure::Other(why));
            }
        }
    }

    // Persistence: when the client asked for it, hand a token back so it can be offered on
    // a later session. The token is not stored across portal restarts yet, a restore-token
    // store would be the next step.
    let (persist_mode, incoming_token) = {
        let inner = lock(&session.inner);
        (inner.persist_mode, inner.restore_token.clone())
    };
    let restore_token = if persist_mode > 0 {
        let token = incoming_token.unwrap_or_else(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("jff-{nanos:x}")
        });
        tracing::info!("Start: issuing restore_token {token:?} (persist_mode {persist_mode})");
        Some(token)
    } else {
        None
    };

    Ok((streams, restore_token))
}

/// Capture one source and put it on a PipeWire stream.
async fn open_stream(
    backend: &Arc<Backend>,
    path: &str,
    session: &Arc<SessionData>,
    source: Source,
) -> Result<StreamEntry, String> {
    let (events_tx, events_rx) = async_channel::unbounded::<CaptureEvent>();
    let wl = backend.wl.clone();
    let wanted = source.clone();
    let started = blocking(move || wl.start(wanted, events_tx))
        .await
        .map_err(|e| format!("could not capture the source: {e:#}"))?;

    let name = format!(
        "jff-portal-{}",
        backend.next_stream.fetch_add(1, Ordering::SeqCst)
    );
    let (width, height, frame, wl_id) = (started.width, started.height, started.frame, started.id);
    jdbg!("dbus open_stream: source={source:?} capture size {width}x{height} wl_id={wl_id}");
    let pw = match blocking(move || stream::start(name, width, height, frame)).await {
        Ok(pw) => pw,
        Err(err) => {
            backend.wl.stop(wl_id);
            return Err(format!("could not make the PipeWire stream: {err:#}"));
        }
    };
    let node_id = pw.node_id;
    jdbg!("dbus open_stream: pipewire node_id={node_id} is what OBS must connect to");
    lock(&session.inner)
        .captures
        .push(ActiveCapture { wl_id, _pw: pw });
    tracing::info!("sharing {source:?} on pipewire node {node_id} ({width}x{height})");

    // When the source goes away (a window closed), the whole session ends.
    let watcher_backend = backend.clone();
    let watcher_path = path.to_string();
    let spawned = std::thread::Builder::new()
        .name("capture-watch".into())
        .spawn(move || {
            if let Ok(CaptureEvent::Stopped(why)) = events_rx.recv_blocking() {
                tracing::info!("{watcher_path}: {why}");
                zbus::block_on(close_session(&watcher_backend, &watcher_path, true));
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("could not watch the capture: {err}");
    }

    let mut props: HashMap<String, OwnedValue> = HashMap::new();
    props.insert("source_type".into(), value(source.source_type()));
    props.insert("size".into(), value((width as i32, height as i32)));
    Ok((node_id, props))
}

/// Open a socket to the PipeWire daemon and hand out its fd. Clients pass the fd to
/// `pw_context_connect_fd`. The socket path is what `pw-cli` and the other portal backends
/// use, `pipewire-rs` does not expose a `pw_core` fd.
fn open_pipewire_socket() -> Result<zbus::zvariant::OwnedFd, String> {
    use std::os::unix::net::UnixStream;

    // The daemon's socket lives under XDG_RUNTIME_DIR. The default name is `pipewire-0`,
    // `PIPEWIRE_REMOTE` overrides it.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_string())?;
    let remote = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".to_string());
    let socket_path = runtime.join(&remote);

    let stream = UnixStream::connect(&socket_path)
        .map_err(|e| format!("connect {}: {e}", socket_path.display()))?;
    stream
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;

    // `OwnedFd` from the stream: `IntoRawFd` transfers ownership, `FromRawFd` takes it.
    let owned: OwnedFd = stream.into();
    tracing::info!(
        "OpenPipeWireRemote: connected to {} ({:?})",
        socket_path.display(),
        owned.as_raw_fd()
    );
    zbus::zvariant::OwnedFd::try_from(owned).map_err(|e| format!("zvariant OwnedFd: {e}"))
}

/// Show the picker `lines` (one per source) and return what it printed. A picker that exits with
/// an error counts as a cancel: most print nothing and exit 1 on Escape.
fn run_picker(chooser: &[String], lines: &str, cancel: &Cancel) -> Result<String, String> {
    let Some((program, args)) = chooser.split_first() else {
        return Err("no picker is configured".into());
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not start the picker `{program}`: {e}"))?;
    cancel.picker.store(child.id() as i32, Ordering::SeqCst);
    if cancel.is_cancelled() {
        cancel.cancel();
    }
    if let Some(mut stdin) = child.stdin.take() {
        // A picker that quit already closes the pipe: not an error here.
        let _ = stdin.write_all(lines.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let output = child.wait_with_output();
    cancel.picker.store(0, Ordering::SeqCst);
    let output = output.map_err(|e| format!("the picker failed: {e}"))?;
    if !output.status.success() {
        return Ok(String::new());
    }
    String::from_utf8(output.stdout).map_err(|_| "the picker printed text that is not UTF-8".into())
}
