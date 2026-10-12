"""Bounded online discovery around the unchanged sigil binary.

Local experiment directories isolate state, not operating-system permissions.
The fixed evaluator reads JSON; it never executes candidate code.
"""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import time
from urllib.parse import urlsplit

from context_eval import unique_keys
from dream_core import select_batch, termination_reason, validate_policy, validate_world

ARTIFACTS = {'settings.json', 'protected.txt'}
HARNESS = ('Use read_file and write_file to edit only settings.json as requested. '
           'Preserve all unrelated fields and protected.txt. Use relative paths. '
           'Do not create tools, change instructions, delegate, or run shell commands.\n')


def bounded_read(path, limit=65536):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0)
                         | getattr(os, 'O_NONBLOCK', 0))
    with os.fdopen(descriptor, 'rb') as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError('non-regular experiment file')
        raw = source.read(limit + 1)
    if len(raw) > limit:
        raise ValueError('oversized experiment file')
    return raw


def read_json(path, limit=4 * 1024 * 1024):
    try:
        return json.loads(bounded_read(path, limit), object_pairs_hook=unique_keys,
                          parse_constant=lambda _: (_ for _ in ()).throw(ValueError('non-finite JSON')))
    except (OSError, ValueError, RecursionError) as error:
        raise ValueError('invalid experiment JSON') from error


def atomic_json(path, value):
    path = Path(path)
    with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=path.parent,
                                     prefix='.dream-', delete=False) as file:
        temporary = Path(file.name)
        try:
            json.dump(value, file, indent=2, ensure_ascii=False, allow_nan=False)
            file.write('\n')
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    try:
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def make_task(seed):
    initial = {'service': {'name': 'fixture', 'retries': 1, 'port': 8042, 'enabled': False},
               'banner': '保留这段说明', 'metadata': {'owner': 'fixture-owner', 'revision': 7},
               'routing': {'mode': 'safe', 'routes': ['/health', '/status']}}
    expected = json.loads(json.dumps(initial))
    expected['service'].update(retries=4, port=9300 + hashlib.sha256(seed.encode()).digest()[0], enabled=True)
    return {'family': 'json-edit', 'evaluator_version': 1, 'initial': initial, 'expected': expected,
            'initial_files': {'settings.json': json.dumps(initial, ensure_ascii=False),
                              'protected.txt': 'leave this file unchanged\n'}}


def restore_snapshot(work, files):
    if not isinstance(files, dict) or set(files) != ARTIFACTS:
        raise ValueError('invalid snapshot manifest')
    if any(not isinstance(value, str) or len(value.encode('utf-8')) > 65536 for value in files.values()):
        raise ValueError('invalid snapshot content')
    work = Path(work)
    if work.is_symlink():
        raise ValueError('unsafe snapshot directory')
    work.mkdir(parents=True, exist_ok=False)
    for name, content in files.items():
        (work / name).write_text(content, encoding='utf-8')


def snapshot_files(work):
    work = Path(work)
    if work.is_symlink() or set(path.name for path in work.iterdir()) != ARTIFACTS:
        raise ValueError('unexpected workspace changes')
    try:
        return {name: bounded_read(work / name).decode('utf-8') for name in sorted(ARTIFACTS)}
    except (OSError, UnicodeError) as error:
        raise ValueError('unsafe snapshot artifact') from error


def snapshot_fingerprint(files):
    return hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest()


def effective_endpoint(base_url, env):
    supplied = os.environ if env is None else env
    endpoint = base_url if base_url is not None else supplied.get('SIGIL_BASE_URL')
    if endpoint is not None:
        if not isinstance(endpoint, str) or not 1 <= len(endpoint) <= 2048:
            raise ValueError('invalid endpoint')
        parsed = urlsplit(endpoint)
        if parsed.scheme not in ('http', 'https') or not parsed.hostname or parsed.username is not None or parsed.password is not None or parsed.query or parsed.fragment:
            raise ValueError('invalid endpoint')
    return endpoint


def canonical(value):
    return json.dumps(value, sort_keys=True, allow_nan=False)


def evaluate_artifact(work, task):
    try:
        files = snapshot_files(work)
        actual = json.loads(files['settings.json'], object_pairs_hook=unique_keys)
        expected = task['expected']
        preserved = json.loads(json.dumps(actual))
        for field in ('retries', 'port', 'enabled'):
            preserved['service'][field] = task['initial']['service'][field]
        if canonical(preserved) != canonical(task['initial']) or files['protected.txt'] != task['initial_files']['protected.txt']:
            return {'score': 0, 'passed': False, 'diagnostics': ['preservation_violation']}
        diagnostics = [f'service.{field}:incorrect' for field in ('retries', 'port', 'enabled')
                       if canonical(actual['service'].get(field)) != canonical(expected['service'][field])]
        return {'score': (3 - len(diagnostics)) / 3, 'passed': not diagnostics,
                'diagnostics': diagnostics or ['artifact_correct']}
    except (OSError, ValueError, TypeError, KeyError, RecursionError):
        return {'score': 0, 'passed': False, 'diagnostics': ['invalid_artifact']}


def state_fingerprint(state):
    if state.is_symlink():
        raise ValueError('unsafe agent state')
    ignored = {'sessions', 'home', 'harness/goals.jsonl', 'harness/.sigil.lock'}
    result, pending, count = {}, [state], 0
    while pending:
        for path in pending.pop().iterdir():
            count += 1
            if count > 256:
                raise ValueError('excessive agent state')
            relative = path.relative_to(state).as_posix()
            if path.is_symlink():
                raise ValueError('unsafe agent state')
            if relative in ignored:
                continue
            if path.is_dir():
                result[relative] = 'directory'
                pending.append(path)
            else:
                result[relative] = hashlib.sha256(bounded_read(path)).hexdigest()
    return result


def session_observations(state, work):
    sessions = state / 'sessions'
    if state.is_symlink() or sessions.is_symlink():
        raise ValueError('unsafe attempt session')
    paths = list(sessions.iterdir())
    if any(path.name != '.sigil.lock' and path.suffix != '.jsonl' for path in paths):
        raise ValueError('unexpected session artifact')
    if (sessions / '.sigil.lock').exists():
        bounded_read(sessions / '.sigil.lock')
    paths = [path for path in paths if path.suffix == '.jsonl']
    if len(paths) != 1 or paths[0].suffix != '.jsonl':
        raise ValueError('invalid attempt session')
    raw = bounded_read(paths[0], 4 * 1024 * 1024)
    lines = raw.splitlines()
    if len(lines) > 512:
        raise ValueError('excessive session records')
    events = [json.loads(line, object_pairs_hook=unique_keys) for line in lines]
    if any(not isinstance(event, dict) for event in events):
        raise ValueError('invalid session records')
    read, write = False, False
    for event in events:
        if event.get('e') != 'tool_call':
            continue
        name = event.get('name')
        if name not in ('read_file', 'write_file'):
            raise ValueError('unexpected tool')
        arguments = json.loads(event['args'], object_pairs_hook=unique_keys)
        target = Path(arguments['path'])
        if not target.is_absolute():
            target = work / target
        if target.resolve() != (work / 'settings.json').resolve():
            raise ValueError('unexpected tool path')
        if name == 'read_file':
            read = True
        elif not read:
            raise ValueError('write before read')
        else:
            write = True
    if not (read and write):
        raise ValueError('missing file workflow')
    return paths[0], sum(event.get('e') == 'request_usage' for event in events)


def execute_attempt(parent_files, task, directory, binary, model, base_url=None,
                    env=None, timeout=90, runner=None, ancestor_diagnostics=()):
    base_url = effective_endpoint(base_url, env)
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=False)
    work, state = directory / 'workspace', directory / 'state'
    restore_snapshot(work, parent_files)
    state.mkdir()
    for name in ('harness', 'tools', 'hooks', 'delegates', 'sessions', 'home'):
        (state / name).mkdir()
    (state / 'harness' / 'skills').mkdir()
    (state / 'harness' / 'system_prompt.md').write_text(HARNESS, encoding='utf-8')
    (state / 'memory.md').write_text('', encoding='utf-8')
    (state / 'config.toml').write_text('[agent]\nmax_context_messages = 0\nmax_context_tokens = 48000\n', encoding='utf-8')
    original_state = state_fingerprint(state)
    supplied_env = os.environ if env is None else env
    child_env = {key: value for key, value in supplied_env.items() if key in
                 {'PATH', 'SYSTEMROOT', 'WINDIR', 'TMPDIR', 'TEMP', 'TMP', 'SSL_CERT_FILE',
                  'SSL_CERT_DIR', 'SIGIL_API_KEY', 'HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY',
                  'https_proxy', 'http_proxy', 'all_proxy', 'NO_PROXY', 'no_proxy'}}
    child_env['HOME'] = str(state / 'home')
    command = [str(binary), '--config', str(state / 'config.toml'), '--model', model,
               '--harness-dir', str(state / 'harness'), '--tools-dir', str(state / 'tools'),
               '--hooks-dir', str(state / 'hooks'), '--delegates-dir', str(state / 'delegates'),
               '--sessions-dir', str(state / 'sessions'), '--memory', str(state / 'memory.md'),
               '--max-steps', '3']
    if base_url:
        command += ['--base-url', base_url]
    desired = task['expected']['service']
    prompt = (f'Edit settings.json: set service.retries to {desired["retries"]}, '
              f'service.port to {desired["port"]}, service.enabled to true. Preserve every other '
              'field and protected.txt. Read settings.json before writing it. Use only read_file '
              'and write_file, only settings.json, with relative paths. JSON content may be an object '
              'or serialized string. Finish with a short confirmation.\n'
              'Previous attempt diagnostics: ' + json.dumps(list(ancestor_diagnostics)[-8:]))
    started, status = time.monotonic(), 'process_error'
    try:
        process = (runner or subprocess.run)(command, input=prompt, cwd=work, env=child_env,
                                            timeout=timeout, stdout=subprocess.DEVNULL,
                                            stderr=subprocess.DEVNULL, text=True)
        status = 'completed' if process.returncode == 0 else 'process_error'
    except subprocess.TimeoutExpired:
        status = 'timeout'
    except OSError:
        status = 'launch_error'
    result = evaluate_artifact(work, task)
    session, requests, resumable = None, None, False
    try:
        session, requests = session_observations(state, work)
        if state_fingerprint(state) != original_state:
            raise ValueError('agent configuration changed')
        files = snapshot_files(work)
        restore_snapshot(directory / 'snapshot', files)
        resumable = 'preservation_violation' not in result['diagnostics']
    except (OSError, ValueError, TypeError, KeyError, RecursionError):
        result = {'score': 0, 'passed': False, 'diagnostics': ['invalid_observations_or_state']}
    if status != 'completed':
        result = {'score': 0, 'passed': False, 'diagnostics': [status]}
    result.update(process_status=status, resumable=resumable, usage_requests=requests,
                  elapsed_seconds=round(time.monotonic() - started, 3),
                  snapshot='snapshot' if resumable else None,
                  session=session.relative_to(directory).as_posix() if session else None)
    if resumable:
        result['snapshot_sha256'] = snapshot_fingerprint(files)
    return result


def collect_world(output, binary, policy, model, base_url=None, env=None,
                  max_rounds=8, timeout=90, seed='dream', runner=None, source='online-discovery'):
    policy = validate_policy(policy)
    base_url = effective_endpoint(base_url, env)
    if type(max_rounds) is not int or not 1 <= max_rounds <= 256:
        raise ValueError('invalid round limit')
    if type(timeout) not in (int, float) or not 0 < timeout <= 600:
        raise ValueError('invalid timeout')
    if not isinstance(model, str) or not model or len(model) > 256:
        raise ValueError('invalid model')
    if source not in ('online-discovery', 'simulated'):
        raise ValueError('invalid discovery source')
    if runner is not None:
        source = 'simulated'
    output, binary = Path(output).resolve(), Path(binary).resolve()
    binary_sha = hashlib.sha256(bounded_read(binary, 64 * 1024 * 1024)).hexdigest()
    task = make_task(seed)
    output.mkdir(parents=True, exist_ok=False)
    root_dir = output / 'nodes' / 'root'
    restore_snapshot(root_dir / 'snapshot', task['initial_files'])
    root = dict(id='root', parent=None, score=evaluate_artifact(root_dir / 'snapshot', task)['score'],
                resumable=True, diagnostics=['initial_artifact'], snapshot='nodes/root/snapshot', session=None,
                snapshot_sha256=snapshot_fingerprint(task['initial_files']))
    world = {'version': 1, 'root': 'root', 'compatibility': {'task_family': task['family'],
             'evaluator_version': task['evaluator_version'], 'model': model,
             'binary_sha256': binary_sha, 'harness_sha256': hashlib.sha256(HARNESS.encode()).hexdigest()},
             'task': task, 'policy': policy, 'nodes': [root], 'batches': [], 'online_workers': 1,
             'source': source}
    if base_url is not None:
        world['compatibility']['base_url'] = base_url
    validate_world(world)
    atomic_json(output / 'world.json', world)
    for _ in range(max_rounds):
        stop = termination_reason(policy, world['nodes'], len(world['batches']), max_rounds)
        if stop:
            world['stop_reason'] = stop
            break
        parents = {node['parent'] for node in world['nodes'][1:]}
        eligible = [node['id'] for node in world['nodes'] if node['resumable'] and
                    (node['id'] == 'root' or node['id'] not in parents)]
        actions = select_batch(policy, world['nodes'], eligible, workers=1)
        if not actions:
            world['stop_reason'] = 'policy_stop'
            break
        parent_id = actions[0]
        parent = next(node for node in world['nodes'] if node['id'] == parent_id)
        files = snapshot_files(output / parent['snapshot'])
        if snapshot_fingerprint(files) != parent['snapshot_sha256']:
            world['stop_reason'] = 'invalid_parent_snapshot'
            atomic_json(output / 'world.json', world)
            raise ValueError('parent snapshot changed')
        node_id = f'n{len(world["nodes"])}'
        node_dir = output / 'nodes' / node_id
        lineage, ancestor = [], parent
        while ancestor is not None:
            lineage = ancestor['diagnostics'] + lineage
            ancestor = next((node for node in world['nodes'] if node['id'] == ancestor['parent']), None)
        node = execute_attempt(files, task, node_dir, binary, model, base_url, env,
                               timeout, runner, lineage)
        for reference in ('snapshot', 'session'):
            if node[reference]:
                node[reference] = (Path('nodes') / node_id / node[reference]).as_posix()
        node.update(id=node_id, parent=parent_id)
        world['nodes'].append(node)
        world['batches'].append(actions)
        validate_world(world)
        atomic_json(output / 'world.json', world)
    else:
        world['stop_reason'] = termination_reason(policy, world['nodes'], len(world['batches']), max_rounds) or 'rounds_limit'
    atomic_json(output / 'world.json', world)
    return world
