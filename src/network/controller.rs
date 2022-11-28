use std::{fs::File, error::Error, sync::Arc};
use tokio::{net::{TcpStream, TcpListener}, sync::{mpsc::{Receiver, channel, Sender}, RwLock}};
use std::io::prelude::*;
use crate::network::peer::Status;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{info, error, debug};
use local_ip_address::local_ip;
use super::peer::{IP, PeerController};

pub enum NetworkControllerEvent {
    CandidateConnection(IP, TcpStream, bool)
}

pub struct NetworkController {
    shutdown_token: CancellationToken,
    peers: Arc<RwLock<PeerController>>,
    channel: (Sender<NetworkControllerEvent>, Receiver<NetworkControllerEvent>)
}

impl NetworkController {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(shutdown_token: CancellationToken, peers_file_path: &str, port: u32, target_out_connections: u64, max_inc_connections: u64, max_simultaneous_out_connections: u64, max_simultaneous_inc_connections: u64, _max_idle_peers: u64, _max_banned_peers: u64, peers_file_dump_interval: u64) -> Result<Self, Box<dyn Error>> {
        // Load peers's ip from file
        let mut peers_file = File::options().read(true).write(true).open(peers_file_path)?;
        let mut content = String::new();
        peers_file.read_to_string(&mut content)?;
        let loc_peers: Vec<String> = serde_json::from_str(&content)?;
//TODO: max idle
//TODO check invalid config, ex no enough ips for target_out_connections
        debug!("{:?} Ips loaded", loc_peers);
    
        let mut peer_controller = PeerController::from(loc_peers);
        // Flag local and public ip to prevent self connection
        peer_controller.flag_as_local(&local_ip().unwrap().to_string());
        peer_controller.flag_as_local(&public_ip::addr().await.unwrap().to_string()); 

        //TODO: sort peers
        let mut controller = NetworkController{
            shutdown_token,
            peers: Arc::new(RwLock::new(peer_controller)),
            channel: channel(4096),
        };

        controller.start_listening_service(port, max_inc_connections, max_simultaneous_inc_connections).await?;
        controller.start_connection_service(target_out_connections, max_simultaneous_out_connections);
        controller.start_peers_file_watcher_service(peers_file, peers_file_dump_interval);

        Ok(controller)
    }

    fn start_peers_file_watcher_service(&self, mut peers_file: File, peers_file_dump_interval: u64) {
        let peers_controller = Arc::clone(&self.peers);
        let shutdown_token = self.shutdown_token.child_token();
        
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(peers_file_dump_interval)).await;

                //TODO: optimise this, no needs for reading again as the file is kept open
                let mut content = String::new();
                peers_file.rewind().unwrap();
                peers_file.read_to_string(&mut content).unwrap();
                let loc_peers: Vec<String> = serde_json::from_str(&content).unwrap();
                
                let current_peers : Vec<IP> = {
                    let l_peer_controller = peers_controller.read().await;
                    l_peer_controller.peers_ip().iter().map(|&ip| ip.clone()).collect()
                };
                // Naive search, can be optimised
                for peer_ip in loc_peers {
                    if !current_peers.iter().any(|hot_peer| { &peer_ip == hot_peer }) {
                        debug!("Updating local peers file");
                        let peers_to_write = serde_json::to_string(&current_peers).unwrap();
                        peers_file.write_all(peers_to_write.as_bytes()).unwrap();
                        break; 
                    }
                }
                if shutdown_token.is_cancelled() {
                    debug!("Gracefully shutdown file watcher service");
                    break;
                }
            }
        });
    }
    
    fn start_connection_service(&mut self, target_out_connections: u64, max_simultaneous_out_connections: u64) {
        let peers_controller = Arc::clone(&self.peers);
        let tx = self.channel.0.clone();
        let shutdown_token = self.shutdown_token.child_token();
        let mut out_connecting = FuturesUnordered::new();
        let mut out_handshaking = FuturesUnordered::new();

        tokio::spawn(async move {
            loop {
                if shutdown_token.is_cancelled() {
                    debug!("Gracefully shutdown out connection service");
                    break;
                }
    
                let (available_slots, available_peer) = {
                    let peers_controller = peers_controller.read().await;
                    let out_alive = peers_controller.out_alive();
                    let currently_out_co = peers_controller.out_connecting();

                    (target_out_connections - out_alive - currently_out_co, peers_controller.idle())
                };

                if available_peer == 0 {
                    info!("No peer available");
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
                {
                    let mut peers_controller = peers_controller.write().await;
                    let to_connect = if available_peer > available_slots {available_slots} else {available_peer};
                    for _ in 0..to_connect {
                        let Some(ip) = peers_controller.best_idle_peer_ip() else {
                            error!("No best peer available, should not happening");
                            break;
                        };

                        peers_controller.on_peer_out_connecting(&ip).unwrap();
                        out_connecting.push(async move {
                            match TcpStream::connect(format!("{}:4242", &ip)).await {
                                Ok(stream) => {
                                    (ip, Ok(stream))
                                },
                                Err(err) => {
                                    error!("[{}] {:?}", ip, err);
                                    (ip, Err(err))
                                }
                            }
                        });
                    }
                }

                loop {
                    info!("OutConnecting: {} OutHandshaking: {}", out_connecting.len(), out_handshaking.len());
                    tokio::select! {
                        Some((ip, res_stream)) = out_connecting.next() => {
                            let peers_controller = peers_controller.clone();
                            if let Ok(stream) = res_stream {
                                let tx_clone = tx.clone();
                                peers_controller.write().await.on_peer_out_handshaking(&ip).unwrap();
                                out_handshaking.push(async move {
                                    if (tx_clone.send(NetworkControllerEvent::CandidateConnection(ip.clone(), stream, true)).await).is_err() {
                                        error!("start_listening_service: Channel closed");
                                        peers_controller.write().await.on_peer_failed(&ip).unwrap();
                                    }
                                });
                            } else {
                                peers_controller.write().await.on_peer_failed(&ip).unwrap();
                            }
                        }

                        Some(_) = out_handshaking.next(), if peers_controller.write().await.out_handshaking() <= max_simultaneous_out_connections => { }
                        else => {
                            sleep(Duration::from_secs(5)).await;
                            break;
                        }
                    }
                }
            }
        });
    }

    async fn start_listening_service(&mut self, port: u32, max_inc_connections: u64, max_simultaneous_inc_connections: u64) -> Result<(), Box<dyn Error>> {
        let tx = self.channel.0.clone();
        let peers_controller = Arc::clone(&self.peers);
        let mut in_handshaking = FuturesUnordered::new();
        let shutdown_token = self.shutdown_token.child_token();

        let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        debug!("Gracefully shutdown listening service");
                        break;
                    }

                    res = listener.accept(), if peers_controller.read().await.in_alive() <= max_inc_connections => {
                        match res {
                            Ok((stream, addr)) => {
                                let ip = addr.ip().to_string();
                                //TODO: add to peer list if not present
                                {
                                    let mut peers_controller = peers_controller.write().await;
                                    if let Some(status) = peers_controller.peer_status(&ip) && status != Status::Idle {
                                        info!("[{}] peer already in {:?}", ip, status);
                                        continue;
                                    }

                                    if let Err(err) = peers_controller.on_peer_connect(&ip) {
                                        error!("[{}] {}", ip, err);
                                        continue;
                                    }
                                }
                                let tx_clone = tx.clone();
                                let peers_controller = peers_controller.clone();
                                in_handshaking.push(async move {
                                    if (tx_clone.send(NetworkControllerEvent::CandidateConnection(ip.clone(), stream, false)).await).is_err() {
                                        error!("start_listening_service: Channel closed");
                                        peers_controller.write().await.on_peer_failed(&ip).unwrap();
                                    }
                                });
                            },
                            Err(err) => error!("= {:?}", err)
                        }
                    }

                    _ = in_handshaking.next(), if peers_controller.read().await.in_handshaking() <= max_simultaneous_inc_connections => {}
                }
            }
        });
        Ok(())
    }

    pub async fn wait_event(&mut self) -> Result<NetworkControllerEvent, String> {
        self.channel.1.recv().await.ok_or(String::from("wait_event: Channel closed"))
    }

    pub async fn feedback_peer_alive(&mut self, ip: &IP) -> Result<(), String> {
        let mut peer_controller = self.peers.write().await;
        let peer_status = peer_controller.peer_status(ip).unwrap();
        match peer_status {
            Status::InAlive | Status::InHandshaking => peer_controller.on_peer_in_alive(ip),
            Status::OutAlive | Status::OutHandshaking  => peer_controller.on_peer_out_alive(ip),
            _ => Err(format!("Peer cannot switch from {:?} to Alive", peer_status))
        }
    }
    
    pub async fn feedback_peer_failed(&mut self, ip: &IP) -> Result<(), String> {
        self.peers.write().await.on_peer_failed(ip) 
    }

    pub async fn feedback_peer_banned(&mut self, ip: &IP) -> Result<(), String> {
        // let peers = self.peers.lock().await;
        // let banned = peers.banned();
        //TODO: drop banned peers
        self.peers.write().await.feedback_peer_banned(ip) 
    }
    
    pub async fn get_good_peer_ips(&self) -> Vec<IP> {
        self.peers.read().await.best_peers()
    }

    pub async fn feedback_peer_list(&self, _peer_ip: &[IP]) {
        //TODO: smart merge
    }
}
