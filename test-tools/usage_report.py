#!/usr/bin/env python3
"""Report observed request tokens from sigil JSONL sessions, without model calls.

Totals are observed subsets, not bills. Cache hit rate uses only prompt tokens
whose cache coverage was reported. Never print conversational event content.
"""
import argparse
import json
from pathlib import Path
import sys


def counter(usage, name):
    value = usage.get(name)
    if type(value) is not int or value < 0:
        raise ValueError(f'invalid {name}: expected a nonnegative integer')
    return value


def empty_totals():
    return dict(requests=0, usage_reported_requests=0, cache_reported_requests=0,
                prompt_tokens=0, completion_tokens=0,
                cache_reported_prompt_tokens=0, prompt_cache_hit_tokens=0)


def add_usage(total, usage):
    total['requests'] += 1
    if usage is None:
        return
    if not isinstance(usage, dict):
        raise ValueError('usage must be an object or null')
    prompt = counter(usage, 'prompt_tokens')
    completion = counter(usage, 'completion_tokens')
    coverage = usage.get('cache_reported_prompt_tokens')
    if coverage is not None:
        coverage = counter(usage, 'cache_reported_prompt_tokens')
        hits = counter({'prompt_cache_hit_tokens': 0, **usage}, 'prompt_cache_hit_tokens')
        if not hits <= coverage <= prompt:
            raise ValueError('cache counters must satisfy hits <= coverage <= prompt')
        total['cache_reported_requests'] += 1
        total['cache_reported_prompt_tokens'] += coverage
        total['prompt_cache_hit_tokens'] += hits
    total['usage_reported_requests'] += 1
    total['prompt_tokens'] += prompt
    total['completion_tokens'] += completion


def finish(total):
    requests = total['requests']
    coverage = total['cache_reported_prompt_tokens']
    return dict(total,
                usage_complete=bool(requests and total['usage_reported_requests'] == requests),
                cache_coverage_rate=(total['cache_reported_requests'] / requests if requests else None),
                cache_hit_rate=(total['prompt_cache_hit_tokens'] / coverage if coverage else None))


def summarize(events):
    """Aggregate request_usage events; unrelated events are ignored."""
    total = empty_totals()
    groups = {}
    for event in events:
        if not isinstance(event, dict):
            raise ValueError('event must be an object')
        if event.get('e') != 'request_usage':
            continue
        model = event.get('model')
        kind = event.get('request_kind')
        if not isinstance(model, str) or not isinstance(kind, str) or 'usage' not in event:
            raise ValueError('request_usage requires model, request_kind, and usage')
        add_usage(total, event['usage'])
        add_usage(groups.setdefault((model, kind), empty_totals()), event['usage'])
    return {'total': finish(total), 'groups': [
        dict(model=model, request_kind=kind, **finish(values))
        for (model, kind), values in sorted(groups.items())]}


def read_events(paths):
    seen = set()
    for path in paths:
        resolved = path.resolve()
        if resolved in seen:
            continue
        seen.add(resolved)
        with path.open('rb') as source:
            for number, line in enumerate(source, 1):
                if not line.strip():
                    continue
                try:
                    decoded = line.decode('utf-8')
                except UnicodeError as error:
                    raise ValueError(f'{path}:{number}: invalid UTF-8') from error
                try:
                    event = json.loads(decoded)
                    # Validate usage here so errors can identify a line without
                    # disclosing any of its contents.
                    summarize([event])
                except (ValueError, TypeError) as error:
                    raise ValueError(f'{path}:{number}: invalid event or usage') from error
                yield event


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--json', action='store_true', help='emit machine-readable totals')
    parser.add_argument('sessions', type=Path, nargs='+', help='explicit JSONL session paths')
    args = parser.parse_args()
    try:
        report = summarize(read_events(args.sessions))
    except (OSError, ValueError) as error:
        print(f'usage report: {error}', file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(report, ensure_ascii=True, indent=2))
    else:
        total = report['total']
        print(f"Requests: {total['requests']}; usage reported: {total['usage_reported_requests']}")
        print(f"Observed tokens: {total['prompt_tokens']} prompt + {total['completion_tokens']} completion")
        rate = total['cache_hit_rate']
        print('Cache hit rate (reported subset): ' + (f'{rate:.2%}' if rate is not None else 'unavailable'))
        print(f"Cache reporting: {total['cache_reported_requests']}/{total['requests']} requests")
        print('Observed tokens only; compare task outcomes and provider bills separately.')
    return 0


if __name__ == '__main__':
    sys.exit(main())
