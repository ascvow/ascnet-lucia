/**
 * 统一离线检查入口。
 *
 * 按固定顺序执行格式、静态检查、测试和构建，覆盖自进化计划 M0-01 要求的基线命令。
 * 所有 cargo 命令带 `--offline`，因此不依赖网络；全部命令都不依赖真实模型。
 *
 * 用法：
 *   bun run check                 执行全部检查
 *   bun run check --list          仅打印将要执行的命令
 *   bun run check --from clippy   从指定步骤开始执行（用于修复后续跑）
 *   bun run check --only fmt,test 只执行指定步骤
 *
 * 任意步骤失败即中止，并以该步骤的退出码退出。
 */

/** 单个检查步骤：`id` 用于命令行筛选，`command` 为实际执行的参数数组。 */
interface CheckStep {
  id: string
  description: string
  command: string[]
}

const repoRoot = `${import.meta.dir}/..`

/** M0-01 定义的标准离线验证序列，顺序敏感：先快后慢，先检查后构建。 */
const steps: CheckStep[] = [
  {
    id: 'fmt',
    description: '检查 Rust 代码格式',
    command: ['cargo', 'fmt', '--all', '--', '--check'],
  },
  {
    id: 'clippy',
    description: '静态检查，warning 视为错误',
    command: ['cargo', 'clippy', '--workspace', '--all-targets', '--offline', '--', '-D', 'warnings'],
  },
  {
    id: 'test',
    description: '运行 workspace 单元与集成测试',
    command: ['cargo', 'test', '--workspace', '--offline'],
  },
  {
    id: 'build:plugin:official',
    description: '构建官方插件 WASM',
    command: ['bun', 'run', 'build:plugin:official'],
  },
  {
    id: 'build:plugin:all',
    description: '构建全部示例插件 WASM',
    command: ['bun', 'run', 'build:plugin:all'],
  },
  {
    id: 'build:tui:core',
    description: '构建无插件 TUI',
    command: ['cargo', 'build', '--offline', '-p', 'lucia', '--no-default-features'],
  },
  {
    id: 'build:tui:plugins',
    description: '构建启用插件的 TUI',
    command: ['cargo', 'build', '--offline', '-p', 'lucia', '--features', 'plugins'],
  },
]

/**
 * 解析命令行参数，返回实际需要执行的步骤子集。
 *
 * `--only` 优先于 `--from`；两者都未指定时返回全部步骤。
 * 当引用了不存在的步骤 id 时抛出错误，避免静默跳过检查。
 */
function selectSteps(argv: string[]): CheckStep[] {
  const only = readOption(argv, '--only')
  const from = readOption(argv, '--from')

  if (only) {
    const wanted = only.split(',').map((value) => value.trim())
    for (const id of wanted) {
      if (!steps.some((step) => step.id === id)) {
        throw new Error(`未知步骤：${id}`)
      }
    }
    return steps.filter((step) => wanted.includes(step.id))
  }

  if (from) {
    const index = steps.findIndex((step) => step.id === from)
    if (index < 0) {
      throw new Error(`未知步骤：${from}`)
    }
    return steps.slice(index)
  }

  return steps
}

/** 读取形如 `--key value` 或 `--key=value` 的选项值，缺失时返回 undefined。 */
function readOption(argv: string[], key: string): string | undefined {
  const inline = argv.find((arg) => arg.startsWith(`${key}=`))
  if (inline) {
    return inline.slice(key.length + 1)
  }
  const index = argv.indexOf(key)
  return index >= 0 ? argv[index + 1] : undefined
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2)
  const selected = selectSteps(argv)

  if (argv.includes('--list')) {
    for (const step of selected) {
      console.log(`${step.id.padEnd(24)} ${step.command.join(' ')}`)
    }
    return
  }

  const startedAt = Date.now()
  for (const [index, step] of selected.entries()) {
    const label = `[${index + 1}/${selected.length}] ${step.id}`
    console.log(`\n${label} ${step.description}`)
    console.log(`> ${step.command.join(' ')}`)

    const child = Bun.spawn(step.command, {
      cwd: repoRoot,
      stdin: 'inherit',
      stdout: 'inherit',
      stderr: 'inherit',
    })
    const exitCode = await child.exited
    if (exitCode !== 0) {
      console.error(`\n${label} 失败，退出码 ${exitCode}`)
      process.exit(exitCode)
    }
  }

  const seconds = ((Date.now() - startedAt) / 1000).toFixed(1)
  console.log(`\n全部 ${selected.length} 项离线检查通过，用时 ${seconds}s`)
}

await main()
