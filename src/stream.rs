//! The PipeWire side: one `Video/Source` stream per shared source, on a thread of its own.
//!
//! jff only sends a picture when the source changed, so a still window would starve a consumer.
//! The stream therefore keeps the latest picture (`wayland::FrameSlot`) and hands it out again
//! every time the graph asks for a buffer. When the source changes size the stream renegotiates
//! its format with the new size.

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
use crate::wayland::FrameSlot;

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

/// Start a stream that shows `frame`, first at `width` by `height`. Returns once PipeWire has
/// made the node (so the application can connect to its id).
pub fn start(name: String, width: u32, height: u32, frame: FrameSlot) -> Result<PwStream> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(err) = run(name, width, height, frame, ready_tx.clone(), quit_rx) {
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
            if let Some(bytes) = buffers_pod(size.width, size.height) {
                if let Some(pod) = Pod::from_bytes(&bytes) {
                    let r = stream.update_params(&mut [pod]);
                    jdbg!("pw buffers pod sent: {}x{} stride={} size={} bytes={} update_params={:?}", size.width, size.height, model::stride(size.width), model::stride(size.width) * size.height, bytes.len(), r.as_ref().map(|_| "ok"));
                    if let Err(err) = r {
                        tracing::warn!("could not set the buffer parameters: {err}");
                    }
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
            let Some(mut buffer) = stream.dequeue_buffer() else {
                if log { jdbg!("pw process #{}: dequeue_buffer returned None", data.calls); }
                return;
            };
            let datas = buffer.datas_mut();
            let Some(plane) = datas.first_mut() else {
                if log { jdbg!("pw process #{}: buffer has no data planes", data.calls); }
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
                let (Some(frame), Some(dst)) = (slot.as_ref(), plane.data()) else {
                    if log { jdbg!("pw process #{}: no frame or plane.data() is None (buffer not mapped?)", data.calls); }
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
            let chunk = plane.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = dst_stride as i32;
            *chunk.size_mut() = (dst_stride * copied) as u32;
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
