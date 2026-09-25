import { lazy, Suspense, useEffect, useState, type CSSProperties, type ReactNode } from "react"
import {
  siAlmalinux, siAlpinelinux, siArchlinux, siCentos, siDebian, siFedora, siLinux, siOpensuse, siRedhat,
  siRockylinux, siUbuntu, type SimpleIcon,
} from "simple-icons"

import { Skeleton } from "@/components/ui/skeleton"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import type { Node } from "@/lib/api"
import {
  bytes, compact, CYCLES, daysUntil, distro, duration, FOREVER, money, monthUsage, osName, cpuName, pair,
  percent, speed, speedRate, uptime,
} from "@/lib/format"
import { Link } from "@/lib/route"
import { cn } from "@/lib/utils"

// Emitted as files and fetched on first use, so a page carries only the flags its
// nodes are in rather than all 271. vite.config.ts keeps the small ones from being
// inlined into the bundle as data URLs.
const FLAGS = Object.fromEntries(
  Object.entries(
    import.meta.glob<string>("/node_modules/flag-icons/flags/4x3/*.svg", {
      query: "?url",
      import: "default",
      eager: true,
    }),
  ).map(([path, url]) => [path.match(/([\w-]+)\.svg$/)![1], url]),
)

const Latency = lazy(() => import("@/components/NodeDetail").then((m) => ({ default: m.Latency })))

/** A node that has reported once knows its shape; one that never connected has nothing to show. */
export function deployed(node: Node) {
  return node.cpu_cores > 0 || node.mem_total > 0
}

export function Dot({ node, className }: { node: Node; className?: string }) {
  return (
    <span
      title={node.online ? "在线" : deployed(node) ? "离线" : "未接入"}
      className={cn(
        "inline-block size-3 shrink-0 rounded-full align-middle",
        node.online ? "bg-(image:--dot-online)" : deployed(node) ? "bg-(image:--dot-offline)" : "bg-muted-foreground/40",
        className,
      )}
    />
  )
}

export function Flag({ code, className }: { code: string; className?: string }) {
  if (!code) return <span className="text-muted-foreground">—</span>
  const src = FLAGS[code.toLowerCase()]
  return (
    <span className={cn("inline-flex items-center justify-center gap-1", className)}>
      {src && <img src={src} alt="" className="h-3 w-4 shrink-0 rounded-[2px] object-cover ring-1 ring-foreground/10" />}
      <span className="@max-3xl:hidden">{code}</span>
    </span>
  )
}

// Matched against the whole release name, since "Red Hat Enterprise Linux" and
// "Raspbian GNU/Linux" do not lead with one word to key on. The distributions a
// VPS ships with; the rest take the penguin. Each logo costs 1-6 KB of entry
// bundle, the Raspberry Pi alone 12 KB, so the list stays at what hosts offer.
const DISTROS: [string, SimpleIcon][] = [
  ["debian", siDebian], ["raspbian", siDebian], ["ubuntu", siUbuntu], ["alpine", siAlpinelinux],
  ["centos", siCentos], ["rocky", siRockylinux], ["almalinux", siAlmalinux], ["red hat", siRedhat],
  ["fedora", siFedora], ["arch", siArchlinux], ["opensuse", siOpensuse],
]

/**
 * The distribution's logo in its brand colour. Mixed toward white on the dark
 * theme, where AlmaLinux's black and CentOS's navy would otherwise vanish.
 */
export function OsIcon({ os, className }: { os: string; className?: string }) {
  if (!os) return null
  const name = os.toLowerCase()
  const icon = DISTROS.find(([key]) => name.includes(key))?.[1] ?? siLinux
  return (
    <svg
      viewBox="0 0 24 24"
      aria-hidden
      style={{ "--brand": `#${icon.hex}` } as CSSProperties}
      className={cn("size-3.5 shrink-0 fill-(--brand) dark:fill-[color-mix(in_oklab,var(--brand)_60%,white)]", className)}
    >
      <path d={icon.path} />
    </svg>
  )
}

/**
 * The label sits over both halves of the bar in the text colour, which is why the
 * fills are light in the light theme and dark in the dark one.
 */
function Bar({ pct, label }: { pct: number | null; label?: string }) {
  const v = pct === null ? 0 : Math.min(100, Math.max(0, pct))
  const tone = v >= 90 ? "bg-(image:--bar-danger)" : v >= 80 ? "bg-(image:--bar-warn)" : "bg-(image:--bar-ok)"
  return (
    <div className="relative h-5 overflow-hidden rounded bg-bar-track shadow-[inset_0_1px_2px_rgb(0_0_0/0.1)] @max-3xl:h-4">
      <div className={cn("h-full rounded-l-[3px] transition-[width] duration-500", tone)} style={{ width: `${v}%` }} />
      <span className="tnum absolute inset-y-0 left-1.5 flex items-center text-[10px] leading-none text-bar-text @max-3xl:left-0.5 @max-3xl:text-[8px]">
        {label ?? (pct === null ? "—" : `${v.toFixed(1)}%`)}
      </span>
    </div>
  )
}

function Expiry({ node }: { node: Node }) {
  if (node.billing_known === false) return <span className="text-muted-foreground" title="未设置到期信息">—</span>
  const days = daysUntil(node.expires_at)
  if (days === null) return <span className="text-muted-foreground" title="永不到期">{FOREVER}</span>
  if (days < 0) return <span className="text-danger">已过期</span>
  return <span className={cn(days <= 7 && "text-warn")}>{days} 天</span>
}

/**
 * Column widths and what folds away, applied to the header and every cell alike.
 * The panel is the container, so the table follows its own width rather than the
 * viewport's. Below 768px it switches to a fixed layout that fits a phone without
 * sideways scrolling, keeping the columns that change every push.
 */
const COL = {
  status: "w-14 @max-3xl:w-[6%]",
  name: "max-w-60 min-w-32 truncate @max-3xl:w-[14%] @max-3xl:max-w-none @max-3xl:min-w-0 @max-sm:w-[27%]",
  location: "w-20 @max-3xl:w-[7%] @max-sm:hidden",
  os: "min-w-24 @max-6xl:hidden",
  uptime: "min-w-18 @max-3xl:hidden",
  expiry: "min-w-18 @max-6xl:hidden",
  load: "w-16 @max-3xl:hidden",
  speed: "min-w-30 @max-3xl:w-[21%] @max-3xl:min-w-0 @max-sm:w-[19%]",
  bar: "w-[7.5%] min-w-22 @max-3xl:w-[10%] @max-3xl:min-w-0 @max-sm:w-[10%]",
  traffic: "w-[7.5%] min-w-22 @max-3xl:w-[22%] @max-3xl:min-w-0 @max-sm:w-[18%]",
}

/** One fact per line, the label in a fixed column so the values align. */
function Line({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid min-w-0 grid-cols-[5.5em_minmax(0,1fr)] gap-x-2">
      <span className="text-muted-foreground">{label}</span>
      <span className="tnum break-words">{children}</span>
    </div>
  )
}

function Details({ node, chart }: { node: Node; chart: boolean }) {
  if (!deployed(node)) {
    return <p className="px-4 py-3 text-muted-foreground">尚未接入。在后台生成安装命令并执行一次。</p>
  }
  const m = node.online ? node.metrics : null
  const usage = (used: number, total: number) => `${pair(used, total)}（${percent(used, total).toFixed(1)}%）`
  const flow = (rx: number, tx: number) => `↓ ${bytes(rx)} · ↑ ${bytes(tx)}`
  const away = node.last_seen ? Date.now() / 1000 - node.last_seen : 0
  const days = daysUntil(node.expires_at)

  return (
    <div className="space-y-3 px-4 pt-2 pb-3 text-[13px] leading-6 @max-3xl:px-2 @max-3xl:text-xs @max-3xl:leading-5">
      {/* Three across, one topic a row: the machine, what it holds, what it is
          doing, what it has moved, and its term. Low enough that the chart
          beneath stays in view when a row opens. */}
      <div className="grid gap-x-8 @2xl:grid-cols-2 @5xl:grid-cols-3">
        <Line label="系统">
          <span className="inline-flex items-center gap-1.5 align-middle">
            <OsIcon os={node.os} />
            {[osName(node.os), node.kernel].filter(Boolean).join(" · ") || "—"}
          </span>
        </Line>
        <Line label="架构">
          {[node.arch, node.virt !== "none" && node.virt, node.agent_version && `agent ${node.agent_version}`]
            .filter(Boolean)
            .join(" · ") || "—"}
        </Line>
        <Line label="CPU">
          {node.cpu_name ? `${cpuName(node.cpu_name)} × ${node.cpu_cores}` : `${node.cpu_cores} 核`}
          {m && `（${m.cpu.toFixed(1)}%）`}
        </Line>

        <Line label="内存">{m ? usage(m.mem_used, m.mem_total) : bytes(node.mem_total)}</Line>
        <Line label="交换">
          {node.swap_total > 0 ? (m ? usage(m.swap_used, m.swap_total) : bytes(node.swap_total)) : "未启用"}
        </Line>
        <Line label="硬盘">{m ? usage(m.disk_used, m.disk_total) : bytes(node.disk_total)}</Line>

        <Line label="负载">{m ? m.load.map((n) => n.toFixed(2)).join(" / ") : "—"}</Line>
        <Line label="进程 / 连接">{m && (m.procs || m.tcp || m.udp) ? `${m.procs} · TCP ${m.tcp} · UDP ${m.udp}` : "—"}</Line>
        <Line label="网速">{m ? `↓ ${speedRate(m.net_rx)} · ↑ ${speedRate(m.net_tx)}` : "—"}</Line>

        <Line label="今日流量">{flow(node.day_rx, node.day_tx)}</Line>
        <Line label="本月流量">{flow(node.month_rx, node.month_tx)}</Line>
        <Line label="总流量">{flow(node.total_rx, node.total_tx)}</Line>

        <Line label={node.online ? "在线" : "离线"}>
          {node.online ? (m ? uptime(m.uptime) : "等待上报") : away >= 60 ? uptime(away) : "刚刚"}
        </Line>
        <Line label="续费">
          {node.billing_known === false ? "未设置" : node.price > 0 ? `${money(node.price, node.currency)} / ${CYCLES[node.billing_cycle] ?? node.billing_cycle}` : "免费"}
        </Line>
        <Line label="到期">
          {node.billing_known === false ? "未设置" : node.expires_at
            ? `${node.expires_at}（${days !== null && days < 0 ? `已过期 ${-days} 天` : `剩余 ${days} 天`}）`
            : "长期有效"}
        </Line>

        {node.traffic_reset_day > 0 && (
          <Line label="流量重置">每月 {node.traffic_reset_day} 日重置</Line>
        )}
      </div>

      <div className="space-y-2 border-t pt-3">
        <div className="flex items-baseline justify-between gap-3 text-xs">
          <span className="text-muted-foreground">网络延迟 · 最近 24 小时</span>
          <Link href={`/node/${node.id}`} className="text-primary hover:underline">查看资源图表 →</Link>
        </div>
        {/* Mounted once the row has finished opening, so the request and the
            chart's first paint do not land on the animation. The placeholder
            holds the height it takes, so nothing shifts when it arrives, and a
            chart fetched a moment ago is drawn from the cache at once. */}
        {chart ? (
          <Suspense fallback={<ChartPlaceholder className={CHART} />}>
            <Latency id={node.id} className={CHART} />
          </Suspense>
        ) : (
          <ChartPlaceholder className={CHART} />
        )}
      </div>
    </div>
  )
}

/**
 * Mount state for a row's body, one animation behind the click: it has to be in
 * the DOM for the height transition to run on, and has to go once closed, since
 * the chart inside it fetches on mount.
 */
function useExpand(open: boolean, ms = 300) {
  const [mounted, setMounted] = useState(false)
  // `grown` opens the body to its height; `settled` marks the height reached.
  const [grown, setGrown] = useState(false)
  const [settled, setSettled] = useState(false)

  // Mounted in the render the click lands on, not in the effect below: a body
  // added one commit later would be on screen already at full height, with
  // nothing for the transition to run on.
  if (open && !mounted) setMounted(true)

  useEffect(() => {
    if (!open) {
      // Dropped only once it has collapsed, and reset so the next open starts
      // from the closed height again.
      const timer = setTimeout(() => {
        setMounted(false)
        setGrown(false)
        setSettled(false)
      }, ms)
      return () => clearTimeout(timer)
    }
    // Two frames, not one: the first paints the body closed, which is the height
    // the transition has to start from.
    let frame = requestAnimationFrame(() => {
      frame = requestAnimationFrame(() => setGrown(true))
    })
    // The chart is held back until the height has stopped moving: fetched as the
    // row opens, its request and first paint land on the animation, and a row
    // opened and closed within this window never asks at all.
    const timer = setTimeout(() => setSettled(true), ms + 80)
    return () => {
      cancelAnimationFrame(frame)
      clearTimeout(timer)
    }
  }, [open, ms])

  return { mounted, expanded: open && grown, chart: open && settled }
}

/** The chart's own height, held open while it loads and while the row expands. */
const CHART = "h-[280px] @max-3xl:h-[220px]"

/**
 * The chart's shape before there is a chart: its legend is a row of its own above
 * the plot, so reserving only the plot would shift everything below by a legend
 * the moment the line arrives.
 */
export function ChartPlaceholder({ className }: { className?: string }) {
  return (
    <div className="space-y-2">
      <Skeleton className="mx-auto h-[26px] w-40" />
      <Skeleton className={cn("w-full", className)} />
    </div>
  )
}

function Row({ node, index }: { node: Node; index: number }) {
  const [open, setOpen] = useState(false)
  const { mounted, expanded, chart } = useExpand(open)
  const m = node.online ? node.metrics : null
  const traffic = monthUsage(node)
  // Parity from the node rather than :nth-child, so an opened detail row takes its
  // node's shade instead of shifting every row beneath it.
  const shade = index % 2 ? "bg-muted" : ""
  const toggle = () => setOpen((o) => !o)

  return (
    <>
      <TableRow
        aria-expanded={open}
        tabIndex={0}
        onClick={toggle}
        onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), toggle())}
        className={cn("cursor-pointer border-0 hover:bg-accent", shade)}
      >
        <TableCell className={COL.status}><Dot node={node} className="mx-auto block size-[9px] @max-sm:size-[7px]" /></TableCell>
        {/* 窄屏时名字靠左，国旗排成一列 */}
        <TableCell className={cn(COL.name, "@max-sm:pl-1 @max-sm:text-left")} title={node.name}>
          {/* 窄屏时「位置」列是隐藏的，小国旗挪到名字前面 */}
          {node.country && FLAGS[node.country.toLowerCase()] && (
            <img src={FLAGS[node.country.toLowerCase()]} alt="" className="mr-1.5 hidden h-2.5 w-3.5 rounded-[1px] object-cover align-[-1px] ring-1 ring-foreground/10 @max-sm:inline-block" />
          )}
          {node.name}
        </TableCell>
        <TableCell className={COL.location}><Flag code={node.country} /></TableCell>
        <TableCell className={COL.os}>
          <span className="inline-flex items-center justify-center gap-1.5">
            <OsIcon os={node.os} />
            {distro(node.os) || "—"}
          </span>
        </TableCell>
        <TableCell className={COL.uptime}>{m ? duration(m.uptime) : "—"}</TableCell>
        <TableCell className={COL.expiry}><Expiry node={node} /></TableCell>
        <TableCell className={COL.load}>{m ? m.load[0].toFixed(2) : "—"}</TableCell>
        <TableCell className={COL.speed}>{m ? `${speed(m.net_rx)} | ${speed(m.net_tx)}` : "— | —"}</TableCell>
        <TableCell className={COL.bar}><Bar pct={m ? m.cpu : null} /></TableCell>
        <TableCell className={COL.bar}><Bar pct={m ? percent(m.mem_used, m.mem_total) : null} /></TableCell>
        <TableCell className={COL.bar}><Bar pct={m ? percent(m.disk_used, m.disk_total) : null} /></TableCell>
        <TableCell
          className={COL.traffic}
          title={`本月已用 ${node.traffic_limit_known === false ? `${bytes(traffic)} · 流量配额未设置` : node.traffic_limit > 0 ? `${pair(traffic, node.traffic_limit)}（${((traffic / node.traffic_limit) * 100).toFixed(1)}%）` : `${bytes(traffic)} · 无流量配额`}`}
        >
          <Bar
            pct={node.traffic_limit > 0 ? percent(traffic, node.traffic_limit) : null}
            label={`${compact(traffic)} / ${node.traffic_limit_known === false ? "—" : node.traffic_limit > 0 ? compact(node.traffic_limit) : FOREVER}`}
          />
        </TableCell>
      </TableRow>
      {mounted && (
        <TableRow className={cn("border-0 hover:bg-transparent", shade)}>
          {/* `h-0` over the body's `h-[29px]`, which a cell would otherwise hold
              as its minimum and a collapsing row would stop short of. */}
          <TableCell colSpan={12} className="h-0! border-t-0! p-0! text-left whitespace-normal">
            {/* Grid rows rather than height: `0fr -> 1fr` is the one way to
                transition to the content's own height, and the child's overflow is
                what lets it close past its contents. */}
            <div className={cn("grid transition-[grid-template-rows] duration-300 ease-out", expanded ? "grid-rows-[1fr]" : "grid-rows-[0fr]")}>
              <div className="overflow-hidden">
                <Details node={node} chart={chart} />
              </div>
            </div>
          </TableCell>
        </TableRow>
      )}
    </>
  )
}

export function ServerTable({ nodes }: { nodes: Node[] }) {
  const heads: [keyof typeof COL, ReactNode][] = [
    ["status", "状态"], ["name", "名称"], ["location", "位置"], ["os", "系统"], ["uptime", "在线"],
    ["expiry", "到期"], ["load", "负载"], ["speed", "网速 ↓|↑"],
    ["bar", "CPU"], ["bar", "内存"], ["bar", "硬盘"], ["traffic", "流量"],
  ]

  return (
    <section className="@container rounded-md border bg-card p-5 text-card-foreground shadow-sm max-md:p-2">
      <h2 className="px-1 pb-3 text-lg font-semibold max-md:pb-2 max-md:text-sm">服务器</h2>
      <Table className="text-center text-sm @max-3xl:table-fixed @max-3xl:text-[10px]">
        <TableHeader>
          <TableRow className="border-0 hover:bg-transparent">
            {heads.map(([col, label], i) => (
              <TableHead key={i} className={cn("h-8 border-t px-1.5 text-center font-semibold @max-3xl:px-0.5", COL[col])}>
                {label}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        {/* Rules between rows rather than under them, as the header row starts. */}
        <TableBody className="[&_td]:h-[29px] [&_td]:border-t [&_td]:px-1.5 [&_td]:py-1 @max-3xl:[&_td]:px-0.5">
          {nodes.map((n, i) => (
            <Row key={n.id} node={n} index={i} />
          ))}
        </TableBody>
      </Table>
    </section>
  )
}
