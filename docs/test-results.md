# Test results: mxl-test VM, 2026-11-24

VM `mxl-test` (Infomaniak a16-ram32: 16 vCPU AMD EPYC, no GPU), Ubuntu 24.04, next to
mxl-stack's single-host example (multiviewer, replay, WebRTC monitor, lm-mxl-mixer, ...),
which alone keeps the host at about 40% of 16 vCPU. lm-mxl-omt `lm-mxl-omt:dev` built on
the VM, 8 OMT inputs and 8 OMT outputs, 1080p50, `OMT_OUT_PIXEL=uyvy` unless noted. All
OMT traffic stays on the host (loopback TCP). Numbers from the bridge's `/api/status`
(`scripts/` on the VM: `scale.py`, `soak.py`), mxl-avsync 2.2.0 and lm-mxl-mixer's
`mxl-grab`.

## 1. Loopback: test player → OMT out → OMT in → MXL

Test player `Out 1` (MXL) → `OMT Out 1` → OMT (VMX, TCP) → `OMT In 1` → multiviewer
`MV In 9` and vision mixer `VM In 4`.

| | |
|---|---|
| Frames | 50.0 fps in and out, 0 dropped / repeated / black, 0 late at the output |
| End-to-end latency, MXL grain time to MXL grain time | **1 grain (20 ms)**: source grain N comes out as grain N+1 (multiviewer timecode 1 frame behind the source) |
| Of which OMT (output read → OMT input arrival, `transit`) | 25 ms mean, 26–31 ms max per second; that is the 20 ms output read offset plus ~5 ms of convert, VMX encode, TCP, VMX decode, convert |
| A/V sync (mxl-avsync generator → OMT Out 3 → OMT In 3 → analyzer, 120 events) | **−0.008 ms**, identical to the generator routed straight into the analyzer (−0.008 ms). Constant, 0 resyncs |
| Picture | 8-bit (UYVY): 10-bit values within ±2 of the source. 10-bit (P216): identical to the source on the test card |
| Codec time per frame | VMX encode 1.4 ms, decode 1.0 ms (libomt statistics) |
| Conversion per frame | v210 → UYVY 0.23 ms (AVX2), UYVY → v210 0.56 ms, v210 → P216 1.6 ms, P216 → v210 2.0 ms |

## 2. Scale: N streams each way (N outputs and N inputs, looped)

30 s windows after 20 s of settling. CPU in % of one core.

| Streams each way | Bridge CPU | Host CPU (16 vCPU) | Input thread | Output thread | Dropped / repeated / late / audio gaps | Latency | Transit mean / max | Video per stream | Audio per stream |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 42% | 40.3% | 17% | 10% | 0 / 0 / 0 / 0 | 20 ms | 25.0 / 26.4 ms | 39 Mbit/s | 24.6 Mbit/s (16 ch) |
| 4 | 112% | 45.3% | 15% | 12% | 0 / 0 / 0 / 0 | 20–40 ms¹ | 26.3 / 29.1 ms | 44 Mbit/s | 19 Mbit/s |
| 8 | 202% | 50.8% | 14% | 11% | 0 / 0 / 0 / 0 | 20 ms | 26.4 / 30.6 ms | 41 Mbit/s | 22 Mbit/s |
| 8, P216 (10-bit) | 345% | 63.1% | 23% | 19% | 0 / 0 / 0 / 0 | 20–40 ms² | 28.7 / 34 ms | 44 Mbit/s | 19 Mbit/s |

So about **25% of one core per 1080p50 stream pair** (one in, one out) in 8-bit, 42% in
10-bit; the threads `in-n` / `out-n` carry nearly all of it (VMX runs in the calling
thread). Idle inputs (no source: black and silence written) cost ~2% each; outputs
without receivers send nothing.

¹ Before the latency trim fix: inputs that picked their delay during the start-up burst
stayed at 2 grains. Fixed (trim whenever a grain can be saved); the 8-stream run after
the fix has all 8 at 20 ms.
² With P216 and more load, frames of 6 of 8 inputs sometimes take > 40 ms, so their delay
stays at 2 grains (by design: the delay covers the slowest frame in 2 s).

Bandwidth: the test card compresses to ~40 Mbit/s per 1080p50 stream. A real picture
(omtsendtest's photo with a moving line, 1080p60) needs **205 Mbit/s**. Audio is
uncompressed float: 1.5 Mbit/s per channel (the test player has 16 channels: 24.6 Mbit/s).

Soak (8 + 8, 10 min): see the end.

## 3. Interop with libomt's own examples

Built in the image from openmediatransport/Examples (`omtsendtest`, `omtrecvtest`), run
headless in the bridge's container (sharing its avahi-daemon):

- **omtsendtest → OMT In 4**: 1080p60 UYVY, sender-paced timestamps (`-1`), 2 ch noise
  audio in 800-sample blocks. Discovered as `MXL-TEST (Test)`, written as 1080p50: exactly
  10 frames/s dropped (60 → 50 frame sync), 0 repeated, 0 resyncs, 0 audio resyncs in
  30 s; 205 Mbit/s, decode 3.4 ms/frame. Picture correct (photo, colours) in the flow.
- **OMT Out 5 → omtrecvtest → `MXL-TEST (OMLoopBack)` → OMT In 5**: omtrecvtest decodes
  and re-encodes. 50 fps, 0 dropped, A/V and timestamps preserved; latency 2 grains
  (its re-encode path prints every frame and has transit spikes > 40 ms).
- **Source loss and reconnect**: omtsendtest stops after 10000 frames. The input held the
  last frame for 48 grains (`OMT_IN_HOLD_MS` 1000), then black and silence, the flow kept
  running at 50 fps. Restarted, it was back (state ok) after 1.2 s, including its start.

## 4. NMOS routing

- `route.py "Out 1 Video=OMT Out 1 Video"` etc.: the bridge's IS-05 receivers take MXL
  routes from the test player, the mxl-avsync generator and the vision mixer.
- `OMT In 1 → MV In 9` (and audio), `OMT In 4 → MV In 10`, `OMT In 1 → VM In 4`: the
  multiviewer shows the tile with audio meters; lm-mxl-mixer reads it as an input.
- `VM PGM Video/Audio → OMT Out 6`, OMT In 6 (`MXL-TEST (OMT Out 6)`) → `MV In 11`: the
  mixer's programme (keyed browser source and PiP) comes back through OMT intact.
- Restart of the bridge: IS-05 routes and input sources restored, all inputs back at
  50 fps within seconds, the multiviewer recovered by itself.

## Soak: 8 + 8 streams, 10 minutes

All 8 outputs looped into the 8 inputs (test player, mxl-avsync generator), 1080p50, UYVY:

| | |
|---|---|
| Grains written by the inputs | 240,080 (8 × 50 × 600 = 240,000) |
| Dropped / repeated / black / resyncs / audio resyncs / silence | 0 / 0 / 0 / 0 / 0 / 0 |
| Frames sent by the outputs | 240,080; late 0, held 0, skipped 0, audio gaps 0 |
| Bridge CPU | 208% of one core on average, 244% at most |
| Host CPU | 54.3% of 16 vCPU (stack alone ≈ 40%) |
| Latency per input | 20 ms for 6 inputs, 40 ms for 2 (a frame > 40 ms after its source grain early on); transit max 31.5 ms in the last second |
