//! The PipeWire side: one `Video/Source` stream per shared source, on a thread of its own.
//!
//! jff only sends a picture when the source changed, so a still window would starve a consumer.
//! The stream therefore keeps the latest picture (`wayland::FrameSlot`) and hands it out again
//! every time the graph asks for a buffer. When the source changes size the stream renegotiates
//! its format with the new size.
//!
//! In the portal's metadata cursor mode the stream also asks for a `SPA_META_Cursor` block on its
//! buffers and fills it on every cycle: the cursor's position and hotspot (in the picture's
//! pixels) and its bitmap, from `wayland::CursorFeed`. No id means "no cursor on this picture".

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, Value};
use pw::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pw::stream::{StreamBox, StreamFlags, StreamState};

use crate::model::{self, copy_rows};
use crate::wayland::{CursorFeed, FrameSlot};

/// A running stream. Dropping it ends the stream and its thread.
pub struct PwStream {
    /// The node an application connects to.
    pub node_id: u32,
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for PwStream {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start a stream that shows `frame`, first at `width` by `height`, with the cursor as metadata
/// when `cursor` is given. Returns once PipeWire has made the node (so the application can
/// connect to its id).
pub fn start(
    name: String,
    width: u32,
    height: u32,
    frame: FrameSlot,
    cursor: Option<CursorFeed>,
) -> Result<PwStream> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(err) = run(
                name,
                width,
                height,
                frame,
                cursor,
                ready_tx.clone(),
                quit_rx,
            ) {
                let _ = ready_tx.send(Err(format!("{err:#}")));
                tracing::error!("pipewire stream failed: {err:#}");
            }
        })
        .context("could not start the PipeWire thread")?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(node_id)) => Ok(PwStream {
            node_id,
            quit: quit_tx,
            thread: Some(thread),
        }),
        Ok(Err(err)) => Err(anyhow!(err)),
        Err(_) => {
            let _ = quit_tx.send(());
            Err(anyhow!(
                "PipeWire did not make the node in time (is pipewire running?)"
            ))
        }
    }
}

struct Data {
    frame: FrameSlot,
    /// Where the cursor is and what it looks like, in metadata mode.
    cursor: Option<CursorFeed>,
    /// The size the format was negotiated at.
    negotiated: Option<(u32, u32)>,
    /// A new format was offered and not agreed on yet.
    renegotiating: bool,
    /// The agreed format has no alpha channel (BGRx): the unused byte is written as 0xff.
    opaque: bool,
    ready: Option<mpsc::Sender<Result<u32, String>>>,
    /// Debug only: process() calls so far.
    calls: u64,
}

fn run(
    name: String,
    width: u32,
    height: u32,
    frame: FrameSlot,
    cursor: Option<CursorFeed>,
    ready: mpsc::Sender<Result<u32, String>>,
    quit: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pipewire main loop")?;
    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None).context("pipewire context")?;
    let core = context
        .connect_rc(None)
        .context("could not connect to pipewire")?;

    let stream = StreamBox::new(
        &core,
        "jff-portal",
        properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::NODE_NAME => name.as_str(),
            *pw::keys::NODE_DESCRIPTION => "jff screen share",
        },
    )
    .context("pipewire stream")?;

    let data = Data {
        frame,
        cursor,
        negotiated: None,
        renegotiating: false,
        opaque: true,
        ready: Some(ready),
        calls: 0,
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|stream, data, old, new| {
            jdbg!("pw state {old:?} -> {new:?} node_id={}", stream.node_id());
            match new {
            StreamState::Paused | StreamState::Streaming => {
                if let Some(ready) = data.ready.take() {
                    let _ = ready.send(Ok(stream.node_id()));
                }
            }
            StreamState::Error(err) => {
                if let Some(ready) = data.ready.take() {
                    let _ = ready.send(Err(err));
                }
            }
            _ => {}
            }
        })
        .param_changed(|stream, data, id, param| {
            jdbg!("pw param_changed id={id} has_param={}", param.is_some());
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let size = info.size();
            jdbg!("pw negotiated format={:?} size={}x{} framerate={:?} (BGRA keeps source alpha, anything else forced opaque)", info.format(), size.width, size.height, info.framerate());
            data.opaque = info.format() != VideoFormat::BGRA;
            data.negotiated = Some((size.width, size.height));
            data.renegotiating = false;
            // The buffer layout and, in metadata mode, the cursor block the consumer may ask
            // for: both go in one call, a later one replaces the earlier params.
            let buffers_bytes = buffers_pod(size.width, size.height);
            let meta_bytes = if data.cursor.is_some() {
                cursor_meta_pod()
            } else {
                None
            };
            let mut pods: Vec<&Pod> = Vec::new();
            pods.extend(buffers_bytes.as_deref().and_then(Pod::from_bytes));
            pods.extend(meta_bytes.as_deref().and_then(Pod::from_bytes));
            if !pods.is_empty() {
                let r = stream.update_params(&mut pods);
                jdbg!("pw buffers pod sent: {}x{} stride={} size={} params={} cursor_meta={} update_params={:?}", size.width, size.height, model::stride(size.width), model::stride(size.width) * size.height, pods.len(), meta_bytes.is_some(), r.as_ref().map(|_| "ok"));
                if let Err(err) = r {
                    tracing::warn!("could not set the buffer parameters: {err}");
                }
            }
        })
        .process(|stream, data| {
            data.calls += 1;
            let log = crate::debug_on() && (data.calls <= 10 || data.calls % 120 == 0);
            let (frame_w, frame_h, source_has_alpha) = {
                let slot = data.frame.lock().unwrap_or_else(|e| e.into_inner());
                match slot.as_ref() {
                    Some(frame) => (
                        frame.width,
                        frame.height,
                        frame.pixel == model::PixelFormat::Bgra,
                    ),
                    // Nothing captured yet: no buffer to give.
                    None => {
                        if log { jdbg!("pw process #{}: NO FRAME in slot yet (nothing to give, consumer sees no/black picture)", data.calls); }
                        return;
                    }
                }
            };
            if data.negotiated != Some((frame_w, frame_h)) {
                if log { jdbg!("pw process #{}: size mismatch negotiated={:?} frame={}x{} renegotiating={}", data.calls, data.negotiated, frame_w, frame_h, data.renegotiating); }
                // The source changed size: offer the new one, once.
                if !data.renegotiating && data.negotiated.is_some() {
                    if let Some(bytes) = format_pod(frame_w, frame_h) {
                        if let Some(pod) = Pod::from_bytes(&bytes) {
                            data.renegotiating = true;
                            if let Err(err) = stream.update_params(&mut [pod]) {
                                tracing::warn!("could not offer the new size: {err}");
                            }
                        }
                    }
                }
                return;
            }
            // The raw buffer, because the cursor block is in its metas, which the safe
            // `Buffer` does not show. It goes back to the stream when `buffer` is dropped.
            // SAFETY: the pointer comes from this stream's `dequeue_raw_buffer`.
            let Some(buffer) = (unsafe { RawBuffer::dequeue(stream) }) else {
                if log { jdbg!("pw process #{}: dequeue_buffer returned None", data.calls); }
                return;
            };
            if let Some(feed) = &data.cursor {
                fill_cursor(buffer.ptr, feed);
            }
            let Some((plane_data, plane_chunk)) = buffer.first_plane() else {
                if log { jdbg!("pw process #{}: buffer has no mapped data plane", data.calls); }
                return;
            };
            let dst_stride = model::stride(frame_w) as usize;
            let row_bytes = frame_w as usize * 4;
            // A consumer that was given BGRx ignores the fourth byte, but one that blends
            // BGRA uses it: compositors often leave it 0 for outputs and windows, which
            // shows up as a black (fully transparent) picture. Only a BGRA stream fed by a
            // source that really has alpha keeps the source's bytes.
            let force_opaque = data.opaque || !source_has_alpha;
            let copied = {
                let slot = data.frame.lock().unwrap_or_else(|e| e.into_inner());
                let (Some(frame), dst) = (slot.as_ref(), plane_data) else {
                    if log { jdbg!("pw process #{}: no frame in the slot", data.calls); }
                    return;
                };
                let copied = copy_rows(
                    &frame.data,
                    frame.stride as usize,
                    dst,
                    dst_stride,
                    row_bytes,
                    frame_h as usize,
                );
                if force_opaque {
                    model::force_opaque(dst, dst_stride, frame_w as usize, copied);
                }
                if log {
                    jdbg!(
                        "pw process #{}: seq={} src={}x{} src_stride={} src_pixel={:?} dst_len={} dst_stride={} rows_copied={}/{} force_opaque={} (stream opaque={}, source_has_alpha={})\n    SRC :: {}\n    DST :: {}",
                        data.calls, frame.seq, frame_w, frame_h, frame.stride, frame.pixel, dst.len(), dst_stride, copied, frame_h, force_opaque, data.opaque, source_has_alpha,
                        model::pixel_stats(&frame.data, frame.stride as usize, frame_w as usize, frame_h as usize),
                        model::pixel_stats(dst, dst_stride, frame_w as usize, copied)
                    );
                }
                copied
            };
            plane_chunk.offset = 0;
            plane_chunk.stride = dst_stride as i32;
            plane_chunk.size = (dst_stride * copied) as u32;
            if log { jdbg!("pw process #{}: chunk offset=0 stride={} size={}", data.calls, dst_stride, dst_stride * copied); }
        })
        .register()
        .context("pipewire listener")?;

    jdbg!("pw connecting stream {name} initial size {width}x{height}, formats BGRx/BGRA, flags MAP_BUFFERS only (graph allocates the buffers)");
    let bytes = format_pod(width, height).context("could not build the format")?;
    let pod = Pod::from_bytes(&bytes).context("bad format pod")?;
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )
        .context("could not connect the stream")?;

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// The format on offer: BGRx (preferred, always opaque) or BGRA at the source's size, up to 60
/// frames a second.
fn format_pod(width: u32, height: u32) -> Option<Vec<u8>> {
    let object = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 60, denom: 1 }
        ),
    );
    serialize(object)
}

/// A buffer taken from the stream with `dequeue_raw_buffer`, queued again when dropped.
struct RawBuffer<'a> {
    stream: &'a pw::stream::Stream,
    ptr: *mut pw::sys::pw_buffer,
}

impl<'a> RawBuffer<'a> {
    /// # Safety
    /// Only inside the stream's `process` callback, where taking a buffer is allowed.
    unsafe fn dequeue(stream: &'a pw::stream::Stream) -> Option<Self> {
        let ptr = stream.dequeue_raw_buffer();
        (!ptr.is_null()).then_some(Self { stream, ptr })
    }

    /// The first data plane's memory (mapped, `MAP_BUFFERS`) and its chunk, which says what part
    /// of the memory holds the picture.
    fn first_plane(&self) -> Option<(&mut [u8], &mut spa::sys::spa_chunk)> {
        // SAFETY: `ptr` is a live buffer until `self` is dropped, the memory and the chunk are
        // the ones PipeWire allocated for it, and nothing else touches them while it is out.
        unsafe {
            let buffer = (*self.ptr).buffer;
            if buffer.is_null() || (*buffer).n_datas == 0 || (*buffer).datas.is_null() {
                return None;
            }
            let plane = &mut *(*buffer).datas;
            if plane.data.is_null() || plane.chunk.is_null() {
                return None;
            }
            let memory =
                std::slice::from_raw_parts_mut(plane.data.cast::<u8>(), plane.maxsize as usize);
            Some((memory, &mut *plane.chunk))
        }
    }
}

impl Drop for RawBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `dequeue_raw_buffer` of this stream and is queued once.
        unsafe { self.stream.queue_raw_buffer(self.ptr) };
    }
}

/// Size of a `SPA_META_Cursor` block that has room for a `width` by `height` bitmap.
fn cursor_meta_size(width: usize, height: usize) -> i32 {
    (std::mem::size_of::<spa::sys::spa_meta_cursor>()
        + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
        + width * height * 4) as i32
}

/// The cursor block a consumer may ask for: any size from 1x1 up to 1024x1024, 64x64 by default
/// (what the consumers that read cursor metadata ask for themselves).
fn cursor_meta_pod() -> Option<Vec<u8>> {
    let object = Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa::sys::SPA_META_Cursor)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Range {
                        default: cursor_meta_size(64, 64),
                        min: cursor_meta_size(1, 1),
                        max: cursor_meta_size(1024, 1024),
                    },
                ))),
            ),
        ],
    };
    serialize(object)
}

/// Write the cursor into the `SPA_META_Cursor` block of `raw`, if the consumer asked for one: its
/// position (of the hotspot, in the picture's pixels), its hotspot and its bitmap (BGRA,
/// premultiplied). A cursor that is not on the picture is `id` 0. A bitmap that does not fit the
/// block the consumer allocated is left out (position only).
fn fill_cursor(raw: *mut pw::sys::pw_buffer, feed: &CursorFeed) {
    use spa::sys::{spa_meta_bitmap, spa_meta_cursor};
    use std::mem::size_of;

    if raw.is_null() {
        return;
    }
    let pos = *feed.pos.lock().unwrap_or_else(|e| e.into_inner());
    let image = feed.image.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: `raw` is the buffer PipeWire just gave us and `spa_buffer`, its metas and their
    // data stay valid until it is queued again. Every write stays inside the `size` of the
    // block, which is checked first.
    unsafe {
        let buffer = (*raw).buffer;
        if buffer.is_null() || (*buffer).metas.is_null() {
            return;
        }
        let metas = std::slice::from_raw_parts((*buffer).metas, (*buffer).n_metas as usize);
        let Some(meta) = metas
            .iter()
            .find(|m| m.type_ == spa::sys::SPA_META_Cursor && !m.data.is_null())
        else {
            return;
        };
        let size = meta.size as usize;
        if size < size_of::<spa_meta_cursor>() {
            return;
        }
        let cursor = meta.data.cast::<spa_meta_cursor>();
        let shown = image.as_ref().filter(|_| pos.on_source);
        let Some(image) = shown else {
            (*cursor).id = 0;
            (*cursor).bitmap_offset = 0;
            return;
        };
        // A new id with every new picture, never 0.
        (*cursor).id = (image.seq % 0x7fff_ffff) as u32 + 1;
        (*cursor).flags = 0;
        (*cursor).position.x = pos.x;
        (*cursor).position.y = pos.y;
        (*cursor).hotspot.x = pos.hotspot_x;
        (*cursor).hotspot.y = pos.hotspot_y;
        (*cursor).bitmap_offset = 0;

        let (width, height) = (image.width as usize, image.height as usize);
        if width == 0 || height == 0 || size < cursor_meta_size(width, height) as usize {
            return;
        }
        let bitmap_at = size_of::<spa_meta_cursor>();
        let bitmap = meta
            .data
            .cast::<u8>()
            .add(bitmap_at)
            .cast::<spa_meta_bitmap>();
        (*bitmap).format = VideoFormat::BGRA.as_raw();
        (*bitmap).size.width = image.width;
        (*bitmap).size.height = image.height;
        (*bitmap).stride = (width * 4) as i32;
        (*bitmap).offset = size_of::<spa_meta_bitmap>() as u32;
        let pixels = std::slice::from_raw_parts_mut(
            meta.data
                .cast::<u8>()
                .add(bitmap_at + size_of::<spa_meta_bitmap>()),
            width * height * 4,
        );
        copy_rows(
            &image.data,
            image.stride as usize,
            pixels,
            width * 4,
            width * 4,
            height,
        );
        (*cursor).bitmap_offset = bitmap_at as u32;
    }
}

/// What a buffer looks like: one block of `stride * height` bytes in shared memory.
///
/// Do **not** declare `SPA_PARAM_BUFFERS_dataType` here: the buffer memory type is chosen by
/// the consumer, and announcing it from the producer side makes PipeWire 1.6+ reject the
/// negotiation with `-EINVAL` ("Argumento inválido").
fn buffers_pod(width: u32, height: u32) -> Option<Vec<u8>> {
    let stride = model::stride(width);
    let size = stride.checked_mul(height)?;
    let object = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Range {
                        default: 4,
                        min: 1,
                        max: 8,
                    },
                ))),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size as i32)),
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_stride,
                Value::Int(stride as i32),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_align, Value::Int(16)),
            // Deliberately no SPA_PARAM_BUFFERS_dataType: the consumer picks MemFd/MemPtr/DmaBuf.
        ],
    };
    serialize(object)
}

fn serialize(object: Object) -> Option<Vec<u8>> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}
