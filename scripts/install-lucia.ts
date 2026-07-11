import { copyFile, mkdir, readFile, rename, rm, stat, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'

/** Files copied from an official Lucia plugin bundle. Lucia 官方插件 bundle 的同步描述。 */
interface OfficialPluginBundle {
  /** Stable plugin ID and installation directory name. 插件稳定 ID，也是安装目录名。 */
  id: string
  /** Runtime files relative to the plugin source directory. 相对于插件源码目录的运行时文件。 */
  files: string[]
}

/** Repository root used as the source of official bundles. 官方插件 bundle 的仓库源目录。 */
const repositoryRoot = join(import.meta.dir, '..')
/** Lucia runtime home, overridable through `LUCIA_HOME`. Lucia 运行目录，可由 `LUCIA_HOME` 覆盖。 */
const luciaHome = process.env.LUCIA_HOME || join(homedir(), '.lucia')
/** Destination root for installer-managed official plugins. 安装器维护的官方插件目标目录。 */
const officialRoot = join(luciaHome, 'official-plugins')
/** zsh startup file updated by the installer. 安装器按需更新的 zsh 启动文件。 */
const zshrcPath = join(homedir(), '.zshrc')
/** Idempotent PATH entry for Cargo-installed binaries. Cargo 安装目录的幂等 PATH 配置。 */
const cargoPathLine = 'export PATH="$HOME/.cargo/bin:$PATH"'

/** Official bundles included in the default Lucia installation. Lucia 默认安装包含的官方 bundle。 */
const bundles: OfficialPluginBundle[] = [
  {
    id: 'context',
    files: ['plugin.toml', 'target/wasm32-wasip2/release/context_plugin.wasm'],
  },
  {
    id: 'mcp',
    files: [
      'plugin.toml',
      'target/wasm32-wasip2/release/mcp_plugin.wasm',
      'config/mastergo.example.json',
    ],
  },
  {
    id: 'skill',
    files: [
      'plugin.toml',
      'target/wasm32-wasip2/release/skill_plugin.wasm',
      'skills/lucia-plugin-development/SKILL.md',
    ],
  },
  {
    id: 'command',
    files: ['plugin.toml', 'target/wasm32-wasip2/release/command_plugin.wasm'],
  },
  {
    id: 'teammate',
    files: ['plugin.toml', 'target/wasm32-wasip2/release/teammate_plugin.wasm'],
  },
  {
    id: 'plan',
    files: ['plugin.toml', 'target/wasm32-wasip2/release/plan_plugin.wasm'],
  },
  {
    id: 'sandbox',
    files: ['plugin.toml', 'target/wasm32-wasip2/release/sandbox_plugin.wasm'],
  },
]

/**
 * Copies one official plugin's runtime files while retaining user-managed files.
 * 同步一个官方插件的运行时文件，同时保留安装目录中的用户文件。
 */
async function syncBundle(bundle: OfficialPluginBundle): Promise<void> {
  const sourceRoot = join(repositoryRoot, 'examples', 'plugins', `${bundle.id}-plugin`)
  const destinationRoot = join(officialRoot, bundle.id)
  await Promise.all(
    bundle.files.map(async (relativePath) => {
      const sourceStat = await stat(join(sourceRoot, relativePath))
      if (!sourceStat.isFile()) {
        throw new Error(`官方插件文件不是普通文件：${relativePath}`)
      }
    }),
  )
  const publishOrder = [...bundle.files].sort(
    (left, right) => Number(left === 'plugin.toml') - Number(right === 'plugin.toml'),
  )
  for (const relativePath of publishOrder) {
    const source = join(sourceRoot, relativePath)
    const destination = join(destinationRoot, relativePath)
    await mkdir(dirname(destination), { recursive: true })
    const temporary = `${destination}.lucia-install-${process.pid}`
    try {
      await copyFile(source, temporary)
      await rename(temporary, destination)
    } finally {
      await rm(temporary, { force: true })
    }
  }
}

/**
 * Ensures new zsh sessions can resolve the Cargo-installed `lucia` binary.
 * Returns whether `.zshrc` changed. 确保新 zsh 会话可找到 `lucia`，并返回是否修改配置。
 */
async function registerZshPath(): Promise<boolean> {
  let current = ''
  try {
    current = await readFile(zshrcPath, 'utf8')
  } catch (error) {
    if (!(error instanceof Error && 'code' in error && error.code === 'ENOENT')) {
      throw error
    }
  }
  if (current.split(/\r?\n/u).includes(cargoPathLine)) {
    return false
  }
  const separator = current.length === 0 || current.endsWith('\n') ? '' : '\n'
  await writeFile(zshrcPath, `${current}${separator}\n# Lucia CLI\n${cargoPathLine}\n`, 'utf8')
  return true
}

/** Runs official bundle synchronization and zsh registration. 执行官方插件同步和 zsh 注册。 */
async function main(): Promise<void> {
  await mkdir(officialRoot, { recursive: true })
  for (const bundle of bundles) {
    await syncBundle(bundle)
  }
  const pathAdded = await registerZshPath()
  console.log(`已同步官方插件：${officialRoot}`)
  console.log(pathAdded ? `已更新 zsh 配置：${zshrcPath}` : 'zsh PATH 已配置，无需修改')
}

await main()
