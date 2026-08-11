# DATACOM

**Source-Agnostic 3D Data Visualization Engine**

DATACOM is a real-time 3D visualization engine written in Rust, built on WebGPU (wgpu) with custom WGSL shaders. It functions as a visual terminal for live data streams: a server sends scene definitions, 3D models, and per-frame transform data over a custom binary protocol layered on UDP, and DATACOM assembles and renders the scene in real time. Originally developed as a command-and-control interface for UAV swarm behavior, it has evolved into a source-agnostic platform capable of receiving and displaying multiple concurrent data streams simultaneously.

## Technical Highlights

- **Custom reliability layer over UDP** — hand-crafted datagram framing with CRC32 checksums, in-order reassembly of live streams via a per-file reorder buffer, per-chunk retransmission requests on checksum failure, and graceful timeout handling. Chunks for multiple files can arrive interleaved and are reassembled by file ID (a 128-bit UUID) and byte offset.
- **Multi-pipeline WebGPU renderer** — six independent wgpu render pipelines (3D models, solid geometry, terrain, lines/trails, text overlay, and UI rectangles) with their own bind group layouts and WGSL shaders. Supports multiple simultaneous viewports, each with its own perspective camera, drawn with scissor rectangles.
- **Multi-threaded streaming architecture** — each connection runs dedicated listener, sender, and assembly threads that coordinate with the main render loop via mpsc channels. Shared live-stream data is exchanged through a lock-guarded ring-buffer registry. Supports both finite file transfers and indefinite live data streams.
- **Multi-source visualization** — multiple servers can stream concurrently on separate ports; each sends its own scene JSON. The first scene defines the environment (viewports, terrain) and every stream's entities are merged into it.
- **HDF5 scientific data integration** — reads multi-dimensional trajectory datasets (position and rotation across timesteps) and maps them to a frame-by-frame behavior system with Euler-to-quaternion conversion.
- **Video recording** — scenes can be captured to a video file for offline playback.

## Features

- **Source-Agnostic Design**: Accept scene and transform data from any source with proper formatting
- **Multi-Viewport Support**: Multiple simultaneous viewports with independent cameras
- **Real-Time Visualization**: Stream and visualize live data with low latency
- **WebGPU Rendering**: wgpu-based engine with WGSL shaders and support for complex, hierarchical scenes
- **Flexible Entity System**: Hierarchical scene graph with composable, frame-driven behaviors
- **Motion Trails**: Moving entities leave rendered trails behind them
- **Multiple Data Modes**: Load scenes from JSON, HDF5 scientific data files, or live network streams

## Prerequisites

- Rust (latest stable version)
- WebGPU-compatible graphics hardware
- HDF5 (bundled and built statically via the `hdf5-metno` crate's `static` feature)

## Installation

Clone the repository:
```bash
git clone https://github.com/cdcomey/DATACOM.git
cd DATACOM
```

## Configuration

Before running DATACOM in network mode, configure the server endpoints in `data/ports.toml`. Each entry maps a host to an array of ports; multiple ports are used for multiple concurrent streams:
```toml
[servers]
"localhost" = [8081, 8082, 8083]
"192.168.1.100" = [8082, 8083]  # Multiple ports supported
```

## Running the Client

To run a JSON scene with a set of behaviors:
```bash
cargo run -- scene.json
```

To run an HDF5 scene:
```bash
cargo run -- scene.hdf5
```

To receive and display a scene through live data streaming (network mode):
```bash
cargo run
```

To run against the built-in test server (spawns one server thread per JSON source and streams it to the client):
```bash
cargo run -- test scene_a.json scene_b.json
```

Append `y` as the final argument to record the session to a video file:
```bash
cargo run -- scene.json y
```

## Protocol Specification

DATACOM uses a custom binary protocol layered on UDP, with an application-level reliability layer (checksums, retransmission requests, and reassembly). All multi-byte values are transmitted in **Big Endian** format. Checksums are calculated using CRC32 (via `crc32fast`).

### Connection Handshake

The server binds a UDP socket to its configured address/port and waits. The client binds an ephemeral local UDP socket, `connect`s to the server address, and sends `"ACK"` as bytes. The server learns the client's address from the incoming datagram and begins transmitting. If the client's ACK bounces (server not yet bound), it is retried.

### Initial File Transfer

The initial transfer phase transmits static scene data (scene definitions, 3D models, etc.) before live streaming begins.

#### File Metadata Message

Initiates transfer for a new file:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x00` (FILE_START) |
| File ID | 16 bytes | Unique file identifier (UUID) |
| Name Length | 1 byte | Length of filename (max 255) |
| Filename | 255 bytes | UTF-8 encoded filename, zero-padded to the max width |
| File Length | 4 bytes | Total file size in bytes |
| Transfer Mode | 1 byte | `0x01` = finite length, `0x00` = indefinite stream |

#### File Data Chunk

Transmits actual file data (chunks may arrive in any order):

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x01` (FILE_CHUNK) |
| File ID | 16 bytes | Matches File ID from metadata |
| Chunk Offset | 8 bytes | Byte offset within file |
| Chunk Length | 4 bytes | Size of payload in this chunk |
| Payload | n bytes | Raw chunk data |
| Checksum | 4 bytes | CRC32 checksum of payload |

**Note**: Chunks for different files can be interleaved. The client reassembles files based on File ID and chunk offsets. If a chunk's CRC32 does not match, the client discards it and sends a `REQUEST_RETRANSMIT_CHUNK` for that offset.

#### Retransmit Request

Sent by the client when a chunk fails its checksum:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x03` (REQUEST_RETRANSMIT_CHUNK) |
| File ID | 16 bytes | ID of the file the bad chunk belongs to |
| Chunk Offset | 8 bytes | Byte offset of the chunk to resend |

#### File End Marker

Signals completion of a file transfer:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x02` (FILE_END) |
| File ID | 16 bytes | ID of completed file |

#### Transmission End Marker

Signals completion of the initial file transfer phase:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x04` (TRANSMISSION_END) |

Upon receiving `TRANSMISSION_END`, the client replies with a `TRANSMISSION_ACK` (`0x00 0x05`) so the server knows the client has all initial files.

#### Command Frame

Carries a global command rather than file data. Only honoured from the stream holding command
authority — see [Command Authority](#command-authority):

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x06` (COMMAND) |
| Command | 2 bytes | See the Command Reference below |

The command id is a separate field rather than its own message type so that adding a command costs
a value in that table instead of a top-level protocol code, and so a command that later takes
arguments has somewhere to put them. A command id this client does not recognise is logged and
ignored; the frame is a fixed four bytes either way, so an unknown one cannot desynchronise the
stream.

### Scene Definition Format

The scene JSON file defines the initial 3D environment and entities. Entities form a **hierarchical scene graph**: each entity has a `Children` array whose members are themselves entities (with their own transforms, meshes, behaviors, and children). Rotations are quaternions in `[w, x, y, z]` order. Example structure:
```json
{
  "authority": true,         // Optional: claim command authority (see below); defaults to false
  "namespace": "fleet_a",    // Optional: qualifies this scene's entity paths (see Entity Addressing)
  "viewports": [
    {
      "x": 0.0,
      "y": 0.0,
      "w": 1600.0,
      "h": 1200.0,
      "camera": {
        "position": [0.0, -5.0, 5.0],
        "rotation": [0.85355335, -0.3535534, 0.14644663, -0.3535534]
      },
      "border color": [0.0, 255.0, 0.0],
      "alignment": "FullScreen"
    },
    {
      "x": 1200.0,
      "y": 450.0,
      "w": 400.0,
      "h": 600.0,
      "camera": {
        "position": [2.7, -5.0, 0.0],
        "rotation": [1.0, 0.0, 0.0, 0.0],
        "stream": "camera_01.bin"  // Optional: drive this camera from a stream (see below)
      },
      "border color": [0.0, 0.0, 255.0],
      "alignment": "BottomRight"
    }
  ],
  "terrain": {
    "z_pos": 0.0,            // Optional: terrain height
    "width": 1000,           // Optional: terrain dimensions
    "color": [0.2, 0.5, 0.3] // Optional: RGB color
  },
  "total_timesteps": 1000,   // Placeholder value for animation steps
  "entities": [
    {
      "Name": "Drone_01",
      "Position": [0.0, 10.0, 0.0],    // [x, y, z] in world space, with +z = up
      "Rotation": [1.0, 0.0, 0.0, 0.0],// quaternion [w, x, y, z]
      "Scale": [1.0, 1.0, 1.0],
      "Behavior": {
        "behaviorType": "ChangeTransform",
        "data": ["entity_pos.bin"]
      },
      "Children": [
        {
          "Name": "DroneBody",
          "ObjectFilePath": "blizzard.obj",
          "Position": [0.0, 0.0, 0.0],    // Relative to parent
          "Rotation": [1.0, 0.0, 0.0, 0.0],
          "Color": [0.8, 0.8, 0.8]
        },
        {
          "Name": "Propeller",
          "ObjectFilePath": "prop.obj",
          "Position": [-0.72, -2.928, 1.191],
          "Rotation": [1.0, 0.0, 0.0, 0.0],
          "Color": [1.0, 0.0, 0.0],
          "Behavior": {
            "behaviorType": "RotateConstantSpeed",
            "data": [0.2, 0.0, 0.2, 0.0]
          }
        }
      ]
    }
  ]
}
```

An entity (or any child) may declare a single `Behavior`. A child with an `ObjectFilePath` is rendered as a mesh; a child without one is a pure transform node that groups its own children.

### Behavior Data Definition

The contents of the `data` field depend on `behaviorType`:
```json
{
  "behaviorType": "Translate",
  "data": [
    // x-offset
    // y-offset
    // z-offset
  ]
}
{
  "behaviorType": "Rotate",          // and "RotateConstantSpeed"
  "data": [
    // rotation speed (scalar factor applied each frame)
    // x-axis component
    // y-axis component
    // z-axis component
  ]
}
{
  "behaviorType": "ChangeTransform",
  "data": [
    // source file to stream further data from (eg "entity_pos.bin" or a ".hdf5" dataset).
    // May be followed by inline sets of 12 f32 transform values instead of / in addition to a file.
    // Each set of 12 values, consumed one per frame, is:
    //   x-position, y-position, z-position,
    //   x-linear velocity (unused), y-linear velocity (unused), z-linear velocity (unused),
    //   x-rotation, y-rotation, z-rotation,
    //   x-rotational velocity (unused), y-rotational velocity (unused), z-rotational velocity (unused)
  ]
}
{
  "behaviorType": "ChangeColor",
  "data": [
    // r, g, b
  ]
}
```

For `ChangeTransform`, one set of 12 transform values is consumed every frame — from a streamed `.bin` file (via the ring buffer), an HDF5 `states` dataset, or inline values. This enables entities to follow streamed trajectory data. The three rotation components are interpreted as the vector part of a unit quaternion, with the scalar part derived so the quaternion is normalized.

### Command Authority

Scene content is decentralized — every stream contributes entities, and no stream is privileged. Some
operations are inherently global, though: they affect entities that other streams contributed. A
server opts into issuing those by setting a top-level `"authority": true` in its scene JSON.

The rules are deliberately minimal:

- **Declared, not elected.** Authority is never inferred from connection order, scene size, or any
  other race. A stream has it only if it asked for it.
- **Claimed once per run, first declarer wins.** A second stream that declares is logged as a
  warning and does not receive authority; its global commands are ignored from that point on.
- **Granted whenever the declaring stream arrives.** An operator console that connects after the
  drones are already streaming still receives authority when its scene is assembled.
- **Absent by default.** A peer-to-peer fleet declares nothing, so no stream holds authority and
  global commands are inert. This is the intended behavior for that deployment, not a degraded mode
  — there are no separate centralized and decentralized modes to configure.

Authority and base-scene ownership are independent. The first scene to finish assembling still
defines viewports and terrain regardless of who holds authority, so a late-arriving operator takes
command authority without redefining the environment.

Commands are only acted on once streaming is under way. During the initial file transfer the client
is still assembling the scene and object files the renderer is built from, so there is nothing for a
command to act on and any that arrives is logged and dropped. Precisely, that phase ends when the
*first* stream signals `TRANSMISSION_END` — not when the window opens — so a command sent during the
window and renderer setup that follows is honoured normally. A server should send commands only
after its own `TRANSMISSION_ACK`, which is comfortably inside the live phase.

A clear takes effect when the client processes it, not when the server sent it. A stream whose scene
finishes assembling in the same moment may land on either side of it; one that lands just before is
merged and then wiped. The server is not told, and a scene is sent only once per stream, so the
count in the client's `cleared the scene (N entities removed)` log line is the only record that it
happened.

#### Issuing a clear from the test server

The built-in test server can send one, so the path can be exercised without an external operator.
Give the scene it transmits a top-level `"authority": true` and set the delay, in whole seconds:

```bash
DATACOM_TEST_CLEAR_AFTER_SECS=10 cargo run -- test scene_a.json n
```

The delay is measured from that server's own `TRANSMISSION_END` rather than from startup, so the
command lands in the live phase however long the initial transfer took. Each server checks its own
scene first and logs — rather than sends — when that scene does not declare authority, since the
client would ignore the command and the reason belongs on the side that has to change. It is one
command per run: a scene is sent once per stream, so nothing repopulates what the clear removed.

The trigger is an environment variable because test mode's arguments are already fully spoken for —
the trailing one is the record-video flag and the rest are the JSON list.

Note that this is a coordination mechanism, not a security one: the wire protocol is unauthenticated,
so authority prevents conflicting or accidental global commands, not hostile ones.

### Entity Addressing

A command that acts on one entity has to name it, and the name has to mean the same thing on both
ends. Entities are addressed by **path** — slash-separated segments running from the namespace,
through the root entity, down the scene graph:

```
fleet_a/Drone_02                     // a root
fleet_a/Drone_02/propeller RRT       // one of its children
Drone_02                             // a run where no namespace was declared
```

The identifier is **authored by the server**, not generated by the client and reported back. A
server names its own entities in the scene JSON it sends, so it can command them immediately —
there is no discovery step, nothing to resynchronise when a stream joins late, and no client→server
message needed. A client-generated id would be unique but meaningless to the server that has to use
it: knowing three UUIDs does not tell you which one is the drone in the middle.

Uniqueness comes from **namespaces**, declared per scene with a top-level `"namespace"`. Every
entity from that scene is addressed under it, so two fleets that both name a drone `Drone_02` stay
distinct without their servers having to coordinate. Declare one in any run where more than one
server contributes entities.

Four collisions are possible, and all four are resolved the same way — the client appends `#N` and
logs a warning naming the fix. Nothing is ever dropped, and no two entities ever share a path:

| Collision | Result |
|---|---|
| Two scenes claim one namespace | First declarer keeps it; the second is addressed under `fleet_a#1` |
| Two scenes declare no namespace | The first keeps bare names; the second is addressed under `#1` |
| Two siblings share a name | *Every* member of the group is suffixed: `propeller#0`, `propeller#1` |
| An entity has no `Name` | Addressed by its index in the array that declared it: `#3` |

Two details worth knowing:

- **All duplicate siblings are suffixed, including the first.** Letting one keep the bare name would
  let a command resolve to an arbitrary member of the group, and a command that quietly hits the
  wrong drone is worse than one that visibly hits nothing.
- **An unnamed entity's index is the declaring server's own array index**, not its position in the
  merged scene. A server can predict the former; nobody can predict the latter, since it depends on
  which stream wins the arrival race.

Sibling names only have to be unique among siblings — every drone in a fleet having a
`propeller RRT` is fine, since the parent segment separates them. `/` is reserved as the separator
and is substituted out of any name or namespace containing it.

Paths are resolved against the scene as it stands when the command arrives. A `CLEAR_SCENE` releases
every namespace along with the entities it addressed, so a fleet that reconnects afterwards gets its
original namespace back rather than being pushed aside by its own earlier claim. Run with
`RUST_LOG=debug` to log every addressable path at load and after each merge.

Note that the `COMMAND` frame has no room for a target yet — it is a fixed four bytes. Carrying a
path is the next protocol change; this section describes the names it will carry.

### Live Data Streaming

After the initial file transfer completes, the server can stream indefinite live data.

**Differences from the initial transfer:**
- File metadata `Transfer Mode` byte is `0x00` (indefinite)
- `File Length` field is still present but ignored
- Chunks stream continuously without a FILE_END marker
- The same FILE_CHUNK format is used
- Chunks are written into a per-stream ring buffer; out-of-order chunks are held in a reorder buffer keyed by offset and flushed in order

### Stream-Driven Cameras

A viewport camera can be driven by a stream instead of by the user, by naming a source in its
`camera` block:

```json
"camera": {
  "position": [0.0, -5.0, 5.0],
  "rotation": [1.0, 0.0, 0.0, 0.0],
  "stream": "chase_cam.bin"
}
```

The frame layout is identical to a `ChangeTransform` behavior's — 12 f32, big-endian, with
position in slots 0-2 and the quaternion's vector part in slots 6-8 — and the name is bound in the
same registry, so no protocol or server change is needed to feed one. A name the server never
sends fails the same silent way an entity's does: the camera simply never moves.

Notes:
- The mode is **locked**. `Enter` will not switch a stream-driven camera into or out of it, and
  keyboard and mouse input are inert on it. Clicking it still takes focus, and says so.
- `position` and `rotation` still apply until the first frame arrives.
- A starved stream **holds the last pose** rather than resetting, matching how entities stall.
- Camera streams do not count toward the run's completion, so a quiet camera will neither hold
  the session open nor end it early.
- Two viewports naming the *same* source each consume from one buffer and will halve each other's
  frame rate. Give each its own name.
- Only the first scene to arrive defines viewports, so in a multi-source run the declaration must
  be in whichever scene wins that race — or, more simply, in all of them.

## Message Type Reference

| Type | Code | Description |
|------|------|-------------|
| FILE_START | `0x00 0x00` | Begin file transfer (metadata) |
| FILE_CHUNK | `0x00 0x01` | File data chunk |
| FILE_END | `0x00 0x02` | File transfer complete |
| REQUEST_RETRANSMIT_CHUNK | `0x00 0x03` | Request a failed chunk be resent |
| TRANSMISSION_END | `0x00 0x04` | Initial transfer phase complete |
| TRANSMISSION_ACK | `0x00 0x05` | Client acknowledges TRANSMISSION_END |
| COMMAND | `0x00 0x06` | A global command; see Command Frame |

### Command Reference

| Command | Code | Description |
|---------|------|-------------|
| CLEAR_SCENE | `0x00 0x00` | Remove every entity and the buffers feeding them |

## Architecture
```
Client (DATACOM)          Server (Data Source)
     |                           |
     |------- Send ACK --------->|   (UDP; server learns client addr)
     |                           |
     |<--- FILE_START (Scene)----|
     |<--- FILE_CHUNK -----------|
     |--- RETRANSMIT (if bad) -->|
     |<--- FILE_CHUNK (resend)---|
     |<--- FILE_END -------------|
     |                           |
     |<--- FILE_START (Model)----|
     |<--- FILE_CHUNK -----------|
     |<--- FILE_END -------------|
     |                           |
     |<--- TRANSMISSION_END -----|
     |--- TRANSMISSION_ACK ----->|
     |                           |
     | [Construct Scene]         |
     |                           |
     |<--- Live Data Chunks -----|
     |<--- Live Data Chunks -----|
     |         ...               |
```

Multiple servers can run this exchange concurrently on separate ports; the client spawns a listener/sender/assembly thread set per connection and merges every stream's entities into a single scene.

## License

GPL-3.0 License

## Technical Stack

- **Language**: Rust
- **Graphics**: wgpu 25 (based on WebGPU), winit
- **Shading**: WGSL (WebGPU Shading Language)
- **Networking**: std::net (UDP)
- **Math**: cgmath, nalgebra
- **Mesh loading**: tobj (OBJ)
- **Scientific data**: hdf5-metno, ndarray
- **Text/Images**: rusttype, image
- **Checksum / IDs**: crc32fast, uuid

---

For questions or collaboration inquiries, please open an issue.
