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
pub enum Event {
    Handshake = 1,
    Alive = 2,
    Misbehaves = 3,
    GetGoodIps = 4
}
impl Event {
    pub fn to_u8(&self) -> u8 {
        *self as u8
    }

    pub fn from_u8(id: u8) -> Result<Self, String> {
        Ok(match id {
            1 => Event::Handshake,
            2 => Event::Alive,
            3 => Event::Misbehaves,
            4 => Event::GetGoodIps,
            _ => return Err(String::from("Unknow event"))
        })
    }
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
       10,
       5,
       3,
       30,
    ).await?;

    let chan: (Sender<Event>, Receiver<Event>) = channel(4096);

    loop {
        tokio::select! {
            evt = net.wait_event() => match evt {
                Ok(msg) => match msg {
                    network::controller::NetworkControllerEvent::CandidateConnection(ip, mut stream, is_outgoing) => {
                        //TODO: Hansdshake
                        // let tx = chan.0.clone();
                        // tokio::spawn(async move {
                            let mut buff: [u8; 1] = [0; 1];
                            if is_outgoing {
                                buff[0] = Event::to_u8(&Event::Handshake);
                                // '1'.encode_utf8(&mut buff);
                                if let Err(err) = stream.write_all(&buff).await {
                                    error!("[{}] {}", ip, err);
                                    net.feedback_peer_failed(&ip).await.unwrap();
                                    continue;
                                }
                            } else {
                                if let Err(err) = stream.read_exact(&mut buff).await {
                                    error!("[{}] stream closed during hanshake {}", ip, err);
                                    net.feedback_peer_failed(&ip).await.unwrap();
                                    // return Err(err.into());
                                    continue;
                                }

                                if let Ok(event) = Event::from_u8(buff[0]) && event == Event::Handshake {
                                } else {
                                    error!("[{}] handshake", ip);
                                    net.feedback_peer_failed(&ip).await.unwrap();
                                    continue;
                                }
                            }

                            // if let Ok(event) = Event::from_u8(buff[0]) && event == Event::Hansdshake {
                                // successfull Hansdshake 
                                net.feedback_peer_alive(&ip, is_outgoing).await.unwrap();
                                loop {
                                    if let Err(err) = stream.read_exact(&mut buff).await {
                                        debug!("[{}] stream closed {}", ip, err);
                                        net.feedback_peer_failed(&ip).await?;
                                        // return Err(err.into());
                                        break;
                                    }
                                    info!("[{}] receive event {}", ip, buff[0]);
                                    net.feedback_peer_alive(&ip, is_outgoing).await.unwrap();
                                    if let Ok(event) = Event::from_u8(buff[0]) {
                                        match event {
                                            Event::Handshake => error!("{} HANSDSHAKE", ip),
                                            Event::Alive => info!("{} ALIVE", ip),
                                            Event::Misbehaves => info!("{} MISBEHAVES", ip),
                                            Event::GetGoodIps => info!("{} GETGOODIPS", ip),
                                        }
                                    } else {
                                        error!("[{}] receive unknow event", ip);
                                        net.feedback_peer_banned(&ip).await.unwrap();
                                    }
                                }
                            // } else {
                            //     error!("[{}] error handshaking", ip);
                            // }
                        // }
                                    

                        // });
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
