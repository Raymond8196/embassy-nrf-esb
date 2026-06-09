#!/usr/bin/env python3
"""
BLE Connected + ESB 3-Mode Performance Capture
Captures 180s of serial data from both dongles and computes real-time stats.
"""
import serial
import time
import sys
import json
import argparse
from collections import defaultdict
from datetime import datetime

def parse_central(line):
    """Parse central (ACM0) line: b= rx= dup= crc= p0:a/b/c/d p1:e/f/g/h s= t0= rd= bk= cn= dt= si= sc= ov= iv= nb= eb= nc= ec= ph= ac= rq= cfg= re="""
    d = {}
    try:
        parts = line.strip().split()
        for p in parts:
            if '=' in p:
                k, v = p.split('=', 1)
                if k.startswith('p0:') or k.startswith('p1:'):
                    pipe_key = k[:2]
                    vals = k[3:].split('/')
                    d[f'{pipe_key}_total'] = int(vals[0])
                    d[f'{pipe_key}_crc'] = int(vals[1])
                    d[f'{pipe_key}_dup'] = int(vals[2])
                    d[f'{pipe_key}_ok'] = int(vals[3])
                else:
                    try:
                        d[k] = int(v)
                    except ValueError:
                        d[k] = v
    except Exception:
        pass
    return d

def parse_peripheral(line):
    """Parse peripheral (ACM1) line: e= ok= att= drop= s= t0= rd= bk= cn= dt= si= sc= ov= iv= lat= sync= wait= fb= lock= miss= age="""
    d = {}
    try:
        parts = line.strip().split()
        for p in parts:
            if '=' in p:
                k, v = p.split('=', 1)
                if v in ('true', 'false'):
                    d[k] = v == 'true'
                else:
                    try:
                        d[k] = int(v)
                    except ValueError:
                        d[k] = v
    except Exception:
        pass
    return d

def main():
    parser = argparse.ArgumentParser(description='BLE+ESB performance capture')
    parser.add_argument('--duration', type=int, default=180, help='Capture duration in seconds')
    parser.add_argument('--central-port', default='/dev/ttyACM0')
    parser.add_argument('--peripheral-port', default='/dev/ttyACM1')
    args = parser.parse_args()

    duration = args.duration
    print(f"=== BLE Connected ESB Capture ({duration}s) ===")
    print(f"Central:     {args.central_port}")
    print(f"Peripheral:  {args.peripheral_port}")
    print(f"Start:       {datetime.now().isoformat()}")
    print()

    try:
        ser_c = serial.Serial(args.central_port, 115200, timeout=1)
    except Exception as e:
        print(f"ERROR: Cannot open central port: {e}")
        sys.exit(1)

    try:
        ser_p = serial.Serial(args.peripheral_port, 115200, timeout=1)
    except Exception as e:
        print(f"ERROR: Cannot open peripheral port: {e}")
        ser_c.close()
        sys.exit(1)

    central_data = []
    peripheral_data = []
    cumulative_data = []

    last_c = {}
    last_p = {}
    prev_c = {}
    prev_p = {}

    last_report = time.time()
    start_time = time.time()
    last_c_count = 0
    last_p_count = 0

    report_interval = 10

    c_ok_rate = []
    c_rx_rate = []
    p_ok_rate = []
    p_latency = []
    p_attempts = []
    p_miss = []

    print(f"{'Time':>6}s | {'C:rx/s':>7} {'C:es/s':>7} {'C:ef/s':>6} {'C:bk/s':>6} | {'P:evt/s':>7} {'P:ok%':>6} {'P:lat_avg':>8} {'P:att_avg':>7} {'P:drop':>5} {'P:miss':>5}")
    print("-" * 103)

    while True:
        now = time.time()
        elapsed = now - start_time
        if elapsed >= duration:
            break

        line_c = ser_c.readline().decode('utf-8', errors='replace').strip()
        line_p = ser_p.readline().decode('utf-8', errors='replace').strip()

        if line_c:
            prev_c = dict(last_c)
            last_c = parse_central(line_c)
            central_data.append({'t': elapsed, **last_c})
            last_c_count += 1

        if line_p:
            prev_p = dict(last_p)
            last_p = parse_peripheral(line_p)
            peripheral_data.append({'t': elapsed, **last_p})
            last_p_count += 1

        if now - last_report >= report_interval:
            dt = now - last_report

            c_rx_s = 0
            c_es_s = 0
            c_ef_s = 0
            c_bk_s = 0
            if last_c and prev_c:
                c_rx_s = max(0, last_c.get('rx', 0) - prev_c.get('rx', 0)) / dt
                c_bk_s = max(0, last_c.get('bk', 0) - prev_c.get('bk', 0)) / dt

            # es/ef are per-report deltas, so sum them across the window.
            recent_c = [d for d in central_data if d['t'] > elapsed - dt]
            if recent_c:
                c_es_s = sum(d.get('es', 0) for d in recent_c) / dt
                c_ef_s = sum(d.get('ef', 0) for d in recent_c) / dt

            p_events = 0
            p_ok_pct = 100.0
            p_lat_avg = 0
            p_att_avg = 0
            p_drop_count = 0
            p_miss_total = 0

            recent_p = [d for d in peripheral_data if d['t'] > elapsed - dt]
            if recent_p:
                p_events = len(recent_p)
                p_ok_pct = sum(1 for d in recent_p if d.get('ok')) / len(recent_p) * 100
                lats = [d.get('lat', 0) for d in recent_p if d.get('lat') is not None]
                p_lat_avg = sum(lats) / len(lats) if lats else 0
                atts = [d.get('att', 0) for d in recent_p if d.get('att') is not None]
                p_att_avg = sum(atts) / len(atts) if atts else 0
                p_drop_count = sum(1 for d in recent_p if not d.get('ok', True))
                p_miss_total = sum(d.get('miss', 0) for d in recent_p)

                p_latency.extend(lats)
                p_attempts.extend(atts)

            c_ok_rate.append(c_rx_s)
            c_rx_rate.append(c_rx_s)

            print(f"{elapsed:6.0f}s | {c_rx_s:7.1f} {c_es_s:7.1f} {c_ef_s:6.1f} {c_bk_s:6.1f} | {p_events:7d} {p_ok_pct:5.1f}% {p_lat_avg:8.0f} {p_att_avg:7.1f} {p_drop_count:5d} {p_miss_total:5d}")

            last_report = now

    ser_c.close()
    ser_p.close()

    elapsed = time.time() - start_time

    print()
    print("=" * 60)
    print(f"  CAPTURE COMPLETE ({elapsed:.1f}s)")
    print(f"  End: {datetime.now().isoformat()}")
    print("=" * 60)

    total_c = len(central_data)
    total_p = len(peripheral_data)
    print(f"\n  Raw lines captured:")
    print(f"    Central:     {total_c}")
    print(f"    Peripheral:  {total_p}")

    if central_data:
        first_c = central_data[0]
        last_c_entry = central_data[-1]
        print(f"\n  Central Cumulative:")
        print(f"    rx total:       {last_c_entry.get('rx', '?')}")
        print(f"    dup total:      {last_c_entry.get('dup', '?')}")
        print(f"    crc errors:     {last_c_entry.get('crc', '?')}")
        print(f"    blocks:         {last_c_entry.get('bk', '?')}")
        print(f"    nobuf:          {last_c_entry.get('nb', '?')}")
        print(f"    event blocks:   {last_c_entry.get('eb', '?')}")
        es_total = sum(d.get('es', 0) for d in central_data)
        ef_total = sum(d.get('ef', 0) for d in central_data)
        ext_total = es_total + ef_total
        print(f"    extend ok:      {es_total}")
        print(f"    extend failed:  {ef_total}" + (f" ({ef_total/ext_total*100:.2f}% yield)" if ext_total else ""))
        print(f"    rx rate:        {last_c_entry.get('rx', 0) / elapsed:.1f}/s" if last_c_entry.get('rx') else "    rx rate: N/A")
        if last_c_entry.get('p1_total') is not None:
            print(f"    pipe1 total:    {last_c_entry.get('p1_total', '?')}")
            print(f"    pipe1 ok:       {last_c_entry.get('p1_ok', '?')}")
            print(f"    pipe1 crc:      {last_c_entry.get('p1_crc', '?')}")
            print(f"    pipe1 dup:      {last_c_entry.get('p1_dup', '?')}")

    if peripheral_data:
        last_p_entry = peripheral_data[-1]
        print(f"\n  Peripheral Cumulative:")
        print(f"    events total:   {last_p_entry.get('e', '?')}")
        print(f"    evt rate:       {last_p_entry.get('e', 0) / elapsed:.1f}/s" if last_p_entry.get('e') else "    evt rate: N/A")

        ok_count = sum(1 for d in peripheral_data if d.get('ok'))
        drop_count = total_p - ok_count
        print(f"    ok:             {ok_count}/{total_p} ({ok_count/total_p*100:.2f}%)" if total_p else "    ok: N/A")
        print(f"    drops:          {drop_count}")

        if p_latency:
            p_latency.sort()
            print(f"\n  Latency (us):")
            print(f"    min:    {min(p_latency)}")
            print(f"    p50:    {p_latency[len(p_latency)//2]}")
            print(f"    p95:    {p_latency[int(len(p_latency)*0.95)]}")
            print(f"    p99:    {p_latency[int(len(p_latency)*0.99)]}")
            print(f"    max:    {max(p_latency)}")
            print(f"    mean:   {sum(p_latency)/len(p_latency):.0f}")

        if p_attempts:
            print(f"\n  TX Attempts:")
            print(f"    mean:   {sum(p_attempts)/len(p_attempts):.2f}")
            print(f"    max:    {max(p_attempts)}")
            att_dist = defaultdict(int)
            for a in p_attempts:
                att_dist[a] += 1
            for a in sorted(att_dist.keys()):
                print(f"    {a} attempts: {att_dist[a]} ({att_dist[a]/len(p_attempts)*100:.1f}%)")

        sync_count = sum(1 for d in peripheral_data if d.get('sync'))
        print(f"\n  Sync events:     {sync_count}/{total_p}")

        wait_count = sum(1 for d in peripheral_data if d.get('wait'))
        print(f"  Wait events:     {wait_count}/{total_p}")

    ts = datetime.now().strftime('%Y%m%d_%H%M%S')
    out_c = f"capture_ble_conn_central_{ts}.jsonl"
    out_p = f"capture_ble_conn_peripheral_{ts}.jsonl"

    with open(out_c, 'w') as f:
        for d in central_data:
            f.write(json.dumps(d) + '\n')
    print(f"\n  Saved: {out_c}")

    with open(out_p, 'w') as f:
        for d in peripheral_data:
            f.write(json.dumps(d) + '\n')
    print(f"  Saved: {out_p}")

    print(f"\n  Test: BLE nRF Connect connected + ESB 3-Mode")
    print(f"  Duration: {elapsed:.1f}s")

if __name__ == '__main__':
    main()
