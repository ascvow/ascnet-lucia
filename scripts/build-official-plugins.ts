import { loadOfficialPluginCatalog } from './official-plugins'

/** 从唯一官方清单读取 crate 列表，并委托现有 WASM 构建脚本。 */
async function main(): Promise<void> {
  const catalog = await loadOfficialPluginCatalog()
  const child = Bun.spawn(
    ['bun', 'run', 'scripts/build-plugin.ts', ...catalog.plugins.map((plugin) => plugin.crate)],
    {
      cwd: `${import.meta.dir}/..`,
      stdin: 'inherit',
      stdout: 'inherit',
      stderr: 'inherit',
    },
  )
  const exitCode = await child.exited
  if (exitCode !== 0) {
    process.exit(exitCode)
  }
}

await main()
