#!/usr/bin/env python3
"""Run the Rust CLI sequentially against one bounded public test file."""
import argparse
import datetime
import json
import pathlib
import statistics
import subprocess
import tempfile
import time
import urllib.parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--url')
    source.add_argument('--url-file', type=pathlib.Path)
    parser.add_argument('--connections', default='1,4,8')
    parser.add_argument('--rounds', type=int, default=1)
    parser.add_argument('--max-mib', type=int, default=128)
    parser.add_argument('--timeout', type=int, default=90)
    parser.add_argument('--output', type=pathlib.Path, default=pathlib.Path('benchmarks/public-results.json'))
    args = parser.parse_args()
    url = args.url if args.url else args.url_file.read_text().strip()
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme not in ('http', 'https') or not parsed.hostname:
        parser.error('an HTTP(S) URL is required')
    connections = [int(c) for c in args.connections.split(',')]
    if not connections or not all(1 <= c <= 16 for c in connections) or not 1 <= args.rounds <= 5:
        parser.error('connections must be 1–16; rounds must be 1–5')
    label = urllib.parse.urlunsplit((parsed.scheme, parsed.hostname, parsed.path, '[redacted]' if parsed.query else '', ''))
    report = {'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'source': label,
              'scope': 'Public HTTPS/HTTP sample, sequential runs with rotated order. No physical bandwidth measurement and no Neat comparison. Hashes verify consistency between completed runs, not publisher authenticity.',
              'rounds': args.rounds, 'samples': []}
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')

    root = pathlib.Path(__file__).resolve().parent.parent
    binary = root / 'target/release/ffdm'
    try:
        probe = subprocess.run([str(binary), 'probe', url], capture_output=True, text=True, timeout=30)
        if probe.returncode:
            raise ValueError(probe.stderr.replace(url, label)[-1000:])
        metadata = json.loads(probe.stdout)
        report['probe'] = metadata
        if not 200 <= metadata['status'] < 300:
            raise ValueError(f"HTTP {metadata['status']}")
        size = metadata.get('content_length') or 0
        if not 0 < size <= args.max_mib * 1024 * 1024:
            raise ValueError('unknown length or file exceeds configured size bound')
    except Exception as error:
        report['probe_error'] = str(error).replace(url, label)
        save()
        raise SystemExit('Source probe failed; see the redacted report.') from None
    report['file_bytes'] = size
    reference = None
    for round_index in range(args.rounds):
        order = connections[round_index % len(connections):] + connections[:round_index % len(connections)]
        for connection in order:
            print(f'Public test: round {round_index + 1}/{args.rounds}, {connection} requested connection(s)', flush=True)
            with tempfile.TemporaryDirectory(prefix='ffdm-wan-') as temp:
                cmd = [str(binary), 'download', url, '--output', str(pathlib.Path(temp) / 'sample.bin'),
                       '--connections', str(connection), '--timeout', str(args.timeout), '--quiet']
                if reference:
                    cmd += ['--sha256', reference]
                started = time.monotonic()
                try:
                    run = subprocess.run(cmd, capture_output=True, text=True, timeout=args.timeout + 30)
                    if run.returncode:
                        sample = {'outcome': 'failed', 'error': run.stderr.replace(url, label)[-2000:]}
                    else:
                        sample = json.loads(run.stdout)
                        if sample['outcome'] != 'completed' or sample['bytes'] != size:
                            raise ValueError('incomplete or unexpected-size download')
                        if reference is None:
                            reference = sample['sha256']
                        if sample['sha256'] != reference:
                            raise ValueError('hash changed between samples')
                except subprocess.TimeoutExpired:
                    sample = {'outcome': 'failed', 'error': 'process exceeded test timeout'}
                sample.update({'round': round_index + 1, 'requested_connections': connection, 'process_wall_seconds': time.monotonic() - started})
                report['samples'].append(sample)
                save()
                if sample['outcome'] == 'completed':
                    print(f"  {sample['total_seconds']:.2f} s, {sample['average_mbps']:.2f} Mbps, SHA-256 {sample['sha256'][:12]}…", flush=True)
                else:
                    print('  failed; see report', flush=True)
    report['reference_sha256'] = reference
    report['summary'] = []
    for connection in connections:
        samples = [s for s in report['samples'] if s['requested_connections'] == connection and s['outcome'] == 'completed']
        if samples:
            report['summary'].append({'requested_connections': connection, 'completed_samples': len(samples),
                                      'median_seconds': statistics.median(s['total_seconds'] for s in samples),
                                      'median_mbps': statistics.median(s['average_mbps'] for s in samples)})
    save()
    print(json.dumps(report['summary'], ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
