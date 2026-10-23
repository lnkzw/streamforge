# ADR 0017: Audio routing and output profiles

- **Status:** Accepted (reviewed with the project owner on 2026-10-23)
- **Date:** 2026-10-23

## Context

Today the audio is one mix (ADR 0004). Live inputs and test tones are strips with gain and mute, summed into a master with gain and a hard limit, and every output receives that same block. The mixer has a single `BlockSink`, and the engine's `AudioTap` hands the master block to each recording or stream.

The architecture vision (§6) and the functional requirements ask for more: inputs, channels, buses, sends and outputs as separate concepts, several independent mixes, defined pre/post-fader semantics, track mapping per output, a monitor route kept apart from program outputs, and routing that persists and migrates. "Separate recording tracks are not a routing model."

The project owner decided to build this before the first release (roadmap M11), so the protocol is frozen after the audio model and the M12 mixer shows the real graph. Effects come later, in slots this design reserves.

## The model

Five concepts, each with a stable id and its own commands:

| Concept | What it is | Id |
|---|---|---|
| **Input** | Where sound comes from: a capture device or a generator (test tone). Later: application audio, files, network. | part of its channel |
| **Channel** | One input's strip: processing, mute, fader, balance, meter, and its sends. | `ch_…` |
| **Bus** | A sum of sends: a submix (Voice, Game, Music) or a mix for a destination (Monitor). Has processing, mute, fader, meter. | `bus_…`; the program bus is `bus_program` |
| **Send** | A connection from a channel to a bus: on/off, a level, and its tap point (pre- or post-fader). | (channel, bus) |
| **Output** | Where a mix goes: the recording, the stream, the monitor device. Each takes a bus as its mix; a recording can add more buses as extra tracks. | `recording`, `stream`, `monitor` |

### Signal flow

```
 channel:  input ─► processing slots ─► mute ─┬─► fader ─► balance ─┬─► meter
                                              │                     │
                                       pre-fader tap          post-fader tap
                                              │                     │
                                   sends (each: on/off · level · tap) ─► buses

 bus:      Σ sends ─► processing slots ─► mute ─► fader ─► meter ─┬─► assign to program (aux buses)
                                                                  └─► outputs that use it

 output:   bus ─► limiter (counted) ─► encoder track / device
```

- **Taps.** The pre-fader tap is after processing and mute, before fader and balance. The post-fader tap is after both. Mute comes before both taps, so a muted channel is silent everywhere: in every bus, every output and the headphones. That is what a cough button must do. A source that must stay audible on one path and not on another is routed with sends, not with mute.
- **Default routing.** A new channel sends to the program bus, post-fader, at 0 dB. With no other buses, the graph sounds exactly like today's mixer.
- **Two tiers of buses, acyclic by construction.** Aux buses take sends from channels. An aux bus can be assigned to the program bus, with a level, as a submix (Voice → Program), or stay a separate mix, such as a monitor mix. The program bus feeds no bus. There is no bus-to-bus routing beyond that, so no cycle can be built and nothing needs checking at run time.
- **Balance, not pan.** Every channel is stereo, so the control is a balance: centre is unity on both sides, and moving it attenuates the other side with an equal-power curve. Mono sources come in as two equal channels from the capture backend, as today.
- **Processing slots.** Each channel and bus gets a fixed number of slots (4) in the places shown, all empty in M11. They fix where an effect sits relative to the taps before any effect exists. An effect added later (EQ, gate, compressor, ducking with a side-chain from another channel) fills a slot and changes no API.
- **Limits.** Summing stays in `f32` without intermediate clipping. The hard limit to [−1, 1] moves from "the master" to every point where a bus leaves the graph for an output, and is counted per output. Buses and channels report their peaks.

### Outputs and output profiles

An **output profile** describes an output, so that one command starts it and the project file keeps it:

| Output | Profile |
|---|---|
| `recording` | mix bus (track 1), extra tracks (buses, in order), video and audio encoder settings, file name pattern |
| `stream` | mix bus (one track; FLV/RTMP carry one audio track), encoder and bitrate settings, the URL. The key is never stored; it comes from the client, the environment or later the OS keychain |
| `monitor` | output device, mix bus (by default the program bus; usually a "Monitor" bus with pre-fader sends) |

- **Profiles are settings, not state.** `start_recording` and `start_streaming` use the profile, and their existing parameters override it for one run, as today.
- **A recording's tracks.** Track 1 is its mix, and the further tracks are the listed buses, each as its own AAC stream in the Matroska file. Each track is named after its bus, and the report has one entry per track. Isolating a source on its own track means routing it to its own bus. The track list is fixed when the recording starts.
- **The monitor** plays a bus on an audio device while the pipeline runs. It is not an encoder output, cannot be "recorded", and its failures never touch the program outputs. Its latency is reported.

## Real-time design

- **Fixed for a run.** The mixer thread builds its graph when the pipeline starts, for the configured channels, buses and outputs: scratch blocks, the send matrix (channels × buses), meters. No allocation happens in the block loop.
- **Live parameters.** Gain, mute, balance, send on/off and level, send tap, bus assignment and the bus an output listens to all go through the existing preallocated pending slot (try-locked once per block), and are ramped over one block like gains today, so nothing clicks.
- **Structure only while idle in M11.** Adding or removing a channel or bus, and the device of a channel or of the monitor, need an idle pipeline, as `set_audio_sources` does today. The capacity is fixed (below), so adding at run time can come later without an API change: it fills a slot that already exists.
- **Capacity.** 8 channels (today's `MAX_INPUTS`), 8 aux buses plus the program bus, and 8 recording tracks. A full matrix is 72 send points per block of 480 frames: multiply-adds that are small against the 10 ms a block lasts; the bench measures it before and after.
- **Fan-out.** The engine's tap hands each output the block of its bus (and a recording one block per track), stamped with the same time. Queues stay bounded, and drops stay counted per output and per track.
- **Clocks.** All channels and buses share the mixer clock, so there is no latency difference inside the graph: nothing in it adds delay until effects with look-ahead exist, and those will report their latency. Capture drift is absorbed per input as today. The monitor device has its own clock. Its feed is a bounded buffer with the same adaptive correction as inputs, and underruns and trims are counted.
- **Feedback.** The graph cannot loop. A loop through the room or the system is still possible, for example when an input captures the monitor device's own output ("monitor of" a sink on PipeWire). When an input is the loopback of the monitor device, the engine warns, and it never routes such a loop silently.

## Commands and persistence

- **New commands.** Channel, bus, send and output-profile commands with the usual id-or-name addressing (ADR 0014):
  - `add_channel` and `remove_channel`, `update_channel`
  - `add_bus` and `remove_bus`, `update_bus`
  - `set_send`
  - `set_output_profile`
  - `list_audio_outputs`, for the monitor devices
- **Removed commands.** They replace `set_audio_sources`, `set_mixer_strip` (by index) and `set_master_gain`. Those are breaking changes, allowed and documented before the first release.
- **Access.** Adding a channel with a capture device and choosing the monitor device are `sensitive`, like microphones today. Fader, mute, balance and send changes are `control`.
- **`get_mixer`.** It returns the graph (channels, buses, sends, output mixes) with ids, settings and levels.
- **`get_health`.** It gains per-bus and per-output-feed counters (limited samples, drops), and the monitor's buffer state.
- **Project file.** Schema version 2 stores the graph and the output profiles. Version 1 files are migrated on load: inputs and tones become channels, each sending to the program bus post-fader, and the master gain becomes the program bus fader. `validate_project` reports a migration as such, and saving writes version 2.

## Alternatives considered

1. **Tracks without buses** (one program mix plus isolated source tracks): simpler, but it cannot express Voice/Game submixes, a monitor mix that differs from the stream, or per-output mixes. The vision rules it out explicitly.
2. **A general DAG** (any bus into any bus): more expressive, but it needs cycle detection on every change and a topological order that changes at run time, and it is harder to show. Two tiers cover the use cases (submixes into program, separate mixes for outputs) and cannot loop.
3. **Mute after the pre-fader tap** (a muted channel still feeds monitor mixes): this is how some consoles work, but a streamer's mute must be total. If needed, a per-send mute gives the other behaviour later without changing this one.
4. **Live structural changes now:** this would need off-thread construction of inputs and hand-over to the running graph. The fixed capacity keeps that possible later.
5. **One encoder per bus, shared by outputs:** this saves an AAC encode when two outputs use the same mix. It couples the outputs and their failures (ADR 0006), and AAC at 48 kHz stereo is cheap. Not now.

## Validation (planned)

- **`audio-engine` unit tests:** summing with sends and assigns; pre- and post-fader taps, including mute before both taps; balance law; ramps; an empty graph; a full graph; a limit per output feed; meters per channel and bus. Each is bit-exact where the operations allow.
- **Engine tests:**
  - commands and their idle-only rules
  - a v1 project migrated to v2 sounds the same (same blocks for a tone)
  - per-output mixes: a stream and a recording with different buses carry different content
  - a recording with three tracks decodes with three AAC streams of the expected content and length
- **Bench:** the added cost of a full graph (8 channels × 8 buses, 3 outputs, 4 tracks) against today's mixer.
- **Monitor:** the playback path is tested natively where a device exists (the development machine). On CI, a test device or the error path is used.

## Implementation steps (one PR each)

1. **The graph in `audio-engine`:** channels, buses, sends, taps, balance, slots and limits, replacing `Mixer`. Pure and tested, with the bench.
2. **Engine and commands:** ids, the new commands, `get_mixer`, health, project file v2 with migration. Outputs still use the program bus.
3. **Per-output mixes:** the mix bus per output and the fan-out per bus; output profiles in the project file.
4. **Multitrack recording:** several AAC tracks in `encoder-ffmpeg`, reports per track.
5. **The monitor output:** a playback backend (Linux through PipeWire first; Windows and macOS through cpal, behind the platform traits), device listing, drift handling, the feedback warning.

## Decisions taken with the project owner

1. **Mute before both taps:** a muted channel is silent everywhere, on the headphones too.
2. **Two tiers of buses:** aux buses take sends and can be assigned to the program bus; there is no other bus-to-bus routing.
3. **Structure changes while idle only in M11:** channels, buses and devices are added or removed only while the pipeline is idle. Parameters (gains, mutes, balance, sends, assigns, output mix choice) change live, and the reserved capacity keeps live structure changes possible later.
