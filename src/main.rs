use std::{
    collections::VecDeque,
    io::{self, BufRead, Read, Write, stdin, stdout},
    net::Ipv4Addr,
    path::PrefixComponent,
    sync::Arc,
    time::Duration,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    Multiaddr, PeerId, Stream, StreamProtocol, autonat, dcutr, identify,
    multiaddr::Protocol,
    noise, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use libp2p_stream as stream;
use ringbuf::{
    HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, RingBuffer, Split},
};
use tokio::sync::{
    Mutex,
    broadcast::{Receiver, Sender, channel},
};
use tracing::{level_filters::LevelFilter, trace};
use tracing_subscriber::EnvFilter;

const VOICE_PROTOCOL: StreamProtocol = StreamProtocol::new("/voice");
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
    let output_device = host.default_output_device().unwrap();
    let input_device = host.default_input_device().unwrap();
    let mut input_config = input_device.default_input_config().unwrap().config();
    let mut output_config = output_device.default_output_config().unwrap().config();

    input_config.sample_rate = cpal::SampleRate::from(48000u32);
    input_config.channels = 1;
    output_config.sample_rate = cpal::SampleRate::from(48000u32);
    output_config.channels = 1;
    let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;

    let (i_audio_tx, i_audio_rx) = channel::<Vec<u8>>(1920);
    let mut i_audio_rx_2 = Arc::new(Mutex::new(i_audio_tx.subscribe()));
    let mut frame_buffer: Vec<i16> = Vec::with_capacity(960 * 2);
    let input_device_stream = input_device.build_input_stream(
        &input_config,
        move |data: &[f32], _| {
            frame_buffer.extend(data.iter().map(|s| (s * i16::MAX as f32) as i16));
            while frame_buffer.len() >= 960 {
                let frame: Vec<i16> = frame_buffer.drain(..960).collect();
                let mut compressed = vec![0u8; 4000];
                if let Ok(len) = encoder.encode(&frame, &mut compressed) {
                    compressed.truncate(len);
                    let _ = i_audio_tx.send(compressed);
                }
            }
        },
        |err| tracing::error!("Audio error: {err}"),
        None,
    )?;
    let (mut o_producer, mut o_consumer) = HeapRb::<i16>::new(48000).split();
    let output_device_stream = output_device.build_output_stream(
        &output_config,
        move |data: &mut [f32], _| {
            for sample in data.iter_mut() {
                if let Some(pcm_sample) = o_consumer.try_pop() {
                    *sample = pcm_sample as f32 / i16::MAX as f32;
                } else {
                    *sample = 0.0;
                }
            }
        },
        |err| tracing::error!("Audio output error: {err}"),
        None,
    )?;
    tokio::spawn(async move {
        while let Some((peer, stream)) = incoming_streams.next().await {
            match receive_samples(stream, &mut o_producer).await {
                Result::Ok(_) => {
                    tracing::info!("Received samples")
                }
                Err(_) => {
                    tracing::error!("Error while receiving samples.");
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
        tokio::spawn(connection_handler(
            peer_id,
            swarm.behaviour().stream.new_control(),
            Arc::new(Mutex::new(i_audio_rx)),
        ));
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
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if !relay_established {
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
                } else {
                    tokio::spawn(connection_handler(
                        peer_id,
                        swarm.behaviour().stream.new_control(),
                        i_audio_rx_2.clone(),
                    ));
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
    i_audio_rx: Arc<Mutex<Receiver<Vec<u8>>>>,
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
    o_producer: &mut HeapProd<i16>,
) -> Result<(), anyhow::Error> {
    let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono)?;
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

        let mut pcm = vec![0i16; 960];
        match decoder.decode(&packet, &mut pcm, false) {
            Ok(_) => {
                for sample in pcm {
                    if o_producer.try_push(sample).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::error!("{e}")
            }
        }
    }
    Ok(())
}
async fn send_samples(
    mut stream: Stream,
    i_audio_rx: Arc<Mutex<Receiver<Vec<u8>>>>,
) -> Result<(), anyhow::Error> {
    let mut i_audio_rx = i_audio_rx.lock().await;
    while let Ok(frame) = i_audio_rx.recv().await {
        if stream
            .write_all(&(frame.len() as u16).to_be_bytes())
            .await
            .is_err()
        {
            break;
        }
        if stream.write_all(&frame).await.is_err() {
            break;
        }
    }
    Ok(())
}
