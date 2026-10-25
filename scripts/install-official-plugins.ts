import { copyFile, lstat, mkdir, mkdtemp, rename, rm } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { loadOfficialPluginCatalog } from './official-plugins'

/** 将已构建的官方插件文件更新到 Lucia Home，保留目录内未受清单管理的用户配置。 */
async function main(): Promise<void> {
  const root = join(import.meta.dir, '..')
  const luciaHome = process.env.LUCIA_HOME || join(homedir(), '.lucia')
  const destinationRoot = join(luciaHome, 'official-plugins')
  const catalog = await loadOfficialPluginCatalog()

  // 只迁移清单明确退役的官方 bundle；不改用户配置、受管理插件或用户自行安装目录。
  // 使用唯一备份目录保留旧文件，反复安装不会覆盖已有备份，也不会再次启用插件。
  for (const id of catalog.retired ?? []) {
    const source = join(destinationRoot, id)
    const exists = await lstat(source).then(() => true).catch((error: NodeJS.ErrnoException) => {
      if (error.code === 'ENOENT') return false
      throw error
    })
    if (!exists) continue
    const disabledRoot = join(luciaHome, 'disabled-official-plugins')
    await mkdir(disabledRoot, { recursive: true })
    const backup = await mkdtemp(join(disabledRoot, `${id}-`))
    await rename(source, join(backup, id))
    console.log(`已停用退役官方插件：${id}（保留于 ${backup}）`)
  }

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
