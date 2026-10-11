"""Offline usage reporting regressions; no model calls or private logs."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('usage_report.py')


class UsageReportTests(unittest.TestCase):
    def report(self, events):
        self.assertTrue(SCRIPT.exists(), 'offline usage reporter is missing')
        spec = importlib.util.spec_from_file_location('usage_report', SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module.summarize(events)

    def event(self, usage, model='m', kind='stream'):
        return {'e': 'request_usage', 'model': model, 'request_kind': kind, 'usage': usage}

    def usage(self, prompt=100, hit=80, coverage=100):
        return dict(prompt_tokens=prompt, completion_tokens=5,
                    prompt_cache_hit_tokens=hit, cache_reported_prompt_tokens=coverage)

    def test_reported_subset_is_denominator_and_unknown_is_not_zero(self):
        result = self.report([self.event(self.usage()),
                              self.event(self.usage(200, 99, None)), self.event(None)])
        total = result['total']
        self.assertEqual(total['requests'], 3)
        self.assertEqual(total['usage_reported_requests'], 2)
        self.assertEqual(total['prompt_tokens'], 300)
        self.assertEqual(total['cache_reported_prompt_tokens'], 100)
        self.assertEqual(total['prompt_cache_hit_tokens'], 80)
        self.assertEqual(total['cache_hit_rate'], .8)
        self.assertEqual(total['cache_coverage_rate'], 1/3)
        self.assertFalse(total['usage_complete'])

    def test_explicit_zero_hits_differs_from_unknown(self):
        reported = self.report([self.event(self.usage(hit=0))])['total']
        unknown = self.report([self.event(self.usage(coverage=None))])['total']
        self.assertEqual(reported['cache_hit_rate'], 0)
        self.assertIsNone(unknown['cache_hit_rate'])

    def test_non_usage_events_are_ignored_and_groups_stay_separate(self):
        result = self.report([{'e': 'user', 'text': 'secret text'},
                              self.event(self.usage(), model='a'),
                              self.event(None, model='b', kind='tool replay')])
        self.assertEqual(result['total']['requests'], 2)
        self.assertEqual(len(result['groups']), 2)
        self.assertNotIn('secret text', json.dumps(result))

    def test_default_cache_hit_counter_matches_rust_deserialization(self):
        usage = self.usage()
        del usage['prompt_cache_hit_tokens']
        self.assertEqual(self.report([self.event(usage)])['total']['cache_hit_rate'], 0)

    def test_empty_or_old_log_is_explicitly_unavailable(self):
        total = self.report([{'e': 'step_end'}])['total']
        self.assertIsNone(total['cache_hit_rate'])
        self.assertIsNone(total['cache_coverage_rate'])
        self.assertFalse(total['usage_complete'])

    def test_invalid_counters_fail_instead_of_creating_misleading_totals(self):
        for usage in [self.usage(hit=101), self.usage(prompt=-1),
                      self.usage(prompt=True), self.usage(coverage=101),
                      {'prompt_tokens': 100}]:
            with self.subTest(usage=usage), self.assertRaises(ValueError):
                self.report([self.event(usage)])

    def test_cli_json_deduplicates_files_and_rejects_malformed_json(self):
        self.assertTrue(SCRIPT.exists(), 'offline usage reporter is missing')
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'session.jsonl'
            path.write_text(json.dumps(self.event(self.usage())) + '\n')
            result = subprocess.run([sys.executable, str(SCRIPT), '--json', str(path), str(path)],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)['total']['requests'], 1)
            path.write_text('{broken sensitive content\n')
            result = subprocess.run([sys.executable, str(SCRIPT), str(path)],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(':1:', result.stderr)
            self.assertNotIn('sensitive content', result.stderr)
    def test_cli_invalid_utf8_fails_without_traceback(self):
        self.assertTrue(SCRIPT.exists())
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'invalid.jsonl'
            path.write_bytes(b'\xff')
            result = subprocess.run([sys.executable, str(SCRIPT), str(path)],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('Traceback', result.stderr)
            self.assertIn('invalid UTF-8', result.stderr)
            self.assertIn(str(path) + ':1:', result.stderr)


if __name__ == '__main__':
    unittest.main()
