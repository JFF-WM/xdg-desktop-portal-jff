//! The Wayland client side: asks jff what can be shared (`ext-foreign-toplevel-list-v1`, the
//! monitors) and reads pictures of a source (`ext-image-copy-capture-v1`) into `wl_shm` buffers.
//! For the portal's metadata cursor mode it also opens a cursor capture session on the same source
//! (`create_pointer_cursor_session`): where the cursor is, its hotspot and its own picture.
//!
//! One thread owns the connection and every Wayland object. The rest of the program talks to it
//! through a `Handle` (commands in, a pipe wakes the thread up) and gets pictures from a shared
//! slot (`FrameSlot`) that holds the latest one. What a capture does next is decided by
//! `capture::Machine`, this file does it.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _, Result};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1::{
    self, ExtImageCopyCaptureCursorSessionV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self, ExtImageCopyCaptureFrameV1, FailureReason,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self, ExtImageCopyCaptureSessionV1,
};

use crate::capture::{Action, Event, FailReason, Machine};
use crate::model::{self, PixelFormat, Source};

/// The latest picture of a capture. The PipeWire side repeats it until a newer one is there.
pub struct SharedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel: PixelFormat,
    pub data: Vec<u8>,
    /// Counts up with every picture.
    pub seq: u64,
}

pub type FrameSlot = Arc<Mutex<Option<SharedFrame>>>;

/// Where the cursor is on a source, as the cursor session says it.
#[derive(Debug, Default, Clone, Copy)]
pub struct CursorPos {
    /// The cursor's picture touches the source (`enter` seen, no `leave` since). Hidden, typing
    /// and a locked session count as off the source.
    pub on_source: bool,
    /// The cursor's hotspot in the source's pixels.
    pub x: i32,
    pub y: i32,
    /// The hotspot's pixel inside the cursor's own picture.
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

/// What a stream needs to publish the cursor: its position and its latest picture (`Argb8888`,
/// premultiplied, `SharedFrame::seq` changes with every new picture).
#[derive(Clone)]
pub struct CursorFeed {
    pub pos: Arc<Mutex<CursorPos>>,
    pub image: FrameSlot,
}

/// What a capture reports back besides pictures.
#[derive(Debug)]
pub enum CaptureEvent {
    /// The capture is over (the window closed, jff dropped the session, an error).
    Stopped(String),
}

/// A capture that is running.
pub struct Started {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub frame: FrameSlot,
    /// Set when the cursor was asked for and the compositor gave a cursor session.
    pub cursor: Option<CursorFeed>,
}

enum Command {
    List(mpsc::Sender<Vec<Source>>),
    Start {
        source: Source,
        /// Also follow the cursor (the portal's metadata cursor mode).
        cursor: bool,
        events: async_channel::Sender<CaptureEvent>,
        reply: mpsc::Sender<Result<Started, String>>,
    },
    Stop(u64),
}

/// How the rest of the program talks to the Wayland thread. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    commands: mpsc::Sender<Command>,
    waker: Arc<OwnedFd>,
}

impl Handle {
    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("the Wayland thread is gone"))?;
        let byte = 1u8;
        // SAFETY: writes one byte from a live local to a pipe we own.
        unsafe { libc::write(self.waker.as_raw_fd(), (&byte as *const u8).cast(), 1) };
        Ok(())
    }

    /// Monitors and windows that can be shared now. Blocks until the Wayland thread answers.
    pub fn list_sources(&self) -> Result<Vec<Source>> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::List(tx))?;
        rx.recv().context("the Wayland thread did not answer")
    }

    /// Start capturing `source`. Blocks until jff has said how big the pictures are.
    pub fn start(
        &self,
        source: Source,
        cursor: bool,
        events: async_channel::Sender<CaptureEvent>,
    ) -> Result<Started> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::Start {
            source,
            cursor,
            events,
            reply: tx,
        })?;
        rx.recv()
            .context("the Wayland thread did not answer")?
            .map_err(|e| anyhow!(e))
    }

    pub fn stop(&self, id: u64) {
        let _ = self.send(Command::Stop(id));
    }
}

/// Connect to jff and start the Wayland thread. Fails (with what is missing) when jff does not
/// offer what a capture needs.
pub fn spawn() -> Result<Handle> {
    let (commands_tx, commands_rx) = mpsc::channel();
    let (waker_read, waker_write) = pipe()?;
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || match connect() {
            Ok((conn, queue, state)) => {
                let _ = ready_tx.send(Ok(()));
                if let Err(err) = run(conn, queue, state, commands_rx, waker_read) {
                    tracing::error!("the Wayland connection ended: {err:#}");
                    std::process::exit(1);
                }
            }
            Err(err) => {
                let _ = ready_tx.send(Err(format!("{err:#}")));
            }
        })
        .context("could not start the Wayland thread")?;
    ready_rx
        .recv()
        .context("the Wayland thread died while starting")?
        .map_err(|e| anyhow!(e))?;
    Ok(Handle {
        commands: commands_tx,
        waker: Arc::new(waker_write),
    })
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe2");
    }
    // SAFETY: pipe2 just made these, nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// ---------------------------------------------------------------------------------------------
// State

struct OutputEntry {
    global: u32,
    proxy: WlOutput,
    name: Option<String>,
}

struct ToplevelEntry {
    proxy: ExtForeignToplevelHandleV1,
    identifier: Option<String>,
    title: String,
    app_id: String,
    pending_title: Option<String>,
    pending_app_id: Option<String>,
    /// `done` was seen: title, app id and identifier are consistent.
    done: bool,
}

struct Capture {
    session: ExtImageCopyCaptureSessionV1,
    machine: Machine,
    buffer: Option<ShmBuffer>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    pixel: PixelFormat,
    slot: FrameSlot,
    seq: u64,
    events: async_channel::Sender<CaptureEvent>,
    /// Waiting for the first constraints to say how big the pictures are.
    reply: Option<mpsc::Sender<Result<Started, String>>>,
    /// The capture that follows this one's cursor (its id), if the cursor was asked for.
    cursor_id: Option<u64>,
    /// On a window or monitor capture: handed to the stream. On a cursor capture: what its
    /// events write to. The same `CursorFeed` (its `image` is the cursor capture's slot).
    feed: Option<CursorFeed>,
    /// Only on a cursor capture: the cursor session whose capture session this is.
    cursor_session: Option<ExtImageCopyCaptureCursorSessionV1>,
}

struct State {
    shm: WlShm,
    copy_manager: ExtImageCopyCaptureManagerV1,
    output_source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    toplevel_source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    /// Not read: kept so the pointer stays alive.
    _seat: Option<WlSeat>,
    /// The seat's pointer, which a cursor session needs. jff accepts only its own seat's.
    pointer: Option<WlPointer>,
    /// Not read: it is kept so the window list stays bound for as long as the program runs.
    _list: ExtForeignToplevelListV1,
    outputs: Vec<OutputEntry>,
    toplevels: Vec<ToplevelEntry>,
    captures: HashMap<u64, Capture>,
    next_id: u64,
}

fn connect() -> Result<(Connection, wayland_client::EventQueue<State>, State)> {
    let conn = Connection::connect_to_env()
        .context("could not connect to the Wayland display (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)
        .context("could not read the list of Wayland globals")?;
    let qh = queue.handle();

    let missing = |what: &str| {
        anyhow!(
            "the compositor does not offer {what}; jff-portal needs jff, and must not run in a \
             sandbox (jff hides these protocols from sandboxed clients)"
        )
    };
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| missing("wl_shm"))?;
    let copy_manager: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| missing("ext-image-copy-capture-v1"))?;
    let list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| missing("ext-foreign-toplevel-list-v1"))?;
    // Only to get a `wl_pointer` for cursor sessions: its capabilities event gives it.
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=1, ()).ok();
    if seat.is_none() {
        tracing::warn!("no wl_seat: the cursor can't be shared");
    }
    let output_source_manager = globals.bind(&qh, 1..=1, ()).ok();
    let toplevel_source_manager = globals.bind(&qh, 1..=1, ()).ok();
    if output_source_manager.is_none() {
        tracing::warn!("no ext-output-image-capture-source-manager-v1: monitors can't be shared");
    }
    if toplevel_source_manager.is_none() {
        tracing::warn!(
            "no ext-foreign-toplevel-image-capture-source-manager-v1: windows can't be shared"
        );
    }

    let mut state = State {
        shm,
        copy_manager,
        output_source_manager,
        toplevel_source_manager,
        _seat: seat,
        pointer: None,
        _list: list,
        outputs: Vec::new(),
        toplevels: Vec::new(),
        captures: HashMap::new(),
        next_id: 1,
    };
    bind_outputs(&globals, &qh, &mut state);
    // The first roundtrip delivers the toplevels and the outputs' names, the second what the
    // toplevel handles say about themselves.
    queue.roundtrip(&mut state).context("Wayland roundtrip")?;
    queue.roundtrip(&mut state).context("Wayland roundtrip")?;
    Ok((conn, queue, state))
}

fn bind_outputs(globals: &GlobalList, qh: &QueueHandle<State>, state: &mut State) {
    let found: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == WlOutput::interface().name)
            .map(|g| (g.name, g.version))
            .collect()
    });
    for (name, version) in found {
        state.add_output(globals.registry(), name, version, qh);
    }
}

impl State {
    fn add_output(
        &mut self,
        registry: &WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<State>,
    ) {
        // `wl_output.name` came with version 4.
        let proxy = registry.bind::<WlOutput, _, _>(global, version.min(4), qh, global);
        self.outputs.push(OutputEntry {
            global,
            proxy,
            name: None,
        });
    }

    fn sources(&self) -> Vec<Source> {
        let mut out: Vec<Source> = self
            .outputs
            .iter()
            .map(|o| Source::Monitor {
                name: o
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("output-{}", o.global)),
            })
            .collect();
        out.extend(self.toplevels.iter().filter(|t| t.done).filter_map(|t| {
            Some(Source::Window {
                identifier: t.identifier.clone()?,
                title: t.title.clone(),
                app_id: t.app_id.clone(),
            })
        }));
        out
    }

    fn start_capture(
        &mut self,
        source: Source,
        with_cursor: bool,
        events: async_channel::Sender<CaptureEvent>,
        reply: mpsc::Sender<Result<Started, String>>,
        qh: &QueueHandle<State>,
    ) {
        let image_source = match &source {
            Source::Monitor { name } => {
                let output = self.outputs.iter().find(|o| {
                    o.name
                        .clone()
                        .unwrap_or_else(|| format!("output-{}", o.global))
                        == *name
                });
                match (&self.output_source_manager, output) {
                    (Some(manager), Some(output)) => {
                        Some(manager.create_source(&output.proxy, qh, ()))
                    }
                    _ => None,
                }
            }
            Source::Window { identifier, .. } => {
                let toplevel = self
                    .toplevels
                    .iter()
                    .find(|t| t.done && t.identifier.as_deref() == Some(identifier));
                match (&self.toplevel_source_manager, toplevel) {
                    (Some(manager), Some(toplevel)) => {
                        Some(manager.create_source(&toplevel.proxy, qh, ()))
                    }
                    _ => None,
                }
            }
        };
        let Some(image_source) = image_source else {
            let _ = reply.send(Err("that source is gone".to_string()));
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        // No `paint_cursors`: the cursor is not in the picture. In metadata mode it comes from a
        // cursor session on the same source.
        let session = self
            .copy_manager
            .create_session(&image_source, Options::empty(), qh, id);
        let mut cursor_id = None;
        let mut feed = None;
        if with_cursor {
            match self.pointer.clone() {
                Some(pointer) => {
                    let cid = self.next_id;
                    self.next_id += 1;
                    let cursor_session = self.copy_manager.create_pointer_cursor_session(
                        &image_source,
                        &pointer,
                        qh,
                        cid,
                    );
                    let cursor_capture = cursor_session.get_capture_session(qh, cid);
                    let image: FrameSlot = Arc::new(Mutex::new(None));
                    let shared = CursorFeed {
                        pos: Arc::new(Mutex::new(CursorPos::default())),
                        image: image.clone(),
                    };
                    // Nobody waits for this capture: if it ends, the stream just has no cursor.
                    let (nobody, _) = async_channel::unbounded();
                    self.captures.insert(
                        cid,
                        Capture {
                            session: cursor_capture,
                            machine: Machine::new(),
                            buffer: None,
                            frame: None,
                            pixel: PixelFormat::Bgra,
                            slot: image,
                            seq: 0,
                            events: nobody,
                            reply: None,
                            cursor_id: None,
                            feed: Some(shared.clone()),
                            cursor_session: Some(cursor_session),
                        },
                    );
                    cursor_id = Some(cid);
                    feed = Some(shared);
                }
                None => tracing::warn!("cursor asked for, but the seat has no pointer"),
            }
        }
        image_source.destroy();
        self.captures.insert(
            id,
            Capture {
                session,
                machine: Machine::new(),
                buffer: None,
                frame: None,
                pixel: PixelFormat::Bgra,
                slot: Arc::new(Mutex::new(None)),
                seq: 0,
                events,
                reply: Some(reply),
                cursor_id,
                feed,
                cursor_session: None,
            },
        );
    }

    fn stop_capture(&mut self, id: u64) {
        if let Some(cap) = self.captures.remove(&id) {
            self.end_with_cursor(cap);
        }
    }

    /// End a capture and the cursor capture that goes with it.
    fn end_with_cursor(&mut self, cap: Capture) {
        if let Some(cursor) = cap.cursor_id.and_then(|cid| self.captures.remove(&cid)) {
            cursor.end();
        }
        cap.end();
    }

    fn session_event(&mut self, id: u64, event: Event, qh: &QueueHandle<State>) {
        let Some(cap) = self.captures.get_mut(&id) else {
            return;
        };
        let noisy = matches!(event, Event::FrameReady);
        if !noisy {
            jdbg!("wayland capture {id}: session event {event:?}");
        }
        let actions = cap.machine.on_event(event);
        if !noisy {
            jdbg!("wayland capture {id}: actions {actions:?}");
        }
        // The first constraints say how big the pictures are: the caller waits for that.
        if cap.reply.is_some() {
            if let Some((width, height)) = cap.machine.size() {
                if let Some(reply) = cap.reply.take() {
                    let _ = reply.send(Ok(Started {
                        id,
                        width,
                        height,
                        frame: cap.slot.clone(),
                        cursor: cap.feed.clone(),
                    }));
                }
            }
        }
        for action in actions {
            match action {
                Action::Capture {
                    width,
                    height,
                    shm_format,
                    pixel,
                    reuse,
                } => {
                    let Some(cap) = self.captures.get_mut(&id) else {
                        return;
                    };
                    if !reuse || cap.buffer.is_none() {
                        match ShmBuffer::new(&self.shm, width, height, shm_format, qh) {
                            Ok(buffer) => cap.buffer = Some(buffer),
                            Err(err) => {
                                self.fail_capture(id, format!("could not make a buffer: {err:#}"));
                                return;
                            }
                        }
                    }
                    if !reuse {
                        jdbg!("wayland capture {id}: new frame request {width}x{height} shm_format={shm_format:#x} pixel={pixel:?} reuse={reuse}");
                    }
                    cap.pixel = pixel;
                    if let Some(old) = cap.frame.take() {
                        old.destroy();
                    }
                    let Some(buffer) = cap.buffer.as_ref() else {
                        return;
                    };
                    let frame = cap.session.create_frame(qh, id);
                    frame.attach_buffer(&buffer.buffer);
                    frame.damage_buffer(0, 0, width as i32, height as i32);
                    frame.capture();
                    cap.frame = Some(frame);
                }
                Action::Deliver => {
                    if let Some(cap) = self.captures.get_mut(&id) {
                        cap.deliver();
                    }
                }
                Action::Stop(why) => {
                    self.fail_capture(id, why);
                    return;
                }
            }
        }
    }

    /// End a capture and tell whoever started it why.
    fn fail_capture(&mut self, id: u64, why: String) {
        let Some(mut cap) = self.captures.remove(&id) else {
            return;
        };
        tracing::info!("capture {id} ended: {why}");
        jdbg!("wayland capture {id} ended: {why}");
        if let Some(reply) = cap.reply.take() {
            let _ = reply.send(Err(why.clone()));
        }
        let _ = cap.events.try_send(CaptureEvent::Stopped(why));
        self.end_with_cursor(cap);
    }
}

impl Capture {
    /// Copy the picture the compositor just wrote into the shared slot.
    fn deliver(&mut self) {
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        // SAFETY: `ptr` maps `len` bytes for as long as `buffer` lives, and the compositor is
        // done writing (the frame is `ready`).
        let src = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
        self.seq += 1;
        if crate::debug_on() && (self.seq <= 5 || self.seq % 60 == 0) {
            jdbg!(
                "wayland deliver seq={} {}x{} stride={} len={} pixel={:?} :: {}",
                self.seq,
                buffer.width,
                buffer.height,
                buffer.stride,
                buffer.len,
                self.pixel,
                model::pixel_stats(
                    src,
                    buffer.stride as usize,
                    buffer.width as usize,
                    buffer.height as usize
                )
            );
        }
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(frame) if frame.data.len() == src.len() => {
                frame.data.copy_from_slice(src);
                frame.width = buffer.width;
                frame.height = buffer.height;
                frame.stride = buffer.stride;
                frame.pixel = self.pixel;
                frame.seq = self.seq;
            }
            _ => {
                *slot = Some(SharedFrame {
                    width: buffer.width,
                    height: buffer.height,
                    stride: buffer.stride,
                    pixel: self.pixel,
                    data: src.to_vec(),
                    seq: self.seq,
                })
            }
        }
    }

    fn end(mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
        if let Some(cursor_session) = self.cursor_session.take() {
            cursor_session.destroy();
            // The cursor is no longer followed: do not leave it drawn where it was.
            if let Some(feed) = &self.feed {
                feed.pos.lock().unwrap_or_else(|e| e.into_inner()).on_source = false;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Shared memory

/// A `wl_shm` buffer backed by a memfd that is also mapped here, to read pictures from.
struct ShmBuffer {
    buffer: WlBuffer,
    ptr: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
}

impl ShmBuffer {
    fn new(
        shm: &WlShm,
        width: u32,
        height: u32,
        format: u32,
        qh: &QueueHandle<State>,
    ) -> Result<Self> {
        if !model::valid_size(width, height) {
            return Err(anyhow!("bad size {width}x{height}"));
        }
        let format = match format {
            model::SHM_ARGB8888 => wl_shm::Format::Argb8888,
            model::SHM_XRGB8888 => wl_shm::Format::Xrgb8888,
            other => return Err(anyhow!("unsupported shm format {other:#x}")),
        };
        let stride = model::stride(width);
        let len = model::frame_len(width, height);
        // SAFETY: plain syscalls, every result is checked, and the fd is owned right away.
        let (fd, ptr) = unsafe {
            let raw = libc::memfd_create(b"jff-portal\0".as_ptr().cast(), libc::MFD_CLOEXEC);
            if raw < 0 {
                return Err(std::io::Error::last_os_error()).context("memfd_create");
            }
            let fd = OwnedFd::from_raw_fd(raw);
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
                return Err(std::io::Error::last_os_error()).context("ftruncate");
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error()).context("mmap");
            }
            (fd, ptr.cast::<u8>())
        };
        jdbg!("wayland shm buffer {width}x{height} format={format:?} stride={stride} len={len}");
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        // The buffer keeps the memory alive on the compositor's side.
        pool.destroy();
        Ok(Self {
            buffer,
            ptr,
            len,
            width,
            height,
            stride,
        })
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        // SAFETY: `ptr`/`len` are what mmap returned in `new`, unmapped once here.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

// ---------------------------------------------------------------------------------------------
// The loop

fn run(
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    mut state: State,
    commands: mpsc::Receiver<Command>,
    waker: OwnedFd,
) -> Result<()> {
    let qh = queue.handle();
    loop {
        queue
            .dispatch_pending(&mut state)
            .context("dispatching Wayland events")?;

        while let Ok(command) = commands.try_recv() {
            match command {
                Command::List(reply) => {
                    let _ = reply.send(state.sources());
                }
                Command::Start {
                    source,
                    cursor,
                    events,
                    reply,
                } => state.start_capture(source, cursor, events, reply, &qh),
                Command::Stop(id) => state.stop_capture(id),
            }
        }

        match conn.flush() {
            Ok(()) => {}
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("flushing Wayland requests"),
        }

        // Events may have arrived while the commands were handled: dispatch them first.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: waker.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfd.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("poll");
        }
        if fds[0].revents != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("reading Wayland events"),
            }
        } else {
            drop(guard);
        }
        if fds[1].revents != 0 {
            let mut sink = [0u8; 64];
            // SAFETY: reads into a local buffer of the given size, from a pipe we own.
            while unsafe { libc::read(waker.as_raw_fd(), sink.as_mut_ptr().cast(), sink.len()) } > 0
            {
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Event handlers

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WlOutput::interface().name => {
                state.add_output(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| o.global != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|o| o.global == *global) {
                entry.name = Some(name);
            }
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(ToplevelEntry {
                proxy: toplevel,
                identifier: None,
                title: String::new(),
                app_id: String::new(),
                pending_title: None,
                pending_app_id: None,
                done: false,
            });
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        let Some(at) = state.toplevels.iter().position(|t| t.proxy == *handle) else {
            return;
        };
        match event {
            Event::Identifier { identifier } => state.toplevels[at].identifier = Some(identifier),
            Event::Title { title } => state.toplevels[at].pending_title = Some(title),
            Event::AppId { app_id } => state.toplevels[at].pending_app_id = Some(app_id),
            Event::Done => {
                let t = &mut state.toplevels[at];
                if let Some(title) = t.pending_title.take() {
                    t.title = title;
                }
                if let Some(app_id) = t.pending_app_id.take() {
                    t.app_id = app_id;
                }
                t.done = true;
            }
            Event::Closed => {
                let t = state.toplevels.remove(at);
                t.proxy.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        id: &u64,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event as E;
        let mapped = match event {
            E::BufferSize { width, height } => Event::BufferSize(width, height),
            E::ShmFormat { format } => Event::ShmFormat(match format {
                WEnum::Value(f) => f as u32,
                WEnum::Unknown(raw) => raw,
            }),
            E::Done => Event::Done,
            E::Stopped => Event::Stopped,
            // dmabuf is not used: jff gives windows through wl_shm only.
            _ => return,
        };
        state.session_event(*id, mapped, qh);
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        id: &u64,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event as E;
        if !matches!(
            event,
            E::Ready | E::Transform { .. } | E::Damage { .. } | E::PresentationTime { .. }
        ) {
            jdbg!("wayland capture {id}: frame event {event:?}");
        }
        let mapped = match event {
            E::Ready => Event::FrameReady,
            E::Failed { reason } => Event::FrameFailed(match reason {
                WEnum::Value(FailureReason::BufferConstraints) => FailReason::BufferConstraints,
                WEnum::Value(FailureReason::Stopped) => FailReason::Stopped,
                _ => FailReason::Unknown,
            }),
            // Transform, damage and presentation time are not needed: every frame is copied whole.
            _ => return,
        };
        // A frame ends with `ready` or `failed`: it is done with.
        frame.destroy();
        if let Some(cap) = state.captures.get_mut(id) {
            if cap.frame.as_ref() == Some(frame) {
                cap.frame = None;
            }
        }
        state.session_event(*id, mapped, qh);
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_pointer = matches!(
                capabilities,
                WEnum::Value(caps) if caps.contains(wl_seat::Capability::Pointer)
            );
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            } else if !has_pointer {
                state.pointer = None;
            }
        }
    }
}

// The pointer is only handed to `create_pointer_cursor_session`: its own events are not read.
impl Dispatch<WlPointer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlPointer,
        _: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The cursor session's own events: where the cursor is on the source. The id is the cursor
/// capture's.
impl Dispatch<ExtImageCopyCaptureCursorSessionV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_cursor_session_v1::Event as E;
        let Some(feed) = state.captures.get(id).and_then(|c| c.feed.as_ref()) else {
            return;
        };
        let mut pos = feed.pos.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            E::Enter => pos.on_source = true,
            E::Leave => pos.on_source = false,
            E::Position { x, y } => {
                pos.x = x;
                pos.y = y;
            }
            E::Hotspot { x, y } => {
                pos.hotspot_x = x;
                pos.hotspot_y = y;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(State: ExtImageCaptureSourceV1);
