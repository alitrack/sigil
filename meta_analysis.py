#!/usr/bin/env python3
"""
连续型变量 Meta 分析 — 完整统计流程
====================================
功能：
  1. 生成示例数据（连续型变量，SMD 效应量）
  2. 自动选择效应模型（固定/随机，基于 I² 与 Q 检验）
  3. 亚组分析（组间异质性 Q 检验）
  4. 异质性检验（Cochran's Q, I², τ²）
  5. 发表偏倚检验（Egger's 回归 + 剪补法 Trim & Fill）
  6. 敏感性分析（Leave-One-Out）
  7. 输出 6 张可视化图表
"""

import numpy as np
import pandas as pd
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
from scipy import stats
from statsmodels.stats.meta_analysis import combine_effects
from meta_analysis_methods import trim_fill, hksj_interval
import os
import warnings
warnings.filterwarnings('ignore')

# ============================================================
# 0. 辅助函数
# ============================================================
def safe_i2(res):
    """I² 截断到 [0, 1] 范围"""
    i2 = getattr(res, 'i2', None)
    if i2 is None:
        # 自行计算
        h2 = res.q / (res.k - 1) if res.k > 1 else 1
        i2 = 1 - 1 / h2 if h2 > 1 else 0
    else:
        i2 = max(0, min(1, i2))
    return i2


# ============================================================
# 1. 生成示例数据
# ============================================================
np.random.seed(42)

def generate_example_data(n_studies=20):
    """
    模拟连续型变量 Meta 分析数据
    生成 SMD (标准化均数差) 及其方差
    含两个亚组，具有一定异质性
    """
    studies = []
    # 两个亚组: 高强度干预 vs 低强度干预
    group_labels = ['高强度干预'] * (n_studies // 2) + ['低强度干预'] * (n_studies - n_studies // 2)

    for i in range(n_studies):
        n1 = np.random.randint(20, 80)
        n2 = np.random.randint(20, 80)
        # 亚组效应差异
        if group_labels[i] == '高强度干预':
            true_effect = 0.65 + np.random.normal(0, 0.12)
        else:
            true_effect = 0.25 + np.random.normal(0, 0.10)
        # 标准误与研究规模相关
        se = 0.10 + 0.25 / np.sqrt(n1 + n2) * 5 + np.random.uniform(0, 0.05)
        variance = se ** 2
        studies.append({
            'study': f'研究_{i+1:02d}',
            'effect': true_effect,
            'variance': variance,
            'se': np.sqrt(variance),
            'n1': n1,
            'n2': n2,
            'group': group_labels[i],
            'year': np.random.randint(2015, 2024)
        })
    return pd.DataFrame(studies)

df = generate_example_data(20)
print("=" * 72)
print("  📊 连续型变量 Meta 分析 — 完整统计报告")
print("=" * 72)

print("\n【数据概览】")
print(f"  研究数量: {len(df)}")
print(f"  效应量范围: {df['effect'].min():.3f} ~ {df['effect'].max():.3f}")
print(f"  标准误范围: {df['se'].min():.3f} ~ {df['se'].max():.3f}")
print(f"  亚组: {df['group'].unique().tolist()}")
print(f"\n  前 6 行数据:")
print(df[['study', 'effect', 'se', 'group']].head(6).to_string(index=False))

# ============================================================
# 2. 执行 Meta 分析
# ============================================================
eff = df['effect'].values
var_eff = df['variance'].values
study_names = df['study'].tolist()

# 2a. 固定效应模型
res_fe = combine_effects(eff, var_eff, method_re='chi2', row_names=study_names, use_t=False)

# 2b. 随机效应模型 (Paule-Mandel 迭代法)
res_re = combine_effects(eff, var_eff, method_re='iterated', row_names=study_names, use_t=True)

# ============================================================
# 3. 异质性检验 & 自动选择效应模型
# ============================================================
i2 = safe_i2(res_re)
p_het = 1 - stats.chi2.cdf(res_re.q, res_re.k - 1)

print("\n" + "=" * 72)
print("  🔄 异质性检验")
print("=" * 72)
print(f"  Cochran's Q 统计量: {res_re.q:.3f}")
print(f"  自由度 (df): {res_re.k - 1}")
print(f"  I² 异质性指数: {i2:.2%}")
print(f"  τ² (组间方差): {res_re.tau2:.6f}")
print(f"  异质性 P 值: {p_het:.4f}")

if p_het < 0.05 or i2 > 0.50:
    print(f"  ⚠️  异质性显著 (I²={i2:.1%}, P={p_het:.4f}) → 选用【随机效应模型】")
    use_random = True
    res = res_re
else:
    print(f"  ✅ 异质性不显著 (I²={i2:.1%}, P={p_het:.4f}) → 选用【固定效应模型】")
    use_random = False
    res = res_fe

# ============================================================
# 4. 合并效应量结果
# ============================================================
print("\n" + "=" * 72)
print("  📈 合并效应量")
print("=" * 72)

# 固定效应 CI
ci_fe_low = res_fe.mean_effect_fe - 1.96 * res_fe.sd_eff_w_fe
ci_fe_upp = res_fe.mean_effect_fe + 1.96 * res_fe.sd_eff_w_fe
# 随机效应 CI (t 分布)
ci_re_low = res_re.mean_effect_re - stats.t.ppf(0.975, res_re.k-1) * res_re.sd_eff_w_re
ci_re_upp = res_re.mean_effect_re + stats.t.ppf(0.975, res_re.k-1) * res_re.sd_eff_w_re

# HKSJ 调整后的 CI
ci_re_hksj_low, ci_re_hksj_upp = hksj_interval(res_re)

print(f"\n  固定效应模型:  {res_fe.mean_effect_fe:.4f}  [{ci_fe_low:.4f}, {ci_fe_upp:.4f}]")
print(f"  随机效应模型:  {res_re.mean_effect_re:.4f}  [{ci_re_low:.4f}, {ci_re_upp:.4f}]")
print(f"  τ²: {res_re.tau2:.6f}")

if use_random:
    print(f"\n  ✅ 最终采用: 随机效应模型 (HKSJ 校正)")
    print(f"  合并效应量 = {res_re.mean_effect_re:.4f}  [{ci_re_hksj_low:.4f}, {ci_re_hksj_upp:.4f}]")
    z_stat = res_re.mean_effect_re / np.sqrt(res_re.var_hksj_re)
    p_val = 2 * (1 - stats.t.cdf(abs(z_stat), res_re.k - 1))
    print(f"  Z = {z_stat:.3f}, P = {p_val:.4f}")
else:
    print(f"\n  ✅ 最终采用: 固定效应模型")
    print(f"  合并效应量 = {res_fe.mean_effect_fe:.4f}  [{ci_fe_low:.4f}, {ci_fe_upp:.4f}]")
    z_stat = res_fe.mean_effect_fe / res_fe.sd_eff_w_fe
    p_val = 2 * (1 - stats.norm.cdf(abs(z_stat)))
    print(f"  Z = {z_stat:.3f}, P = {p_val:.4f}")

# ============================================================
# 5. 亚组分析
# ============================================================
print("\n" + "=" * 72)
print("  🔬 亚组分析")
print("=" * 72)

groups = df['group'].unique()
subgroup_results = {}
for g in groups:
    mask = df['group'] == g
    sub_eff = df.loc[mask, 'effect'].values
    sub_var = df.loc[mask, 'variance'].values
    sub_names = df.loc[mask, 'study'].tolist()
    sub_res = combine_effects(sub_eff, sub_var, method_re='iterated',
                              row_names=sub_names, use_t=True)
    sub_i2 = safe_i2(sub_res)
    low, upp = hksj_interval(sub_res)
    subgroup_results[g] = {
        'k': sub_res.k,
        'effect': sub_res.mean_effect_re,
        'ci_low': low,
        'ci_upp': upp,
        'i2': sub_i2,
        'tau2': sub_res.tau2
    }
    print(f"\n  【{g}】({sub_res.k} 项研究)")
    print(f"    合并效应量 = {sub_res.mean_effect_re:.4f}  [{low:.4f}, {upp:.4f}]")
    print(f"    I² = {sub_i2:.2%},  τ² = {sub_res.tau2:.6f}")

# 亚组间异质性 Q 检验
q_within = 0
for g in groups:
    mask = df['group'] == g
    sub_eff = df.loc[mask, 'effect'].values
    sub_var = df.loc[mask, 'variance'].values
    sub_res_fe = combine_effects(sub_eff, sub_var, method_re='chi2', use_t=False)
    q_within += sub_res_fe.q

q_between = res_fe.q - q_within
df_between = len(groups) - 1
p_between = 1 - stats.chi2.cdf(q_between, df_between)

print(f"\n  亚组间异质性: Q_between = {q_between:.3f}, df = {df_between}, P = {p_between:.4f}")
if p_between < 0.05:
    print(f"  ⚠️  亚组间差异显著 (P={p_between:.4f}) → 亚组效应存在统计学差异")
else:
    print(f"  ✅ 亚组间差异不显著 (P={p_between:.4f})")

# ============================================================
# 6. 发表偏倚检验
# ============================================================
print("\n" + "=" * 72)
print("  📉 发表偏倚检验")
print("=" * 72)

# 6a. Egger's 回归检验
precision = 1.0 / np.sqrt(var_eff)
snd = eff / np.sqrt(var_eff)

X_egger = np.column_stack([np.ones_like(precision), precision])
weights_egger = 1.0 / np.sqrt(var_eff)
W = np.diag(weights_egger)
XtWX = X_egger.T @ W @ X_egger
XtWy = X_egger.T @ W @ snd
beta_egger = np.linalg.solve(XtWX, XtWy)
residuals = snd - X_egger @ beta_egger
mse = np.sum(residuals**2 * weights_egger) / (len(snd) - 2)
se_beta = np.sqrt(mse * np.diag(np.linalg.inv(XtWX)))
t_stats = beta_egger / se_beta
p_egger = 2 * (1 - stats.t.cdf(abs(t_stats), len(snd) - 2))

print(f"\n  Egger's 回归检验 (加权线性回归):")
print(f"  截距 (bias): {beta_egger[0]:.4f} (SE={se_beta[0]:.4f}, t={t_stats[0]:.3f}, P={p_egger[0]:.4f})")
print(f"  斜率:        {beta_egger[1]:.4f} (SE={se_beta[1]:.4f}, t={t_stats[1]:.3f}, P={p_egger[1]:.4f})")

if p_egger[0] < 0.05:
    print(f"  ⚠️  Egger's 检验 P={p_egger[0]:.4f} < 0.05 → 可能存在发表偏倚")
else:
    print(f"  ✅ Egger's 检验 P={p_egger[0]:.4f} ≥ 0.05 → 未检测到显著发表偏倚")

# 6b. 剪补法 (Trim & Fill) — 简化版
def trim_and_fill(eff, var_eff, side='right'):
    """Duval/Tweedie R0 trim-and-fill, with Paule-Mandel/HKSJ inference."""
    result = trim_fill(eff, var_eff, side=side, method='pm', hksj=True)
    return (result['y_filled'], result['v_filled'], result['n_filled'],
            result['pooled_adjusted'],
            (result['ci_lower_adjusted'], result['ci_upper_adjusted']))

filled_eff, filled_var, k0, filled_effect, filled_ci = trim_and_fill(eff, var_eff, side='right')

print(f"\n  剪补法 (Trim & Fill):")
if k0 > 0:
    print(f"  估计缺失研究数: k₀ = {k0}")
    print(f"  填补后合并效应量 (RE): {filled_effect:.4f}  [{filled_ci[0]:.4f}, {filled_ci[1]:.4f}]")
else:
    print(f"  未检测到缺失研究 (k₀ = 0)，漏斗图对称")

# ============================================================
# 7. 敏感性分析 (Leave-One-Out)
# ============================================================
print("\n" + "=" * 72)
print("  🔧 敏感性分析 (Leave-One-Out)")
print("=" * 72)

loo_results = []
for i in range(len(df)):
    mask = np.ones(len(df), dtype=bool)
    mask[i] = False
    loo_eff = eff[mask]
    loo_var = var_eff[mask]
    loo_names = np.array(study_names)[mask].tolist()
    loo_res = combine_effects(loo_eff, loo_var, method_re='iterated',
                              row_names=loo_names, use_t=True)
    low, upp = hksj_interval(loo_res)
    loo_results.append({
        'removed': study_names[i],
        'effect': loo_res.mean_effect_re,
        'ci_low': low,
        'ci_upp': upp,
        'i2': safe_i2(loo_res),
        'tau2': loo_res.tau2
    })
    print(f"  剔除 {study_names[i]:>6s}: {loo_res.mean_effect_re:.4f}  [{low:.4f}, {upp:.4f}]  I²={safe_i2(loo_res):.0%}")

loo_df = pd.DataFrame(loo_results)
effect_range = loo_df['effect'].max() - loo_df['effect'].min()
print(f"\n  Leave-One-Out 效应量范围: {loo_df['effect'].min():.4f} ~ {loo_df['effect'].max():.4f}")
print(f"  范围宽度: {effect_range:.4f}")
if effect_range < 0.3:
    print(f"  ✅ 结果稳健: 剔除任一研究后合并效应量波动小")
else:
    print(f"  ⚠️  结果敏感度高: 剔除部分研究后效应量波动较大")

# ============================================================
# 8. 可视化图表
# ============================================================
print("\n" + "=" * 72)
print("  🎨 生成可视化图表...")
print("=" * 72)

output_dir = "meta_analysis_output"
os.makedirs(output_dir, exist_ok=True)

# 颜色方案
colors_group = {
    groups[0]: '#E53935' if '高' in groups[0] else '#1565C0',
    groups[1]: '#E53935' if '高' in groups[1] else '#1565C0'
}
# 如果名字不是高低，用不同颜色
if len(groups) >= 2:
    colors_group[groups[0]] = '#E53935'
    colors_group[groups[1]] = '#1565C0'

# 统一使用
color_a = '#E53935'
color_b = '#1565C0'
color_overall = '#4CAF50'

# 获取最终使用的效应量
if use_random:
    overall = res_re.mean_effect_re
    overall_low = ci_re_hksj_low
    overall_upp = ci_re_hksj_upp
    model_label = '随机效应模型 (HKSJ)'
else:
    overall = res_fe.mean_effect_fe
    overall_low = ci_fe_low
    overall_upp = ci_fe_upp
    model_label = '固定效应模型'

# 按效应量排序
sort_idx = np.argsort(eff)
sorted_df = df.iloc[sort_idx].reset_index(drop=True)
sorted_eff = eff[sort_idx]
sorted_var = var_eff[sort_idx]
sorted_names = np.array(study_names)[sort_idx]
sorted_se = np.sqrt(sorted_var)
sorted_group = sorted_df['group'].values

# ====== 8a. 森林图 ======
fig, ax = plt.subplots(figsize=(14, 9))
y_pos = np.arange(len(sorted_df))

for i in range(len(sorted_df)):
    y = y_pos[i]
    ci_low = sorted_eff[i] - 1.96 * sorted_se[i]
    ci_upp = sorted_eff[i] + 1.96 * sorted_se[i]
    color = colors_group.get(sorted_group[i], '#2196F3')
    ax.plot(sorted_eff[i], y, 'D', color=color, markersize=8, zorder=5)
    ax.plot([ci_low, ci_upp], [y, y], '-', color=color, linewidth=2, zorder=4)

# 合并效应量菱形
y_overall = -1.5
diamond_h = 0.6
dx = [overall_low, overall, overall_upp, overall]
dy = [y_overall, y_overall + diamond_h, y_overall, y_overall - diamond_h]
ax.fill(dx, dy, color=color_overall, alpha=0.7, edgecolor='#2E7D32', linewidth=1.5, zorder=6)
ax.plot([overall_low, overall_upp], [y_overall, y_overall], '-', color='#2E7D32', linewidth=2, zorder=5)

ax.axvline(x=0, color='gray', linestyle='--', alpha=0.5, linewidth=1)
ax.axvline(x=overall, color='#2E7D32', linestyle=':', alpha=0.4, linewidth=1)

# 标签
ax.set_yticks(list(y_pos) + [y_overall])
ax.set_yticklabels(list(sorted_names) + [f'合并效应量 ({model_label})'], fontsize=10)
ax.set_xlabel('效应量 (SMD, 标准化均数差)', fontsize=12)
ax.set_title('森林图 (Forest Plot)', fontsize=15, fontweight='bold')

# 数值标注
for i in range(len(sorted_df)):
    y = y_pos[i]
    ci_low = sorted_eff[i] - 1.96 * sorted_se[i]
    ci_upp = sorted_eff[i] + 1.96 * sorted_se[i]
    ax.text(ci_upp + 0.03, y, f'{sorted_eff[i]:.2f} [{ci_low:.2f}, {ci_upp:.2f}]',
            va='center', fontsize=8, fontfamily='monospace')

ax.text(overall_upp + 0.03, y_overall,
        f'{overall:.2f} [{overall_low:.2f}, {overall_upp:.2f}]',
        va='center', fontsize=9, fontweight='bold', fontfamily='monospace')

legend_elements = [
    mpatches.Patch(color=color_a, label=groups[0]),
    mpatches.Patch(color=color_b, label=groups[1]),
    mpatches.Patch(color=color_overall, label=model_label)
]
ax.legend(handles=legend_elements, loc='lower right', fontsize=10, framealpha=0.9)

x_margin = max(sorted_eff + 1.96 * sorted_se) * 0.15 + 0.15
ax.set_xlim(left=min(sorted_eff - 1.96 * sorted_se) - 0.2,
            right=max(sorted_eff + 1.96 * sorted_se) + 0.6)
ax.invert_yaxis()
ax.grid(axis='x', alpha=0.3)
plt.tight_layout()
plt.savefig(f'{output_dir}/01_forest_plot.png', dpi=150, bbox_inches='tight')
plt.close()
print(f"  ✅ 森林图 → {output_dir}/01_forest_plot.png")

# ====== 8b. 漏斗图 ======
fig, ax = plt.subplots(figsize=(10, 8))

for g in groups:
    mask = df['group'] == g
    ax.scatter(df.loc[mask, 'effect'], 1 / np.sqrt(df.loc[mask, 'variance']),
               c=colors_group[g], s=90, alpha=0.7, edgecolors='white', linewidth=1,
               label=g, zorder=5)

# 漏斗 95% CI 区域
max_se = max(np.sqrt(var_eff))
funnel_se = np.linspace(0.02, max_se, 100)
funnel_left = overall - 1.96 * funnel_se
funnel_right = overall + 1.96 * funnel_se
ax.fill_betweenx(1/funnel_se, funnel_left, funnel_right, alpha=0.08, color='gray')
ax.plot(funnel_left, 1/funnel_se, '--', color='gray', alpha=0.5, linewidth=1)
ax.plot(funnel_right, 1/funnel_se, '--', color='gray', alpha=0.5, linewidth=1)
ax.axvline(x=overall, color=color_overall, linestyle='-', linewidth=2, alpha=0.7,
           label=f'合并效应量 = {overall:.3f}')

# 剪补法填补的研究
if k0 > 0:
    filled_eff_plot = filled_eff[-k0:]
    filled_se_plot = np.sqrt(filled_var[-k0:])
    ax.scatter(filled_eff_plot, 1/filled_se_plot, c='red', s=60, alpha=0.4,
               marker='o', facecolors='none', linewidth=2, label=f'填补研究 (k₀={k0})')

ax.set_xlabel('效应量 (SMD)', fontsize=12)
ax.set_ylabel('精度 (1/SE)', fontsize=12)
ax.set_title('漏斗图 (Funnel Plot)', fontsize=14, fontweight='bold')
ax.legend(loc='best', fontsize=10)
ax.grid(alpha=0.3)
plt.tight_layout()
plt.savefig(f'{output_dir}/02_funnel_plot.png', dpi=150, bbox_inches='tight')
plt.close()
print(f"  ✅ 漏斗图 → {output_dir}/02_funnel_plot.png")

# ====== 8c. 亚组森林图 ======
fig, ax = plt.subplots(figsize=(14, 8))

# 按亚组排序
group_order = sorted(df['group'].unique())
plot_data = []
for g in group_order:
    sub = df[df['group'] == g].sort_values('effect')
    plot_data.append(sub)

y_pos_list = []
y_labels = []
current_y = 0
subgroup_boundaries = []

for g_idx, sub_df in enumerate(plot_data):
    n = len(sub_df)
    for i in range(n):
        y = current_y + i
        y_pos_list.append(y)
        eff_val = sub_df.iloc[i]['effect']
        se_val = sub_df.iloc[i]['se']
        ci_l = eff_val - 1.96 * se_val
        ci_u = eff_val + 1.96 * se_val
        color = colors_group[sub_df.iloc[i]['group']]
        ax.plot(eff_val, y, 's', color=color, markersize=7, zorder=5)
        ax.plot([ci_l, ci_u], [y, y], '-', color=color, linewidth=1.8, zorder=4)
        y_labels.append(sub_df.iloc[i]['study'])

    # 亚组汇总菱形
    sr = subgroup_results[g]
    sub_y_mean = current_y + (n - 1) / 2
    dh = 0.4
    dx_s = [sr['ci_low'], sr['effect'], sr['ci_upp'], sr['effect']]
    dy_s = [sub_y_mean, sub_y_mean + dh, sub_y_mean, sub_y_mean - dh]
    ax.fill(dx_s, dy_s, color=colors_group[g], alpha=0.5, edgecolor='black', linewidth=1.2, zorder=6)
    ax.text(sr['ci_upp'] + 0.05, sub_y_mean,
            f'{sr["effect"]:.2f} [{sr["ci_low"]:.2f}, {sr["ci_upp"]:.2f}]',
            va='center', fontsize=9, fontweight='bold')

    subgroup_boundaries.append((current_y, current_y + n - 1, g, sr))
    current_y += n + 1  # 亚组间留空

# 总体合并菱形
y_total = current_y
dh_t = 0.5
dx_t = [overall_low, overall, overall_upp, overall]
dy_t = [y_total, y_total + dh_t, y_total, y_total - dh_t]
ax.fill(dx_t, dy_t, color=color_overall, alpha=0.7, edgecolor='#2E7D32', linewidth=1.5, zorder=6)
ax.plot([overall_low, overall_upp], [y_total, y_total], '-', color='#2E7D32', linewidth=2, zorder=5)
ax.text(overall_upp + 0.05, y_total,
        f'{overall:.2f} [{overall_low:.2f}, {overall_upp:.2f}]',
        va='center', fontsize=10, fontweight='bold')

y_labels.append(f'总体 ({model_label})')
y_pos_list.append(y_total)

ax.axvline(x=0, color='gray', linestyle='--', alpha=0.5)
ax.set_yticks(y_pos_list)
ax.set_yticklabels(y_labels, fontsize=9)
ax.invert_yaxis()
ax.set_xlabel('效应量 (SMD)', fontsize=12)
ax.set_title('亚组森林图 (Subgroup Forest Plot)', fontsize=14, fontweight='bold')
ax.grid(axis='x', alpha=0.3)

# 亚组分割线
for start, end, g, sr in subgroup_boundaries:
    mid = (start + end) / 2
    ax.annotate(f'{g}  (I²={sr["i2"]:.0%}, k={sr["k"]})',
                xy=(0.02, 1 - (mid + 1) / (y_total + 2)),
                xycoords='axes fraction', fontsize=10, fontweight='bold',
                color=colors_group[g], ha='left', va='center')

plt.tight_layout()
plt.savefig(f'{output_dir}/03_subgroup_forest.png', dpi=150, bbox_inches='tight')
plt.close()
print(f"  ✅ 亚组森林图 → {output_dir}/03_subgroup_forest.png")

# ====== 8d. 敏感性分析 ======
fig, ax = plt.subplots(figsize=(14, 8))

loo_effects = [r['effect'] for r in loo_results]
loo_lows = [r['ci_low'] for r in loo_results]
loo_upps = [r['ci_upp'] for r in loo_results]
loo_names = [r['removed'] for r in loo_results]

y_loo = range(len(loo_results), 0, -1)
for i in range(len(loo_results)):
    y = y_loo[i]
    # 颜色: 若剔除后 CI 跨越 0 则标红
    if loo_lows[i] > 0:
        color = '#E53935'
    elif loo_upps[i] < 0:
        color = '#E53935'
    else:
        color = '#1565C0'
    ax.plot(loo_effects[i], y, 'o', color=color, markersize=9, zorder=5)
    ax.plot([loo_lows[i], loo_upps[i]], [y, y], '-', color=color, linewidth=2.5, zorder=4)
    ax.plot([loo_lows[i], loo_upps[i]], [y, y], '-', color=color, linewidth=2.5, zorder=4)

# 总体效应参考区间
ax.axvline(x=overall, color=color_overall, linestyle='-', linewidth=2.5, alpha=0.8,
           label=f'总体效应量 = {overall:.3f}')
ax.axvline(x=overall_low, color=color_overall, linestyle=':', linewidth=1.5, alpha=0.5)
ax.axvline(x=overall_upp, color=color_overall, linestyle=':', linewidth=1.5, alpha=0.5)
ax.fill_betweenx([min(y_loo)-1, max(y_loo)+1], overall_low, overall_upp,
                 alpha=0.05, color=color_overall)
ax.axvline(x=0, color='gray', linestyle='--', alpha=0.5)

ax.set_yticks(list(y_loo))
ax.set_yticklabels([f'剔除 {n}' for n in loo_names], fontsize=9)
ax.set_xlabel('效应量 (SMD)', fontsize=12)
ax.set_title('敏感性分析 — Leave-One-Out (逐一剔除)', fontsize=14, fontweight='bold')
ax.legend(loc='best', fontsize=10)
ax.grid(axis='x', alpha=0.3)
plt.tight_layout()
plt.savefig(f'{output_dir}/04_sensitivity_loo.png', dpi=150, bbox_inches='tight')
plt.close()
print(f"  ✅ 敏感性分析图 → {output_dir}/04_sensitivity_loo.png")

# ====== 8e. Egger's 回归图 ======
fig, ax = plt.subplots(figsize=(10, 7))

for g in groups:
    mask = df['group'] == g
    ax.scatter(precision[mask], snd[mask], c=colors_group[g], s=90, alpha=0.7,
               edgecolors='white', linewidth=1, label=g, zorder=5)

x_line = np.linspace(min(precision) - 0.5, max(precision) + 0.5, 100)
y_line = beta_egger[0] + beta_egger[1] * x_line
ax.plot(x_line, y_line, '-', color='#E53935', linewidth=2.5, zorder=4,
        label=f'Egger 回归线 (截距={beta_egger[0]:.3f}, P={p_egger[0]:.4f})')

# 95% CI 带
x_pred = np.linspace(min(precision), max(precision), 50)
X_pred = np.column_stack([np.ones_like(x_pred), x_pred])
pred_se = np.sqrt(np.sum((X_pred @ np.linalg.inv(XtWX)) * X_pred, axis=1) * mse)
y_pred = beta_egger[0] + beta_egger[1] * x_pred
ax.fill_between(x_pred, y_pred - 1.96 * pred_se, y_pred + 1.96 * pred_se,
                alpha=0.1, color='#E53935')

# 为极端点加标签
for i in range(len(df)):
    if abs(snd[i]) > 2 or precision[i] > 5:
        ax.annotate(study_names[i], (precision[i], snd[i]),
                    textcoords="offset points", xytext=(5, 5), fontsize=8)

ax.axhline(y=0, color='gray', linestyle='--', alpha=0.5)
ax.set_xlabel('精度 (1/SE)', fontsize=12)
ax.set_ylabel('标准正态离差 (SND = Effect / SE)', fontsize=12)
ax.set_title("Egger's 回归检验 — 漏斗图对称性检验", fontsize=14, fontweight='bold')
ax.legend(loc='best', fontsize=10)
ax.grid(alpha=0.3)
plt.tight_layout()
plt.savefig(f'{output_dir}/05_egger_test.png', dpi=150, bbox_inches='tight')
plt.close()
print(f"  ✅ Egger 回归图 → {output_dir}/05_egger_test.png")

# ====== 8f. 综合报告图 ======
fig, axes = plt.subplots(2, 2, figsize=(18, 13))

# 左上: 效应量分布直方图
ax1 = axes[0, 0]
ax1.hist(eff, bins=8, color='#1976D2', alpha=0.7, edgecolor='white', linewidth=1.5)
ax1.axvline(x=overall, color='#E53935', linestyle='-', linewidth=2.5, label=f'合并效应量 = {overall:.3f}')
ax1.axvline(x=0, color='gray', linestyle='--', alpha=0.5)
ax1.set_xlabel('效应量 (SMD)', fontsize=11)
ax1.set_ylabel('研究数量', fontsize=11)
ax1.set_title('效应量分布直方图', fontsize=12, fontweight='bold')
ax1.legend(fontsize=9)
ax1.grid(alpha=0.3)

# 右上: 权重分布对比
ax2 = axes[0, 1]
weights_fe = 1 / var_eff
weights_re = 1 / (var_eff + res_re.tau2) if res_re.tau2 > 0 else weights_fe
x_idx = np.arange(len(df))
ax2.bar(x_idx - 0.2, weights_fe / weights_fe.sum() * 100, 0.35,
        color='#1565C0', alpha=0.7, label='固定效应权重')
ax2.bar(x_idx + 0.2, weights_re / weights_re.sum() * 100, 0.35,
        color='#FF9800', alpha=0.7, label='随机效应权重')
ax2.set_xticks(x_idx)
ax2.set_xticklabels(study_names, rotation=60, fontsize=7)
ax2.set_ylabel('权重 (%)', fontsize=11)
ax2.set_title('权重分布 (固定 vs 随机效应)', fontsize=12, fontweight='bold')
ax2.legend(fontsize=9)
ax2.grid(alpha=0.3)

# 左下: 结果摘要文本
ax3 = axes[1, 0]
ax3.axis('off')

summary_lines = [
    "═══ Meta 分析结果摘要 ═══",
    "",
    f"研究数量: {len(df)}",
    f"效应模型: {model_label}",
    "",
    "── 异质性检验 ──",
    f"  I² = {i2:.1%}",
    f"  Q  = {res_re.q:.2f}  (df={res_re.k-1})",
    f"  P  = {p_het:.4f}",
    f"  τ² = {res_re.tau2:.6f}",
    "",
    "── 合并效应量 ──",
    f"  {overall:.4f}  [{overall_low:.4f}, {overall_upp:.4f}]",
    "",
    "── 亚组分析 ──",
]
for g in groups:
    sr = subgroup_results[g]
    summary_lines.append(f"  {g}:")
    summary_lines.append(f"    {sr['effect']:.4f} [{sr['ci_low']:.4f}, {sr['ci_upp']:.4f}]")
    summary_lines.append(f"    I²={sr['i2']:.0%}, k={sr['k']}")

summary_lines += [
    f"  亚组间: Q_b={q_between:.2f}, P={p_between:.4f}",
    "",
    "── 发表偏倚 ──",
    f"  Egger's 截距 P={p_egger[0]:.4f}",
    f"  剪补法 k₀={k0}",
    "",
    "── 敏感性分析 ──",
    f"  LOO 范围: [{loo_df['effect'].min():.4f}, {loo_df['effect'].max():.4f}]",
    f"  宽度: {effect_range:.4f}",
]

ax3.text(0.05, 0.95, '\n'.join(summary_lines), transform=ax3.transAxes, fontsize=10,
         verticalalignment='top', fontfamily='monospace',
         bbox=dict(boxstyle='round,pad=0.8', facecolor='#F5F5F5', edgecolor='#BDBDBD', alpha=0.9))

# 右下: I² 异质性对比
ax4 = axes[1, 1]
sub_labels = list(groups)
sub_i2_vals = [subgroup_results[g]['i2'] for g in groups]
bar_colors = [colors_group[g] for g in groups]

bars = ax4.barh(sub_labels + ['总体'], sub_i2_vals + [i2],
                color=bar_colors + [color_overall],
                alpha=0.7, edgecolor='white', linewidth=1.5, height=0.5)
ax4.axvline(x=0.25, color='orange', linestyle=':', alpha=0.7, linewidth=1.5, label='低异质性 (25%)')
ax4.axvline(x=0.50, color='red', linestyle=':', alpha=0.7, linewidth=1.5, label='中异质性 (50%)')
ax4.axvline(x=0.75, color='darkred', linestyle=':', alpha=0.7, linewidth=1.5, label='高异质性 (75%)')
ax4.set_xlabel('I² 异质性指数', fontsize=11)
ax4.set_title('异质性对比 (I²)', fontsize=12, fontweight='bold')
ax4.legend(fontsize=9)
ax4.grid(axis='x', alpha=0.3)
for i, (v, bar) in enumerate(zip(sub_i2_vals + [i2], bars)):
    ax4.text(v + 0.01, bar.get_y() + bar.get_height()/2, f'{v:.0%}',
             va='center', fontsize=11, fontweight='bold')

plt.suptitle('连续型变量 Meta 分析 — 综合报告', fontsize=17, fontweight='bold', y=1.01)
plt.tight_layout()
plt.savefig(f'{output_dir}/06_summary_report.png', dpi=150, bbox_inches='tight')
plt.close()
print(f"  ✅ 综合报告图 → {output_dir}/06_summary_report.png")

# ============================================================
# 9. 输出总结
# ============================================================
print("\n" + "=" * 72)
print("  📋 分析完成 — 输出文件列表")
print("=" * 72)
for f in sorted(os.listdir(output_dir)):
    size = os.path.getsize(f'{output_dir}/{f}')
    print(f"    📄 {f}  ({size/1024:.1f} KB)")

print(f"\n{'='*72}")
print("  ✅ 连续型变量 Meta 分析完整流程执行完毕！")
print(f"{'='*72}")