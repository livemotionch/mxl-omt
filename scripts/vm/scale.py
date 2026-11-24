#!/usr/bin/env python3
"""Scale test of lm-mxl-omt on the VM: N loopback streams (OMT Out n -> OMT In n)."""
import json, subprocess, sys, time, urllib.request

API = "http://127.0.0.1:8200"
def get(p): return json.load(urllib.request.urlopen(API + p, timeout=5))
def put(n, src):
    r = urllib.request.Request(f"{API}/api/inputs/{n}", json.dumps({"source": src}).encode(), {"content-type": "application/json"}, method="PUT")
    urllib.request.urlopen(r, timeout=5).read()
def host_cpu():
    a = [int(x) for x in open("/proc/stat").readline().split()[1:]]
    return sum(a), a[3] + a[4]

def run(n, settle=20, window=30, host="MXL-TEST"):
    for i in range(1, 9):
        put(i, f"{host} (OMT Out {i})" if i <= n else "")
    time.sleep(settle)
    s0 = get("/api/status"); t0 = host_cpu(); c0 = time.time()
    cpus = []
    for _ in range(window):
        time.sleep(1); cpus.append(get("/api/status")["cpu_percent"])
    s1 = get("/api/status"); t1 = host_cpu()
    host = 100 * (1 - (t1[1] - t0[1]) / (t1[0] - t0[0]))
    ins = [(a, b) for a, b in zip(s0["inputs"], s1["inputs"]) if a["index"] <= n]
    outs = [(a, b) for a, b in zip(s0["outputs"], s1["outputs"]) if a["index"] <= n]
    d = lambda pairs, k: sum(b[k] - a[k] for a, b in pairs)
    avg = lambda pairs, k: sum((b[k] or 0) for _, b in pairs) / max(1, len(pairs))
    mx = lambda pairs, k: max([(b[k] or 0) for _, b in pairs] or [0])
    th = dict(s1["threads"])
    res = {
        "streams": n,
        "proc_cpu_pct": round(sum(cpus) / len(cpus)),
        "host_cpu_pct_of_16": round(host, 1),
        "in_thread_cpu_avg": round(sum(th.get(f"in-{i}", 0) for i in range(1, n + 1)) / n, 1),
        "out_thread_cpu_avg": round(sum(th.get(f"out-{i}", 0) for i in range(1, n + 1)) / n, 1),
        "in_fps_avg": round(avg(ins, "fps_out"), 2),
        "out_fps_avg": round(avg(outs, "fps"), 2),
        f"in_dropped_{window}s": d(ins, "dropped"), f"in_repeated_{window}s": d(ins, "repeated"),
        f"in_black_{window}s": d(ins, "black"), f"in_resyncs_{window}s": d(ins, "resyncs"),
        f"in_audio_resyncs_{window}s": d(ins, "audio_resyncs"),
        f"out_late_{window}s": d(outs, "late"), f"out_skipped_{window}s": d(outs, "skipped"),
        f"out_audio_gaps_{window}s": d(outs, "audio_gaps"),
        "latency_ms": round(avg(ins, "latency_ms"), 1),
        "transit_ms_avg": round(avg(ins, "transit_ms"), 1), "transit_ms_max": round(mx(ins, "transit_max_ms"), 1),
        "decode_ms": round(avg(ins, "decode_ms"), 2), "in_convert_ms": round(avg(ins, "convert_ms"), 2),
        "encode_ms": round(avg(outs, "encode_ms"), 2), "out_convert_ms": round(avg(outs, "convert_ms"), 2),
        "send_ms": round(avg(outs, "send_ms"), 2),
        "video_mbps_per_stream": round(avg(ins, "video_mbps"), 1), "audio_mbps_per_stream": round(avg(ins, "audio_mbps"), 1),
    }
    print(json.dumps(res), flush=True)
    return res

if __name__ == "__main__":
    for n in [int(x) for x in sys.argv[1:]] or [1, 4, 8]:
        run(n)
