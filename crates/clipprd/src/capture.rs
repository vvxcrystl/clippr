use std::{
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
};

use anyhow::{Context, Result};
use clippr_core::{
    clip::AudioTrackKind,
    config::{Codec, Config, KEYFRAME_INTERVAL_SECS},
};
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
    audio: Option<crate::audio::ApplicationAudio>,
    pub screen: Option<Screen>,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub codec: String,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.audio.take();
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
    let keyframes = fps * KEYFRAME_INTERVAL_SECS;
    let source = if test {
        // Cached color bars keep CPU generation out of the real-time test's
        // budget: the animated ball alone nearly saturates a core at 1080p120.
        format!("videotestsrc name=screen is-live=true pattern=smpte75 ! video/x-raw,format=NV12,width={source_width},height={source_height},framerate={fps}/1 ! ")
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
        let quality = bitrate_config.video.quality_quantizer().map_or_else(
            || "pass=cbr".to_owned(),
            |qp| format!("pass=qual quantizer={qp}"),
        );
        format!("videoconvert ! videoscale ! video/x-raw,format=I420,width={width},height={height} ! x264enc name=encoder bitrate={bitrate} {quality} vbv-buf-capacity=2000 key-int-max={keyframes} bframes=0 speed-preset=superfast tune=zerolatency ! ")
    } else {
        // Ring trimming and MP4 saving require presentation order (DTS == PTS).
        // AV1's future references must also be disabled, not just H.26x B-frames.
        format!("vapostproc name=postproc ! video/x-raw(memory:VAMemory),format=NV12,width={width},height={height} ! {encoder} name=encoder bitrate={bitrate} cpb-size={} target-usage=1 key-int-max={keyframes} {} ! ",
            bitrate * 2,
            if codec == Codec::Av1 { "hierarchical-level=1" } else { "b-frames=0" })
    };
    let mut description = format!("{source}{convert}{parser} ! {caps} ! appsink name=video sync=false enable-last-sample=false max-buffers=8");
    let tracks = config.audio.source_tracks();
    crate::lock(&ring).configure_audio(&tracks);
    let aac = if gst::ElementFactory::find("fdkaacenc").is_some() {
        "fdkaacenc"
    } else {
        "avenc_aac"
    };
    for (i, &kind) in tracks.iter().enumerate() {
        let channels = if kind == AudioTrackKind::Mic { 1 } else { 2 };
        let bitrate = if matches!(kind, AudioTrackKind::Mic | AudioTrackKind::Voice) {
            96_000
        } else {
            config.audio.bitrate_kbps * 1000
        };
        let tail = format!("audioconvert ! audioresample ! audio/x-raw,rate=48000,channels={channels} ! {aac} bitrate={bitrate} ! audio/mpeg,mpegversion=4,stream-format=raw ! appsink name=audio{i} sync=false enable-last-sample=false max-buffers=16");
        if test {
            let frequency = match kind {
                AudioTrackKind::Mic => 660,
                AudioTrackKind::Voice => 880,
                _ => 440,
            };
            description.push_str(&format!(" audiotestsrc is-live=true wave=sine freq={frequency} volume=0.05 ! audio/x-raw,rate=48000,channels={channels} ! {tail}"));
        } else if kind == AudioTrackKind::Desktop || kind == AudioTrackKind::Voice {
            let name = if kind == AudioTrackKind::Desktop {
                "desktop"
            } else {
                "voice"
            };
            // A permanent silent input anchors the timeline even when every app is
            // gone. Dynamic taps inherit this pipeline's clock and base time.
            description.push_str(&format!(" audiomixer name={name}_mix force-live=true ignore-inactive-pads=true latency=40000000 output-buffer-duration=21333333 ! {tail} audiotestsrc is-live=true wave=silence samplesperbuffer=1024 ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! {name}_mix."));
        } else if kind == AudioTrackKind::Mic {
            description.push_str(&format!(
                " pipewiresrc name=mic do-timestamp=true provide-clock=false ! {tail}"
            ));
        } else if config.audio.capture_desktop && config.audio.capture_mic {
            description.push_str(&format!(" audiomixer name=mix output-buffer-duration=21333333 ! {tail} pipewiresrc name=desktop do-timestamp=true ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! mix. pipewiresrc name=mic do-timestamp=true ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! mix."));
        } else {
            let source = if config.audio.capture_desktop {
                "desktop"
            } else {
                "mic"
            };
            description.push_str(&format!(
                " pipewiresrc name={source} do-timestamp=true ! {tail}"
            ));
        }
    }

    tracing::debug!(pipeline = %description, "building capture");
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("capture is not a pipeline"))?;
    let mut capture = Capture {
        audio: None,
        pipeline,
        screen,
        encoder: encoder.into(),
        width,
        height,
        codec: codec_name.into(),
    };
    if !software {
        let encoder = capture
            .pipeline
            .by_name("encoder")
            .context("missing encoder")?;
        // Rate-control modes are driver-specific enum values. Older VA drivers
        // may lack VBR; keep their default rather than breaking capture startup.
        let vbr = encoder
            .find_property("rate-control")
            .and_then(|p| gst::glib::EnumClass::with_type(p.value_type()))
            .is_some_and(|class| class.value_by_nick("vbr").is_some());
        if let Some(qp) = bitrate_config.video.quality_quantizer() {
            if vbr {
                // VBR with a QP floor avoids spending the budget on easy frames.
                // Unlike QVBR on radeonsi, this does not chase a quality target
                // regardless of bitrate. 100% makes bitrate the nominal maximum.
                encoder.set_property_from_str("rate-control", "vbr");
                encoder.set_property("target-percentage", 100u32);
                encoder.set_property("min-qp", qp);
            } else {
                tracing::warn!("VA encoder lacks VBR; using its default rate control");
            }
        }
    }
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
    let expected = (1u8 << (tracks.len() + 1)) - 1;
    let streams = std::iter::once(("video".to_owned(), None))
        .chain((0..tracks.len()).map(|i| (format!("audio{i}"), Some(i))));
    for (name, audio) in streams {
        let video = audio.is_none();
        let Some(sink) = capture.pipeline.by_name(&name) else {
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
                    crate::lock(&ring).push_stream(
                        Packet {
                            buffer,
                            pts,
                            duration,
                            keyframe,
                        },
                        caps,
                        audio,
                    );
                    if !ready.load(Ordering::Relaxed) {
                        let flag = 1 << audio.map_or(0, |i| i + 1);
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
    // PipeWire is variable-rate on a quiet desktop. A GOP limit in frames alone
    // can span several seconds. Request a keyframe from arriving buffer timestamps,
    // so sparse captures still have bounded GOPs, without a timer.
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
                                pts.nseconds().saturating_add(
                                    u64::from(KEYFRAME_INTERVAL_SECS) * 1_000_000_000,
                                ),
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
    if !test && config.audio.separate_tracks && config.audio.capture_desktop {
        let events = events.clone();
        capture.audio = Some(crate::audio::ApplicationAudio::start(
            &capture.pipeline,
            &config.audio,
            generation,
            move |error| {
                let _ = events.send(CaptureEvent::Failed(generation, error, false));
            },
        )?);
    }
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
