use clear_on_drop::clear::Clear;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Key {
    pub key: [u8; 32],
    pub id: u32,
}

// zero key on drop
impl Drop for Key {
    fn drop(&mut self) {
        self.key.clear()
    }
}

#[cfg(test)]
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.key[..] == other.key[..]
    }
}

#[derive(Debug, Clone)]
pub struct KeyPair {
    pub birth: Instant,  // when was the key-pair created
    pub initiator: bool, // has the key-pair been confirmed?
    pub send: Key,       // key for outbound messages
    pub recv: Key,       // key for inbound messages
}

impl KeyPair {
    pub fn local_id(&self) -> u32 {
        self.recv.id
    }
}
