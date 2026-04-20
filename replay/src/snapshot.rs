use flowlab_core::types::SeqNum;

/// Snapshot for fast-forward replay.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub seq: SeqNum,
    pub state_hash: u64,
    pub book_data: Vec<u8>,
}

// TODO: implement serialize/deserialize for snapshot persistence
