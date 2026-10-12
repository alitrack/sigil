"""Bounded declarative DREAM policies and offline replay; no execution or I/O.

Stagnation counts consecutive non-improving edges between non-root ancestors.
The initial root-to-branch edge is excluded. Branch width caps opened branches,
not the number of resumable leaves. Batches contain parent IDs.
"""
import copy
import math
import re

BASELINE = dict(version=1, name='baseline', branch_width=2, ranking='score',
                stagnation=0, stop_score=None, max_attempts=8)
_ID = re.compile(r'[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}\Z')


def _invalid(kind):
    raise ValueError('Invalid ' + kind)


def _integer(value, low, high):
    return type(value) is int and low <= value <= high


def _number(value, low=0, high=1):
    return type(value) in (int, float) and low <= value <= high and math.isfinite(value)


def _identifier(value):
    return isinstance(value, str) and _ID.fullmatch(value) is not None


def validate_policy(data):
    """Validate exactly the fixed policy schema and return an independent copy."""
    if not isinstance(data, dict) or set(data) != set(BASELINE):
        _invalid('policy')
    if (type(data['version']) is not int or data['version'] != 1
            or not _identifier(data['name'])
            or not _integer(data['branch_width'], 1, 32)
            or data['ranking'] not in ('score', 'depth', 'breadth')
            or not _integer(data['stagnation'], 0, 64)
            or (data['stop_score'] is not None and not _number(data['stop_score']))
            or not _integer(data['max_attempts'], 1, 256)):
        _invalid('policy')
    return copy.deepcopy(data)


def _reference(value):
    return (value is None or isinstance(value, str) and 0 < len(value) <= 1024
            and '\\' not in value and ':' not in value and '\x00' not in value
            and all(part not in ('', '.', '..') for part in value.split('/')))


def _nodes(records):
    if not isinstance(records, list) or not 1 <= len(records) <= 4096:
        _invalid('world')
    seen, children = {}, {}
    for index, record in enumerate(records):
        if not isinstance(record, dict):
            _invalid('world')
        ident, parent = record.get('id'), record.get('parent')
        if (not _identifier(ident) or ident in seen
                or not _number(record.get('score'))
                or type(record.get('resumable')) is not bool
                or not isinstance(record.get('diagnostics'), list)
                or len(record['diagnostics']) > 64
                or any(not isinstance(d, str) or len(d) > 4096 for d in record['diagnostics'])):
            _invalid('world')
        if index == 0:
            if ident != 'root' or parent is not None:
                _invalid('world')
        elif not isinstance(parent, str) or parent not in seen:
            _invalid('world')
        if parent is not None:
            if not seen[parent]['resumable']:
                _invalid('world')
            children.setdefault(parent, []).append(ident)
            if parent != 'root' and len(children[parent]) > 1:
                _invalid('world')
        for key in ('snapshot', 'session', 'snapshot_ref', 'session_ref'):
            if key in record and not _reference(record[key]):
                _invalid('world')
        seen[ident] = record
    return seen, children


def validate_world(world):
    """Validate recorded lineage and safe references without dereferencing them."""
    if not isinstance(world, dict) or type(world.get('version')) is not int or world['version'] != 1 or world.get('root') != 'root':
        _invalid('world')
    if 'source' in world and world['source'] not in ('online-discovery', 'simulated'):
        _invalid('world source')
    compatibility = world.get('compatibility')
    if (not isinstance(compatibility, dict)
            or not {'task_family', 'evaluator_version', 'model', 'binary_sha256'} <= set(compatibility)
            or not set(compatibility) <= {'task_family', 'evaluator_version', 'model', 'binary_sha256', 'base_url', 'harness_sha256'}
            or compatibility['task_family'] != 'json-edit'
            or type(compatibility['evaluator_version']) is not int
            or compatibility['evaluator_version'] != 1
            or not isinstance(compatibility['model'], str)
            or not 1 <= len(compatibility['model']) <= 256
            or not isinstance(compatibility['binary_sha256'], str)
            or re.fullmatch('[0-9a-fA-F]{64}', compatibility['binary_sha256']) is None):
        _invalid('world')
    if 'base_url' in compatibility and (not isinstance(compatibility['base_url'], str) or not 1 <= len(compatibility['base_url']) <= 2048):
        _invalid('world')
    if 'harness_sha256' in compatibility and (not isinstance(compatibility['harness_sha256'], str) or re.fullmatch('[0-9a-fA-F]{64}', compatibility['harness_sha256']) is None):
        _invalid('world')
    _nodes(world.get('nodes'))
    return copy.deepcopy(world)


def termination_reason(policy, revealed_nodes, decision_rounds, max_rounds):
    """Shared stopping priority: observed success, attempt cap, then round cap."""
    policy = validate_policy(policy)
    seen, _ = _nodes(revealed_nodes)
    if not _integer(decision_rounds, 0, 256) or not _integer(max_rounds, 1, 256):
        _invalid('replay limits')
    if policy['stop_score'] is not None and max(n['score'] for n in seen.values()) >= policy['stop_score']:
        return 'policy_stop'
    if len(revealed_nodes) - 1 >= policy['max_attempts']:
        return 'attempts_limit'
    if decision_rounds >= max_rounds:
        return 'rounds_limit'
    return None


def select_batch(policy, revealed_nodes, eligible, workers=1):
    """Choose only from observed resumable leaves and supplied availability IDs."""
    policy = validate_policy(policy)
    if not _integer(workers, 1, 32):
        _invalid('workers')
    seen, children = _nodes(revealed_nodes)
    if not isinstance(eligible, list) or len(eligible) > 4096 or any(not isinstance(i, str) or i not in seen for i in eligible):
        _invalid('availability')
    if len(revealed_nodes) - 1 >= policy['max_attempts']:
        return []
    if policy['stop_score'] is not None and max(n['score'] for n in seen.values()) >= policy['stop_score']:
        return []
    depths, stale = {'root': 0}, {'root': 0}
    for n in revealed_nodes[1:]:
        parent = n['parent']
        depths[n['id']] = depths[parent] + 1
        stale[n['id']] = (stale[parent] + 1 if parent != 'root' and n['score'] <= seen[parent]['score'] else 0)
    leaves = [i for i in set(eligible) if i != 'root' and i not in children and seen[i]['resumable']
              and (not policy['stagnation'] or stale[i] < policy['stagnation'])]
    ranking = policy['ranking']
    if ranking == 'score':
        leaves.sort(key=lambda i: (-seen[i]['score'], -depths[i], i))
    else:
        leaves.sort(key=lambda i: ((-depths[i] if ranking == 'depth' else depths[i]), i))
    # Open one root branch per batch until the fixed branch quota is filled.
    if 'root' in eligible and seen['root']['resumable'] and len(children.get('root', [])) < policy['branch_width']:
        leaves.insert(0, 'root')
    return leaves[:min(workers, policy['max_attempts'] - len(revealed_nodes) + 1)]


def replay(world, policy, workers=1, max_rounds=64, beta1=.01, beta2=0):
    """Reveal historical outcomes after each complete batch, entirely offline."""
    world, policy = validate_world(world), validate_policy(policy)
    if not _integer(workers, 1, 32) or not _integer(max_rounds, 1, 256):
        _invalid('replay limits')
    if not _number(beta1, 0, 1e6) or not _number(beta2, 0, 1e6):
        _invalid('objective coefficients')
    seen, children = _nodes(world['nodes'])
    revealed, reached, batches = [seen['root']], ['root'], []
    consumed = {}
    while True:
        attempts, rounds = len(reached) - 1, len(batches)
        best = max(n['score'] for n in revealed)
        reason = termination_reason(policy, revealed, rounds, max_rounds)
        if reason is not None:
            break
        _, revealed_children = _nodes(revealed)
        available = [n['id'] for n in revealed if n['resumable']
                     and (n['id'] == 'root' or n['id'] not in revealed_children)
                     and consumed.get(n['id'], 0) < len(children.get(n['id'], []))]
        batch = select_batch(policy, revealed, available, workers)
        if not batch:
            online_eligible = [n['id'] for n in revealed if n['resumable']
                               and (n['id'] == 'root' or n['id'] not in revealed_children)]
            reason = ('support_exhausted' if select_batch(policy, revealed, online_eligible, workers)
                      else 'policy_stop')
            break
        additions = []
        for parent in batch:
            child = children[parent][consumed.get(parent, 0)]
            consumed[parent] = consumed.get(parent, 0) + 1
            additions.append(seen[child])
        batches.append(batch)
        reached.extend(n['id'] for n in additions)
        revealed.extend(additions)
    return dict(reached_nodes=reached, batches=batches, best_quality=best,
                represented_attempts=attempts, decision_rounds=rounds,
                objective=best-beta1*attempts+beta2*attempts/max(1, rounds),
                stop_reason=reason, workers=workers, max_rounds=max_rounds,
                beta1=beta1, beta2=beta2)


def select_policy(worlds, incumbent, candidates, **replay_kwargs):
    """Retain incumbent unless a valid candidate strictly improves pool mean."""
    if not isinstance(worlds, list) or not 1 <= len(worlds) <= 256:
        _invalid('world pool')
    worlds = [validate_world(w) for w in worlds]
    if any(w.get('source') != worlds[0].get('source') for w in worlds):
        _invalid('world sources')
    if any(w['compatibility'] != worlds[0]['compatibility'] for w in worlds):
        _invalid('world compatibility')
    incumbent = validate_policy(incumbent)
    if not isinstance(candidates, list) or len(candidates) > 256:
        _invalid('candidate pool')
    comparisons = []
    selected = incumbent
    incumbent_score = selected_score = None
    for index, candidate in enumerate([incumbent] + candidates):
        try:
            normalized = validate_policy(candidate)
            reports = [replay(w, normalized, **replay_kwargs) for w in worlds]
            score = sum(r['objective'] for r in reports)/len(reports)
            comparisons.append(dict(policy=normalized, eligible=True, score=score, replays=reports))
        except (ValueError, TypeError, OverflowError):
            if index == 0:
                _invalid('incumbent replay')
            comparisons.append(dict(policy=None, eligible=False, score=None, rejection_reason='Invalid candidate or replay'))
            continue
        if index == 0:
            incumbent_score = selected_score = score
        elif score > selected_score:
            selected, selected_score = normalized, score
    return dict(selected_policy=selected, incumbent_score=incumbent_score,
                selected_score=selected_score, comparisons=comparisons)


def candidate_policies(incumbent):
    """Bounded parameter sweep; policies are data, never arbitrary programs."""
    incumbent = validate_policy(incumbent)
    policies = [incumbent]
    for ranking in ('score', 'depth', 'breadth'):
        for width in (1, 2, 4):
            for stagnation in (0, 2):
                for stop in (None, 1):
                    policy = dict(incumbent, name=f'{ranking}-w{width}-s{stagnation}-q{stop}',
                                  ranking=ranking, branch_width=width, stagnation=stagnation, stop_score=stop)
                    policies.append(validate_policy(policy))
    return policies
