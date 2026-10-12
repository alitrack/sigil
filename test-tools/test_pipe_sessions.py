"""Real CLI regression against a local fake LLM; no paid endpoint or private state.

Build first: cargo build --locked --release
Run: python3 test-tools/test_pipe_sessions.py
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BINARY = Path(__file__).resolve().parents[1] / 'target' / 'release' / 'sigil'


class PipeSessions(unittest.TestCase):
    def test_record_resume_usage_and_default_nonrecording(self):
        requests = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()
                chunks = [
                    {'choices': [{'delta': {'content': 'fixture answer'}, 'finish_reason': None}]},
                    {'choices': [{'delta': {}, 'finish_reason': 'stop'}]},
                    {'choices': [], 'usage': {'prompt_tokens': 100, 'completion_tokens': 5,
                                             'prompt_tokens_details': {'cached_tokens': 80}}},
                ]
                for chunk in chunks:
                    self.wfile.write(('data: ' + json.dumps(chunk) + '\n\n').encode())
                self.wfile.write(b'data: [DONE]\n\n')
                self.wfile.flush()

        self.assertTrue(BINARY.exists(), 'build the release binary first')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / 'config.toml'
            config.write_text('[llm]\nmodel = "fixture"\n')
            server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith('SIGIL_')}
            env.update(HOME=tmp, XDG_CONFIG_HOME=tmp, NO_PROXY='127.0.0.1', no_proxy='127.0.0.1')
            command = [str(BINARY), '--config', str(config), '--api-key', 'fixture',
                       '--base-url', f'http://127.0.0.1:{server.server_port}/v1',
                       '--harness-dir', str(root / 'harness'), '--tools-dir', str(root / 'tools'),
                       '--hooks-dir', str(root / 'hooks'), '--memory', str(root / 'memory.md'),
                       '--delegates-dir', str(root / 'delegates')]
            sessions = root / 'sessions'

            def run(prompt, *extra):
                result = subprocess.run(command + list(extra), input=prompt, text=True,
                                        capture_output=True, cwd=tmp, env=env, timeout=15)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('fixture answer', result.stdout)

            try:
                run('first', '--sessions-dir', str(sessions))
                logs = list(sessions.glob('*.jsonl'))
                self.assertEqual(len(logs), 1)
                run('second', '--sessions-dir', str(sessions), '--resume', 'latest')
                events = [json.loads(line) for line in logs[0].read_text().splitlines()]
                self.assertEqual([e['text'] for e in events if e['e'] == 'user'], ['first', 'second'])
                self.assertEqual(len([e for e in events if e['e'] == 'assistant']), 2)
                usage = [e['usage'] for e in events if e['e'] == 'request_usage']
                self.assertEqual(len(usage), 2)
                self.assertTrue(all(u['prompt_cache_hit_tokens'] == 80 for u in usage))
                contents = [m.get('content') for m in requests[1]['messages']]
                self.assertIn('first', contents)
                self.assertIn('fixture answer', contents)
                self.assertIn('second', contents)
                run('unrecorded')
                self.assertFalse((root / '.sigil' / 'sessions').exists())
                self.assertEqual(len(requests), 3, 'telemetry must not issue extra requests')
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
