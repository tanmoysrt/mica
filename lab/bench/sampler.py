#!/usr/bin/env python3
"""Every second: unsaved and local MiB, and S3 MiB of the attached disks of the lab daemon, as TSV."""
import json
import os
import subprocess
import sys
import time

# The v2 socket is root only: the CLI asks through the lab root helper.
LAB = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

out = open(sys.argv[1], "a", buffering=1)
out.write("t\tunsaved_MiB\tlocal_MiB\ts3_in_MiB\ts3_out_MiB\n")
while True:
    try:
        status = subprocess.run([f"{LAB}/lab.sh", "mica", "status", "--json"], capture_output=True, timeout=5, check=True)
        r = json.loads(status.stdout)
        disks = [d["status"] for d in r["disks"] if "status" in d]
        unsaved = sum(d["unsaved_bytes"] for d in disks) >> 20
        local = sum(d["local_bytes"] for d in disks) >> 20
        s3_in = sum(d["requests"]["bytes_in"] for d in disks) >> 20
        s3_out = sum(d["requests"]["bytes_out"] for d in disks) >> 20
        out.write(f"{time.time():.1f}\t{unsaved}\t{local}\t{s3_in}\t{s3_out}\n")
    except Exception as error:
        out.write(f"{time.time():.1f}\terror {error}\n")
    time.sleep(1)
