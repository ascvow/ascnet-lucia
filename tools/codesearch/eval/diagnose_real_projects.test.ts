import { expect, test } from "bun:test";
import { covers, loss } from "./diagnose_real_projects";

/** 首行可从中间开始，必须检查 UTF-8 字节而非仅比较行号。 */
test("范围不能替代实际语句覆盖", () => {
  const source = "中文();关键();\n";
  const location = { path: "a.ts", start_line: 1, end_line: 1, needles: ["关键()"] };
  const span = { path: "a.ts", start_line: 1, end_line: 1, start_byte: 0, end_byte: Buffer.byteLength(source), revision_id: 1 };
  expect(covers(span, location, source)).toBe(true);
  expect(covers({ ...span, end_byte: Buffer.byteLength("中文();") }, location, source)).toBe(false);
  expect(covers({ ...span, path: "b.ts" }, location, source)).toBe(false);
});

/** 按最靠近输出的证据分类，不能把扩展后补回的命中算成召回失败。 */
test("区分召回、候选截断、输出名额与正文裁剪", () => {
  expect(loss({}, true, true)).toBe("recall_miss");
  expect(loss({}, false, false)).toBe("index_coverage");
  expect(loss({}, true, false)).toBe("embedding_unavailable");
  expect(loss({ fused: 41 }, true, true)).toBe("candidate_limit");
  expect(loss({ ordered: 15 }, true, true)).toBe("result_selection");
  expect(loss({ budget_before: 1 }, true, true)).toBe("context_clipping");
  expect(loss({ budget_before: 1, final: 1 }, true, true)).toBe("hit");
});
