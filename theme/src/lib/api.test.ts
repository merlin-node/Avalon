/// <reference types="node" />
import assert from "node:assert/strict"
import { adaptNode, safeNodes, type Node } from "./api.ts"

const node = { id: 1, metrics: { uptime: 100, cpu: 1, load: [0.1, 0.2, 0.3],
  mem_total: 1024, mem_used: 512, swap_total: 0, swap_used: 0, disk_total: 2048, disk_used: 1024,
  net_rx: 10, net_tx: 20, total_rx: 100, total_tx: 200, month_rx: 50, month_tx: 100,
  tcp: 3, udp: 4, procs: 20 } } as Node
assert.equal(safeNodes([node])[0], node)
for (const patch of [{ load: null }, { load: [1, "bad", 3] }, { cpu: "bad" }, { net_rx: Infinity }]) {
  const bad = { ...node, metrics: { ...node.metrics, ...patch } } as unknown as Node
  const result = safeNodes([bad, node])
  assert.equal(result[0].metrics, null)
  assert.equal(result[1], node)
}
console.log("invalid live reports are isolated")

type Probe = Parameters<typeof adaptNode>[0]
const probe = (disks: unknown) => adaptNode({ id: "0123456789abcdef", online: true, disks } as unknown as Probe).disks
const two = [{ mount: "/", used: 1, total: 2 }, { mount: "", used: 0, total: 5 }]
assert.deepEqual(probe(two), two)
assert.deepEqual(probe(undefined), [], "旧主控、单盘机器没有分盘")
assert.deepEqual(probe([two[0]]), [], "只有一块就不分开列")
assert.deepEqual(probe([two[0], { mount: "/data", used: "x", total: 5 }]), [], "有一项不对就整个不用")
assert.deepEqual(probe([two[0], null]), [])
console.log("disk lists fall back to the total")
