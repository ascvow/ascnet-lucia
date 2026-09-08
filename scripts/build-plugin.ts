import { readdir, copyFile, mkdir, stat } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join, resolve } from 'node:path'

const repositoryRoot = resolve(import.meta.dir, '..')
const pluginNames = process.argv.slice(2)

if (pluginNames.length === 0) {
  throw new Error('缺少插件目录名，例如 mcp-plugin')
}

const sharedTarget = join(repositoryRoot, 'target')

/**
 * 定位 wasi-sdk 安装目录。
 *
 * 带 C 依赖的插件需要 wasi-sdk 提供的 clang 与
 * wasm32-wasip2 sysroot 才能编译；纯 Rust 插件不需要。按 `WASI_SDK_PATH`、`~/.wasi-sdk`、
 * `/opt/wasi-sdk` 的顺序查找，找不到时返回 `undefined` 而不是报错——不能让纯 Rust 插件的
 * 构建被一个它用不到的工具链挡住。
 */
async function locateWasiSdk(): Promise<string | undefined> {
  const explicit = Bun.env.WASI_SDK_PATH
  if (explicit && (await isDirectory(join(explicit, 'share', 'wasi-sysroot')))) {
    return explicit
  }
  for (const base of [join(homedir(), '.wasi-sdk'), '/opt/wasi-sdk']) {
    if (await isDirectory(join(base, 'share', 'wasi-sysroot'))) {
      return base
    }
    // 官方发行包解压后自带 `wasi-sdk-<版本>-<架构>-<系统>` 一层目录，取版本号最大的一个。
    const nested = (await readdir(base).catch(() => []))
      .filter((name) => name.startsWith('wasi-sdk-'))
      .sort()
      .reverse()
    for (const name of nested) {
      const candidate = join(base, name)
      if (await isDirectory(join(candidate, 'share', 'wasi-sysroot'))) {
        return candidate
      }
    }
  }
  return undefined
}

/** 判断路径是否为已存在的目录。 */
async function isDirectory(path: string): Promise<boolean> {
  return await stat(path)
    .then((info) => info.isDirectory())
    .catch(() => false)
}

/**
 * 组装构建环境：找到 wasi-sdk 时注入 C 交叉编译变量，否则原样返回。
 *
 * 变量只在 cc 真正被调用时才生效，因此对纯 Rust 插件没有副作用。
 */
async function buildEnvironment(): Promise<Record<string, string | undefined>> {
  const sdk = await locateWasiSdk()
  if (!sdk) {
    console.warn(
      '[build-plugin] 未找到 wasi-sdk；含 C 依赖的插件将编译失败。\n' +
        '  安装：https://github.com/WebAssembly/wasi-sdk/releases（解压到 ~/.wasi-sdk 或设置 WASI_SDK_PATH）',
    )
    return Bun.env
  }
  const sysroot = join(sdk, 'share', 'wasi-sysroot')
  console.log(`[build-plugin] 使用 wasi-sdk：${sdk}`)
  return {
    ...Bun.env,
    CC_wasm32_wasip2: join(sdk, 'bin', 'clang'),
    AR_wasm32_wasip2: join(sdk, 'bin', 'llvm-ar'),
    CFLAGS_wasm32_wasip2: `--sysroot=${sysroot} --target=wasm32-wasip2`,
  }
}

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
      env: await buildEnvironment(),
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
