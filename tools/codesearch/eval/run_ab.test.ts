/** 评测统计本身的回归测试，避免把失败或平局统计成准确率提升。 */
import { expect, test } from "bun:test";
import { signTest, summarize } from "./run_ab";

test("双侧符号检验保留平局与小样本的不确定性", () => {
  expect(signTest(0, 0)).toBe(1);
  expect(signTest(4, 0)).toBe(0.125);
  expect(signTest(0, 4)).toBe(0.125);
  expect(signTest(3, 3)).toBe(1);
});

test("失败仍计入分母，不伪装成成功查询", () => {
  const metrics = summarize([
    { rank: 1, milliseconds: 10, results: [] },
    { rank: 3, milliseconds: 20, results: [] },
    { rank: null, milliseconds: 30, results: [], error: "服务不可用" },
  ]);
  expect(metrics.recall1).toBeCloseTo(1 / 3);
  expect(metrics.recall3).toBeCloseTo(2 / 3);
  expect(metrics.mrr).toBeCloseTo(4 / 9);
  expect(metrics.failures).toBe(1);
});
