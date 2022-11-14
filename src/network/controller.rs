use std::{fs::File, error::Error, sync::Arc, collections::HashMap};
use tokio::{net::{TcpStream, TcpListener}, sync::{mpsc::{Receiver, channel, Sender}, Notify}, join};
use std::io::prelude::*;
use crate::network::peer::Status;

use super::peer::{IP, PeerController};
use tokio::time::{sleep, Duration};
use tokio::sync::Mutex; 
use tracing::{info, error, debug};
use local_ip_address::local_ip;

#[derive(Eq, Hash, PartialEq)]
#[derive(Clone, Copy)]
#[derive(Debug)]
enum NotifierEvent {
    TargetOutConnections,
    MaxIncConnections,
    MaxSimOutConnections,
    MaxSimIncConnections,
    MaxIdlePeers,
    MaxBannedPeers
}

struct NotifierService  {
    notifiers: HashMap<NotifierEvent, Arc<Notify>>,
    limits: HashMap<NotifierEvent, u64>
}

impl NotifierService {
    pub fn new(limits: HashMap<NotifierEvent, u64>) -> Self {
        NotifierService { notifiers: HashMap::new(), limits }
    }

    // pub async fn wait_event(&mut self, event: NotifierEvent) {
    //     match self.notifiers.get(&event) {
    //         Some(notifier) => notifier.notified().await,
    //         None => {
    //             let new_notifier = Notify::new();
    //             self.notifiers.insert(event, new_notifier);
    //             self.notifiers.get(&event).unwrap().notified().await;
    //         },
    //     }
    // }

    pub async fn wakeup(&mut self, event: &NotifierEvent) -> Result<(), String> {
        self.notifiers.remove(event).ok_or("Event not found")?.notify_waiters();
        Ok(())
    }

    // return an option with a Notify to wait on if the limit is reach
    pub fn check_limit(&mut self, event: &NotifierEvent, current_val: u64) -> Option<Arc<Notify>> {
        let limit = self.limits.get(event).unwrap();
        debug!("{:?} {}/{}", event, current_val, limit);

        if current_val >= *limit { 
            debug!("Reach limit on {:?}", event);
            // self.wait_event(NotifierEvent::TargetOutConnections).await;
            match self.notifiers.get(event) {
                Some(notifier) => return Some(notifier.clone()),
                None => {
                    let new_notifier = Arc::new(Notify::new());
                    self.notifiers.insert(*event, new_notifier);
                    return Some(self.notifiers.get(event).unwrap().clone())
                },
            };
        }
        None
    }
    
    pub fn waiting_events(&self) -> Vec<NotifierEvent> {
        self.notifiers.keys().copied().collect() 
    }
}

pub enum NetworkControllerEvent {
    CandidateConnection(IP, TcpStream, bool)
}

pub struct NetworkController {
    peers: Arc<Mutex<PeerController>>,
    channel: (Sender<NetworkControllerEvent>, Receiver<NetworkControllerEvent>),
    notifier: Arc<Mutex<NotifierService>>
}

impl NetworkController {
    pub async fn new(peers_file_path: &str, port: u32, target_out_connections: u64, max_inc_connections: u64, max_simultaneous_out_connections: u64, max_simultaneous_inc_connections: u64, max_idle_peers: u64, max_banned_peers: u64, peers_file_dump_interval: u64) -> Result<Self, Box<dyn Error>> {
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
            peers: Arc::new(Mutex::new(peer_controller)),
            channel: channel(4096),
            notifier: Arc::new(Mutex::new(NotifierService::new(HashMap::from([
                                                                             (NotifierEvent::TargetOutConnections, target_out_connections),
                                                                             (NotifierEvent::MaxIncConnections, max_inc_connections),
                                                                             (NotifierEvent::MaxSimOutConnections, max_simultaneous_out_connections),
                                                                             (NotifierEvent::MaxSimIncConnections, max_simultaneous_inc_connections),
                                                                             (NotifierEvent::MaxIdlePeers, max_idle_peers),
                                                                             (NotifierEvent::MaxBannedPeers, max_banned_peers)
                                                                        ]))))
        };

        controller.start_listening_service(port).await?;
        controller.start_connection_service();
        controller.start_notifier_wakeup_service().await?;
        controller.start_peers_file_watcher_service(peers_file, peers_file_dump_interval);
        Ok(controller)
    }

    // fn peer_rank(ip: &str) -> u64 {
    //     //TODO Rank algo
    //
    //     10
    // }
    
    fn start_peers_file_watcher_service(&self, mut peers_file: File, peers_file_dump_interval: u64) {
        let peers_controller = Arc::downgrade(&self.peers);

        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(peers_file_dump_interval)).await;

                //TODO: optimise this, no needs for reading again as the file is kept open
                let mut content = String::new();
                peers_file.rewind().unwrap();
                peers_file.read_to_string(&mut content).unwrap();
                let loc_peers: Vec<String> = serde_json::from_str(&content).unwrap();
                
                let peers = peers_controller.upgrade().expect("start_peers_file_watcher_service: outlived network controller");
                let current_peers : Vec<IP> = {
                    let l_peer_controller = peers.lock().await;
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
            }
        });
    }
    
    fn start_connection_service(&mut self) {
        let peers_controller = Arc::downgrade(&self.peers);
        let notifier_service = Arc::downgrade(&self.notifier);
        let tx = self.channel.0.clone();

        tokio::spawn(async move {
            loop {
                let controller = peers_controller.upgrade().expect("start_listening_service: outlived network controller");
                let notifier = notifier_service.upgrade().expect("start_listening_service: outlived network controller");

                let check_out_alive = async || {
                    let out_alive = controller.lock().await.out_alive();
                    // if let Some(notify) = notifier.lock().await.check_limit(&NotifierEvent::TargetOutConnections, out_alive) {
                    let mut tt = notifier.lock().await;
                    if let Some(notify) = tt.check_limit(&NotifierEvent::TargetOutConnections, out_alive) {
                        drop(tt);
                        notify.notified().await;
                    }
                };

                let check_out_connecting = async || {
                    let currently_out_co_hand = {
                        let l_controller = controller.lock().await;
                        l_controller.out_connecting() + l_controller.out_handshaking()
                    };

                    let mut tt = notifier.lock().await;
                    if let Some(notify) = tt.check_limit(&NotifierEvent::MaxSimOutConnections, currently_out_co_hand) {
                    // if let Some(notify) = notifier.lock().await.check_limit(&NotifierEvent::MaxSimOutConnections, currently_out_co_hand) {
                        drop(tt);
                        notify.notified().await; 
                        check_out_alive().await;  // check again because the main loop can pass the out_alive await while a peer is out_connecting
                    }
                };
                
                join!(
                    check_out_alive(),
                    check_out_connecting()
                );

                let Some(ip) = controller.lock().await.best_idle_peer_ip() else {
                    // error!("No best peer, SHOULD NOT HAPPENING");
                    error!("No available peer");

                    sleep(Duration::from_secs(10)).await;
                    return;
                };

                controller.lock().await.on_peer_out_connecting(&ip).unwrap();
                match TcpStream::connect(format!("{}:4242", &ip)).await {
                     Ok(stream) => {
                            {
                                controller.lock().await.on_peer_out_handshaking(&ip).unwrap();
                            }
                            if (tx.send(NetworkControllerEvent::CandidateConnection(ip.clone(), stream, true)).await).is_err() {
                                error!("start_listening_service: Channel closed");
                                controller.lock().await.on_peer_failed(&ip).unwrap();
                            }
                    },
                    Err(err) => {
                        error!("[{}] {:?}", ip, err);
                        controller.lock().await.on_peer_failed(&ip).unwrap();
                        // controller.lock().await.feedback_peer_banned(&ip).unwrap();
                        sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        });
    }

    async fn start_listening_service(&mut self, port: u32) -> Result<(), Box<dyn Error>> {
        let tx = self.channel.0.clone();
        let peers_controller = Arc::downgrade(&self.peers);
        let notifier_service = Arc::downgrade(&self.notifier);

        let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
        tokio::spawn(async move {
            let notifier = notifier_service.upgrade().expect("start_listening_service: outlived network controller");
            let controller = peers_controller.upgrade().expect("start_listening_service: outlived network controller");

            loop {
                {
                    let in_alive = controller.lock().await.in_alive();
                    let mut tt = notifier.lock().await;
                    if let Some(notify) = tt.check_limit(&NotifierEvent::MaxIncConnections, in_alive) {
                    // if let Some(notify) = notifier.lock().await.check_limit(&NotifierEvent::MaxSimOutConnections, currently_out_co_hand) {
                        drop(tt);
                        notify.notified().await; 
                    }
                }

                info!("Listening for new peers");
                match listener.accept().await {
                    Ok((stream, addr)) => {
                            let ip = addr.ip().to_string();
                            //TODO: add to peer list if not present
                            {
                                let mut peers = controller.lock().await;
                                if let Some(status) = peers.peer_status(&ip) && status != Status::Idle {
                                    info!("[{}] peer already in {:?}", ip, status);
                                    continue;
                                }

                                if let Err(err) = peers.on_peer_connect(&ip) {
                                    error!("[{}] {}", ip, err);
                                    continue;
                                }
                                let in_handshaking = peers.in_handshaking();
                                let mut tt = notifier.lock().await;
                                if let Some(notify) = tt.check_limit(&NotifierEvent::MaxSimIncConnections, in_handshaking) {
                                // if let Some(notify) = notifier.lock().await.check_limit(&NotifierEvent::MaxSimOutConnections, currently_out_co_hand) {
                                    drop(tt);
                                    notify.notified().await; 
                                }
                            }
                            if (tx.send(NetworkControllerEvent::CandidateConnection(ip.clone(), stream, false)).await).is_err() {
                                error!("start_listening_service: Channel closed");
                                controller.lock().await.on_peer_failed(&ip).unwrap();
                            }
                    },
                    Err(err) => error!("= {:?}", err)
                }
                sleep(Duration::from_secs(1)).await;
            }
        });
        Ok(())
    }

    async fn start_notifier_wakeup_service(&mut self) -> Result<(), Box<dyn Error>> {
        let notifier_service = Arc::downgrade(&self.notifier);
        let peers_controller = Arc::downgrade(&self.peers);

        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(2)).await;
                
                let controller = peers_controller.upgrade().expect("start_listening_service: outlived network controller");
                let peers = controller.lock().await;
                let notifier = notifier_service.upgrade().unwrap();
                let mut notifier = notifier.lock().await;

                for event in notifier.waiting_events() {
                    if match event {
                        NotifierEvent::TargetOutConnections => peers.out_alive() < *notifier.limits.get(&NotifierEvent::TargetOutConnections).unwrap(),
                        NotifierEvent::MaxIncConnections => peers.in_alive() < *notifier.limits.get(&NotifierEvent::MaxIncConnections).unwrap(),
                        NotifierEvent::MaxSimOutConnections => peers.out_connecting() + peers.out_handshaking() < *notifier.limits.get(&NotifierEvent::MaxSimOutConnections).unwrap(),
                        NotifierEvent::MaxSimIncConnections => peers.in_handshaking() < *notifier.limits.get(&NotifierEvent::MaxSimIncConnections).unwrap(), 
                        NotifierEvent::MaxIdlePeers => peers.idle() < *notifier.limits.get(&NotifierEvent::MaxIdlePeers).unwrap(),
                        NotifierEvent::MaxBannedPeers => peers.banned() < *notifier.limits.get(&NotifierEvent::MaxBannedPeers).unwrap()
                    } {
                        notifier.wakeup(&event).await.unwrap();
                    }
                }
            }
        });      
        Ok(())
    }
    
    // pub async fn get_best_idle_peer_ip(&self) -> Option<IP> {
    //     let peer_controller = self.peers.lock().await;
    //     let sorted_peers = self.sorted_peers.lock().await;
    //
    //     for ip in sorted_peers.iter() {
    //         if peer_controller.is_idle(ip).expect("Sorted peers should correspond to peer controller list") {
    //             return Some(ip.clone())
    //         }
    //     }
    //
    //     None
    // }

    pub async fn wait_event(&mut self) -> Result<NetworkControllerEvent, String> {
        self.channel.1.recv().await.ok_or(String::from("wait_event: Channel closed"))
    }

    pub async fn feedback_peer_alive(&mut self, ip: &IP) -> Result<(), String> {
        let peer_status = self.peers.lock().await.peer_status(ip).unwrap();
        match peer_status {
            Status::InAlive | Status::InHandshaking => self.peers.lock().await.on_peer_in_alive(ip),
            Status::OutAlive | Status::OutHandshaking  => self.peers.lock().await.on_peer_out_alive(ip),
            _ => Err(format!("Peer cannot switch from {:?} to Alive", peer_status))
        }
    }
    
    pub async fn feedback_peer_failed(&mut self, ip: &IP) -> Result<(), String> {
        self.peers.lock().await.on_peer_failed(ip) 
    }

    pub async fn feedback_peer_banned(&mut self, ip: &IP) -> Result<(), String> {
        let peers = self.peers.lock().await;
        let banned = peers.banned();
        let mut tt = self.notifier.lock().await;
        if tt.check_limit(&NotifierEvent::MaxBannedPeers, banned).is_some() {
            drop(tt);
            //TODO: drop banned peers
        }
        self.peers.lock().await.feedback_peer_banned(ip) 
    }
    
    pub async fn get_good_peer_ips(&self) -> Vec<IP> {
        self.peers.lock().await.best_peers()
    }

    pub async fn feedback_peer_list(&self, peer_ip: &[IP]) {
        //TODO: smart merge
    }
}
