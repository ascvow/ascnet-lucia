/** 真实项目只读预检与索引准备：缓存写到本工具 target，疑似凭证文件整份排除，不输出凭证。 */
import { resolve, join } from "node:path";
import { mkdir } from "node:fs/promises";

/** 本轮预先固定的作者源码范围；不依赖查询或答案位置选择文件。 */
const extensions = ["vue", "js", "ts", "tsx", "jsx", "php", "java", "wxml", "html", "css", "scss"];
const directories = [".git", "node_modules", "target", "dist", "build", "out", ".next", ".nuxt", "vendor", "thinkphp", "uni_modules", "uview-ui", "unpackage", "uploads", "runtime", "logs", "coverage", "ec-canvas", "aegis-mp-sdk"];
const excludes = [...directories.map(name => `**/${name}`), "**/public/assets/libs", "**/addons/*/library", "**/*.min.js", "**/*.min.css", "**/*config.*", "**/config/**", "**/database.php"];

/** 只识别高风险的硬编码凭证形态；它是保守预检，不保证识别所有秘密。 */
function suspicious(source: string): boolean {
  return /\bsk-[A-Za-z0-9_-]{20,}\b|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/.test(source)
    || /(?:api[_-]?key|app[_-]?secret|access[_-]?key|secret[_-]?key|client[_-]?secret|password)\s*['"]?\s*[:=]\s*['"][^'"\r\n]{8,}['"]/i.test(source);
}

/** 返回进程输出及退出码，不把失败的 Git 检查当成干净工作区。 */
async function command(args: string[], cwd?: string) {
  const child = Bun.spawn(args, { cwd, stdout: "pipe", stderr: "ignore" });
  const output = await new Response(child.stdout).text();
  return { output, code: await child.exited };
}

/** 在建索引前记录源文件哈希、Git 状态和排除清单，然后顺序请求已配置的真实模型。 */
async function main() {
  const base = resolve(import.meta.dir, "..");
  const setPath = join(base, "eval/real-projects-queries.json");
  const setText = await Bun.file(setPath).text();
  const set = JSON.parse(setText);
  const out = join(base, "target/real-projects-2026-09-08");
  const binary = resolve(Bun.argv[2] ?? join(base, "target/codesearch-candidate-final"));
  const hash = (text: string | ArrayBuffer) => new Bun.CryptoHasher("sha256").update(text).digest("hex");
  await mkdir(out, { recursive: true });
  const manifestPath = join(out, "manifest.json");
  const existing = await Bun.file(manifestPath).exists();
  const manifest: any = existing ? await Bun.file(manifestPath).json() : { queries_sha256: hash(setText), binary, binary_sha256: hash(await Bun.file(binary).arrayBuffer()), created_at: new Date().toISOString(), scope: { extensions, excludes }, projects: [] };
  if (manifest.queries_sha256 !== hash(setText) || manifest.binary_sha256 !== hash(await Bun.file(binary).arrayBuffer())) throw new Error("查询或二进制已变更，必须新建实验目录");
  for (const project of existing ? [] : set.projects) {
    const args = ["rg", "--files", "--hidden", "-0"];
    for (const extension of extensions) args.push("-g", `*.${extension}`);
    for (const dir of directories) args.push("-g", `!**/${dir}/**`);
    for (const pattern of excludes.slice(directories.length)) args.push("-g", `!${pattern}/**`, "-g", `!${pattern}`);
    args.push(".");
    const listed = await command(args, project.root);
    if (listed.code !== 0) throw new Error(`${project.id}: 文件盘点失败`);
    const files = [];
    const sensitive: string[] = [];
    for (const name of listed.output.split("\0").filter(Boolean).sort()) {
      const path = name.replace(/^\.\//, "");
      const file = Bun.file(join(project.root, path));
      const source = await file.text();
      files.push({ path, bytes: file.size, sha256: hash(source) });
      if (suspicious(source)) sensitive.push(path);
    }
    const git = await command(["git", "status", "--porcelain=v1", "--untracked-files=all"], project.root);
    const head = await command(["git", "rev-parse", "HEAD"], project.root);
    const directory = join(out, project.id);
    await mkdir(directory, { recursive: true });
    const config = join(directory, "codesearch.toml");
    const dataDir = join(directory, "index");
    const escaped = sensitive.map(path => path.replace(/[\\*?\[\]!#]/g, "\\$&"));
    const settings = `[embedding]\nprovider = "openai_compatible"\nbase_url = "https://api.siliconflow.cn/v1"\nmodel = "BAAI/bge-m3"\napi_key_env = "SILICONFLOW_API_KEY"\ndimensions = 1024\nsend_dimensions = false\nbatch_size = 16\nmax_concurrency = 1\nmax_retries = 5\n\n[index]\nrefresh_on_search = false\ninclude = ${JSON.stringify(extensions.map(extension => `**/*.${extension}`))}\nexclude = ${JSON.stringify([...excludes, ...escaped])}\n`;
    await Bun.write(config, settings);
    const entry = { id: project.id, root: project.root, config, data_dir: dataDir, files, sensitive_excluded: sensitive, git_status: git.code === 0 ? git.output : null, head: head.code === 0 ? head.output.trim() : null, index: null as any };
    manifest.projects.push(entry);
    await Bun.write(join(out, "manifest.json"), JSON.stringify(manifest, null, 2));
    console.log(JSON.stringify({ stage: "preflight", project: project.id, source_files: files.length, sensitive_excluded: sensitive.length, git_available: git.code === 0 }));
  }
  for (const project of manifest.projects) {
    // 已尝试的项目保留原始结果，断点继续不能覆盖失败记录或初始源码快照。
    if (project.index) continue;
    console.log(JSON.stringify({ stage: "index_start", project: project.id }));
    const child = Bun.spawn([binary, "index", "--root", project.root, "--config", project.config, "--data-dir", project.data_dir, "--json"], { stdout: "pipe", stderr: "inherit" });
    const response = await new Response(child.stdout).text();
    if (await child.exited !== 0) throw new Error(`${project.id}: 索引进程失败`);
    project.index = JSON.parse(response);
    await Bun.write(join(out, "manifest.json"), JSON.stringify(manifest, null, 2));
    console.log(JSON.stringify({ stage: "indexed", project: project.id, ...project.index }));
    if (project.index.degraded || project.index.embeddings_pending || project.index.embeddings_failed) console.log(JSON.stringify({ stage: "index_incomplete", project: project.id }));
  }
}

await main();
