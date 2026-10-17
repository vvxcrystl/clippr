//! Application monitor taps. Only links owned by this client are ever removed;
//! playback links, defaults, device properties and volumes are never modified.
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::mpsc,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use clippr_core::config::Audio;
use gstreamer::{self as gst, prelude::*};
use pipewire::{self as pw, properties::properties, proxy::ProxyT, types::ObjectType};

type Props = BTreeMap<String, String>;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Target {
    serial: String,
    voice: bool,
}
pub struct ApplicationAudio {
    stop: pw::channel::Sender<()>,
    monitor: Option<JoinHandle<()>>,
}
impl Drop for ApplicationAudio {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

struct Tap {
    _listener: pw::stream::StreamListener<()>,
    stream: pw::stream::StreamRc,
    source: gstreamer_app::AppSrc,
    pad: gst::Pad,
    mixer: gst::Element,
    pipeline: gst::Pipeline,
}
impl Drop for Tap {
    fn drop(&mut self) {
        let _ = self.source.set_state(gst::State::Null);
        if let Some(src) = self.source.static_pad("src") {
            let _ = src.unlink(&self.pad);
        }
        self.mixer.release_request_pad(&self.pad);
        let _ = self.pipeline.remove(&self.source);
    }
}

impl ApplicationAudio {
    pub fn start(
        pipeline: &gst::Pipeline,
        audio: &Audio,
        generation: u64,
        failure: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::start_remote(pipeline, audio, generation, None, failure)
    }

    fn start_remote(
        pipeline: &gst::Pipeline,
        audio: &Audio,
        generation: u64,
        remote: Option<String>,
        failure: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self> {
        let prefix = format!("clippr-tap-{}-{generation}-", std::process::id());
        let (stop, stop_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let config = audio.clone();
        let pipeline = pipeline.clone();
        let monitor = thread::Builder::new()
            .name("audio-registry".into())
            .spawn(move || {
                if let Err(e) = monitor(config, prefix, pipeline, remote, stop_rx, ready_tx) {
                    failure(format!("application audio: {e:#}"));
                }
            })?;
        ready_rx.recv().context("audio registry startup failed")??;
        Ok(Self {
            stop,
            monitor: Some(monitor),
        })
    }
}

fn add_tap(
    core: &pw::core::CoreRc,
    pipeline: &gst::Pipeline,
    prefix: &str,
    target: &Target,
) -> Result<Tap> {
    use pw::spa;
    let source = gstreamer_app::AppSrc::builder()
        .name(format!("tap_{}", target.serial))
        .is_live(true)
        .format(gst::Format::Time)
        .do_timestamp(true)
        .block(false)
        .max_buffers(8)
        .leaky_type(gstreamer_app::AppLeakyType::Downstream)
        .caps(
            &gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48000i32)
                .field("channels", 2i32)
                .field("channel-mask", gst::Bitmask::new(3))
                .build(),
        )
        .build();
    let mixer = pipeline
        .by_name(if target.voice {
            "voice_mix"
        } else {
            "desktop_mix"
        })
        .context("missing source-track mixer")?;
    let pad = mixer
        .request_pad_simple("sink_%u")
        .context("missing mixer input")?;
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "clippr application capture",
        properties! {
            "node.name" => format!("{prefix}{}", target.serial),
            "node.autoconnect" => "false", "node.dont-reconnect" => "true",
            "node.passive" => "true", "node.dont-move" => "true",
            "media.type" => "Audio", "media.category" => "Capture", "media.role" => "Production",
            "adapter.auto-port-config" => "{ mode = dsp monitor = false position = preserve }",
        },
    )?;
    let output = source.clone();
    // No RT_PROCESS: the callback runs on our main loop. Allocation and the
    // nonblocking appsrc push never run in PipeWire's real-time graph thread.
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else { return };
            if bytes.is_empty() || size == 0 || size > bytes.len() || !size.is_multiple_of(8) {
                return;
            }
            // SPA chunk offsets wrap within maxsize; a chunk can straddle its end.
            let offset = offset % bytes.len();
            let first = size.min(bytes.len() - offset);
            let mut samples = Vec::with_capacity(size);
            samples.extend_from_slice(&bytes[offset..offset + first]);
            samples.extend_from_slice(&bytes[..size - first]);
            let mut packet = gst::Buffer::from_slice(samples);
            if let Some(packet) = packet.get_mut() {
                packet.set_duration(gst::ClockTime::from_nseconds(
                    (size as u64 / 8) * 1_000_000_000 / 48000,
                ));
            }
            let _ = output.push_buffer(packet);
        })
        .register()?;
    let tap = Tap {
        _listener: listener,
        stream,
        source,
        pad,
        mixer,
        pipeline: pipeline.clone(),
    };
    pipeline.add(&tap.source)?;
    tap.source
        .static_pad("src")
        .context("missing tap output")?
        .link(&tap.pad)?;
    tap.source.sync_state_with_parent()?;
    let mut format = spa::param::audio::AudioInfoRaw::new();
    format.set_format(spa::param::audio::AudioFormat::F32LE);
    format.set_rate(48000);
    format.set_channels(2);
    let mut position = [0; 64];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    format.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    };
    let values = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let pod = spa::pod::Pod::from_bytes(&values).context("invalid audio format pod")?;
    tap.stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [pod],
    )?;
    Ok(tap)
}

#[derive(Default)]
struct Graph {
    nodes: BTreeMap<u32, Props>,
    clients: BTreeMap<u32, Props>,
    identified_nodes: BTreeSet<u32>,
    identified_clients: BTreeSet<u32>,
    ports: BTreeMap<u32, Props>,
    playback: BTreeMap<u32, (u32, u32)>,
    default_sink: Option<String>,
    links: BTreeMap<(u32, u32), pw::link::Link>,
    taps: BTreeMap<Target, Tap>,
}
impl Graph {
    fn reconcile(
        &mut self,
        core: &pw::core::CoreRc,
        config: &Audio,
        prefix: &str,
        pipeline: &gst::Pipeline,
    ) -> Result<()> {
        let default_sink = self
            .nodes
            .iter()
            .find(|(_, p)| p.get("node.name") == self.default_sink.as_ref())
            .map(|(&id, _)| id);
        let mut targets = BTreeSet::new();
        for (&id, props) in &self.nodes {
            if props.get("media.class").map(String::as_str) != Some("Stream/Output/Audio") {
                continue;
            }
            // Binary identity is often absent from registry globals (notably
            // WebRTC). Wait for full node/client properties before allowing a
            // stream onto Game, so a newly started voice app cannot leak there.
            if !self.identified_nodes.contains(&id) {
                continue;
            }
            let client_id = number(props, "client.id");
            if client_id.is_some_and(|id| !self.identified_clients.contains(&id)) {
                continue;
            }
            let client = props
                .get("client.id")
                .and_then(|s| s.parse::<u32>().ok())
                .and_then(|id| self.clients.get(&id));
            let identities = [
                "application.name",
                "application.process.binary",
                "node.name",
            ]
            .into_iter()
            .flat_map(|key| {
                props
                    .get(key)
                    .into_iter()
                    .chain(client.and_then(|p| p.get(key)))
            })
            .map(String::as_str);
            let voice = config.is_voice_app(identities);
            let playing_to_default = self
                .playback
                .values()
                .any(|&(out, sink)| out == id && Some(sink) == default_sink);
            if voice || playing_to_default {
                if let Some(serial) = props.get("object.serial") {
                    targets.insert(Target {
                        serial: serial.clone(),
                        voice,
                    });
                }
            }
        }
        self.taps.retain(|target, _| targets.contains(target));
        for target in targets {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.taps.entry(target) {
                let tap = add_tap(core, pipeline, prefix, entry.key())?;
                entry.insert(tap);
            }
        }
        let mut desired = BTreeSet::new();
        for (&input_node, props) in &self.nodes {
            let Some(serial) = props.get("node.name").and_then(|s| s.strip_prefix(prefix)) else {
                continue;
            };
            let Some((&output_node, _)) = self
                .nodes
                .iter()
                .find(|(_, p)| p.get("object.serial").map(String::as_str) == Some(serial))
            else {
                continue;
            };
            for (&input, port) in &self.ports {
                if number(port, "node.id") != Some(input_node)
                    || port.get("port.direction").map(String::as_str) != Some("in")
                {
                    continue;
                }
                for (&output, port_out) in &self.ports {
                    if number(port_out, "node.id") == Some(output_node)
                        && port_out.get("port.direction").map(String::as_str) == Some("out")
                        && (port_out.get("audio.channel") == port.get("audio.channel")
                            || port_out.get("audio.channel").map(String::as_str) == Some("MONO"))
                    {
                        desired.insert((output, input));
                    }
                }
            }
        }
        self.links.retain(|key, _| desired.contains(key));
        for (out, input) in desired {
            if self.links.contains_key(&(out, input)) {
                continue;
            }
            // Request passive links as well as passive capture nodes. Stock
            // PipeWire may derive link passivity itself; no server policy is changed.
            let link = core.create_object::<pw::link::Link>(
                "link-factory",
                &properties! {
                    "link.output.port" => out.to_string(), "link.input.port" => input.to_string(),
                    "link.passive" => "true", "object.linger" => "false",
                },
            )?;
            self.links.insert((out, input), link);
        }
        Ok(())
    }
}
fn number(props: &Props, key: &str) -> Option<u32> {
    props.get(key)?.parse().ok()
}
fn props(dict: &pw::spa::utils::dict::DictRef) -> Props {
    dict.iter().map(|(k, v)| (k.into(), v.into())).collect()
}

type Bindings = BTreeMap<u32, (Box<dyn ProxyT>, Box<dyn pw::proxy::Listener>)>;
fn monitor(
    config: Audio,
    prefix: String,
    pipeline: gst::Pipeline,
    remote: Option<String>,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(remote.map(|remote| properties! { "remote.name" => remote }))?;
    let registry = core.get_registry_rc()?;
    let graph = Rc::new(RefCell::new(Graph::default()));
    let bindings = Rc::new(RefCell::new(Bindings::new()));
    let quit = mainloop.clone();
    let _stop = stop.attach(mainloop.loop_(), move |_| quit.quit());
    let error = Rc::new(RefCell::new(None));
    let core_error = error.clone();
    let quit = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, seq, res, message| {
            // Globals may disappear between their announcement and our bind/link,
            // especially when an adapter replaces ports during format negotiation.
            // The following global_remove event reconciles those stale references.
            if res == -libc::ENOENT || res == -libc::ESTALE {
                tracing::debug!(id, seq, %message, "application audio object disappeared");
                return;
            }
            core_error
                .borrow_mut()
                .get_or_insert_with(|| format!("PipeWire object {id}: {message}"));
            quit.quit();
        })
        .register();
    let update = {
        let graph = graph.clone();
        let core = core.clone();
        let error = error.clone();
        let quit = mainloop.clone();
        Rc::new(move || {
            if let Err(e) = graph
                .borrow_mut()
                .reconcile(&core, &config, &prefix, &pipeline)
            {
                *error.borrow_mut() = Some(e.to_string());
                quit.quit();
            }
        })
    };
    let weak_registry = registry.downgrade();
    let g = graph.clone();
    let b = bindings.clone();
    let changed = update.clone();
    let removed_graph = graph.clone();
    let removed_bindings = bindings.clone();
    let removed_update = update.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(dict) = global.props else { return };
            let id = global.id;
            let p = props(dict);
            let Some(registry) = weak_registry.upgrade() else {
                return;
            };
            match global.type_ {
                ObjectType::Node => {
                    g.borrow_mut().nodes.insert(id, p);
                    if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                        let g = g.clone();
                        let changed = changed.clone();
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(p) = info.props() {
                                    g.borrow_mut().identified_nodes.insert(id);
                                    g.borrow_mut().nodes.entry(id).or_default().extend(props(p));
                                    changed();
                                }
                            })
                            .register();
                        b.borrow_mut()
                            .insert(id, (Box::new(node), Box::new(listener)));
                    }
                }
                ObjectType::Client => {
                    g.borrow_mut().clients.insert(id, p);
                    if let Ok(client) = registry.bind::<pw::client::Client, _>(global) {
                        let g = g.clone();
                        let changed = changed.clone();
                        let listener = client
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(p) = info.props() {
                                    g.borrow_mut().identified_clients.insert(id);
                                    g.borrow_mut()
                                        .clients
                                        .entry(id)
                                        .or_default()
                                        .extend(props(p));
                                    changed();
                                }
                            })
                            .register();
                        b.borrow_mut()
                            .insert(id, (Box::new(client), Box::new(listener)));
                    }
                }
                ObjectType::Port => {
                    g.borrow_mut().ports.insert(id, p);
                }
                ObjectType::Link => {
                    if let (Some(out), Some(input)) = (
                        number(&p, "link.output.node"),
                        number(&p, "link.input.node"),
                    ) {
                        g.borrow_mut().playback.insert(id, (out, input));
                    }
                }
                ObjectType::Metadata
                    if p.get("metadata.name").map(String::as_str) == Some("default") =>
                {
                    if let Ok(meta) = registry.bind::<pw::metadata::Metadata, _>(global) {
                        let g = g.clone();
                        let changed = changed.clone();
                        let listener = meta
                            .add_listener_local()
                            .property(move |subject, key, _, value| {
                                if subject == 0
                                    && (key == Some("default.audio.sink") || key.is_none())
                                {
                                    g.borrow_mut().default_sink = value
                                        .and_then(|v| {
                                            serde_json::from_str::<serde_json::Value>(v).ok()
                                        })
                                        .and_then(|v| v["name"].as_str().map(String::from));
                                    changed();
                                }
                                0
                            })
                            .register();
                        b.borrow_mut()
                            .insert(id, (Box::new(meta), Box::new(listener)));
                    }
                }
                _ => (),
            }
            changed();
        })
        .global_remove(move |id| {
            let mut g = removed_graph.borrow_mut();
            g.nodes.remove(&id);
            g.identified_nodes.remove(&id);
            g.identified_clients.remove(&id);
            g.clients.remove(&id);
            g.ports.remove(&id);
            g.playback.remove(&id);
            drop(g);
            removed_bindings.borrow_mut().remove(&id);
            removed_update();
        })
        .register();
    let _ = ready.send(Ok(()));
    mainloop.run();
    graph.borrow_mut().links.clear();
    graph.borrow_mut().taps.clear();
    if let Some(error) = error.borrow_mut().take() {
        anyhow::bail!(error)
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Pipeline(gst::Pipeline);
    impl Drop for Pipeline {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    #[track_caller]
    fn wait(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "private audio graph timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn dump(root: &std::path::Path) -> serde_json::Value {
        let out = Command::new("pw-dump")
            .env("PIPEWIRE_RUNTIME_DIR", root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn amplitude(samples: &[f32], frequency: f64) -> f64 {
        let samples: Vec<_> = samples
            .iter()
            .step_by(2)
            .rev()
            .take(24000)
            .copied()
            .collect();
        let mut re = 0.0;
        let mut im = 0.0;
        for (i, &s) in samples.iter().enumerate() {
            let a = i as f64 * frequency * std::f64::consts::TAU / 48000.0;
            re += f64::from(s) * a.cos();
            im += f64::from(s) * a.sin();
        }
        re.hypot(im) * 2.0 / samples.len().max(1) as f64
    }

    #[test]
    fn passive_taps_mix_exclude_and_survive_restart() -> Result<()> {
        gst::init()?;
        let root = tempfile::tempdir()?;
        let config = root.path().join("private.conf");
        std::fs::write(
            &config,
            r#"
context.properties = { core.daemon = true core.name = pipewire-0 default.clock.rate = 48000 default.clock.quantum = 1024 }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
context.modules = [
 { name = libpipewire-module-protocol-native }
 { name = libpipewire-module-spa-node-factory }
 { name = libpipewire-module-client-node }
 { name = libpipewire-module-adapter }
 { name = libpipewire-module-link-factory }
 { name = libpipewire-module-metadata }
 { name = libpipewire-module-access }
]
context.objects = [
 { factory = spa-node-factory args = { factory.name = support.node.driver node.name = Dummy-Driver node.group = pipewire.dummy priority.driver = 20000 } }
 { factory = metadata args = { metadata.name = default metadata.values = [ { key = default.audio.sink value = { "name": "test_sink" } } ] } }
 { factory = adapter args = { factory.name = support.null-audio-sink node.name = test_sink media.class = Audio/Sink audio.position = [ FL FR ] adapter.auto-port-config = { mode = dsp monitor = false position = preserve } } }
]
"#,
        )?;
        let _server = Server(
            Command::new("pipewire")
                .arg("-c")
                .arg(config)
                .env("PIPEWIRE_RUNTIME_DIR", root.path())
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        wait(|| root.path().join("pipewire-0").exists());
        let remote = root
            .path()
            .join("pipewire-0")
            .to_string_lossy()
            .into_owned();
        let p = gst::parse::launch("audiomixer name=desktop_mix force-live=true ignore-inactive-pads=true latency=40000000 output-buffer-duration=21333333 ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! appsink name=game sync=false audiotestsrc is-live=true wave=silence samplesperbuffer=1024 ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! desktop_mix. audiomixer name=voice_mix force-live=true ignore-inactive-pads=true latency=40000000 output-buffer-duration=21333333 ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! appsink name=voice sync=false audiotestsrc is-live=true wave=silence samplesperbuffer=1024 ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! voice_mix.")?.downcast::<gst::Pipeline>().unwrap();
        let p = Pipeline(p);
        let game = Arc::new(Mutex::new(Vec::<f32>::new()));
        let voice = Arc::new(Mutex::new(Vec::<f32>::new()));
        for (name, samples) in [("game", game.clone()), ("voice", voice.clone())] {
            p.0.by_name(name)
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap()
                .set_callbacks(
                    gstreamer_app::AppSinkCallbacks::builder()
                        .new_sample(move |s| {
                            let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            if let Some(b) = sample.buffer() {
                                if let Ok(b) = b.map_readable() {
                                    let mut samples = samples.lock().unwrap();
                                    samples.extend(
                                        b.chunks_exact(4)
                                            .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                                    );
                                    let trim = samples.len().saturating_sub(96000);
                                    if trim > 0 {
                                        samples.drain(..trim);
                                    }
                                }
                            }
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
        }
        p.0.set_state(gst::State::Playing)?;
        let errors = Arc::new(Mutex::new(Vec::new()));
        let error = errors.clone();
        let capture = ApplicationAudio::start_remote(
            &p.0,
            &Audio::default(),
            123,
            Some(remote.clone()),
            move |e| error.lock().unwrap().push(e),
        )?;
        let app = |name: &str, frequency: u32, rate: u32, channels: u32| -> Result<Pipeline> {
            let p = gst::parse::launch(&format!("audiotestsrc is-live=true volume=0.05 freq={frequency} ! audio/x-raw,format=F32LE,channels={channels},rate={rate} ! pipewiresink name=pw mode=provide"))?.downcast::<gst::Pipeline>().unwrap();
            let sink = p.by_name("pw").unwrap();
            use std::os::fd::AsRawFd;
            let socket = std::os::unix::net::UnixStream::connect(&remote)?;
            sink.set_property("fd", socket.as_raw_fd());
            sink.set_property(
                "stream-properties",
                gst::Structure::builder("props")
                    .field("node.name", name)
                    .field(
                        "application.name",
                        if frequency == 880 {
                            "WEBRTC VoiceEngine"
                        } else {
                            name
                        },
                    )
                    .field(
                        "application.process.binary",
                        if frequency == 880 {
                            "Discord"
                        } else {
                            "fixture"
                        },
                    )
                    .field("node.autoconnect", false)
                    .field(
                        "adapter.auto-port-config",
                        "{ mode = dsp monitor = false position = preserve }",
                    )
                    .build(),
            );
            p.set_state(gst::State::Playing)?;
            Ok(Pipeline(p))
        };
        let first = app("game1", 440, 44100, 2)?;
        let second = app("game2", 660, 48000, 1)?;
        let discord = app("chat", 880, 48000, 2)?;
        // These are the fixture's ordinary playback links. Recorder links must
        // remain additional passive edges and leave every one of these intact.
        let link_playback = |name: &str| {
            let mut links = Vec::new();
            wait(|| {
                let d = dump(root.path());
                let entries = d.as_array().unwrap();
                let node = entries
                    .iter()
                    .find(|x| x["info"]["props"]["node.name"] == name)
                    .and_then(|x| x["id"].as_u64());
                let sink = entries
                    .iter()
                    .find(|x| x["info"]["props"]["node.name"] == "test_sink")
                    .and_then(|x| x["id"].as_u64());
                if let (Some(node), Some(sink)) = (node, sink) {
                    for x in entries.iter().filter(|x| {
                        x["info"]["props"]["node.id"] == node
                            && x["info"]["props"]["port.direction"] == "out"
                    }) {
                        let ch = x["info"]["props"]["audio.channel"].as_str().unwrap();
                        for y in entries.iter().filter(|y| {
                            y["info"]["props"]["node.id"] == sink
                                && (y["info"]["props"]["audio.channel"] == ch || ch == "MONO")
                        }) {
                            links.push((x["id"].as_u64().unwrap(), y["id"].as_u64().unwrap()));
                        }
                    }
                }
                !links.is_empty()
            });
            for (out, input) in links {
                assert!(Command::new("pw-link")
                    .env("PIPEWIRE_RUNTIME_DIR", root.path())
                    .args([out.to_string(), input.to_string()])
                    .status()
                    .unwrap()
                    .success());
            }
        };
        link_playback("game1");
        link_playback("game2");
        link_playback("chat");
        wait(|| {
            let game = game.lock().unwrap();
            let voice = voice.lock().unwrap();
            amplitude(&game, 440.0) > 0.025
                && amplitude(&game, 660.0) > 0.025
                && amplitude(&voice, 880.0) > 0.025
        });
        assert!(amplitude(&game.lock().unwrap(), 880.0) < 0.002);
        assert!(amplitude(&voice.lock().unwrap(), 440.0) < 0.002);
        let before = dump(root.path());
        let playback = |d: &serde_json::Value| -> BTreeSet<u64> {
            let sink = d
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["info"]["props"]["node.name"] == "test_sink")
                .unwrap()["id"]
                .as_u64()
                .unwrap();
            d.as_array()
                .unwrap()
                .iter()
                .filter(|x| x["info"]["props"]["link.input.node"] == sink)
                .filter_map(|x| x["id"].as_u64())
                .collect()
        };
        let links = playback(&before);
        assert_eq!(links.len(), 6);
        drop(discord);
        wait(|| amplitude(&voice.lock().unwrap(), 880.0) < 0.002);
        let discord = app("chat", 880, 44100, 1)?;
        link_playback("chat");
        wait(|| amplitude(&voice.lock().unwrap(), 880.0) > 0.025);
        // Changing only the private fixture's default metadata changes capture
        // membership; it must neither move nor destroy ordinary playback links.
        let set_default = |name: &str| {
            assert!(Command::new("pw-metadata")
                .env("PIPEWIRE_RUNTIME_DIR", root.path())
                .args([
                    "-n",
                    "default",
                    "0",
                    "default.audio.sink",
                    &format!("{{\"name\":\"{name}\"}}"),
                    "Spa:String:JSON"
                ])
                .stdout(Stdio::null())
                .status()
                .unwrap()
                .success());
        };
        set_default("unavailable_sink");
        wait(|| amplitude(&game.lock().unwrap(), 440.0) < 0.002);
        assert!(amplitude(&voice.lock().unwrap(), 880.0) > 0.025);
        set_default("test_sink");
        wait(|| amplitude(&game.lock().unwrap(), 440.0) > 0.025);
        for _ in 0..4 {
            drop(app("short-lived-chat", 880, 48000, 2)?);
        }
        drop(capture);
        assert!(
            errors.lock().unwrap().is_empty(),
            "{:?}",
            errors.lock().unwrap()
        );
        let after = dump(root.path());
        assert_eq!(playback(&after).len(), 6);
        assert!(after
            .as_array()
            .unwrap()
            .iter()
            .all(|x| !x["info"]["props"]["node.name"]
                .as_str()
                .unwrap_or("")
                .starts_with("clippr-tap-")));
        drop((first, second, discord));
        Ok(())
    }
}
