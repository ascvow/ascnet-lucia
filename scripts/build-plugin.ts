import { copyFile, mkdir } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'

const repositoryRoot = resolve(import.meta.dir, '..')
const pluginNames = process.argv.slice(2)

if (pluginNames.length === 0) {
  throw new Error('缺少插件目录名，例如 context-plugin')
}

const sharedTarget = join(repositoryRoot, 'target')

/**
 * 在根 workspace 中一次构建指定插件，并仅把最终 WASM 回拷到 manifest 约定的位置。
 * 构建失败时保留 Cargo 的退出码，避免安装流程使用旧产物继续执行。
 */
async function buildPlugin(): Promise<void> {
  const child = Bun.spawn(
    [
      'cargo',
      'build',
      '--offline',
      ...pluginNames.flatMap((pluginName) => ['-p', pluginName]),
      '--release',
      '--target',
      'wasm32-wasip2',
    ],
    {
      cwd: repositoryRoot,
      env: Bun.env,
      stdin: 'inherit',
      stdout: 'inherit',
      stderr: 'inherit',
    },
  )

  const exitCode = await child.exited
  if (exitCode !== 0) {
    process.exitCode = exitCode
    throw new Error(`插件构建失败：${pluginNames.join(', ')}`)
  }

  await Promise.all(
    pluginNames.map(async (pluginName) => {
      const pluginRoot = join(repositoryRoot, 'examples', 'plugins', pluginName)
      const artifactName = `${pluginName.replaceAll('-', '_')}.wasm`
      const sharedArtifact = join(sharedTarget, 'wasm32-wasip2', 'release', artifactName)
      const localArtifact = join(pluginRoot, 'target', 'wasm32-wasip2', 'release', artifactName)

      await mkdir(dirname(localArtifact), { recursive: true })
      await copyFile(sharedArtifact, localArtifact)
    }),
  )
}

await buildPlugin()
