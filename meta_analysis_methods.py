"""Shared inference for the two example reports.

Trim-and-fill follows Duval/Tweedie's signed-rank R0/L0 iteration, as
implemented by metafor (https://github.com/wviechtb/metafor/blob/master/R/trimfill.rma.uni.r).
Average ranks make tied observations independent of input ordering.
"""
import numpy as np
from scipy import stats
from statsmodels.stats.meta_analysis import combine_effects


def _study_arrays(y, v):
    y, v = np.asarray(y, dtype=float), np.asarray(v, dtype=float)
    if y.ndim != 1 or v.shape != y.shape or len(y) < 2:
        raise ValueError('at least two studies with matching one-dimensional variances are required')
    if not np.all(np.isfinite(y)) or not np.all(np.isfinite(v)) or np.any(v <= 0):
        raise ValueError('effects must be finite and variances finite and positive')
    return y, v


def random_effect_summary(y, v, method='dl', hksj=False):
    y, v = _study_arrays(y, v)
    w = 1 / v
    if method == 'dl':
        mean_fe = np.sum(w * y) / w.sum()
        q = np.sum(w * (y - mean_fe) ** 2)
        tau2 = max(0., (q - (len(y) - 1)) / (w.sum() - np.sum(w ** 2) / w.sum()))
    elif method == 'pm':
        tau2 = combine_effects(y, v, method_re='iterated').tau2
    else:
        raise ValueError('method must be dl or pm')
    w = 1 / (v + tau2)
    mean = np.sum(w * y) / w.sum()
    variance = 1 / w.sum()
    if hksj:
        variance *= np.sum(w * (y - mean) ** 2) / (len(y) - 1)
        critical = stats.t.ppf(.975, len(y) - 1)
    else:
        critical = stats.norm.ppf(.975)
    se = np.sqrt(variance)
    return mean, (mean - critical * se, mean + critical * se)


def hksj_interval(result):
    """Use the estimated residual scale, rather than only a t critical value."""
    half_width = stats.t.ppf(.975, result.k - 1) * np.sqrt(result.var_hksj_re)
    return result.mean_effect_re - half_width, result.mean_effect_re + half_width


def trim_fill(y, v, side='auto', estimator='R0', max_iter=100, method='dl', hksj=False):
    """Estimate missing studies, trim until stable, then mirror once.

    ``side`` names the missing side. Auto chooses the side from the Egger
    intercept sign (equivalently the fixed-effect regression of effect on
    SE), and defaults to left when all standard errors are identical.
    Nonconvergence is an error, not a silently accepted partial estimate.
    """
    y, v = _study_arrays(y, v)
    if estimator not in ('R0', 'L0'):
        raise ValueError('estimator must be R0 or L0')
    if not isinstance(max_iter, int) or max_iter <= 0:
        raise ValueError('max_iter must be a positive integer')
    if side == 'auto':
        precision = 1 / np.sqrt(v)
        if np.ptp(precision) == 0:
            side = 'left'
        else:
            intercept = np.linalg.lstsq(
                np.column_stack([np.ones(len(y)), precision]), y * precision, rcond=None
            )[0][0]
            side = 'right' if intercept < 0 else 'left'
    if side not in ('left', 'right'):
        raise ValueError('side must be left, right, or auto')

    oriented = -y if side == 'right' else y
    # Sort on effect, then variance so ties do not depend on caller ordering.
    order = np.lexsort((v, oriented))
    sorted_y, sorted_v = oriented[order], v[order]
    k, missing = len(y), 0
    for iteration in range(1, max_iter + 1):
        retained = k - missing
        if retained < 2:
            raise ValueError('trim-and-fill leaves fewer than two studies')
        center, _ = random_effect_summary(sorted_y[:retained], sorted_v[:retained], method)
        residual = sorted_y - center
        ranks = stats.rankdata(np.abs(residual), method='average')
        signed_ranks = np.sign(residual) * ranks
        if estimator == 'R0':
            negative = signed_ranks[signed_ranks < 0]
            if not np.any(signed_ranks):
                estimate = 0
            else:
                estimate = k - np.max(-negative) - 1 if len(negative) else k - 1
        else:
            estimate = (4 * signed_ranks[signed_ranks > 0].sum() - k * (k + 1)) / (2 * k - 1)
        new_missing = max(0, int(np.rint(estimate)))
        if new_missing == missing:
            break
        missing = new_missing
    else:
        raise RuntimeError('trim-and-fill did not converge')

    mirror_indices = order[k - missing:] if missing else np.array([], dtype=int)
    original_center = -center if side == 'right' else center
    filled_y = np.concatenate([y, 2 * original_center - y[mirror_indices]])
    filled_v = np.concatenate([v, v[mirror_indices]])
    original, _ = random_effect_summary(y, v, method, hksj)
    adjusted, interval = random_effect_summary(filled_y, filled_v, method, hksj)
    return {
        'n_filled': missing, 'side': side, 'pooled_original': original,
        'pooled_adjusted': adjusted, 'ci_lower_adjusted': interval[0],
        'ci_upper_adjusted': interval[1], 'y_filled': filled_y, 'v_filled': filled_v,
        'n_total': len(filled_y), 'original_indices': np.arange(k),
        'mirror_indices': mirror_indices, 'iterations': iteration,
    }
