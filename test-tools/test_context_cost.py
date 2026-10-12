"""Cost-analysis tests use hypothetical rates, never provider prices."""
import importlib.util
import json
import subprocess
import sys
import tempfile
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name('context_cost.py')


def module():
    if not SCRIPT.exists():
        raise AssertionError('explicit-price analyzer is missing')
    spec = importlib.util.spec_from_file_location('context_cost', SCRIPT)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def row(policy, prompt, hit, completion=100, passed=True, complete=True, covered=None, pair=1):
    return {'pair': pair, 'policy': policy, 'status': 'pass' if passed else 'wrong_artifact',
            'passed': passed, 'usage_observation_status': 'valid', 'usage': {
                'total': {'requests': 1, 'usage_reported_requests': 1,
                          'prompt_tokens': prompt, 'completion_tokens': completion,
                          'cache_reported_prompt_tokens': prompt if covered is None else covered,
                          'prompt_cache_hit_tokens': hit, 'usage_complete': complete},
                'groups': [{'model': 'fixture'}]}}


class ContextCost(unittest.TestCase):
    def analyze(self, rows, rates=('1', '.1', '2')):
        return module().analyze({'results': rows}, rates, 'TEST' if rates else None)

    def test_fewer_tokens_can_cost_more_after_cache_loss(self):
        result = self.analyze([row('off', 50000, 50000), row('on', 8000, 0)])
        self.assertEqual(result['policies']['off']['estimated_cost'], '0.0052')
        self.assertEqual(result['policies']['on']['estimated_cost'], '0.0082')
        self.assertEqual(result['estimated_on_minus_off'], '0.0030')

    def test_no_prices_produces_symbolic_difference_not_money(self):
        result = self.analyze([row('off', 100, 80), row('on', 60, 40)], rates=None)
        self.assertIsNone(result['estimated_on_minus_off'])
        self.assertIsNone(result['policies']['on']['estimated_cost'])
        self.assertEqual(result['token_delta']['uncached_input'], 0)
        self.assertEqual(result['token_delta']['cached_input'], -40)

    def test_unknown_cache_or_interrupted_usage_is_not_zero_cost(self):
        rows = [row('off', 100, 80), row('on', 100, 0, covered=0)]
        self.assertIsNone(self.analyze(rows)['policies']['on']['estimated_cost'])
        rows[1] = row('on', 100, 50, complete=False)
        self.assertIsNone(self.analyze(rows)['estimated_on_minus_off'])
        rows[1] = row('on', 100, 50)
        rows[1]['status'] = 'timeout'
        rows[1]['passed'] = False
        self.assertIsNone(self.analyze(rows)['policies']['on']['estimated_cost'])

    def test_identical_input_prices_do_not_need_cache_split(self):
        result = self.analyze([row('off', 100, 0, covered=0), row('on', 60, 0, covered=0)],
                              rates=('1', '1', '2'))
        self.assertEqual(result['policies']['on']['estimated_cost'], '0.00026')

    def test_failed_attempt_cost_is_retained_and_zero_success_is_undefined(self):
        result = self.analyze([row('off', 100, 0, passed=False), row('on', 100, 0, passed=False)])
        self.assertEqual(result['policies']['off']['estimated_cost'], '0.0003')
        self.assertIsNone(result['policies']['off']['estimated_cost_per_success'])

    def test_invalid_rates_and_unpaired_or_duplicate_results_fail(self):
        valid = [row('off', 100, 0), row('on', 100, 0)]
        for rates in [('NaN', '0', '1'), ('Infinity', '0', '1'), ('-1', '0', '1'), ('1e-1000000', '0', '1')]:
            with self.subTest(rates=rates), self.assertRaises(ValueError):
                self.analyze(valid, rates)
        for rows in [valid[:1], valid + [valid[0]], [row('off', 100, 101), valid[1]]]:
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                self.analyze(rows)
        mixed = [row('off', 100, 0), row('on', 100, 0)]
        mixed[1]['usage']['groups'] = [{'model': 'other'}]
        with self.assertRaises(ValueError):
            self.analyze(mixed)
    def test_pair_members_must_describe_same_fixture_and_valid_status(self):
        rows = [row('off', 100, 0), row('on', 100, 0)]
        rows[0]['case'] = 'json-edit'
        rows[1]['case'] = 'retention'
        with self.assertRaises(ValueError):
            self.analyze(rows)
        rows[1]['case'] = 'json-edit'
        rows[1]['status'] = 'unknown'
        rows[1]['passed'] = False
        with self.assertRaises(ValueError):
            self.analyze(rows)
    def test_cli_does_not_guess_prices_or_echo_invalid_records(self):
        with tempfile.TemporaryDirectory() as temp:
            report = Path(temp) / 'report.json'
            report.write_text(json.dumps({'results': [row('off', 100, 0), row('on', 60, 0)]}))
            result = subprocess.run([sys.executable, str(SCRIPT), str(report)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIsNone(json.loads(result.stdout)['estimated_on_minus_off'])
            result = subprocess.run([sys.executable, str(SCRIPT), str(report), '--uncached-input-rate', '1'],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            report.write_text(json.dumps({'results': 'secret-placeholder'}))
            result = subprocess.run([sys.executable, str(SCRIPT), str(report)], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('secret-placeholder', result.stderr)


if __name__ == '__main__':
    unittest.main()
