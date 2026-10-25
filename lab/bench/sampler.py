#!/usr/bin/env python3
"""Every second: unsaved bytes, local chunks and S3 bytes of the lab daemon, as TSV."""
import json
import socket
import sys
import time

out = open(sys.argv[1], "a", buffering=1)
out.write("t\tunsaved_MiB\tlocal_chunks\ts3_in_MiB\ts3_out_MiB\n")
while True:
    try:
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(2)
        s.connect("/run/mica-lab.sock")
        s.sendall(b'{"command":"status"}\n')
        data = b""
        while not data.endswith(b"\n"):
            chunk = s.recv(65536)
            if not chunk:
                break
            data += chunk
        s.close()
        r = json.loads(data)["result"]
        disks = [d["status"] for d in r["disks"] if "status" in d]
        unsaved = sum(d["unsaved_bytes"] for d in disks) >> 20
        chunks = sum(d["local_chunks"] for d in disks)
        out.write(f"{time.time():.1f}\t{unsaved}\t{chunks}\t{r['s3']['bytes_in'] >> 20}\t{r['s3']['bytes_out'] >> 20}\n")
    except Exception as error:
        out.write(f"{time.time():.1f}\terror {error}\n")
    time.sleep(1)
