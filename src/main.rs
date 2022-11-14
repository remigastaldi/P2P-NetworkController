#![feature(let_chains)]
#![feature(async_closure)]

mod network;

use std::error::Error;

use network::IP;
use tokio::{net::{TcpStream, tcp::OwnedWriteHalf}, sync::{mpsc::{channel, Sender, Receiver}, oneshot}, io::AsyncWriteExt, time::Duration, signal};
use tokio::io::AsyncReadExt;
use tracing::{info, debug, error, Level};
use tracing_subscriber::FmtSubscriber;
use std::mem;

// The first byte of data is the event, followed by the size of the payload encoded on a u64
const U64SIZE: usize = mem::size_of::<u64>();
const HEADER_SIZE: usize = U64SIZE + 1;

#[derive(PartialEq)]
#[derive(Clone, Copy)]
#[derive(Debug)]
enum ConsensusEvent {
    Handshake = 1,
    Alive = 2,
    Misbehave = 3,
    GetGoodIps = 4,
    GoodIps = 5
}

impl ConsensusEvent {
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(id: u8) -> Result<Self, String> {
        Ok(match id {
            1 => ConsensusEvent::Handshake,
            2 => ConsensusEvent::Alive,
            3 => ConsensusEvent::Misbehave,
            4 => ConsensusEvent::GetGoodIps,
            5 => ConsensusEvent::GoodIps,
            _ => return Err(String::from("Unknow event"))
        })
    }
}

#[derive(Debug)]
enum PeerEvent {
    Alive(IP),
    Disconnected(IP),
    Misbehave(IP),
    GoodIps(Vec<IP>),
    GetGoodIps(oneshot::Sender<Vec<IP>>)
}

// struct WriteWrapper<T = ()>(std::marker::PhantomData<T>);
// impl<T> WriteWrapper<T> {
async fn write_event_to_stream<T>(stream: &mut OwnedWriteHalf, event: ConsensusEvent, payload: Option<T>) -> Result<(), String>
     where T: serde::Serialize 
{
    let mut buff = match payload {
        Some(data) => {
            let s_peers_ips = serde_json::to_string(&data).unwrap();
            let b_peers_ips = s_peers_ips.as_bytes();
            
            let payload_size_buff: [u8; U64SIZE] = (b_peers_ips.len() as u64).to_be_bytes();
            let mut buff = vec![0;  HEADER_SIZE + b_peers_ips.len()];

            buff[1..U64SIZE + 1].copy_from_slice(&payload_size_buff);
            buff[HEADER_SIZE..].copy_from_slice(b_peers_ips);
            buff    
        },
        None => {
            vec![0; HEADER_SIZE]
        }
    };

    buff[0] = ConsensusEvent::to_u8(event);
    if let Err(err) = stream.write_all(&buff).await {
        return Err(err.to_string())
    }
    Ok(())
}

async fn on_peer_consensus_event(write: &mut OwnedWriteHalf, event_chan: &Sender<PeerEvent>, event: ConsensusEvent, payload: Option<Vec<u8>>, ip: &IP) -> Result<(), String> {
    debug!("[{}] {:?}", ip, event);
    match event {
        ConsensusEvent::Handshake => {
            event_chan.send(PeerEvent::Misbehave(ip.clone())).await.unwrap();
            return Err(String::from("HANSDSHAKE already done"))
        },
        ConsensusEvent::Alive => event_chan.send(PeerEvent::Alive(ip.clone())).await.unwrap(),
        ConsensusEvent::Misbehave => event_chan.send(PeerEvent::Misbehave(ip.clone())).await.unwrap(),
        ConsensusEvent::GetGoodIps => {
            let (tx, rx) = oneshot::channel();
            event_chan.send(PeerEvent::GetGoodIps(tx)).await.unwrap();
            let Ok(peers_ip) = rx.await else {
                return Err(format!("[{}] Sender drop", ip))
            };

            if let Err(err) = write_event_to_stream(write, ConsensusEvent::GoodIps, Some(peers_ip)).await {
                event_chan.send(PeerEvent::Disconnected(ip.clone())).await.unwrap();
                return Err(format!("[{}] {}", ip, err))
            }
        }
        ConsensusEvent::GoodIps => {
            let peers_ips: Vec<IP> = serde_json::from_str(&String::from_utf8(payload.unwrap()).unwrap()).unwrap();
            debug!("GoodIps: {:?}", peers_ips);
            event_chan.send(PeerEvent::GoodIps(peers_ips)).await.unwrap();
        }
    }
    Ok(())
}

async fn on_candidate_connection(ip: IP, stream: TcpStream, event_chan: Sender<PeerEvent>, is_outgoing: bool) {
    let (mut read, mut write) = stream.into_split();

    tokio::spawn(async move {
        //TODO: Hansdshake with private / public key
        //Hansdshake
        let mut buff: [u8; U64SIZE + 1] = [0; U64SIZE + 1];
        if is_outgoing {
            if let Err(err) = write_event_to_stream::<()>(&mut write, ConsensusEvent::Handshake, None).await {
                error!("[{}] {}", ip, err);
                event_chan.send(PeerEvent::Disconnected(ip.clone())).await.unwrap();
                return;
            }
        } else {
            if let Err(err) = read.read_exact(&mut buff).await {
                error!("[{}] stream closed during hanshake {}", ip, err);
                event_chan.send(PeerEvent::Disconnected(ip)).await.unwrap();
                return;
            }

            if let Ok(event) = ConsensusEvent::from_u8(buff[0]) && event == ConsensusEvent::Handshake {
                //TODO
            } else {
                error!("[{}] handshake failed", ip);
                event_chan.send(PeerEvent::Disconnected(ip)).await.unwrap();
                return;
            }
        }

        // Handshake successful
        event_chan.send(PeerEvent::Alive(ip.clone())).await.unwrap();

        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(err) = write_event_to_stream::<()>(&mut write, ConsensusEvent::GetGoodIps, None).await {
                        error!("[{}] {}", ip, err);
                        event_chan.send(PeerEvent::Disconnected(ip.clone())).await.unwrap();
                        return;
                    }
                },
// change this, read_exact is not cancelable
                data = read.read_exact(&mut buff) => { 
                    if data.is_err() {
                        debug!("[{}] stream closed", ip);
                        event_chan.send(PeerEvent::Disconnected(ip)).await.unwrap();
                        return;
                    };
                    event_chan.send(PeerEvent::Alive(ip.clone())).await.unwrap();

                    if let Ok(event) = ConsensusEvent::from_u8(buff[0]) {
                        let mut size_buff = [0; U64SIZE];
                        size_buff.copy_from_slice(&buff[1..9]);
                        let payload_size = u64::from_be_bytes(size_buff);

                        let payload = if payload_size > 0 {
                            let mut payload_buff = vec![0; payload_size as usize];
                            if let Err(err) = read.read_exact(&mut payload_buff).await {
                                event_chan.send(PeerEvent::Disconnected(ip.clone())).await.unwrap();
                                error!("[{}] {}", ip, err);
                            }
                            Some(payload_buff)
                        } else {
                            None
                        };

                        if let Err(err) = on_peer_consensus_event(&mut write, &event_chan, event, payload, &ip).await {
                            error!(err);
                            return;
                        }
                    } else {
                        error!("[{}] receive unknow event", ip);
                        event_chan.send(PeerEvent::Misbehave(ip.clone())).await.unwrap();
                    }
                }
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default subscriber failed");

    info!("Starting node");
    // peers_file,
    // listen_port,
    // target_outgoing_connections,
    // max_incoming_connections,
    // max_simultaneous_outgoing_connection_attempts,
    // max_simultaneous_incoming_connection_attempts,
    // max_idle_peers,
    // max_banned_peers,
    // peer_file_dump_interval_seconds
    let mut net = network::controller::NetworkController::new(
       "config/peers.json",
       4242,
       2,
       2,
       2,
       2,
       5,
       8,
       30,
    ).await?;

    let (tx, mut rx): (Sender<PeerEvent>, Receiver<PeerEvent>) = channel(4096);

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                //TODO shutdown properly
                return Ok(());
            },
            evt = rx.recv() => match evt {
                Some(msg) => match msg {
                        PeerEvent::Alive(ip) => net.feedback_peer_alive(&ip).await.unwrap(),
                        PeerEvent::Disconnected(ip) => net.feedback_peer_failed(&ip).await.unwrap(),
                        PeerEvent::Misbehave(ip) => net.feedback_peer_banned(&ip).await.unwrap(),
                        PeerEvent::GoodIps(peers_ip) => net.feedback_peer_list(&peers_ip).await,
                        PeerEvent::GetGoodIps(channel) => channel.send(net.get_good_peer_ips().await).unwrap()
                    },
                None => error!("Error with channel"),
            },
            evt = net.wait_event() => match evt {
                Ok(msg) => match msg {
                    network::controller::NetworkControllerEvent::CandidateConnection(ip, stream, is_outgoing) => {
                        let event_chan = tx.clone();
                        on_candidate_connection(ip, stream, event_chan, is_outgoing).await;
                    }
                },
                Err(e) => return Err(e.into())
            }
       }
    }
}
