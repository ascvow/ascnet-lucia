import { mkdir, rm } from 'node:fs/promises'
import { join } from 'node:path'
import { zipSync, type Zippable } from 'fflate'
import { loadOfficialPluginCatalog, type OfficialPluginBundle } from './official-plugins'

/** plugin.toml 中打包需要读取的稳定字段。 */
interface PluginManifestDocument {
  /** 插件身份、版本、ABI 和 WASM 路径。 */
  plugin: {
    id: string
    name: string
    version: string
    api_version: string
    wasm: string
    description?: string
  }
  /** 必需或可选的插件依赖。 */
  dependencies?: Array<{ id: string; version?: string; optional?: boolean }>
}

/** Registry 输出中的单个插件版本。 */
interface RegistryVersionDocument {
  /** 独立插件版本。 */
  version: string
  /** 插件 ABI 版本。 */
  api_version: string
  /** 不可变 GitHub Release 资产引用。 */
  github: { owner: string; repository: string; tag: string; asset: string }
  /** ZIP 内容的 SHA-256。 */
  sha256: string
  /** 从 plugin.toml 转换的依赖约束。 */
  dependencies: Array<{ name: string; requirement: string; optional: boolean }>
}

/** 读取插件 manifest，并保证清单 ID 与 manifest 身份一致。 */
async function readManifest(
  root: string,
  plugin: OfficialPluginBundle,
): Promise<PluginManifestDocument> {
  const path = join(root, plugin.directory, 'plugin.toml')
  const manifest = Bun.TOML.parse(await Bun.file(path).text()) as unknown as PluginManifestDocument
  if (manifest.plugin?.id !== plugin.id) {
    throw new Error(`官方清单 ID 与 manifest 不一致：${plugin.id}`)
  }
  if (!manifest.plugin.version || !manifest.plugin.api_version || !manifest.plugin.wasm) {
    throw new Error(`官方插件 manifest 字段不完整：${plugin.id}`)
  }
  if (!plugin.files.includes(manifest.plugin.wasm)) {
    throw new Error(`官方插件清单未包含 manifest WASM：${plugin.id}`)
  }
  return manifest
}

/** 读取 bundle 文件并生成路径稳定、时间戳固定的 ZIP。 */
async function createArchive(root: string, plugin: OfficialPluginBundle): Promise<Uint8Array> {
  const entries: Zippable = {}
  const archiveTimestamp = new Date(1980, 0, 1)
  for (const relativePath of [...plugin.files].sort()) {
    const file = Bun.file(join(root, plugin.directory, relativePath))
    if (!(await file.exists())) {
      throw new Error(`官方插件文件不存在：${plugin.id}/${relativePath}`)
    }
    entries[relativePath] = [new Uint8Array(await file.arrayBuffer()), { mtime: archiveTimestamp }]
  }
  return zipSync(entries, { level: 9, mtime: archiveTimestamp })
}

/** 为每个官方插件生成独立 ZIP、SHA-256 和 Registry 索引。 */
async function main(): Promise<void> {
  const root = join(import.meta.dir, '..')
  const output = join(root, 'dist', 'plugin-release')
  const catalog = await loadOfficialPluginCatalog()
  await rm(output, { recursive: true, force: true })
  await mkdir(output, { recursive: true })

  const packages: Record<
    string,
    { description: string; publisher: string; official: true; versions: RegistryVersionDocument[] }
  > = {}
  for (const plugin of catalog.plugins) {
    const manifest = await readManifest(root, plugin)
    const archive = await createArchive(root, plugin)
    const asset = `lucia-plugin-${plugin.id}-${manifest.plugin.version}.zip`
    const hasher = new Bun.CryptoHasher('sha256')
    hasher.update(archive)
    const sha256 = hasher.digest('hex')
    await Bun.write(join(output, asset), archive)
    await Bun.write(join(output, `${asset}.sha256`), `${sha256}  ${asset}\n`)
    packages[plugin.id] = {
      description: manifest.plugin.description ?? manifest.plugin.name,
      publisher: catalog.publisher,
      official: true,
      versions: [
        {
          version: manifest.plugin.version,
          api_version: manifest.plugin.api_version,
          github: { ...catalog.release, asset },
          sha256,
          dependencies: (manifest.dependencies ?? []).map((dependency) => ({
            name: dependency.id,
            requirement: dependency.version ?? '*',
            optional: dependency.optional ?? false,
          })),
        },
      ],
    }
  }
  await Bun.write(
    join(output, 'registry.json'),
    `${JSON.stringify({ schema_version: 1, packages }, null, 2)}\n`,
  )
  console.log(`已生成官方插件 Release 资产：${output}`)
}

await main()
