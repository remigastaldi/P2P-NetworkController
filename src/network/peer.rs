use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, error, debug, Level, field::debug};

pub type IP = String;

#[derive(Clone, Copy)]
#[derive(PartialEq)]
#[derive(Debug)]
pub enum Status {
    Idle,
    OutConnecting,
    OutHandshaking,
    OutAlive,
    InHandshaking,
    InAlive,
    Banned,
    Local
}

struct Peer {
   status: Status,
   last_alive: Option<DateTime<Utc>>,
   last_failure: Option<DateTime<Utc>>,
}

impl Peer {
    pub fn new() -> Self {
        Peer{ status: Status::Idle, last_alive: None, last_failure: None }
    }

    pub fn from<T>(status: T) -> Self
        where T: Into<Status> {
        Peer{ status: status.into() , last_alive: None, last_failure: None }
    }

    // Return old status
    pub fn set_status(&mut self, status: Status) -> Result<Status, String> {
        match (self.status, status) {
            (Status::Banned, Status::InHandshaking) => {
                self.last_failure = Some(Utc::now());
                return Err(String::from("Peer banned"));
            }
            (Status::InAlive | Status::OutAlive, Status::InHandshaking) => return Err(String::from("Already connected")),
            (Status::InHandshaking, Status::Idle) => self.last_failure = Some(Utc::now()),
            (_, Status::InAlive | Status::OutAlive) => self.last_alive = Some(Utc::now()),
            (_, Status::Banned) => self.last_failure = Some(Utc::now()),
            (_,_) => { }
        } 
        let old = self.status;
        self.status = status;
        Ok(old)
    }

    pub fn status(&self) -> &Status {
        &self.status
    }
}

pub struct PeerController {
    peers: HashMap<IP, Peer>,
    idle: u64,
    out_connecting: u64,
    out_handshaking: u64,
    out_alive: u64,
    in_handshaking: u64,
    in_alive: u64,
    banned: u64
}

impl PeerController {
    fn new() -> Self {
        PeerController { peers: HashMap::new(), idle: 0, out_connecting: 0, out_handshaking: 0, out_alive: 0, in_handshaking: 0, in_alive: 0, banned: 0 }
    }

    // pub fn from<T>(peers_ips: T) -> Self
    //     where T: ExactSizeIterator + IntoIterator<Item = IP> {
    pub fn from(peers_ips: Vec<IP>) -> Self {
        let len = peers_ips.len() as u64;
        PeerController { peers: peers_ips.into_iter().map(|peer_ip| (peer_ip, Peer::new())).collect::<HashMap::<IP, Peer>>(), idle: len, out_connecting: 0, out_handshaking: 0, out_alive: 0, in_handshaking: 0, in_alive: 0, banned: 0 }
    }

    // Abstraction of Status struct
    pub fn on_peer_connect(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] InHandshaking", ip);
        self.update_peer_status(ip, Status::InHandshaking)
    }

    pub fn on_peer_out_connecting(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] OutConnecting", ip);
        self.update_peer_status(ip, Status::OutConnecting)
    }

    pub fn on_peer_out_handshaking(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] OutHandshaking", ip);
        self.update_peer_status(ip, Status::OutHandshaking)
    }

    pub fn on_peer_in_alive(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] InAlive", ip);
        self.update_peer_status(ip, Status::InAlive)
    }

    pub fn on_peer_out_alive(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] OutAlive", ip);
        self.update_peer_status(ip, Status::OutAlive)
    }

    pub fn on_peer_failed(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] Idle", ip);
        self.update_peer_status(ip, Status::Idle)
    }

    pub fn feedback_peer_banned(&mut self, ip: &IP) -> Result<(), String> {
        debug!("[{}] Banned", ip);
        self.update_peer_status(ip, Status::Banned)
    }

    pub fn flag_as_local(&mut self, ip: &IP) { 
        for peer in self.peers.iter_mut() {
            if peer.0 == ip {
                debug!("[{}] Is local ip", ip);
                peer.1.set_status(Status::Local)    .unwrap();
            }
        }
    }
    
    //TODO: replace those matches with a macro
    fn update_peer_status(&mut self, ip: &IP, status: Status) -> Result<(), String> {
        match self.peers.get_mut(ip) {
            Some(peer) => {
                let old_status = peer.set_status(status)?;
                match old_status {
                    Status::Idle => self.idle -= 1,
                    Status::OutConnecting => self.out_connecting -= 1,
                    Status::OutHandshaking => self.out_handshaking -= 1,
                    Status::OutAlive => self.out_alive -= 1,
                    Status::InHandshaking => self.in_handshaking -= 1,
                    Status::InAlive => self.in_alive -= 1,
                    Status::Banned => self.banned -= 1,
                    Status::Local => todo!() 
                }
            },
            None => { self.peers.insert(ip.clone(), Peer::from(status)); }
        }
        
        match status {
            Status::Idle => self.idle += 1,
            Status::OutConnecting => self.out_connecting += 1,
            Status::OutHandshaking => self.out_handshaking += 1,
            Status::OutAlive => self.out_alive += 1,
            Status::InHandshaking => self.in_handshaking += 1,
            Status::InAlive => self.in_alive += 1,
            Status::Banned => self.banned += 1,
            Status::Local => todo!(),
        }

        Ok(())
    }

    //TODO
    pub fn best_peers(&self) -> Vec<IP> {
       self.peers.iter().map(|ip| ip.0.clone()).collect() 
    }

    //TODO
    pub fn best_idle_peer_ip(&self) -> Option<IP> {
        for peer in self.peers.iter() {
            if peer.1.status() == &Status::Idle {
                return Some(peer.0.clone())
            }
        }
        None
    }

    pub fn peer_status(&self, ip: &IP) -> Option<Status> {
        Some(self.peers.get(ip)?.status().clone())
    }

    pub fn is_idle(&self, ip: &IP) -> Option<bool> {
       Some(self.peers.get(ip)?.status() == &Status::Idle)
    }

    pub fn peers_ip(&self) -> Vec<&IP> {
        self.peers.iter().map(|peer| peer.0).collect()
    } 

    pub fn idle(&self) -> u64 {
        self.idle
    }

    pub fn out_connecting(&self) -> u64 {
        self.out_connecting
    }

    pub fn in_handshaking(&self) -> u64 {
        self.in_handshaking
    }

    pub fn out_handshaking(&self) -> u64 {
        self.out_handshaking
    }

    pub fn out_alive(&self) -> u64 {
        self.out_alive
    }

    pub fn in_alive(&self) -> u64 {
        self.in_alive
    }

    pub fn banned(&self) -> u64 {
        self.banned
    }
}
