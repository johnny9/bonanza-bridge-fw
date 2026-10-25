#!/usr/bin/env python3
"""Byte-check 1004 DMA queues using the isolated ESP bridge-load bench fixture.

ASIC power must remain OFF. Requires a bench-loopback or bench-source bridge
image and the ESP queue-load fixture; never run against a mining device.
"""
import argparse
import json
from pathlib import Path
import time
import urllib.request


def u32(data, offset):
    return int.from_bytes(data[offset:offset + 4], "little")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host")
    parser.add_argument("--mode", choices=("loopback", "source", "flood"), required=True)
    parser.add_argument("--runs", type=int, default=8)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.runs <= 100:
        parser.error("runs must be 1..100")
    base = "http://" + args.host + "/api/system/"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(endpoint, method="GET"):
        req = urllib.request.Request(base + endpoint, method=method,
                                     data=b"" if method == "POST" else None)
        with opener.open(req, timeout=20) as response:
            return json.load(response)

    info = request("info")
    if not info.get("miningPaused"):
        raise RuntimeError("Pause mining and confirm ASIC power is OFF first")
    results = {"host": args.host, "mode": args.mode, "started": time.time(),
               "passed": False, "runs": []}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    try:
        for index in range(args.runs):
            before = request("bridge-trial")
            if args.mode != "flood" and args.mode not in before["bridge_version"]:
                raise RuntimeError("Bridge bench image does not match requested mode")
            mode = {"loopback": 1, "source": 2, "flood": 3}[args.mode]
            size = 131072 if mode == 3 else 2097152
            load = request(f"bridge-load?bytes={size}&loopback={mode}", "POST")
            after = request("bridge-trial")
            row = {"before": before, "load": load, "after": after}
            results["runs"].append(row)
            b, a = (bytes.fromhex(v["transport_hex"]) for v in (before, after))
            qb, qa = (bytes.fromhex(v["queue_hex"]) for v in (before, after))
            assert len(a) == len(b) == 84 and len(qa) == len(qb) == 68
            assert a[1] == 1 and int.from_bytes(a[12:20], "little") >= int.from_bytes(b[12:20], "little")
            assert load["status"] == load["mismatch"] == 0
            expected_rx = 0 if mode == 3 else 1048576
            assert load["received"] == expected_rx
            assert load["sent"] == (2 if mode == 2 else size)
            if mode == 2:
                assert load["elapsed_us"] < 2_400_000
            else:
                assert load["tx_us"] < size * 2.1 + 2000
            for offset in (48, 52, 56, 60, 64, 68, 76):
                assert u32(a, offset) == u32(b, offset), ("transport error", offset, row)
            for offset in (20, 24, 60, 64):
                assert u32(qa, offset) == u32(qb, offset), ("queue error", offset, row)
            if mode != 3:
                assert u32(qa, 32) > u32(qb, 32), "ASIC DMA did not reload"
            if mode == 1:
                assert u32(qa, 28) >= u32(qb, 28) + 2, "host DMA did not reload twice"
            assert u32(a, 40) - u32(b, 40) == expected_rx
            assert u32(a, 44) - u32(b, 44) == expected_rx
            if mode == 3:
                assert u32(a, 32) - u32(b, 32) == size
                assert u32(a, 36) - u32(b, 36) == size // 2
            print(f"{index + 1}/{args.runs}: verified {expected_rx} return bytes; {load['elapsed_us']} us", flush=True)
        results["passed"] = True
    finally:
        results["finished"] = time.time()
        args.out.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
