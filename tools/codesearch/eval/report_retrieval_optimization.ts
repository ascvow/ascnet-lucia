/** 汇总单变量实验、同向量审计及真实 MCP 验证；只发布位置和指标，不复制业务源码。 */
import { join, resolve } from "node:path";
import { paired } from "./replay_real_projects";
import { verifyProject } from "./summarize_real_projects";

/** 延迟统计不把未返回结果的请求当作快速成功。 */
function latency(values: number[]) {
  const sorted = [...values].sort((a,b)=>a-b);
  return {median_ms:sorted[Math.floor(sorted.length/2)]??null,p95_ms:sorted[Math.ceil(sorted.length*0.95)-1]??null,max_ms:sorted.at(-1)??null};
}

/** 对照固定锚点版本分离重排收益；最后核验源码与 trace，失败问题仍留在分母。 */
async function main() {
  const base = resolve(import.meta.dir,"..");
  const out = join(base,"target/real-projects-2026-09-08");
  const manifest = await Bun.file(join(out,"manifest.json")).json();
  const truth = await Bun.file(join(base,"eval/real-projects-truth.json")).json();
  const diagnosis = await Bun.file(join(out,"stages-v1-summary.json")).json();
  const hash = (text: string | Uint8Array) => new Bun.CryptoHasher("sha256").update(text).digest("hex");
  if (hash(await Bun.file(join(base,"eval/real-projects-truth.json")).text()) !== diagnosis.truth_sha256
    || hash(await Bun.file(join(base,"eval/real-projects-queries.json")).text()) !== manifest.queries_sha256) throw new Error("冻结问题或真值变化");
  const context = await Bun.file(join(out,"context-v2-summary.json")).json();
  const variants = [];
  for (const label of ["context-v1","context-v2","rerank-bge-v1","rerank-qwen-v2"]) {
    const summary = await Bun.file(join(out,`${label}-summary.json`)).json();
    const rows = summary.rows.map((row: any) => ({...row,before:context.rows.find((r:any)=>r.id===row.id).after}));
    variants.push({label,versus_original:{default:summary.default,php:summary.php_supplement},
      versus_anchor:{default:paired(rows.filter((r:any)=>r.kind==="implemented"&&!r.id.startsWith("1086"))),
        php:paired(rows.filter((r:any)=>r.kind==="implemented"&&r.id.startsWith("1086")))},rows:summary.rows});
  }
  const audit = await Bun.file(join(out,"vector-audit-v1.json")).json();
  const auditRows = diagnosis.rows.filter((row:any)=>row.kind==="implemented").map((row:any)=>{
    const measured = audit.find((r:any)=>r.id===row.id);
    const targets = new Set(row.targets.map((target:any)=>target.id));
    const rank = (list:any[]) => {const index=list.findIndex(([id])=>targets.has(id));return index<0?null:index+1;};
    return {id:row.id,query:row.query,ann_rank:rank(measured.ann),exact_rank:rank(measured.exact),
      frozen_rank:row.ranks.vector,corpus_vectors:measured.exact.length};
  });
  const timing = [];
  for (const label of ["rerank-bge-v1","rerank-qwen-v1"]) {
    const raw = await Bun.file(join(out,`${label}-timing.json`)).json();
    timing.push({label,model:raw.model,...latency(raw.timing.map((row:any)=>row.milliseconds)),
      initial_timeouts:label==="rerank-qwen-v1"?1:0, timeout_ms:label==="rerank-qwen-v1"?120000:null});
  }
  const integrity = [];
  const live = [];
  for (const project of manifest.projects) {
    const repeat = Bun.file(join(out,`${project.id}-repeat-manifest.json`));
    const status = await verifyProject(await repeat.exists()?(await repeat.json()).project:project,manifest.scope);
    if (!status.source_unchanged) throw new Error(`${project.id} 源码变化，停止统计`);
    integrity.push({id:project.id,...status});
    const prefix = project.id + (project.id==="1086"?"-input6000":"");
    const raw = await Bun.file(join(out,`${prefix}-qwen-final-v1-results.json`)).json();
    if (!raw.complete || raw.rows.length!==6) throw new Error(`${project.id} 最终 MCP 未完成`);
    const traces = (await Bun.file(join(out,`${prefix}-qwen-final-v1-results.jsonl`)).text()).trim().split("\n").map(line=>JSON.parse(line));
    for (const row of raw.rows) {
      if (!row.observation.error) {
        const stages = traces.find((trace:any)=>trace.query===row.query)?.stages;
        if (!stages?.some((stage:any)=>stage.stage==="reranked") || !stages?.some((stage:any)=>stage.stage==="rerank_input")) {
          throw new Error(`${row.id} 没有实际重排证据`);
        }
      }
      live.push({id:row.id,query:row.query,kind:truth[row.id].kind,
        before:context.rows.find((r:any)=>r.id===row.id).after,after:row.observation.implementation_rank??null,
        milliseconds:row.observation.latency_ms??null,error:row.observation.error??null,
        references_verified:row.observation.references_verified??0});
    }
  }
  const summary = {date:"2026-09-08",binary_sha256:hash(await Bun.file(join(base,"target/codesearch-quality-v1")).bytes()),
    queries_sha256:manifest.queries_sha256,truth_sha256:diagnosis.truth_sha256,
    input_scope:"原始六个项目；默认 PHP 不完整索引另存，主准确率不包含 PHP 补测",variants,audit:auditRows,timing,integrity,
    live:{default:paired(live.filter(r=>r.kind==="implemented"&&!r.id.startsWith("1086"))),
      php:paired(live.filter(r=>r.kind==="implemented"&&r.id.startsWith("1086"))),
      latency:latency(live.filter(r=>!r.error).map(r=>r.milliseconds)),errors:live.filter(r=>r.error),
      references_verified:live.reduce((sum,row)=>sum+row.references_verified,0),rows:live},
    excluded_runs:["rerank-qwen-v1-output.json 在生产候选未完成时错误使用基线输入，已排除；v2 校验完整输入指纹后重跑",
      "qwen-live-v1 的分类判断错误导致未实际重排，已排除；最终必须存在 rerank_input 与 reranked 证据"],
    limitations:["没有新增未参与调优项目的盲测，不能把这些分数视为泛化准确率","Qwen 初次离线评测发生一次 120 秒超时，恢复后只补跑该题，失败不隐藏",
      "重排实验采用同一锚点版本；第一版合并资格变更未采用","未自动扩充索引、未修改原项目、未修复 PHP 长输入索引失败"]};
  await Bun.write(join(out,"optimization-summary.json"),JSON.stringify(summary,null,2));
  if (Bun.argv.includes("--publish")) await Bun.write(join(base,"reports/retrieval-optimization-2026-09-08.json"),JSON.stringify(summary,null,2));
  console.log(JSON.stringify({live:summary.live.default,php:summary.live.php,latency:summary.live.latency,
    errors:summary.live.errors,refs:summary.live.references_verified,timing},null,2));
}

await main();
