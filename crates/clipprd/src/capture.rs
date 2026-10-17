use std::{
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
};

use anyhow::{Context, Result};
use clippr_core::config::{Codec, Config};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as app;
use tokio::sync::mpsc;

use crate::{
    portal::Screen,
    ring::{Packet, Ring},
    CaptureEvent,
};

pub struct Capture {
    pub pipeline: gst::Pipeline,
    pub screen: Option<Screen>,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub codec: String,
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        self.screen.take();
    }
}

pub fn build(
    config: &Config,
    screen: Option<Screen>,
    test: bool,
    software: bool,
    ring: Arc<Mutex<Ring>>,
    events: mpsc::UnboundedSender<CaptureEvent>,
    generation: u64,
) -> Result<Capture> {
    let fps = config.video.fps;
    let (source_width, source_height) = screen
        .as_ref()
        .map(|s| (s.width, s.height))
        .unwrap_or((1920, 1080));
    let height = config
        .video
        .resolution
        .height()
        .unwrap_or(source_height)
        .min(source_height)
        & !1;
    let width =
        ((u64::from(source_width) * u64::from(height) / u64::from(source_height)) as u32) & !1;
    let codec = if software {
        Codec::H264
    } else {
        config.video.codec
    };
    let (encoder, parser, caps, codec_name) = match codec {
        Codec::H264 => (
            if software { "x264enc" } else { "vah264enc" },
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=avc,alignment=au",
            "h264",
        ),
        Codec::Hevc => (
            "vah265enc",
            "h265parse config-interval=-1",
            "video/x-h265,stream-format=hvc1,alignment=au",
            "hevc",
        ),
        Codec::Av1 => (
            "vaav1enc",
            "av1parse",
            "video/x-av1,stream-format=obu-stream,alignment=tu",
            "av1",
        ),
    };
    // Software fallback encodes H.264, so use that codec's bitrate scaling as well.
    let mut bitrate_config = config.clone();
    bitrate_config.video.codec = codec;
    let bitrate = bitrate_config.video_bitrate_kbps(height);
    let source = if test {
        format!("videotestsrc name=screen is-live=true pattern=ball ! video/x-raw,width={source_width},height={source_height},framerate={fps}/1 ! ")
    } else {
        let screen = screen.as_ref().context("no portal source")?;
        format!(
            "pipewiresrc name=screen fd={} path={} do-timestamp=true on-disconnect=error keepalive-time={} ! {} ! ",
            screen.fd.as_raw_fd(),
            screen.node,
            (1000 / fps).max(1),
            if software {
                format!("video/x-raw,max-framerate={fps}/1")
            } else {
                format!("video/x-raw(memory:DMABuf),max-framerate={fps}/1")
            }
        )
    };
    let convert = if software {
        format!("videoconvert ! videoscale ! video/x-raw,format=I420,width={width},height={height} ! x264enc name=encoder bitrate={bitrate} key-int-max={fps} bframes=0 speed-preset=superfast tune=zerolatency ! ")
    } else {
        format!("vapostproc name=postproc ! video/x-raw(memory:VAMemory),format=NV12,width={width},height={height} ! {encoder} name=encoder bitrate={bitrate} key-int-max={fps} {} ! ",
            if codec == Codec::Av1 { "" } else { "b-frames=0" })
    };
    let mut description = format!("{source}{convert}{parser} ! {caps} ! appsink name=video sync=false enable-last-sample=false max-buffers=8");
    if config.audio.capture_desktop || config.audio.capture_mic {
        let aac = if gst::ElementFactory::find("fdkaacenc").is_some() {
            "fdkaacenc"
        } else {
            "avenc_aac"
        };
        let audio_tail = format!("audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! {aac} bitrate={} ! audio/mpeg,mpegversion=4,stream-format=raw ! appsink name=audio sync=false enable-last-sample=false max-buffers=16", config.audio.bitrate_kbps * 1000);
        if test {
            description.push_str(&format!(" audiotestsrc is-live=true wave=sine volume=0.05 ! audio/x-raw,rate=48000,channels=2 ! {audio_tail}"));
        } else if config.audio.capture_desktop && config.audio.capture_mic {
            description.push_str(&format!(" audiomixer name=mix output-buffer-duration=21333333 ! {audio_tail} pipewiresrc name=desktop do-timestamp=true ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! mix. pipewiresrc name=mic do-timestamp=true ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! mix."));
        } else {
            let source = if config.audio.capture_desktop {
                "desktop"
            } else {
                "mic"
            };
            description.push_str(&format!(
                " pipewiresrc name={source} do-timestamp=true ! {audio_tail}"
            ));
        }
    }

    tracing::debug!(pipeline = %description, "building capture");
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("capture is not a pipeline"))?;
    let capture = Capture {
        pipeline,
        screen,
        encoder: encoder.into(),
        width,
        height,
        codec: codec_name.into(),
    };
    if let Some(desktop) = capture.pipeline.by_name("desktop") {
        // WirePlumber routes this stream to the default sink's monitor, tracking default
        // sink changes. Ordinary audio input would silently record the microphone.
        desktop.set_property(
            "stream-properties",
            gst::Structure::builder("props")
                .field("stream.capture.sink", true)
                .field("node.dont-reconnect", false)
                .field("node.dont-move", false)
                .field("media.type", "Audio")
                .field("media.category", "Capture")
                .field("media.role", "Production")
                .build(),
        );
    }
    if let Some(mic) = capture.pipeline.by_name("mic") {
        if let Some(device) = config.audio.mic_device.as_deref().filter(|s| !s.is_empty()) {
            mic.set_property("target-object", device);
        }
    }
    let streams_ready = Arc::new(AtomicU8::new(0));
    let reported_ready = Arc::new(AtomicBool::new(false));
    let expected = if config.audio.capture_desktop || config.audio.capture_mic {
        3
    } else {
        1
    };
    for (name, video) in [("video", true), ("audio", false)] {
        let Some(sink) = capture.pipeline.by_name(name) else {
            continue;
        };
        let sink = sink
            .downcast::<app::AppSink>()
            .map_err(|_| anyhow::anyhow!("{name} is not an appsink"))?;
        let ring = ring.clone();
        let events = events.clone();
        let ready = reported_ready.clone();
        let streams_ready = streams_ready.clone();
        sink.set_callbacks(
            app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let Some(pts) = buffer.pts() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let pts = sample
                        .segment()
                        .and_then(|s| s.downcast_ref::<gst::ClockTime>())
                        .and_then(|s| s.to_running_time(pts))
                        .unwrap_or(pts)
                        .nseconds();
                    let duration = buffer.duration().map(|d| d.nseconds()).unwrap_or(if video {
                        1_000_000_000 / u64::from(fps)
                    } else {
                        1_024_000_000_000 / 48_000
                    });
                    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    crate::lock(&ring).push(
                        Packet {
                            buffer,
                            pts,
                            duration,
                            keyframe,
                        },
                        caps,
                        video,
                    );
                    if !ready.load(Ordering::Relaxed) {
                        let flag = if video { 1 } else { 2 };
                        let seen = streams_ready.fetch_or(flag, Ordering::Relaxed) | flag;
                        if seen == expected && !ready.swap(true, Ordering::Relaxed) {
                            tracing::debug!(%caps, "all encoded capture streams ready");
                            let _ = events.send(CaptureEvent::Ready(generation));
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }
    // PipeWire is variable-rate on a quiet desktop. A GOP of `fps` frames alone
    // can span several seconds. Request a keyframe from arriving buffer timestamps,
    // so sparse captures still have one-second seek points, without a timer.
    let encoder_pad = capture
        .pipeline
        .by_name("encoder")
        .context("missing encoder")?
        .static_pad("sink")
        .context("encoder has no sink pad")?;
    let next_keyframe = AtomicU64::new(0);
    encoder_pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |pad, info| {
            match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    if let Some(pts) = buffer.pts() {
                        if pts.nseconds() >= next_keyframe.load(Ordering::Relaxed) {
                            next_keyframe.store(
                                pts.nseconds().saturating_add(1_000_000_000),
                                Ordering::Relaxed,
                            );
                            pad.send_event(
                                gstreamer_video::DownstreamForceKeyUnitEvent::builder()
                                    .all_headers(true)
                                    .build(),
                            );
                        }
                    }
                }
                Some(gst::PadProbeData::Event(event)) => {
                    if let gst::EventView::Caps(caps) = event.view() {
                        tracing::debug!(caps = %caps.caps(), "negotiated encoder input caps");
                    }
                }
                _ => (),
            }
            gst::PadProbeReturn::Ok
        },
    );
    // Log the actual source memory feature once per negotiation. No frame probe/mapping.
    let source_pad = capture
        .pipeline
        .by_name("screen")
        .context("missing screen source")?
        .static_pad("src")
        .context("screen has no source pad")?;
    source_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data {
            if let gst::EventView::Caps(caps) = event.view() {
                tracing::debug!(caps = %caps.caps(), "negotiated screen source caps");
            }
        }
        gst::PadProbeReturn::Ok
    });
    capture
        .pipeline
        .bus()
        .context("capture pipeline has no bus")?
        .set_sync_handler(move |_, message| {
            let failure = match message.view() {
                gst::MessageView::Error(e) => {
                    let source = e.src().map(|s| s.name().to_string()).unwrap_or_default();
                    let fallback = !software
                        && (matches!(source.as_str(), "encoder" | "postproc")
                            || e.debug()
                                .is_some_and(|debug| debug.contains("not-negotiated")));
                    Some((
                        format!(
                            "{}: {} ({:?})",
                            e.src().map(|s| s.path_string()).unwrap_or_default(),
                            e.error(),
                            e.debug()
                        ),
                        fallback,
                    ))
                }
                gst::MessageView::Eos(_) => Some(("capture source disconnected".into(), false)),
                _ => None,
            };
            if let Some((error, fallback)) = failure {
                let _ = events.send(CaptureEvent::Failed(generation, error, fallback));
            }
            // No GLib main loop or unconsumed bus queue.
            gst::BusSyncReply::Drop
        });
    capture
        .pipeline
        .set_state(gst::State::Playing)
        .context("starting capture pipeline")?;
    Ok(capture)
}
