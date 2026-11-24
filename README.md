# lm-mxl-omt

A bridge between **MXL** (the Media eXchange Layer shared-memory data plane of
the EBU / Linux Foundation Dynamic Media Facility) and **OMT**
([Open Media Transport](https://github.com/openmediatransport), the open,
low-latency, VMX-compressed LAN video protocol), modelled on
[mxl-decklink](https://github.com/LeeO86/mxl-decklink): where mxl-decklink
bridges MXL and SDI cards, this bridges MXL and OMT sources on the network.
It runs next to the [mxl-stack](https://github.com/LeeO86/mxl-stack)
functions and is routed with NMOS IS-04/IS-05 like them.

- **OMT in → MXL.** Each OMT input connects to an OMT source (found by OMT
  discovery, DNS-SD; or `omt://host:port`), and writes a `video/v210` flow in
  the house format (e.g. 1080p50) and an `audio/float32` 48 kHz flow,
  TAI-locked. The flows always exist: black and silence without a source.
- **MXL → OMT out.** Each OMT output reads an MXL video and audio flow
  (routed with IS-05) and publishes them as a named OMT source
  (`HOST (OMT Out 1)`).
- **NMOS** (BCP-007-03, the `mxl-nmos` crate as in lm-mxl-mixer): one node and
  device; per OMT input an IS-04 video and audio **sender**, per OMT output an
  IS-05 video and audio **receiver**. The crosspoint or `route.py` routes them.
- Status web page and JSON (`/api/status`): per input and output the state,
  fps, dropped / repeated / black frames, latency, OMT bandwidth, codec
  times; per-thread CPU. `/metrics` for Prometheus.

Rust; libomt is used through its C API (`libomt.h`), loaded at run time.

## How it works

```
 OMT source ──TCP/VMX──► libomt receive ──UYVY/P216──► v210 ──► MXL video flow  (grain = ts + D)
                              (decode)                       └─► MXL audio flow  (sample = ts + D)

 MXL video flow ──v210──► UYVY or P216 ──► libomt send (VMX encode) ──TCP──► OMT receivers
 MXL audio flow ──float32──► FPA1 planar ─┘   stamped with the grain's TAI time
```

**Formats.** v210 and UYVY have the same sample order, so v210 ⇄ UYVY is a
repack and a shift; P216 (16-bit planar 4:2:2) carries all 10 bits, and OMT
encodes it as high-bit-depth VMX (`OMT_OUT_PIXEL=p216`). Inputs ask libomt for
`UYVYorUYVAorP216orPA16`: 8-bit sources arrive as UYVY, high-bit-depth ones
as P216, both written without loss. A source of another size is scaled
(bilinear, 10-bit planar; `OMT_IN_SCALE=false` refuses it instead and writes
black with an error). Another frame rate is frame-synchronised (frames
dropped or repeated, counted). Interlaced sources are written as progressive
frames (flagged). Audio must be 48 kHz; channels beyond `AUDIO_CHANNELS` are
dropped, missing ones silent.

**Timing (OMT in).** OMT frames carry the sender's timestamp. At the first
frame an input picks a delay `D` = arrival − timestamp (TAI) + `OMT_IN_DELAY_MS`;
then a frame stamped `ts` goes to the grain whose period holds `ts + D`, its
audio to the sample index of `ts + D`. So A/V keep the sync they had at the
sender and network jitter doesn't move frames between grains. `D` is picked
again (a *resync*) when frames keep arriving a grain late, when a 2 s window
shows it could be a grain shorter, or when the sender's clock jumps.

**Timing (OMT out).** Per grain `N`, at TAI `t(N)`, the output reads the
grain of `N − OMT_OUT_READ_OFFSET_GRAINS` (default 1), sends it stamped with
that grain's TAI time, then that grain period's audio. Another lm-mxl-omt
receiving it therefore reports the true end-to-end latency (`latency_ms`:
destination grain time − source grain time) and the network/codec part
(`transit_ms`).

**Source loss (OMT in).** Grains nobody wrote are filled half a grain after
they are due: with the last frame for `OMT_IN_HOLD_MS` (`OMT_IN_LOSS=hold`),
then black, like mxl-decklink's signal-loss standby; silence when no audio
came for 100 ms. libomt reconnects by itself; when the source is back the
input picks its delay again.

**Stalled MXL flows (OMT out).** The reader is lm-mxl-mixer's: a writer that
stalls or re-creates its flow holds the last grain for `OMT_OUT_HOLD_MS`,
then black; no route sends black and silence (`OMT_OUT_IDLE_BLACK=false`:
nothing).

**Discovery.** OMT on Linux uses DNS-SD through avahi-daemon (D-Bus). The
image's entrypoint starts dbus and avahi-daemon inside the container
(`OMT_AVAHI=internal`, the container starts as root and runs the bridge as
`RUN_AS`), so it needs host networking. With a host avahi-daemon:
`OMT_AVAHI=host` and mount `/run/dbus`. Without mDNS (e.g. clouds that drop
multicast): `OMT_DISCOVERY_SERVER=omt://host:port` (an
[OMTDiscoveryServer](https://github.com/openmediatransport/OMTDiscoveryServer)),
or give inputs `omt://host:port` directly. Senders listen on
`OMT_PORT_START`…`OMT_PORT_END` (one port each).

## Configuration

| Variable | Default | |
|---|---|---|
| `VIDEO_FORMAT` | `1080p50` | House format of the input flows (720/1080/2160, p23.98…p60) |
| `OMT_INPUTS` / `OMT_OUTPUTS` | 1 / 1 | Number of OMT inputs and outputs (0–32) |
| `OMT_IN<n>_SOURCE` | – | Source of input n: `HOST (Name)` or `omt://host:port`. Also set at run time in the UI / `PUT /api/inputs/<n>` (saved in `OMT_STATE_DIR/sources.json`; the environment wins at start) |
| `OMT_OUT<n>_NAME` | `<prefix> Out <n>` | OMT source name of output n |
| `OMT_LABEL_PREFIX` | `OMT` | NMOS labels: `OMT In 1 Video`, `OMT Out 1 Audio`, ... |
| `AUDIO_CHANNELS` | 2 | Channels of the input audio flows |
| `OMT_IN_LOSS` | `hold` | `hold` (last frame for `OMT_IN_HOLD_MS`, then black) or `black` |
| `OMT_IN_HOLD_MS` | 1000 | |
| `OMT_IN_DELAY_MS` | 0 | Extra delay on top of the measured one (more jitter headroom) |
| `OMT_IN_SCALE` | true | Scale sources of another size; false: refuse them |
| `OMT_IN_QUALITY` | `default` | Quality the inputs suggest to their senders: default, low, medium, high |
| `OMT_OUT_PIXEL` | `uyvy` | `uyvy` (8-bit) or `p216` (10-bit, high-bit-depth VMX) |
| `OMT_OUT_QUALITY` | `default` | VMX quality of the outputs (default: what receivers suggest) |
| `OMT_OUT_READ_OFFSET_GRAINS` | 1 | How many grains behind now the outputs read |
| `OMT_OUT_HOLD_MS` | 1000 | Hold a stalled MXL flow this long, then black |
| `OMT_OUT_IDLE_BLACK` | true | Send black and silence without a route |
| `OMT_AVAHI` | `internal` | Entrypoint: `internal`, `host` or `off` (see Discovery) |
| `OMT_DISCOVERY_SERVER` | – | `omt://host:port` of an OMT discovery server instead of DNS-SD |
| `OMT_PORT_START` / `OMT_PORT_END` | 6400 / 6600 | TCP ports of the OMT senders |
| `OMT_LOG_FILE` | libomt default | libomt's log file (default under `OMT_STORAGE_PATH`, `/config/omt`) |
| `WEB_PORT` / `NMOS_PORT` | 8200 / 3334 | |
| `NMOS_*` | | As every mxl-nmos node (registry, host address, seed, label) |
| `MXL_DOMAIN_SCAN_PATH` | `/Volumes/mxl` | Domains the receivers may read |
| `MXL_OUTPUT_DOMAIN_DIR` | `<scan>/omt-bridge` | The input flows' domain |
| `MXL_HISTORY_MS` | 200 | Ring length of a new output domain |
| `MXL_CLEANUP_ON_EXIT` | false | Remove the output domain on exit |
| `OMT_STATE_DIR` | `/config` | Sources and IS-05 connections, kept across restarts |

Exit codes: 0, 75 (port, libmxl or libomt unavailable), 78 (configuration),
143 (SIGTERM).

## Build and run

```bash
docker build -f docker/Dockerfile -t lm-mxl-omt:dev .
# Docker Hub rate limit: --build-arg UBUNTU=public.ecr.aws/docker/library/ubuntu:24.04
```

The image builds libmxl (the mxl-stack pin), libvmx (clang, AVX2), libomtnet
and libomt (.NET 8 SDK, NativeAOT → `libomt.so`; the runtime needs only
`libicu`), the bridge, and libomt's C++ examples `omtsendtest` and
`omtrecvtest` (headless OMT sender/receiver, for interop tests:
`docker run --rm --network host --entrypoint omtrecvtest lm-mxl-omt:dev`).

Next to mxl-stack's single-host compose: `deploy/compose.single-host.yaml`.

Local development: `cargo test` (conversions, FFI layout) needs neither
libmxl nor libomt.

## Licences and attributions

lm-mxl-omt: Livemotion, private. It uses and ships, unmodified:

- **libomt**, **libomtnet**, **libvmx** and the OMT **Examples**
  (`omtsendtest`, `omtrecvtest`), © 2025 Open Media Transport Contributors,
  MIT. Their licence texts are in the image under `/usr/local/share/omt/`.
  Open Media Transport and OMT are names of that project; this bridge is not
  affiliated with it.
- **MXL** (dmf-mxl/mxl, Apache-2.0), loaded at run time; `mxl` / `mxl-sys`
  Rust bindings (Apache-2.0).
- **mxl-nmos** (LeeO86, MIT / Apache-2.0).
- `src/mxlio/domain.rs` and `scripts/build-mxl.sh` come from LeeO86's
  mxl-audio-mixer (MIT) through lm-mxl-mixer; `src/mxlio/` and
  `src/reader.rs` from lm-mxl-mixer.
- Structure and conventions (env vars, NMOS layout, exit codes, signal loss)
  follow LeeO86's mxl-decklink (MIT); no code from it.

## Test results and known gaps

Measured on the mxl-test VM: [docs/test-results.md](docs/test-results.md). In short,
1080p50 through OMT and back adds one grain (20 ms) with A/V sync unchanged, 8 streams
each way ran with no dropped or late frames at ~25% of a core per stream pair, and
libomt's own example sender and receiver interoperate.

Known gaps:

- Only on one host so far: all OMT traffic was loopback TCP. Real networks (and
  mDNS across hosts; the Infomaniak cloud drops multicast) are untested.
- The P216 (10-bit) conversions are scalar (≈2 ms a frame each way); 8-bit uses AVX2.
- Scaling (another source size) is unit-tested, not tested with a live source.
- Interlaced sources are written as progressive frames; no field handling.
- Audio: 48 kHz only (others are counted and replaced by silence); no resampling of a
  drifting sender's audio: its drift is absorbed by a 10 ms slip, then a resync.
- No OMT metadata, tally, preview stream, or alpha (UYVA/PA16 alpha is dropped; v210a
  is not produced).
- The latency trim keeps the delay over the slowest frame of 2 s; there is no tolerance
  for a rare late frame (a spike keeps the input at 2 grains).
- OMT discovery needs an avahi-daemon; the image runs one in the container as root
  before dropping to `RUN_AS`.
