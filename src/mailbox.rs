/* Copyright (C) 2020 by Jacob Alexander
 *
 * This file is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This file is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this file.  If not, see <http://www.gnu.org/licenses/>.
 */

/// Mailbox
/// Handles message passing between devices, modules and api calls
/// Uses a broadcast channel to handle communication
// ----- Modules -----
use crate::api::Endpoint;
use crate::protocol::hidio;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::stream::StreamExt;
use tokio::sync::broadcast;

// ----- Enumerations -----

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Address {
    // All/any addressed (used as a broadcast destination, not as a source)
    All,
    // Capnproto API address, with node uid
    ApiCapnp {
        uid: u64,
    },
    // Cancel subscription
    // Used to gracefully end message streams
    CancelSubscription {
        // Uid of endpoint of the subscription
        uid: u64,
        // Subscription id
        sid: u64,
    },
    // HidIo address, with node uid
    DeviceHidio {
        uid: u64,
    },
    // Generic HID address, with nod uid
    DeviceHid {
        uid: u64,
    },
    // Drop subscription
    DropSubscription,
    // Module address
    Module,
}

// ----- Consts -----

/// Number of message slots for the mailbox broadcast channel
/// Must be equal to the largest queue needed for the slowest receiver
const CHANNEL_SLOTS: usize = 100;

// ----- Structs -----

/// HID-IO Mailbox
///
/// Handles passing messages to various components inside of HID-IO
/// Best thought of as a broadcast style packet switcher.
/// Each thread (usually async tokio) is given a receiver and can filter for
/// any desired packets.
/// This is not quite as effecient as direct channels; however, this greatly
/// simplifies message passing across HID-IO. Making it easier to add new modules.
///
/// This struct can be safely cloned and passed around anywhere in the codebase.
/// In most cases only the sender field is used (as it has the subscribe() function as well).
#[derive(Clone, Debug)]
pub struct Mailbox {
    pub nodes: Arc<RwLock<Vec<Endpoint>>>,
    pub last_uid: Arc<RwLock<u64>>,
    pub lookup: Arc<RwLock<HashMap<String, Vec<u64>>>>,
    pub sender: broadcast::Sender<Message>,
}

impl Mailbox {
    pub fn new() -> Mailbox {
        // Create broadcast channel
        let (sender, _) = broadcast::channel::<Message>(CHANNEL_SLOTS);
        // Setup nodes list
        let nodes = Arc::new(RwLock::new(vec![]));
        // Setup nodes lookup table
        let lookup = Arc::new(RwLock::new(HashMap::new()));
        // Setup last uid assigned (uids are reused when possible for devices)
        let last_uid: Arc<RwLock<u64>> = Arc::new(RwLock::new(0));
        Mailbox {
            nodes,
            last_uid,
            lookup,
            sender,
        }
    }

    /// Attempt to locate an unused id for the device key
    pub fn get_uid(&mut self, key: String, path: String) -> Option<u64> {
        let mut lookup = self.lookup.write().unwrap();
        let lookup_entry = lookup.entry(key).or_default();

        // Locate an id
        'outer: for uid in lookup_entry.iter() {
            for mut node in (*self.nodes.read().unwrap()).clone() {
                if node.uid() == *uid {
                    // Id is being used, and has the same path (i.e. this device)
                    if node.path() == path {
                        // Return an invalid Id (0)
                        return Some(0);
                    }

                    // Id is being used, and is not available
                    continue 'outer;
                }
            }
            // Id is not being used
            return Some(*uid);
        }

        // Could not locate an id
        None
    }

    /// Add uid to lookup
    pub fn add_uid(&mut self, key: String, uid: u64) {
        let mut lookup = self.lookup.write().unwrap();
        let lookup_entry = lookup.entry(key).or_default();
        lookup_entry.push(uid);
    }

    /// Assign uid
    /// This function will attempt to lookup an existing id first
    /// And generate a new uid if necessary
    /// An error is returned if this lookup already has a uid (string+path)
    pub fn assign_uid(&mut self, key: String, path: String) -> Result<u64, std::io::Error> {
        match self.get_uid(key.clone(), path) {
            Some(0) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "uid has already been registered!",
            )),
            Some(uid) => Ok(uid),
            None => {
                // Get last created id and increment
                (*self.last_uid.write().unwrap()) += 1;
                let uid = *self.last_uid.read().unwrap();

                // Add id to lookup
                self.add_uid(key, uid);
                Ok(uid)
            }
        }
    }

    /// Register node as an endpoint (device or api)
    pub fn register_node(&mut self, mut endpoint: Endpoint) {
        info!("Registering endpoint: {}", endpoint.uid());
        let mut nodes = self.nodes.write().unwrap();
        (*nodes).push(endpoint);
    }

    /// Unregister node as an endpoint (device or api)
    pub fn unregister_node(&mut self, uid: u64) {
        info!("Unregistering endpoint: {}", uid);
        let mut nodes = self.nodes.write().unwrap();
        *nodes = nodes
            .drain_filter(|dev| dev.uid() != uid)
            .collect::<Vec<_>>();
    }

    /// Convenience function to send a HidIo Command to device using the mailbox
    pub fn send_command(
        &self,
        src: Address,
        dst: Address,
        id: hidio::HidIoCommandID,
        data: Vec<u8>,
    ) {
        // Construct command packet
        let data = hidio::HidIoPacketBuffer {
            ptype: hidio::HidIoPacketType::Data,
            id: id as u32,
            max_len: 64, //..Defaults
            data,
            done: true,
        };

        // Construct command message and broadcast
        let result = self.sender.send(Message { src, dst, data });

        if let Err(e) = result {
            error!("send_command (no active receivers) {:?}", e);
        }
    }

    /// Convenience function to wait for an ack
    /// Will return the next matching ack or nak packet
    /// If a sync packet is returned, then an error is thrown (supports waiting for a number of
    /// sync packets)
    pub async fn ack_wait(
        &self,
        src: Address,
        id: hidio::HidIoCommandID,
        mut max_sync_packets: usize,
    ) -> Result<Message, AckWaitError> {
        // Prepare receiver and filter
        let receiver = self.sender.subscribe();
        tokio::pin! {
            let stream = receiver.into_stream().filter(Result::is_ok).map(Result::unwrap).filter(|msg| msg.src == src && msg.data.id == id as u32);
        }
        // Wait on filtered messages
        while let Some(msg) = stream.next().await {
            match msg.data.ptype {
                // Syncs signify a timeout as they are only sent when there is no HidIo traffic
                // Some wait operations may take a while, so it might be necessary to skip syncs
                // before timing out
                hidio::HidIoPacketType::Sync => {
                    if max_sync_packets == 0 {
                        return Err(AckWaitError::TooManySyncs);
                    }
                    max_sync_packets -= 1;
                }
                hidio::HidIoPacketType::ACK => {
                    return Ok(msg);
                }
                // We may still want the message data from a NAK
                hidio::HidIoPacketType::NAK => {
                    return Err(AckWaitError::NAKReceived { msg });
                }
                // We don't care about data packets
                hidio::HidIoPacketType::NAData | hidio::HidIoPacketType::NAContinued => {}
                hidio::HidIoPacketType::Continued | hidio::HidIoPacketType::Data => {}
            }
        }
        Err(AckWaitError::Invalid)
    }

    pub fn drop_subscriber(&self, uid: u64, sid: u64) {
        // Construct a dummy message
        let data = hidio::HidIoPacketBuffer::default();

        // Construct command message and broadcast
        let result = self.sender.send(Message {
            src: Address::DropSubscription,
            dst: Address::CancelSubscription { uid, sid },
            data,
        });

        if let Err(e) = result {
            error!("drop_subscriber {:?}", e);
        }
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Container for HidIoPacketBuffer
/// Used to indicate the source and destinations inside of hid-io-core.
/// Also contains a variety of convenience functions using the src and dst information.
#[derive(PartialEq, Clone, Debug)]
pub struct Message {
    pub src: Address,
    pub dst: Address,
    pub data: hidio::HidIoPacketBuffer,
}

impl Message {
    pub fn new(src: Address, dst: Address, data: hidio::HidIoPacketBuffer) -> Message {
        Message { src, dst, data }
    }

    /// Acknowledgement of a HidIo packet
    pub fn send_ack(&self, sender: broadcast::Sender<Message>, data: Vec<u8>) {
        let src = self.dst;
        let dst = self.src;

        // Construct ack packet
        let data = hidio::HidIoPacketBuffer {
            ptype: hidio::HidIoPacketType::ACK,
            id: self.data.id, // id,
            max_len: 64,      // Default
            data,
            done: true,
        };

        // Construct ack message and broadcast
        let result = sender.send(Message { src, dst, data });

        if let Err(e) = result {
            error!("send_ack {:?}", e);
        }
    }

    /// Rejection/NAK of a HidIo packet
    pub fn send_nak(&self, sender: broadcast::Sender<Message>, data: Vec<u8>) {
        let src = self.dst;
        let dst = self.src;

        // Construct ack packet
        let data = hidio::HidIoPacketBuffer {
            ptype: hidio::HidIoPacketType::NAK,
            id: self.data.id, // id,
            max_len: 64,      // Default
            data,
            done: true,
        };

        // Construct ack message and broadcast
        let result = sender.send(Message { src, dst, data });

        if let Err(e) = result {
            error!("send_ack {:?}", e);
        }
    }
}

pub enum AckWaitError {
    TooManySyncs,
    NAKReceived { msg: Message },
    Invalid,
}
