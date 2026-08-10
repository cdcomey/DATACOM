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

/// Drops every buffer in the registry.
///
/// The registry co-owns each buffer with the behaviors bound to it — a `ChangeTransform` holds a
/// *clone* of the same `Arc`, not the only handle — so deleting every entity takes a buffer's
/// refcount from 2 to 1, never to 0, and its frames survive. Clearing a scene therefore has to
/// purge the registry explicitly or a replacement scene that reuses a filename binds to the old
/// buffer and replays up to `CAPACITY_FRAMES` of pre-clear positions.
///
/// This is safe to do while streams are live. `com::write_to_registry` looks entries up with
/// `get`, so chunks that arrive for a purged name are dropped rather than resurrecting it, and the
/// next scene that binds the name gets a fresh empty buffer that those chunks then flow into.
///
/// Must run as part of the clear itself, *before* any replacement scene is merged: a behavior that
/// bound first would keep its `Arc` clone while the registry lost the entry, leaving that entity
/// frozen against a buffer nothing can write to.
pub fn clear_registry(registry: &BufferRegistry) {
    registry.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(names: &[&str]) -> BufferRegistry {
        let registry = new_registry();
        {
            let mut map = registry.lock().unwrap();
            for name in names {
                let buf = Arc::new(Mutex::new(RingBuffer::new()));
                buf.lock().unwrap().write(&[1u8; FRAME_BYTE_WIDTH]);
                map.insert((*name).to_string(), buf);
            }
        }
        registry
    }

    // The premise behind clear_registry: an entity's handle is a clone, so dropping it leaves the
    // buffer and its frames alive in the map. If this ever stops being true, the explicit purge
    // could be reconsidered — so assert it rather than assume it.
    #[test]
    fn dropping_a_behaviors_handle_does_not_free_the_buffer() {
        let registry = registry_with(&["drone1.bin"]);

        // stands in for the Arc clone a ChangeTransform holds
        let behavior_handle = registry.lock().unwrap().get("drone1.bin").unwrap().clone();
        assert_eq!(Arc::strong_count(&behavior_handle), 2, "registry and behavior should co-own");

        drop(behavior_handle);

        let map = registry.lock().unwrap();
        let survivor = map.get("drone1.bin").expect("entry outlives the behavior that used it");
        assert!(!survivor.lock().unwrap().is_empty(), "buffered frames outlive it too");
    }

    #[test]
    fn clear_registry_drops_every_buffer() {
        let registry = registry_with(&["drone1.bin", "drone2.bin", "drone3.bin"]);

        clear_registry(&registry);

        assert!(registry.lock().unwrap().is_empty());
    }

    // The self-healing property that makes purging safe mid-stream: a name that comes back is a
    // new, empty buffer rather than the old one, so no pre-clear frame can leak into a new entity.
    #[test]
    fn a_purged_name_rebinds_to_a_fresh_buffer() {
        let registry = registry_with(&["drone1.bin"]);

        clear_registry(&registry);

        let rebound = registry.lock().unwrap()
            .entry("drone1.bin".to_string())
            .or_insert_with(|| Arc::new(Mutex::new(RingBuffer::new())))
            .clone();

        assert!(rebound.lock().unwrap().is_empty(), "rebound buffer must not carry pre-clear frames");
    }
}
