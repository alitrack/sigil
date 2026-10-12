#!/usr/bin/env python3
"""Analyze paired evaluator reports offline; estimates are not invoices.

Rates, when supplied, are currency units per million tokens. Without rates,
only observed token differences are reported. No network or model calls.
"""
import argparse
from decimal import Decimal, InvalidOperation, localcontext
import json
from pathlib import Path
import sys


def counter(total, key):
    value = total.get(key)
    if type(value) is not int or not 0 <= value <= (1 << 64) - 1:
        raise ValueError('invalid token/request counter')
    return value


def parse_rates(values, currency):
    if values is None:
        if currency is not None:
            raise ValueError('currency requires all three rates')
        return None
    if len(values) != 3 or not currency or not currency.isascii() or not currency.isalpha() or len(currency) > 8:
        raise ValueError('three rates and a short alphabetic currency/unit are required')
    try:
        rates = tuple(Decimal(str(v)) for v in values)
    except InvalidOperation as error:
        raise ValueError('invalid rate') from error
    if any(not r.is_finite() or r < 0 or r > Decimal('1e12') for r in rates):
        raise ValueError('rates must be finite, nonnegative and at most 1e12')
    if any(len(r.as_tuple().digits) > 28 or (r != 0 and not -36 <= r.as_tuple().exponent <= 12) for r in rates):
        raise ValueError('rates exceed supported decimal precision')
    return tuple(Decimal(0) if r == 0 else r for r in rates)


def analyze(report, values=None, currency=None):
    with localcontext() as context:
        context.prec = 96
        return _analyze(report, values, currency)


def _analyze(report, values=None, currency=None):
    rates = parse_rates(values, currency)
    rows = report.get('results')
    if not isinstance(rows, list) or not rows:
        raise ValueError('nonempty paired results required')
    models, seen, signatures = set(), set(), {}
    buckets = {p: dict(tasks=0, successes=0, requests=0, prompt=0, completion=0,
                       covered=0, hits=0, usage_complete=True) for p in ('off', 'on')}
    for row in rows:
        pair, policy = row.get('pair'), row.get('policy')
        if type(pair) is not int or pair <= 0 or policy not in buckets or (pair, policy) in seen:
            raise ValueError('invalid or duplicate pair member')
        seen.add((pair, policy))
        signatures[(pair, policy)] = (row.get('case'), row.get('fixture_version'))
        passed, status = row.get('passed'), row.get('status')
        if (status not in ('pass', 'wrong_answer', 'wrong_artifact', 'timeout', 'process_error', 'launch_error')
                or type(passed) is not bool or (passed != (status == 'pass'))):
            raise ValueError('inconsistent outcome')
        total = row['usage']['total']
        prompt = counter(total, 'prompt_tokens')
        completion = counter(total, 'completion_tokens')
        requests = counter(total, 'requests')
        reported = counter(total, 'usage_reported_requests')
        if reported > requests:
            raise ValueError('reported requests exceed requests')
        coverage = total.get('cache_reported_prompt_tokens')
        # Old records without covered-token metadata remain unknown. Their
        # historical cache numerator is not promoted into trusted coverage.
        covered = counter(total, 'cache_reported_prompt_tokens') if coverage is not None else 0
        hits = counter(total, 'prompt_cache_hit_tokens') if coverage is not None else 0
        if not hits <= covered <= prompt:
            raise ValueError('cache counts must satisfy hits <= coverage <= input')
        if type(total.get('usage_complete')) is not bool:
            raise ValueError('usage completeness must be boolean')
        complete = (total['usage_complete'] and requests > 0 and reported == requests
                    and status not in ('timeout', 'process_error', 'launch_error')
                    and row.get('usage_observation_status', 'valid') == 'valid')
        for group in row['usage'].get('groups', []):
            name = group.get('model')
            if isinstance(name, str) and name:
                models.add(name)
        bucket = buckets[policy]
        bucket['tasks'] += 1
        bucket['successes'] += passed
        bucket['requests'] += requests
        bucket['prompt'] += prompt
        bucket['completion'] += completion
        bucket['covered'] += covered
        bucket['hits'] += hits
        bucket['usage_complete'] &= complete
    for pair, policy in seen:
        if (pair, 'on' if policy == 'off' else 'off') not in seen:
            raise ValueError('every pair requires both policies')
        if signatures[(pair, 'off')] != signatures[(pair, 'on')]:
            raise ValueError('pair members must use the same fixture/version')
    if rates is not None and len(models) > 1:
        raise ValueError('one rate tuple cannot price mixed models')
    costs, policies = {}, {}
    for policy, b in buckets.items():
        split_known = b['covered'] == b['prompt']
        reason = 'rates_required' if rates is None else 'incomplete_usage'
        cost = None
        if rates is not None and b['usage_complete']:
            ru, rc, ro = rates
            if split_known:
                cost = ((b['prompt'] - b['hits']) * ru + b['hits'] * rc + b['completion'] * ro) / Decimal(1000000)
            elif ru == rc:
                cost = (b['prompt'] * ru + b['completion'] * ro) / Decimal(1000000)
            reason = 'estimated' if cost is not None else 'unknown_cache_split'
        costs[policy] = cost
        policies[policy] = dict(tasks=b['tasks'], successful_tasks=b['successes'],
                               requests=b['requests'], usage_complete=b['usage_complete'],
                               cache_split_known=split_known, estimate_status=reason,
                               estimated_cost=str(cost) if cost is not None else None,
                               estimated_cost_per_success=str(cost / b['successes'])
                               if cost is not None and b['successes'] else None)
    off, on = buckets['off'], buckets['on']
    split_known = all(b['covered'] == b['prompt'] for b in buckets.values())
    delta = dict(prompt=on['prompt'] - off['prompt'], completion=on['completion'] - off['completion'],
                 cached_input=on['hits'] - off['hits'] if split_known else None,
                 uncached_input=(on['prompt'] - on['hits']) - (off['prompt'] - off['hits']) if split_known else None,
                 observed_only=not all(b['usage_complete'] for b in buckets.values()))
    difference = costs['on'] - costs['off'] if all(c is not None for c in costs.values()) else None
    return dict(currency=currency, rates_per_million=[str(r) for r in rates] if rates else None,
                models=sorted(models), policies=policies, token_delta=delta,
                estimated_on_minus_off=str(difference) if difference is not None else None,
                charge_relation=(f"({delta['uncached_input']} * Ru + {delta['cached_input']} * Rc + {delta['completion']} * Ro) / 1000000"
                                 if split_known and not delta['observed_only'] else None),
                billing='unmeasured; estimates exclude unreported and non-token charges')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('report', type=Path)
    parser.add_argument('--uncached-input-rate')
    parser.add_argument('--cached-input-rate')
    parser.add_argument('--output-rate')
    parser.add_argument('--currency')
    args = parser.parse_args()
    values = (args.uncached_input_rate, args.cached_input_rate, args.output_rate)
    if any(v is not None for v in values) and not all(v is not None for v in values):
        parser.error('provide all three rates together')
    try:
        report = json.loads(args.report.read_text(encoding='utf-8'))
        if not isinstance(report, dict):
            raise ValueError('report must be an object')
        result = analyze(report, values if all(v is not None for v in values) else None, args.currency)
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        print('Invalid report or explicit prices; no estimate produced.', file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
