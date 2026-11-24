import json, urllib.request
m = json.load(urllib.request.urlopen("http://127.0.0.1:8211/api/v1/measurement"))
keep = {}
def walk(d, p=""):
    for k, v in d.items():
        if isinstance(v, dict): walk(v, p + k + ".")
        elif isinstance(v, (int, float, str)) and any(x in k for x in ("offset", "latency", "grade", "arrival", "events", "count", "mean", "p50", "p95", "max", "min", "state")):
            keep[p + k] = v
walk(m)
for k, v in keep.items(): print(k, v)
