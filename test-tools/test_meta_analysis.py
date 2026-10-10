"""Statistical regressions without running the report scripts on import."""
import ast
from pathlib import Path
import sys
import unittest

import numpy as np
from scipy import stats
from statsmodels.stats.meta_analysis import combine_effects

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))


def analysis_functions(filename):
    tree = ast.parse((ROOT / filename).read_text())
    nodes = [n for n in tree.body if isinstance(n, ast.FunctionDef)]
    nodes += [n for n in tree.body if isinstance(n, ast.ImportFrom)
              and n.module == 'meta_analysis_methods']
    namespace = {'np': np, 'stats': stats, 'combine_effects': combine_effects}
    exec(compile(ast.Module(body=nodes, type_ignores=[]), filename, 'exec'), namespace)
    return namespace


class TrimFillRegression(unittest.TestCase):
    def test_symmetric_funnel_does_not_gain_studies(self):
        y = np.array([-2., -1., 1., 2.])
        v = np.ones(4)
        for filename in ['meta_analysis.py', 'continuous_meta_analysis.py']:
            with self.subTest(filename=filename):
                result = analysis_functions(filename)['trim_and_fill'](y, v)
                missing = result[2] if isinstance(result, tuple) else result['n_filled']
                self.assertEqual(missing, 0)

    def test_no_fill_retains_original_random_effect(self):
        y = np.array([-2., -1., 1., 2.]) + 3.
        v = np.ones(4)
        result = analysis_functions('meta_analysis.py')['trim_and_fill'](y, v)
        self.assertEqual(result[2], 0)
        self.assertAlmostEqual(result[3], 3.)
        self.assertEqual(len(result[4]), 2)

    def test_signed_rank_reference_requires_three_iterations(self):
        from meta_analysis_methods import trim_fill
        # R0 by hand: k0 progresses 0 -> 1 -> 2 -> 2. The final trimmed
        # mean is 8/6; mirror the two rightmost studies (9 and 10).
        y = np.array([-2., -1., 0., 1., 2., 8., 9., 10.])
        r = trim_fill(y, np.ones(8), side='left')
        self.assertEqual(r['n_filled'], 2)
        self.assertEqual(r['iterations'], 3)
        np.testing.assert_allclose(r['y_filled'][-2:], [-19/3, -22/3])
        self.assertAlmostEqual(r['pooled_adjusted'], 4/3)

    def test_estimator_is_honored(self):
        from meta_analysis_methods import trim_fill
        y = np.array([-2., -1., 0., 1., 2., 8., 9., 10.])
        r = trim_fill(y, np.ones(8), side='left', estimator='L0')
        self.assertEqual(r['n_filled'], 1)
        self.assertAlmostEqual(r['pooled_adjusted'], 17/7)

    def test_reflection_and_input_order_do_not_change_estimate(self):
        from meta_analysis_methods import trim_fill
        y = np.array([-2., -1., 0., 1., 2., 8., 9., 10.])
        v = np.array([1., 1.1, .9, 1.2, .8, 1., 1.3, .7])
        r = trim_fill(y, v, side='left')
        order = [7, 3, 1, 6, 5, 0, 4, 2]
        permuted = trim_fill(y[order], v[order], side='left')
        reflected = trim_fill(-y, v, side='right')
        self.assertEqual(r['n_filled'], permuted['n_filled'])
        self.assertAlmostEqual(r['pooled_adjusted'], permuted['pooled_adjusted'])
        self.assertAlmostEqual(r['pooled_adjusted'], -reflected['pooled_adjusted'])
        np.testing.assert_allclose(r['v_filled'][len(v):], v[r['mirror_indices']])

    def test_nonconvergence_and_invalid_data_are_explicit_errors(self):
        from meta_analysis_methods import trim_fill
        y = [-2., -1., 0., 1., 2., 8., 9., 10.]
        with self.assertRaises(RuntimeError):
            trim_fill(y, np.ones(8), side='left', max_iter=1)
        for effects, variances in [([1.], [1.]), ([1., 2.], [1., 0.]),
                                   ([1., np.nan], [1., 1.])]:
            with self.assertRaises(ValueError):
                trim_fill(effects, variances)

    def test_hksj_uses_residual_scale(self):
        from meta_analysis_methods import hksj_interval
        r = combine_effects(np.array([-.1, 0., .1]), np.ones(3), method_re='iterated')
        expected_half_width = stats.t.ppf(.975, 2) * np.sqrt(.01/3)
        np.testing.assert_allclose(hksj_interval(r), [-expected_half_width, expected_half_width])

    def test_identical_effects_do_not_imply_missing_studies(self):
        from meta_analysis_methods import trim_fill
        for estimator in ['R0', 'L0']:
            r = trim_fill([1., 1., 1.], [1., 1., 1.], estimator=estimator)
            self.assertEqual(r['n_filled'], 0)
            self.assertAlmostEqual(r['pooled_adjusted'], 1.)


if __name__ == '__main__':
    unittest.main()
