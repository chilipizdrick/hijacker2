pub mod cli_args;

pub use pipewire;

use anyhow::anyhow;
use pipewire::properties::{PropertiesBox, properties};
use pipewire::registry::GlobalObject;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::pod::Pod;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::sys::{SPA_PARAM_EnumFormat, SPA_TYPE_OBJECT_Format};
use pipewire::spa::utils::Direction;
use pipewire::spa::utils::dict::ParsableValue;
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
use std::thread;
use std::time::Duration;
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CodecParameters, Decoder};
use symphonia::core::formats::FormatReader;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::sample::SampleFormat;

use crate::cli_args::Args;

/// Relied on user to call `pipewire::init()` and `pipewire::deinit()` before and after calling this.
pub fn run(args: Args) -> anyhow::Result<()> {
    let (decoder, format_reader) = decode_audio_file(args.file_path)?;

    let connection = PipewireConnection::new()?;

    let codec_params = decoder.codec_params();
    let num_channels = channel_count(codec_params)
        .ok_or(anyhow!("Missing audio channel data in codec parameters"))?;

    // NOTE: The stream should be created before we collect the objects from the server. Otherwise
    // the stream's ports will not be available, unless we recreate connection.
    let stream = connection.create_and_connect_stream(codec_params)?;

    let objects = connection.get_server_objects(&stream)?;
    log::debug!(
        "Found {} relevant objects on pipewire server",
        objects.len()
    );

    let source_name = match args.audio_source_name {
        Some(name) => name,
        None => {
            let default_metadata = connection
                .find_default_metadata_object(&objects)?
                .ok_or(anyhow!("Could not get default metadata on pipewire server"))?;

            let name = select_audio_source_name(&default_metadata).ok_or(anyhow!(
                "Pipewire metadata object does not contain any default audio source"
            ))?;

            name.to_string()
        }
    };
    log::info!("Using reference audio source node with name `{source_name}`");

    let source_node = find_source_node(&objects, &source_name).ok_or(anyhow!(
        "Could not find audio source node with name `{source_name}`"
    ))?;
    log::debug!("Found audio source node (id: {})", source_node.id);

    let source_adjacent_links = node_adjacent_links(&objects, source_node);
    log::debug!("Found following links connected to source node: {source_adjacent_links:?}");

    let node_ids_to_connect_to: Vec<_> = link_adjacent_input_node_ids(&source_adjacent_links)
        .into_iter()
        .collect();
    log::debug!("Nodes to connect to: {node_ids_to_connect_to:?}");

    let ports: Vec<_> = objects
        .iter()
        .filter(|&o| o.type_ == ObjectType::Port)
        .collect();

    let connection_targets: Vec<_> = connection_targets(&ports, &node_ids_to_connect_to);
    log::debug!("Found following connection targets: {connection_targets:?}");

    let stream_node_id = stream.node_id();

    let stream_ports: Vec<_> = node_ports(&ports, stream_node_id);
    log::debug!("Stream ports: {stream_ports:?}");

    let stream_node = NodeWithPorts::new(stream_node_id, stream_ports);

    connection.create_links_from_source_to_nodes(&stream_node, &connection_targets)?;

    let audio_buffer = HeapRb::<f32>::new(32768 * num_channels as usize);
    let (producer, consumer) = audio_buffer.split();

    let _stream_listener =
        connection.register_stream_proccessing_listener(&stream, consumer, num_channels)?;
    play_audio_into_ring_buffer(producer, decoder, format_reader);
    stream.set_active(true)?;

    connection.sync()?;
    connection.mainloop.run();

    Ok(())
}

type PwObject = GlobalObject<PropertiesBox>;

fn is_default_metadata_object(object: &PwObject) -> bool {
    is_of_type_and_has_str_property_equal_to(
        object,
        ObjectType::Metadata,
        "metadata.name",
        "default",
    )
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

fn audio_info_from_codec_parameters(
    codec_params: &CodecParameters,
) -> anyhow::Result<AudioInfoRaw> {
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
            .ok_or(anyhow!("Audio sample rate not found in codec parameters"))?,
    );

    let channels = codec_params
        .channels
        .ok_or(anyhow!("Audio channel data not found in codec parameters"))?;
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

    Ok(audio_info)
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

fn select_audio_source_name(metadata: &DefaultMetadata) -> Option<&String> {
    metadata
        .default_audio_source_name
        .as_ref()
        .or(metadata.default_configured_audio_source_name.as_ref())
}

#[derive(Debug, Clone)]
struct NodeWithPorts {
    id: u32,
    ports: Vec<Port>,
}

impl NodeWithPorts {
    fn new(id: u32, ports: Vec<Port>) -> Self {
        Self { id, ports }
    }
}

#[derive(Debug, Clone)]
struct Port {
    id: u32,
    audio_channel: String,
}

impl Port {
    fn new(id: u32, audio_channel: String) -> Self {
        Self { id, audio_channel }
    }
}

#[derive(Debug, Clone)]
struct Link {
    output_node: u32,
    output_port: u32,
    input_node: u32,
    input_port: u32,
}

impl Link {
    fn new(output_node: u32, output_port: u32, input_node: u32, input_port: u32) -> Self {
        Self {
            output_node,
            output_port,
            input_node,
            input_port,
        }
    }
}

impl PipewireConnection {
    fn create_link(&self, link: Link) -> Result<pw::link::Link, pw::Error> {
        self.core.create_object(
            "link-factory",
            &properties! {
                "link.output.node" => link.output_node.to_string(),
                "link.output.port" => link.output_port.to_string(),
                "link.input.node" => link.input_node.to_string(),
                "link.input.port" => link.input_port.to_string(),
                "object.linger" => "1",
            },
        )
    }

    fn create_and_connect_stream(
        &self,
        codec_params: &CodecParameters,
    ) -> anyhow::Result<pw::stream::StreamBox<'_>> {
        let num_channels = channel_count(codec_params)
            .ok_or(anyhow!("Missing audio channel data in codec parameters"))?;

        let props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::AUDIO_CHANNELS => num_channels.to_string(),
        };
        let stream = StreamBox::new(&self.core, "hijacker", props)?;

        let stream_flags = StreamFlags::MAP_BUFFERS | StreamFlags::AUTOCONNECT;

        let audio_info = audio_info_from_codec_parameters(codec_params)?;
        let object = pod::Object {
            type_: SPA_TYPE_OBJECT_Format,
            id: SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        };
        let pod_value = pod::Value::Object(object);
        let values: Vec<u8> = PodSerializer::serialize(Cursor::new(Vec::new()), &pod_value)?
            .0
            .into_inner();
        let mut params = [Pod::from_bytes(&values).ok_or(anyhow!("Could not serialize SPA pod"))?];

        stream.connect(Direction::Output, None, stream_flags, &mut params)?;

        Ok(stream)
    }

    fn register_registry_listener(
        &self,
        objects: &Rc<RefCell<Vec<PwObject>>>,
    ) -> pw::registry::Listener {
        let objects_ = objects.clone();
        self.registry
            .add_listener_local()
            .global(move |global| {
                use pw::types::ObjectType as OT;
                if matches!(global.type_, OT::Node | OT::Link | OT::Metadata | OT::Port) {
                    objects_.borrow_mut().push(global.to_owned());
                }
            })
            .register()
    }

    fn find_default_metadata_object(
        &self,
        objects: &[PwObject],
    ) -> Result<Option<DefaultMetadata>, pw::Error> {
        let Some(default_metadata_object) = objects.iter().find(|o| is_default_metadata_object(o))
        else {
            return Ok(None);
        };

        let default_metadata = Rc::new(RefCell::new(DefaultMetadata::default()));

        let default_metadata_ = default_metadata.clone();
        let proxy: pw::metadata::Metadata = self.registry.bind(default_metadata_object)?;
        let metadata_listener = proxy
            .add_listener_local()
            .property(move |_, key, _, value| {
                use serde_json::Value as JsonValue;
                if let (Some(key), Some(value)) = (key, value) {
                    let value: JsonValue = serde_json::from_str(value).unwrap();
                    let Some(name_value) = value.get("name") else {
                        log::warn!("Could not get name attribute of default metadata entry: key: {key}, value: {value}");
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
        let _core_done_listener = register_core_done_listener(self);

        self.core.sync(0)?;
        self.mainloop.run();

        // NOTE: Needed for the line below not to panic
        drop(metadata_listener);
        Ok(Some(Rc::into_inner(default_metadata).unwrap().into_inner()))
    }

    // FIXME: This probably can be done more efficiently, for example by first sorting by audio
    // channel
    fn create_links_from_source_to_nodes(
        &self,
        stream: &NodeWithPorts,
        connection_targets: &[NodeWithPorts],
    ) -> Result<(), pw::Error> {
        for stream_port in &stream.ports {
            for target in connection_targets {
                for target_port in &target.ports {
                    let target_channel = target_port.audio_channel.as_str();
                    let stream_channel = stream_port.audio_channel.as_str();

                    if stream_channel == "MONO"
                        || target_channel == "MONO"
                        || target_channel == stream_channel
                    {
                        let link = Link::new(stream.id, stream_port.id, target.id, target_port.id);

                        log::debug!("Creating a link: {link:?}");
                        self.create_link(link)?;
                    }
                }
            }
        }

        Ok(())
    }

    fn register_stream_proccessing_listener<C>(
        &self,
        stream: &pw::stream::Stream,
        mut consumer: C,
        num_channels: u32,
    ) -> Result<pw::stream::StreamListener<()>, pw::Error>
    where
        C: Consumer<Item = f32> + Send + 'static,
    {
        let mainloop_weak = self.mainloop.downgrade();
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

    fn sync(&self) -> Result<pw::spa::utils::result::AsyncSeq, pw::Error> {
        self.core.sync(0)
    }

    /// Here we need to pass the stream to get its ports since they do not register before the
    /// stream becomes active. Pipewire mainloop will be stopped after the stream is active.
    fn get_server_objects(&self, stream: &pw::stream::Stream) -> Result<Vec<PwObject>, pw::Error> {
        let objects = Rc::new(RefCell::new(Vec::new()));
        let registry_listener = self.register_registry_listener(&objects);

        let mainloop_weak = self.mainloop.downgrade();
        let _stream_state_listener = stream
            .add_local_listener()
            .state_changed(move |stream, _: &mut (), before, now| {
                log::debug!("Stream state changed from `{before:?}` to `{now:?}`");
                if before == StreamState::Paused && now == StreamState::Streaming {
                    stream.set_active(false).unwrap();
                    log::debug!("Sync complete. Stopping pipewire loop...");
                    mainloop_weak.upgrade().unwrap().quit();
                }
            })
            .register();

        self.sync()?;
        self.mainloop.run();

        // NOTE: Needed for the line below not to panic
        drop(registry_listener);
        Ok(Rc::into_inner(objects).unwrap().into_inner())
    }
}

fn channel_count(params: &CodecParameters) -> Option<u32> {
    params.channels.map(|c| c.bits().count_ones())
}

fn find_source_node<'a>(objects: &'a [PwObject], source_name: &str) -> Option<&'a PwObject> {
    objects.iter().find(|o| {
        is_of_type_and_has_str_property_equal_to(o, ObjectType::Node, "node.name", source_name)
    })
}

fn node_adjacent_links<'a>(
    objects: &'a [PwObject],
    source_node: &'a PwObject,
) -> Vec<&'a PwObject> {
    objects
        .iter()
        .filter(|&o| {
            is_of_type_and_has_property_equal_to(
                o,
                ObjectType::Link,
                "link.output.node",
                source_node.id,
            )
        })
        .collect()
}

fn is_of_type_and_has_property_equal_to<T>(
    object: &PwObject,
    type_: ObjectType,
    prop: &str,
    value: T,
) -> bool
where
    T: PartialEq + ParsableValue,
{
    object.type_ == type_
        && object
            .props
            .as_ref()
            .is_some_and(|p| p.dict().parse(prop) == Some(Ok(value)))
}

fn is_of_type_and_has_str_property_equal_to(
    object: &PwObject,
    type_: ObjectType,
    prop: &str,
    value: &str,
) -> bool {
    object.type_ == type_
        && object
            .props
            .as_ref()
            .is_some_and(|p| p.get(prop) == Some(value))
}

fn link_adjacent_input_node_ids(links: &[&PwObject]) -> HashSet<u32> {
    links
        .iter()
        .filter_map(|&o| {
            o.props
                .as_ref()
                .and_then(|p| p.dict().parse::<u32>("link.input.node"))
        })
        .flatten()
        .collect()
}

fn connection_targets(ports: &[&PwObject], node_ids: &[u32]) -> Vec<NodeWithPorts> {
    node_ids
        .iter()
        .map(|&node_id| {
            let ports = node_input_ports_with_audio_channel(ports, node_id);
            NodeWithPorts::new(node_id, ports)
        })
        .collect()
}

fn node_input_ports_with_audio_channel(ports: &[&PwObject], node_id: u32) -> Vec<Port> {
    let mut result = Vec::new();

    for port in ports {
        if let Some(props) = port.props.as_ref() {
            let port_node_id = props.dict().parse::<u32>("node.id");
            let port_direction = props.get("port.direction");
            if port_node_id == Some(Ok(node_id)) && port_direction == Some("in") {
                let audio_channel = props.get("audio.channel").unwrap_or("MONO").to_string();
                result.push(Port::new(port.id, audio_channel));
            }
        }
    }

    result
}

fn node_ports(ports: &[&PwObject], node_id: u32) -> Vec<Port> {
    ports
        .iter()
        .filter(|&o| is_of_type_and_has_property_equal_to(o, ObjectType::Port, "node.id", node_id))
        .map(|&o| {
            let audio_channel = o
                .props
                .as_ref()
                .and_then(|p| p.get("audio.channel"))
                .unwrap_or("MONO")
                .to_string();
            Port::new(o.id, audio_channel)
        })
        .collect()
}
