#![feature(let_chains)]
#![feature(async_closure)]

mod network;

use std::{error::Error, collections::HashMap};

use network::IP;
use tokio::{net::TcpStream, sync::mpsc::{channel, Sender, Receiver}, io::AsyncWriteExt};
use tokio::io::{AsyncReadExt};
use tracing::{info, debug, error, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(PartialEq)]
#[derive(Clone, Copy)]
pub enum ConsensusEvent {
    Handshake = 1,
    Alive = 2,
    Misbehave = 3,
    GetGoodIps = 4
}
impl ConsensusEvent {
    pub fn to_u8(&self) -> u8 {
        *self as u8
    }

    pub fn from_u8(id: u8) -> Result<Self, String> {
        Ok(match id {
            1 => ConsensusEvent::Handshake,
            2 => ConsensusEvent::Alive,
            3 => ConsensusEvent::Misbehave,
            4 => ConsensusEvent::GetGoodIps,
            _ => return Err(String::from("Unknow event"))
        })
    }
}

#[derive(Debug)]
enum PeerEvent {
    Alive(IP, bool),
    Disconnected(IP),
    Misbehave(IP)
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
            evt = rx.recv() => match evt {
                Some(msg) => match msg {
                        PeerEvent::Alive(ip, outgoing) => net.feedback_peer_alive(&ip, outgoing).await.unwrap(),
                        PeerEvent::Disconnected(ip) => net.feedback_peer_failed(&ip).await.unwrap(),
                        PeerEvent::Misbehave(ip) => net.feedback_peer_banned(&ip).await.unwrap()
                    },
                None => error!("Error blablabalba"),
            },
            evt = net.wait_event() => match evt {
                Ok(msg) => match msg {
                    network::controller::NetworkControllerEvent::CandidateConnection(ip, mut stream, is_outgoing) => {
                        //TODO: Hansdshake
                        let tx2 = tx.clone();
                        tokio::spawn(async move {
                            let mut buff: [u8; 1] = [0; 1];
                            if is_outgoing {
                                buff[0] = ConsensusEvent::to_u8(&ConsensusEvent::Handshake);
                                // '1'.encode_utf8(&mut buff);
                                if let Err(err) = stream.write_all(&buff).await {
                                    error!("[{}] {}", ip, err);
                                    tx2.send(PeerEvent::Disconnected(ip)).await.unwrap();
                                    return;
                                }
                            } else {
                                if let Err(err) = stream.read_exact(&mut buff).await {
                                    error!("[{}] stream closed during hanshake {}", ip, err);
                                    tx2.send(PeerEvent::Disconnected(ip)).await.unwrap();
                                    return;
                                }

                                if let Ok(event) = ConsensusEvent::from_u8(buff[0]) && event == ConsensusEvent::Handshake {
                                } else {
                                    error!("[{}] handshake", ip);
                                    tx2.send(PeerEvent::Disconnected(ip)).await.unwrap();
                                    return;
                                }
                            }

                            // if let Ok(event) = Event::from_u8(buff[0]) && event == Event::Hansdshake {
                                // successfull Hansdshake 
                                tx2.send(PeerEvent::Alive(ip.clone(), is_outgoing)).await.unwrap();
                                loop {
                                    if let Err(err) = stream.read_exact(&mut buff).await {
                                        debug!("[{}] stream closed {}", ip, err);
                                        tx2.send(PeerEvent::Disconnected(ip)).await.unwrap();
                                        break;
                                    }
                                    info!("[{}] receive event {}", ip, buff[0]);
                                    tx2.send(PeerEvent::Alive(ip.clone(), is_outgoing)).await.unwrap();
                                    if let Ok(event) = ConsensusEvent::from_u8(buff[0]) {
                                        match event {
                                            ConsensusEvent::Handshake => error!("{} HANSDSHAKE", ip),
                                            ConsensusEvent::Alive => info!("{} ALIVE", ip),
                                            ConsensusEvent::Misbehave => info!("{} MISBEHAVES", ip),
                                            ConsensusEvent::GetGoodIps => info!("{} GETGOODIPS", ip),
                                        }
                                    } else {
                                        error!("[{}] receive unknow event", ip);
                                        tx2.send(PeerEvent::Misbehave(ip.clone())).await.unwrap();
                                    }
                                }
                            // } else {
                            //     error!("[{}] error handshaking", ip);
                            // }
                        // }
                                    

                        });
                        // Hansdshake failed
                        // net.feedback_peer_failed(&ip).await.unwrap();
                    }
                },
                // Err(e) => return Err(e.into())
                Err(e) => return Err(e.into())
            }
       }
    }
}
