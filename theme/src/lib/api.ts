import { useEffect, useState } from "react"

export type Metrics = {
  uptime: number
  cpu: number
  load: [number, number, number]
  mem_total: number
  mem_used: number
  swap_total: number
  swap_used: number
  disk_total: number
  disk_used: number
  net_rx: number
  net_tx: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  tcp: number
  udp: number
  procs: number
}

export type Node = {
  id: number
  name: string
  sort: number
  public: boolean
  online: boolean
  /** ISO 3166-1 alpha-2, or empty when the hub could not locate the address. */
  country: string
  last_seen: number
  metrics: Metrics | null
  os: string
  kernel: string
  arch: string
  virt: string
  cpu_name: string
  cpu_cores: number
  mem_total: number
  swap_total: number
  disk_total: number
  agent_version: string
  price: number
  /** Adapter metadata: distinguish an unconfigured plan from a genuinely free one. */
  billing_known?: boolean
  currency: string
  billing_cycle: string
  expires_at: string | null
  traffic_limit: number
  traffic_limit_known?: boolean
  traffic_mode: string
  traffic_reset_day: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  month_start: string
  day_rx: number
  day_tx: number
  /** Panel only. */
  hostname?: string
  ip?: string
  remark?: string
}

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

/** Keep the upstream theme's data shape while reading the Rust hub's public API. */
type ProbeNode = {
  id: string; name: string; online: boolean; last_seen: number | null
  cpu: number; memory: number; disk: number; rx: number; tx: number
  load: number; load5: number; load15: number; uptime: number
  mem_used: number; mem_total: number; swap_used: number; swap_total: number
  disk_used: number; disk_total: number; os: string; kernel: string
  arch: string; cpu_model: string; cpu_cores: number; agent_version?: string
  rx_total: number; tx_total: number; day_rx: number; day_tx: number
  month_rx: number; month_tx: number
  country?: string; price?: number; currency?: string; billing_cycle?: string; expires_at?: string
  traffic_limit?: number; traffic_mode?: string; traffic_reset_day?: number
}

const originalIds = new Map<number, string>()
function publicId(id: string): number {
  // All hub-generated IDs are 16 hexadecimal digits. The first 13 fit in a JS number.
  return parseInt(id.slice(0, 13), 16)
}

export function adaptNode(n: ProbeNode, sort = 0): Node {
  const id = publicId(n.id)
  originalIds.set(id, n.id)
  const m: Metrics = {
    uptime: n.uptime, cpu: n.cpu, load: [n.load, n.load5, n.load15],
    mem_total: n.mem_total, mem_used: n.mem_used, swap_total: n.swap_total,
    swap_used: n.swap_used, disk_total: n.disk_total, disk_used: n.disk_used,
    net_rx: n.rx, net_tx: n.tx, total_rx: n.rx_total, total_tx: n.tx_total,
    month_rx: n.month_rx, month_tx: n.month_tx, tcp: 0, udp: 0, procs: 0,
  }
  return {
    id, name: n.name, sort, public: true, online: n.online, country: n.country ?? "",
    last_seen: n.last_seen ?? 0, metrics: n.online ? m : null,
    os: n.os, kernel: n.kernel, arch: n.arch, virt: "", cpu_name: n.cpu_model,
    cpu_cores: n.cpu_cores, mem_total: n.mem_total, swap_total: n.swap_total,
    disk_total: n.disk_total, agent_version: n.agent_version ?? "", price: n.price ?? 0,
    billing_known: Boolean(n.billing_cycle || n.currency || n.price || n.expires_at), currency: n.currency ?? "",
    billing_cycle: n.billing_cycle ?? "", expires_at: n.expires_at || null,
    traffic_limit: n.traffic_limit ?? 0, traffic_limit_known: Boolean(n.traffic_limit), traffic_mode: n.traffic_mode ?? "",
    traffic_reset_day: n.traffic_reset_day ?? 1, total_rx: n.rx_total, total_tx: n.tx_total,
    month_rx: n.month_rx, month_tx: n.month_tx, month_start: "",
    day_rx: n.day_rx, day_tx: n.day_tx,
  }
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  if (path === "/me") {
    // 站点名称在后台可改，hub 从 /api/site 下发；拿不到就用默认名，页面照常出来。
    const site = await fetch("/api/site", { signal: init?.signal }).then((r) => r.json()).catch(() => ({}))
    // admin_url 只在已登录的管理员请求时才有；隐藏的后台地址不会出现在给访客的响应里。
    return { authed: !!site.admin_url, admin_url: site.admin_url || null, github: false, site_name: site.site_name || "Avalon", public_page: true } as T
  }
  const history = path.match(/^\/nodes\/(\d+)\/metrics\?hours=(\d+)&points=(\d+)&series=(metrics|ping)$/)
  const original = history ? originalIds.get(Number(history[1])) : undefined
  if (history && !original) throw new ApiError(404, "节点不存在")
  const target = history
    ? history[4] === "ping"
      ? `/api/nodes/${original}/ping?hours=${history[2]}&points=${history[3]}`
      : `/api/nodes/${original}/history?hours=${history[2]}`
    : "/api/nodes"
  const res = await fetch(target, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  })
  if (!res.ok) throw new ApiError(res.status, (await res.text()) || res.statusText)
  if (res.status === 204) return undefined as T
  const data = await res.json()
  if (path === "/nodes") return { nodes: (data as ProbeNode[]).map(adaptNode) } as T
  if (history) {
    // 延迟：hub 直接给出这个节点上跑的每个监控的曲线、波动区间和丢包率，
    // 一个节点多条线。这里不再需要拉一次 /api/nodes 去凑探测名。
    if (history[4] === "ping") return data as T
    const nodeList = await fetch("/api/nodes", { signal: init?.signal }).then((r) => r.json()) as ProbeNode[]
    const node = nodeList.find((n) => n.id === original)
    return { metrics: (data as Array<{ ts: number; cpu: number; memory: number; disk: number; rx: number; tx: number }>).map((p) => ({
      ts: p.ts, cpu: p.cpu,
      mem_used: (node?.mem_total ?? 0) * p.memory / 100,
      disk_used: (node?.disk_total ?? 0) * p.disk / 100,
      net_rx: p.rx, net_tx: p.tx,
    })), ping: [], probes: {} } as T
  }
  throw new ApiError(404, "没有此接口")
}

/** A malformed report must not remove every other node from the page. */
export function safeNodes(nodes: Node[]): Node[] {
  const number = (v: unknown) => typeof v === "number" && Number.isFinite(v) && v >= 0
  const fields = ["uptime", "cpu", "mem_total", "mem_used", "swap_total", "swap_used", "disk_total", "disk_used",
    "net_rx", "net_tx", "total_rx", "total_tx", "month_rx", "month_tx", "tcp", "udp", "procs"] as const
  return nodes.map((node) => {
    const m = node.metrics
    return !m || (fields.every((key) => number(m[key])) && Array.isArray(m.load) && m.load.length === 3 && m.load.every(number))
      ? node : { ...node, metrics: null }
  })
}

/** Public node list; poll the existing Rust hub every five seconds. */
export function useNodes() {
  const [nodes, setNodes] = useState<Node[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Set when the hub answers 401: the status page has been closed to anonymous
  // callers since this tab loaded. The hub also ends the stream, so this surfaces
  // on the fallback fetch the reconnect starts; a close allows a client to
  // re-query its state but cannot compel it.
  const [closed, setClosed] = useState(false)

  useEffect(() => {
    let poll: ReturnType<typeof setInterval> | null = null

    const apply = (list: Node[]) => {
      setNodes(safeNodes(list))
      setError(null)
      setClosed(false)
    }

    // The hub updates every few seconds, and a hidden tab pays for each of those
    // renders -- the whole table, or the open detail page with its four charts.
    // The newest payload is held instead, and applied when the page is looked at
    // again, which is when drawing it is worth anything.
    let held: Node[] | null = null

    const receive = (list: Node[]) => {
      if (document.hidden) {
        held = list
        return
      }
      held = null
      apply(list)
    }

    const onVisible = () => {
      if (document.hidden || !held) return
      const list = held
      held = null
      apply(list)
    }
    document.addEventListener("visibilitychange", onVisible)

    const fetchOnce = () =>
      api<{ nodes: Node[] }>("/nodes")
        .then((d) => receive(d.nodes))
        .catch((e: Error) => {
          setError(e.message)
          if (e instanceof ApiError && e.status === 401) setClosed(true)
        })

    fetchOnce()

    // This hub exposes live reports through a small polling API.
    poll = setInterval(fetchOnce, 5000)

    return () => {
      if (poll) clearInterval(poll)
      document.removeEventListener("visibilitychange", onVisible)
    }
  }, [])

  return { nodes, error, closed }
}
