import { copyFile, mkdir, rm } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { loadOfficialPluginCatalog } from './official-plugins'

/** 将已构建的官方插件文件更新到 Lucia Home，保留目录内未受清单管理的用户配置。 */
async function main(): Promise<void> {
  const root = join(import.meta.dir, '..')
  const luciaHome = process.env.LUCIA_HOME || join(homedir(), '.lucia')
  const destinationRoot = join(luciaHome, 'official-plugins')
  const catalog = await loadOfficialPluginCatalog()

  for (const plugin of catalog.plugins) {
    for (const replacedId of plugin.replaces ?? []) {
      await rm(join(destinationRoot, replacedId), { recursive: true, force: true })
      console.log(`已移除被替代的官方插件：${replacedId}`)
    }
    const destination = join(destinationRoot, plugin.id)
    for (const relativePath of plugin.files) {
      const source = join(root, plugin.directory, relativePath)
      const target = join(destination, relativePath)
      await mkdir(dirname(target), { recursive: true })
      await copyFile(source, target)
    }
    console.log(`已更新官方插件：${plugin.id}`)
  }
}

await main()
