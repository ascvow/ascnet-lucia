/** 在冻结的召回并集上验证真实 Cross-Encoder；仅用于实验，不修改默认搜索或原项目。 */
import { Database, constants } from "bun:sqlite";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { mkdir } from "node:fs/promises";

/** 保存原始请求与响应以便恢复及审计；失败中止，禁止悄悄退回原排序。 */
async function main() {
  const base = resolve(import.meta.dir, "..");
  const out = join(base, "target/real-projects-2026-09-08");
  const label = Bun.argv[2];
  const model = Bun.argv[3] ?? "BAAI/bge-reranker-v2-m3";
  if (!label || !/^[a-z0-9-]+$/.test(label)) throw new Error("需要独立实验标签");
  const cacheDir = join(out, label);
  await mkdir(cacheDir, { recursive: true, mode: 0o700 });
  const manifest = await Bun.file(join(out, "manifest.json")).json();
  const input = [];
  const timing = [];
  for (const project of manifest.projects) {
    const php = project.id === "1086";
    const prefix = project.id + (php ? "-input6000" : "");
    const configPath = php ? join(project.config, "../codesearch-input6000.toml") : project.config;
    const config = Bun.TOML.parse(await Bun.file(configPath).text()) as any;
    const dbPath = join(php ? join(project.data_dir, "../index-input6000") : project.data_dir, "index.db");
    const wal = Bun.file(dbPath + "-wal");
    if (await wal.exists() && wal.size > 0) throw new Error("存在未合并 WAL，不能读取静态索引");
    const db = new Database(`${pathToFileURL(dbPath)}?immutable=1`, constants.SQLITE_OPEN_READONLY | constants.SQLITE_OPEN_URI);
    const chunks = new Map((db.query(`SELECT c.*, f.path, r.content FROM chunks c JOIN files f ON f.id=c.file_id
      JOIN file_revisions r ON r.id=c.revision_id WHERE c.is_current=1`).all() as any[]).map(row => [row.id, row]));
    const traces = (await Bun.file(join(out, `${prefix}-stages-v1-results.jsonl`)).text()).trim().split("\n").map(line => JSON.parse(line));
    const baseline = await Bun.file(join(out, `${prefix}-stages-v1-results.json`)).json();
    const rows = [];
    for (const row of baseline.rows) {
      const trace = traces.find(trace => trace.query === row.query);
      const candidates = trace.stages.find((stage: any) => stage.stage === "prior").data;
      const documents = candidates.map((candidate: any) => {
        const chunk = chunks.get(candidate.chunk_id);
        if (!chunk) throw new Error("冻结候选版本不存在");
        const content = chunk.kind === "overview" ? chunk.embed_input : Buffer.from(chunk.content).subarray(chunk.start_byte, chunk.end_byte).toString("utf8");
        return Array.from(`${chunk.path}\n${chunk.symbol ?? ""}\n${chunk.signature ?? ""}\n${content}`).slice(0, 6000).join("");
      });
      const request = { model, query: row.query, documents, top_n: documents.length, return_documents: false };
      const fingerprint = new Bun.CryptoHasher("sha256").update(JSON.stringify(request)).digest("hex");
      const cache = Bun.file(join(cacheDir, `${row.id}.json`));
      let saved;
      if (await cache.exists()) {
        saved = await cache.json();
        if (saved.fingerprint !== fingerprint) throw new Error("实验输入变化，请使用新标签");
      } else {
        const key = process.env[config.embedding.api_key_env];
        if (!key) throw new Error("缺少模型凭证环境变量");
        const started = performance.now();
        try {
          const response = await fetch(config.embedding.base_url.replace(/\/$/, "") + "/rerank", {
            method: "POST", headers: { "Content-Type": "application/json", Authorization: `Bearer ${key}` },
            body: JSON.stringify(request), signal: AbortSignal.timeout(120_000),
          });
          if (!response.ok) throw new Error(`HTTP ${response.status}`);
          const result = await response.json();
          saved = { fingerprint, milliseconds: performance.now()-started, request, response: result };
        } catch (error) {
          await Bun.write(join(cacheDir, `${row.id}-failure-${Date.now()}.json`), JSON.stringify({id:row.id, fingerprint,
            milliseconds:performance.now()-started, error_name:error instanceof Error ? error.name : "unknown"}));
          throw error;
        }
        await Bun.write(cache, JSON.stringify(saved));
      }
      const results = saved.response.results;
      if (!Array.isArray(results) || results.length !== candidates.length || new Set(results.map((r: any) => r.index)).size !== candidates.length
        || results.some((r: any) => !Number.isInteger(r.index) || r.index < 0 || r.index >= candidates.length || !Number.isFinite(r.relevance_score))) {
        throw new Error(`${row.id} 重排响应索引或分数不合法`);
      }
      const sorted = results.map((r: any) => ({ ...candidates[r.index], score: r.relevance_score }))
        .sort((a: any,b: any) => b.score-a.score || a.chunk_id-b.chunk_id);
      rows.push({ id: row.id, query: row.query, candidates: sorted.slice(0, 40) });
      timing.push({id:row.id,milliseconds:saved.milliseconds,candidates:candidates.length});
      console.log(`${row.id} 重排 ${candidates.length} 个候选，${Math.round(saved.milliseconds)}ms`);
    }
    input.push({ db: dbPath, config: configPath, rows });
    db.close();
  }
  await Bun.write(join(out, `${label}-input.json`), JSON.stringify(input));
  await Bun.write(join(out, `${label}-timing.json`), JSON.stringify({model,timing},null,2));
}

await main();
