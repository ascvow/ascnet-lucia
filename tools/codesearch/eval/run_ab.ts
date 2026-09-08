/**
 * 固定语料的修改前后真实模型评测。用 Bun 执行，分别建立索引，交错查询并保存逐条证据。
 * 必须显式指定模型配置；错误、降级、路径标签失效会中止，不把未运行计入成功。
 */
import { resolve, join, relative } from "node:path";
import { mkdir } from "node:fs/promises";

/** 人工真值；expect_text 可把文件级命中收紧为实际代码位置命中。 */
interface Case {
  query: string;
  expect_paths: string[];
  expect_text?: string;
  language?: string;
}

/** CLI 返回的最小原始代码契约。 */
interface Hit {
  path: string;
  content: string;
  start_line: number;
  end_line: number;
  symbol?: string;
  code_ref: string;
}

/** 单次查询的原始响应、耗时和有效名次；失败单独记录。 */
interface Observation {
  rank: number | null;
  milliseconds: number;
  results: Hit[];
  error?: string;
}

/** 一条查询的配对观测，多轮观测用于报告网络波动。 */
interface Pair {
  query: string;
  baseline: Observation[];
  candidate: Observation[];
}

/** 计算双侧配对符号检验；只比较每条查询首轮名次，排除平局。 */
export function signTest(wins: number, losses: number): number {
  const n = wins + losses;
  if (!n) return 1;
  let term = 2 ** -n;
  let probability = term;
  for (let k = 1; k <= Math.min(wins, losses); k++) {
    term *= (n - k + 1) / k;
    probability += term;
  }
  return Math.min(1, 2 * probability);
}

/** 汇总文件及位置命中率；延迟包含 CLI 启动、读取索引与远程查询，不等于纯模型耗时。 */
export function summarize(rows: Observation[]) {
  const times = rows.map(row => row.milliseconds).sort((a, b) => a - b);
  return {
    count: rows.length,
    failures: rows.filter(row => row.error).length,
    recall1: rows.filter(row => row.rank !== null && row.rank <= 1).length / rows.length,
    recall3: rows.filter(row => row.rank !== null && row.rank <= 3).length / rows.length,
    recall5: rows.filter(row => row.rank !== null && row.rank <= 5).length / rows.length,
    mrr: rows.reduce((sum, row) => sum + (row.rank ? 1 / row.rank : 0), 0) / rows.length,
    median_ms: times[Math.floor(times.length / 2)],
    p95_ms: times[Math.ceil(times.length * 0.95) - 1],
  };
}

/** 运行 CLI 并解析 JSON；非零退出或非法响应都作为评测错误，不悄悄回退。 */
async function cli(binary: string, args: string[]) {
  const child = Bun.spawn([binary, ...args, "--json"], { stdout: "pipe", stderr: "inherit" });
  const text = await new Response(child.stdout).text();
  if (await child.exited !== 0) throw new Error(`CLI 执行失败: ${binary} ${args[0]}`);
  return JSON.parse(text);
}

/** 校验真值与源码，在两套独立索引上执行相同查询并输出可审查报告。 */
async function main() {
  const args = Bun.argv.slice(2);
  const flags = new Map<string, string>();
  for (let i = 0; i < args.length; i += 2) {
    if (!args[i + 1] || !["--baseline", "--candidate", "--root", "--queries", "--config", "--out", "--rounds"].includes(args[i])) {
      throw new Error("参数必须为 --baseline/--candidate/--root/--queries/--config/--out/--rounds 与对应值");
    }
    flags.set(args[i], args[i + 1]);
  }
  const required = (name: string) => {
    const value = flags.get(name);
    if (!value) throw new Error(`缺少 ${name}`);
    return resolve(value);
  };
  const root = required("--root");
  const output = required("--out");
  const config = required("--config");
  const queryPath = required("--queries");
  const binaries = { baseline: required("--baseline"), candidate: required("--candidate") };
  const sha256 = async (path: string) => new Bun.CryptoHasher("sha256").update(await Bun.file(path).arrayBuffer()).digest("hex");
  const fingerprints = {
    baseline: await sha256(binaries.baseline), candidate: await sha256(binaries.candidate),
    queries: await sha256(queryPath), config: await sha256(config),
  };
  const rounds = Number(flags.get("--rounds") ?? "3");
  if (!Number.isInteger(rounds) || rounds < 1 || rounds > 20) throw new Error("rounds 必须在 1..20 之间");
  const set = await Bun.file(queryPath).json() as { queries: Case[] };
  if (!Array.isArray(set.queries) || !set.queries.length) throw new Error("查询集为空或格式错误");
  const sources = new Map<string, string>();
  for (const item of set.queries) {
    if (!item.query?.trim() || !item.expect_paths?.length) throw new Error("查询或真值路径为空");
    for (const path of item.expect_paths) {
      if (relative(root, resolve(root, path)).startsWith("..")) throw new Error("真值路径越界");
      const source = await Bun.file(join(root, path)).text();
      if (item.expect_text && !source.includes(item.expect_text)) throw new Error(`真值已失效: ${path}: ${item.expect_text}`);
      sources.set(path, source);
    }
  }
  await mkdir(output, { recursive: true });
  const pairs: Pair[] = set.queries.map(item => ({ query: item.query, baseline: [], candidate: [] }));
  const indexes: Record<string, unknown> = {};
  const common = (side: keyof typeof binaries) => ["--root", root, "--config", config, "--data-dir", join(output, side)];
  for (const side of ["baseline", "candidate"] as const) {
    const index = await cli(binaries[side], ["index", ...common(side)]);
    indexes[side] = index;
    if (index.degraded || index.embeddings_pending || index.embeddings_failed) throw new Error(`${side} 索引降级或向量不完整`);
  }
  const save = async () => {
    const ranks = (side: "baseline" | "candidate") => pairs.map(pair => pair[side][0]).filter(Boolean);
    let wins = 0, losses = 0;
    for (const pair of pairs) {
      if (!pair.baseline[0] || !pair.candidate[0]) continue;
      const delta = (pair.baseline[0].rank ?? Infinity) - (pair.candidate[0].rank ?? Infinity);
      if (delta > 0) wins++;
      if (delta < 0) losses++;
    }
    const metrics = {
      baseline: summarize(ranks("baseline")), candidate: summarize(ranks("candidate")), wins, losses, sign_test_p: signTest(wins, losses),
      all_rounds: { baseline: summarize(pairs.flatMap(pair => pair.baseline)), candidate: summarize(pairs.flatMap(pair => pair.candidate)) },
    };
    const report = { root, queryPath, config, binaries, fingerprints, rounds, indexes, metrics, pairs,
      complete: pairs.every(pair => pair.baseline.length === rounds && pair.candidate.length === rounds
        && [...pair.baseline, ...pair.candidate].every(row => !row.error)) };
    await Bun.write(join(output, "report.json"), JSON.stringify(report, null, 2));
    return metrics;
  };
  for (let round = 0; round < rounds; round++) {
    for (let i = 0; i < set.queries.length; i++) {
      const item = set.queries[i];
      const order = (round + i) % 2 ? ["candidate", "baseline"] as const : ["baseline", "candidate"] as const;
      for (const side of order) {
        const started = performance.now();
        try {
          const response = await cli(binaries[side], ["search", ...common(side), "--query", item.query,
            "--max-results", "10", "--token-budget", "16000", "--exclude", "eval/**", "--exclude", "reports/**",
            "--exclude", relative(root, queryPath)]);
          if (response.status.degraded || !response.status.vector_search_used) throw new Error("未实际执行完整语义检索");
          const results = response.results as Hit[];
          for (const hit of results) {
            if (!sources.has(hit.path)) sources.set(hit.path, await Bun.file(join(root, hit.path)).text());
            const lines = sources.get(hit.path)!.split("\n");
            if (hit.start_line < 1 || hit.end_line < hit.start_line || hit.end_line > lines.length) throw new Error(`返回行号越界: ${hit.path}`);
          }
          const index = results.findIndex(hit => item.expect_paths.includes(hit.path)
            && (!item.expect_text || (hit.content.includes(item.expect_text)
              && sources.get(hit.path)!.split("\n").slice(hit.start_line - 1, hit.end_line).join("\n").includes(item.expect_text))));
          pairs[i][side].push({ rank: index < 0 ? null : index + 1, milliseconds: performance.now() - started, results });
        } catch (error) {
          pairs[i][side].push({ rank: null, milliseconds: performance.now() - started, results: [], error: String(error) });
          await save();
          throw error;
        }
      }
      await save();
      console.log(`${round + 1}/${rounds} ${i + 1}/${set.queries.length}: ${pairs[i].baseline[round].rank ?? "未命中"} -> ${pairs[i].candidate[round].rank ?? "未命中"} ${item.query}`);
    }
  }
  console.log(JSON.stringify(await save(), null, 2));
}

if (import.meta.main) await main();
