"""DREAM's fixed evaluator, snapshots and real continuation boundary."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


def online():
    import dream_online
    return dream_online


class OnlineTests(unittest.TestCase):
    def test_checker_preserves_types_and_unrelated_fields(self):
        module = online()
        task = module.make_task('test')
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp) / 'workspace'
            module.restore_snapshot(work, task['initial_files'])
            (work / 'settings.json').write_text(json.dumps(task['expected']))
            self.assertEqual(module.evaluate_artifact(work, task)['score'], 1)
            value = json.loads(json.dumps(task['expected']))
            value['service']['enabled'] = 1
            (work / 'settings.json').write_text(json.dumps(value))
            self.assertEqual(module.evaluate_artifact(work, task)['score'], 2 / 3)
            value['metadata']['owner'] = 'changed'
            (work / 'settings.json').write_text(json.dumps(value))
            self.assertEqual(module.evaluate_artifact(work, task)['score'], 0)

    def test_snapshot_rejects_unexpected_special_or_unsafe_files(self):
        module = online()
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp) / 'workspace'
            task = module.make_task('test')
            module.restore_snapshot(work, task['initial_files'])
            self.assertEqual(module.snapshot_files(work), task['initial_files'])
            (work / 'extra').write_text('x')
            with self.assertRaises(ValueError):
                module.snapshot_files(work)
            (work / 'extra').unlink()
            (work / 'settings.json').unlink()
            (work / 'settings.json').symlink_to(work / 'protected.txt')
            with self.assertRaises(ValueError):
                module.snapshot_files(work)
            with self.assertRaises(ValueError):
                module.restore_snapshot(work / 'other', {'../outside': 'bad'})

    def test_attempt_uses_parent_artifact_and_keeps_state_separate(self):
        module = online()
        task = module.make_task('test')
        parent = dict(task['initial_files'])
        partial = json.loads(parent['settings.json'])
        partial['service']['retries'] = 4
        parent['settings.json'] = json.dumps(partial)
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp) / 'node'
            def runner(command, **kwargs):
                work = kwargs['cwd']
                self.assertEqual(kwargs.get('stdout'), subprocess.DEVNULL)
                self.assertEqual(kwargs.get('stderr'), subprocess.DEVNULL)
                self.assertNotIn('capture_output', kwargs)
                self.assertEqual(json.loads((work / 'settings.json').read_text()), partial)
                self.assertIn('service.port', kwargs['input'])
                self.assertNotIn('--resume', command)
                self.assertNotIn('secret', ' '.join(command))
                session_dir = Path(command[command.index('--sessions-dir') + 1])
                events = [dict(e='user', text='fixture')]
                events += [dict(e='tool_call', name=name, args=json.dumps({'path': 'settings.json'}))
                           for name in ('read_file', 'write_file')]
                (session_dir / 'attempt.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in events))
                (work / 'settings.json').write_text(json.dumps(task['expected']))
                return subprocess.CompletedProcess(command, 0, 'Done', 'secret')
            result = module.execute_attempt(parent, task, directory, Path('/fake/sigil'),
                                           'fixture', env={'SIGIL_API_KEY': 'secret'}, runner=runner)
            self.assertEqual(result['score'], 1)
            self.assertTrue(result['resumable'])
            self.assertNotIn('secret', json.dumps(result))
            self.assertNotIn('api_key', (directory / 'state' / 'config.toml').read_text())
            self.assertEqual(module.snapshot_files(directory / 'snapshot')['settings.json'],
                             json.dumps(task['expected']))

    def test_timeout_and_wrong_tool_do_not_disappear(self):
        module = online()
        task = module.make_task('test')
        with tempfile.TemporaryDirectory() as temp:
            def timeout(command, **kwargs):
                raise subprocess.TimeoutExpired(command, kwargs['timeout'], stderr=b'secret')
            result = module.execute_attempt(task['initial_files'], task, Path(temp) / 'timeout',
                                           Path('/fake/sigil'), 'fixture', runner=timeout)
            self.assertEqual(result['score'], 0)
            self.assertEqual(result['process_status'], 'timeout')
            self.assertNotIn('secret', json.dumps(result))
            def wrong(command, **kwargs):
                state = Path(command[command.index('--sessions-dir') + 1])
                (state / 'attempt.jsonl').write_text(json.dumps(dict(e='tool_call', name='bash', args='{}')) + '\n')
                (kwargs['cwd'] / 'settings.json').write_text(json.dumps(task['expected']))
                return subprocess.CompletedProcess(command, 0, '', '')
            result = module.execute_attempt(task['initial_files'], task, Path(temp) / 'wrong',
                                           Path('/fake/sigil'), 'fixture', runner=wrong)
            self.assertEqual(result['score'], 0)
            self.assertFalse(result['resumable'])

    def test_world_records_lineage_and_restores_each_parent(self):
        module = online()
        from dream_core import BASELINE, validate_world
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / 'sigil'
            binary.write_bytes(b'fixture binary')
            seen = []
            def runner(command, **kwargs):
                work = kwargs['cwd']
                value = json.loads((work / 'settings.json').read_text())
                seen.append(value['service']['retries'])
                value['service']['retries'] = 4
                (work / 'settings.json').write_text(json.dumps(value))
                session_dir = Path(command[command.index('--sessions-dir') + 1])
                events = [dict(e='tool_call', name=name, args=json.dumps({'path': 'settings.json'}))
                          for name in ('read_file', 'write_file')]
                (session_dir / 'attempt.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in events))
                return subprocess.CompletedProcess(command, 0, '', '')
            policy = dict(BASELINE, branch_width=1, max_attempts=2)
            world = module.collect_world(Path(temp) / 'world', binary, policy, 'fixture',
                                         max_rounds=2, runner=runner)
            validate_world(world)
            self.assertEqual(seen, [1, 4])
            self.assertEqual([n['parent'] for n in world['nodes']], [None, 'root', 'n1'])
            self.assertEqual(json.loads((Path(temp) / 'world' / 'world.json').read_text()), world)

    def test_environment_endpoint_is_explicit_and_fingerprinted(self):
        module = online()
        from dream_core import BASELINE
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / 'sigil'
            binary.write_bytes(b'fixture')
            seen = []
            def runner(command, **kwargs):
                seen.append(command)
                return subprocess.CompletedProcess(command, 1, '', '')
            endpoint = 'http://127.0.0.1:3001/v1'
            world = module.collect_world(Path(temp) / 'world', binary, BASELINE, 'fixture',
                                         env={'SIGIL_BASE_URL': endpoint}, max_rounds=1, runner=runner)
            self.assertEqual(world['compatibility']['base_url'], endpoint)
            self.assertEqual(seen[0][seen[0].index('--base-url') + 1], endpoint)

    def test_attempt_budget_has_the_same_reason_online_and_replay(self):
        module = online()
        from dream_core import BASELINE, replay
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / 'sigil'
            binary.write_bytes(b'fixture')
            def runner(command, **kwargs):
                return subprocess.CompletedProcess(command, 1, '', '')
            policy = dict(BASELINE, max_attempts=1)
            world = module.collect_world(Path(temp) / 'world', binary, policy, 'fixture',
                                         max_rounds=3, runner=runner)
            self.assertEqual(world['stop_reason'], 'attempts_limit')
            self.assertEqual(world['stop_reason'], replay(world, policy, max_rounds=3)['stop_reason'])

    def test_endpoints_with_embedded_credentials_are_rejected(self):
        module = online()
        for endpoint in ['https://secret@example.com/v1', 'https://example.com/v1?api_key=secret',
                         'file:///private/tmp/input', 'http://127.0.0.1/v1#secret']:
            with self.subTest(endpoint=endpoint), self.assertRaises(ValueError):
                module.effective_endpoint(endpoint, {})

    def test_frozen_state_changes_are_terminal(self):
        module = online()
        task = module.make_task('test')
        with tempfile.TemporaryDirectory() as temp:
            def runner(command, **kwargs):
                state = Path(command[command.index('--sessions-dir') + 1])
                events = [dict(e='tool_call', name=name, args=json.dumps({'path': 'settings.json'}))
                          for name in ('read_file', 'write_file')]
                (state / 'attempt.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in events))
                (state.parent / 'harness' / 'system_prompt.md').write_text('changed')
                (kwargs['cwd'] / 'settings.json').write_text(json.dumps(task['expected']))
                return subprocess.CompletedProcess(command, 0, '', '')
            result = module.execute_attempt(task['initial_files'], task, Path(temp) / 'node',
                                           Path('/fake/sigil'), 'fixture', runner=runner)
            self.assertFalse(result['resumable'])
            self.assertEqual(result['score'], 0)

    def test_corrupted_parent_snapshot_stops_without_inventing_a_child(self):
        module = online()
        from dream_core import BASELINE
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / 'sigil'
            binary.write_bytes(b'fixture')
            output = Path(temp) / 'world'
            def runner(command, **kwargs):
                # Attempt 1 damages the recorded root snapshot outside its work.
                (output / 'nodes' / 'root' / 'snapshot' / 'settings.json').write_text('{}')
                return subprocess.CompletedProcess(command, 1, '', '')
            with self.assertRaises(ValueError):
                module.collect_world(output, binary, BASELINE, 'fixture', max_rounds=2, runner=runner)
            world = json.loads((output / 'world.json').read_text())
            self.assertEqual(len(world['nodes']), 2)
            self.assertEqual(world['stop_reason'], 'invalid_parent_snapshot')


if __name__ == '__main__':
    unittest.main()
