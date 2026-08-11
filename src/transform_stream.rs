use std::sync::{Arc, Mutex};
use cgmath::{InnerSpace, Point3, Quaternion, Vector3};

use crate::ring_buffer;

/// Number of f32 in one transform frame — the unit every source below deals in, identical on the
/// wire, in an HDF5 `states` row, and in inline scene JSON.
pub const DATA_ARR_WIDTH: usize = 12;
const F32_SIZE: usize = std::mem::size_of::<f32>();
const FRAME_BYTE_WIDTH: usize = DATA_ARR_WIDTH * F32_SIZE;

/// A source of transform frames, yielding one per rendered frame.
///
/// Three sources sit behind one interface: a live ring buffer fed by the network threads, an HDF5
/// `states` dataset read eagerly at load, and values written inline in the scene JSON. The
/// consumer neither knows nor cares which it got.
///
/// This is shared by entities and cameras. What they do with a frame differs — an entity records
/// a trail point and drives a scene graph, a camera drives a view matrix — but where the frame
/// comes from and how its twelve floats are read must not, or a camera streaming alongside the
/// drone it follows would sit at a plausible but wrong offset with nothing reporting an error.
pub struct TransformStream {
    /// Frames already in hand, flattened: HDF5 rows read at load time, followed by any inline
    /// values the scene JSON listed after the source name.
    data: Vec<f32>,
    /// A live stream, co-owned with the buffer registry. `None` for the eager sources.
    buffer: Option<ring_buffer::SharedBuffer>,
    /// The registry key `buffer` was bound under, so the binding can be remade after a purge.
    /// Only set for live streams — the eager sources have nothing to re-bind to.
    source: Option<String>,
}

/// Summarised rather than derived: the ring buffer behind a live stream holds up to 48KB, and
/// dumping it would bury the three facts that identify a stream at a glance.
impl std::fmt::Debug for TransformStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformStream")
            .field("source", &self.source)
            .field("live", &self.buffer.is_some())
            .field("frames_in_hand", &(self.data.len() / DATA_ARR_WIDTH))
            .finish()
    }
}

impl TransformStream {
    /// Builds a stream from a behavior's `data` array: element 0 names the source, and anything
    /// after it is inline frame values appended to whatever that source yields.
    ///
    /// A `.hdf5` name is opened and flattened immediately; any other name binds to the buffer
    /// registry, creating the entry if the stream has not connected yet. That binding is by
    /// filename alone and cannot fail — a name no server ever sends simply yields no frames.
    pub fn from_json_data(
        values: &[&serde_json::Value],
        registry: &ring_buffer::BufferRegistry,
    ) -> TransformStream {
        let mut data: Vec<f32> = Vec::new();

        let Some((source, inline)) = values.split_first() else {
            return TransformStream { data, buffer: None, source: None };
        };

        let name = source
            .as_str()
            .expect("the first data element of a streaming behavior must be its source filename");

        let mut stream = if name.ends_with(".hdf5") {
            let file = hdf5::File::open(name).unwrap();
            let dataset = file.dataset("states").unwrap();
            let data_array: ndarray::Array2<f32> = dataset.read_2d().unwrap();
            for row in data_array.rows() {
                data.extend(row.iter().copied());
            }
            TransformStream { data: Vec::new(), buffer: None, source: None }
        } else {
            TransformStream::from_registry(name, registry)
        };

        for value in inline {
            data.push(value.as_f64().unwrap() as f32);
        }
        stream.data = data;

        stream
    }

    /// Binds to a registry entry by filename, creating it if the stream has not connected yet.
    pub fn from_registry(name: &str, registry: &ring_buffer::BufferRegistry) -> TransformStream {
        let buffer = registry.lock().unwrap()
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(ring_buffer::RingBuffer::new())))
            .clone();

        TransformStream {
            data: Vec::new(),
            buffer: Some(buffer),
            source: Some(name.to_string()),
        }
    }

    /// Builds a stream over frames already flattened in memory, for the HDF5 scene loader.
    pub fn from_values(data: Vec<f32>) -> TransformStream {
        TransformStream { data, buffer: None, source: None }
    }

    /// Re-binds a live stream to the registry, replacing a handle whose entry has been purged.
    ///
    /// Dropping a `TransformStream` is normally enough, because `clear_registry` runs alongside
    /// dropping everything that held one. A camera on the surviving viewport is the exception: it
    /// outlives the purge still holding its `Arc`, and `com::write_to_registry` looks entries up
    /// with `get`, so every arriving chunk for that name would be discarded and the camera would
    /// stall forever with nothing reporting an error. A no-op for the eager sources.
    pub fn rebind(&mut self, registry: &ring_buffer::BufferRegistry) {
        let Some(ref name) = self.source else { return };
        *self = TransformStream::from_registry(&name.clone(), registry);
    }

    /// Takes the next whole frame, or `None` when there is not one to take.
    ///
    /// A bound buffer is authoritative even while empty — it never falls through to `data`. The
    /// two coexist only when a scene lists inline values after a `.bin` name, and a live stream
    /// that outruns the render loop must not silently rewind into stale JSON.
    ///
    /// A partial frame is dropped rather than held: `RingBuffer::read` drains what it returns, so
    /// the leftover bytes are gone. Frames arrive whole, so this only fires on a truncated tail.
    pub fn next_frame(&mut self) -> Option<[f32; DATA_ARR_WIDTH]> {
        let mut frame = [0f32; DATA_ARR_WIDTH];

        if let Some(ref buffer) = self.buffer {
            let bytes = buffer.lock().unwrap().read(FRAME_BYTE_WIDTH);
            if bytes.len() != FRAME_BYTE_WIDTH {
                return None;
            }
            for (i, slot) in frame.iter_mut().enumerate() {
                let word = bytes[i * F32_SIZE..(i + 1) * F32_SIZE].try_into().unwrap();
                *slot = f32::from_be_bytes(word);
            }
        } else {
            if self.data.len() < DATA_ARR_WIDTH {
                return None;
            }
            frame.copy_from_slice(&self.data[..DATA_ARR_WIDTH]);
            self.data.drain(0..DATA_ARR_WIDTH);
        }

        Some(frame)
    }

    /// Whether this stream can never yield another frame.
    ///
    /// A live buffer that is merely empty still counts as exhausted: the network path uses this to
    /// decide a transmission is over, and a connected stream between packets is indistinguishable
    /// from one that has finished.
    pub fn is_exhausted(&self) -> bool {
        let buffer_empty = self.buffer.as_ref()
            .map_or(true, |buffer| buffer.lock().unwrap().is_empty());

        buffer_empty && self.data.len() < DATA_ARR_WIDTH
    }

    /// The registry key this stream reads, if it is a live one. `None` for the eager sources,
    /// which have no shared buffer and so cannot collide with anything.
    pub fn source_name(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// How many f32 remain in hand. Zero for a live stream, whose total is unknowable — callers
    /// sizing a progress bar want the eager sources only.
    pub fn remaining(&self) -> usize {
        self.data.len()
    }
}

/// Reads a frame's position and rotation.
///
/// Only six of the twelve floats are used. The quaternion arrives as its vector part alone, with
/// the scalar reconstructed on the unit-norm assumption; `max(0.0)` guards the square root against
/// a vector that floating-point error has pushed just past unit length.
pub fn frame_to_transform(frame: &[f32; DATA_ARR_WIDTH]) -> (Point3<f32>, Quaternion<f32>) {
    let position = Point3::new(frame[0], frame[1], frame[2]);
    let rotation_vector = Vector3::new(frame[6], frame[7], frame[8]);
    let w = (1.0 - rotation_vector.magnitude2()).max(0.0).sqrt();

    (position, Quaternion::from_sv(w, rotation_vector))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_values(seed: f32) -> Vec<f32> {
        (0..DATA_ARR_WIDTH).map(|i| seed + i as f32).collect()
    }

    fn buffered(frames: &[Vec<f32>]) -> TransformStream {
        let buffer = Arc::new(Mutex::new(ring_buffer::RingBuffer::new()));
        {
            let mut guard = buffer.lock().unwrap();
            for frame in frames {
                let bytes: Vec<u8> = frame.iter().flat_map(|v| v.to_be_bytes()).collect();
                guard.write(&bytes);
            }
        }
        TransformStream { data: Vec::new(), buffer: Some(buffer), source: None }
    }

    #[test]
    fn inline_values_are_consumed_one_frame_at_a_time() {
        let mut values = frame_values(0.0);
        values.extend(frame_values(100.0));
        let mut stream = TransformStream::from_values(values);

        assert_eq!(stream.next_frame().unwrap()[0], 0.0);
        assert_eq!(stream.remaining(), DATA_ARR_WIDTH, "one frame consumed, one left");
        assert_eq!(stream.next_frame().unwrap()[0], 100.0);
        assert_eq!(stream.next_frame(), None);
    }

    // The wire format is big-endian; reading it natively would decode to garbage on x86/ARM
    // without erroring, so pin the byte order rather than assume it.
    #[test]
    fn buffered_frames_are_decoded_big_endian() {
        let mut stream = buffered(&[frame_values(1.5)]);

        let frame = stream.next_frame().expect("a whole frame was written");

        assert_eq!(frame.to_vec(), frame_values(1.5));
        assert_eq!(stream.next_frame(), None, "nothing left after the one frame");
    }

    // A live stream that outruns the render loop must stall, not fall back on stale JSON values
    // that would rewind the entity to wherever the scene file happened to start it.
    #[test]
    fn an_empty_buffer_does_not_fall_through_to_inline_values() {
        let mut stream = buffered(&[]);
        stream.data = frame_values(42.0);

        assert_eq!(stream.next_frame(), None);
        assert_eq!(stream.remaining(), DATA_ARR_WIDTH, "the inline frame is untouched, not consumed");
    }

    // The hazard `Scene::clear` has to close. A camera on the surviving viewport keeps its Arc
    // across a registry purge, and `write_to_registry` looks entries up with `get` — so without
    // the re-bind it would sit on an orphan buffer that nothing can ever write to again.
    #[test]
    fn rebinding_reconnects_a_stream_to_a_purged_registry() {
        let registry = ring_buffer::new_registry();
        let mut stream = TransformStream::from_registry("camera1.bin", &registry);

        ring_buffer::clear_registry(&registry);
        // stands in for a chunk arriving after the purge, into whatever entry now holds the name
        let replacement = TransformStream::from_registry("camera1.bin", &registry);
        let bytes: Vec<u8> = frame_values(7.0).iter().flat_map(|v| v.to_be_bytes()).collect();
        registry.lock().unwrap().get("camera1.bin").unwrap().lock().unwrap().write(&bytes);

        assert_eq!(stream.next_frame(), None, "the stale handle sees nothing");

        stream.rebind(&registry);

        assert_eq!(stream.next_frame().map(|f| f.to_vec()), Some(frame_values(7.0)));
        drop(replacement);
    }

    // The eager sources have no registry entry to reconnect to, and clobbering their frames on a
    // clear would silently empty an HDF5-backed entity.
    #[test]
    fn rebinding_an_eager_stream_is_a_no_op() {
        let registry = ring_buffer::new_registry();
        let mut stream = TransformStream::from_values(frame_values(3.0));

        stream.rebind(&registry);

        assert_eq!(stream.next_frame().map(|f| f.to_vec()), Some(frame_values(3.0)));
    }

    #[test]
    fn a_partial_frame_yields_nothing() {
        let stream_buffer = Arc::new(Mutex::new(ring_buffer::RingBuffer::new()));
        stream_buffer.lock().unwrap().write(&[0u8; FRAME_BYTE_WIDTH - 4]);
        let mut stream = TransformStream { data: Vec::new(), buffer: Some(stream_buffer), source: None };

        assert_eq!(stream.next_frame(), None);
    }

    #[test]
    fn exhaustion_covers_both_sources() {
        let mut eager = TransformStream::from_values(frame_values(0.0));
        assert!(!eager.is_exhausted());
        eager.next_frame();
        assert!(eager.is_exhausted(), "no whole frame left");

        let mut live = buffered(&[frame_values(0.0)]);
        assert!(!live.is_exhausted());
        live.next_frame();
        assert!(live.is_exhausted(), "a drained buffer reads as exhausted");
    }

    // Rotation lives at 6..9, not 3..6 — six of the twelve floats go unused, which makes the
    // wrong offset an easy and silent mistake for a second consumer to make.
    #[test]
    fn transform_reads_position_and_rotation_from_their_own_slots() {
        let mut frame = [0f32; DATA_ARR_WIDTH];
        frame[0..3].copy_from_slice(&[1.0, 2.0, 3.0]);
        frame[3..6].copy_from_slice(&[9.0, 9.0, 9.0]); // must be ignored
        frame[6..9].copy_from_slice(&[0.5, 0.5, 0.5]);

        let (position, rotation) = frame_to_transform(&frame);

        assert_eq!(position, Point3::new(1.0, 2.0, 3.0));
        assert_eq!(rotation.v, Vector3::new(0.5, 0.5, 0.5));
        assert!((rotation.s - 0.5).abs() < 1e-6, "w = sqrt(1 - 0.75)");
    }

    // Floating-point error can push a normalised vector just past unit length, and a negative
    // square root would poison every downstream matrix with NaN.
    #[test]
    fn an_overlong_rotation_vector_does_not_produce_nan() {
        let mut frame = [0f32; DATA_ARR_WIDTH];
        frame[6..9].copy_from_slice(&[1.0, 1.0, 1.0]);

        let (_, rotation) = frame_to_transform(&frame);

        assert_eq!(rotation.s, 0.0);
    }
}
