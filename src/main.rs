use std::{
    io::{self, BufRead, Write, stdin, stdout},
    sync::Arc,
    time::Duration,
};

use audioadapter_buffers::owned::InterleavedOwned;
use cpal::{
    SampleFormat, SupportedStreamConfigRange,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    Multiaddr, PeerId, Stream, StreamProtocol, autonat, identify,
    multiaddr::Protocol,
    noise, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use libp2p_stream as stream;
use ringbuf::{
    HeapProd, HeapRb,
    traits::{Consumer, Producer, Split},
};
use rubato::{Fft, FixedSync, Resampler};
use tokio::sync::{
    Mutex,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

const VOICE_PROTOCOL: StreamProtocol = StreamProtocol::new("/voice");
const OPUS_RATE: u32 = 48000;
const OPUS_FRAME_SAMPLES: usize = 960; // 20ms at 48000Hz

#[derive(NetworkBehaviour)]
struct Behaviour {
    relay_client: relay::client::Behaviour,
    identify: identify::Behaviour,
    stream: stream::Behaviour,
    autonat: autonat::v2::client::Behaviour,
}
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt().init();
    let mut input_relay_address = String::new();
    print!("Enter relay address or enter to use without relay: ");
    stdout().lock().flush()?;
    let _ = stdin().lock().read_line(&mut input_relay_address);
    let mut maybe_relay_address: Option<Multiaddr> = None;
    if let Ok(relay_address) = input_relay_address.trim().parse::<Multiaddr>() {
        maybe_relay_address = Some(relay_address);
    }
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|keypair, relay_behaviour| Behaviour {
            relay_client: relay_behaviour,
            identify: identify::Behaviour::new(identify::Config::new(
                "/voice_chat/0.0.1".to_string(),
                keypair.public(),
            )),
            autonat: autonat::v2::client::Behaviour::new(
                rand::rngs::OsRng,
                autonat::v2::client::Config::default().with_probe_interval(Duration::from_secs(2)),
            ),
            stream: stream::Behaviour::new(),
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_mins(10)))
        .build();

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    let mut incoming_streams = swarm
        .behaviour()
        .stream
        .new_control()
        .accept(VOICE_PROTOCOL)
        .unwrap();

    let host = cpal::default_host();

    // --- Input device selection ---
    let input_devices: Vec<_> = host.input_devices()?.collect();
    let default_input = host.default_input_device();
    println!("\nAvailable input devices:");
    for (i, dev) in input_devices.iter().enumerate() {
        let desc = format!("{:?}", dev.description());
        let is_default = default_input
            .as_ref()
            .and_then(|d| d.id().ok())
            .zip(dev.id().ok())
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        println!(
            "  [{}] {}{}",
            i,
            desc,
            if is_default { " (default)" } else { "" }
        );
    }
    let input_device = select_item("Select input device", &input_devices)?;

    // --- Input config selection (f32 only) ---
    let input_configs: Vec<SupportedStreamConfigRange> = input_device
        .supported_input_configs()?
        .filter(|c| c.sample_format() == SampleFormat::F32)
        .collect();
    println!("\nSupported input configurations:");
    for (i, cfg) in input_configs.iter().enumerate() {
        println!(
            "  [{}] channels={}, sample_rate={}..{}, sample_format={:?}",
            i,
            cfg.channels(),
            cfg.min_sample_rate(),
            cfg.max_sample_rate(),
            cfg.sample_format(),
        );
    }
    let selected_input_cfg = select_item("Select input config", &input_configs)?;
    let input_config = pick_sample_rate(selected_input_cfg)?.config();

    // --- Output device selection ---
    let output_devices: Vec<_> = host.output_devices()?.collect();
    let default_output = host.default_output_device();
    println!("\nAvailable output devices:");
    for (i, dev) in output_devices.iter().enumerate() {
        let desc = format!("{:?}", dev.description());
        let is_default = default_output
            .as_ref()
            .and_then(|d| d.id().ok())
            .zip(dev.id().ok())
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        println!(
            "  [{}] {}{}",
            i,
            desc,
            if is_default { " (default)" } else { "" }
        );
    }
    let output_device = select_item("Select output device", &output_devices)?;

    // --- Output config selection (f32 only) ---
    let output_configs: Vec<SupportedStreamConfigRange> = output_device
        .supported_output_configs()?
        .filter(|c| c.sample_format() == SampleFormat::F32)
        .collect();
    println!("\nSupported output configurations:");
    for (i, cfg) in output_configs.iter().enumerate() {
        println!(
            "  [{}] channels={}, sample_rate={}..{}, sample_format={:?}",
            i,
            cfg.channels(),
            cfg.min_sample_rate(),
            cfg.max_sample_rate(),
            cfg.sample_format(),
        );
    }
    let selected_output_cfg = select_item("Select output config", &output_configs)?;
    let output_config = pick_sample_rate(selected_output_cfg)?.config();

    // --- Input capture: downmix to mono, send raw f32 ---
    let input_rate = input_config.sample_rate;
    let input_channels = input_config.channels as usize;

    let (raw_audio_tx, raw_audio_rx) = unbounded_channel::<Vec<f32>>();
    let input_device_stream = input_device.build_input_stream(
        &input_config,
        move |data: &[f32], _| {
            // Downmix to mono by averaging all channels
            let mono: Vec<f32> = data
                .chunks(input_channels)
                .map(|frame| frame.iter().sum::<f32>() / input_channels as f32)
                .collect();
            let _ = raw_audio_tx.send(mono);
        },
        |err| tracing::error!("Audio error: {err}"),
        None,
    )?;

    // --- Spawn encoding pipeline: resample to 48kHz + Opus encode ---
    let (encoded_tx, encoded_rx) = unbounded_channel::<Vec<u8>>();
    let encoded_rx = Arc::new(Mutex::new(Some(encoded_rx)));
    tokio::task::spawn_blocking(move || {
        input_encode_pipeline(raw_audio_rx, encoded_tx, input_rate);
    });

    // --- Output playback: f32 ring buffer ---
    let output_rate = output_config.sample_rate;
    let output_channels = output_config.channels as usize;
    let ring_buf_size = output_rate as usize * output_channels; // ~1s buffer
    let (mut o_producer, mut o_consumer) = HeapRb::<f32>::new(ring_buf_size).split();
    let output_device_stream = output_device.build_output_stream(
        &output_config,
        move |data: &mut [f32], _| {
            for sample in data.iter_mut() {
                *sample = o_consumer.try_pop().unwrap_or(0.0);
            }
        },
        |err| tracing::error!("Audio output error: {err}"),
        None,
    )?;
    let encoded_rx_for_incoming = encoded_rx.clone();
    let incoming_send_control = swarm.behaviour().stream.new_control();
    tokio::spawn(async move {
        while let Some((peer, stream)) = incoming_streams.next().await {
            tracing::info!(%peer, "Incoming voice stream");

            // If we haven't started sending yet, open a stream back to this peer
            let maybe_rx = encoded_rx_for_incoming.lock().await.take();
            if let Some(rx) = maybe_rx {
                tokio::spawn(connection_handler(peer, incoming_send_control.clone(), rx));
            }

            match receive_samples(stream, &mut o_producer, output_rate, output_channels).await {
                Result::Ok(_) => {
                    tracing::info!("Received samples")
                }
                Err(e) => {
                    tracing::error!("Error while receiving samples: {e}");
                }
            }
        }
    });
    if let Some(relay_address) = maybe_relay_address.as_ref() {
        swarm.dial(relay_address.clone())?;
    }
    print!("Enter an address to dial or press enter to wait for connection: ");
    io::stdout().flush()?;
    let mut input_address = String::new();
    let mut handle = io::stdin().lock();
    handle.read_line(&mut input_address)?;
    let mut maybe_address: Option<Multiaddr> = None;
    if input_address.trim() != "" {
        match input_address.trim().parse::<Multiaddr>() {
            Ok(multiaddr) => {
                maybe_address = Some(multiaddr);
            }
            Err(e) => {
                tracing::error!("{e}")
            }
        }
    }
    tracing::info!("{}", maybe_address.is_some());
    if let Some(address) = maybe_address {
        let Some(Protocol::P2p(peer_id)) = address.iter().last() else {
            anyhow::bail!("Provided address does not end with /p2p");
        };
        swarm.dial(address)?;
        let maybe_rx = encoded_rx.lock().await.take();
        if let Some(rx) = maybe_rx {
            tokio::spawn(connection_handler(
                peer_id,
                swarm.behaviour().stream.new_control(),
                rx,
            ));
        }
    }
    input_device_stream.play()?;
    output_device_stream.play()?;
    let mut relay_established = false;
    loop {
        let event = swarm.next().await.expect("Never terminates.");
        match event {
            libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                let listen_address = address.with_p2p(*swarm.local_peer_id()).unwrap();
                tracing::info!(%listen_address);
            }

            SwarmEvent::ConnectionEstablished { peer_id, .. } if !relay_established => {
                // Once we're connected to the relay, request a circuit reservation
                if let Some(relay_address) = maybe_relay_address.as_ref() {
                    tracing::info!(%peer_id, "Connection established, requesting circuit reservation");
                    // Strip any existing /p2p/ suffix to avoid duplication
                    let base_addr: Multiaddr = relay_address
                        .iter()
                        .filter(|p| !matches!(p, Protocol::P2p(_)))
                        .collect();
                    let relay_circuit_addr = base_addr
                        .with(Protocol::P2p(peer_id))
                        .with(Protocol::P2pCircuit);
                    tracing::info!(%relay_circuit_addr, "Listening on relay circuit");
                    match swarm.listen_on(relay_circuit_addr) {
                        Ok(_) => {
                            relay_established = true;
                        }
                        Err(e) => {
                            tracing::error!("Failed to listen on relay circuit: {e}");
                        }
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                info,
                ..
            })) => {
                tracing::info!(%info.observed_addr)
            }
            event => tracing::info!(?event),
        }
    }
}
async fn connection_handler(
    peer: PeerId,
    mut control: stream::Control,
    i_audio_rx: UnboundedReceiver<Vec<u8>>,
) {
    let stream = match control.open_stream(peer, VOICE_PROTOCOL).await {
        Result::Ok(stream) => stream,
        Err(error @ stream::OpenStreamError::UnsupportedProtocol(_)) => {
            tracing::info!(%peer, %error);
            return;
        }
        Err(error) => {
            tracing::debug!(%peer, %error);
            return;
        }
    };
    if let Err(e) = send_samples(stream, i_audio_rx).await {
        tracing::warn!(%peer, "Voice protocol failed: {e}");
        return;
    }
    tracing::info!(%peer, "Voice complete");
}
async fn receive_samples(
    mut stream: Stream,
    o_producer: &mut HeapProd<f32>,
    output_rate: u32,
    output_channels: usize,
) -> Result<(), anyhow::Error> {
    let mut decoder = opus::Decoder::new(OPUS_RATE, opus::Channels::Mono)?;

    let need_resample = output_rate != OPUS_RATE;
    let mut resampler = if need_resample {
        Some(Fft::<f32>::new(
            OPUS_RATE as usize,
            output_rate as usize,
            OPUS_FRAME_SAMPLES,
            2,
            1,
            FixedSync::Input,
        )?)
    } else {
        None
    };
    let mut resample_in_buf: Vec<f32> = Vec::new();

    loop {
        let mut len_buf = [0u8; 2];
        if stream.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut packet = vec![0u8; len];
        if stream.read_exact(&mut packet).await.is_err() {
            break;
        }

        let mut pcm_i16 = vec![0i16; OPUS_FRAME_SAMPLES];
        let decoded = match decoder.decode(&packet, &mut pcm_i16, false) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Opus decode error: {e}");
                continue;
            }
        };

        // Convert to f32
        let pcm_f32: Vec<f32> = pcm_i16[..decoded]
            .iter()
            .map(|&s| s as f32 / i16::MAX as f32)
            .collect();

        // Resample if needed, then upmix and push
        if let Some(ref mut resampler) = resampler {
            resample_in_buf.extend_from_slice(&pcm_f32);
            let chunk_size = resampler.input_frames_next();
            while resample_in_buf.len() >= chunk_size {
                let chunk: Vec<f32> = resample_in_buf.drain(..chunk_size).collect();
                let input_buf = InterleavedOwned::new_from(chunk, 1, chunk_size).unwrap();
                match resampler.process(&input_buf, 0, None) {
                    Ok(output) => push_upmixed(o_producer, &output.take_data(), output_channels),
                    Err(e) => tracing::error!("Resample error: {e}"),
                }
            }
        } else {
            push_upmixed(o_producer, &pcm_f32, output_channels);
        }
    }
    Ok(())
}
async fn send_samples(
    mut stream: Stream,
    mut i_audio_rx: UnboundedReceiver<Vec<u8>>,
) -> Result<(), anyhow::Error> {
    while let Some(frame) = i_audio_rx.recv().await {
        stream
            .write_all(&(frame.len() as u16).to_be_bytes())
            .await?;
        stream.write_all(&frame).await?;
        stream.flush().await?;
    }
    Ok(())
}

fn input_encode_pipeline(
    mut raw_rx: UnboundedReceiver<Vec<f32>>,
    encoded_tx: UnboundedSender<Vec<u8>>,
    device_rate: u32,
) {
    let mut encoder =
        opus::Encoder::new(OPUS_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();

    let mut resampler = if device_rate != OPUS_RATE {
        Some(
            Fft::<f32>::new(
                device_rate as usize,
                OPUS_RATE as usize,
                1024,
                2,
                1,
                FixedSync::Input,
            )
            .unwrap(),
        )
    } else {
        None
    };

    let mut resample_in_buf: Vec<f32> = Vec::new();
    let mut frame_buf: Vec<f32> = Vec::new();

    while let Some(mono_samples) = raw_rx.blocking_recv() {
        if let Some(ref mut resampler) = resampler {
            resample_in_buf.extend_from_slice(&mono_samples);
            let chunk_size = resampler.input_frames_next();
            while resample_in_buf.len() >= chunk_size {
                let chunk: Vec<f32> = resample_in_buf.drain(..chunk_size).collect();
                let input_buf = InterleavedOwned::new_from(chunk, 1, chunk_size).unwrap();
                if let Ok(output) = resampler.process(&input_buf, 0, None) {
                    frame_buf.extend_from_slice(&output.take_data());
                }
            }
        } else {
            frame_buf.extend_from_slice(&mono_samples);
        }

        while frame_buf.len() >= OPUS_FRAME_SAMPLES {
            let frame: Vec<f32> = frame_buf.drain(..OPUS_FRAME_SAMPLES).collect();
            let frame_i16: Vec<i16> = frame
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .collect();
            let mut compressed = vec![0u8; 4000];
            if let Ok(len) = encoder.encode(&frame_i16, &mut compressed) {
                compressed.truncate(len);
                let _ = encoded_tx.send(compressed);
            }
        }
    }
}

fn push_upmixed(producer: &mut HeapProd<f32>, mono: &[f32], channels: usize) {
    for &sample in mono {
        for _ in 0..channels {
            let _ = producer.try_push(sample);
        }
    }
}

fn select_item<'a, T>(prompt: &str, items: &'a [T]) -> Result<&'a T, anyhow::Error> {
    if items.is_empty() {
        anyhow::bail!("No items available to select");
    }
    loop {
        print!("{} [0-{}]: ", prompt, items.len() - 1);
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().lock().read_line(&mut buf)?;
        match buf.trim().parse::<usize>() {
            Ok(i) if i < items.len() => return Ok(&items[i]),
            _ => println!("Invalid selection, try again."),
        }
    }
}

fn pick_sample_rate(
    cfg: &cpal::SupportedStreamConfigRange,
) -> Result<cpal::SupportedStreamConfig, anyhow::Error> {
    let min = cfg.min_sample_rate();
    let max = cfg.max_sample_rate();
    if min == max {
        return Ok((*cfg).with_sample_rate(min));
    }
    // Prefer common Opus-compatible rates
    let preferred: &[u32] = &[48000, 24000, 16000, 12000, 8000];
    for &rate in preferred {
        if rate >= min && rate <= max {
            println!("Auto-selected sample rate: {rate}");
            return Ok((*cfg).with_sample_rate(rate));
        }
    }
    loop {
        print!("Enter sample rate ({min}-{max}): ");
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().lock().read_line(&mut buf)?;
        match buf.trim().parse::<u32>() {
            Ok(r) if r >= min && r <= max => return Ok((*cfg).with_sample_rate(r)),
            _ => println!("Invalid sample rate, try again."),
        }
    }
}
