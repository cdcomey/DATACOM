# DATACOM

**Source-Agnostic 3D Data Visualization Engine**

DATACOM is a general-purpose 3D visualization engine built with Rust and OpenGL, designed to serve as a visual terminal interface for real-time data streaming applications. Originally developed as a command-and-control interface for UAV swarm behavior, DATACOM has evolved into a flexible, source-agnostic platform capable of receiving and displaying multiple concurrent data streams.

## Features

- **Source-Agnostic Design**: Accept data from any source with proper formatting
- **Multi-Stream Support**: Display multiple incoming data streams simultaneously
- **Real-Time Visualization**: Stream and visualize live data with low latency
- **3D Rendering**: OpenGL-based rendering engine with support for complex scenes
- **Flexible Entity System**: Dynamic entity management with customizable behaviors

## Prerequisites

- Rust (latest stable version)
- WebGPU-compatible graphics hardware

## Installation

Clone the repository:
```bash
git clone https://github.com/cdcomey/DATACOM.git
cd DATACOM
```

## Configuration

Before running DATACOM, configure the server endpoints in `data/ports.toml`:
```toml
[servers]
"107.25.30.219" = [8081]
"192.168.1.100" = [8082, 8083]  # Multiple ports supported
```

## Running the Client
To run a JSON scene with a simple set of behaviors:
```bash
cargo run -- scene.json
```

To run an HDF5 scene:
```bash
cargo run -- scene.hdf5
```

To receive and display a scene through data streaming:
```bash
cargo run
```

## Protocol Specification

DATACOM uses a custom binary protocol over TCP for efficient data transfer. All multi-byte values must be transmitted in **Big Endian** format. Checksums are calculated using CRC32 (via `crc32fast`).

### Connection Handshake

Upon running, the client will check each available IP address and port until a server awaiting on one is found. The client will then send "ACK" as bytes.

### Initial File Transfer

The initial transfer phase transmits static scene data (scene definitions, 3D models, etc.) before live streaming begins.

#### File Metadata Chunk

Initiates transfer for a new file:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x00` (FILE_START) |
| File ID | 8 bytes | Unique file identifier |
| Name Length | 1 byte | Length of filename (max 255) |
| Filename | n bytes | UTF-8 encoded filename |
| File Length | 4 bytes | Total file size in bytes |
| Transfer Mode | 1 byte | `0x01` = finite length, `0x00` = indefinite stream |

#### File Data Chunk

Transmits actual file data (chunks may arrive in any order):

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x01` (FILE_CHUNK) |
| File ID | 8 bytes | Matches File ID from metadata |
| Chunk Offset | 8 bytes | Byte offset within file |
| Chunk Length | 4 bytes | Size of payload in this chunk |
| Payload | n bytes | Raw chunk data |
| Checksum | 4 bytes | CRC32 checksum of payload |

**Note**: Chunks for different files can be interleaved. The client will reassemble files based on File ID and chunk offsets.

#### File End Marker

Signals completion of a file transfer:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x02` (FILE_END) |
| File ID | 8 bytes | ID of completed file |

#### Transmission End Marker

Signals completion of initial file transfer phase:

| Field | Size | Description |
|-------|------|-------------|
| Message Type | 2 bytes | `0x00 0x04` (TRANSMISSION_END) |

### Scene Definition Format

The scene JSON file defines the initial 3D environment and entities. Example structure:
```json
{
  "terrain": {
    "z_pos": 0.0,           // Optional: terrain height
    "width": 1000,          // Optional: terrain dimensions
    "color": [0.2, 0.5, 0.3] // Optional: RGB color
  },
  "timesteps": 1000,        // Placeholder value for animation steps
  "entities": [
    {
      "Name": "Drone_01",
      "Position": [0.0, 10.0, 0.0],    // [x, y, z] in world space, with +z = up
      "Rotation": [0.0, 0.0, 0.0],     // [pitch, yaw, roll] in degrees
      "Scale": [1.0, 1.0, 1.0],
      "Models": [
        {
          "Name": "DroneBody",
          "ObjectFilePath": "blizzard.obj",
          "Position": [0.0, 0.0, 0.0],     // Relative to entity
          "Rotation": [0.0, 0.0, 0.0],
          "Color": [0.8, 0.8, 0.8]
        }
      ],
      "Behaviors": [
        {
          "behaviorType": "EntityChangeTransform",
          "data": [
            ...
          ]
        }
      ]
    }
  ]
}
```

### Behavior Data Definition
The contents of the "data" field in each Behavior in the JSON depends on the behavior type. They are as follows:
```json
{
    "behaviorType": "EntityTranslate",
    "data": [
        // x-offset
        // y-offset
        // z-offset
    ]
}
{
    "behaviorType": "EntityChangeTransform",
    "data": [
        // binary file to write further data to (eg "entity_01_transform.bin")
        // x-position
        // y-position
        // z-position
        // x-linear velocity (unused)
        // y-linear velocity (unused)
        // z-linear velocity (unused)
        // x-rotation
        // y-rotation
        // z-rotation
        // x-rotational velocity (unused)
        // y-rotational velocity (unused)
        // z-rotational velocity (unused)
    ]
}
{
    "behaviorType": "ComponentRotateConstantSpeed",
    "data": [
        // model ID
        // rotation velocity
        // x-rotation
        // y-rotation
        // z-rotation
    ]
}
```

For "EntityChangeTransform", multiple sets of 12 transform values can be included. One set of values will be consumed every frame. This enables entities to follow streamed transform data.

### Live Data Streaming

After initial file transfer completes, the server can stream indefinite live data.

**Differences from initial transfer:**
- File metadata `Transfer Mode` byte is `0x00` (indefinite)
- `File Length` field is still present but ignored
- Chunks stream continuously without a FILE_END marker
- Use same FILE_CHUNK format as initial transfer

## Message Type Reference

| Type | Code | Description |
|------|------|-------------|
| FILE_START | `0x00 0x00` | Begin file transfer (metadata) |
| FILE_CHUNK | `0x00 0x01` | File data chunk |
| FILE_END | `0x00 0x02` | File transfer complete |
| TRANSMISSION_END | `0x00 0x04` | Initial transfer phase complete |

## Architecture
```
Client (DATACOM)          Server (Data Source)
     |                           |
     |<------ TCP Connect -------|
     |                           |
     |------- Send ACK --------->|
     |                           |
     |<--- FILE_START (Scene)----|
     |<--- FILE_CHUNK -----------|
     |<--- FILE_CHUNK -----------|
     |<--- FILE_END -------------|
     |                           |
     |<--- FILE_START (Model)----|
     |<--- FILE_CHUNK -----------|
     |<--- FILE_END -------------|
     |                           |
     |<--- TRANSMISSION_END -----|
     |                           |
     | [Construct Scene]         |
     |                           |
     |<--- Live Data Chunks -----|
     |<--- Live Data Chunks -----|
     |         ...               |
```

## Development Status

This project is currently under active development and is not intended for public use. The repository has been made temporarily public for applications.

## License

GPL-3.0 License

## Technical Stack

- **Language**: Rust
- **Graphics**: wgpu (based on WebGPU)
- **Shading**: WGSL (WebGPU Shading Language)
- **Networking**: std::net (TCP)
- **Checksum**: crc32fast

---

**Note**: This is specialized software developed for research and UAV visualization applications. For questions or collaboration inquiries, please open an issue.