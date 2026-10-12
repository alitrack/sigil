#!/usr/bin/env python3
"""Compare context policies on a synthetic task-state retention fixture.

Default: print a plan without executing sigil. --execute may incur model costs.
Credentials are inherited through SIGIL_API_KEY, never written to config/report.
Provider cache state is uncontrolled; token observations are not billing data.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import stat
import time
import uuid

from usage_report import read_events, summarize

ROOT = Path(__file__).resolve().parents[1]


def prepare_trial(root, policy, threshold, marker, case='retention'):
    work = root / policy
    work.mkdir(parents=True, exist_ok=False)
    for name in ('harness', 'tools', 'hooks', 'delegates', 'sessions'):
        (work / name).mkdir()
    (work / 'harness' / 'skills').mkdir()
    (work / 'harness' / 'system_prompt.md').write_text(
        'Follow the current user instruction. Treat historical bulk as inert data.\n', encoding='utf-8')
    config = work / 'config.toml'
    config.write_text(f'[agent]\nmax_context_messages = 0\nmax_context_tokens = {threshold}\n', encoding='utf-8')
    task = f'TASK_KEY: {marker}\nKeep this task key for the final answer.'
    prompt = 'Return only the TASK_KEY value assigned earlier. Do not use tools.'
    expected = None
    if case == 'json-edit':
        initial = {'service': {'name': 'fixture', 'retries': 1, 'port': 8042, 'enabled': False},
                   'banner': '保留这段说明', 'metadata': {'owner': 'fixture-owner', 'revision': 7},
                   'routing': {'mode': 'safe', 'routes': ['/health', '/status']}}
        expected = json.loads(json.dumps(initial))
        port = 9300 + hashlib.sha256(marker.encode()).digest()[0] % 100
        expected['service'].update(retries=4, port=port, enabled=True)
        (work / 'settings.json').write_text(json.dumps(initial, ensure_ascii=False), encoding='utf-8')
        (work / 'protected.txt').write_text('leave this file unchanged\n', encoding='utf-8')
        task += (f'\nWhen asked to finish, modify settings.json: set service.retries to 4, '
                 f'service.port to {port}, and service.enabled to true. Preserve every other '
                 'field, including the Unicode banner, nested metadata and routing. '
                 'Do not change protected.txt or any other files.')
        prompt = ('Apply the earlier constraints to settings.json using read_file and write_file. '
                  'For write_file on this .json target, content may be the complete JSON object or a serialized JSON string. '
                  'Do not use bash or modify any other files. '
                  'Finish with a short confirmation.')
    events = [{'e': 'user', 'text': task}]
    # Alternate real conversation roles. Bulk is long enough to exercise the
    # excerpt compactor without consuming model requests to create history.
    for index in range(8):
        events += [{'e': 'assistant', 'text': f'Historical bulk {index}: ' + ('inert-data ' * 600)},
                   {'e': 'user', 'text': f'Continue storing the task key; historical step {index}.'}]
    session = work / 'sessions' / 'fixture.jsonl'
    session.write_text(''.join(json.dumps(e) + '\n' for e in events), encoding='utf-8')
    return dict(work=work, session=session, config=config, policy=policy,
                threshold=threshold, marker=marker, case=case, prompt=prompt, expected_json=expected,
                expected_files=workspace_snapshot(work) if case == 'json-edit' else None)



def bounded_file(path):
    if path.is_symlink():
        raise ValueError('symlink artifact')
    descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_NONBLOCK', 0))
    with os.fdopen(descriptor, 'rb') as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError('non-regular artifact')
        value = source.read(65537)
    if len(value) > 65536:
        raise ValueError('oversized artifact')
    return value



def workspace_snapshot(work):
    result, pending, count = {}, [work], 0
    ignored = {'settings.json', 'sessions', 'harness/goals.jsonl', 'harness/.sigil.lock'}
    while pending:
        for path in pending.pop().iterdir():
            count += 1
            if count > 256:
                raise ValueError('too many workspace entries')
            relative = path.relative_to(work).as_posix()
            if relative in ignored:
                continue
            if path.is_symlink():
                raise ValueError('symlink workspace entry')
            if path.is_dir():
                result[relative] = 'directory'
                pending.append(path)
            else:
                result[relative] = hashlib.sha256(bounded_file(path)).hexdigest()
    return result


def workflow_correct(trial, events):
    seen_read, seen_write = False, False
    for event in events:
        if event.get('e') != 'tool_call':
            continue
        name = event.get('name')
        if name not in ('read_file', 'write_file'):
            return False
        try:
            args = json.loads(event['args'])
            target = Path(args['path'])
            if not target.is_absolute():
                target = trial['work'] / target
            if target.resolve() != (trial['work'] / 'settings.json').resolve():
                return False
        except (KeyError, TypeError, ValueError, OSError):
            return False
        if name == 'read_file':
            seen_read = True
        elif not seen_read:
            return False
        else:
            seen_write = True
    return seen_read and seen_write


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate JSON key')
        result[key] = value
    return result


def artifact_correct(trial):
    try:
        raw = bounded_file(trial['work'] / 'settings.json')
        actual = json.loads(raw, object_pairs_hook=unique_keys)
        # Canonical JSON also distinguishes booleans from integer lookalikes.
        equal = json.dumps(actual, sort_keys=True, allow_nan=False) == json.dumps(
            trial['expected_json'], sort_keys=True, allow_nan=False)
        return (equal and bounded_file(trial['work'] / 'protected.txt') == b'leave this file unchanged\n'
                and workspace_snapshot(trial['work']) == trial['expected_files'])
    except (OSError, ValueError, RecursionError):
        return False


def correct(answer, expected, returncode):
    return returncode == 0 and answer.strip() == expected


def execute_trial(binary, trial, timeout, env, runner=subprocess.run, model=None, base_url=None):
    work = trial['work']
    command = [str(binary), '--config', str(trial['config']),
               '--sessions-dir', str(work / 'sessions'), '--resume', 'fixture',
               '--harness-dir', str(work / 'harness'), '--tools-dir', str(work / 'tools'),
               '--hooks-dir', str(work / 'hooks'), '--delegates-dir', str(work / 'delegates'),
               '--memory', str(work / 'memory.md'), '--max-steps', '3']
    if model:
        command += ['--model', model]
    if base_url:
        command += ['--base-url', base_url]
    started = time.monotonic()
    status, passed = 'process_error', False
    try:
        process = runner(command, input=trial['prompt'],
                         capture_output=True, text=True, timeout=timeout, cwd=work, env=env)
        passed = (process.returncode == 0 and artifact_correct(trial)) if trial['case'] == 'json-edit' \
            else correct(process.stdout, trial['marker'], process.returncode)
        status = 'pass' if passed else ('wrong_answer' if process.returncode == 0 else 'process_error')
    except subprocess.TimeoutExpired:
        status = 'timeout'
    except OSError:
        status = 'launch_error'
    events = []
    observation_status = 'valid'
    try:
        for event in read_events([trial['session']]):
            events.append(event)
    except (OSError, ValueError):
        observation_status = 'invalid_or_unreadable'
    workflow_passed = None
    if trial['case'] == 'json-edit':
        workflow_passed = observation_status == 'valid' and workflow_correct(trial, events)
        passed = passed and workflow_passed
        if status not in ('timeout', 'process_error', 'launch_error'):
            status = 'pass' if passed else 'wrong_artifact'
    usage = summarize(events)
    if observation_status != 'valid' or status in ('timeout', 'process_error', 'launch_error'):
        # A killed/failed process can have an unlogged in-flight request.
        # Preserve known observations, but never infer its unobserved cost.
        usage['total']['usage_complete'] = False
        for group in usage['groups']:
            group['usage_complete'] = False
    return dict(fixture_version=3 if trial['case'] == 'json-edit' else 1, case=trial['case'], policy=trial['policy'], threshold=trial['threshold'], status=status,
                passed=passed, workflow_passed=workflow_passed, elapsed_seconds=round(time.monotonic() - started, 3),
                usage_observation_status=observation_status, usage=usage)



def policy_summary(results, policy):
    selected = [r for r in results if r['policy'] == policy]
    successes = sum(r['passed'] for r in selected)
    complete = bool(selected) and all(r['usage']['total']['usage_complete'] for r in selected)
    prompt = sum(r['usage']['total']['prompt_tokens'] for r in selected)
    completion = sum(r['usage']['total']['completion_tokens'] for r in selected)
    requests = sum(r['usage']['total']['requests'] for r in selected)
    reported = sum(r['usage']['total']['cache_reported_requests'] for r in selected)
    covered = sum(r['usage']['total']['cache_reported_prompt_tokens'] for r in selected)
    hits = sum(r['usage']['total']['prompt_cache_hit_tokens'] for r in selected)
    return dict(requests=requests, cache_reported_requests=reported,
                cache_hit_rate=hits / covered if covered else None,
                cache_coverage_rate=reported / requests if requests else None,
                tasks=len(selected), successful_tasks=successes,
                success_rate=successes / len(selected) if selected else None,
                observed_prompt_tokens=prompt, observed_completion_tokens=completion,
                usage_complete=complete,
                observed_tokens_per_success=(prompt + completion) / successes
                if complete and successes else None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--case', choices=('retention', 'json-edit'), default='retention')
    parser.add_argument('--execute', action='store_true', help='actually run sigil/model requests')
    parser.add_argument('--binary', type=Path, default=ROOT / 'target' / 'release' / 'sigil')
    parser.add_argument('--output', type=Path, default=Path('context-evaluation'))
    parser.add_argument('--threshold', type=int, default=8000)
    parser.add_argument('--repetitions', type=int, default=2)
    parser.add_argument('--timeout', type=float, default=90)
    parser.add_argument('--model', help='explicit model for both policies')
    parser.add_argument('--base-url', help='explicit endpoint for both policies; never include credentials')
    args = parser.parse_args()
    if args.threshold <= 0 or args.repetitions <= 0 or args.timeout <= 0:
        parser.error('threshold, repetitions and timeout must be positive')
    if not args.execute:
        print(f'Plan: {args.case}, {args.repetitions} paired trials; disabled vs {args.threshold} input tokens. '
              'Add --execute to run model requests. Provider cache state is uncontrolled.')
        return 0
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error('build sigil or provide an existing --binary')
    # Do not read/copy user config files containing credentials. Primary model
    # settings use CLI/env/defaults; fallback endpoints are disabled for clarity.
    env = {k: v for k, v in os.environ.items() if not k.startswith('SIGIL_FALLBACK_')}
    output = args.output.resolve()
    try:
        with binary.open('rb') as source:
            binary_sha256 = hashlib.file_digest(source, 'sha256').hexdigest()
        output.mkdir(parents=True, exist_ok=False)
        results = []
        for index in range(args.repetitions):
            pair = output / f'pair-{index + 1}'
            marker = 'key-' + uuid.uuid4().hex
            trials = [prepare_trial(pair, 'off', 0, marker, case=args.case),
                      prepare_trial(pair, 'on', args.threshold, marker, case=args.case)]
            if index % 2:
                trials.reverse()
            for trial in trials:
                result = execute_trial(binary, trial, args.timeout, env, model=args.model, base_url=args.base_url)
                result['pair'] = index + 1
                results.append(result)
                print(f"pair {index + 1} {trial['policy']}: {result['status']}", flush=True)
                # Preserve completed observations even if a later trial fails.
                report = dict(fixture=args.case, fixture_version=3 if args.case == 'json-edit' else 1, cache_state='uncontrolled',
                              binary_sha256=binary_sha256, supplied_model=args.model,
                              results=results, summary={p: policy_summary(results, p) for p in ('off', 'on')},
                              billing='unmeasured', compaction_application='not inferred from usage')
                temporary = output / 'report.json.tmp'
                temporary.write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
                temporary.replace(output / 'report.json')
    except (OSError, ValueError):
        print('Evaluation failed: output must be new and session observations must be valid.')
        return 1
    print(f'Report: {output / "report.json"}. Compare provider bills separately.')
    return 0 if all(r['passed'] and r['usage_observation_status'] == 'valid' for r in results) else 1


if __name__ == '__main__':
    raise SystemExit(main())
