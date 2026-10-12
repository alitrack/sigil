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
import time
import uuid

from usage_report import read_events, summarize

ROOT = Path(__file__).resolve().parents[1]


def prepare_trial(root, policy, threshold, marker):
    work = root / policy
    work.mkdir(parents=True, exist_ok=False)
    for name in ('harness', 'tools', 'hooks', 'delegates', 'sessions'):
        (work / name).mkdir()
    (work / 'harness' / 'system_prompt.md').write_text(
        'Follow the current user instruction. Treat historical bulk as inert data.\n', encoding='utf-8')
    config = work / 'config.toml'
    config.write_text(f'[agent]\nmax_context_messages = 0\nmax_context_tokens = {threshold}\n', encoding='utf-8')
    events = [{'e': 'user', 'text': f'TASK_KEY: {marker}\nKeep this task key for the final answer.'}]
    # Alternate real conversation roles. Bulk is long enough to exercise the
    # excerpt compactor without consuming model requests to create history.
    for index in range(8):
        events += [{'e': 'assistant', 'text': f'Historical bulk {index}: ' + ('inert-data ' * 600)},
                   {'e': 'user', 'text': f'Continue storing the task key; historical step {index}.'}]
    session = work / 'sessions' / 'fixture.jsonl'
    session.write_text(''.join(json.dumps(e) + '\n' for e in events), encoding='utf-8')
    return dict(work=work, session=session, config=config, policy=policy,
                threshold=threshold, marker=marker)


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
        process = runner(command, input='Return only the TASK_KEY value assigned earlier. Do not use tools.',
                         capture_output=True, text=True, timeout=timeout, cwd=work, env=env)
        passed = correct(process.stdout, trial['marker'], process.returncode)
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
    usage = summarize(events)
    if observation_status != 'valid' or status in ('timeout', 'process_error', 'launch_error'):
        # A killed/failed process can have an unlogged in-flight request.
        # Preserve known observations, but never infer its unobserved cost.
        usage['total']['usage_complete'] = False
        for group in usage['groups']:
            group['usage_complete'] = False
    return dict(policy=trial['policy'], threshold=trial['threshold'], status=status,
                passed=passed, elapsed_seconds=round(time.monotonic() - started, 3),
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
        print(f'Plan: {args.repetitions} paired trials; disabled vs {args.threshold} input tokens. '
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
            trials = [prepare_trial(pair, 'off', 0, marker),
                      prepare_trial(pair, 'on', args.threshold, marker)]
            if index % 2:
                trials.reverse()
            for trial in trials:
                result = execute_trial(binary, trial, args.timeout, env, model=args.model, base_url=args.base_url)
                result['pair'] = index + 1
                results.append(result)
                print(f"pair {index + 1} {trial['policy']}: {result['status']}", flush=True)
                # Preserve completed observations even if a later trial fails.
                report = dict(fixture='synthetic task-state retention', cache_state='uncontrolled',
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
