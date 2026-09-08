/** 统计口径回归：未实现问题不冒充成功，服务错误仍留在应答分母中。 */
import { expect, test } from "bun:test";
import { metrics } from "./summarize_real_projects";

test("仅用独立实现问题首轮评分，不把重复请求增算为样本", () => {
  const rows = [
    { round: 1, kind: "implemented", observation: { implementation_rank: 1, file_rank: 1, latency_ms: 10 } },
    { round: 2, kind: "implemented", observation: { implementation_rank: 1, file_rank: 1, latency_ms: 11 } },
    { round: 1, kind: "implemented", observation: { error: "模型不可用" } },
    { round: 1, kind: "unimplemented", observation: { implementation_rank: 1, file_rank: 1, latency_ms: 12 } },
  ];
  const summary = metrics(rows);
  expect(summary.implemented_queries).toBe(2);
  expect(summary.implementation_top1).toBe(0.5);
  expect(summary.request_errors).toBe(1);
});
