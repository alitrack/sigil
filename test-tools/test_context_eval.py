"""Context policy evaluation regressions using synthetic data only."""
import importlib.util
import json
import os
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('context_eval.py')


def module():
    if not SCRIPT.exists():
        raise AssertionError('context policy evaluator is missing')
    spec = importlib.util.spec_from_file_location('context_eval', SCRIPT)
    result = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(SCRIPT.parent))
    spec.loader.exec_module(result)
    return result


class ContextEvaluation(unittest.TestCase):
    def test_trial_policies_have_identical_seed_and_isolated_state(self):
        evaluator = module()
        with tempfile.TemporaryDirectory() as temp:
            off = evaluator.prepare_trial(Path(temp), 'off', 0, 'marker-1')
            on = evaluator.prepare_trial(Path(temp), 'on', 8000, 'marker-1')
            self.assertEqual(off['session'].read_bytes(), on['session'].read_bytes())
            self.assertNotEqual(off['work'], on['work'])
            self.assertIn('max_context_tokens = 0', off['config'].read_text())
            self.assertIn('max_context_tokens = 8000', on['config'].read_text())
            self.assertNotIn('api_key', on['config'].read_text())
            self.assertIn('marker-1', off['session'].read_text())

    def test_success_requires_exact_answer_and_successful_process(self):
        evaluator = module()
        self.assertTrue(evaluator.correct('fixture-key\n', 'fixture-key', 0))
        self.assertFalse(evaluator.correct('I forgot fixture-key', 'fixture-key', 0))
        self.assertFalse(evaluator.correct('fixture-key', 'fixture-key', 1))

    def test_missing_telemetry_and_timeout_do_not_disappear(self):
        evaluator = module()
        with tempfile.TemporaryDirectory() as temp:
            trial = evaluator.prepare_trial(Path(temp), 'off', 0, 'fixture-key')
            def timeout(*args, **kwargs):
                raise subprocess.TimeoutExpired(args[0], kwargs['timeout'])
            result = evaluator.execute_trial(Path('/fake/sigil'), trial, 2, {}, runner=timeout)
            self.assertFalse(result['passed'])
            self.assertEqual(result['status'], 'timeout')
            self.assertFalse(result['usage']['total']['usage_complete'])
            self.assertEqual(result['usage']['total']['requests'], 0)

    def test_timeout_with_prior_usage_is_still_incomplete(self):
        evaluator = module()
        with tempfile.TemporaryDirectory() as temp:
            trial = evaluator.prepare_trial(Path(temp), 'off', 0, 'fixture-key')
            with trial['session'].open('a') as file:
                file.write(json.dumps({'e': 'request_usage', 'model': 'fixture', 'request_kind': 'stream',
                                       'usage': {'prompt_tokens': 100, 'completion_tokens': 5}}) + '\n')
            def timeout(*args, **kwargs):
                raise subprocess.TimeoutExpired(args[0], kwargs['timeout'])
            result = evaluator.execute_trial(Path('/fake/sigil'), trial, 2, {}, runner=timeout)
            self.assertEqual(result['usage']['total']['prompt_tokens'], 100)
            self.assertFalse(result['usage']['total']['usage_complete'])

    def test_invalid_telemetry_retains_trial_outcome(self):
        evaluator = module()
        with tempfile.TemporaryDirectory() as temp:
            trial = evaluator.prepare_trial(Path(temp), 'off', 0, 'fixture-key')
            with trial['session'].open('a') as file:
                file.write('{partial')
            def run(command, **kwargs):
                return subprocess.CompletedProcess(command, 1, '', '')
            result = evaluator.execute_trial(Path('/fake/sigil'), trial, 2, {}, runner=run)
            self.assertEqual(result['status'], 'process_error')
            self.assertEqual(result['usage_observation_status'], 'invalid_or_unreadable')
            self.assertFalse(result['usage']['total']['usage_complete'])

    def test_process_output_is_not_saved_in_summary(self):
        evaluator = module()
        with tempfile.TemporaryDirectory() as temp:
            trial = evaluator.prepare_trial(Path(temp), 'on', 8000, 'fixture-key')
            commands = []
            def run(command, **kwargs):
                commands.append(command)
                return subprocess.CompletedProcess(command, 1, 'secret-value', 'secret-error')
            result = evaluator.execute_trial(Path('/fake/sigil'), trial, 2, {}, runner=run)
            self.assertNotIn('secret', json.dumps(result))
            self.assertEqual(result['status'], 'process_error')
            self.assertIn('--max-steps', commands[0])
            self.assertIn('--hooks-dir', commands[0])
            self.assertIn('--config', commands[0])

    def test_summary_keeps_failed_attempt_tokens_and_unknown_coverage(self):
        evaluator = module()
        usage = {'total': {'prompt_tokens': 100, 'completion_tokens': 5, 'usage_complete': True,
                          'requests': 1, 'cache_reported_requests': 1,
                          'cache_reported_prompt_tokens': 100, 'prompt_cache_hit_tokens': 80}}
        rows = [dict(policy='on', passed=True, usage=usage), dict(policy='on', passed=False, usage=usage)]
        summary = evaluator.policy_summary(rows, 'on')
        self.assertEqual(summary['success_rate'], .5)
        self.assertEqual(summary['observed_tokens_per_success'], 210)
        self.assertEqual(summary['cache_hit_rate'], .8)
        rows.append(dict(policy='on', passed=False, usage={'total': dict(usage['total'], usage_complete=False)}))
        self.assertIsNone(evaluator.policy_summary(rows, 'on')['observed_tokens_per_success'])

    def test_default_cli_plan_does_not_execute_or_create_output(self):
        self.assertTrue(SCRIPT.exists())
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'missing'
            result = subprocess.run([sys.executable, str(SCRIPT), '--output', str(output)],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(output.exists())
            self.assertIn('execute', result.stdout)

    @unittest.skipUnless(os.environ.get('SIGIL_EVAL_E2E') == '1', 'local-server E2E is opt-in')
    def test_real_binary_compares_policies_against_local_fixture(self):
        observed = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                text = '\n'.join(m.get('content', '') for m in request['messages'])
                observed.append(text)
                marker = re.search(r'TASK_KEY: (key-[a-f0-9]+)', text).group(1)
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()
                chunks = [{'choices': [{'delta': {'content': marker}, 'finish_reason': None}]},
                          {'choices': [{'delta': {}, 'finish_reason': 'stop'}]},
                          {'choices': [], 'usage': {'prompt_tokens': len(text)//4, 'completion_tokens': 10,
                                                   'prompt_tokens_details': {'cached_tokens': 0}}}]
                for chunk in chunks:
                    self.wfile.write(('data: ' + json.dumps(chunk) + '\n\n').encode())
                self.wfile.write(b'data: [DONE]\n\n')
                self.wfile.flush()
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'results'
            server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            env = dict(os.environ, SIGIL_API_KEY='fixture', NO_PROXY='127.0.0.1', no_proxy='127.0.0.1')
            try:
                result = subprocess.run([sys.executable, str(SCRIPT), '--execute', '--output', str(output),
                                         '--model', 'fixture', '--base-url', f'http://127.0.0.1:{server.server_port}/v1'],
                                        capture_output=True, text=True, env=env, timeout=30)
                self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
                report = json.loads((output / 'report.json').read_text())
                self.assertEqual([r['policy'] for r in report['results']], ['off', 'on', 'on', 'off'])
                self.assertTrue(all(r['passed'] for r in report['results']))
                self.assertEqual(len(observed), 4)
                self.assertFalse('[earlier conversation compressed]' in observed[0])
                self.assertIn('[earlier conversation compressed]', observed[1])
                self.assertLess(len(observed[1]), len(observed[0]))
                self.assertNotIn('fixture', (output / 'pair-1' / 'on' / 'config.toml').read_text())
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
