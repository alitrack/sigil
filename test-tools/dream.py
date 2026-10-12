#!/usr/bin/env python3
"""Opt-in DREAM discovery, offline policy replay and recursive redeployment."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import sys

from dream_core import BASELINE, candidate_policies, select_policy, validate_policy, validate_world
from dream_online import atomic_json, collect_world, effective_endpoint, make_task, read_json

ROOT = Path(__file__).resolve().parents[1]


def policy_fingerprint(policy):
    return hashlib.sha256(json.dumps(policy, sort_keys=True).encode()).hexdigest()


def recursive_run(output, binary, policy, model, base_url, candidates, rounds=2,
                  decision_rounds=4, timeout=90, beta1=.01, beta2=0, env=None,
                  source='online-discovery'):
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    incumbent = validate_policy(policy)
    worlds = []
    report = {'version': 1, 'status': 'running', 'rounds': [], 'online_workers': 1,
              'policy_family': 'bounded-declarative-v1', 'beta1': beta1, 'beta2': beta2,
              'source': source, 'initial_policy': incumbent}
    atomic_json(output / 'run.json', report)
    atomic_json(output / 'selected-policy.json', incumbent)
    try:
        for index in range(rounds):
            directory = output / f'round-{index + 1}'
            world = collect_world(directory, binary, incumbent, model, base_url, env,
                                  max_rounds=decision_rounds, timeout=timeout, seed=f'dream-round-{index + 1}',
                                  source=source)
            worlds.append(world)
            atomic_json(directory / 'incumbent-policy.json', incumbent)
            row = {'round': index + 1, 'world': f'round-{index + 1}/world.json',
                   'incumbent_policy': incumbent, 'incumbent_sha256': policy_fingerprint(incumbent),
                   'attempts': len(world['nodes']) - 1, 'selected_policy': None}
            report['rounds'].append(row)
            atomic_json(output / 'run.json', report)
            selection = select_policy(worlds, incumbent, candidate_policies(incumbent) + candidates,
                                      workers=1, max_rounds=decision_rounds, beta1=beta1, beta2=beta2)
            selection['worlds'] = [{'round': i + 1, 'source': item['source'],
                                   'sha256': hashlib.sha256(json.dumps(item, sort_keys=True).encode()).hexdigest()}
                                  for i, item in enumerate(worlds)]
            atomic_json(directory / 'replay.json', selection)
            incumbent = selection['selected_policy']
            atomic_json(directory / 'selected-policy.json', incumbent)
            row.update(selected_policy=incumbent, selected_sha256=policy_fingerprint(incumbent),
                       incumbent_score=selection['incumbent_score'], selected_score=selection['selected_score'])
            atomic_json(output / 'selected-policy.json', incumbent)
            atomic_json(output / 'run.json', report)
        report['status'] = 'completed'
    except BaseException:
        report['status'] = 'interrupted'
        atomic_json(output / 'run.json', report)
        raise
    atomic_json(output / 'run.json', report)
    return report


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest='command', required=True)
    for name in ('explore', 'run', 'replay'):
        command = commands.add_parser(name)
        command.add_argument('--policy', type=Path, help='incumbent JSON policy; default is the fixed baseline')
        command.add_argument('--candidate', action='append', type=Path, default=[], help='additional candidate JSON policy')
        command.add_argument('--decision-rounds', type=int, default=4)
        command.add_argument('--beta1', type=float, default=.01, help='historical attempt penalty, not currency')
        command.add_argument('--beta2', type=float, default=0, help='batching bonus')
        if name == 'replay':
            command.add_argument('--output', type=Path, help='new directory for atomic comparison and selected-policy files')
            command.add_argument('--world', action='append', type=Path, required=True)
            command.add_argument('--workers', type=int, default=1)
        else:
            command.add_argument('--execute', action='store_true', help='execute sigil and contact the configured model')
            command.add_argument('--output', type=Path, required=True, help='new experiment directory')
            command.add_argument('--binary', type=Path, default=ROOT / 'target' / 'release' / 'sigil')
            command.add_argument('--model', default=os.getenv('SIGIL_MODEL'))
            command.add_argument('--base-url', default=os.getenv('SIGIL_BASE_URL'))
            command.add_argument('--attempts', type=int, help='override policy max_attempts')
            command.add_argument('--timeout', type=float, default=90)
            command.add_argument('--source', choices=('online-discovery', 'simulated'), default='online-discovery',
                                 help='label simulated providers explicitly')
            if name == 'run':
                command.add_argument('--rounds', type=int, default=2)
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        if not 1 <= args.decision_rounds <= 256:
            raise ValueError('invalid decision limit')
        if any(not math.isfinite(value) or not 0 <= value <= 1e6 for value in (args.beta1, args.beta2)):
            raise ValueError('invalid objective')
        incumbent = validate_policy(read_json(args.policy, 65536) if args.policy else BASELINE)
        if len(args.candidate) > 200:
            raise ValueError('excessive candidates')
        candidates = [read_json(path, 65536) for path in args.candidate]
        if args.command == 'replay':
            if not 1 <= args.workers <= 32 or len(args.world) > 256:
                raise ValueError('invalid replay limits')
            paths = [path.resolve() for path in args.world]
            if len(set(paths)) != len(paths):
                raise ValueError('duplicate worlds')
            worlds = [validate_world(read_json(path)) for path in paths]
            report = select_policy(worlds, incumbent, candidate_policies(incumbent) + candidates,
                                   workers=args.workers, max_rounds=args.decision_rounds,
                                   beta1=args.beta1, beta2=args.beta2)
            report['worlds'] = [{'path': str(path), 'source': item.get('source', 'unspecified'),
                                 'sha256': hashlib.sha256(json.dumps(item, sort_keys=True).encode()).hexdigest()}
                                for path, item in zip(paths, worlds)]
            if args.output:
                output = args.output.resolve()
                output.mkdir(parents=True, exist_ok=False)
                atomic_json(output / 'replay.json', report)
                atomic_json(output / 'selected-policy.json', report['selected_policy'])
        else:
            args.base_url = effective_endpoint(args.base_url, None)
            if args.attempts is not None:
                incumbent = validate_policy(dict(incumbent, max_attempts=args.attempts))
            rounds = args.rounds if args.command == 'run' else 1
            if not 1 <= rounds <= 32 or not math.isfinite(args.timeout) or not 0 < args.timeout <= 600:
                raise ValueError('invalid online limits')
            if not args.execute:
                report = {'execute': False, 'command': args.command, 'outer_rounds': rounds,
                          'decision_rounds': args.decision_rounds, 'online_workers': 1,
                          'max_agent_attempts': rounds * args.decision_rounds,
                          'timeout_seconds_per_attempt': args.timeout, 'policy': incumbent,
                          'binary': str(args.binary.resolve()), 'model': args.model, 'base_url': args.base_url,
                          'output': str(args.output.resolve()), 'fixture': 'json-edit-v1',
                          'next_step': 'Add --execute to collect new discovery worlds.'}
                report['tasks'] = [make_task(f'dream-round-{index + 1}' if args.command == 'run' else 'dream')
                                   for index in range(rounds)]
                report['source'] = args.source
            else:
                if not args.model:
                    raise ValueError('explicit model required')
                if args.command == 'explore':
                    world = collect_world(args.output, args.binary, incumbent, args.model, args.base_url,
                                          max_rounds=args.decision_rounds, timeout=args.timeout, source=args.source)
                    report = {'world': str(args.output.resolve() / 'world.json'),
                              'attempts': len(world['nodes']) - 1,
                              'best_quality': max(node['score'] for node in world['nodes'])}
                else:
                    run = recursive_run(args.output, args.binary, incumbent, args.model, args.base_url,
                                        candidates, rounds, args.decision_rounds, args.timeout,
                                        args.beta1, args.beta2, source=args.source)
                    report = {'status': run['status'], 'outer_rounds': len(run['rounds']),
                              'report': str(args.output.resolve() / 'run.json'),
                              'selected_policy': run['rounds'][-1]['selected_policy']}
        print(json.dumps(report, indent=2, ensure_ascii=False, allow_nan=False))
        return 0
    except (OSError, ValueError, TypeError, KeyError, RecursionError):
        print('DREAM failed: verify bounded inputs, matching worlds, model and a new output directory.', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
