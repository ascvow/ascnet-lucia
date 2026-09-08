/** 准备冻结候选重放并按固定真值评分；不调用模型，不把真值提供给生产组装。 */
import { join, resolve } from "node:path";
import { signTest } from "./run_ab";

/** 同时要求关键范围和实际语句，未命中返回 null。 */
export function implementationRank(results: any[], locations: any[]): number | null {
  const index = results.findIndex(hit => locations.some(target => target.path === hit.path && hit.start_line <= target.start_line
    && hit.end_line >= target.end_line && target.needles.every((needle: string) => hit.content.includes(needle))));
  return index < 0 ? null : index + 1;
}

/** 统计独立问题，不把重放次数增加为样本数；原始未命中作为无穷名次参与配对。 */
export function paired(rows: any[]) {
  const wins = rows.filter(row => (row.after ?? Infinity) < (row.before ?? Infinity)).length;
  const losses = rows.filter(row => (row.after ?? Infinity) > (row.before ?? Infinity)).length;
  const scores = (side: string) => Object.fromEntries([1, 5, 10].map(k => [`top${k}`, rows.filter(row => row[side] != null && row[side] <= k).length]));
  return { n: rows.length, before: scores("before"), after: scores("after"), wins, losses, ties: rows.length-wins-losses, sign_test_p: signTest(wins, losses) };
}

/** 输出在 target 内；可用不同 input 替换候选评分以做单变量重排实验。 */
async function main() {
  const base = resolve(import.meta.dir, "..");
  const out = join(base, "target/real-projects-2026-09-08");
  const label = Bun.argv[2];
  if (!label || !/^[a-z0-9-]+$/.test(label)) throw new Error("需要独立的小写实验标签");
  const manifest = await Bun.file(join(out, "manifest.json")).json();
  const truth = await Bun.file(join(base, "eval/real-projects-truth.json")).json();
  const input = [];
  const baseline = [];
  for (const project of manifest.projects) {
    const php = project.id === "1086";
    const prefix = project.id + (php ? "-input6000" : "");
    const traces = (await Bun.file(join(out, `${prefix}-stages-v1-results.jsonl`)).text()).trim().split("\n").map(line => JSON.parse(line));
    const results = await Bun.file(join(out, `${prefix}-stages-v1-results.json`)).json();
    baseline.push(...results.rows);
    input.push({ db: join(php ? join(project.data_dir, "../index-input6000") : project.data_dir, "index.db"),
      config: php ? join(project.config, "../codesearch-input6000.toml") : project.config,
      rows: results.rows.map((row: any) => ({ id: row.id, query: row.query,
        candidates: traces.find(trace => trace.query === row.query).stages.find((stage: any) => stage.stage === "candidates").data })) });
  }
  const inputPath = join(out, `${label}-input.json`);
  const outputPath = join(out, `${label}-output.json`);
  if (!await Bun.file(inputPath).exists()) {
    if (Bun.argv.includes("--require-input")) throw new Error("实验候选尚未生成，不得用基线候选替代");
    await Bun.write(inputPath, JSON.stringify(input));
  }
  const inputHash = new Bun.CryptoHasher("sha256").update(await Bun.file(inputPath).bytes()).digest("hex");
  const meta = Bun.file(outputPath + ".meta.json");
  if (await Bun.file(outputPath).exists() && (!await meta.exists() || (await meta.json()).input_sha256 !== inputHash)) {
    throw new Error("已有重放输出缺少输入指纹或输入已变，请使用新标签重新运行");
  }
  if (!await Bun.file(outputPath).exists()) {
    const child = Bun.spawn([join(base,"target/debug/examples/replay_context"), "--input", inputPath, "--output", outputPath], {stdout:"inherit",stderr:"inherit"});
    if (await child.exited !== 0) throw new Error("候选重放失败");
    if (new Bun.CryptoHasher("sha256").update(await Bun.file(inputPath).bytes()).digest("hex") !== inputHash) throw new Error("重放期间输入变化");
    await Bun.write(meta, JSON.stringify({input_sha256:inputHash}));
  }
  const output = await Bun.file(outputPath).json();
  if (output.length !== baseline.length) throw new Error("重放问题数量不一致");
  const rows = baseline.map(row => {
    const observed = output.find((after: any) => after.id === row.id);
    if (!observed || observed.query !== row.query) throw new Error("重放查询不一致");
    return { id: row.id, query: row.query, kind: row.kind, before: row.observation.implementation_rank,
      after: implementationRank(observed.results, truth[row.id].locations), milliseconds: observed.milliseconds,
      references_verified: observed.results.length };
  });
  const summary = {label, default: paired(rows.filter(row => row.kind === "implemented" && !row.id.startsWith("1086"))),
    php_supplement: paired(rows.filter(row => row.kind === "implemented" && row.id.startsWith("1086"))),rows};
  await Bun.write(join(out,`${label}-summary.json`),JSON.stringify(summary,null,2));
  console.log(JSON.stringify(summary,null,2));
}

if (import.meta.main) await main();
