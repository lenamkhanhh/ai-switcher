import { useCallback, useEffect, useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, BarChart3, Loader2, RefreshCw } from "lucide-react";
import { api } from "./tauri";
import type { DayUsage, ModelUsage, SessionUsage, TokenBreakdown, ToolUsage, UsageReport } from "./types";

const RANGES: { label: string; days: number }[] = [
  { label: "7d", days: 7 },
  { label: "30d", days: 30 },
  { label: "90d", days: 90 },
  { label: "All", days: 0 },
];

export function UsageView() {
  const [report, setReport] = useState<UsageReport | null>(null);
  const [activityReport, setActivityReport] = useState<UsageReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string>("all");
  const [range, setRange] = useState(30);

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const nextReport = await api.getUsage(range);
      setReport(nextReport);
      setActivityReport(range === 0 ? nextReport : await api.getUsage(0));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }, [range]);

  useEffect(() => {
    void load();
  }, [load]);

  const usageTools = useMemo(() => {
    if (!report) return [];
    return [buildAllUsage(report.tools), ...report.tools];
  }, [report]);

  const activityTools = useMemo(() => {
    if (!activityReport) return [];
    return [buildAllUsage(activityReport.tools), ...activityReport.tools];
  }, [activityReport]);

  // The background poller refreshes the cache every 5 minutes → refetch with the current range.
  useEffect(() => {
    const unlisten = listen("usage-changed", () => void load());
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [load]);

  return (
    <section className="panel">
      <div className="panelHead">
        <div className="titleRow">
          <h2>Token Usage</h2>
          <PriceStatus report={report} />
        </div>
        <div className="actions">
          <div className="usageRange" role="group" aria-label="Time range">
            {RANGES.map((r) => (
              <button
                key={r.days}
                className={range === r.days ? "selected" : ""}
                onClick={() => setRange(r.days)}
                disabled={busy}
              >
                {r.label}
              </button>
            ))}
          </div>
          <button onClick={load} disabled={busy}>
            {busy ? <Loader2 className="spin" /> : <RefreshCw />}
            Refresh
          </button>
        </div>
      </div>

      <p className="usageLead">
        Token usage &amp; estimated cost from Claude Code and Codex local logs on this machine,
        totaled per tool across all accounts. Antigravity has no token logs and is not shown.
      </p>

      {error && (
        <div className="drift">
          <AlertTriangle />
          <span>{error}</span>
        </div>
      )}

      {report && (
        <>
          <div className="usageTabs">
            {usageTools.map((tool) => (
              <button
                key={tool.toolId}
                className={tool.toolId === selected ? "selected" : ""}
                onClick={() => setSelected(tool.toolId)}
              >
                {tool.displayName}
                {tool.estimate && <span className="estimateMini">≈ est</span>}
              </button>
            ))}
          </div>
          {(() => {
            const tool = usageTools.find((t) => t.toolId === selected) ?? usageTools[0];
            const activityTool = activityTools.find((item) => item.toolId === tool?.toolId);
            return tool ? (
              <ToolUsageSection
                tool={tool}
                activityDaily={activityTool?.daily ?? tool.daily}
                range={range}
                priceUnavailable={report.priceStatus === "unavailable"}
              />
            ) : null;
          })()}
        </>
      )}

      {!report && !error && (
        <div className="empty">
          <Loader2 className="spin" />
          <span>Reading usage logs…</span>
        </div>
      )}
    </section>
  );
}

function PriceStatus({ report }: { report: UsageReport | null }) {
  if (!report) return null;
  if (report.priceStatus === "unavailable") {
    return <span className="badge warn" title="LiteLLM prices could not be loaded">Cost hidden</span>;
  }
  if (report.priceStatus === "cached") {
    const when = report.priceUpdatedAt ? formatDate(report.priceUpdatedAt) : "earlier";
    return <span className="badge muted" title={`Using saved LiteLLM prices from ${when}`}>Saved prices</span>;
  }
  return <span className="badge ok" title="LiteLLM prices loaded">Live prices</span>;
}

function ToolUsageSection({
  tool,
  activityDaily,
  range,
  priceUnavailable,
}: {
  tool: ToolUsage;
  activityDaily: DayUsage[];
  range: number;
  priceUnavailable: boolean;
}) {
  const empty = total(tool.total) === 0;
  return (
    <div className="usageToolBody">
      {empty ? (
        <div className="usageEmpty">
          <BarChart3 />
          <span>No usage yet. Use {tool.displayName} through the app to start tracking.</span>
        </div>
      ) : (
        <>
          <UsageInsights daily={tool.daily} range={range} />

          <div className="usageStats">
            <StatTile label="Total cost" value={formatUsd(tool.totalCostUsd)} sub={`${formatTokens(total(tool.total))} tokens`} big />
            <StatTile label="Today" value={formatUsd(tool.todayCostUsd)} sub={`${formatTokens(total(tool.today))} tokens`} />
            <StatTile label="Output" value={formatTokens(tool.total.output)} sub="generated tokens" />
            <StatTile label="Cache read" value={formatTokens(tool.total.cacheRead)} sub="reused tokens" />
          </div>

          <TokenActivity daily={activityDaily} priceUnavailable={priceUnavailable} />
          <ModelTable models={tool.byModel} />
          <SessionTable sessions={tool.sessions} />
        </>
      )}
    </div>
  );
}

function StatTile({ label, value, sub, big }: { label: string; value: string; sub: string; big?: boolean }) {
  return (
    <div className={`statTile ${big ? "big" : ""}`}>
      <span className="statLabel">{label}</span>
      <strong className="statValue">{value}</strong>
      <span className="statSub">{sub}</span>
    </div>
  );
}

function UsageInsights({ daily, range }: { daily: DayUsage[]; range: number }) {
  const calendar = continuousDays(daily, range);
  const active = calendar.filter((day) => total(day.tokens) > 0);
  const peak = active.reduce<DayUsage | null>(
    (best, day) => (!best || total(day.tokens) > total(best.tokens) ? day : best),
    null,
  );
  // Streaks are a property of the whole history, not the selected range: computing them on the
  // range-limited calendar caps the longest streak at the window length (e.g. 7) and truncates the
  // current streak. Peak/token totals below stay range-scoped, which is correct.
  const streaks = calculateStreaks(continuousDays(daily, 0));
  const rangeLabel = range === 0 ? "all time" : `${range}d`;

  return (
    <div className="usageInsights" aria-label="Usage highlights">
      <Insight value={formatTokens(calendar.reduce((sum, day) => sum + total(day.tokens), 0))} label={`Tokens · ${rangeLabel}`} />
      <Insight value={peak ? formatTokens(total(peak.tokens)) : "0"} label="Peak day" />
      <Insight value={`${active.length}`} label="Active days" />
      <Insight value={`${streaks.current}d`} label="Current streak" />
      <Insight value={`${streaks.longest}d`} label="Longest streak" />
    </div>
  );
}

function Insight({ value, label }: { value: string; label: string }) {
  return (
    <div className="usageInsight">
      <strong>{value}</strong>
      <span>{label}</span>
    </div>
  );
}

/** Simple inline SVG bar chart of the last CHART_DAYS days (cost when priced, else tokens). */
// Totals/tables follow the selected range, but the bar chart stays readable by showing at most
// the 45 most recent days (so 90d / all time don't render hundreds of slivers).
const CHART_MAX_BARS = 45;

function TrendChart({
  daily,
  range,
  priceUnavailable,
}: {
  daily: DayUsage[];
  range: number;
  priceUnavailable: boolean;
}) {
  const [hover, setHover] = useState<number | null>(null);

  // Build a continuous calendar window ending today so days with no usage still show as a 0 bar
  // (otherwise "7d" would render fewer bars than 7 when some days went unused).
  const windowLen = range > 0 ? Math.min(range, CHART_MAX_BARS) : CHART_MAX_BARS;
  const byDate = new Map(daily.map((d) => [d.date, d] as const));
  const today = localToday();
  const days: DayUsage[] = Array.from({ length: windowLen }, (_, i) => {
    const date = addDays(today, -(windowLen - 1 - i));
    return byDate.get(date) ?? { date, tokens: { input: 0, output: 0, cacheRead: 0, cacheCreation: 0 }, costUsd: null };
  });
  if (days.length === 0) return null;

  const useCost = !priceUnavailable && days.some((d) => d.costUsd != null);
  const valueOf = (d: DayUsage) => (useCost ? d.costUsd ?? 0 : total(d.tokens));
  const max = Math.max(...days.map(valueOf), 1);

  const width = 100;
  const height = 36;
  const gap = 1.5;
  const barW = (width - gap * (days.length - 1)) / days.length;

  const active = hover != null ? days[hover] : null;

  return (
    <div className="usageChart">
      <div className="usageChartHead">
        <span>{useCost ? "Daily cost" : "Daily tokens"} · last {days.length} days</span>
        {active ? (
          <span className="usageChartReadout">
            <strong>{active.date}</strong>
            {" · "}
            {formatTokens(total(active.tokens))} tokens
            {" · "}
            {formatUsd(active.costUsd)}
          </span>
        ) : (
          <span className="usageChartHint">hover a bar for the day</span>
        )}
      </div>
      <svg
        viewBox={`0 0 ${width} ${height}`}
        preserveAspectRatio="none"
        className="usageChartSvg"
        role="img"
        onMouseLeave={() => setHover(null)}
      >
        {days.map((d, i) => {
          const v = valueOf(d);
          const h = Math.max((v / max) * height, v > 0 ? 0.6 : 0);
          const x = i * (barW + gap);
          const label = `${d.date} · ${formatTokens(total(d.tokens))} tokens · ${formatUsd(d.costUsd)}`;
          return (
            <g key={d.date} onMouseEnter={() => setHover(i)}>
              {/* full-height hit area so thin bars are still easy to hover */}
              <rect x={x} y={0} width={barW + gap} height={height} fill="transparent" />
              <rect
                x={x}
                y={height - h}
                width={barW}
                height={h}
                rx={0.5}
                className={`usageBar ${hover === i ? "active" : ""}`}
              >
                <title>{label}</title>
              </rect>
            </g>
          );
        })}
      </svg>
      <div className="usageChartAxis">
        <span>{days[0].date.slice(5)}</span>
        <span>{days[days.length - 1].date.slice(5)}</span>
      </div>
    </div>
  );
}

type ActivityMode = "daily" | "weekly";

interface ActivityHover {
  detail: string;
  x: number;
}

function TokenActivity({
  daily,
  priceUnavailable,
}: {
  daily: DayUsage[];
  priceUnavailable: boolean;
}) {
  const [mode, setMode] = useState<ActivityMode>("daily");
  const [hover, setHover] = useState<ActivityHover | null>(null);
  const days = activityYear(daily);
  const cells = mode === "weekly" ? groupByWeek(days) : days;
  const values = cells.map((day) => total(day.tokens));
  const positiveValues = values.filter((value) => value > 0).sort((a, b) => a - b);
  const months = activityMonths(days);

  return (
    <section className="tokenActivity">
      <div className="tokenActivityHead">
        <strong>Token activity</strong>
        <div className="activityModes" role="group" aria-label="Activity grouping">
          {(["daily", "weekly"] as ActivityMode[]).map((item) => (
            <button
              key={item}
              className={mode === item ? "selected" : ""}
              onClick={() => {
                setHover(null);
                setMode(item);
              }}
            >
              {item[0].toUpperCase() + item.slice(1)}
            </button>
          ))}
        </div>
      </div>

      <div className="activityCalendar" onMouseLeave={() => setHover(null)}>
        {hover && (
          <div className="activityTooltip" style={{ "--activity-x": `${hover.x}%` } as CSSProperties}>
            {hover.detail}
          </div>
        )}
        <div
          className={`activityGrid ${mode === "weekly" ? "weekly" : ""}`}
          role="group"
          aria-label={`${mode} token activity`}
        >
          {cells.map((day, index) => {
            if (mode === "daily" && day.date > localToday()) {
              return <span key={`${mode}-${day.date}`} className="activityCell future" aria-hidden="true" />;
            }
            const tokenCount = total(day.tokens);
            const value = values[index];
            const level = activityLevel(value, positiveValues);
            const period = mode === "weekly" ? `Week of ${day.date}` : day.date;
            const detail = `${formatTokens(tokenCount)} tokens on ${formatActivityTooltipDate(day.date, mode)}`;
            const column = mode === "weekly" ? index : Math.floor(index / 7);
            const hoverValue = { detail, x: ((column + 0.5) / 53) * 100 };
            const breakdown = `Input ${formatTokens(day.tokens.input)} · Output ${formatTokens(day.tokens.output)} · Cache ${formatTokens(day.tokens.cacheRead + day.tokens.cacheCreation)}`;
            const cost = !priceUnavailable && day.costUsd != null ? ` · ${formatUsd(day.costUsd)}` : "";
            return (
              <button
                type="button"
                key={`${mode}-${day.date}`}
                className="activityCell"
                data-level={level}
                aria-label={`${period}: ${formatTokens(tokenCount)} tokens${cost}. ${breakdown}`}
                onBlur={() => setHover(null)}
                onFocus={() => setHover(hoverValue)}
                onMouseEnter={() => setHover(hoverValue)}
              />
            );
          })}
        </div>
        <div className="activityMonths" aria-hidden="true">
          {months.map((month) => (
            <span key={`${month.label}-${month.column}`} style={{ gridColumn: month.column }}>{month.label}</span>
          ))}
        </div>
      </div>
    </section>
  );
}

function ModelTable({ models }: { models: ModelUsage[] }) {
  if (models.length === 0) return null;
  return (
    <div className="usageTable">
      <div className="usageTableHead">By model</div>
      <table>
        <thead>
          <tr>
            <th>Model</th>
            <th className="num">Tokens</th>
            <th className="num">Cost</th>
          </tr>
        </thead>
        <tbody>
          {models.map((m) => (
            <tr key={m.model}>
              <td><code>{m.model}</code></td>
              <td className="num">{formatTokens(total(m.tokens))}</td>
              <td className="num">{formatUsd(m.costUsd)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function SessionTable({ sessions }: { sessions: SessionUsage[] }) {
  if (sessions.length === 0) return null;
  return (
    <div className="usageTable">
      <div className="usageTableHead">Recent sessions</div>
      <table>
        <thead>
          <tr>
            <th>Date</th>
            <th>Session</th>
            <th>Model</th>
            <th className="num">Tokens</th>
            <th className="num">Cost</th>
          </tr>
        </thead>
        <tbody>
          {sessions.map((s) => (
            <tr key={s.id + s.date}>
              <td>{s.date}</td>
              <td><code>{s.id.slice(0, 8)}</code></td>
              <td><code>{s.model}</code></td>
              <td className="num">{formatTokens(total(s.tokens))}</td>
              <td className="num">{formatUsd(s.costUsd)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// --- helpers ---

function buildAllUsage(tools: ToolUsage[]): ToolUsage {
  const daily = mergeByDate(tools.flatMap((tool) => tool.daily));
  const byModel = mergeByModel(tools.flatMap((tool) => tool.byModel));
  const sessions = tools
    .flatMap((tool) =>
      tool.sessions.map((session) => ({
        ...session,
        id: `${tool.toolId}:${session.id}`,
        model: `${tool.displayName} / ${session.model}`,
      })),
    )
    .sort((a, b) => b.date.localeCompare(a.date))
    .slice(0, 20);

  return {
    toolId: "all",
    displayName: "All",
    estimate: tools.some((tool) => tool.estimate),
    total: sumTokens(tools.map((tool) => tool.total)),
    totalCostUsd: sumNullable(tools.map((tool) => tool.totalCostUsd)),
    today: sumTokens(tools.map((tool) => tool.today)),
    todayCostUsd: sumNullable(tools.map((tool) => tool.todayCostUsd)),
    daily,
    byModel,
    sessions,
  };
}

function mergeByDate(days: DayUsage[]): DayUsage[] {
  const byDate = new Map<string, { tokens: TokenBreakdown; costs: (number | null)[] }>();
  for (const day of days) {
    const current = byDate.get(day.date) ?? { tokens: zeroTokens(), costs: [] };
    current.tokens = addTokens(current.tokens, day.tokens);
    current.costs.push(day.costUsd);
    byDate.set(day.date, current);
  }
  return Array.from(byDate.entries())
    .map(([date, item]) => ({
      date,
      tokens: item.tokens,
      costUsd: sumNullable(item.costs),
    }))
    .sort((a, b) => a.date.localeCompare(b.date));
}

function continuousDays(daily: DayUsage[], range: number): DayUsage[] {
  const byDate = new Map(daily.map((day) => [day.date, day] as const));
  const today = localToday();
  const earliest = daily[0]?.date ?? today;
  const allTimeDays = Math.max(1, dayDistance(earliest, today) + 1);
  const windowLength = range > 0 ? range : allTimeDays;

  return Array.from({ length: windowLength }, (_, index) => {
    const date = addDays(today, -(windowLength - 1 - index));
    return byDate.get(date) ?? { date, tokens: zeroTokens(), costUsd: null };
  });
}

function groupByWeek(days: DayUsage[]): DayUsage[] {
  const weeks = new Map<string, DayUsage[]>();
  for (const day of days) {
    const week = startOfWeek(day.date);
    weeks.set(week, [...(weeks.get(week) ?? []), day]);
  }
  return Array.from(weeks.entries()).map(([date, entries]) => ({
    date,
    tokens: sumTokens(entries.map((entry) => entry.tokens)),
    costUsd: sumNullable(entries.map((entry) => entry.costUsd)),
  }));
}

function activityYear(daily: DayUsage[]): DayUsage[] {
  const byDate = new Map(daily.map((day) => [day.date, day] as const));
  const currentWeek = startOfWeek(localToday());
  const firstDay = addDays(currentWeek, -52 * 7);
  return Array.from({ length: 53 * 7 }, (_, index) => {
    const date = addDays(firstDay, index);
    return byDate.get(date) ?? { date, tokens: zeroTokens(), costUsd: null };
  });
}

function activityMonths(days: DayUsage[]) {
  const labels: { column: number; label: string }[] = [];
  for (let index = 0; index < days.length; index += 1) {
    const date = new Date(`${days[index].date}T00:00:00`);
    if (index !== 0 && date.getDate() !== 1) continue;
    const column = Math.floor(index / 7) + 1;
    if (labels.some((item) => item.column === column)) continue;
    labels.push({
      column,
      label: new Intl.DateTimeFormat("en-US", { month: "short" }).format(date),
    });
  }
  if (labels.length > 1 && labels[0].label === labels[labels.length - 1].label) {
    labels.shift();
  }
  return labels;
}

function activityLevel(value: number, sortedPositive: number[]) {
  if (value <= 0 || sortedPositive.length === 0) return 0;
  const quantile = (fraction: number) => sortedPositive[Math.min(sortedPositive.length - 1, Math.floor((sortedPositive.length - 1) * fraction))];
  if (value <= quantile(0.25)) return 1;
  if (value <= quantile(0.5)) return 2;
  if (value <= quantile(0.75)) return 3;
  return 4;
}

function formatActivityTooltipDate(date: string, mode: ActivityMode) {
  const formatted = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric" }).format(new Date(`${date}T00:00:00`));
  return mode === "weekly" ? `week of ${formatted}` : formatted;
}

function calculateStreaks(days: DayUsage[]) {
  let current = 0;
  let longest = 0;
  let running = 0;
  for (const day of days) {
    if (total(day.tokens) > 0) {
      running += 1;
      longest = Math.max(longest, running);
    } else {
      running = 0;
    }
  }
  current = running;
  return { current, longest };
}

function startOfWeek(date: string) {
  const value = new Date(`${date}T00:00:00`);
  const offset = (value.getDay() + 6) % 7;
  value.setDate(value.getDate() - offset);
  return fmtDay(value);
}

function dayDistance(from: string, to: string) {
  const start = new Date(`${from}T00:00:00`).getTime();
  const end = new Date(`${to}T00:00:00`).getTime();
  return Math.round((end - start) / 86_400_000);
}

function formatActivityDate(date?: string) {
  if (!date) return "";
  return new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric" }).format(new Date(`${date}T00:00:00`));
}

function mergeByModel(models: ModelUsage[]): ModelUsage[] {
  const byModel = new Map<string, { tokens: TokenBreakdown; costs: (number | null)[] }>();
  for (const model of models) {
    const current = byModel.get(model.model) ?? { tokens: zeroTokens(), costs: [] };
    current.tokens = addTokens(current.tokens, model.tokens);
    current.costs.push(model.costUsd);
    byModel.set(model.model, current);
  }
  return Array.from(byModel.entries())
    .map(([model, item]) => ({
      model,
      tokens: item.tokens,
      costUsd: sumNullable(item.costs),
    }))
    .sort((a, b) => total(b.tokens) - total(a.tokens));
}

function sumTokens(items: TokenBreakdown[]) {
  return items.reduce(addTokens, zeroTokens());
}

function addTokens(a: TokenBreakdown, b: TokenBreakdown): TokenBreakdown {
  return {
    input: a.input + b.input,
    output: a.output + b.output,
    cacheRead: a.cacheRead + b.cacheRead,
    cacheCreation: a.cacheCreation + b.cacheCreation,
  };
}

function zeroTokens(): TokenBreakdown {
  return { input: 0, output: 0, cacheRead: 0, cacheCreation: 0 };
}

function sumNullable(values: (number | null)[]) {
  const present = values.filter((value): value is number => value != null);
  return present.length > 0 ? present.reduce((sum, value) => sum + value, 0) : null;
}

function total(t: TokenBreakdown) {
  return t.input + t.output + t.cacheRead + t.cacheCreation;
}

function formatTokens(n: number) {
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return `${n}`;
}

function formatUsd(n: number | null) {
  if (n == null) return "—";
  if (n > 0 && n < 0.01) return `$${n.toFixed(4)}`;
  return `$${n.toFixed(2)}`;
}

function localToday() {
  return fmtDay(new Date());
}

function addDays(date: string, n: number) {
  const d = new Date(`${date}T00:00:00`);
  d.setDate(d.getDate() + n);
  return fmtDay(d);
}

function fmtDay(d: Date) {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function formatDate(value: string) {
  try {
    return new Intl.DateTimeFormat("en-US", { dateStyle: "short" }).format(new Date(value));
  } catch {
    return value;
  }
}
