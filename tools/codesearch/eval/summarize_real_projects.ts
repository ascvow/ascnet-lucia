/** 汇总真实 MCP 结果并复核源码未变；不复制业务源码到版本化报告。 */
import { join, resolve } from "node:path";

/** 仅首轮用于准确率，重复轮次只报告稳定性与交互延迟。 */
export function metrics(rows: any[]) {
  const first = rows.filter(row => row.round === 1 && row.kind === "implemented");
  const rate = (field: string, k: number) => first.length
    ? first.filter(row => !row.observation.error && row.observation[field] != null && row.observation[field] <= k).length / first.length : null;
  const times = rows.filter(row => !row.observation.error).map(row => row.observation.latency_ms).sort((a, b) => a - b);
  return {
    implemented_queries: first.length,
    file_top1: rate("file_rank", 1), file_top5: rate("file_rank", 5), file_top10: rate("file_rank", 10),
    implementation_top1: rate("implementation_rank", 1), implementation_top3: rate("implementation_rank", 3),
    implementation_top5: rate("implementation_rank", 5), implementation_top10: rate("implementation_rank", 10),
    mrr: first.length ? first.reduce((sum, row) => sum + (!row.observation.error && row.observation.implementation_rank ? 1 / row.observation.implementation_rank : 0), 0) / first.length : null,
    request_errors: rows.filter(row => row.observation.error).length,
    references_verified: rows.reduce((sum, row) => sum + (row.observation.references_verified || 0), 0),
    reference_line_expansions: rows.reduce((sum, row) => sum + (row.observation.reference_line_expansions || 0), 0),
    median_ms: times[Math.floor(times.length / 2)] ?? null, p95_ms: times[Math.ceil(times.length * 0.95) - 1] ?? null,
  };
}

/** 逐文件校验初始清单，并用 Git 状态辅助发现新增、删除和目录外的其他改动。 */
async function verifyProject(project: any, scope: any) {
  const hash = (text: string) => new Bun.CryptoHasher("sha256").update(text).digest("hex");
  const changed: string[] = [];
  for (const file of project.files) {
    const path = join(project.root, file.path);
    if (!await Bun.file(path).exists() || hash(await Bun.file(path).text()) !== file.sha256) changed.push(file.path);
  }
  const args = ["rg", "--files", "--hidden", "-0"];
  for (const extension of scope.extensions) args.push("-g", `*.${extension}`);
  for (const pattern of scope.excludes) args.push("-g", `!${pattern}`, "-g", `!${pattern}/**`);
  args.push(".");
  const scanner = Bun.spawn(args, { cwd: project.root, stdout: "pipe", stderr: "ignore" });
  const listed = await new Response(scanner.stdout).text();
  if (await scanner.exited !== 0) throw new Error(`${project.id} 无法复核文件列表`);
  const before = new Set(project.files.map((file: any) => file.path));
  const added = listed.split("\0").filter(Boolean).map(path => path.replace(/^\.\//, "")).filter(path => !before.has(path));
  const git = Bun.spawn(["git", "status", "--porcelain=v1", "--untracked-files=all"], { cwd: project.root, stdout: "pipe", stderr: "ignore" });
  const status = await new Response(git.stdout).text();
  const gitAvailable = await git.exited === 0;
  return { changed, added, git_available: gitAvailable, git_status_unchanged: gitAvailable ? status === project.git_status : null,
    source_unchanged: changed.length === 0 && added.length === 0 };
}

/** 输出固定字段的摘要与逐查询位置，完整原始片段仍留在 target 中。 */
async function main() {
  const base = resolve(import.meta.dir, "..");
  const out = join(base, "target/real-projects-2026-09-08");
  const manifest = await Bun.file(join(out, "manifest.json")).json();
  const finalStatuses = [];
  for (const project of manifest.projects) {
    const child = Bun.spawn([manifest.binary, "status", "--root", project.root, "--config", project.config,
      "--data-dir", project.data_dir, "--json"], { stdout: "pipe", stderr: "ignore" });
    const status = await new Response(child.stdout).json();
    if (await child.exited !== 0) throw new Error(`${project.id} 无法核对最终索引状态`);
    finalStatuses.push({ id: project.id, status });
  }
  await Bun.write(join(out, "final-status.json"), JSON.stringify(finalStatuses, null, 2));
  const queriesText = await Bun.file(join(base, "eval/real-projects-queries.json")).text();
  const truthText = await Bun.file(join(base, "eval/real-projects-truth.json")).text();
  const sha = (text: string) => new Bun.CryptoHasher("sha256").update(text).digest("hex");
  if (sha(queriesText) !== manifest.queries_sha256) throw new Error("查询集已被改写");
  const projects = [];
  let validRows: any[] = [];
  let incompleteRows: any[] = [];
  for (const project of manifest.projects) {
    const repeatFile = Bun.file(join(out, `${project.id}-repeat-manifest.json`));
    const repeat = await repeatFile.exists() ? await repeatFile.json() : null;
    const result = await Bun.file(join(out, `${project.id}-${repeat ? "stable" : "verified"}-results.json`)).json();
    if (!result.complete || result.rows.length !== 18) throw new Error(`${project.id} 评测未完成`);
    const integrity = await verifyProject(repeat?.project ?? project, manifest.scope);
    if (!integrity.source_unchanged) throw new Error(`${project.id} 源码变化，结果不可直接计入统计`);
    if (result.index_complete) validRows.push(...result.rows); else incompleteRows.push(...result.rows);
    const first = result.rows.filter((row: any) => row.round === 1);
    projects.push({ id: project.id, root: project.root, head: project.head, index: project.index, index_complete: result.index_complete,
      final_status: finalStatuses.find((entry: any) => entry.id === project.id)?.status,
      repeat_reason: repeat?.reason ?? null, external_changes_before_repeat: repeat?.changed ?? [],
      sensitive_excluded: project.sensitive_excluded, integrity, metrics: metrics(result.rows),
      queries: first.map((row: any) => ({ id: row.id, query: row.query, kind: row.kind, note: row.note,
        observations: result.rows.filter((other: any) => other.id === row.id).map((other: any) => ({ round: other.round,
          file_rank: other.observation.file_rank ?? null, implementation_rank: other.observation.implementation_rank ?? null,
          error: other.observation.error ?? null, latency_ms: other.observation.latency_ms ?? null })),
        results: (row.observation.response?.results || []).map((hit: any) => ({ path: hit.path, start_line: hit.start_line, end_line: hit.end_line,
          symbol: hit.symbol, truncated: hit.truncated, file_hash: hit.file_hash, matched_by: hit.matched_by })) })) });
  }
  const supplement = await Bun.file(join(out, "1086-input6000-results.json")).json();
  if (!supplement.complete || supplement.rows.length !== 18) throw new Error("PHP 补测未完成");
  const summary = { date: "2026-09-08", binary_sha256: manifest.binary_sha256, queries_sha256: sha(queriesText), truth_sha256: sha(truthText),
    model: "BAAI/bge-m3", request: { tool: "search_context", fields: ["query"], default_max_results: 10, default_token_budget: 6000 },
    source_scope: manifest.scope, default_complete_index_metrics: metrics(validRows), default_incomplete_index_metrics: metrics(incompleteRows),
    php_supplement: { max_input_chars: 6000, index_complete: supplement.index_complete, index: supplement.index, metrics: metrics(supplement.rows),
      queries: supplement.rows.filter((row: any) => row.round === 1).map((row: any) => ({id:row.id, query:row.query, kind:row.kind,
        file_rank:row.observation.file_rank, implementation_rank:row.observation.implementation_rank, error:row.observation.error ?? null})) },
    projects };
  await Bun.write(join(out, "summary.json"), JSON.stringify(summary, null, 2));
  console.log(JSON.stringify({ metrics: summary.default_complete_index_metrics, php_supplement: summary.php_supplement,
    integrity: projects.map(project => ({id: project.id, ...project.integrity})) }, null, 2));
}

if (import.meta.main) await main();
