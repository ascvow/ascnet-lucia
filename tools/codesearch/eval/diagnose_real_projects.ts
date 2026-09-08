/** 用同次调用的阶段快照定位损失；真值只在离线读取 SQLite 时使用，不进入搜索请求。 */
import { Database, constants } from "bun:sqlite";
import { pathToFileURL } from "node:url";
import { join, resolve } from "node:path";
import { verifyProject } from "./summarize_real_projects";

type Span = { path: string; start_line: number; end_line: number; start_byte: number; end_byte: number; revision_id: number };
type Location = { path: string; start_line: number; end_line: number; needles: string[] };
type Ranks = Record<string, number | null>;

/** 范围和实际返回字节必须同时覆盖关键语句，不能用同文件或容器名字冒充实现命中。 */
export function covers(span: Span, location: Location, source: string): boolean {
  if (span.path !== location.path || span.start_line > location.start_line || span.end_line < location.end_line) return false;
  const body = Buffer.from(source).subarray(span.start_byte, span.end_byte).toString("utf8");
  return location.needles.every(needle => body.includes(needle));
}

/** 从离输出最近的有效证据倒推损失；扩展和合并可能补回此前未覆盖的语句。 */
export function loss(ranks: Ranks, indexed: boolean, ready: boolean): string {
  if (ranks.final != null) return "hit";
  if (ranks.budget_before != null) return "context_clipping";
  if (ranks.ordered != null) return "result_selection";
  if (ranks.expanded != null) return "context_merge";
  if (ranks.selected != null) return "context_expansion";
  if (ranks.candidates != null) return "current_version_filter";
  if (ranks.fused != null) return "candidate_limit";
  if (!indexed) return "index_coverage";
  if (!ready) return "embedding_unavailable";
  return "recall_miss";
}

/** 复核冻结问题、真值和所有源码；记录开关构建哈希，禁止把漂移语料计入对照。 */
async function main() {
  const base = resolve(import.meta.dir, "..");
  const out = join(base, "target/real-projects-2026-09-08");
  const label = process.argv.find(arg => arg.startsWith("--label="))?.slice(8) ?? "stages-v1";
  if (!/^[a-zA-Z0-9-]+$/.test(label)) throw new Error("标签不合法");
  const binaryArgument = process.argv.find(arg => arg.startsWith("--binary="))?.slice(9);
  if (label !== "stages-v1" && !binaryArgument) throw new Error("新实验必须指定对应的冻结 --binary 路径");
  const binary = binaryArgument ?? join(base, "target/codesearch-diagnostics-stages-v1");
  const manifest = await Bun.file(join(out, "manifest.json")).json();
  const oldSummary = await Bun.file(join(out, "summary.json")).json();
  const hash = (value: string | Uint8Array) => new Bun.CryptoHasher("sha256").update(value).digest("hex");
  const queryText = await Bun.file(join(base, "eval/real-projects-queries.json")).text();
  const truthText = await Bun.file(join(base, "eval/real-projects-truth.json")).text();
  if (hash(queryText) !== manifest.queries_sha256 || hash(truthText) !== oldSummary.truth_sha256) throw new Error("冻结问题或真值变化");
  const truth: Record<string, { kind: string; locations: Location[] }> = JSON.parse(truthText);
  const integrity = [];
  const snapshots = new Map<string, any>();
  for (const project of manifest.projects) {
    const repeatFile = Bun.file(join(out, `${project.id}-repeat-manifest.json`));
    const snapshot = await repeatFile.exists() ? (await repeatFile.json()).project : project;
    snapshots.set(project.id, snapshot);
    const status = await verifyProject(snapshot, manifest.scope);
    integrity.push({ id: project.id, ...status });
    if (!status.source_unchanged) throw new Error(`${project.id} 源码变化，停止评分`);
  }
  if (process.argv.includes("--preflight")) { console.log(JSON.stringify(integrity, null, 2)); return; }
  const rows: any[] = [];
  const comparisons: any[] = [];
  for (const project of manifest.projects) {
    const php = project.id === "1086";
    const prefix = `${project.id}${php ? "-input6000" : ""}`;
    const result = await Bun.file(join(out, `${prefix}-${label}-results.json`)).json();
    const traces = (await Bun.file(join(out, `${prefix}-${label}-results.jsonl`)).text()).trim().split("\n").map(line => JSON.parse(line));
    const baselineName = php ? "1086-input6000-results.json" : `${project.id}-${project.id === "1507" ? "stable" : "verified"}-results.json`;
    const baseline = await Bun.file(join(out, baselineName)).json();
    if (!result.complete || result.rows.length !== 6 || traces.length !== 6) throw new Error(`${project.id} 诊断未完成`);
    // macOS 的只读 WAL 打开依赖辅助文件；服务退出后仅允许读取已完整 checkpoint 的静态库。
    const dbPath = join(php ? join(project.data_dir, "../index-input6000") : project.data_dir, "index.db");
    const wal = Bun.file(`${dbPath}-wal`);
    if (await wal.exists() && wal.size > 0) throw new Error(`${project.id} 存在 WAL，请先正常关闭索引服务`);
    const dbHash = hash(await Bun.file(dbPath).bytes());
    const db = new Database(`${pathToFileURL(dbPath)}?immutable=1`, constants.SQLITE_OPEN_READONLY | constants.SQLITE_OPEN_URI);
    const chunks = db.query(`SELECT c.*, f.path, r.file_hash FROM chunks c JOIN files f ON f.id=c.file_id
      JOIN file_revisions r ON r.id=c.revision_id WHERE c.is_current=1`).all() as any[];
    const revisions = new Map((db.query("SELECT id, content FROM file_revisions").all() as any[]).map(row => [row.id, row.content as string]));
    const sourceHashes = new Map(snapshots.get(project.id).files.map((file: any) => [file.path, file.sha256]));
    for (const file of db.query("SELECT f.path, r.content FROM files f JOIN file_revisions r ON r.id=f.current_revision_id WHERE f.state='active'").all() as any[]) {
      if (sourceHashes.get(file.path) !== hash(file.content)) throw new Error(`${project.id} ${file.path} 索引快照与冻结源码不一致`);
    }
    const missingVectors = db.query(`SELECT count(*) AS count FROM chunks c LEFT JOIN embedding_cache e
      ON c.cache_key=e.cache_key AND c.embedding_space_id=e.space_id WHERE c.is_current=1 AND (e.cache_key IS NULL OR length(e.vector) != e.dimensions*4)`).get() as {count:number};
    if (missingVectors.count > 0) throw new Error(`${project.id} 向量缓存缺失或长度错误`);
    const byId = new Map(chunks.map(chunk => [chunk.id, chunk]));
    const state = db.query("SELECT embed_state, count(*) AS count FROM chunks WHERE is_current=1 GROUP BY embed_state").all();
    if (chunks.some(chunk => chunk.embed_state !== "ready")) throw new Error(`${project.id} 索引不完整`);
    const rank = (spans: any[], locations: Location[]) => {
      const i = spans.findIndex(span => span && locations.some(location => covers(span, location, revisions.get(span.revision_id) ?? "")));
      return i < 0 ? null : i + 1;
    };
    for (const row of result.rows) {
      if (row.observation.error) throw new Error(`${row.id} 搜索或引用验证失败: ${row.observation.error}`);
      const trace = traces.find(trace => trace.query === row.query);
      if (!trace || traces.filter(trace => trace.query === row.query).length !== 1) throw new Error(`${row.id} 同调用对应关系不唯一`);
      const response = row.observation.response;
      if (trace.catalog_epoch !== response.catalog_epoch) throw new Error("诊断与响应版本不一致");
      const previousObservation = baseline.rows.find((old: any) => old.id === row.id && old.round === 1).observation;
      const previous = previousObservation.response;
      // 服务重启扫描会推进目录 epoch；比较所有其他字段，同时独立记录两端 epoch。
      const { catalog_epoch: beforeEpoch, ...before } = previous;
      const { catalog_epoch: afterEpoch, ...after } = response;
      const identical = JSON.stringify(before) === JSON.stringify(after);
      comparisons.push({ id: row.id, response_identical_except_epoch: identical, before_epoch: beforeEpoch, after_epoch: afterEpoch,
        file_rank_unchanged: previousObservation.file_rank === row.observation.file_rank,
        implementation_rank_unchanged: previousObservation.implementation_rank === row.observation.implementation_rank,
        references_verified: row.observation.references_verified });
      // 真实模型重复调用可能变化，不能删掉差异或声称全响应相同；归因只使用本次调用记录。
      const stages = Object.fromEntries(trace.stages.filter((stage: any) => stage.stage !== "budget").map((stage: any) => [stage.stage, stage.data]));
      const target = truth[row.id];
      const indexed = chunks.filter(chunk => target.locations.some(location => covers(chunk, location, revisions.get(chunk.revision_id) ?? "")));
      const candidates = (name: string) => (stages[name] ?? []).map((candidate: any) => byId.get(candidate.chunk_id));
      const ranks: Ranks = {};
      for (const name of ["vector", "fts", "symbol", "path", "literal"]) {
        const list = (stages.recall ?? []).find((list: any) => list.source === name);
        ranks[name] = rank((list?.ids ?? []).map((id: number) => byId.get(id)), target.locations);
      }
      for (const name of ["fused", "prior", "candidates"]) ranks[name] = rank(candidates(name), target.locations);
      for (const name of ["selected", "expanded", "ordered"]) ranks[name] = rank(stages[name] ?? [], target.locations);
      const budgets = trace.stages.filter((stage: any) => stage.stage === "budget").map((stage: any) => stage.data);
      ranks.budget_before = rank(budgets.map((budget: any) => budget.selection), target.locations);
      ranks.budget_after = rank(budgets.map((budget: any) => ({ ...budget.selection, end_line: budget.end_line, end_byte: budget.end_byte })), target.locations);
      ranks.final = row.observation.implementation_rank;
      if (ranks.budget_after !== ranks.final) throw new Error(`${row.id} 阶段字节判定与实际返回判定不一致`);
      rows.push({ id: row.id, query: row.query, kind: target.kind, supplement: php, ranks,
        loss: target.kind === "implemented" ? loss(ranks, indexed.length > 0, indexed.some(chunk => chunk.embed_state === "ready")) : "not_scored",
        targets: indexed.map(chunk => ({ id: chunk.id, path: chunk.path, start_line: chunk.start_line, end_line: chunk.end_line,
          kind: chunk.kind, embed_state: chunk.embed_state, embed_input_truncated: chunk.embed_input_truncated,
          embedding_contains_needles: target.locations.some(location => location.path === chunk.path && location.needles.every(needle => chunk.embed_input.includes(needle))) })),
        budget_windows: budgets.filter((budget: any) => rank([budget.selection], target.locations) != null).map((budget: any) => ({
          path: budget.selection.path, before_start: budget.selection.start_line, before_end: budget.selection.end_line,
          after_end: budget.end_line, result_rank: budget.result_index + 1, content_budget: budget.content_budget })),
        query_plan: stages.query_plan, index_state: state });
    }
    db.close();
    if (hash(await Bun.file(dbPath).bytes()) !== dbHash || (await wal.exists() && wal.size > 0)) throw new Error(`${project.id} 读取期间索引变化`);
  }
  const counts = (supplement: boolean) => {
    const counts: Record<string, number> = {};
    for (const row of rows.filter(row => row.kind === "implemented" && row.supplement === supplement)) counts[row.loss] = (counts[row.loss] ?? 0) + 1;
    return counts;
  };
  const summary = { schema: 1, label, queries_sha256: hash(queryText), truth_sha256: hash(truthText),
    diagnostic_binary_sha256: hash(await Bun.file(binary).bytes()),
    baseline_binary_sha256: manifest.binary_sha256, integrity, comparisons,
    default_losses: counts(false), php_supplement_losses: counts(true), rows };
  await Bun.write(join(out, `${label}-summary.json`), JSON.stringify(summary, null, 2));
  if (process.argv.includes("--publish")) {
    const kinds: Record<string, string> = { implemented: "已实现", unimplemented: "未实现", partial: "部分实现", ambiguous: "歧义", unconfirmed: "未确认" };
    const losses: Record<string, string> = { hit: "命中", recall_miss: "未召回", candidate_limit: "候选上限", result_selection: "输出选择", context_clipping: "正文裁剪", not_scored: "不计分" };
    const table = ["# 真实项目逐题损失表", "", "由冻结问题与同次调用记录生成；1086 为独立的输入上限 6000 补测。未实现等条目的名次只表示标注位置，不代表功能存在。", "",
      "名次从 1 开始，`-` 表示没有覆盖关键语句。融合列为先验修正前，候选列为修正并截断后；组装列为预算裁剪前。", "",
      "| 问题 | 原始搜索文本 | 真值类别 | 向量 | 全文 | 融合 | 候选 | 组装 | 返回 | 损失阶段 |",
      "|---|---|---|---:|---:|---:|---:|---:|---:|---|",
      ...rows.map(row => `| ${row.id} | ${row.query} | ${kinds[row.kind]} | ${["vector", "fts", "fused", "candidates", "ordered", "final"].map(key => row.ranks[key] ?? "-").join(" | ")} | ${losses[row.loss] ?? row.loss} |`), ""];
    await Bun.write(join(base, `reports/real-projects-${label}-2026-09-08.json`), JSON.stringify(summary, null, 2));
    await Bun.write(join(base, `reports/real-projects-${label}-cases-2026-09-08.md`), table.join("\n"));
  }
  console.log(JSON.stringify({ default_losses: summary.default_losses, php_supplement_losses: summary.php_supplement_losses,
    responses_identical: comparisons.filter(row => row.response_identical_except_epoch).length,
    ranks_unchanged: comparisons.filter(row => row.file_rank_unchanged && row.implementation_rank_unchanged).length,
    references_verified: comparisons.reduce((sum, row) => sum + row.references_verified, 0) }, null, 2));
}

if (import.meta.main) await main();
