use anyhow::bail;
use clap::Parser;
use pipewire::properties::{PropertiesBox, properties};
use pipewire::registry::GlobalObject;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::pod::Pod;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::sys::{SPA_PARAM_EnumFormat, SPA_TYPE_OBJECT_Format};
use pipewire::spa::utils::Direction;
use pipewire::stream::{StreamBox, StreamFlags, StreamState};
use pipewire::types::ObjectType;
use pipewire::{self as pw, spa::pod};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Producer, Split};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;
use std::{process, thread};
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CodecParameters, Decoder};
use symphonia::core::formats::FormatReader;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::sample::SampleFormat;

#[derive(clap::Parser)]
struct Args {
    /// Path to the audio file to be played
    pub file_path: String,

    /// Name of the audio source node to use as the "microphone". If not specified, the default
    /// audio source node will be used.
    #[arg(short, long)]
    audio_source_name: Option<String>,
}

fn main() {
    let args = Args::parse();
    env_logger::init_from_env(env_logger::Env::default());

    pw::init();

    if let Err(err) = run(args) {
        eprintln!("Error: {err}");
        if let Some(source) = err.source() {
            eprintln!("Caused by: {}", source);
        }
        process::exit(1);
    }

    unsafe { pw::deinit() };
}

fn run(args: Args) -> anyhow::Result<()> {
    let (decoder, format_reader) = decode_audio_file(args.file_path)?;

    let pw_conn = PipewireConnection::new()?;

    let codec_params = decoder.codec_params();
    let num_channels = codec_params
        .channels
        .expect("Missing audio channels in codec parameters")
        .bits()
        .count_ones();

    // NOTE: The stream should be created before we collect the objects from the server. Otherwise
    // we will need to do it again to get stream's ports.
    let stream = create_and_connect_stream(&pw_conn, codec_params)?;

    let objects = {
        let objects = Rc::new(RefCell::new(Vec::new()));
        let registry_listener = register_registry_listener(&pw_conn, &objects);

        let mainloop_weak = pw_conn.mainloop.downgrade();
        let _stream_state_listener = stream
            .add_local_listener()
            .state_changed(move |stream, _: &mut (), before, now| {
                log::debug!("Stream state changed from `{before:?}` to `{now:?}`");
                if before == StreamState::Paused && now == StreamState::Streaming {
                    stream.set_active(false).unwrap();
                    log::debug!("Stopping pipewire loop...");
                    mainloop_weak.upgrade().unwrap().quit();
                }
            })
            .register();

        pw_conn.core.sync(0)?;
        pw_conn.mainloop.run();

        // NOTE: Needed for the line below not to panic
        drop(registry_listener);
        Rc::into_inner(objects).unwrap().into_inner()
    };
    log::debug!(
        "Found {} relevant objects on pipewire server",
        objects.len()
    );

    let source_name = match args.audio_source_name {
        Some(name) => name,
        None => {
            let Some(default_metadata) = get_default_metadata(&pw_conn, &objects)? else {
                bail!("Could not get default metadata on pipewire server".to_string());
            };

            let Some(name) = select_audio_source_name(&default_metadata) else {
                bail!("Metadata object does not contain any audio source".to_string());
            };

            name.to_string()
        }
    };
    log::info!("Using audio source node with name `{}`", source_name);

    let Some(source_node) = objects.iter().find(|o| {
        o.type_ == ObjectType::Node
            && o.props
                .as_ref()
                .and_then(|p| p.get("node.name"))
                .is_some_and(|n| n == source_name)
    }) else {
        bail!(
            "Could not find audio source node with name `{}`",
            source_name
        );
    };
    log::debug!("Found audio source node (id: {})", source_node.id);

    let source_adjacent_links: Vec<_> = objects
        .iter()
        .filter(|&o| {
            o.type_ == ObjectType::Link
                && o.props
                    .as_ref()
                    .is_some_and(|p| p.dict().parse("link.output.node") == Some(Ok(source_node.id)))
        })
        .to_owned()
        .collect();

    let node_ids_to_connect_to: HashSet<_> = source_adjacent_links
        .iter()
        .filter_map(|&o| {
            o.props
                .as_ref()
                .and_then(|p| p.dict().parse::<u32>("link.input.node"))
        })
        .flatten()
        .collect();
    log::debug!("Nodes to connect to: {node_ids_to_connect_to:?}");

    let ports: Vec<_> = objects
        .iter()
        .filter(|&o| o.type_ == ObjectType::Port)
        .collect();

    // FIXME: Rewrite this to be more legible
    let connection_targets: Vec<_> = node_ids_to_connect_to
        .iter()
        .map(|&node_id| {
            let ports: Vec<_> = ports
                .iter()
                .filter_map(|&o| {
                    o.props
                        .as_ref()
                        .map(|p| (p.dict().parse::<u32>("node.id"), p.get("port.direction")))
                        .filter(|(id, direction)| {
                            *id == Some(Ok(node_id)) && *direction == Some("in")
                        })
                        .map(|_| {
                            let audio_channel = o
                                .props
                                .as_ref()
                                .and_then(|p| p.get("audio.channel"))
                                .unwrap_or("FL")
                                .to_string();
                            Port {
                                id: o.id,
                                audio_channel,
                            }
                        })
                })
                .collect();
            SinkConnectionTarget { node_id, ports }
        })
        .collect();
    log::debug!("Found following connection targets: {connection_targets:?}");

    let stream_node_id = stream.node_id();
    let stream_ports: Vec<_> = ports
        .iter()
        .filter(|&&o| {
            o.props
                .as_ref()
                .is_some_and(|p| p.dict().parse("node.id") == Some(Ok(stream_node_id)))
        })
        .map(|&o| {
            let audio_channel = o
                .props
                .as_ref()
                .and_then(|p| p.get("audio.channel"))
                .unwrap_or("FL")
                .to_string();
            Port {
                id: o.id,
                audio_channel,
            }
        })
        .collect();
    log::debug!("Stream ports: {stream_ports:?}");

    // FIXME: This can be done more efficiently
    for stream_port in stream_ports {
        for connection_target in connection_targets.iter() {
            for target_port in connection_target.ports.iter() {
                if target_port.audio_channel == stream_port.audio_channel {
                    log::debug!(
                        "Creating a link: output.node={}, output.port={}, input.node={}, input.port={}",
                        stream_node_id,
                        stream_port.id,
                        connection_target.node_id,
                        target_port.id
                    );
                    pw_conn.core.create_object::<pw::link::Link>(
                        "link-factory",
                        &pw::properties::properties! {
                            "link.output.node" => stream_node_id.to_string(),
                            "link.output.port" => stream_port.id.to_string(),
                            "link.input.node" => connection_target.node_id.to_string(),
                            "link.input.port" => target_port.id.to_string(),
                            "object.linger" => "1",
                        },
                    )?;
                }
            }
        }
    }

    let audio_buffer = HeapRb::<f32>::new((1 << 14) * num_channels as usize);
    let (producer, consumer) = audio_buffer.split();
    let _stream_listener =
        register_stream_proccessing_listener(&stream, &pw_conn, consumer, num_channels)?;
    play_audio_into_ring_buffer(producer, decoder, format_reader);
    stream.set_active(true)?;

    pw_conn.core.sync(0)?;
    pw_conn.mainloop.run();

    Ok(())
}

type PwObject = GlobalObject<PropertiesBox>;

fn is_default_metadata_object(object: &PwObject) -> bool {
    object.type_ == ObjectType::Metadata
        && object
            .props
            .as_ref()
            .and_then(|p| p.get("metadata.name"))
            .is_some_and(|n| n == "default")
}

fn play_audio_into_ring_buffer<P>(
    mut producer: P,
    mut decoder: Box<dyn Decoder + 'static>,
    mut format_reader: Box<dyn FormatReader + 'static>,
) where
    P: Producer<Item = f32> + Send + 'static,
{
    thread::spawn(move || {
        let mut sample_buf = None;
        while let Ok(packet) = format_reader.next_packet() {
            let decoded = decoder
                .decode(&packet)
                .expect("Could not decode audio packet");

            if sample_buf.is_none() {
                let spec = *decoded.spec();
                sample_buf = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
            }

            if let Some(ref mut buf) = sample_buf {
                buf.copy_interleaved_ref(decoded);

                let samples = buf.samples();
                let mut current_sample_idx = 0;
                let samples_len = samples.len();

                while current_sample_idx < samples_len {
                    let new_sample_idx =
                        (current_sample_idx + producer.vacant_len()).min(samples_len);
                    producer.push_slice(&samples[current_sample_idx..new_sample_idx]);
                    current_sample_idx = new_sample_idx;
                    if current_sample_idx < samples_len {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
    });
}

fn register_core_done_listener(pw_conn: &PipewireConnection) -> pw::core::Listener {
    let mainloop_weak = pw_conn.mainloop.downgrade();
    pw_conn
        .core
        .add_listener_local()
        .done(move |_, _| {
            if let Some(loop_handle) = mainloop_weak.upgrade() {
                log::debug!("Sync complete. Stopping pipewire loop...");
                loop_handle.quit();
            }
        })
        .register()
}

fn register_stream_proccessing_listener<C>(
    stream: &pw::stream::Stream,
    pw_conn: &PipewireConnection,
    mut consumer: C,
    num_channels: u32,
) -> Result<pw::stream::StreamListener<()>, pw::Error>
where
    C: Consumer<Item = f32> + Send + 'static,
{
    let mainloop_weak = pw_conn.mainloop.downgrade();
    stream
        .add_local_listener()
        .process(move |stream, _: &mut ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                log::error!("Could not dequeue audio sample buffer!");
                return;
            };

            let data = &mut buffer.datas_mut()[0];

            if let Some(slice) = data.data() {
                let slice_len = slice.len();
                let f32_slice = bytemuck::cast_slice_mut(slice);
                let read_samples = consumer.pop_slice(f32_slice);
                if read_samples < f32_slice.len() {
                    f32_slice[read_samples..].fill(0.0);
                }
                if read_samples == 0
                    && let Some(loop_handle) = mainloop_weak.upgrade()
                {
                    log::debug!("Finished playing audio. Stopping pipewire loop...");
                    loop_handle.quit();
                }

                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = (size_of::<f32>() as u32 * num_channels) as i32;
                *chunk.size_mut() = slice_len as u32;
            }
        })
        .register()
}

fn register_registry_listener(
    pw_conn: &PipewireConnection,
    objects: &Rc<RefCell<Vec<PwObject>>>,
) -> pw::registry::Listener {
    let objects_ = objects.clone();
    pw_conn
        .registry
        .add_listener_local()
        .global(move |global| {
            use pw::types::ObjectType as OT;
            if matches!(global.type_, OT::Node | OT::Link | OT::Metadata | OT::Port) {
                objects_.borrow_mut().push(global.to_owned());
            }
        })
        .register()
}

fn audio_info_from_codec_parameters(codec_params: &CodecParameters) -> AudioInfoRaw {
    let mut audio_info = AudioInfoRaw::new();

    let format = match codec_params.sample_format {
        Some(SampleFormat::U8) => AudioFormat::U8,
        Some(SampleFormat::U16) => AudioFormat::U16,
        Some(SampleFormat::U24) => AudioFormat::U24,
        Some(SampleFormat::U32) => AudioFormat::U32,
        Some(SampleFormat::S8) => AudioFormat::S8,
        Some(SampleFormat::S16) => AudioFormat::S16,
        Some(SampleFormat::S24) => AudioFormat::S24,
        Some(SampleFormat::S32) => AudioFormat::S32,
        Some(SampleFormat::F32) => AudioFormat::F32LE,
        Some(SampleFormat::F64) => AudioFormat::F64LE,
        None => AudioFormat::F32LE,
    };
    audio_info.set_format(format);

    audio_info.set_rate(
        codec_params
            .sample_rate
            .expect("Could not determine audio sample rate"),
    );

    let channels = codec_params
        .channels
        .expect("Could not determine audio channels");
    let num_channels = channels.bits().count_ones();
    audio_info.set_channels(num_channels);

    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    for (i, chan) in channels.iter().enumerate() {
        use pw::spa::sys::*;
        position[i] = match chan {
            Channels::FRONT_LEFT => SPA_AUDIO_CHANNEL_FL,
            Channels::FRONT_RIGHT => SPA_AUDIO_CHANNEL_FR,
            Channels::FRONT_CENTRE => SPA_AUDIO_CHANNEL_FC,
            Channels::LFE1 => SPA_AUDIO_CHANNEL_LFE,
            Channels::REAR_LEFT => SPA_AUDIO_CHANNEL_RL,
            Channels::REAR_RIGHT => SPA_AUDIO_CHANNEL_RR,
            Channels::FRONT_LEFT_CENTRE => SPA_AUDIO_CHANNEL_FLC,
            Channels::FRONT_RIGHT_CENTRE => SPA_AUDIO_CHANNEL_FRC,
            Channels::REAR_CENTRE => SPA_AUDIO_CHANNEL_RC,
            Channels::SIDE_LEFT => SPA_AUDIO_CHANNEL_SL,
            Channels::SIDE_RIGHT => SPA_AUDIO_CHANNEL_SR,
            Channels::TOP_CENTRE => SPA_AUDIO_CHANNEL_TC,
            Channels::TOP_FRONT_LEFT => SPA_AUDIO_CHANNEL_TFL,
            Channels::TOP_FRONT_CENTRE => SPA_AUDIO_CHANNEL_TFC,
            Channels::TOP_FRONT_RIGHT => SPA_AUDIO_CHANNEL_TFR,
            Channels::TOP_REAR_LEFT => SPA_AUDIO_CHANNEL_TRL,
            Channels::TOP_REAR_CENTRE => SPA_AUDIO_CHANNEL_TRC,
            Channels::TOP_REAR_RIGHT => SPA_AUDIO_CHANNEL_TRR,
            Channels::REAR_LEFT_CENTRE => SPA_AUDIO_CHANNEL_RLC,
            Channels::REAR_RIGHT_CENTRE => SPA_AUDIO_CHANNEL_RRC,
            Channels::FRONT_LEFT_WIDE => SPA_AUDIO_CHANNEL_FLW,
            Channels::FRONT_RIGHT_WIDE => SPA_AUDIO_CHANNEL_FRW,
            Channels::FRONT_LEFT_HIGH => SPA_AUDIO_CHANNEL_FLH,
            Channels::FRONT_CENTRE_HIGH => SPA_AUDIO_CHANNEL_FCH,
            Channels::FRONT_RIGHT_HIGH => SPA_AUDIO_CHANNEL_FRH,
            Channels::LFE2 => SPA_AUDIO_CHANNEL_LFE2,
            _ => 0,
        }
    }
    audio_info.set_position(position);

    audio_info
}

fn create_and_connect_stream<'a>(
    pw_conn: &'a PipewireConnection,
    codec_params: &CodecParameters,
) -> Result<pw::stream::StreamBox<'a>, pw::Error> {
    let num_channels = codec_params
        .channels
        .expect("Missing audio channels in codec parameters")
        .bits()
        .count_ones();

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::AUDIO_CHANNELS => num_channels.to_string(),
    };
    let stream = StreamBox::new(&pw_conn.core, "hijacker", props).unwrap();

    let stream_flags = StreamFlags::MAP_BUFFERS | StreamFlags::AUTOCONNECT;

    let audio_info = audio_info_from_codec_parameters(codec_params);
    let object = pod::Object {
        type_: SPA_TYPE_OBJECT_Format,
        id: SPA_PARAM_EnumFormat,
        properties: audio_info.into(),
    };
    let values: Vec<u8> =
        PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(object))
            .unwrap()
            .0
            .into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(Direction::Output, None, stream_flags, &mut params)?;

    Ok(stream)
}

#[allow(clippy::type_complexity)]
fn decode_audio_file(
    file_path: impl AsRef<Path>,
) -> Result<(Box<dyn Decoder>, Box<dyn FormatReader>), symphonia::core::errors::Error> {
    let audio_source_file = File::open(&file_path)?;
    let mss = MediaSourceStream::new(Box::new(audio_source_file), Default::default());
    let format_reader = symphonia::default::get_probe()
        .format(
            &Default::default(),
            mss,
            &Default::default(),
            &Default::default(),
        )?
        .format;

    let track = &format_reader.tracks()[0];
    let decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &Default::default())?;
    Ok((decoder, format_reader))
}

struct PipewireConnection {
    pub mainloop: pw::main_loop::MainLoopRc,
    pub core: pw::core::CoreRc,
    pub _context: pw::context::ContextRc,
    pub registry: pw::registry::RegistryRc,
}

impl PipewireConnection {
    fn new() -> Result<Self, pw::Error> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        Ok(Self {
            mainloop,
            core,
            _context: context,
            registry,
        })
    }
}

#[derive(Debug, Default)]
struct DefaultMetadata {
    default_audio_source_name: Option<String>,
    default_configured_audio_source_name: Option<String>,
    default_audio_sink_name: Option<String>,
    default_configured_audio_sink_name: Option<String>,
}

fn get_default_metadata(
    pw_conn: &PipewireConnection,
    objects: &[PwObject],
) -> Result<Option<DefaultMetadata>, pw::Error> {
    let Some(default_metadata_object) = objects.iter().find(|o| is_default_metadata_object(o))
    else {
        return Ok(None);
    };

    let default_metadata = Rc::new(RefCell::new(DefaultMetadata::default()));

    let default_metadata_ = default_metadata.clone();
    let proxy: pw::metadata::Metadata = pw_conn.registry.bind(default_metadata_object)?;
    let metadata_listener = proxy
        .add_listener_local()
        .property(move |_, key, _, value| {
            use serde_json::Value as JsonValue;
            if let (Some(key), Some(value)) = (key, value) {
                let value: JsonValue = serde_json::from_str(value).unwrap();
                let Some(name_value) = value.get("name") else {
                    log::warn!("Could not get name attribute of default metadata entry");
                    return 0;
                };
                let Some(name) = name_value.as_str() else {
                    log::warn!(
                        "Could not interpret name attribute of default metadata entry as string"
                    );
                    return 0;
                };

                let name = name.to_owned();
                let mut proxy = default_metadata_.borrow_mut();

                match key {
                    "default.configured.audio.sink" => {
                        proxy.default_configured_audio_sink_name = Some(name)
                    }
                    "default.configured.audio.source" => {
                        proxy.default_configured_audio_source_name = Some(name)
                    }
                    "default.audio.sink" => proxy.default_audio_sink_name = Some(name),
                    "default.audio.source" => proxy.default_audio_source_name = Some(name),
                    _ => (),
                }
            }
            0
        })
        .register();
    let _core_done_listener = register_core_done_listener(pw_conn);

    pw_conn.core.sync(0)?;
    pw_conn.mainloop.run();

    // NOTE: Needed for the line below not to panic
    drop(metadata_listener);
    Ok(Some(Rc::into_inner(default_metadata).unwrap().into_inner()))
}

fn select_audio_source_name(metadata: &DefaultMetadata) -> Option<&String> {
    metadata
        .default_audio_source_name
        .as_ref()
        .or(metadata.default_configured_audio_source_name.as_ref())
}

#[derive(Debug)]
struct SinkConnectionTarget {
    node_id: u32,
    ports: Vec<Port>,
}

#[derive(Debug)]
struct Port {
    id: u32,
    audio_channel: String,
}
