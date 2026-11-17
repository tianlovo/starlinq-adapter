//! 零外网依赖的离线交互式测试与基准可视化 HTML 报告生成器

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::constants::{REPORT_HTML_FILE, REPORT_JSON_FILE};
use crate::models::QaFullReport;

/// 将完整测试报告输出为 JSON 数据文件与零依赖交互式 HTML 仪表盘
pub fn write_qa_reports(
    out_dir: &Path,
    report: &QaFullReport,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("创建报告输出目录失败: {}", out_dir.display()))?;

    let json_path = out_dir.join(REPORT_JSON_FILE);
    let html_path = out_dir.join(REPORT_HTML_FILE);

    let pretty_json = serde_json::to_string_pretty(report).context("序列化测试报告 JSON 失败")?;
    fs::write(&json_path, pretty_json)
        .with_context(|| format!("写入 JSON 报告失败: {}", json_path.display()))?;

    let embedded_json = serde_json::to_string(report)
        .context("序列化内嵌报告 JSON 失败")?
        .replace("</", "<\\/");
    let html_content = build_offline_html(&embedded_json, report.overall_passed);
    fs::write(&html_path, html_content)
        .with_context(|| format!("写入 HTML 报告失败: {}", html_path.display()))?;

    Ok((json_path, html_path))
}

fn build_offline_html(embedded_json: &str, overall_passed: bool) -> String {
    let title_state = if overall_passed { "PASS" } else { "FAIL" };
    let head_and_styles = format!(
        r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>QQNT-UIA-Adapter QA & Benchmark Dashboard [{title_state}]</title>
<style>
:root {{
  --bg: #0b0f17;
  --bg-elev: #111827;
  --card: #151e2e;
  --card-hover: #1b263b;
  --border: #26334d;
  --text: #e6edf7;
  --muted: #8ea0bd;
  --accent: #38bdf8;
  --accent-2: #818cf8;
  --pass: #22c55e;
  --pass-bg: rgba(34, 197, 94, 0.14);
  --warn: #f59e0b;
  --warn-bg: rgba(245, 158, 11, 0.14);
  --fail: #ef4444;
  --fail-bg: rgba(239, 68, 68, 0.16);
  --mono: "Cascadia Mono", "JetBrains Mono", Consolas, monospace;
  --sans: "Microsoft YaHei UI", "PingFang SC", system-ui, -apple-system, sans-serif;
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0;
  padding: 24px;
  background: radial-gradient(circle at top right, #162238 0%, var(--bg) 48%);
  color: var(--text);
  font-family: var(--sans);
  line-height: 1.5;
}}
.container {{ max-width: 1480px; margin: 0 auto; display: flex; flex-direction: column; gap: 20px; }}
.panel {{
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: 14px;
  padding: 18px 20px;
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.28);
}}
.hero {{
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 16px;
  background: linear-gradient(135deg, rgba(56, 189, 248, 0.12), rgba(129, 140, 248, 0.08)), var(--card);
}}
.hero h1 {{ margin: 0 0 6px; font-size: 22px; font-weight: 700; letter-spacing: 0.2px; }}
.scope-banner {{
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  border-left: 4px solid var(--warn);
  background: rgba(245, 158, 11, 0.08);
}}
.meta-row {{ display: flex; flex-wrap: wrap; gap: 10px; color: var(--muted); font-size: 13px; }}
.pill {{
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 10px;
  border-radius: 999px;
  font-size: 12px;
  font-weight: 600;
  font-family: var(--mono);
  border: 1px solid transparent;
}}
.pill.pass {{ color: var(--pass); background: var(--pass-bg); border-color: rgba(34, 197, 94, 0.35); }}
.pill.fail {{ color: var(--fail); background: var(--fail-bg); border-color: rgba(239, 68, 68, 0.35); }}
.pill.warn {{ color: var(--warn); background: var(--warn-bg); border-color: rgba(245, 158, 11, 0.35); }}
.pill.info {{ color: var(--accent); background: rgba(56, 189, 248, 0.12); border-color: rgba(56, 189, 248, 0.32); }}
.kpi-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr)); gap: 14px; }}
.kpi-card {{
  background: var(--bg-elev);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 14px 16px;
}}
.kpi-label {{ font-size: 12px; color: var(--muted); text-transform: uppercase; letter-spacing: 0.5px; }}
.kpi-value {{ font-size: 24px; font-weight: 700; font-family: var(--mono); margin: 4px 0; }}
.kpi-sub {{ font-size: 12px; color: var(--muted); }}
.section-head {{
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  margin-bottom: 14px;
}}
.section-head h2 {{ margin: 0; font-size: 17px; font-weight: 650; }}
.controls {{ display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }}
button.btn {{
  cursor: pointer;
  background: var(--bg-elev);
  color: var(--muted);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 6px 12px;
  font-size: 12px;
  font-weight: 600;
  transition: all 0.15s ease;
}}
button.btn:hover, button.btn.active {{
  color: var(--text);
  border-color: var(--accent);
  background: rgba(56, 189, 248, 0.14);
}}
input.search {{
  background: var(--bg-elev);
  color: var(--text);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 6px 12px;
  font-size: 13px;
  min-width: 220px;
}}
.cat-list {{ display: flex; flex-direction: column; gap: 10px; }}
.cat-card {{
  background: var(--bg-elev);
  border: 1px solid var(--border);
  border-radius: 11px;
  overflow: hidden;
}}
.cat-summary {{
  display: grid;
  grid-template-columns: 95px 60px minmax(200px, 1.4fr) minmax(180px, 1.2fr) 110px 95px 32px;
  align-items: center;
  gap: 10px;
  padding: 12px 14px;
  cursor: pointer;
}}
.cat-summary:hover {{ background: var(--card-hover); }}
.cat-detail {{ border-top: 1px solid var(--border); padding: 14px; display: none; background: rgba(11, 15, 23, 0.55); }}
.cat-card.open .cat-detail {{ display: block; }}
.metric-chips {{ display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 12px; }}
.chip {{
  font-family: var(--mono);
  font-size: 11px;
  padding: 4px 8px;
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: 6px;
  color: var(--text);
}}
.chart-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(460px, 1fr)); gap: 16px; margin-bottom: 16px; }}
.chart-box {{
  background: var(--bg-elev);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 14px;
}}
.chart-box h3 {{ margin: 0 0 10px; font-size: 14px; color: var(--muted); font-weight: 600; }}
canvas {{ width: 100%; height: 290px; display: block; }}
.table-wrap {{ overflow-x: auto; }}
table {{ width: 100%; border-collapse: collapse; font-size: 12.5px; }}
th, td {{ text-align: left; padding: 9px 10px; border-bottom: 1px solid var(--border); vertical-align: top; }}
th {{ color: var(--muted); font-weight: 600; font-size: 11.5px; text-transform: uppercase; letter-spacing: 0.4px; white-space: nowrap; }}
td.mono {{ font-family: var(--mono); white-space: nowrap; }}
tr.bench-row {{ cursor: pointer; }}
tr.bench-row:hover, tr.bench-row.selected {{ background: rgba(56, 189, 248, 0.12); }}
.progress-track {{ width: 100%; height: 7px; border-radius: 999px; background: #1e293b; overflow: hidden; }}
.progress-fill {{ height: 100%; background: linear-gradient(90deg, var(--accent), var(--pass)); }}
@media (max-width: 960px) {{
  .cat-summary {{ grid-template-columns: 1fr 1fr; }}
  .chart-grid {{ grid-template-columns: 1fr; }}
}}
</style>
</head>
<body>
<div class="container">
  <section class="panel hero">
    <div>
      <h1>QQNT-UIA-Adapter 质量验证与行业基准仪表盘</h1>
      <div class="meta-row" id="heroMeta"></div>
    </div>
    <div id="heroBadge"></div>
  </section>

  <section class="panel scope-banner" id="scopeBanner"></section>

  <section class="kpi-grid" id="kpiGrid"></section>

  <section class="panel">
    <div class="section-head">
      <h2>宿主机与进程实时资源遥测</h2>
      <span class="pill info" id="daemonStatusPill">检查中</span>
    </div>
    <div class="table-wrap" id="telemetryWrap"></div>
  </section>

  <section class="panel">
    <div class="section-head">
      <h2>分级专项测试矩阵 (L0–L4 · 12 大专项)</h2>
      <div class="controls">
        <div id="tierFilters" class="controls"></div>
        <input id="caseSearch" class="search" type="search" placeholder="筛选类别或用例名称">
      </div>
    </div>
    <div class="cat-list" id="categoryList"></div>
  </section>

  <section class="panel">
    <div class="section-head">
      <h2>行业标准统计基准测试 (Criterion / Google Benchmark 方法论)</h2>
      <div class="controls">
        <button class="btn active" data-chart-mode="ops" id="btnOpsMode">吞吐量 (ops/sec)</button>
        <button class="btn" data-chart-mode="latency" id="btnLatencyMode">均值与 95% Bootstrap CI</button>
      </div>
    </div>
    <div class="chart-grid">
      <div class="chart-box">
        <h3 id="mainChartTitle">各基准项吞吐量对比 (ops/sec)</h3>
        <canvas id="benchBarCanvas"></canvas>
      </div>
      <div class="chart-box">
        <h3 id="sampleChartTitle">选中项 40 样本均值分布与百分位 (点击下方表格行切换)</h3>
        <canvas id="sampleDistCanvas"></canvas>
      </div>
    </div>
    <div class="table-wrap" id="benchTableWrap"></div>
  </section>

  <section class="panel">
    <div class="section-head">
      <h2>缺陷发现与闭环台账 (QA-BUG &amp; LIVE-BUG Audit Log)</h2>
      <span class="pill info" id="defectCountPill"></span>
    </div>
    <div class="table-wrap" id="defectTableWrap"></div>
  </section>
</div>
<script id="qa-data" type="application/json">"##
    );

    let script_and_tail = r##"</script>
<script>
(() => {
  const report = JSON.parse(document.getElementById("qa-data").textContent);
  let activeTier = "ALL";
  let searchTerm = "";
  let chartMode = "ops";
  let selectedBenchIdx = 0;

  const esc = (v) => String(v ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

  const fmtNs = (ns) => {
    if (ns >= 1e6) return `${(ns / 1e6).toFixed(2)} ms`;
    if (ns >= 1e3) return `${(ns / 1e3).toFixed(2)} µs`;
    return `${Number(ns).toFixed(1)} ns`;
  };

  const fmtOps = (ops) => {
    if (ops >= 1e6) return `${(ops / 1e6).toFixed(2)} M/s`;
    if (ops >= 1e3) return `${(ops / 1e3).toFixed(1)} K/s`;
    return `${Number(ops).toFixed(1)} /s`;
  };

  function renderHeaderAndKpis() {
    const env = report.environment;
    document.getElementById("heroMeta").innerHTML = [
      `<span>层级: <b>${esc(env.tier_requested)}</b></span>`,
      `<span>构建: <b>${esc(env.build_profile)}</b></span>`,
      `<span>逻辑核心: <b>${env.logical_cores}C</b></span>`,
      `<span>物理内存: <b>${env.avail_phys_memory_mib.toFixed(0)} / ${env.total_phys_memory_mib.toFixed(0)} MiB</b></span>`,
      `<span>Harness PID: <b>${env.harness_pid}</b></span>`,
      `<span>生成时间: <b>${esc(env.generated_at_rfc3339)}</b></span>`,
      `<span>验证范围: <b>${esc(report.verification_scope || "OFFLINE_MOCK_AND_TREE_REPLAY (NOT_LIVE_QQ_E2E)")}</b></span>`
    ].join(" · ");

    const liveVerified = Boolean(report.live_e2e_verified);
    const scopeText = report.verification_scope || "OFFLINE_MOCK_AND_TREE_REPLAY (NOT_LIVE_QQ_E2E)";
    document.getElementById("scopeBanner").innerHTML = `
      <div>
        <div style="font-weight:700;color:var(--warn);">验证范围声明 · ${esc(scopeText)} (live_e2e_verified=${liveVerified})</div>
        <div style="font-size:12.5px;color:var(--muted);margin-top:3px;">
          本报告由 qq-qa-harness 自动生成，仅覆盖协议契约、并发互斥、SQLite 持久化与 Win32 Mock / 离线 UI 树回放验证，<strong>不能替代真实 QQNT 桌面客户端 (QQ.exe) 端到端实机验收</strong>。
        </div>
      </div>
      <span class="pill ${liveVerified ? "pass" : "warn"}">${liveVerified ? "LIVE_E2E_VERIFIED" : "OFFLINE_ONLY · NOT_LIVE_QQ_E2E"}</span>
    `;

    document.getElementById("heroBadge").innerHTML = report.overall_passed
      ? `<span class="pill pass">OVERALL PASS · ${(report.total_duration_ms / 1000).toFixed(2)}s</span>`
      : `<span class="pill fail">OVERALL FAIL · ${(report.total_duration_ms / 1000).toFixed(2)}s</span>`;

    const passRate = report.total_cases > 0
      ? ((report.passed_cases / report.total_cases) * 100).toFixed(2)
      : "0.00";
    const daemonPill = document.getElementById("daemonStatusPill");
    if (env.live_daemon_detected) {
      daemonPill.className = "pill pass";
      daemonPill.textContent = `实况守护进程在线 · PID ${env.live_daemon_pid ?? "-"}`;
    } else {
      daemonPill.className = "pill warn";
      daemonPill.textContent = "未检测到实况守护进程 (离线模式)";
    }

    const snaps = Array.isArray(env.live_telemetry)
      ? env.live_telemetry
      : (env.live_telemetry ? [env.live_telemetry] : []);
    const latestHarness = snaps.find((t) => t.pid === env.harness_pid) || null;
    const liveSnap = env.live_daemon_pid
      ? snaps.find((t) => t.pid === env.live_daemon_pid) || snaps[0] || null
      : snaps[0] || null;

    const skippedCases = Number(report.skipped_cases ?? 0);
    const livePendingCount = (report.defects || []).filter((d) => String(d.status).toLowerCase().includes("pending")).length;
    const unitFixedCount = (report.defects || []).length - livePendingCount;

    const kpis = [
      {
        label: "总体通过率 (不含跳过)",
        value: `${passRate}%`,
        sub: `${report.passed_cases} / ${report.total_cases} 通过 (${skippedCases} 跳过 · ${report.failed_cases} 失败)`
      },
      {
        label: "专项覆盖度",
        value: `${report.passed_categories} / ${report.total_categories}`,
        sub: "L0 冒烟至 L4 混沌 (非实机 E2E)"
      },
      {
        label: "行业标准基准",
        value: `${report.total_benchmarks} 项`,
        sub: "40 样本 · 1000x Bootstrap 95% CI"
      },
      {
        label: "逻辑处理器 / 内存",
        value: `${env.logical_cores} 核`,
        sub: `可用 ${env.avail_phys_memory_mib.toFixed(0)} / ${env.total_phys_memory_mib.toFixed(0)} MiB`
      },
      {
        label: "守护进程常驻内存",
        value: liveSnap ? `${liveSnap.working_set_mib.toFixed(1)} MiB` : "离线",
        sub: liveSnap
          ? `峰值 ${liveSnap.peak_working_set_mib.toFixed(1)} MiB · CPU ${liveSnap.cpu_percent_window == null ? "-" : liveSnap.cpu_percent_window.toFixed(2)}%`
          : "未连接 127.0.0.1:18770"
      },
      {
        label: "缺陷整改与复验台账",
        value: `${report.defects.length} 项`,
        sub: `${unitFixedCount} 项单元/桩回归 · ${livePendingCount} 项待实机复验`
      }
    ];

    document.getElementById("kpiGrid").innerHTML = kpis.map((k) => `
      <div class="kpi-card">
        <div class="kpi-label">${esc(k.label)}</div>
        <div class="kpi-value">${esc(k.value)}</div>
        <div class="kpi-sub">${esc(k.sub)}</div>
      </div>
    `).join("");

    document.getElementById("telemetryWrap").innerHTML = snaps.length === 0
      ? `<p style="color:var(--muted);margin:0;">本次运行未记录进程级遥测样本${latestHarness ? "" : "（L3 类别内含独立内存/CPU 曲线验收）"}。</p>`
      : `<table>
          <thead><tr>
            <th>进程标签</th><th>PID</th><th>工作集 (MiB)</th><th>峰值工作集 (MiB)</th>
            <th>私有提交 (MiB)</th><th>内核态耗时 (ms)</th><th>用户态耗时 (ms)</th><th>窗口 CPU%</th>
          </tr></thead>
          <tbody>
            ${snaps.map((s) => `<tr>
              <td>${esc(s.label)}</td>
              <td class="mono">${s.pid}</td>
              <td class="mono">${s.working_set_mib.toFixed(2)}</td>
              <td class="mono">${s.peak_working_set_mib.toFixed(2)}</td>
              <td class="mono">${s.private_bytes_mib.toFixed(2)}</td>
              <td class="mono">${s.kernel_time_ms.toFixed(1)}</td>
              <td class="mono">${s.user_time_ms.toFixed(1)}</td>
              <td class="mono">${s.cpu_percent_window == null ? "-" : s.cpu_percent_window.toFixed(2) + "%"}</td>
            </tr>`).join("")}
          </tbody>
        </table>`;
  }

  function renderTierFilters() {
    const tiers = ["ALL", "L0", "L1", "L2", "L3", "L4"];
    document.getElementById("tierFilters").innerHTML = tiers.map((t) =>
      `<button class="btn ${activeTier === t ? "active" : ""}" data-tier="${t}">${t === "ALL" ? "全部层级" : t}</button>`
    ).join("");
    document.querySelectorAll("[data-tier]").forEach((btn) => {
      btn.addEventListener("click", () => {
        activeTier = btn.getAttribute("data-tier");
        renderTierFilters();
        renderCategories();
      });
    });
  }

  function renderCategories() {
    const q = searchTerm.trim().toLowerCase();
    const filtered = report.categories.filter((cat) => {
      if (activeTier !== "ALL" && cat.tier !== activeTier) return false;
      if (!q) return true;
      const inHeader = [cat.id, cat.category_zh, cat.category_en, cat.sla_target, cat.sla_actual]
        .some((s) => String(s).toLowerCase().includes(q));
      const inCases = cat.cases.some((c) => c.name.toLowerCase().includes(q) || c.detail.toLowerCase().includes(q));
      return inHeader || inCases;
    });

    const host = document.getElementById("categoryList");
    host.innerHTML = filtered.map((cat) => {
      const pct = cat.total_cases > 0 ? Math.round((cat.passed_cases / cat.total_cases) * 100) : 0;
      const statusPill = cat.passed
        ? `<span class="pill pass">PASS</span>`
        : `<span class="pill fail">FAIL</span>`;
      const metricChips = Object.entries(cat.key_metrics || {})
        .map(([k, v]) => `<span class="chip">${esc(k)}: ${esc(v)}</span>`)
        .join("");

      const caseRows = cat.cases
        .filter((c) => !q || c.name.toLowerCase().includes(q) || c.detail.toLowerCase().includes(q) || cat.category_zh.toLowerCase().includes(q))
        .map((c) => {
          const st = c.skipped
            ? `<span class="pill warn">SKIP</span>`
            : c.passed ? `<span class="pill pass">PASS</span>` : `<span class="pill fail">FAIL</span>`;
          return `<tr>
            <td class="mono">${esc(c.name)}</td>
            <td>${st}</td>
            <td class="mono">${c.duration_ms.toFixed(2)} ms</td>
            <td>${esc(c.detail)}</td>
          </tr>`;
        }).join("");

      return `<div class="cat-card ${cat.passed ? "" : "open"}" data-cat-id="${esc(cat.id)}">
        <div class="cat-summary">
          <div><span class="pill info">${esc(cat.id)}</span></div>
          <div><span class="pill info">${esc(cat.tier)}</span></div>
          <div>
            <div style="font-weight:600;">${esc(cat.category_zh)}</div>
            <div style="font-size:12px;color:var(--muted);">${esc(cat.category_en)}</div>
          </div>
          <div>
            <div style="font-size:12px;">SLA: ${esc(cat.sla_actual)}</div>
            <div style="font-size:11px;color:var(--muted);">目标: ${esc(cat.sla_target)}</div>
          </div>
          <div>
            <div class="mono" style="font-size:12px;margin-bottom:4px;">${cat.passed_cases}/${cat.total_cases}${cat.skipped_cases ? ` (+${cat.skipped_cases}跳过)` : ""} (${pct}%)</div>
            <div class="progress-track"><div class="progress-fill" style="width:${pct}%"></div></div>
          </div>
          <div>${statusPill} <span class="mono" style="font-size:11px;color:var(--muted);margin-left:4px;">${cat.duration_ms.toFixed(0)}ms</span></div>
          <div style="text-align:right;color:var(--muted);">▾</div>
        </div>
        <div class="cat-detail">
          ${metricChips ? `<div class="metric-chips">${metricChips}</div>` : ""}
          <table>
            <thead><tr><th>用例名称</th><th>状态</th><th>耗时</th><th>验证详情</th></tr></thead>
            <tbody>${caseRows}</tbody>
          </table>
        </div>
      </div>`;
    }).join("");

    host.querySelectorAll(".cat-summary").forEach((el) => {
      el.addEventListener("click", () => el.parentElement.classList.toggle("open"));
    });
  }

  function renderBenchmarkTable() {
    const benches = report.benchmarks || [];
    document.getElementById("benchTableWrap").innerHTML = `<table>
      <thead><tr>
        <th>ID</th><th>基准测试项</th><th>子系统</th><th>样本×每轮</th>
        <th>均值 (95% CI)</th><th>中位数 / MAD</th><th>RME%</th>
        <th>离群值 L/H</th><th>P95 / P99 / P99.9</th><th>吞吐 ops/sec</th><th>带宽 MiB/s</th>
      </tr></thead>
      <tbody>
        ${benches.map((b, i) => `<tr class="bench-row ${i === selectedBenchIdx ? "selected" : ""}" data-bidx="${i}">
          <td class="mono">${esc(b.id)}</td>
          <td><div><b>${esc(b.title_zh)}</b></div><div style="font-size:11px;color:var(--muted);">${esc(b.title_en)}</div></td>
          <td><span class="pill info">${esc(b.subsystem)}</span></td>
          <td class="mono">${b.samples} × ${b.iters_per_sample}</td>
          <td class="mono">${fmtNs(b.mean_ns)}<br><span style="color:var(--muted);font-size:11px;">[${fmtNs(b.ci95_low_ns)}, ${fmtNs(b.ci95_high_ns)}]</span></td>
          <td class="mono">${fmtNs(b.median_ns)} / ${fmtNs(b.mad_ns)}</td>
          <td class="mono">${b.rme_percent.toFixed(2)}%</td>
          <td class="mono">${b.outliers_low} / ${b.outliers_high}</td>
          <td class="mono">${fmtNs(b.percentiles.p95_ns)} / ${fmtNs(b.percentiles.p99_ns)} / ${fmtNs(b.percentiles.p999_ns)}</td>
          <td class="mono"><b>${fmtOps(b.ops_per_sec)}</b></td>
          <td class="mono">${b.mib_per_sec != null && b.mib_per_sec > 0 ? b.mib_per_sec.toFixed(2) : "-"}</td>
        </tr>`).join("")}
      </tbody>
    </table>`;

    document.querySelectorAll("tr.bench-row").forEach((row) => {
      row.addEventListener("click", () => {
        selectedBenchIdx = Number(row.getAttribute("data-bidx"));
        renderBenchmarkTable();
        drawCharts();
      });
    });
  }

  function setupCanvas(canvas) {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    const w = Math.max(320, rect.width);
    const h = Math.max(240, rect.height);
    canvas.width = Math.floor(w * dpr);
    canvas.height = Math.floor(h * dpr);
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    return { ctx, w, h };
  }

  function drawBarChart() {
    const canvas = document.getElementById("benchBarCanvas");
    const { ctx, w, h } = setupCanvas(canvas);
    const benches = report.benchmarks || [];
    if (!benches.length) return;

    const isOps = chartMode === "ops";
    document.getElementById("mainChartTitle").textContent = isOps
      ? "各基准项吞吐量对比 (ops/sec · 对数刻度)"
      : "各基准项均值延迟与 95% Bootstrap CI (ns · 对数刻度)";

    const padL = 78;
    const padR = 88;
    const padT = 14;
    const padB = 22;
    const innerW = w - padL - padR;
    const rowH = (h - padT - padB) / benches.length;
    const values = benches.map((b) => Math.max(1, isOps ? b.ops_per_sec : b.ci95_high_ns));
    const maxLog = Math.log10(Math.max(...values, 10));

    ctx.font = "11px Consolas, monospace";
    benches.forEach((b, idx) => {
      const y = padT + idx * rowH;
      const barH = Math.max(10, rowH * 0.56);
      const val = Math.max(1, isOps ? b.ops_per_sec : b.mean_ns);
      const frac = Math.min(1, Math.log10(val + 1) / Math.max(1, maxLog));
      const barW = Math.max(4, innerW * frac);

      ctx.fillStyle = idx === selectedBenchIdx ? "#38bdf8" : "rgba(56, 189, 248, 0.55)";
      ctx.fillRect(padL, y + (rowH - barH) / 2, barW, barH);

      if (!isOps) {
        const loFrac = Math.min(1, Math.log10(Math.max(1, b.ci95_low_ns) + 1) / Math.max(1, maxLog));
        const hiFrac = Math.min(1, Math.log10(Math.max(1, b.ci95_high_ns) + 1) / Math.max(1, maxLog));
        const midY = y + rowH / 2;
        ctx.strokeStyle = "#f59e0b";
        ctx.lineWidth = 1.6;
        ctx.beginPath();
        ctx.moveTo(padL + innerW * loFrac, midY);
        ctx.lineTo(padL + innerW * hiFrac, midY);
        ctx.stroke();
      }

      ctx.fillStyle = idx === selectedBenchIdx ? "#ffffff" : "#8ea0bd";
      ctx.textAlign = "right";
      ctx.textBaseline = "middle";
      ctx.fillText(b.id, padL - 8, y + rowH / 2);

      ctx.textAlign = "left";
      ctx.fillStyle = "#e6edf7";
      ctx.fillText(isOps ? fmtOps(b.ops_per_sec) : fmtNs(b.mean_ns), padL + barW + 8, y + rowH / 2);
    });
  }

  function drawSampleChart() {
    const canvas = document.getElementById("sampleDistCanvas");
    const { ctx, w, h } = setupCanvas(canvas);
    const benches = report.benchmarks || [];
    const bench = benches[selectedBenchIdx];
    if (!bench || !bench.sample_means_ns.length) return;

    document.getElementById("sampleChartTitle").textContent =
      `${bench.id} · ${bench.title_zh} (${bench.sample_means_ns.length} 样本均值 & 95% CI 区间)`;

    const samples = bench.sample_means_ns;
    const minVal = Math.min(...samples, bench.ci95_low_ns) * 0.92;
    const maxVal = Math.max(...samples, bench.ci95_high_ns, minVal + 1) * 1.08;
    const padL = 68;
    const padR = 18;
    const padT = 20;
    const padB = 34;
    const innerW = w - padL - padR;
    const innerH = h - padT - padB;
    const yOf = (v) => padT + innerH - ((v - minVal) / (maxVal - minVal)) * innerH;

    const ciTop = yOf(bench.ci95_high_ns);
    const ciBot = yOf(bench.ci95_low_ns);
    ctx.fillStyle = "rgba(34, 197, 94, 0.14)";
    ctx.fillRect(padL, ciTop, innerW, Math.max(2, ciBot - ciTop));

    ctx.strokeStyle = "#22c55e";
    ctx.setLineDash([4, 4]);
    ctx.beginPath();
    ctx.moveTo(padL, yOf(bench.mean_ns));
    ctx.lineTo(padL + innerW, yOf(bench.mean_ns));
    ctx.stroke();
    ctx.setLineDash([]);

    ctx.strokeStyle = "#38bdf8";
    ctx.lineWidth = 1.8;
    ctx.beginPath();
    samples.forEach((v, i) => {
      const x = padL + (samples.length === 1 ? innerW / 2 : (i / (samples.length - 1)) * innerW);
      const y = yOf(v);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.stroke();

    samples.forEach((v, i) => {
      const x = padL + (samples.length === 1 ? innerW / 2 : (i / (samples.length - 1)) * innerW);
      const y = yOf(v);
      ctx.fillStyle = v > bench.percentiles.p95_ns ? "#f59e0b" : "#38bdf8";
      ctx.beginPath();
      ctx.arc(x, y, 2.8, 0, Math.PI * 2);
      ctx.fill();
    });

    ctx.font = "11px Consolas, monospace";
    ctx.fillStyle = "#8ea0bd";
    ctx.textAlign = "right";
    ctx.fillText(fmtNs(maxVal), padL - 6, padT + 4);
    ctx.fillText(fmtNs(bench.mean_ns), padL - 6, yOf(bench.mean_ns) + 3);
    ctx.fillText(fmtNs(minVal), padL - 6, padT + innerH);
    ctx.textAlign = "center";
    ctx.fillText(`样本 #1 .. #${samples.length} (P50=${fmtNs(bench.median_ns)}, P95=${fmtNs(bench.percentiles.p95_ns)}, P99=${fmtNs(bench.percentiles.p99_ns)})`, padL + innerW / 2, h - 10);
  }

  function drawCharts() {
    drawBarChart();
    drawSampleChart();
  }

  function renderDefects() {
    const defs = report.defects || [];
    document.getElementById("defectCountPill").textContent = `共 ${defs.length} 条记录`;
    document.getElementById("defectTableWrap").innerHTML = `<table>
      <thead><tr>
        <th>编号</th><th>严重级</th><th>状态</th><th>归属模块</th><th>问题描述与根因</th><th>修复方案与回归测试</th>
      </tr></thead>
      <tbody>
        ${defs.map((d) => {
          const sevCls = String(d.severity).toUpperCase().includes("HIGH") || String(d.severity).toUpperCase().includes("CRIT")
            ? "fail"
            : String(d.severity).toUpperCase().includes("MED") ? "warn" : "info";
          const statusStr = String(d.status || "");
          const statusCls = statusStr.toLowerCase().includes("pending")
            ? "warn"
            : statusStr.toLowerCase().includes("stub") || statusStr.toLowerCase().includes("regression")
              ? "info"
              : "pass";
          return `<tr>
            <td class="mono"><b>${esc(d.id)}</b></td>
            <td><span class="pill ${sevCls}">${esc(d.severity)}</span></td>
            <td><span class="pill ${statusCls}">${esc(d.status)}</span></td>
            <td class="mono">${esc(d.component)}<br><span style="color:var(--muted);font-size:11px;">${esc(d.category)}</span></td>
            <td><div><b>${esc(d.summary)}</b></div><div style="color:var(--muted);margin-top:3px;">${esc(d.root_cause)}</div></td>
            <td><div>${esc(d.resolution)}</div><div class="mono" style="color:var(--accent);font-size:11px;margin-top:4px;">${esc(d.regression_test)}</div></td>
          </tr>`;
        }).join("")}
      </tbody>
    </table>`;
  }

  document.getElementById("caseSearch").addEventListener("input", (e) => {
    searchTerm = e.target.value;
    renderCategories();
  });

  document.querySelectorAll("[data-chart-mode]").forEach((btn) => {
    btn.addEventListener("click", () => {
      chartMode = btn.getAttribute("data-chart-mode");
      document.querySelectorAll("[data-chart-mode]").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      drawCharts();
    });
  });

  window.addEventListener("resize", drawCharts);

  renderHeaderAndKpis();
  renderTierFilters();
  renderCategories();
  renderBenchmarkTable();
  renderDefects();
  drawCharts();
})();
</script>
</body>
</html>
"##;

    format!("{head_and_styles}{embedded_json}{script_and_tail}")
}
