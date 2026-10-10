#!/usr/bin/env python3
"""
连续型变量 Meta 分析完整流程
========================================
功能：
  1. 自动选择效应模型（固定/随机效应模型）
  2. 亚组分析
  3. 异质性检验（Q 检验、I²）
  4. 发表偏倚检验（Egger 回归、剪补法）
  5. 敏感性分析（Leave-one-out）
  6. 可视化森林图、漏斗图、敏感性分析图

作者：sigil
"""

import numpy as np
import pandas as pd
from scipy import stats
from meta_analysis_methods import trim_fill
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import warnings
warnings.filterwarnings('ignore')

# ============================================================
# 1. 生成示例数据（模拟连续型变量的 Meta 分析）
# ============================================================
def generate_example_data(random_seed=42):
    """
    生成连续型变量 Meta 分析的示例数据。
    假设结局变量为连续变量，提供各研究的均值、标准差、样本量。
    包含一个亚组变量（Subgroup）。
    """
    rng = np.random.default_rng(random_seed)
    n_studies = 18

    studies = []
    # 基础效应：真实效应量 ~ N(0.5, 0.15)
    true_effect = 0.5

    for i in range(n_studies):
        # 各研究样本量 20~200
        n = int(rng.uniform(20, 200))
        # 各研究效应量在真实效应附近波动
        if i < 6:  # 亚组 A: 效应量较小
            subgroup = "A"
            effect = rng.normal(0.3, 0.12)
        elif i < 12:  # 亚组 B: 效应量中等
            subgroup = "B"
            effect = rng.normal(0.5, 0.15)
        else:  # 亚组 C: 效应量较大
            subgroup = "C"
            effect = rng.normal(0.7, 0.18)

        # 构造均值和标准差（仅用于展示，分析时直接使用效应量）
        mean_exp = effect
        sd_exp = rng.uniform(0.4, 1.0)
        mean_ctrl = 0.0
        sd_ctrl = rng.uniform(0.4, 1.0)

        # 计算 Cohen's d
        sp = np.sqrt(((n - 1) * sd_exp**2 + (n - 1) * sd_ctrl**2) / (2 * n - 2))
        d = (mean_exp - mean_ctrl) / sp

        # 计算 d 的方差
        var_d = (1/n + 1/n) + d**2 / (2 * n)

        studies.append({
            'Study': f"Study_{i+1:02d}",
            'n1': n,
            'n2': n,
            'Mean1': mean_exp,
            'SD1': sd_exp,
            'Mean2': mean_ctrl,
            'SD2': sd_ctrl,
            'Cohen_d': d,
            'Var_d': var_d,
            'Subgroup': subgroup
        })

    df = pd.DataFrame(studies)
    return df


# ============================================================
# 2. 效应量计算与方差稳定化
# ============================================================
def compute_hedges_g(d, var_d, n1, n2):
    """
    将 Cohen's d 转换为 Hedges' g（小样本校正）。
    """
    df = n1 + n2 - 2
    J = 1 - 3 / (4 * df - 1)  # 校正因子
    g = J * d
    var_g = J**2 * var_d
    return g, var_g


# ============================================================
# 3. 异质性检验与 I² 计算
# ============================================================
def heterogeneity_test(y, v):
    """
    异质性检验：Q 统计量、I²、H 统计量
    参数：
      y: 各研究的效应量向量
      v: 各研究的效应量方差向量
    返回：
      Q, Q_df, Q_pval, I2, H
    """
    # 固定效应权重
    w = 1.0 / v
    # 固定效应加权平均
    y_bar_fe = np.sum(w * y) / np.sum(w)

    # Q 统计量
    Q = np.sum(w * (y - y_bar_fe)**2)
    k = len(y)
    df = k - 1

    # Q 检验 p 值
    Q_pval = 1 - stats.chi2.cdf(Q, df)

    # I² 统计量
    I2 = max(0, (Q - df) / Q * 100) if Q > df else 0.0

    # H 统计量
    H = np.sqrt(Q / df) if df > 0 else 1.0

    return Q, df, Q_pval, I2, H


# ============================================================
# 4. 自动选择效应模型
# ============================================================
def choose_model(I2, Q_pval, alpha=0.10):
    """
    自动选择效应模型：
    - I² < 25% 且 Q 检验 p >= alpha → 固定效应模型
    - 否则 → 随机效应模型（DerSimonian-Laird）
    """
    if I2 < 25 and Q_pval >= alpha:
        return "fixed", "固定效应模型 (Fixed-Effect Model)"
    else:
        return "random", "随机效应模型 (Random-Effects Model, DL法)"


# ============================================================
# 5. 固定效应模型合并
# ============================================================
def fixed_effect_meta(y, v):
    """
    固定效应模型：逆方差加权法
    """
    w = 1.0 / v
    pooled = np.sum(w * y) / np.sum(w)
    var_pooled = 1.0 / np.sum(w)
    se_pooled = np.sqrt(var_pooled)
    z = pooled / se_pooled
    pval = 2 * (1 - stats.norm.cdf(abs(z)))

    # 各研究权重百分比
    weights_pct = w / np.sum(w) * 100

    # 置信区间
    ci_lower = pooled - 1.96 * se_pooled
    ci_upper = pooled + 1.96 * se_pooled

    return {
        'pooled': pooled,
        'se': se_pooled,
        'var': var_pooled,
        'z': z,
        'pval': pval,
        'ci_lower': ci_lower,
        'ci_upper': ci_upper,
        'weights_pct': weights_pct
    }


# ============================================================
# 6. 随机效应模型（DerSimonian-Laird）
# ============================================================
def random_effects_meta(y, v):
    """
    随机效应模型：DerSimonian-Laird 法
    """
    w = 1.0 / v
    y_bar_fe = np.sum(w * y) / np.sum(w)
    Q = np.sum(w * (y - y_bar_fe)**2)
    k = len(y)

    # DL 估计的 tau²
    C = np.sum(w) - np.sum(w**2) / np.sum(w)
    tau2 = max(0, (Q - (k - 1)) / C)

    # 随机效应权重
    w_re = 1.0 / (v + tau2)
    pooled = np.sum(w_re * y) / np.sum(w_re)
    var_pooled = 1.0 / np.sum(w_re)
    se_pooled = np.sqrt(var_pooled)
    z = pooled / se_pooled
    pval = 2 * (1 - stats.norm.cdf(abs(z)))

    weights_pct = w_re / np.sum(w_re) * 100

    ci_lower = pooled - 1.96 * se_pooled
    ci_upper = pooled + 1.96 * se_pooled

    return {
        'pooled': pooled,
        'se': se_pooled,
        'var': var_pooled,
        'z': z,
        'pval': pval,
        'ci_lower': ci_lower,
        'ci_upper': ci_upper,
        'tau2': tau2,
        'weights_pct': weights_pct
    }


# ============================================================
# 7. Egger 回归检验（发表偏倚）
# ============================================================
def egger_test(y, v):
    """
    Egger's 回归检验：以标准误为自变量，效应量/SE 为因变量
    回归截距显著 ≠ 0 提示发表偏倚
    """
    se = np.sqrt(v)
    # 精确度（1/SE）作为自变量，效应量/SE 作为因变量
    # 标准 Egger 回归：y/SE ~ 1/SE
    x = 1.0 / se
    y_std = y / se

    # 线性回归
    X = np.column_stack([np.ones_like(x), x])
    beta = np.linalg.lstsq(X, y_std, rcond=None)[0]

    # 残差和标准误
    residuals = y_std - X @ beta
    n = len(y)
    mse = np.sum(residuals**2) / (n - 2)
    var_beta = mse * np.linalg.inv(X.T @ X)
    se_beta = np.sqrt(np.diag(var_beta))

    # 截距项检验（bias）
    t_intercept = beta[0] / se_beta[0]
    p_intercept = 2 * (1 - stats.t.cdf(abs(t_intercept), df=n - 2))

    # 斜率
    t_slope = beta[1] / se_beta[1]
    p_slope = 2 * (1 - stats.t.cdf(abs(t_slope), df=n - 2))

    return {
        'intercept': beta[0],
        'intercept_se': se_beta[0],
        'intercept_t': t_intercept,
        'intercept_pval': p_intercept,
        'slope': beta[1],
        'slope_se': se_beta[1],
        'slope_t': t_slope,
        'slope_pval': p_slope
    }


# ============================================================
# 8. 剪补法（Trim and Fill）
# ============================================================
def trim_and_fill(y, v, side='auto', n_estimator='R0', max_iter=50):
    """Duval/Tweedie signed-rank trim-and-fill with DL pooling."""
    return trim_fill(y, v, side=side, estimator=n_estimator,
                     max_iter=max_iter, method='dl')


# ============================================================
# 9. 敏感性分析（Leave-One-Out）
# ============================================================
def sensitivity_analysis(y, v, model_type='random'):
    """
    敏感性分析：逐一剔除每个研究，重新计算合并效应量
    """
    k = len(y)
    results = []

    for i in range(k):
        y_loo = np.delete(y, i)
        v_loo = np.delete(v, i)

        if model_type == 'fixed':
            res = fixed_effect_meta(y_loo, v_loo)
        else:
            res = random_effects_meta(y_loo, v_loo)

        results.append({
            'Omitted': i,
            'Pooled': res['pooled'],
            'CI_lower': res['ci_lower'],
            'CI_upper': res['ci_upper'],
            'SE': res['se'],
            'Z': res['z'],
            'Pval': res['pval']
        })

    return pd.DataFrame(results)


# ============================================================
# 10. 亚组分析
# ============================================================
def subgroup_analysis(df, y_col='Cohen_d', v_col='Var_d', subgroup_col='Subgroup'):
    """
    亚组分析：按亚组分别进行 Meta 分析，并检验亚组间异质性
    """
    subgroups = df[subgroup_col].unique()
    results = {}

    all_y = df[y_col].values
    all_v = df[v_col].values

    for sg in sorted(subgroups):
        mask = df[subgroup_col] == sg
        y_sg = df.loc[mask, y_col].values
        v_sg = df.loc[mask, v_col].values

        # 异质性检验
        Q, Q_df, Q_pval, I2, H = heterogeneity_test(y_sg, v_sg)

        # 选择模型
        model_type, model_name = choose_model(I2, Q_pval)

        # 合并
        if model_type == 'fixed':
            res = fixed_effect_meta(y_sg, v_sg)
        else:
            res = random_effects_meta(y_sg, v_sg)

        results[sg] = {
            'k': len(y_sg),
            'model': model_name,
            'pooled': res['pooled'],
            'ci_lower': res['ci_lower'],
            'ci_upper': res['ci_upper'],
            'se': res['se'],
            'z': res['z'],
            'pval': res['pval'],
            'Q': Q,
            'Q_df': Q_df,
            'Q_pval': Q_pval,
            'I2': I2,
            'H': H
        }

    # 亚组间异质性检验（类似 ANOVA）
    # 用 Q_between 统计量
    # 先计算各亚组内合并效应
    # 然后计算亚组间 Q
    sg_estimates = []
    for sg in sorted(subgroups):
        mask = df[subgroup_col] == sg
        y_sg = df.loc[mask, y_col].values
        v_sg = df.loc[mask, v_col].values
        # 使用固定效应权重计算亚组内合并
        w_sg = 1.0 / v_sg
        pooled_sg = np.sum(w_sg * y_sg) / np.sum(w_sg)
        var_sg = 1.0 / np.sum(w_sg)
        sg_estimates.append({
            'subgroup': sg,
            'pooled': pooled_sg,
            'var': var_sg
        })

    # 总合并
    w_total = 1.0 / all_v
    pooled_total = np.sum(w_total * all_y) / np.sum(w_total)

    # Q_between
    Q_between = 0
    for sg_e in sg_estimates:
        w_sg = 1.0 / sg_e['var']
        Q_between += w_sg * (sg_e['pooled'] - pooled_total)**2

    df_between = len(subgroups) - 1
    Q_between_pval = 1 - stats.chi2.cdf(Q_between, df_between)

    return results, {
        'Q_between': Q_between,
        'df_between': df_between,
        'pval_between': Q_between_pval
    }


# ============================================================
# 11. 可视化函数
# ============================================================

def forest_plot(ax, y, v, study_names, model_results, model_type, model_name,
                subgroup=None, subgroup_colors=None):
    """
    绘制森林图（Forest Plot）
    """
    k = len(y)
    se = np.sqrt(v)
    ci_low = y - 1.96 * se
    ci_high = y + 1.96 * se

    # 排序：按效应量大小
    order = np.argsort(y)
    y_sorted = y[order]
    ci_low_sorted = ci_low[order]
    ci_high_sorted = ci_high[order]
    names_sorted = [study_names[i] for i in order]
    se_sorted = se[order]

    # 合并效应量
    pooled = model_results['pooled']
    pooled_ci_low = model_results['ci_lower']
    pooled_ci_high = model_results['ci_upper']

    # 设置颜色
    if subgroup is not None and subgroup_colors is not None:
        sg_sorted = [subgroup[i] for i in order]
        colors = [subgroup_colors.get(sg, '#333333') for sg in sg_sorted]
    else:
        colors = ['#4A90D9'] * k

    # 绘图
    y_positions = np.arange(k)

    # 权重
    weights_pct = model_results['weights_pct'][order]

    # 绘制每个研究
    for i in range(k):
        idx = k - 1 - i
        ax.plot([y_sorted[idx], y_sorted[idx]], [idx, idx],
                color=colors[idx], linewidth=1, zorder=2)
        # CI 线
        ax.plot([ci_low_sorted[idx], ci_high_sorted[idx]], [idx, idx],
                color=colors[idx], linewidth=2, zorder=2)
        # 方块（大小按权重）
        marker_size = 8 + 20 * np.sqrt(weights_pct[idx] / 100)
        ax.scatter(y_sorted[idx], idx, s=marker_size,
                   color=colors[idx], edgecolors='white', linewidth=0.5,
                   zorder=3)

    # 合并效应量菱形
    y_diamond = -1.5
    # 菱形顶点
    diamond_x = [pooled, pooled_ci_low, pooled, pooled_ci_high, pooled]
    diamond_y = [y_diamond, y_diamond - 0.3, y_diamond - 0.6, y_diamond - 0.3, y_diamond]
    ax.fill(diamond_x, diamond_y, color='#E74C3C', alpha=0.6, edgecolor='#C0392B',
            linewidth=1.5, zorder=4, label=f'合并效应量: {pooled:.3f}')

    # 垂直线（无效线 + 合并效应量线）
    ax.axvline(x=0, color='gray', linestyle='-', linewidth=0.8, alpha=0.5)
    ax.axvline(x=pooled, color='#E74C3C', linestyle='--', linewidth=0.8, alpha=0.4)

    # 坐标轴
    ax.set_yticks(list(range(k)))
    ax.set_yticklabels(names_sorted, fontsize=8)
    ax.set_xlabel('Hedges\' g (效应量)', fontsize=10)
    ax.set_title(f'森林图 (Forest Plot)\n{model_name}', fontsize=11, fontweight='bold')

    # 添加文本标注
    text_y = y_diamond - 0.8
    ax.text(0.02, text_y,
            f'合并效应量 = {pooled:.3f} [{pooled_ci_low:.3f}, {pooled_ci_high:.3f}]',
            transform=ax.get_xaxis_transform(), fontsize=9,
            bbox=dict(boxstyle='round,pad=0.3', facecolor='wheat', alpha=0.7))

    # 添加权重标注
    for i in range(k):
        idx = k - 1 - i
        ax.text(ci_high_sorted[idx] + 0.05, idx,
                f'{weights_pct[idx]:.1f}%',
                fontsize=7, va='center')

    ax.set_xlim(auto=True)
    return ax


def funnel_plot(ax, y, v, model_results, model_type, egger_result=None):
    """
    绘制漏斗图（Funnel Plot）
    """
    se = np.sqrt(v)
    pooled = model_results['pooled']

    # 散点
    ax.scatter(y, se, s=30, color='#4A90D9', alpha=0.7, edgecolors='white',
               linewidth=0.5, zorder=3)

    # 漏斗线
    se_range = np.linspace(0, max(se) * 1.1, 100)
    # 95% CI 线
    ax.plot(pooled + 1.96 * se_range, se_range, '--', color='#E74C3C',
            linewidth=1.2, alpha=0.6, label='95% CI')
    ax.plot(pooled - 1.96 * se_range, se_range, '--', color='#E74C3C',
            linewidth=1.2, alpha=0.6)
    # 中心线
    ax.axvline(x=pooled, color='#333333', linestyle='-', linewidth=1.2, alpha=0.7)

    # 倒置 y 轴（SE 大在上）
    ax.invert_yaxis()

    ax.set_xlabel('Hedges\' g (效应量)', fontsize=10)
    ax.set_ylabel('标准误 (Standard Error)', fontsize=10)
    ax.set_title('漏斗图 (Funnel Plot)', fontsize=11, fontweight='bold')

    # Egger 检验结果标注
    if egger_result:
        text = f"Egger's test:\nintercept = {egger_result['intercept']:.3f}\np = {egger_result['intercept_pval']:.4f}"
        ax.text(0.95, 0.95, text, transform=ax.transAxes, fontsize=8,
                verticalalignment='top', horizontalalignment='right',
                bbox=dict(boxstyle='round', facecolor='wheat', alpha=0.8))

    ax.legend(loc='lower right', fontsize=8)
    return ax


def funnel_plot_trimfill(ax, y, v, tf_result, model_results, model_type):
    """
    绘制含剪补法的漏斗图
    """
    se = np.sqrt(v)
    pooled_orig = tf_result['pooled_original']
    pooled_adj = tf_result['pooled_adjusted']

    # 原始研究
    ax.scatter(y, se, s=30, color='#4A90D9', alpha=0.7, edgecolors='white',
               linewidth=0.5, zorder=3, label='原始研究')

    # 填补的研究
    if tf_result['n_filled'] > 0:
        y_filled = tf_result['y_filled']
        v_filled = tf_result['v_filled']
        se_filled = np.sqrt(v_filled)
        # 找出填补的研究（原始索引之外的）
        filled_indices = np.arange(len(y_filled)) >= len(y)
        ax.scatter(y_filled[filled_indices], se_filled[filled_indices],
                   s=30, color='#E74C3C', alpha=0.7, edgecolors='darkred',
                   linewidth=0.5, marker='s', zorder=3, label='填补研究')

    # 漏斗线 - 原始合并
    se_range = np.linspace(0, max(se) * 1.1, 100)
    ax.plot(pooled_orig + 1.96 * se_range, se_range, '--', color='#E74C3C',
            linewidth=1, alpha=0.5, label='95% CI (原始)')
    ax.plot(pooled_orig - 1.96 * se_range, se_range, '--', color='#E74C3C',
            linewidth=1, alpha=0.5)
    ax.axvline(x=pooled_orig, color='#E74C3C', linestyle='-', linewidth=1.2,
               alpha=0.6, label=f'原始: {pooled_orig:.3f}')

    # 调整后合并
    if tf_result['n_filled'] > 0:
        ax.axvline(x=pooled_adj, color='#2ECC71', linestyle='-', linewidth=1.5,
                   alpha=0.8, label=f'调整后: {pooled_adj:.3f} [{tf_result["ci_lower_adjusted"]:.3f}, {tf_result["ci_upper_adjusted"]:.3f}]')

    ax.invert_yaxis()
    ax.set_xlabel('Hedges\' g (效应量)', fontsize=10)
    ax.set_ylabel('标准误 (Standard Error)', fontsize=10)
    ax.set_title(f'剪补法漏斗图 (Trim & Fill)\n缺失侧: {tf_result["side"]}, 填补: {tf_result["n_filled"]} 个研究',
                 fontsize=11, fontweight='bold')
    ax.legend(loc='lower right', fontsize=7)
    return ax


def sensitivity_plot(ax, sa_df, study_names, model_type):
    """
    敏感性分析图（Leave-One-Out）
    """
    k = len(sa_df)
    y_pos = np.arange(k)

    pooled_vals = sa_df['Pooled'].values
    ci_low = sa_df['CI_lower'].values
    ci_high = sa_df['CI_upper'].values

    # 按合并效应量排序
    order = np.argsort(pooled_vals)
    pooled_sorted = pooled_vals[order]
    ci_low_sorted = ci_low[order]
    ci_high_sorted = ci_high[order]
    names_sorted = [study_names[i] for i in order]

    for i in range(k):
        idx = k - 1 - i
        ax.plot([ci_low_sorted[idx], ci_high_sorted[idx]], [idx, idx],
                color='#4A90D9', linewidth=2, zorder=2)
        ax.scatter(pooled_sorted[idx], idx, s=40, color='#4A90D9',
                   edgecolors='white', linewidth=0.5, zorder=3)

    # 总合并效应量参考线
    overall_pooled = np.mean(pooled_vals)
    ax.axvline(x=overall_pooled, color='#E74C3C', linestyle='--', linewidth=1.2,
               alpha=0.7, label=f'平均值: {overall_pooled:.3f}')

    ax.set_yticks(list(range(k)))
    ax.set_yticklabels([f"剔除 {name}" for name in names_sorted], fontsize=7)
    ax.set_xlabel('合并效应量 (Hedges\' g) [95% CI]', fontsize=10)
    ax.set_title(f'敏感性分析 (Leave-One-Out)\n{model_type}模型', fontsize=11, fontweight='bold')
    ax.legend(loc='lower right', fontsize=8)
    return ax


def heterogeneity_plot(ax, Q, df, Q_pval, I2, H):
    """
    异质性可视化图
    """
    # 绘制 Q 统计量的分布
    x = np.linspace(0, max(Q * 1.5, 50), 500)
    y_chi2 = stats.chi2.pdf(x, df)

    ax.plot(x, y_chi2, 'b-', linewidth=2, label=f'χ²(df={df})')
    ax.fill_between(x, y_chi2, where=(x >= Q), color='#E74C3C', alpha=0.3,
                     label=f'Q = {Q:.2f}')
    ax.axvline(x=Q, color='#E74C3C', linestyle='--', linewidth=2)

    # 标注
    ax.set_xlabel('Q 统计量', fontsize=10)
    ax.set_ylabel('密度', fontsize=10)
    ax.set_title('异质性检验', fontsize=11, fontweight='bold')

    text = (f'Q = {Q:.2f}, df = {df}\n'
            f'p = {Q_pval:.4f}\n'
            f'I² = {I2:.1f}%\n'
            f'H = {H:.2f}')
    ax.text(0.95, 0.95, text, transform=ax.transAxes, fontsize=9,
            verticalalignment='top', horizontalalignment='right',
            bbox=dict(boxstyle='round', facecolor='wheat', alpha=0.8))
    ax.legend(loc='upper right', fontsize=8)
    return ax


def subgroup_forest_plot(ax, df, subgroup_results, between_test, y_col='Cohen_d', v_col='Var_d'):
    """
    亚组分析森林图
    """
    subgroups = sorted(df['Subgroup'].unique())
    colors = {'A': '#3498DB', 'B': '#2ECC71', 'C': '#E67E22'}
    y_offset = 0
    all_labels = []
    all_positions = []
    pooled_all = []

    for sg in subgroups:
        mask = df['Subgroup'] == sg
        y_sg = df.loc[mask, y_col].values
        v_sg = df.loc[mask, v_col].values
        names = df.loc[mask, 'Study'].values
        se_sg = np.sqrt(v_sg)
        ci_low = y_sg - 1.96 * se_sg
        ci_high = y_sg + 1.96 * se_sg

        # 亚组标签
        all_labels.append(f'--- 亚组 {sg} ---')
        all_positions.append(y_offset)
        y_offset += 1

        # 亚组内研究
        for i in range(len(y_sg)):
            all_labels.append(names[i])
            all_positions.append(y_offset)
            ax.plot([ci_low[i], ci_high[i]], [y_offset, y_offset],
                    color=colors.get(sg, '#333'), linewidth=1.5)
            ax.scatter(y_sg[i], y_offset, s=20, color=colors.get(sg, '#333'),
                       edgecolors='white', linewidth=0.5)
            y_offset += 1

        # 亚组合并效应量
        sg_res = subgroup_results[sg]
        all_labels.append(f'亚组 {sg} 合并: {sg_res["pooled"]:.3f} [{sg_res["ci_lower"]:.3f}, {sg_res["ci_upper"]:.3f}]')
        all_positions.append(y_offset)
        ax.plot([sg_res['ci_lower'], sg_res['ci_upper']], [y_offset, y_offset],
                color=colors.get(sg, '#333'), linewidth=3)
        ax.scatter(sg_res['pooled'], y_offset, s=60, color=colors.get(sg, '#333'),
                   marker='D', edgecolors='white', linewidth=1, zorder=4)
        pooled_all.append(sg_res['pooled'])
        y_offset += 2  # 亚组间空行

    ax.set_yticks(all_positions)
    ax.set_yticklabels(all_labels, fontsize=7)
    ax.axvline(x=0, color='gray', linestyle='-', linewidth=0.8, alpha=0.5)
    ax.set_xlabel('Hedges\' g (效应量)', fontsize=10)
    ax.set_title(f'亚组分析森林图\nQ_between = {between_test["Q_between"]:.2f}, '
                  f'p = {between_test["pval_between"]:.4f}',
                  fontsize=11, fontweight='bold')
    return ax


# ============================================================
# 12. 主函数
# ============================================================
def main():
    print("=" * 70)
    print("  连续型变量 Meta 分析完整流程")
    print("=" * 70)

    # 生成示例数据
    print("\n[1] 生成示例数据...")
    df = generate_example_data(42)
    print(f"  共 {len(df)} 个研究")
    print(f"  亚组分布: {df['Subgroup'].value_counts().to_dict()}")
    print(df[['Study', 'n1', 'n2', 'Cohen_d', 'Var_d', 'Subgroup']].to_string(index=False))

    # 计算 Hedges' g
    print("\n[2] 计算 Hedges' g (小样本校正)...")
    g_vals = []
    g_var = []
    for _, row in df.iterrows():
        g, vg = compute_hedges_g(row['Cohen_d'], row['Var_d'], row['n1'], row['n2'])
        g_vals.append(g)
        g_var.append(vg)
    df['Hedges_g'] = g_vals
    df['Var_g'] = g_var
    print(f"  Hedges' g 范围: {min(g_vals):.3f} ~ {max(g_vals):.3f}")

    # 异质性检验
    print("\n[3] 异质性检验...")
    Q, Q_df, Q_pval, I2, H = heterogeneity_test(df['Hedges_g'].values, df['Var_g'].values)
    print(f"  Q = {Q:.3f}, df = {Q_df}")
    print(f"  异质性检验 p = {Q_pval:.4f}")
    print(f"  I² = {I2:.1f}%")
    print(f"  H = {H:.3f}")

    # 自动选择模型
    print("\n[4] 自动选择效应模型...")
    model_type, model_name = choose_model(I2, Q_pval)
    print(f"  选择: {model_name}")

    # 合并效应量
    print("\n[5] 合并效应量...")
    if model_type == 'fixed':
        results = fixed_effect_meta(df['Hedges_g'].values, df['Var_g'].values)
    else:
        results = random_effects_meta(df['Hedges_g'].values, df['Var_g'].values)

    print(f"  合并效应量 = {results['pooled']:.4f}")
    print(f"  95% CI: [{results['ci_lower']:.4f}, {results['ci_upper']:.4f}]")
    print(f"  Z = {results['z']:.3f}, p = {results['pval']:.4f}")
    if 'tau2' in results:
        print(f"  τ² = {results['tau2']:.4f}")

    # Egger 检验
    print("\n[6] 发表偏倚检验 - Egger's 回归...")
    egger = egger_test(df['Hedges_g'].values, df['Var_g'].values)
    print(f"  截距 = {egger['intercept']:.4f} (SE = {egger['intercept_se']:.4f})")
    print(f"  t = {egger['intercept_t']:.3f}, p = {egger['intercept_pval']:.4f}")
    if egger['intercept_pval'] < 0.05:
        print("  → 存在显著发表偏倚 (p < 0.05)")
    else:
        print("  → 未发现显著发表偏倚 (p ≥ 0.05)")

    # 剪补法
    print("\n[7] 发表偏倚检验 - 剪补法 (Trim & Fill)...")
    tf = trim_and_fill(df['Hedges_g'].values, df['Var_g'].values, side='auto')
    print(f"  缺失侧: {tf['side']}")
    print(f"  填补研究数: {tf['n_filled']}")
    print(f"  原始合并效应量: {tf['pooled_original']:.4f}")
    print(f"  调整后合并效应量: {tf['pooled_adjusted']:.4f}")
    if tf['n_filled'] > 0:
        print(f"  调整后 95% CI: [{tf['ci_lower_adjusted']:.4f}, {tf['ci_upper_adjusted']:.4f}]")

    # 敏感性分析
    print("\n[8] 敏感性分析 (Leave-One-Out)...")
    sa_df = sensitivity_analysis(df['Hedges_g'].values, df['Var_g'].values, model_type)
    print(f"  剔除后合并效应量范围: {sa_df['Pooled'].min():.4f} ~ {sa_df['Pooled'].max():.4f}")
    print(f"  总体平均: {sa_df['Pooled'].mean():.4f}")
    # 找出影响最大的研究
    influence = abs(sa_df['Pooled'] - sa_df['Pooled'].mean())
    max_influence_idx = influence.idxmax()
    print(f"  影响最大的研究: {df.iloc[max_influence_idx]['Study']} "
          f"(剔除后合并效应量 = {sa_df.iloc[max_influence_idx]['Pooled']:.4f})")

    # 亚组分析
    print("\n[9] 亚组分析...")
    sg_results, between_test = subgroup_analysis(df, 'Hedges_g', 'Var_g', 'Subgroup')
    for sg, res in sg_results.items():
        print(f"  亚组 {sg}: k={res['k']}, 合并={res['pooled']:.4f} "
              f"[{res['ci_lower']:.4f}, {res['ci_upper']:.4f}], "
              f"I²={res['I2']:.1f}%, 模型={res['model']}")
    print(f"  亚组间异质性: Q_between = {between_test['Q_between']:.3f}, "
          f"p = {between_test['pval_between']:.4f}")

    # ==========================================
    # 10. 可视化
    # ==========================================
    print("\n[10] 生成可视化图表...")

    fig = plt.figure(figsize=(18, 20))
    gs = fig.add_gridspec(4, 3, hspace=0.35, wspace=0.3)

    # 10a. 森林图
    ax1 = fig.add_subplot(gs[0, :])
    forest_plot(ax1, df['Hedges_g'].values, df['Var_g'].values,
                df['Study'].values, results, model_type, model_name)

    # 10b. 漏斗图
    ax2 = fig.add_subplot(gs[1, 0])
    funnel_plot(ax2, df['Hedges_g'].values, df['Var_g'].values,
                results, model_type, egger)

    # 10c. 剪补法漏斗图
    ax3 = fig.add_subplot(gs[1, 1])
    funnel_plot_trimfill(ax3, df['Hedges_g'].values, df['Var_g'].values,
                         tf, results, model_type)

    # 10d. 异质性图
    ax4 = fig.add_subplot(gs[1, 2])
    heterogeneity_plot(ax4, Q, Q_df, Q_pval, I2, H)

    # 10e. 敏感性分析图
    ax5 = fig.add_subplot(gs[2, :])
    sensitivity_plot(ax5, sa_df, df['Study'].values, model_name)

    # 10f. 亚组分析森林图
    ax6 = fig.add_subplot(gs[3, :])
    subgroup_forest_plot(ax6, df, sg_results, between_test)

    plt.suptitle('连续型变量 Meta 分析结果汇总', fontsize=14, fontweight='bold', y=0.98)
    plt.savefig('meta_analysis_results.png', dpi=200, bbox_inches='tight')
    print("  → 已保存: meta_analysis_results.png")

    # ==========================================
    # 11. 输出汇总表
    # ==========================================
    print("\n" + "=" * 70)
    print("  Meta 分析结果汇总")
    print("=" * 70)
    print(f"\n  效应模型: {model_name}")
    print(f"  合并效应量 (Hedges' g) = {results['pooled']:.4f}")
    print(f"  95% 置信区间: [{results['ci_lower']:.4f}, {results['ci_upper']:.4f}]")
    print(f"  检验: Z = {results['z']:.3f}, p = {results['pval']:.4f}")
    print()
    print(f"  异质性: Q = {Q:.2f}, df = {Q_df}, p = {Q_pval:.4f}")
    print(f"          I² = {I2:.1f}%, H = {H:.3f}")
    print()
    print(f"  发表偏倚 (Egger): 截距 = {egger['intercept']:.4f}, p = {egger['intercept_pval']:.4f}")
    print(f"  发表偏倚 (剪补法): 填补 {tf['n_filled']} 个研究, "
          f"调整后合并 = {tf['pooled_adjusted']:.4f}")
    print()
    print(f"  亚组分析: Q_between = {between_test['Q_between']:.2f}, "
          f"p = {between_test['pval_between']:.4f}")
    print("=" * 70)

    # 显示图片
    print("\n  图片已保存至: meta_analysis_results.png")
    print("=" * 70)

    return df, results, egger, tf, sa_df, sg_results


if __name__ == '__main__':
    df, results, egger, tf, sa_df, sg_results = main()