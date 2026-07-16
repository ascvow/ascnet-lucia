import { chmod, copyFile, mkdir, readdir, rm } from 'node:fs/promises'
import { basename, join, resolve } from 'node:path'
import { zipSync, type Zippable } from 'fflate'

const repositoryRoot = resolve(import.meta.dir, '..')
const inputRoot = join(repositoryRoot, 'dist', 'actions')
const outputRoot = join(repositoryRoot, 'dist', 'release')
const pluginArtifactName = 'lucia-official-plugins-wasm'
const desktopArtifactPattern =
  /^lucia-(?:core|tui-core|tui-plugins)-(?:linux|macos|windows)-(?:x64|arm64)$/

/** 读取 Actions 下载目录中的直接文件，拒绝意外的嵌套目录。 */
async function listArtifactFiles(root: string): Promise<string[]> {
  const entries = await readdir(root, { withFileTypes: true })
  if (entries.some((entry) => !entry.isFile())) {
    throw new Error(`Release 产物包含不支持的嵌套目录：${root}`)
  }
  const files = entries.map((entry) => entry.name).sort()
  if (files.length === 0) {
    throw new Error(`Release 产物目录为空：${root}`)
  }
  return files
}

/** 为 Release 资产生成同名 SHA-256 文件。 */
async function writeChecksum(path: string): Promise<void> {
  const bytes = new Uint8Array(await Bun.file(path).arrayBuffer())
  const sha256 = new Bun.CryptoHasher('sha256').update(bytes).digest('hex')
  await Bun.write(`${path}.sha256`, `${sha256}  ${basename(path)}\n`)
}

/** Windows 资产使用 ZIP，保持下载后无需额外归档工具。 */
async function createZip(source: string, output: string, files: string[]): Promise<void> {
  const entries: Zippable = {}
  const timestamp = new Date(1980, 0, 1)
  for (const file of files) {
    entries[file] = [
      new Uint8Array(await Bun.file(join(source, file)).arrayBuffer()),
      { mtime: timestamp },
    ]
  }
  await Bun.write(output, zipSync(entries, { level: 9, mtime: timestamp }))
}

/** Linux 与 macOS 资产使用 tar.gz，并恢复 TUI 可执行位。 */
async function createTarGz(source: string, output: string, artifact: string): Promise<void> {
  if (artifact.startsWith('lucia-tui-')) {
    const executable = join(source, 'lucia')
    if (!(await Bun.file(executable).exists())) {
      throw new Error(`TUI 产物缺少 lucia 可执行文件：${artifact}`)
    }
    await chmod(executable, 0o755)
  }
  const child = Bun.spawn(['tar', '-czf', output, '-C', source, '.'], {
    stdin: 'ignore',
    stdout: 'inherit',
    stderr: 'inherit',
  })
  const exitCode = await child.exited
  if (exitCode !== 0) {
    throw new Error(`tar.gz 打包失败：${artifact}`)
  }
}

/** 把平台无关的插件 ZIP、校验和与 Registry 直接提升为 Release 资产。 */
async function copyPluginAssets(source: string): Promise<void> {
  const files = await listArtifactFiles(source)
  if (!files.includes('registry.json') || !files.some((file) => file.endsWith('.zip'))) {
    throw new Error('官方插件产物缺少 Registry 或插件 ZIP')
  }
  for (const file of files) {
    await copyFile(join(source, file), join(outputRoot, file))
  }
  await writeChecksum(join(outputRoot, 'registry.json'))
}

/** 将当前工作流的全部 Actions 产物转换为可永久挂载到 GitHub Release 的资产。 */
async function main(): Promise<void> {
  await rm(outputRoot, { recursive: true, force: true })
  await mkdir(outputRoot, { recursive: true })

  const artifacts = (await readdir(inputRoot, { withFileTypes: true }))
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort()
  if (!artifacts.includes(pluginArtifactName)) {
    throw new Error('缺少官方插件 Actions 产物')
  }

  for (const artifact of artifacts) {
    const source = join(inputRoot, artifact)
    if (artifact === pluginArtifactName) {
      await copyPluginAssets(source)
      continue
    }
    if (!desktopArtifactPattern.test(artifact)) {
      throw new Error(`不支持的 Actions 产物：${artifact}`)
    }

    const files = await listArtifactFiles(source)
    const windows = artifact.includes('-windows-')
    const archive = join(outputRoot, `${artifact}.${windows ? 'zip' : 'tar.gz'}`)
    if (windows) {
      await createZip(source, archive, files)
    } else {
      await createTarGz(source, archive, artifact)
    }
    await writeChecksum(archive)
  }

  console.log(`已生成 Lucia GitHub Release 资产：${outputRoot}`)
}

await main()
