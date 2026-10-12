"""CLI and recursive DREAM integration, including opt-in real-binary E2E."""
import contextlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = Path(__file__).with_name('dream.py')


class DreamCLI(unittest.TestCase):
    def test_default_plan_never_starts_online_execution(self):
        import dream
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'new'
            with patch('dream.collect_world', side_effect=AssertionError('online call')), contextlib.redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(dream.main(['run', '--output', str(output), '--rounds', '2']), 0)
            plan = json.loads(stdout.getvalue())
            self.assertFalse(plan['execute'])
            self.assertEqual(plan['outer_rounds'], 2)
            self.assertIn('tasks', plan)
            self.assertIn('base_url', plan)
            self.assertFalse(output.exists())

    def test_offline_replay_never_spawns_processes(self):
        import dream
        from dream_core import BASELINE
        from test_dream_core import world
        with tempfile.TemporaryDirectory() as temp:
            source = Path(temp) / 'world.json'
            source.write_text(json.dumps(world()))
            policy = Path(temp) / 'policy.json'
            policy.write_text(json.dumps(BASELINE))
            with patch('subprocess.run', side_effect=AssertionError('execution')), contextlib.redirect_stdout(io.StringIO()) as stdout:
                output = Path(temp) / 'replay'
                code = dream.main(['replay', '--world', str(source), '--policy', str(policy), '--output', str(output)])
            self.assertEqual(code, 0)
            report = json.loads(stdout.getvalue())
            self.assertGreaterEqual(report['selected_score'], report['incumbent_score'])
            self.assertEqual(json.loads((output / 'replay.json').read_text()), report)
            self.assertEqual(json.loads((output / 'selected-policy.json').read_text()), report['selected_policy'])

    def test_plan_budget_covers_promoted_candidates(self):
        import dream
        from dream_core import BASELINE
        with tempfile.TemporaryDirectory() as temp:
            policy, candidate = Path(temp) / 'policy.json', Path(temp) / 'candidate.json'
            policy.write_text(json.dumps(dict(BASELINE, max_attempts=1)))
            candidate.write_text(json.dumps(dict(BASELINE, name='larger', max_attempts=8)))
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                code = dream.main(['run', '--output', str(Path(temp) / 'output'), '--policy', str(policy),
                                   '--candidate', str(candidate), '--rounds', '2', '--decision-rounds', '3'])
            self.assertEqual(code, 0)
            self.assertEqual(json.loads(stdout.getvalue())['max_agent_attempts'], 6)

    def test_invalid_inputs_do_not_echo_records(self):
        import dream
        with tempfile.TemporaryDirectory() as temp:
            policy = Path(temp) / 'policy.json'
            policy.write_text('{"secret": "do-not-echo"}')
            with contextlib.redirect_stderr(io.StringIO()) as stderr, contextlib.redirect_stdout(io.StringIO()) as stdout:
                code = dream.main(['run', '--policy', str(policy), '--output', str(Path(temp) / 'out')])
            self.assertNotEqual(code, 0)
            self.assertNotIn('do-not-echo', stdout.getvalue() + stderr.getvalue())

    @unittest.skipUnless(os.getenv('SIGIL_DREAM_E2E') == '1', 'opt-in actual-binary fake-provider E2E')
    def test_two_rounds_collect_replay_and_redeploy_with_real_binary(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        observed = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                observed.append(request)
                messages = request['messages']
                user = next(message['content'] for message in messages if message['role'] == 'user')
                port = int(re.search(r'service.port to (\d+)', user).group(1))
                tool_results = sum(message['role'] == 'tool' for message in messages)
                call = None
                if tool_results == 0:
                    call = ('read_file', {'path': 'settings.json'})
                elif tool_results == 1:
                    content = '\n'.join(message['content'] for message in messages if message['role'] == 'tool')
                    raw = '\n'.join(re.findall(r'^\d+\|(.*)$', content, re.MULTILINE))
                    value = json.loads(raw)
                    value['service'].update(retries=4, port=port, enabled=True)
                    call = ('write_file', {'path': 'settings.json', 'content': value})
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream' if request.get('stream') else 'application/json')
                self.end_headers()
                usage = {'prompt_tokens': 100, 'completion_tokens': 10}
                if not request.get('stream'):
                    message = {'role': 'assistant', 'content': ''}
                    if call:
                        message['tool_calls'] = [{'id': 'call-' + call[0], 'type': 'function',
                                                 'function': {'name': call[0], 'arguments': json.dumps(call[1])}}]
                    self.wfile.write(json.dumps({'choices': [{'message': message}], 'usage': usage}).encode())
                    return
                delta = {'content': 'Done'}
                if call:
                    delta = {'tool_calls': [{'index': 0, 'id': 'call-' + call[0], 'type': 'function',
                                             'function': {'name': call[0], 'arguments': json.dumps(call[1])}}]}
                chunks = [{'choices': [{'delta': delta, 'finish_reason': None}]},
                          {'choices': [{'delta': {}, 'finish_reason': 'tool_calls' if call else 'stop'}], 'usage': usage}]
                for chunk in chunks:
                    self.wfile.write(('data: ' + json.dumps(chunk) + '\n\n').encode())
                self.wfile.write(b'data: [DONE]\n\n')
                self.wfile.flush()
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as temp:
                output = Path(temp) / 'experiment'
                env = dict(os.environ, SIGIL_API_KEY='test-only-auth-no-log', NO_PROXY='127.0.0.1', no_proxy='127.0.0.1')
                result = subprocess.run([sys.executable, str(SCRIPT), 'run', '--execute', '--output', str(output),
                                         '--rounds', '2', '--decision-rounds', '3', '--attempts', '3',
                                         '--timeout', '10', '--model', 'fixture', '--source', 'simulated',
                                         '--base-url', f'http://127.0.0.1:{server.server_port}/v1'],
                                        env=env, capture_output=True, text=True, timeout=45)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                report = json.loads((output / 'run.json').read_text())
                first, second = [json.loads((output / row['world']).read_text()) for row in report['rounds']]
                self.assertEqual(report['source'], 'simulated')
                self.assertTrue(all(tree['source'] == 'simulated' for tree in (first, second)))
                self.assertEqual(len(first['nodes']), 4, json.dumps(first))
                self.assertEqual(len(second['nodes']), 2)
                self.assertIsNone(first['policy']['stop_score'])
                self.assertEqual(second['policy']['stop_score'], 1)
                self.assertTrue(all(node['passed'] for tree in (first, second) for node in tree['nodes'][1:]))
                self.assertEqual(len(observed), 12)
                self.assertNotIn('test-only-auth-no-log', (output / 'run.json').read_text())
                self.assertTrue((output / 'selected-policy.json').exists())
                self.assertEqual(report['rounds'][0]['selected_policy'], second['policy'])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
