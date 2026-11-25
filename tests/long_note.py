"""Windowed acceptance suite for doc/CLI.md. Python 3.9+, standard library.

Run: python3 tests/long_note.py --base PATH --candidate PATH --profile mac4k120
     --output NEW_DIRECTORY [--timeout 600]
Each PATH is an executable measurement adapter supplied by the implementation.
One invocation = one fresh process. Read one JSON request on stdin, prepare the
fixture before timing, and write one JSON object on stdout. Logs go to stderr.
Exit 2 for unavailable hardware/instrumentation. Timeout, crash or OOM fails.
No adapter is necessary to run the model benchmark in note-ui.

Request keys are written to request.json in every row directory. 'size' means
hidden strokes (A/E), page count (B), visible strokes (C), or input points (D).
The control viewport has 1000 finished strokes; B has 1000 strokes per page.
Use seed 1, 24 points per finished stroke, a fixed camera and a fixed edit.
Extend D in equal batches and measure the update at the requested prefix.
For interactive rows warm up then measure repetitions, restoring the same state
outside timing. Aggregate deferred work including all buffer slots into each
operation. Do not flush the GPU inside a timed interval to collect counters.

Response fields:
  device: {machine, ram_bytes, integrated_gpu, os, driver, power_mode,
    pressure_start, replay_hz, width, height, refresh_hz, external, platform}
  commit, release (boolean), fixture_digest, viewport_digest, warmups_completed
  observations: one object per measured action in input order:
    index, edit_id, presented_edit_id, frame_id, input_ns, commit_ns, submit_ns,
    gpu_done_ns, presented_ns (one monotonic clock), inspected, index_visits,
    regenerated, submitted_vertices, uploaded_bytes, gpu_ns, appended_vertices,
    appended_vertex_bytes, deferred_ns, cpu_work_ns, drawable_wait_ns, coalesced.
    Blank erase is a no-op: only input_ns, index and counters are required.
    For other actions edit_id identifies the input/update, including camera moves.
  active_windows: for append/write/pan_inside/extend/commit_long, an array of
    {first_slot, last_slot, presented_slots}. Slots are real display refresh
    serials (inclusive) within contiguous active input, including empty gaps.
  memory: {peak_rss_bytes, post_close_rss_bytes, cache_bytes, resident_pages}
  before_digest, restored_digest: independent reads of ordered stroke geometry,
    colours, widths and counts before each scenario and after restoring it.
  expected_after_digest, actual_after_digest: independent replay oracle and
    observed state at the action boundary before restoration (including camera
    or selection for view operations). Both include geometry, order and count.
  steps: for traversal only, visited page IDs followed by first page ID;
    reopened_digest; swap_out_bytes at start and after each of two full tours.
  cold_usable_ns: time to first usable ink for cold-open rows.

Evidence is saved before evaluation. 0 = candidate qualified with complete comparison evidence,
1 = observed candidate failure, 2 = incomplete evidence. Run both hardware profiles independently.
"""
import argparse
import json
import math
import os
import statistics
import subprocess
import sys
from pathlib import Path


class Missing(ValueError):
    pass


class Breach(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Missing(message)


def gate(condition, message):
    if not condition:
        raise Breach(message)


def integer(value, name):
    require(type(value) is int and value >= 0, name)
    return value


def number(row, name):
    return integer(row[name], name)


def tails(values):
    ordered = sorted(values)
    return {**{f'p{p}': ordered[(p * len(ordered) + 99) // 100 - 1]
               for p in (50, 95, 99)}, 'max': ordered[-1], 'n': len(ordered)}


def matrix():
    hidden = (0, 10_000, 100_000, 1_000_000)
    return [('A', hidden, ('append', 'blank_erase')),
            ('B', (1, 100, 1000), ('append', 'blank_erase', 'cold_open', 'traverse')),
            ('C', (1000, 10_000, 100_000, 1_000_000), ('write', 'pan_inside')),
            ('D', (24, 256, 2048, 16384), ('extend', 'commit_long')),
            ('E', hidden, ('pan_inside', 'pan_across', 'zoom_rung', 'move_selection',
                           'undo', 'redo'))]


def check_slots(windows):
    require(isinstance(windows, list) and windows, 'active windows')
    expected = shown = 0
    previous = -1
    for w in windows:
        first, last = number(w, 'first_slot'), number(w, 'last_slot')
        require(previous < first <= last, 'overlapping or inverted active windows')
        slots = w['presented_slots']
        require(isinstance(slots, list), 'presented_slots')
        for slot in slots:
            require(first <= integer(slot, 'slot') <= last, 'slot outside window')
        require(slots == sorted(set(slots)), 'duplicate or unordered slot')
        expected += last - first + 1
        shown += len(slots)
        previous = last
    gate((expected - shown) * 100 <= expected, 'missed more than 1% of refresh slots')
    return expected - shown, expected


def validate(req, data, controls):
    action, case, size = (req[k] for k in ('action', 'case', 'size'))
    device = data['device']
    for key in ('machine', 'os', 'driver', 'power_mode', 'pressure_start', 'platform'):
        require(isinstance(device[key], str) and device[key], key)
    for key in ('refresh_hz', 'replay_hz'):
        v = device[key]
        require(type(v) in (int, float) and math.isfinite(v) and v > 0, key)
    for key in ('external', 'integrated_gpu'):
        require(type(device[key]) is bool, key)
    for key in ('width', 'height', 'ram_bytes'):
        require(number(device, key) > 0, key)
    if req['profile'] == 'mac4k120':
        require(device['platform'] == 'macos' and device['external'] is True and
                (device['width'], device['height']) == (3840, 2160) and
                abs(device['refresh_hz'] - 120) <= 0.5, 'external Mac 4K120 profile')
    else:
        require(device['integrated_gpu'] is True and
                7 * 2**30 <= device['ram_bytes'] <= 8 * 2**30, '8 GB iGPU profile')
    require(data['release'] is True and data['commit'], 'release build identity')
    for key in ('fixture_digest', 'viewport_digest', 'before_digest', 'restored_digest',
                'expected_after_digest', 'actual_after_digest'):
        require(isinstance(data[key], str) and len(data[key]) > 0, key)
    gate(data['before_digest'] == data['restored_digest'], 'stroke state was not restored')
    gate(data['expected_after_digest'] == data['actual_after_digest'], 'action state differs from oracle')
    for key in ('peak_rss_bytes', 'post_close_rss_bytes', 'cache_bytes', 'resident_pages'):
        number(data['memory'], key)
    require(data['memory']['peak_rss_bytes'] > 0, 'missing peak RSS')
    if action == 'cold_open':
        return {'cold_ns': number(data, 'cold_usable_ns')}
    if action == 'traverse':
        steps = data['steps']
        require(len(steps) == min(20, size) + 1 and steps[-1] == steps[0] and
                len(set(steps[:-1])) == min(20, size) and
                all(type(p) is int and 0 <= p < size for p in steps), 'page tour')
        gate(data['reopened_digest'] == data['before_digest'], 'reopened ink changed')
        swap = data['swap_out_bytes']
        require(len(swap) == 3 and all(integer(v, 'swap') >= 0 for v in swap)
                and swap == sorted(swap), 'two traversal cycles')
        if req['profile'] == 'igpu8':
            gate(not (swap[0] < swap[1] < swap[2]), 'sustained swap growth')
        return {'peak_rss': data['memory']['peak_rss_bytes']}
    require(number(data, 'warmups_completed') >= req['warmups'], 'warmup count')
    rows = data['observations']
    require(isinstance(rows, list) and len(rows) == req['samples'], 'sample count')
    latency = []
    seen = set()
    for i, row in enumerate(rows):
        require(number(row, 'index') == i, 'sample order')
        for k in ('input_ns', 'inspected', 'index_visits', 'regenerated', 'submitted_vertices',
                  'uploaded_bytes', 'gpu_ns', 'cpu_work_ns', 'drawable_wait_ns', 'coalesced',
                  'deferred_ns', 'appended_vertices', 'appended_vertex_bytes'):
            number(row, k)
        if action == 'blank_erase':
            gate(row['regenerated'] == row['uploaded_bytes'] == 0, 'blank erase rebuilt ink')
            continue
        update = number(row, 'edit_id')
        require(update not in seen, 'duplicate edit ID')
        seen.add(update)
        require('presented_edit_id' in row and 'frame_id' in row, 'presentation evidence')
        gate(row['presented_edit_id'] == update and row['frame_id'] is not None,
             'edit missing from presented frame')
        stamps = [number(row, k) for k in ('input_ns', 'commit_ns', 'submit_ns',
                                          'gpu_done_ns', 'presented_ns')]
        require(stamps == sorted(stamps), 'clock order')
        latency.append(stamps[-1] - stamps[0])
    result = {k: tails([r[k] for r in rows]) for k in
              ('inspected', 'regenerated', 'uploaded_bytes', 'submitted_vertices',
               'gpu_ns', 'cpu_work_ns', 'deferred_ns')}
    if latency:
        result['latency_ns'] = tails(latency)
        if action in ('append', 'write', 'pan_inside', 'extend', 'commit_long'):
            gate(result['latency_ns']['p99'] <= 2e9 / device['refresh_hz'], 'p99 over two refreshes')
    if action in ('append', 'write', 'pan_inside', 'extend', 'commit_long'):
        result['slots_missed_total'] = check_slots(data['active_windows'])
    if case in ('A', 'B'):
        key = (req['build'], req['run'], case, action)
        if size == (0 if case == 'A' else 1):
            if action == 'append':
                for field in ('appended_vertices', 'appended_vertex_bytes'):
                    require(rows[0][field] > 0 and all(r[field] == rows[0][field] for r in rows),
                            'inconsistent reference stroke geometry')
            controls[key] = (data['viewport_digest'], result, rows[0]['appended_vertices'],
                             rows[0]['appended_vertex_bytes'])
        require(key in controls, 'unavailable locality control')
        viewport, baseline, vertices, byte_count = controls[key]
        require(data['viewport_digest'] == viewport, 'control viewport differs')
        for field, slack in (('inspected', 64), ('regenerated', vertices),
                             ('uploaded_bytes', byte_count)):
            gate(result[field]['p99'] <= baseline[field]['p99'] + slack,
                 f'{field} exceeds control + {slack}')
    return result


def invoke(adapter, directory, req, timeout):
    directory.mkdir()
    payload = json.dumps(req)
    (directory / 'request.json').write_text(payload)
    raw = directory / 'response.json'
    try:
        with raw.open('wb') as out, (directory / 'stderr.log').open('wb') as err:
            process = subprocess.run([adapter], input=payload.encode(), stdout=out,
                                     stderr=err, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        raise Breach(f'target exceeded {timeout}s') from exc
    require(process.returncode != 2, 'adapter reported unavailable evidence')
    gate(process.returncode == 0, f'adapter exited {process.returncode}')
    return json.loads(raw.read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for kind in ('base', 'candidate'):
        parser.add_argument('--' + kind, type=Path, required=True)
    parser.add_argument('--profile', choices=('mac4k120', 'igpu8'), required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout', type=int, default=600)
    args = parser.parse_args()
    if args.timeout <= 0 or args.output.exists():
        parser.error('use a positive timeout and a new output directory')
    for path in (args.base, args.candidate):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f'adapter executable unavailable: {path}')
    args.output.mkdir(parents=True)
    controls, fixtures, commits, cold = {}, {}, {}, {}
    hardware = None
    counts = {build: dict(passed=0, failed=0, incomplete=0) for build in ('base', 'candidate')}
    for case, sizes, actions in matrix():
        for size in sizes:
            for action in actions:
                cold_open = action == 'cold_open'
                for run in range(10 if cold_open else 3):
                    order = ('base', 'candidate') if run % 2 == 0 else ('candidate', 'base')
                    for build in order:
                        name = f'{case}-{size}-{action}-{run}-{build}'
                        folder = args.output / name
                        req = dict(case=case, size=size, action=action, run=run, build=build,
                                   profile=args.profile, seed=1, timeout_seconds=args.timeout,
                                   warmups=0 if cold_open or action == 'traverse' else 100,
                                   samples=1 if cold_open or action == 'traverse' else 1000)
                        try:
                            data = invoke(getattr(args, build).resolve(), folder, req, args.timeout)
                            result = validate(req, data, controls)
                            identity = {k: v for k, v in data['device'].items()
                                        if k != 'pressure_start'}
                            if hardware is None:
                                hardware = identity
                            require(identity == hardware, 'hardware or replay mode changed')
                            require(commits.setdefault(build, data['commit']) == data['commit'],
                                    'build changed between runs')
                            fixture = (case, size)
                            require(fixtures.setdefault(fixture, data['fixture_digest']) ==
                                    data['fixture_digest'], 'fixture changed across runs/builds')
                            if cold_open:
                                series = cold.setdefault((build, size), [])
                                series.append(result['cold_ns'])
                                if len(series) == 10:
                                    result.update(median_ns=statistics.median(series),
                                                  max_ns=max(series), cold_samples_ns=series)
                            verdict = dict(status='passed', result=result)
                        except Breach as exc:
                            verdict = dict(status='failed', reason=str(exc))
                        except (Missing, KeyError, TypeError, ValueError, OSError) as exc:
                            verdict = dict(status='incomplete', reason=str(exc))
                        counts[build][verdict['status']] += 1
                        (folder / 'verdict.json').write_text(json.dumps(verdict, indent=2))
                        print(name, verdict['status'], verdict.get('reason', ''), flush=True)
    (args.output / 'summary.json').write_text(json.dumps(counts, indent=2))
    if counts['candidate']['failed']:
        return 1
    return 2 if any(v['incomplete'] for v in counts.values()) else 0


if __name__ == '__main__':
    sys.exit(main())
