use std::collections::{VecDeque, HashMap};
use std::sync::{Arc, Mutex};

const FRAME_BYTE_WIDTH: usize = 48; // DATA_ARR_WIDTH (12) * F32_SIZE (4)
const CAPACITY_FRAMES: usize = 1024;

pub struct RingBuffer {
    data: VecDeque<u8>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new() -> Self {
        let capacity = CAPACITY_FRAMES * FRAME_BYTE_WIDTH;
        RingBuffer {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        while self.data.len() + bytes.len() > self.capacity {
            let drop = FRAME_BYTE_WIDTH.min(self.data.len());
            self.data.drain(..drop);
        }
        self.data.extend(bytes);
    }

    pub fn read(&mut self, max_bytes: usize) -> Vec<u8> {
        let take = max_bytes.min(self.data.len());
        self.data.drain(..take).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

pub type SharedBuffer = Arc<Mutex<RingBuffer>>;
pub type BufferRegistry = Arc<Mutex<HashMap<String, SharedBuffer>>>;

pub fn new_registry() -> BufferRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
