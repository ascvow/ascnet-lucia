/** 官方插件发布清单中的单个 bundle。 */
export interface OfficialPluginBundle {
  /** 插件稳定 ID，必须与 plugin.toml 一致。 */
  id: string
  /** 当前插件替代的旧官方插件 ID；同步时会删除对应旧 bundle。 */
  replaces?: string[]
  /** 独立插件 crate 名称。 */
  crate: string
  /** 相对于仓库根目录的插件目录。 */
  directory: string
  /** bundle 内保留相对路径的运行时文件。 */
  files: string[]
}

/** 官方 Registry 所在的 GitHub Release。 */
export interface OfficialPluginRelease {
  /** GitHub 仓库所有者。 */
  owner: string
  /** GitHub 仓库名。 */
  repository: string
  /** 本批资产使用的不可变 Release tag。 */
  tag: string
}

/** 构建和 Release 打包共享的唯一官方插件清单。 */
export interface OfficialPluginCatalog {
  /** 清单格式版本。 */
  schema_version: number
  /** 官方插件发布者标识。 */
  publisher: string
  /** Registry 和插件资产的 Release 位置。 */
  release: OfficialPluginRelease
  /** 官方插件 bundle 列表。 */
  plugins: OfficialPluginBundle[]
}

/** 读取并验证官方插件清单，拒绝重复 ID、路径穿越和不完整条目。 */
export async function loadOfficialPluginCatalog(): Promise<OfficialPluginCatalog> {
  const path = `${import.meta.dir}/../registry/official-plugins.json`
  const catalog = (await Bun.file(path).json()) as OfficialPluginCatalog
  if (catalog.schema_version !== 1) {
    throw new Error(`不支持官方插件清单版本：${catalog.schema_version}`)
  }
  if (
    !catalog.publisher ||
    !catalog.release?.owner ||
    !catalog.release.repository ||
    !catalog.release.tag
  ) {
    throw new Error('官方插件清单缺少发布信息')
  }
  const ids = new Set<string>()
  for (const plugin of catalog.plugins) {
    if (!plugin.id || !plugin.crate || !plugin.directory || plugin.files.length === 0) {
      throw new Error('官方插件清单包含不完整条目')
    }
    if (ids.has(plugin.id)) {
      throw new Error(`官方插件清单重复声明 ID：${plugin.id}`)
    }
    ids.add(plugin.id)
    for (const path of [plugin.directory, ...plugin.files, ...(plugin.replaces ?? [])]) {
      if (path.startsWith('/') || path.split('/').includes('..')) {
        throw new Error(`官方插件路径不安全：${path}`)
      }
    }
  }
  const replacedIds = new Set<string>()
  for (const plugin of catalog.plugins) {
    for (const replacedId of plugin.replaces ?? []) {
      if (
        !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(replacedId) ||
        replacedId === plugin.id ||
        ids.has(replacedId)
      ) {
        throw new Error(`官方插件替代关系无效：${plugin.id} -> ${replacedId}`)
      }
      if (replacedIds.has(replacedId)) {
        throw new Error(`旧官方插件 ID 被重复替代：${replacedId}`)
      }
      replacedIds.add(replacedId)
    }
  }
  return catalog
}
