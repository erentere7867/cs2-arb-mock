"use strict";

const moneyFormat = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
const money = cents => cents == null ? "—" : moneyFormat.format(cents / 100);
const clock = new Intl.DateTimeFormat("en-GB", {
  timeZone: "UTC", dateStyle: "medium", timeStyle: "medium"
});
const stamp = date => date == null ? "—" : clock.format(new Date(date)) + " UTC";
function duration(seconds) {
  if (seconds == null) return "—";
  const s = Math.max(0, Math.floor(seconds));
  if (s < 60) return s + "s";
  if (s < 3600) return Math.floor(s / 60) + "m " + s % 60 + "s";
  if (s < 86400) return Math.floor(s / 3600) + "h " + Math.floor(s / 60) % 60 + "m";
  return Math.floor(s / 86400) + "d " + Math.floor(s / 3600) % 24 + "h";
}
const secondsSince = date => date == null ? null : (Date.now() - Date.parse(date)) / 1000;
const signClass = value => value > 0 ? "good" : value < 0 ? "loss" : "";

function el(tag, text, cls) {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  if (cls) node.className = cls;
  return node;
}
function panel(title, description, wide) {
  const node = el("section", null, wide ? "panel wide" : "panel");
  node.append(el("h2", title));
  if (description) node.append(el("p", description, "hint"));
  return node;
}
function metric(label, value, cls) {
  const node = el("div", null, "metric");
  node.append(el("span", label), el("strong", value, cls));
  return node;
}
function table(headers, rows) {
  const flow = el("div", null, "table-scroll");
  const t = el("table");
  const heading = t.createTHead().insertRow();
  headers.forEach(label => {
    const th = el("th", label);
    th.scope = "col";
    heading.append(th);
  });
  const body = t.createTBody();
  rows.forEach(cells => {
    const row = body.insertRow();
    cells.forEach(cell => {
      const td = row.insertCell();
      if (Array.isArray(cell)) {
        td.textContent = cell[0];
        td.className = cell[1];
      } else {
        td.textContent = cell;
      }
    });
  });
  flow.append(t);
  return flow;
}

function chart(panel, history) {
  const batches = [...history].reverse();
  const values = batches.map(batch => batch.total).filter(value => value != null);
  if (!values.length) {
    panel.append(el("p", "Waiting for a batch with P&L data.", "hint"));
    return;
  }
  let low = Math.min(0, ...values);
  let high = Math.max(0, ...values);
  if (low === high) { low = -100; high = 100; }
  const width = 550, left = 82, right = 534, top = 17, bottom = 141;
  const x = i => batches.length === 1 ? (left + right) / 2
    : left + i * (right - left) / (batches.length - 1);
  const y = value => bottom - (bottom - top) * (value - low) / (high - low);
  function svg(tag, attrs, text) {
    const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
    Object.entries(attrs).forEach(([key, value]) => node.setAttribute(key, value));
    if (text != null) node.textContent = text;
    return node;
  }
  const plot = svg("svg", {
    class: "chart", viewBox: "0 0 " + width + " 155", role: "img",
    "aria-label": "Total P&L by batch from " + money(values[0]) + " to " + money(values.at(-1))
  });
  for (const value of new Set([high, 0, low])) {
    plot.append(
      svg("line", { x1: left, x2: right, y1: y(value), y2: y(value), class: value === 0 ? "zero" : "rule" }),
      svg("text", { x: left - 8, y: y(value) + 4, "text-anchor": "end" }, money(value))
    );
  }
  let path = "", contiguous = false;
  const dots = [];
  batches.forEach((batch, i) => {
    if (batch.total == null) {
      contiguous = false;
      return;
    }
    path += (contiguous ? "L" : "M") + x(i) + " " + y(batch.total);
    contiguous = true;
    const dot = svg("circle", { cx: x(i), cy: y(batch.total), r: 3.6 });
    dot.append(svg("title", {}, stamp(batch.at) + " · " + money(batch.total)));
    dots.push(dot);
  });
  plot.append(svg("path", { d: path, class: "trace" }), ...dots);
  const range = el("div", null, "chart-caption hint");
  range.append(el("span", stamp(batches[0].at)));
  if (batches.length > 1) range.append(el("span", stamp(batches.at(-1).at)));
  panel.append(plot, range);
}

const pnlFields = [
  ["Total P&L", "total"], ["Net realized", "net"],
  ["Gross realized", "gross"], ["Unrealized", "unrealized"]
];
function overview(reports) {
  const latest = reports.history[0];
  const node = panel("Latest batch P&L", latest ? "Completed " + stamp(latest.at) : "No completed batches yet.");
  const totals = reports.latest?.strategies[0]?.pnl;
  if (totals) {
    const metrics = el("div", null, "metrics");
    pnlFields.forEach(([label, key]) => metrics.append(metric(label, money(totals[key]), signClass(totals[key]))));
    node.append(metrics);
    node.append(table(["Strategy", ...pnlFields.map(([label]) => label)], reports.latest.strategies.slice(1).map(strategy =>
      [strategy.name, ...pnlFields.map(([, key]) => [money(strategy.pnl[key]), signClass(strategy.pnl[key])])]
    )));
  } else if (latest) {
    node.append(el("p", "No P&L summary could be read for this batch.", "hint"));
  }
  return node;
}
function batchStatus(reports) {
  const node = panel("Batch activity", "Cadence is the time between the two most recent completed batches.");
  const pending = reports.pending_since;
  const metrics = el("div", null, "metrics");
  metrics.append(
    metric("Last report age", duration(secondsSince(reports.history[0]?.at))),
    metric("Last interval", duration(reports.cadence_secs)),
    metric("Batch marker", pending ? "In progress" : "Not present", pending ? "good" : ""),
    metric("Marker age", duration(secondsSince(pending)))
  );
  node.append(metrics);
  if (pending) node.append(el("p", "Marker created " + stamp(pending) + ". A leftover marker does not guarantee the job is running.", "hint"));
  if (reports.unavailable) node.append(el("p", "Some report data could not be read.", "warn hint"));
  return node;
}
function marketHealth(game) {
  const signal = game.market;
  const node = panel("Market API health", "From up to 300 journal entries for this game over the last hour.");
  if (!signal.readable) {
    node.append(el("p", "No readable recent unit logs. Market status is unknown.", "warn hint"));
    return node;
  }
  const metrics = el("div", null, "metrics");
  metrics.append(metric("Market reads skipped", signal.skipped_reads));
  if (game.id === "cs2") {
    metrics.append(
      metric("CSFloat reads skipped", signal.csfloat_skipped_reads),
      metric("CSFloat HTTP 429s", signal.csfloat_http_429s, signal.csfloat_http_429s ? "warn" : ""),
      metric("Other throttle signals", signal.csfloat_throttle_signals, signal.csfloat_throttle_signals ? "warn" : "")
    );
  }
  node.append(metrics);
  const observed = signal.last_csfloat_signal ? "Last CSFloat throttle signal: " + stamp(signal.last_csfloat_signal) + ". " : "";
  const note = game.id === "cs2"
    ? "A 429 or local cooldown does not establish an IP ban; zero observed signals do not establish API health."
    : "Zero skipped reads does not establish API health.";
  node.append(el("p", observed + note, "hint"));
  return node;
}
function pegStats(latest) {
  const node = panel("Peg bidding & relisting", "Simulated buy orders and relisting for the latest batch.");
  const metrics = Object.entries(latest?.peg ?? {});
  if (!metrics.length) {
    node.append(el("p", "No peg-bidding summary available.", "hint"));
    return node;
  }
  node.append(table(["Metric", "Value"], metrics));
  return node;
}
function watcherStats(watcher) {
  const node = panel("Steam update watcher", "CS2 public branch events and active simulated dip windows.");
  if (!watcher) {
    node.append(el("p", "No readable watcher database.", "hint"));
    return node;
  }
  const stats = el("div", null, "metrics");
  stats.append(metric("Last poll age", duration(secondsSince(watcher.observed_at))),
    metric("Active windows", watcher.windows.length, watcher.windows.length ? "good" : ""));
  node.append(stats);
  if (watcher.windows.length) node.append(table(["Branch", "Window closes"], watcher.windows.map(window =>
    [window.branch, stamp(window.closes_at)]
  )));
  if (watcher.events.length) node.append(table(["Recent event", "Branch", "Queued at"], watcher.events.map(event =>
    [event.kind.replaceAll("_", " "), event.branch ?? "—", stamp(event.at)]
  )));
  return node;
}

function renderReport(markdown) {
  const output = el("div", null, "report");
  if (!markdown) {
    output.append(el("p", "No report available.", "hint"));
    return output;
  }
  const lines = markdown.split(/\r?\n/);
  const cells = line => line.slice(1, line.endsWith("|") ? -1 : undefined)
    .split(/(?<!\\)\|/).map(field => field.trim().replaceAll("\\|", "|"));
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (!line) continue;
    if (line.startsWith("|") && /^\|\s*:?-+/.test(lines[i + 1] ?? "")) {
      const headers = cells(line);
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].startsWith("|")) rows.push(cells(lines[i++]));
      i--;
      output.append(table(headers, rows));
      continue;
    }
    const heading = /^(#{1,6}) (.+)$/.exec(line);
    if (heading) output.append(el("h" + Math.min(6, heading[1].length + 2), heading[2]));
    else if (line.startsWith("> ")) output.append(el("blockquote", line.slice(2)));
    else if (line === "_None_") output.append(el("p", "None", "hint"));
    else output.append(el("p", line));
  }
  return output;
}

const dashboard = document.querySelector("#dashboard");
const buttons = document.querySelectorAll("nav button");
const savedOpen = { cs2: true, rust: true };
let selected = "cs2";
let data;
let renderedReport;

function render() {
  if (!data) return;
  const game = data.games.find(game => game.id === selected);
  const reports = game.reports;
  const grid = el("div", null, "grid");
  const trend = panel("Recent batch trend", "Each point is one independent batch. Do not sum batches as a portfolio.");
  chart(trend, reports.history);
  grid.append(overview(reports), trend, batchStatus(reports), marketHealth(game), pegStats(reports.latest));
  if (selected === "cs2") grid.append(watcherStats(data.watcher));
  dashboard.querySelector(".grid")?.remove();
  dashboard.prepend(grid);

  const reportKey = selected + ":" + (reports.history[0]?.at ?? "") + ":" + Boolean(reports.latest);
  if (reportKey !== renderedReport) {
    dashboard.querySelector("details")?.remove();
    const details = el("details", null, "panel");
    const id = selected;
    details.open = savedOpen[id];
    details.addEventListener("toggle", () => { savedOpen[id] = details.open; });
    details.append(el("summary", "Latest batch report"), renderReport(reports.latest?.markdown));
    dashboard.append(details);
    renderedReport = reportKey;
  }
}

buttons.forEach(button => button.addEventListener("click", () => {
  selected = button.dataset.game;
  buttons.forEach(item => item.setAttribute("aria-pressed", String(item === button)));
  render();
}));

const status = document.querySelector("#connection");
const updated = document.querySelector("#updated");
const kill = document.querySelector("#kill");
async function refresh() {
  try {
    const response = await fetch("/api/dashboard", { cache: "no-store", signal: AbortSignal.timeout(4000) });
    if (!response.ok) throw new Error("Dashboard request failed");
    data = await response.json();
    render();
    const age = secondsSince(data.updated_at);
    status.textContent = age > 25 ? "Stale snapshot" : "Refreshing every 5s";
    status.className = "pill " + (age > 25 ? "warn" : "good");
    updated.textContent = "Snapshot: " + stamp(data.updated_at);
    const state = data.kill_switch_file;
    kill.textContent = "Kill-switch file: " + (state == null ? "unknown" : state ? "present" : "absent");
    kill.className = "pill " + (state === false ? "" : "warn");
    kill.title = "File indicator only; it does not reveal environment or a process-latched switch.";
  } catch {
    status.textContent = data ? "Offline · showing last snapshot" : "Dashboard data unavailable";
    status.className = "pill warn";
  } finally {
    setTimeout(refresh, 5000);
  }
}
refresh();
